//! OAuth 2.0 login and account linking with Discord, GitHub, Google and any OpenID Connect
//! issuer: authorization codes with PKCE and single-use states, identities tied to
//! accounts, and the providers' tokens sealed under the keyring so they can be refreshed
//! and revoked later.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use discoclip_bot::DiscordEndpoints;
use discoclip_engine::StoreError;
use discoclip_engine::reqwest::{self, header};
use discoclip_engine::rusqlite::{self, OptionalExtension, params};
use discoclip_engine::store::sqlite::SqliteStore;
use jiff::Timestamp;
use secrecy::{ExposeSecret, SecretString};
use serde::{Deserialize, Serialize};
use serde_json::Value as Json;
use sha2::{Digest, Sha256};
use tokio::sync::OnceCell;
use url::Url;
use uuid::Uuid;

use crate::db::{nanos, timestamp, transact};
use crate::secrets::{Keyring, SecretError};
use crate::sessions::random_token;
use crate::settings::{AuthConfig, OAuthClient, OidcClient};
use crate::users::UserId;

#[derive(Debug, thiserror::Error)]
pub enum OAuthError {
    #[error("no login provider named {0}")]
    UnknownProvider(String),
    #[error("{provider}: {message}")]
    Discovery { provider: String, message: String },
    #[error("{provider} could not be reached: {source}")]
    Http {
        provider: String,
        source: reqwest::Error,
    },
    #[error("{provider} answered {status}: {body}")]
    Refused {
        provider: String,
        status: u16,
        body: String,
    },
    #[error("{provider} sent an answer without {missing}")]
    Malformed {
        provider: String,
        missing: &'static str,
    },
    #[error("that {0} identity is linked to another account")]
    AlreadyLinked(String),
    #[error("this account is linked to a different {0} identity; unlink it first")]
    ProviderLinked(String),
    #[error("no {0} identity is linked to this account")]
    NotLinked(String),
    #[error("this identity is the account's only way to log in; set a password first")]
    LastLogin,
    #[error("{0} issued no refresh token, so its access token cannot be renewed")]
    NoRefreshToken(String),
    #[error("identity {0} was unlinked meanwhile")]
    Gone(Uuid),
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error(transparent)]
    Secret(#[from] SecretError),
}

impl From<rusqlite::Error> for OAuthError {
    fn from(error: rusqlite::Error) -> Self {
        OAuthError::Store(error.into())
    }
}

/// How a provider shapes its requests and answers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Discord,
    GitHub,
    /// OpenID Connect: Google and the generic issuer.
    Oidc,
}

#[derive(Debug, Clone)]
pub struct Endpoints {
    pub authorize: Url,
    pub token: Url,
    pub userinfo: Url,
    pub revoke: Option<Url>,
    /// Send the client secret as HTTP basic auth rather than form fields.
    pub basic_auth: bool,
}

enum Discovery {
    Fixed(Endpoints),
    Issuer(Url, OnceCell<Endpoints>),
}

pub struct Provider {
    pub id: String,
    pub name: String,
    pub kind: Kind,
    pub client_id: String,
    pub client_secret: SecretString,
    pub scopes: Vec<String>,
    /// Extra query parameters on the authorization request.
    pub authorize_params: Vec<(String, String)>,
    discovery: Discovery,
}

impl std::fmt::Debug for Provider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Provider")
            .field("id", &self.id)
            .field("kind", &self.kind)
            .finish_non_exhaustive()
    }
}

fn url(text: &str) -> Url {
    Url::parse(text).expect("provider endpoints are valid URLs")
}

impl Provider {
    /// Discord, through the application `client` names, at `endpoints`.
    pub fn discord(client: &OAuthClient, endpoints: &DiscordEndpoints) -> Self {
        let api = endpoints.api_base();
        let at = |path: &str| api.join(path).expect("Discord API paths are valid");
        Self {
            id: "discord".into(),
            name: "Discord".into(),
            kind: Kind::Discord,
            client_id: client.client_id.clone(),
            client_secret: client.client_secret.clone(),
            scopes: vec![
                "identify".into(),
                "guilds".into(),
                "guilds.members.read".into(),
            ],
            authorize_params: vec![("prompt".into(), "none".into())],
            discovery: Discovery::Fixed(Endpoints {
                authorize: endpoints.authorize_url(),
                token: at("oauth2/token"),
                userinfo: at("users/@me"),
                revoke: Some(at("oauth2/token/revoke")),
                basic_auth: false,
            }),
        }
    }

    pub fn github(client: &OAuthClient) -> Self {
        Self {
            id: "github".into(),
            name: "GitHub".into(),
            kind: Kind::GitHub,
            client_id: client.client_id.clone(),
            client_secret: client.client_secret.clone(),
            scopes: vec!["read:user".into()],
            authorize_params: Vec::new(),
            discovery: Discovery::Fixed(Endpoints {
                authorize: url("https://github.com/login/oauth/authorize"),
                token: url("https://github.com/login/oauth/access_token"),
                userinfo: url("https://api.github.com/user"),
                revoke: Some(url("https://api.github.com/applications")),
                basic_auth: false,
            }),
        }
    }

    pub fn google(client: &OAuthClient) -> Self {
        Self {
            id: "google".into(),
            name: "Google".into(),
            kind: Kind::Oidc,
            client_id: client.client_id.clone(),
            client_secret: client.client_secret.clone(),
            scopes: vec!["openid".into(), "profile".into(), "email".into()],
            authorize_params: vec![
                ("access_type".into(), "offline".into()),
                ("prompt".into(), "consent".into()),
            ],
            discovery: Discovery::Issuer(url("https://accounts.google.com"), OnceCell::new()),
        }
    }

    pub fn oidc(client: &OidcClient) -> Self {
        Self {
            id: "oidc".into(),
            name: client.name.clone(),
            kind: Kind::Oidc,
            client_id: client.client_id.clone(),
            client_secret: client.client_secret.clone(),
            scopes: client.scopes.clone(),
            authorize_params: Vec::new(),
            discovery: Discovery::Issuer(client.issuer.clone(), OnceCell::new()),
        }
    }

    /// A provider with known endpoints, as tests point at a stand-in server.
    #[cfg(test)]
    pub fn fixed(id: &str, kind: Kind, client: &OAuthClient, endpoints: Endpoints) -> Self {
        let mut provider = match kind {
            Kind::Discord => Self::discord(client, &DiscordEndpoints::default()),
            Kind::GitHub => Self::github(client),
            Kind::Oidc => Self::google(client),
        };
        provider.id = id.into();
        provider.discovery = Discovery::Fixed(endpoints);
        provider
    }

