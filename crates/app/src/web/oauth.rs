//! Logging in and linking accounts through OAuth providers. The browser is sent to the
//! provider by `start` and comes back to `callback`, which ends in a redirect: to `/` after
//! a login, to `/account` after linking, and to either with `?error=<reason>` when the
//! flow failed.

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, header};
use axum::response::Redirect;
use axum_extra::extract::cookie::CookieJar;
use serde::{Deserialize, Serialize};
use url::Url;

use super::AppState;
use super::auth::{Auth, ClientIp, MaybeAuth, start_session};
use super::error::ApiError;
use crate::oauth::{Identity, Intent, OAuthError, ProviderInfo, suggested_username};
use crate::users::{Role, User, UserError};

pub async fn providers(State(state): State<AppState>) -> Json<Vec<ProviderInfo>> {
    Json(state.oauth.providers())
}

/// Where a provider sends the browser back to: under `web.public_url`, or the host the
/// request came to.
fn callback_url(state: &AppState, headers: &HeaderMap, provider: &str) -> Result<Url, ApiError> {
    let base = match &state.public_url {
        Some(url) => url.clone(),
        None => {
            let host = headers
                .get(header::HOST)
                .and_then(|value| value.to_str().ok())
                .ok_or_else(|| ApiError::BadRequest("request has no Host header".into()))?;
            Url::parse(&format!("http://{host}"))
                .map_err(|_| ApiError::BadRequest("request has an unusable Host header".into()))?
        }
    };
    base.join(&format!("/api/auth/{provider}/callback"))
        .map_err(|e| ApiError::Internal(format!("callback URL: {e}")))
}

#[derive(Debug, Deserialize)]
pub struct StartQuery {
    #[serde(default = "login")]
    pub intent: Intent,
}

fn login() -> Intent {
    Intent::Login
}

/// Sends the browser to the provider.
pub async fn start(
    State(state): State<AppState>,
    Path(provider): Path<String>,
    Query(query): Query<StartQuery>,
    MaybeAuth(identity): MaybeAuth,
    headers: HeaderMap,
) -> Result<Redirect, ApiError> {
    let user = match query.intent {
        Intent::Login => None,
        Intent::Link => Some(identity.ok_or(ApiError::Unauthorized)?.user.id),
    };
    let redirect_uri = callback_url(&state, &headers, &provider)?;
    let location = state
        .oauth
        .begin(&provider, query.intent, user, redirect_uri)
        .await?
        .ok_or_else(|| ApiError::TooManyRequests(std::time::Duration::from_secs(60)))?;
    Ok(Redirect::to(location.as_str()))
}

#[derive(Debug, Deserialize)]
pub struct CallbackQuery {
    pub code: Option<String>,
    pub state: Option<String>,
    pub error: Option<String>,
}

fn failed(intent: Intent, reason: &str) -> Redirect {
    let page = match intent {
        Intent::Login => "/login",
        Intent::Link => "/account",
    };
    Redirect::to(&format!("{page}?error={reason}"))
}