    fn http_error(&self, source: reqwest::Error) -> OAuthError {
        OAuthError::Http {
            provider: self.name.clone(),
            source,
        }
    }

    fn malformed(&self, missing: &'static str) -> OAuthError {
        OAuthError::Malformed {
            provider: self.name.clone(),
            missing,
        }
    }

    async fn endpoints(&self, http: &reqwest::Client) -> Result<&Endpoints, OAuthError> {
        match &self.discovery {
            Discovery::Fixed(endpoints) => Ok(endpoints),
            Discovery::Issuer(issuer, cell) => {
                cell.get_or_try_init(|| self.discover(http, issuer)).await
            }
        }
    }

    async fn discover(
        &self,
        http: &reqwest::Client,
        issuer: &Url,
    ) -> Result<Endpoints, OAuthError> {
        let mut base = issuer.clone();
        if !base.path().ends_with('/') {
            base.set_path(&format!("{}/", base.path()));
        }
        let location =
            base.join(".well-known/openid-configuration")
                .map_err(|e| OAuthError::Discovery {
                    provider: self.name.clone(),
                    message: e.to_string(),
                })?;
        let document: Json = http
            .get(location.clone())
            .send()
            .await
            .and_then(reqwest::Response::error_for_status)
            .map_err(|e| self.http_error(e))?
            .json()
            .await
            .map_err(|e| self.http_error(e))?;
        let endpoint = |name: &str| -> Result<Url, OAuthError> {
            document
                .get(name)
                .and_then(Json::as_str)
                .and_then(|text| Url::parse(text).ok())
                .ok_or_else(|| OAuthError::Discovery {
                    provider: self.name.clone(),
                    message: format!("{location} has no {name}"),
                })
        };
        let methods = document
            .get("token_endpoint_auth_methods_supported")
            .and_then(Json::as_array);
        let basic_auth = methods.is_none_or(|methods| {
            methods
                .iter()
                .any(|m| m.as_str() == Some("client_secret_basic"))
        });
        Ok(Endpoints {
            authorize: endpoint("authorization_endpoint")?,
            token: endpoint("token_endpoint")?,
            userinfo: endpoint("userinfo_endpoint")?,
            revoke: endpoint("revocation_endpoint").ok(),
            basic_auth,
        })
    }

    /// Where to send the browser.
    pub async fn authorize_url(
        &self,
        http: &reqwest::Client,
        redirect_uri: &Url,
        state: &str,
        code_challenge: &str,
    ) -> Result<Url, OAuthError> {
        let mut location = self.endpoints(http).await?.authorize.clone();
        {
            let mut query = location.query_pairs_mut();
            query
                .append_pair("response_type", "code")
                .append_pair("client_id", &self.client_id)
                .append_pair("redirect_uri", redirect_uri.as_str())
                .append_pair("scope", &self.scopes.join(" "))
                .append_pair("state", state)
                .append_pair("code_challenge", code_challenge)
                .append_pair("code_challenge_method", "S256");
            for (key, value) in &self.authorize_params {
                query.append_pair(key, value);
            }
        }
        Ok(location)
    }

    async fn token_request(
        &self,
        http: &reqwest::Client,
        form: &[(&str, &str)],
    ) -> Result<TokenSet, OAuthError> {
        let endpoints = self.endpoints(http).await?;
        let mut form: Vec<(&str, &str)> = form.to_vec();
        let mut request = http
            .post(endpoints.token.clone())
            .header(header::ACCEPT, "application/json");
        if endpoints.basic_auth {
            request = request.basic_auth(&self.client_id, Some(self.client_secret.expose_secret()));
        } else {
            form.push(("client_id", &self.client_id));
            form.push(("client_secret", self.client_secret.expose_secret()));
        }
        let response = request
            .form(&form)
            .send()
            .await
            .map_err(|e| self.http_error(e))?;
        let status = response.status();
        let body = response.text().await.map_err(|e| self.http_error(e))?;
        let json: Json = serde_json::from_str(&body).unwrap_or(Json::Null);
        // GitHub reports failures with 200 and an `error` field.
        if !status.is_success() || json.get("error").is_some() {
            return Err(OAuthError::Refused {
                provider: self.name.clone(),
                status: status.as_u16(),
                body: body.chars().take(300).collect(),
            });
        }
        let access_token = json
            .get("access_token")
            .and_then(Json::as_str)
            .ok_or_else(|| self.malformed("an access token"))?
            .to_string();
        let expires_at = json
            .get("expires_in")
            .and_then(Json::as_i64)
            .map(|secs| Timestamp::now() + jiff::SignedDuration::from_secs(secs));
        Ok(TokenSet {
            access_token,
            refresh_token: json
                .get("refresh_token")
                .and_then(Json::as_str)
                .map(str::to_string),
            expires_at,
            scope: json.get("scope").and_then(Json::as_str).map(str::to_string),
        })
    }

    /// Trades the code the browser brought back for tokens.
    pub async fn exchange(
        &self,
        http: &reqwest::Client,
        code: &str,
        code_verifier: &str,
        redirect_uri: &Url,
    ) -> Result<TokenSet, OAuthError> {
        self.token_request(
            http,
            &[
                ("grant_type", "authorization_code"),
                ("code", code),
                ("redirect_uri", redirect_uri.as_str()),
                ("code_verifier", code_verifier),
            ],
        )
        .await
    }

    /// Trades a refresh token for new tokens; the old refresh token stays when the
    /// provider sends no new one.
    pub async fn refresh(
        &self,
        http: &reqwest::Client,
        refresh_token: &str,
    ) -> Result<TokenSet, OAuthError> {
        let mut tokens = self
            .token_request(
                http,
                &[
                    ("grant_type", "refresh_token"),
                    ("refresh_token", refresh_token),
                ],
            )
            .await?;
        if tokens.refresh_token.is_none() {
            tokens.refresh_token = Some(refresh_token.to_string());
        }
        Ok(tokens)
    }

    /// Tells the provider to forget the grant behind `tokens`. Ok(false) when the provider
    /// has no way to be told.
    pub async fn revoke(
        &self,
        http: &reqwest::Client,
        tokens: &Tokens,
    ) -> Result<bool, OAuthError> {
        let endpoints = self.endpoints(http).await?;
        let Some(revoke) = &endpoints.revoke else {
            return Ok(false);
        };
        let response = match self.kind {
            Kind::GitHub => {
                let mut location = revoke.clone();
                location
                    .path_segments_mut()
                    .map_err(|_| self.malformed("a revocation URL"))?
                    .push(&self.client_id)
                    .push("token");
                http.delete(location)
                    .basic_auth(&self.client_id, Some(self.client_secret.expose_secret()))
                    .header(header::ACCEPT, "application/vnd.github+json")
                    .json(&serde_json::json!({ "access_token": tokens.access_token }))
                    .send()
                    .await
            }
            Kind::Discord | Kind::Oidc => {
                // Revoking the refresh token ends the whole grant; without one, the access
                // token is all there is to revoke.
                let (token, hint) = match &tokens.refresh_token {
                    Some(refresh) => (refresh.as_str(), "refresh_token"),
                    None => (tokens.access_token.as_str(), "access_token"),
                };
                let mut form = vec![("token", token), ("token_type_hint", hint)];
                let mut request = http.post(revoke.clone());
                if endpoints.basic_auth {
                    request = request
                        .basic_auth(&self.client_id, Some(self.client_secret.expose_secret()));
                } else {
                    form.push(("client_id", &self.client_id));
                    form.push(("client_secret", self.client_secret.expose_secret()));
                }
                request.form(&form).send().await
            }
        }
        .map_err(|e| self.http_error(e))?;
        if response.status().is_success() {
            Ok(true)
        } else {
            let status = response.status().as_u16();
            let body = response.text().await.unwrap_or_default();
            Err(OAuthError::Refused {
                provider: self.name.clone(),
                status,
                body: body.chars().take(300).collect(),
            })
        }
    }

    /// Where Discord lists the user's guilds; None for other kinds.
    pub async fn discord_guilds_url(
        &self,
        http: &reqwest::Client,
    ) -> Result<Option<Url>, OAuthError> {
        if self.kind != Kind::Discord {
            return Ok(None);
        }
        let userinfo = &self.endpoints(http).await?.userinfo;
        let mut location = userinfo.clone();
        location.set_path(&format!("{}/guilds", userinfo.path().trim_end_matches('/')));
        Ok(Some(location))
    }