/// Takes the browser back from the provider.
pub async fn callback(
    State(state): State<AppState>,
    Path(provider): Path<String>,
    Query(query): Query<CallbackQuery>,
    MaybeAuth(current): MaybeAuth,
    ClientIp(ip): ClientIp,
    jar: CookieJar,
    headers: HeaderMap,
) -> Result<(CookieJar, Redirect), ApiError> {
    let pending = query
        .state
        .as_deref()
        .and_then(|s| state.oauth.states.take(s));
    let Some(pending) = pending.filter(|p| p.provider == provider) else {
        tracing::info!(
            provider,
            "login flow came back with an unknown or spent state"
        );
        return Ok((jar, failed(Intent::Login, "state")));
    };
    if let Some(error) = &query.error {
        tracing::info!(provider, error, "provider refused the login flow");
        let reason = if error == "access_denied" {
            "denied"
        } else {
            "provider"
        };
        return Ok((jar, failed(pending.intent, reason)));
    }
    let Some(code) = query.code.as_deref() else {
        return Ok((jar, failed(pending.intent, "state")));
    };
    let (provider, remote, tokens) = match state.oauth.complete(&pending, code).await {
        Ok(done) => done,
        Err(error) => {
            tracing::warn!(provider = pending.provider, %error, "login flow failed");
            let reason = match error {
                OAuthError::Malformed { .. } => "identity",
                _ => "exchange",
            };
            return Ok((jar, failed(pending.intent, reason)));
        }
    };
    match pending.intent {
        Intent::Link => {
            let Some(current) = current.filter(|c| Some(c.user.id) == pending.user) else {
                return Ok((jar, failed(Intent::Link, "session")));
            };
            match state
                .oauth
                .store
                .link(current.user.id, &provider.id, &remote, &tokens)
                .await
            {
                Ok(linked) => {
                    tracing::info!(
                        username = current.user.username,
                        provider = provider.id,
                        subject = linked.subject,
                        "identity linked"
                    );
                    sync_discord(&state, current.user.id, &provider.id).await;
                    Ok((jar, Redirect::to("/account")))
                }
                Err(OAuthError::AlreadyLinked(_)) => {
                    Ok((jar, failed(Intent::Link, "already_linked")))
                }
                Err(OAuthError::ProviderLinked(_)) => {
                    Ok((jar, failed(Intent::Link, "provider_linked")))
                }
                Err(error) => Err(error.into()),
            }
        }
        Intent::Login => {
            let user = match state
                .oauth
                .store
                .find(&provider.id, &remote.subject)
                .await?
            {
                Some(linked) => {
                    state
                        .oauth
                        .store
                        .update(linked.id, Some(&remote), Some(&tokens))
                        .await?;
                    let Some(user) = state.users.get(linked.user_id).await? else {
                        return Ok((jar, failed(Intent::Login, "unknown_identity")));
                    };
                    user
                }
                None if state.oauth.signup => {
                    let user = sign_up(&state, &provider.id, &remote).await?;
                    state
                        .oauth
                        .store
                        .link(user.id, &provider.id, &remote, &tokens)
                        .await?;
                    tracing::info!(
                        username = user.username,
                        provider = provider.id,
                        "account created at first login"
                    );
                    user
                }
                None => {
                    tracing::info!(
                        provider = provider.id,
                        subject = remote.subject,
                        "login refused: identity linked to no account"
                    );
                    return Ok((jar, failed(Intent::Login, "unknown_identity")));
                }
            };
            tracing::info!(username = user.username, provider = provider.id, %ip, "logged in");
            sync_discord(&state, user.id, &provider.id).await;
            let (jar, _) = start_session(&state, user, jar, &headers, ip).await?;
            Ok((jar, Redirect::to("/")))
        }
    }
}

/// Stores the guilds a fresh Discord grant can see. A login or link that succeeded is not
/// undone when this fails; the guilds page offers a refresh that reports the error.
async fn sync_discord(state: &AppState, user: crate::users::UserId, provider: &str) {
    if provider != "discord" {
        return;
    }
    if let Err(error) = super::discord::sync_guilds(state, user).await {
        tracing::warn!(%user, error = ?error, "guilds not fetched from Discord");
    }
}

/// A viewer account named after the provider profile, with a number added when the name
/// is taken.
async fn sign_up(
    state: &AppState,
    provider: &str,
    remote: &crate::oauth::RemoteIdentity,
) -> Result<User, ApiError> {
    let base = suggested_username(provider, remote);
    for attempt in 0..100u32 {
        let name = if attempt == 0 {
            base.clone()
        } else {
            let suffix = format!("-{}", attempt + 1);
            let keep = crate::users::USERNAME_MAX - suffix.len();
            format!("{}{suffix}", base.chars().take(keep).collect::<String>())
        };
        match state.users.create(&name, None, Role::Viewer).await {
            Ok(user) => return Ok(user),
            Err(UserError::Taken(_)) => continue,
            Err(error) => return Err(error.into()),
        }
    }
    Err(ApiError::Conflict(format!(
        "every username derived from {base} is taken"
    )))
}

pub async fn identities(
    State(state): State<AppState>,
    Auth(identity): Auth,
) -> Result<Json<Vec<Identity>>, ApiError> {
    Ok(Json(state.oauth.store.list_for(identity.user.id).await?))
}

/// Fetches the linked profile again, renewing the provider's tokens when they expired;
/// for Discord, the guilds too.
pub async fn refresh_identity(
    State(state): State<AppState>,
    Auth(identity): Auth,
    Path(provider): Path<String>,
) -> Result<Json<Identity>, ApiError> {
    let refreshed = state
        .oauth
        .refresh_profile(identity.user.id, &provider)
        .await?;
    if provider == "discord" {
        super::discord::sync_guilds(&state, identity.user.id).await?;
    }
    Ok(Json(refreshed))
}