    /// Who the access token belongs to, at the provider.
    pub async fn identity(
        &self,
        http: &reqwest::Client,
        access_token: &str,
    ) -> Result<RemoteIdentity, OAuthError> {
        let endpoints = self.endpoints(http).await?;
        let mut request = http
            .get(endpoints.userinfo.clone())
            .bearer_auth(access_token)
            .header(header::ACCEPT, "application/json");
        if self.kind == Kind::GitHub {
            request = request.header(header::ACCEPT, "application/vnd.github+json");
        }
        let response = request.send().await.map_err(|e| self.http_error(e))?;
        if !response.status().is_success() {
            let status = response.status().as_u16();
            let body = response.text().await.unwrap_or_default();
            return Err(OAuthError::Refused {
                provider: self.name.clone(),
                status,
                body: body.chars().take(300).collect(),
            });
        }
        let json: Json = response.json().await.map_err(|e| self.http_error(e))?;
        let text = |name: &str| json.get(name).and_then(Json::as_str).map(str::to_string);
        let subject = match self.kind {
            Kind::Discord => text("id"),
            Kind::GitHub => json.get("id").and_then(|id| match id {
                Json::Number(n) => Some(n.to_string()),
                Json::String(s) => Some(s.clone()),
                _ => None,
            }),
            Kind::Oidc => text("sub"),
        }
        .ok_or_else(|| self.malformed("a subject"))?;
        let (username, display_name) = match self.kind {
            Kind::Discord => (text("username"), text("global_name")),
            Kind::GitHub => (text("login"), text("name")),
            Kind::Oidc => (text("preferred_username"), text("name")),
        };
        Ok(RemoteIdentity {
            subject,
            username,
            display_name,
            email: text("email"),
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TokenSet {
    pub access_token: String,
    pub refresh_token: Option<String>,
    pub expires_at: Option<Timestamp>,
    pub scope: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RemoteIdentity {
    pub subject: String,
    pub username: Option<String>,
    pub display_name: Option<String>,
    pub email: Option<String>,
}

/// The providers this server offers, from the `auth` settings, and Discord through the
/// application marked for login.
#[derive(Debug, Default)]
pub struct Registry {
    providers: Vec<Arc<Provider>>,
    /// Whether the Discord provider came from an application rather than the settings.
    discord_from_application: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct ProviderInfo {
    pub id: String,
    pub name: String,
}

impl Registry {
    /// The providers the `auth` settings name; Discord joins through [`set_discord`]
    /// once an application is marked for login.
    ///
    /// [`set_discord`]: Registry::set_discord
    pub fn from_config(auth: &AuthConfig) -> Self {
        Self::new(Self::configured(auth))
    }

    /// The providers `auth` names, in the order the login page shows them.
    pub fn configured(auth: &AuthConfig) -> Vec<Provider> {
        let mut providers = Vec::new();
        if let Some(client) = &auth.github {
            providers.push(Provider::github(client));
        }
        if let Some(client) = &auth.google {
            providers.push(Provider::google(client));
        }
        if let Some(client) = &auth.oidc {
            providers.push(Provider::oidc(client));
        }
        providers
    }

    pub fn new(providers: Vec<Provider>) -> Self {
        Self {
            providers: providers.into_iter().map(Arc::new).collect(),
            discord_from_application: false,
        }
    }

    /// Offers Discord login through an application's `provider`, taking Discord over from
    /// whatever was there; `None` withdraws an application's provider and leaves any other.
    pub fn set_discord(&mut self, provider: Option<Provider>) {
        match provider {
            Some(provider) => {
                self.providers.retain(|p| p.id != "discord");
                self.providers.insert(0, Arc::new(provider));
                self.discord_from_application = true;
            }
            None if self.discord_from_application => {
                self.providers.retain(|p| p.id != "discord");
                self.discord_from_application = false;
            }
            None => {}
        }
    }

    /// Replaces the providers the settings name, keeping the Discord provider an
    /// application supplies.
    pub fn replace_configured(&mut self, providers: Vec<Provider>) {
        let from_application = if self.discord_from_application {
            self.providers.iter().find(|p| p.id == "discord").cloned()
        } else {
            None
        };
        self.providers = providers.into_iter().map(Arc::new).collect();
        if let Some(discord) = from_application {
            self.providers.retain(|p| p.id != "discord");
            self.providers.insert(0, discord);
        }
    }

    pub fn get(&self, id: &str) -> Result<Arc<Provider>, OAuthError> {
        self.providers
            .iter()
            .find(|p| p.id == id)
            .cloned()
            .ok_or_else(|| OAuthError::UnknownProvider(id.to_string()))
    }

    pub fn list(&self) -> Vec<ProviderInfo> {
        self.providers
            .iter()
            .map(|p| ProviderInfo {
                id: p.id.clone(),
                name: p.name.clone(),
            })
            .collect()
    }
}

/// A PKCE verifier and its S256 challenge.
pub fn pkce() -> (String, String) {
    let verifier = random_token();
    let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
    (verifier, challenge)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Intent {
    Login,
    Link,
}

/// A flow the browser has been sent off on and has not come back from.
#[derive(Debug, Clone)]
pub struct Pending {
    pub provider: String,
    pub intent: Intent,
    /// The account linking, for [`Intent::Link`].
    pub user: Option<UserId>,
    pub code_verifier: String,
    pub redirect_uri: Url,
    started: Instant,
}

/// States handed out to browsers, each good once, for ten minutes.
#[derive(Default)]
pub struct PendingStates {
    map: Mutex<HashMap<String, Pending>>,
}

const STATE_LIFETIME: Duration = Duration::from_secs(10 * 60);
const STATE_CAPACITY: usize = 10_000;

impl PendingStates {
    /// Records a flow and returns its state; None when too many are already waiting.
    pub fn begin(
        &self,
        provider: &str,
        intent: Intent,
        user: Option<UserId>,
        code_verifier: String,
        redirect_uri: Url,
    ) -> Option<String> {
        let mut map = self.map.lock().unwrap_or_else(|e| e.into_inner());
        map.retain(|_, pending| pending.started.elapsed() < STATE_LIFETIME);
        if map.len() >= STATE_CAPACITY {
            return None;
        }
        let state = random_token();
        map.insert(
            state.clone(),
            Pending {
                provider: provider.to_string(),
                intent,
                user,
                code_verifier,
                redirect_uri,
                started: Instant::now(),
            },
        );
        Some(state)
    }

    /// The flow behind `state`, which is then spent.
    pub fn take(&self, state: &str) -> Option<Pending> {
        let mut map = self.map.lock().unwrap_or_else(|e| e.into_inner());
        let pending = map.remove(state)?;
        (pending.started.elapsed() < STATE_LIFETIME).then_some(pending)
    }
}

/// A provider identity linked to an account.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Identity {
    pub id: Uuid,
    pub user_id: UserId,
    pub provider: String,
    pub subject: String,
    pub username: Option<String>,
    pub display_name: Option<String>,
    pub email: Option<String>,
    pub scope: Option<String>,
    pub expires_at: Option<Timestamp>,
    pub has_refresh_token: bool,
    pub linked_at: Timestamp,
    pub updated_at: Timestamp,
}

/// An identity's tokens, opened.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tokens {
    pub access_token: String,
    pub refresh_token: Option<String>,
}

fn context(id: Uuid, column: &str) -> String {
    format!("oauth_identities:{id}:{column}")
}

#[derive(Clone)]
pub struct OAuthStore {
    db: SqliteStore,
    keyring: Keyring,
}

const SELECT: &str = "SELECT id, user_id, provider, subject, username, display_name, email, scope, \
     expires_at, refresh_token IS NOT NULL, linked_at, updated_at";

impl OAuthStore {
    /// Over a database the application's migrations have been applied to.
    pub fn new(db: SqliteStore, keyring: Keyring) -> Self {
        Self { db, keyring }
    }

    /// Ties a provider identity to `user`, storing its tokens sealed. Linking the same
    /// identity again renews its tokens and profile.
    pub async fn link(
        &self,
        user: UserId,
        provider: &str,
        remote: &RemoteIdentity,
        tokens: &TokenSet,
    ) -> Result<Identity, OAuthError> {
        let keyring = self.keyring.clone();
        let provider = provider.to_string();
        let remote = remote.clone();
        let tokens = tokens.clone();
        transact(&self.db, move |tx| {
            let now = Timestamp::now();
            let existing = tx
                .query_row(
                    &format!("{SELECT} FROM oauth_identities WHERE provider = ?1 AND subject = ?2"),
                    params![provider, remote.subject],
                    row_to_identity,
                )
                .optional()?;
            if let Some(existing) = existing {
                if existing.user_id != user {
                    return Err(OAuthError::AlreadyLinked(provider));
                }
                return update_in(tx, &keyring, existing, Some(&remote), Some(&tokens), now);
            }
            let other: Option<String> = tx
                .query_row(
                    "SELECT subject FROM oauth_identities WHERE user_id = ?1 AND provider = ?2",
                    params![user.to_string(), provider],
                    |row| row.get(0),
                )
                .optional()?;
            if other.is_some() {
                return Err(OAuthError::ProviderLinked(provider));
            }
            let id = Uuid::now_v7();
            tx.execute(
                "INSERT INTO oauth_identities (id, user_id, provider, subject, username, display_name, email, \
                 scope, access_token, refresh_token, expires_at, linked_at, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
                params![
                    id.to_string(),
                    user.to_string(),
                    provider,
                    remote.subject,
                    remote.username,
                    remote.display_name,
                    remote.email,
                    tokens.scope,
                    keyring.seal_str(&tokens.access_token, &context(id, "access_token")),
                    tokens
                        .refresh_token
                        .as_deref()
                        .map(|t| keyring.seal_str(t, &context(id, "refresh_token"))),
                    tokens.expires_at.map(nanos),
                    nanos(now),
                    nanos(now),
                ],
            )?;
            Ok(Identity {
                id,
                user_id: user,
                provider,
                subject: remote.subject,
                username: remote.username,
                display_name: remote.display_name,
                email: remote.email,
                scope: tokens.scope,
                expires_at: tokens.expires_at,
                has_refresh_token: tokens.refresh_token.is_some(),
                linked_at: now,
                updated_at: now,
            })
        })
        .await
    }

    /// The account a provider identity is linked to, if any.
    pub async fn find(
        &self,
        provider: &str,
        subject: &str,
    ) -> Result<Option<Identity>, OAuthError> {
        let provider = provider.to_string();
        let subject = subject.to_string();
        transact(&self.db, move |tx| {
            Ok(tx
                .query_row(
                    &format!("{SELECT} FROM oauth_identities WHERE provider = ?1 AND subject = ?2"),
                    params![provider, subject],
                    row_to_identity,
                )
                .optional()?)
        })
        .await
    }

    pub async fn list_for(&self, user: UserId) -> Result<Vec<Identity>, OAuthError> {
        transact(&self.db, move |tx| {
            let mut stmt = tx.prepare(&format!(
                "{SELECT} FROM oauth_identities WHERE user_id = ?1 ORDER BY linked_at, id"
            ))?;
            let rows = stmt.query_map(params![user.to_string()], row_to_identity)?;
            Ok(rows.collect::<Result<Vec<_>, _>>()?)
        })
        .await
    }

    /// `user`'s identity at `provider`, with its tokens opened.
    pub async fn get(
        &self,
        user: UserId,
        provider: &str,
    ) -> Result<(Identity, Tokens), OAuthError> {
        let keyring = self.keyring.clone();
        let provider = provider.to_string();
        transact(&self.db, move |tx| {
            let found = tx
                .query_row(
                    &format!(
                        "{SELECT}, access_token, refresh_token FROM oauth_identities \
                         WHERE user_id = ?1 AND provider = ?2"
                    ),
                    params![user.to_string(), provider],
                    |row| {
                        Ok((
                            row_to_identity(row)?,
                            row.get::<_, String>(12)?,
                            row.get::<_, Option<String>>(13)?,
                        ))
                    },
                )
                .optional()?;
            let (identity, access, refresh) = found.ok_or(OAuthError::NotLinked(provider))?;
            let tokens = open_tokens(&keyring, identity.id, &access, refresh.as_deref())?;
            Ok((identity, tokens))
        })
        .await
    }

    /// Renews an identity's profile, its tokens, or both.
    pub async fn update(
        &self,
        id: Uuid,
        remote: Option<&RemoteIdentity>,
        tokens: Option<&TokenSet>,
    ) -> Result<Identity, OAuthError> {
        let keyring = self.keyring.clone();
        let remote = remote.cloned();
        let tokens = tokens.cloned();
        transact(&self.db, move |tx| {
            let identity = tx
                .query_row(
                    &format!("{SELECT} FROM oauth_identities WHERE id = ?1"),
                    params![id.to_string()],
                    row_to_identity,
                )
                .optional()?
                .ok_or(OAuthError::Gone(id))?;
            update_in(
                tx,
                &keyring,
                identity,
                remote.as_ref(),
                tokens.as_ref(),
                Timestamp::now(),
            )
        })
        .await
    }

    /// Removes `user`'s identity at `provider`, returning it with its tokens so the grant
    /// can be revoked. An account without a password keeps its last identity.
    pub async fn unlink(
        &self,
        user: UserId,
        provider: &str,
    ) -> Result<(Identity, Tokens), OAuthError> {
        let keyring = self.keyring.clone();
        let provider = provider.to_string();
        transact(&self.db, move |tx| {
            let has_password: bool = tx.query_row(
                "SELECT password_hash IS NOT NULL FROM users WHERE id = ?1",
                params![user.to_string()],
                |row| row.get(0),
            )?;
            let linked: i64 = tx.query_row(
                "SELECT COUNT(*) FROM oauth_identities WHERE user_id = ?1",
                params![user.to_string()],
                |row| row.get(0),
            )?;
            let mut removed = remove_in(
                tx,
                &keyring,
                "user_id = ?1 AND provider = ?2",
                params![user.to_string(), provider],
            )?;
            let (identity, tokens) = removed.pop().ok_or(OAuthError::NotLinked(provider))?;
            if !has_password && linked <= 1 {
                return Err(OAuthError::LastLogin);
            }
            Ok((identity, tokens))
        })
        .await
    }

    /// Every identity of `user` with its tokens opened, for revoking their grants when the
    /// account goes.
    pub async fn all_tokens_for(
        &self,
        user: UserId,
    ) -> Result<Vec<(Identity, Tokens)>, OAuthError> {
        let keyring = self.keyring.clone();
        transact(&self.db, move |tx| {
            let mut stmt = tx.prepare(&format!(
                "{SELECT}, access_token, refresh_token FROM oauth_identities WHERE user_id = ?1 ORDER BY linked_at, id"
            ))?;
            let rows = stmt.query_map(params![user.to_string()], |row| {
                Ok((
                    row_to_identity(row)?,
                    row.get::<_, String>(12)?,
                    row.get::<_, Option<String>>(13)?,
                ))
            })?;
            let mut all = Vec::new();
            for row in rows {
                let (identity, access, refresh) = row?;
                all.push((identity.clone(), open_tokens(&keyring, identity.id, &access, refresh.as_deref())?));
            }
            Ok(all)
        })
        .await
    }
}

fn open_tokens(
    keyring: &Keyring,
    id: Uuid,
    access: &str,
    refresh: Option<&str>,
) -> Result<Tokens, OAuthError> {
    Ok(Tokens {
        access_token: keyring.open_str(access, &context(id, "access_token"))?,
        refresh_token: refresh
            .map(|sealed| keyring.open_str(sealed, &context(id, "refresh_token")))
            .transpose()?,
    })
}

fn update_in(
    tx: &rusqlite::Transaction<'_>,
    keyring: &Keyring,
    mut identity: Identity,
    remote: Option<&RemoteIdentity>,
    tokens: Option<&TokenSet>,
    now: Timestamp,
) -> Result<Identity, OAuthError> {
    if let Some(remote) = remote {
        tx.execute(
            "UPDATE oauth_identities SET username = ?2, display_name = ?3, email = ?4 WHERE id = ?1",
            params![
                identity.id.to_string(),
                remote.username,
                remote.display_name,
                remote.email
            ],
        )?;
        identity.username = remote.username.clone();
        identity.display_name = remote.display_name.clone();
        identity.email = remote.email.clone();
    }
    if let Some(tokens) = tokens {
        tx.execute(
            "UPDATE oauth_identities SET access_token = ?2, refresh_token = ?3, expires_at = ?4, scope = ?5 \
             WHERE id = ?1",
            params![
                identity.id.to_string(),
                keyring.seal_str(&tokens.access_token, &context(identity.id, "access_token")),
                tokens
                    .refresh_token
                    .as_deref()
                    .map(|t| keyring.seal_str(t, &context(identity.id, "refresh_token"))),
                tokens.expires_at.map(nanos),
                tokens.scope,
            ],
        )?;
        identity.expires_at = tokens.expires_at;
        identity.has_refresh_token = tokens.refresh_token.is_some();
        identity.scope = tokens.scope.clone();
    }
    tx.execute(
        "UPDATE oauth_identities SET updated_at = ?2 WHERE id = ?1",
        params![identity.id.to_string(), nanos(now)],
    )?;
    identity.updated_at = now;
    Ok(identity)
}

fn remove_in(
    tx: &rusqlite::Transaction<'_>,
    keyring: &Keyring,
    filter: &str,
    args: impl rusqlite::Params,
) -> Result<Vec<(Identity, Tokens)>, OAuthError> {
    let mut stmt = tx.prepare(&format!(
        "DELETE FROM oauth_identities WHERE {filter} RETURNING {}, access_token, refresh_token",
        SELECT.trim_start_matches("SELECT ")
    ))?;
    let rows = stmt.query_map(args, |row| {
        Ok((
            row_to_identity(row)?,
            row.get::<_, String>(12)?,
            row.get::<_, Option<String>>(13)?,
        ))
    })?;
    let mut removed = Vec::new();
    for row in rows {
        let (identity, access, refresh) = row?;
        let tokens = open_tokens(keyring, identity.id, &access, refresh.as_deref())?;
        removed.push((identity, tokens));
    }
    Ok(removed)
}

fn row_to_identity(row: &rusqlite::Row<'_>) -> rusqlite::Result<Identity> {
    let corrupt = |message: String| {
        rusqlite::Error::FromSqlConversionFailure(
            0,
            rusqlite::types::Type::Text,
            Box::new(StoreError::Corrupt(message)),
        )
    };
    let id: String = row.get(0)?;
    let user_id: String = row.get(1)?;
    let expires_at: Option<i64> = row.get(8)?;
    Ok(Identity {
        id: id
            .parse()
            .map_err(|e| corrupt(format!("identity id {id}: {e}")))?,
        user_id: user_id
            .parse()
            .map_err(|e| corrupt(format!("identity user {user_id}: {e}")))?,
        provider: row.get(2)?,
        subject: row.get(3)?,
        username: row.get(4)?,
        display_name: row.get(5)?,
        email: row.get(6)?,
        scope: row.get(7)?,
        expires_at: expires_at
            .map(|at| timestamp("oauth_identities.expires_at", at))
            .transpose()
            .map_err(|e| corrupt(e.to_string()))?,
        has_refresh_token: row.get(9)?,
        linked_at: timestamp("oauth_identities.linked_at", row.get(10)?)
            .map_err(|e| corrupt(e.to_string()))?,
        updated_at: timestamp("oauth_identities.updated_at", row.get(11)?)
            .map_err(|e| corrupt(e.to_string()))?,
    })
}

/// Everything the web app needs to run the flows.
pub struct OAuthService {
    pub registry: RwLock<Registry>,
    pub store: OAuthStore,
    pub http: reqwest::Client,
    pub states: PendingStates,
    /// Create a viewer account for an identity nobody has, at its first login.
    pub signup: std::sync::atomic::AtomicBool,
}

impl OAuthService {
    pub fn signup(&self) -> bool {
        self.signup.load(std::sync::atomic::Ordering::Relaxed)
    }
}

/// How much life an access token must have left to be used as is.
const REFRESH_MARGIN: jiff::SignedDuration = jiff::SignedDuration::from_secs(60);

impl OAuthService {
    pub fn provider(&self, id: &str) -> Result<Arc<Provider>, OAuthError> {
        self.registry
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .get(id)
    }

    pub fn providers(&self) -> Vec<ProviderInfo> {
        self.registry
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .list()
    }

    /// Starts a flow: the URL to send the browser to.
    pub async fn begin(
        &self,
        provider_id: &str,
        intent: Intent,
        user: Option<UserId>,
        redirect_uri: Url,
    ) -> Result<Option<Url>, OAuthError> {
        let provider = self.provider(provider_id)?;
        let (verifier, challenge) = pkce();
        let Some(state) =
            self.states
                .begin(provider_id, intent, user, verifier, redirect_uri.clone())
        else {
            return Ok(None);
        };
        provider
            .authorize_url(&self.http, &redirect_uri, &state, &challenge)
            .await
            .map(Some)
    }

    /// Finishes a flow: the tokens the code buys and who they belong to.
    pub async fn complete(
        &self,
        pending: &Pending,
        code: &str,
    ) -> Result<(Arc<Provider>, RemoteIdentity, TokenSet), OAuthError> {
        let provider = self.provider(&pending.provider)?;
        let tokens = provider
            .exchange(
                &self.http,
                code,
                &pending.code_verifier,
                &pending.redirect_uri,
            )
            .await?;
        let remote = provider.identity(&self.http, &tokens.access_token).await?;
        Ok((provider, remote, tokens))
    }

    /// `user`'s tokens at `provider_id`, renewed first when the access token is about to
    /// expire; whether they were renewed.
    pub async fn tokens(
        &self,
        user: UserId,
        provider_id: &str,
    ) -> Result<(Identity, Tokens, bool), OAuthError> {
        let provider = self.provider(provider_id)?;
        let (identity, tokens) = self.store.get(user, provider_id).await?;
        let fresh = identity
            .expires_at
            .is_none_or(|at| at - REFRESH_MARGIN > Timestamp::now());
        if fresh {
            return Ok((identity, tokens, false));
        }
        let refresh = tokens
            .refresh_token
            .as_deref()
            .ok_or_else(|| OAuthError::NoRefreshToken(provider.name.clone()))?;
        let renewed = provider.refresh(&self.http, refresh).await?;
        let identity = self.store.update(identity.id, None, Some(&renewed)).await?;
        Ok((
            identity,
            Tokens {
                access_token: renewed.access_token,
                refresh_token: renewed.refresh_token,
            },
            true,
        ))
    }

    /// Fetches the identity's profile from the provider again, renewing tokens on the way.
    pub async fn refresh_profile(
        &self,
        user: UserId,
        provider_id: &str,
    ) -> Result<Identity, OAuthError> {
        let provider = self.provider(provider_id)?;
        let (identity, tokens, _) = self.tokens(user, provider_id).await?;
        let remote = provider.identity(&self.http, &tokens.access_token).await?;
        self.store.update(identity.id, Some(&remote), None).await
    }

    /// Removes the link and tells the provider to forget the grant; whether the provider
    /// confirmed that.
    pub async fn unlink(
        &self,
        user: UserId,
        provider_id: &str,
    ) -> Result<(Identity, bool), OAuthError> {
        let provider = self.provider(provider_id)?;
        let (identity, tokens) = self.store.unlink(user, provider_id).await?;
        let revoked = match provider.revoke(&self.http, &tokens).await {
            Ok(revoked) => revoked,
            Err(error) => {
                tracing::warn!(provider = provider_id, %error, "grant not revoked at the provider");
                false
            }
        };
        Ok((identity, revoked))
    }

    /// Tells each provider to forget the grants behind `identities`, as when their account
    /// is deleted.
    pub async fn revoke_grants(&self, identities: &[(Identity, Tokens)]) {
        for (identity, tokens) in identities {
            match self.provider(&identity.provider) {
                Ok(provider) => {
                    if let Err(error) = provider.revoke(&self.http, tokens).await {
                        tracing::warn!(provider = identity.provider, %error, "grant not revoked at the provider");
                    }
                }
                Err(_) => tracing::warn!(
                    provider = identity.provider,
                    "grant not revoked: the provider is no longer configured"
                ),
            }
        }
    }
}

/// A username for a new account, from what the provider knows.
pub fn suggested_username(provider_id: &str, remote: &RemoteIdentity) -> String {
    let raw = remote
        .username
        .as_deref()
        .or_else(|| remote.email.as_deref().and_then(|e| e.split('@').next()))
        .or(remote.display_name.as_deref())
        .unwrap_or(provider_id);
    let mut name: String = raw
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                c
            } else {
                '_'
            }
        })
        .take(crate::users::USERNAME_MAX)
        .collect();
    if name.is_empty() {
        name = provider_id.to_string();
    }
    name
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::users::{Role, UserStore};

    async fn store() -> (OAuthStore, UserId, UserId) {
        let db = SqliteStore::open_in_memory().await.unwrap();
        crate::migrations::apply(&db).await.unwrap();
        let users = UserStore::new(db.clone());
        let a = users.set_up("a", "correct horse").await.unwrap().id;
        let b = users.create("b", None, Role::Viewer).await.unwrap().id;
        (OAuthStore::new(db, Keyring::from_key([1; 32])), a, b)
    }

    fn remote(subject: &str) -> RemoteIdentity {
        RemoteIdentity {
            subject: subject.into(),
            username: Some("octo".into()),
            display_name: Some("Octo Cat".into()),
            email: None,
        }
    }

    fn tokens(access: &str) -> TokenSet {
        TokenSet {
            access_token: access.into(),
            refresh_token: Some(format!("refresh-{access}")),
            expires_at: Some(Timestamp::now() + jiff::SignedDuration::from_hours(1)),
            scope: Some("read:user".into()),
        }
    }

    #[tokio::test]
    async fn identities_link_once_per_provider_and_open_their_tokens() {
        let (store, a, b) = store().await;
        let linked = store
            .link(a, "github", &remote("1"), &tokens("t1"))
            .await
            .unwrap();
        assert_eq!(linked.subject, "1");
        assert!(linked.has_refresh_token);
        assert_eq!(
            store.find("github", "1").await.unwrap(),
            Some(linked.clone())
        );
        assert!(store.find("github", "2").await.unwrap().is_none());
        let (identity, opened) = store.get(a, "github").await.unwrap();
        assert_eq!(identity, linked);
        assert_eq!(opened.access_token, "t1");
        assert_eq!(opened.refresh_token.as_deref(), Some("refresh-t1"));

        assert!(matches!(
            store.link(b, "github", &remote("1"), &tokens("t2")).await,
            Err(OAuthError::AlreadyLinked(_))
        ));
        assert!(matches!(
            store.link(a, "github", &remote("9"), &tokens("t2")).await,
            Err(OAuthError::ProviderLinked(_))
        ));
        let again = store
            .link(a, "github", &remote("1"), &tokens("t3"))
            .await
            .unwrap();
        assert_eq!(again.id, linked.id);
        assert_eq!(store.get(a, "github").await.unwrap().1.access_token, "t3");
        store
            .link(a, "discord", &remote("d"), &tokens("t4"))
            .await
            .unwrap();
        assert_eq!(store.list_for(a).await.unwrap().len(), 2);
        assert!(matches!(
            store.get(b, "github").await,
            Err(OAuthError::NotLinked(_))
        ));
    }

    #[tokio::test]
    async fn tokens_are_sealed_at_rest() {
        let (store, a, _) = store().await;
        let linked = store
            .link(a, "github", &remote("1"), &tokens("plain-token"))
            .await
            .unwrap();
        let (access, refresh): (String, String) = store
            .db
            .call(move |conn| {
                Ok(conn.query_row(
                    "SELECT access_token, refresh_token FROM oauth_identities WHERE id = ?1",
                    params![linked.id.to_string()],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )?)
            })
            .await
            .unwrap();
        assert!(access.starts_with("v1."));
        assert!(!access.contains("plain-token"));
        assert!(refresh.starts_with("v1."));
        let other = OAuthStore::new(store.db.clone(), Keyring::from_key([2; 32]));
        assert!(matches!(
            other.get(a, "github").await,
            Err(OAuthError::Secret(_))
        ));
    }

    #[tokio::test]
    async fn unlinking_keeps_the_last_login_of_a_passwordless_account() {
        let (store, a, b) = store().await;
        store
            .link(b, "github", &remote("1"), &tokens("t1"))
            .await
            .unwrap();
        assert!(matches!(
            store.unlink(b, "github").await,
            Err(OAuthError::LastLogin)
        ));
        assert_eq!(store.list_for(b).await.unwrap().len(), 1);
        store
            .link(b, "discord", &remote("d"), &tokens("t2"))
            .await
            .unwrap();
        let (identity, opened) = store.unlink(b, "github").await.unwrap();
        assert_eq!(identity.subject, "1");
        assert_eq!(opened.access_token, "t1");
        assert!(matches!(
            store.unlink(b, "github").await,
            Err(OAuthError::NotLinked(_))
        ));
        // With a password, the last identity may go.
        store
            .link(a, "github", &remote("2"), &tokens("t3"))
            .await
            .unwrap();
        store.unlink(a, "github").await.unwrap();
        assert!(store.list_for(a).await.unwrap().is_empty());
        let all = store.all_tokens_for(b).await.unwrap();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].1.access_token, "t2");
        assert!(store.all_tokens_for(a).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn updates_renew_profile_and_tokens() {
        let (store, a, _) = store().await;
        let linked = store
            .link(a, "github", &remote("1"), &tokens("t1"))
            .await
            .unwrap();
        let renewed = TokenSet {
            access_token: "t2".into(),
            refresh_token: None,
            expires_at: None,
            scope: None,
        };
        let updated = store.update(linked.id, None, Some(&renewed)).await.unwrap();
        assert!(!updated.has_refresh_token);
        assert!(updated.expires_at.is_none());
        assert_eq!(updated.username.as_deref(), Some("octo"));
        let (_, opened) = store.get(a, "github").await.unwrap();
        assert_eq!(opened.access_token, "t2");
        assert!(opened.refresh_token.is_none());
        let profile = RemoteIdentity {
            subject: "1".into(),
            username: Some("octocat".into()),
            display_name: None,
            email: Some("o@example.com".into()),
        };
        let updated = store.update(linked.id, Some(&profile), None).await.unwrap();
        assert_eq!(updated.username.as_deref(), Some("octocat"));
        assert_eq!(updated.email.as_deref(), Some("o@example.com"));
        assert!(updated.display_name.is_none());
    }

    #[test]
    fn pending_states_are_single_use() {
        let states = PendingStates::default();
        let uri = Url::parse("http://localhost/cb").unwrap();
        let state = states
            .begin("github", Intent::Login, None, "v".into(), uri.clone())
            .unwrap();
        assert_eq!(state.len(), 43);
        let pending = states.take(&state).unwrap();
        assert_eq!(pending.provider, "github");
        assert_eq!(pending.intent, Intent::Login);
        assert_eq!(pending.code_verifier, "v");
        assert_eq!(pending.redirect_uri, uri);
        assert!(states.take(&state).is_none());
        assert!(states.take("nope").is_none());
    }

    #[test]
    fn pkce_challenges_are_s256() {
        let (verifier, challenge) = pkce();
        assert_eq!(verifier.len(), 43);
        assert_eq!(
            challenge,
            URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()))
        );
    }