#[derive(Debug, Serialize)]
pub struct Unlinked {
    pub identity: Identity,
    /// Whether the provider confirmed forgetting the grant.
    pub revoked: bool,
}

pub async fn unlink(
    State(state): State<AppState>,
    Auth(identity): Auth,
    Path(provider): Path<String>,
) -> Result<Json<Unlinked>, ApiError> {
    let (unlinked, revoked) = state.oauth.unlink(identity.user.id, &provider).await?;
    tracing::info!(
        username = identity.user.username,
        provider,
        revoked,
        "identity unlinked"
    );
    Ok(Json(Unlinked {
        identity: unlinked,
        revoked,
    }))
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use axum::http::{Method, StatusCode};
    use base64::Engine;
    use serde_json::json;
    use sha2::{Digest, Sha256};
    use url::Url;

    use crate::oauth::{Kind, Registry};
    use crate::settings::WebConfig;
    use crate::web::testing::{Client, FakeProvider, app_with};

    async fn setup(signup: bool) -> (FakeProvider, crate::web::WebApp) {
        let fake = FakeProvider::start().await;
        let registry = Registry::new(vec![
            fake.oidc(),
            fake.provider("discord", Kind::Discord),
            fake.provider("github", Kind::GitHub),
            fake.provider("google", Kind::Oidc),
        ]);
        let app = app_with(WebConfig::default(), registry, signup).await;
        app.state
            .users
            .set_up("nick", "correct horse")
            .await
            .unwrap();
        (fake, app)
    }

    fn oidc_profile(sub: &str, username: &str) -> serde_json::Value {
        json!({"sub": sub, "preferred_username": username, "name": "Octo Cat", "email": "octo@example.com"})
    }

    /// Acts as the browser: follows the start redirect to the provider and comes back
    /// with `code`. Returns the redirect the callback ended in.
    async fn run_flow(
        client: &mut Client,
        fake: &FakeProvider,
        provider: &str,
        intent: &str,
        code: &str,
    ) -> String {
        let (status, body) = client
            .get(&format!("/api/auth/{provider}/start?intent={intent}"))
            .await;
        assert_eq!(status, StatusCode::SEE_OTHER, "{body}");
        let location = Url::parse(&client.location.clone().unwrap()).unwrap();
        assert_eq!(location.path(), "/authorize");
        let query: HashMap<String, String> = location.query_pairs().into_owned().collect();
        assert_eq!(query["client_id"], "cid");
        assert_eq!(query["code_challenge_method"], "S256");
        assert_eq!(
            query["redirect_uri"],
            format!("http://localhost:8080/api/auth/{provider}/callback")
        );
        let (status, body) = client
            .get(&format!(
                "/api/auth/{provider}/callback?code={code}&state={}",
                query["state"]
            ))
            .await;
        assert_eq!(status, StatusCode::SEE_OTHER, "{body}");
        let verifier = fake.lock().verifiers.last().cloned();
        if let Some(verifier) = verifier {
            let challenge = base64::engine::general_purpose::URL_SAFE_NO_PAD
                .encode(Sha256::digest(verifier.as_bytes()));
            assert_eq!(challenge, query["code_challenge"], "PKCE verifier matches");
        }
        let landed = client.location.clone().unwrap();
        // The page the browser lands on asks who it is, which brings the CSRF token.
        if client.cookie.is_some() {
            client.get("/api/session").await;
        }
        landed
    }

    #[tokio::test]
    async fn providers_are_listed() {
        let (_, app) = setup(false).await;
        let mut client = Client::new(&app);
        let (status, body) = client.get("/api/auth/providers").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            body,
            json!([
                {"id": "oidc", "name": "Fake SSO"},
                {"id": "discord", "name": "Discord"},
                {"id": "github", "name": "GitHub"},
                {"id": "google", "name": "Google"}
            ])
        );
        let (status, _) = client.get("/api/auth/nope/start").await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn first_login_creates_an_account_when_signup_is_on() {
        let (fake, app) = setup(true).await;
        fake.grant("code-1", "sub-1", oidc_profile("sub-1", "Octo Cat"));
        let mut browser = Client::new(&app);
        let landed = run_flow(&mut browser, &fake, "oidc", "login", "code-1").await;
        assert_eq!(landed, "/");
        assert!(browser.cookie.is_some());
        let (status, body) = browser.get("/api/session").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["user"]["username"], "Octo_Cat");
        assert_eq!(body["user"]["role"], "viewer");
        assert_eq!(body["user"]["has_password"], false);

        let (status, body) = browser.get("/api/auth/identities").await;
        assert_eq!(status, StatusCode::OK);
        let identities = body.as_array().unwrap();
        assert_eq!(identities.len(), 1);
        assert_eq!(identities[0]["provider"], "oidc");
        assert_eq!(identities[0]["subject"], "sub-1");
        assert_eq!(identities[0]["username"], "Octo Cat");
        assert_eq!(identities[0]["email"], "octo@example.com");
        assert_eq!(identities[0]["has_refresh_token"], true);
        assert!(identities[0].get("access_token").is_none());

        // The same identity logs into the same account; another gets its own name.
        fake.grant("code-2", "sub-1", oidc_profile("sub-1", "Octo Cat"));
        let mut again = Client::new(&app);
        run_flow(&mut again, &fake, "oidc", "login", "code-2").await;
        let (_, body) = again.get("/api/session").await;
        assert_eq!(body["user"]["username"], "Octo_Cat");
        fake.grant("code-3", "sub-2", oidc_profile("sub-2", "Octo Cat"));
        let mut other = Client::new(&app);
        run_flow(&mut other, &fake, "oidc", "login", "code-3").await;
        let (_, body) = other.get("/api/session").await;
        assert_eq!(body["user"]["username"], "Octo_Cat-2");
    }

    #[tokio::test]
    async fn without_signup_an_identity_must_be_linked_first() {
        let (fake, app) = setup(false).await;
        fake.grant("code-1", "sub-1", oidc_profile("sub-1", "octo"));
        let mut browser = Client::new(&app);
        let landed = run_flow(&mut browser, &fake, "oidc", "login", "code-1").await;
        assert_eq!(landed, "/login?error=unknown_identity");
        assert!(browser.cookie.is_none());

        // Linking needs a session.
        let (status, _) = browser.get("/api/auth/oidc/start?intent=link").await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);

        let mut nick = Client::new(&app);
        nick.login("nick", "correct horse").await;
        fake.grant("code-2", "sub-1", oidc_profile("sub-1", "octo"));
        let landed = run_flow(&mut nick, &fake, "oidc", "link", "code-2").await;
        assert_eq!(landed, "/account");
        let (_, body) = nick.get("/api/auth/identities").await;
        assert_eq!(body[0]["subject"], "sub-1");
        assert_eq!(
            body[0]["user_id"],
            nick.get("/api/session").await.1["user"]["id"]
        );

        // Now the identity logs in as nick, refreshing the stored profile.
        fake.grant("code-3", "sub-1", oidc_profile("sub-1", "octocat"));
        let mut browser = Client::new(&app);
        let landed = run_flow(&mut browser, &fake, "oidc", "login", "code-3").await;
        assert_eq!(landed, "/");
        let (_, body) = browser.get("/api/session").await;
        assert_eq!(body["user"]["username"], "nick");
        let (_, body) = browser.get("/api/auth/identities").await;
        assert_eq!(body[0]["username"], "octocat");

        // Linking again with a different identity of the same provider is refused, as is
        // linking an identity another account owns.
        fake.grant("code-4", "sub-9", oidc_profile("sub-9", "nine"));
        let landed = run_flow(&mut nick, &fake, "oidc", "link", "code-4").await;
        assert_eq!(landed, "/account?error=provider_linked");
        app.state
            .users
            .create("viewer", Some("battery staple"), crate::users::Role::Viewer)
            .await
            .unwrap();
        let mut viewer = Client::new(&app);
        viewer.login("viewer", "battery staple").await;
        fake.grant("code-5", "sub-1", oidc_profile("sub-1", "octo"));
        let landed = run_flow(&mut viewer, &fake, "oidc", "link", "code-5").await;
        assert_eq!(landed, "/account?error=already_linked");
    }

    #[tokio::test]
    async fn failures_come_back_with_a_reason() {
        let (fake, app) = setup(true).await;
        let mut browser = Client::new(&app);
        let (status, _) = browser
            .get("/api/auth/oidc/callback?code=x&state=unknown")
            .await;
        assert_eq!(status, StatusCode::SEE_OTHER);
        assert_eq!(browser.location.as_deref(), Some("/login?error=state"));

        browser.get("/api/auth/oidc/start").await;
        let location = Url::parse(&browser.location.clone().unwrap()).unwrap();
        let query: HashMap<String, String> = location.query_pairs().into_owned().collect();
        let state = query["state"].clone();
        browser
            .get(&format!(
                "/api/auth/oidc/callback?error=access_denied&state={state}"
            ))
            .await;
        assert_eq!(browser.location.as_deref(), Some("/login?error=denied"));
        // The state was spent by that.
        browser
            .get(&format!("/api/auth/oidc/callback?code=x&state={state}"))
            .await;
        assert_eq!(browser.location.as_deref(), Some("/login?error=state"));

        // A state from one provider is no good at another.
        browser.get("/api/auth/github/start").await;
        let location = Url::parse(&browser.location.clone().unwrap()).unwrap();
        let query: HashMap<String, String> = location.query_pairs().into_owned().collect();
        browser
            .get(&format!(
                "/api/auth/oidc/callback?code=x&state={}",
                query["state"]
            ))
            .await;
        assert_eq!(browser.location.as_deref(), Some("/login?error=state"));

        // A code the provider rejects.
        let landed = run_flow(&mut browser, &fake, "oidc", "login", "not-granted").await;
        assert_eq!(landed, "/login?error=exchange");
        fake.lock().fail_token = true;
        fake.grant("code-1", "sub-1", oidc_profile("sub-1", "octo"));
        let landed = run_flow(&mut browser, &fake, "oidc", "login", "code-1").await;
        assert_eq!(landed, "/login?error=exchange");
        fake.lock().fail_token = false;

        // A profile without a subject.
        fake.grant("code-2", "sub-2", json!({"name": "no subject"}));
        let landed = run_flow(&mut browser, &fake, "oidc", "login", "code-2").await;
        assert_eq!(landed, "/login?error=identity");
        assert!(browser.cookie.is_none());
    }

    #[tokio::test]
    async fn discord_github_and_google_profiles_are_read() {
        let (fake, app) = setup(true).await;
        fake.grant(
            "d",
            "1234",
            json!({"id": "1234", "username": "octo", "global_name": "Octo", "email": null}),
        );
        let mut discord = Client::new(&app);
        assert_eq!(
            run_flow(&mut discord, &fake, "discord", "login", "d").await,
            "/"
        );
        let (_, body) = discord.get("/api/auth/identities").await;
        assert_eq!(body[0]["provider"], "discord");
        assert_eq!(body[0]["subject"], "1234");
        assert_eq!(body[0]["username"], "octo");
        assert_eq!(body[0]["display_name"], "Octo");

        fake.grant(
            "g",
            "42",
            json!({"id": 42, "login": "octocat", "name": "Octo Cat"}),
        );
        let mut github = Client::new(&app);
        assert_eq!(
            run_flow(&mut github, &fake, "github", "login", "g").await,
            "/"
        );
        let (_, body) = github.get("/api/auth/identities").await;
        assert_eq!(body[0]["subject"], "42");
        assert_eq!(body[0]["username"], "octocat");
        let (_, body) = github.get("/api/session").await;
        assert_eq!(body["user"]["username"], "octocat");

        fake.grant(
            "go",
            "g-1",
            json!({"sub": "g-1", "name": "Goo", "email": "goo@gmail.com"}),
        );
        let mut google = Client::new(&app);
        let (status, _) = google.get("/api/auth/google/start").await;
        assert_eq!(status, StatusCode::SEE_OTHER);
        let location = Url::parse(&google.location.clone().unwrap()).unwrap();
        let query: HashMap<String, String> = location.query_pairs().into_owned().collect();
        assert_eq!(query["access_type"], "offline");
        assert_eq!(query["prompt"], "consent");
        assert_eq!(query["scope"], "openid profile email");
        assert_eq!(
            run_flow(&mut google, &fake, "google", "login", "go").await,
            "/"
        );
        let (_, body) = google.get("/api/session").await;
        assert_eq!(body["user"]["username"], "goo");
    }

    #[tokio::test]
    async fn unlinking_revokes_the_grant() {
        let (fake, app) = setup(true).await;
        fake.grant("code-1", "sub-1", oidc_profile("sub-1", "octo"));
        let mut browser = Client::new(&app);
        run_flow(&mut browser, &fake, "oidc", "login", "code-1").await;
        // The only login of a passwordless account stays.
        let (status, body) = browser.delete("/api/auth/identities/oidc").await;
        assert_eq!(status, StatusCode::CONFLICT, "{body}");
        fake.grant("d", "1234", json!({"id": "1234", "username": "octo"}));
        assert_eq!(
            run_flow(&mut browser, &fake, "discord", "link", "d").await,
            "/account"
        );

        let (status, body) = browser.delete("/api/auth/identities/oidc").await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["revoked"], true);
        assert_eq!(body["identity"]["provider"], "oidc");
        assert_eq!(fake.lock().revoked, vec!["refresh-1"]);
        let (status, _) = browser.delete("/api/auth/identities/oidc").await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        let (_, body) = browser.get("/api/auth/identities").await;
        assert_eq!(body.as_array().unwrap().len(), 1);

        // GitHub revocation goes through its own endpoint with the access token.
        fake.grant("g", "42", json!({"id": 42, "login": "octocat"}));
        let mut github = Client::new(&app);
        run_flow(&mut github, &fake, "github", "login", "g").await;
        let issued = fake.lock().issued;
        let (status, body) = github.delete("/api/auth/identities/github").await;
        assert_eq!(status, StatusCode::CONFLICT, "{body}");
        let admin_client = {
            let mut c = Client::new(&app);
            c.login("nick", "correct horse").await;
            c
        };
        let mut admin = admin_client;
        let (_, me) = github.get("/api/session").await;
        let (status, _) = admin
            .delete(&format!(
                "/api/users/{}",
                me["user"]["id"].as_str().unwrap()
            ))
            .await;
        assert_eq!(status, StatusCode::NO_CONTENT);
        assert!(fake.lock().revoked.contains(&format!("access-{issued}")));
        let (status, _) = github.get("/api/session").await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn refreshing_renews_expired_tokens_and_the_profile() {
        let (fake, app) = setup(true).await;
        fake.lock().expires_in = 0;
        fake.grant("code-1", "sub-1", oidc_profile("sub-1", "octo"));
        let mut browser = Client::new(&app);
        run_flow(&mut browser, &fake, "oidc", "login", "code-1").await;
        fake.lock()
            .profiles
            .insert("sub-1".into(), oidc_profile("sub-1", "octocat"));

        let (status, body) = browser
            .send(Method::POST, "/api/auth/identities/oidc/refresh", None)
            .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["username"], "octocat");
        assert_eq!(fake.lock().refreshes, 1);
        let (identity, tokens) = app
            .state
            .oauth
            .store
            .get(
                browser.get("/api/session").await.1["user"]["id"]
                    .as_str()
                    .unwrap()
                    .parse()
                    .unwrap(),
                "oidc",
            )
            .await
            .unwrap();
        assert_eq!(identity.username.as_deref(), Some("octocat"));
        assert_eq!(tokens.access_token, "access-2");
        assert_eq!(tokens.refresh_token.as_deref(), Some("refresh-2"));

        // A token that has not expired is used as is.
        fake.lock().expires_in = 3600;
        fake.grant("d", "1234", json!({"id": "1234", "username": "octo"}));
        run_flow(&mut browser, &fake, "discord", "link", "d").await;
        let (status, _) = browser
            .send(Method::POST, "/api/auth/identities/discord/refresh", None)
            .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(fake.lock().refreshes, 1);
        let (status, _) = browser
            .send(Method::POST, "/api/auth/identities/github/refresh", None)
            .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn the_public_url_shapes_the_callback() {
        let fake = FakeProvider::start().await;
        let config = WebConfig {
            public_url: Some(Url::parse("https://clips.example.com").unwrap()),
            ..WebConfig::default()
        };
        let app = app_with(config, Registry::new(vec![fake.oidc()]), true).await;
        let mut browser = Client::new(&app);
        browser.get("/api/auth/oidc/start").await;
        let location = Url::parse(&browser.location.clone().unwrap()).unwrap();
        let query: HashMap<String, String> = location.query_pairs().into_owned().collect();
        assert_eq!(
            query["redirect_uri"],
            "https://clips.example.com/api/auth/oidc/callback"
        );
        assert_eq!(query["scope"], "openid profile");
    }
}