    #[test]
    fn usernames_are_suggested_from_the_profile() {
        let suggest = |username: Option<&str>, email: Option<&str>, display: Option<&str>| {
            suggested_username(
                "oidc",
                &RemoteIdentity {
                    subject: "s".into(),
                    username: username.map(Into::into),
                    display_name: display.map(Into::into),
                    email: email.map(Into::into),
                },
            )
        };
        assert_eq!(suggest(Some("Octo Cat!"), None, None), "Octo_Cat_");
        assert_eq!(suggest(None, Some("nick@example.com"), None), "nick");
        assert_eq!(suggest(None, None, Some("Nick H")), "Nick_H");
        assert_eq!(suggest(None, None, None), "oidc");
        assert_eq!(suggest(Some(&"x".repeat(40)), None, None).len(), 32);
    }

    #[tokio::test]
    async fn registries_come_from_settings() {
        let client = OAuthClient {
            client_id: "id".into(),
            client_secret: "secret".into(),
        };
        let auth = AuthConfig {
            github: None,
            google: Some(client.clone()),
            oidc: Some(OidcClient {
                name: "Work".into(),
                issuer: Url::parse("https://issuer.example/realms/x").unwrap(),
                client_id: "id".into(),
                client_secret: "secret".into(),
                scopes: vec!["openid".into()],
            }),
            oauth_signup: false,
        };
        let mut registry = Registry::from_config(&auth);
        let ids: Vec<String> = registry.list().into_iter().map(|p| p.id).collect();
        assert_eq!(ids, vec!["google", "oidc"]);
        registry.set_discord(Some(Provider::discord(
            &client,
            &DiscordEndpoints::default(),
        )));
        let ids: Vec<String> = registry.list().into_iter().map(|p| p.id).collect();
        assert_eq!(ids, vec!["discord", "google", "oidc"]);
        assert_eq!(registry.get("oidc").unwrap().name, "Work");
        assert_eq!(registry.get("google").unwrap().kind, Kind::Oidc);
        assert!(matches!(
            registry.get("github"),
            Err(OAuthError::UnknownProvider(_))
        ));
        let discord = registry.get("discord").unwrap();
        let location = discord
            .authorize_url(
                &reqwest::Client::new(),
                &Url::parse("http://localhost:8080/api/auth/discord/callback").unwrap(),
                "st",
                "ch",
            )
            .await
            .unwrap();
        assert_eq!(location.host_str(), Some("discord.com"));
        let query: HashMap<String, String> = location.query_pairs().into_owned().collect();
        assert_eq!(query["client_id"], "id");
        assert_eq!(query["scope"], "identify guilds guilds.members.read");
        assert_eq!(
            discord
                .discord_guilds_url(&reqwest::Client::new())
                .await
                .unwrap()
                .unwrap()
                .as_str(),
            "https://discord.com/api/v10/users/@me/guilds"
        );
        registry.set_discord(None);
        assert!(registry.get("discord").is_err());
        let mut configured = Registry::new(vec![Provider::discord(
            &client,
            &DiscordEndpoints::default(),
        )]);
        configured.set_discord(None);
        assert!(configured.get("discord").is_ok());
        configured.set_discord(Some(Provider::discord(
            &client,
            &DiscordEndpoints::default(),
        )));
        configured.set_discord(None);
        assert!(configured.get("discord").is_err());
        let proxied = Provider::discord(
            &client,
            &DiscordEndpoints {
                http_proxy: Some("127.0.0.1:9".into()),
                gateway_url: None,
            },
        );
        assert_eq!(
            proxied
                .authorize_url(
                    &reqwest::Client::new(),
                    &Url::parse("http://localhost/cb").unwrap(),
                    "s",
                    "c"
                )
                .await
                .unwrap()
                .as_str()
                .split('?')
                .next()
                .unwrap(),
            "http://127.0.0.1:9/oauth2/authorize"
        );
        assert!(
            registry
                .get("google")
                .unwrap()
                .discord_guilds_url(&reqwest::Client::new())
                .await
                .unwrap()
                .is_none()
        );
        assert_eq!(query["state"], "st");
        assert_eq!(query["code_challenge"], "ch");
        assert_eq!(query["code_challenge_method"], "S256");
        assert_eq!(
            query["redirect_uri"],
            "http://localhost:8080/api/auth/discord/callback"
        );
    }
}
