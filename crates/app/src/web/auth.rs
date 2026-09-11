//! Who is asking: the session cookie, the CSRF token it must echo, first-run setup, login,
//! logout, and the sessions an account can see and end.

use std::net::{IpAddr, SocketAddr};

use axum::Json;
use axum::extract::{ConnectInfo, FromRequestParts, Path, Request, State};
use axum::http::request::Parts;
use axum::http::{HeaderMap, Method, StatusCode, header};
use axum::middleware::Next;
use axum::response::Response;
use axum_extra::extract::cookie::{Cookie, CookieJar, SameSite};
use jiff::Timestamp;
use serde::{Deserialize, Serialize};
use subtle::ConstantTimeEq;

use super::AppState;
use super::error::ApiError;
use crate::sessions::{ABSOLUTE_LIFETIME, Session, SessionId};
use crate::tokens::ApiToken;
use crate::users::{Permission, User};

pub const COOKIE: &str = "discoclip_session";
pub const CSRF_HEADER: &str = "x-csrf-token";

/// The account behind a request, established by the `identify` middleware.
#[derive(Debug, Clone)]
pub struct Identity {
    pub user: User,
    pub via: Via,
}

/// What carried the account's credentials.
#[derive(Debug, Clone)]
pub enum Via {
    /// A browser session cookie.
    Session {
        session: Session,
        csrf_token: String,
    },
    /// A bearer API token.
    Token(ApiToken),
}

impl Identity {
    /// 403 unless the account's role allows `permission` and, for an API token, the
    /// token was minted with it.
    pub fn require(&self, permission: Permission) -> Result<(), ApiError> {
        if !self.user.role.allows(permission) {
            return Err(ApiError::Forbidden(format!(
                "the {} role does not allow {permission}",
                self.user.role
            )));
        }
        if let Via::Token(token) = &self.via
            && !token.scopes.contains(&permission)
        {
            return Err(ApiError::Forbidden(format!(
                "this API token was not given {permission}"
            )));
        }
        Ok(())
    }

    /// The browser session behind the request; 403 for an API token.
    pub fn session(&self) -> Result<&Session, ApiError> {
        match &self.via {
            Via::Session { session, .. } => Ok(session),
            Via::Token(_) => Err(ApiError::Forbidden(
                "this needs a browser session, not an API token".into(),
            )),
        }
    }

    pub fn session_id(&self) -> Option<SessionId> {
        match &self.via {
            Via::Session { session, .. } => Some(session.id),
            Via::Token(_) => None,
        }
    }
}

/// The request's account, or 401.
pub struct Auth(pub Identity);

impl<S: Send + Sync> FromRequestParts<S> for Auth {
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, _: &S) -> Result<Self, ApiError> {
        parts
            .extensions
            .get::<Identity>()
            .cloned()
            .map(Auth)
            .ok_or(ApiError::Unauthorized)
    }
}

/// The request's account, when it has one.
pub struct MaybeAuth(pub Option<Identity>);

impl<S: Send + Sync> FromRequestParts<S> for MaybeAuth {
    type Rejection = std::convert::Infallible;

    async fn from_request_parts(parts: &mut Parts, _: &S) -> Result<Self, Self::Rejection> {
        Ok(MaybeAuth(parts.extensions.get::<Identity>().cloned()))
    }
}

/// The address the request came from.
pub struct ClientIp(pub IpAddr);

impl<S: Send + Sync> FromRequestParts<S> for ClientIp {
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, _: &S) -> Result<Self, ApiError> {
        parts
            .extensions
            .get::<ConnectInfo<SocketAddr>>()
            .map(|info| ClientIp(info.0.ip()))
            .ok_or_else(|| ApiError::Internal("client address unavailable".into()))
    }
}

/// Resolves a bearer API token, or else a live session cookie, into an [`Identity`]. A
/// bearer token that names nothing ends the request with 401, and counts against the
/// address like a wrong password.
pub async fn identify(
    State(state): State<AppState>,
    ClientIp(ip): ClientIp,
    jar: CookieJar,
    mut request: Request,
    next: Next,
) -> Result<Response, ApiError> {
    let bearer = request
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .map(str::trim)
        .map(str::to_string);
    if let Some(secret) = bearer {
        let ip_key = ip.to_string();
        state
            .limits
            .login_ip
            .check(&ip_key)
            .map_err(ApiError::TooManyRequests)?;
        let found = match state.tokens.authenticate(&secret).await? {
            Some(token) => state
                .users
                .get(token.user_id)
                .await?
                .map(|user| (user, token)),
            None => None,
        };
        let Some((user, token)) = found else {
            state.limits.login_ip.strike(&ip_key);
            return Err(ApiError::Unauthorized);
        };
        request.extensions_mut().insert(Identity {
            user,
            via: Via::Token(token),
        });
    } else if let Some(cookie) = jar.get(COOKIE)
        && let Some(auth) = state.sessions.authenticate(cookie.value()).await?
        && let Some(user) = state.users.get(auth.session.user_id).await?
    {
        request.extensions_mut().insert(Identity {
            user,
            via: Via::Session {
                session: auth.session,
                csrf_token: auth.csrf_token,
            },
        });
    }
    Ok(next.run(request).await)
}

/// Refuses cross-site state changes: the request's origin must be this host, and a
/// session must echo its CSRF token in the `x-csrf-token` header.
pub async fn csrf_guard(request: Request, next: Next) -> Result<Response, ApiError> {
    if !matches!(
        *request.method(),
        Method::GET | Method::HEAD | Method::OPTIONS
    ) {
        check_origin(request.headers())?;
        if let Some(Identity {
            via: Via::Session { csrf_token, .. },
            ..
        }) = request.extensions().get::<Identity>()
        {
            let sent = request
                .headers()
                .get(CSRF_HEADER)
                .and_then(|value| value.to_str().ok())
                .unwrap_or("");
            if !bool::from(sent.as_bytes().ct_eq(csrf_token.as_bytes())) {
                return Err(ApiError::Forbidden("missing or wrong CSRF token".into()));
            }
        }
    }
    Ok(next.run(request).await)
}

fn check_origin(headers: &HeaderMap) -> Result<(), ApiError> {
    if let Some(site) = headers
        .get("sec-fetch-site")
        .and_then(|value| value.to_str().ok())
        && !matches!(site, "same-origin" | "none")
    {
        return Err(ApiError::Forbidden("cross-site request".into()));
    }
    if let Some(origin) = headers
        .get(header::ORIGIN)
        .and_then(|value| value.to_str().ok())
    {
        let host = headers
            .get(header::HOST)
            .and_then(|value| value.to_str().ok())
            .unwrap_or("");
        if !same_authority(origin, host) {
            return Err(ApiError::Forbidden(
                "request origin does not match this host".into(),
            ));
        }
    }
    Ok(())
}

/// Whether an `Origin` header names the host and port a `Host` header does. Browsers leave
/// default ports out of both.
fn same_authority(origin: &str, host: &str) -> bool {
    origin
        .split_once("://")
        .is_some_and(|(_, authority)| !host.is_empty() && authority.eq_ignore_ascii_case(host))
}

#[derive(Debug, Serialize)]
pub struct SessionView {
    pub id: SessionId,
    pub created_at: Timestamp,
    pub last_seen_at: Timestamp,
    pub expires_at: Timestamp,
    pub user_agent: Option<String>,
    pub ip: Option<IpAddr>,
    /// Whether this is the session making the request.
    pub current: bool,
}

impl SessionView {
    pub fn of(session: &Session, current: SessionId) -> Self {
        Self {
            id: session.id,
            created_at: session.created_at,
            last_seen_at: session.last_seen_at,
            expires_at: session.expires_at(),
            user_agent: session.user_agent.clone(),
            ip: session.ip,
            current: session.id == current,
        }
    }
}

/// Who is asking: the account, and for a browser the CSRF token to send back, for a
/// script the token it used.
#[derive(Debug, Serialize)]
pub struct WhoAmI {
    pub user: User,
    pub csrf_token: Option<String>,
    pub session: Option<SessionView>,
    pub token: Option<ApiToken>,
}

impl WhoAmI {
    fn of(identity: Identity) -> Self {
        match identity.via {
            Via::Session {
                session,
                csrf_token,
            } => Self {
                user: identity.user,
                csrf_token: Some(csrf_token),
                session: Some(SessionView::of(&session, session.id)),
                token: None,
            },
            Via::Token(token) => Self {
                user: identity.user,
                csrf_token: None,
                session: None,
                token: Some(token),
            },
        }
    }
}

/// Opens a session for `user` and puts its cookie in the jar.
pub async fn start_session(
    state: &AppState,
    user: User,
    jar: CookieJar,
    headers: &HeaderMap,
    ip: IpAddr,
) -> Result<(CookieJar, Identity), ApiError> {
    let user_agent = headers
        .get(header::USER_AGENT)
        .and_then(|value| value.to_str().ok())
        .map(|agent| agent.chars().take(256).collect());
    let created = state.sessions.create(user.id, user_agent, Some(ip)).await?;
    let cookie = Cookie::build((COOKIE, created.token))
        .path("/")
        .http_only(true)
        .same_site(SameSite::Lax)
        .max_age(time::Duration::seconds(ABSOLUTE_LIFETIME.as_secs()))
        .build();
    let identity = Identity {
        user,
        via: Via::Session {
            session: created.session,
            csrf_token: created.csrf_token,
        },
    };
    Ok((jar.add(cookie), identity))
}

async fn open_session(
    state: &AppState,
    user: User,
    jar: CookieJar,
    headers: &HeaderMap,
    ip: IpAddr,
) -> Result<(CookieJar, Json<WhoAmI>), ApiError> {
    let (jar, identity) = start_session(state, user, jar, headers, ip).await?;
    Ok((jar, Json(WhoAmI::of(identity))))
}

pub fn cleared(jar: CookieJar) -> CookieJar {
    jar.remove(
        Cookie::build(COOKIE)
            .path("/")
            .http_only(true)
            .same_site(SameSite::Lax),
    )
}

#[derive(Debug, Serialize)]
pub struct SetupStatus {
    /// Whether the first account still has to be created.
    pub needed: bool,
}

pub async fn setup_status(State(state): State<AppState>) -> Result<Json<SetupStatus>, ApiError> {
    Ok(Json(SetupStatus {
        needed: !state.users.is_set_up().await?,
    }))
}

#[derive(Debug, Deserialize)]
pub struct SetupRequest {
    pub username: String,
    pub password: String,
    /// The token the server printed when it started without accounts.
    pub token: String,
}

/// Creates the first account, an admin, and logs it in.
pub async fn setup(
    State(state): State<AppState>,
    ClientIp(ip): ClientIp,
    jar: CookieJar,
    headers: HeaderMap,
    Json(request): Json<SetupRequest>,
) -> Result<(CookieJar, Json<WhoAmI>), ApiError> {
    let ip_key = ip.to_string();
    state
        .limits
        .setup
        .check(&ip_key)
        .map_err(ApiError::TooManyRequests)?;
    let expected = state
        .setup_token
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone()
        .ok_or_else(|| ApiError::Conflict(crate::users::UserError::AlreadySetUp.to_string()))?;
    if !bool::from(request.token.as_bytes().ct_eq(expected.as_bytes())) {
        state.limits.setup.strike(&ip_key);
        return Err(ApiError::Forbidden("wrong setup token".into()));
    }
    let user = state
        .users
        .set_up(&request.username, &request.password)
        .await?;
    *state.setup_token.lock().unwrap_or_else(|e| e.into_inner()) = None;
    tracing::info!(username = user.username, "first account created");
    open_session(&state, user, jar, &headers, ip).await
}

#[derive(Debug, Deserialize)]
pub struct LoginRequest {
    pub username: String,
    pub password: String,
}

pub async fn login(
    State(state): State<AppState>,
    ClientIp(ip): ClientIp,
    jar: CookieJar,
    headers: HeaderMap,
    Json(request): Json<LoginRequest>,
) -> Result<(CookieJar, Json<WhoAmI>), ApiError> {
    let user_key = request.username.to_ascii_lowercase();
    let ip_key = ip.to_string();
    let wait = [
        state.limits.login_user.check(&user_key),
        state.limits.login_ip.check(&ip_key),
    ]
    .into_iter()
    .filter_map(Result::err)
    .max();
    if let Some(wait) = wait {
        return Err(ApiError::TooManyRequests(wait));
    }
    match state
        .users
        .verify_login(&request.username, &request.password)
        .await?
    {
        Some(user) => {
            state.limits.login_user.clear(&user_key);
            tracing::info!(username = user.username, %ip, "logged in");
            open_session(&state, user, cleared(jar), &headers, ip).await
        }
        None => {
            state.limits.login_user.strike(&user_key);
            state.limits.login_ip.strike(&ip_key);
            tracing::info!(username = request.username, %ip, "login refused");
            Err(ApiError::Unauthorized)
        }
    }
}

pub async fn logout(
    State(state): State<AppState>,
    Auth(identity): Auth,
    jar: CookieJar,
) -> Result<(CookieJar, StatusCode), ApiError> {
    state.sessions.revoke(identity.session()?.id).await?;
    Ok((cleared(jar), StatusCode::NO_CONTENT))
}

pub async fn current_session(Auth(identity): Auth) -> Json<WhoAmI> {
    Json(WhoAmI::of(identity))
}

pub async fn list_sessions(
    State(state): State<AppState>,
    Auth(identity): Auth,
) -> Result<Json<Vec<SessionView>>, ApiError> {
    let current = identity.session()?.id;
    let sessions = state.sessions.list_for(identity.user.id).await?;
    Ok(Json(
        sessions
            .iter()
            .map(|session| SessionView::of(session, current))
            .collect(),
    ))
}

/// Ends one of the account's own sessions.
pub async fn revoke_session(
    State(state): State<AppState>,
    Auth(identity): Auth,
    Path(id): Path<String>,
    jar: CookieJar,
) -> Result<(CookieJar, StatusCode), ApiError> {
    let current = identity.session()?.id;
    let id: SessionId = parse_id(&id)?;
    let session = state.sessions.get(id).await?.ok_or(ApiError::NotFound)?;
    if session.user_id != identity.user.id {
        return Err(ApiError::NotFound);
    }
    state.sessions.revoke(id).await?;
    let jar = if id == current { cleared(jar) } else { jar };
    Ok((jar, StatusCode::NO_CONTENT))
}

#[derive(Debug, Serialize)]
pub struct Revoked {
    pub revoked: usize,
}

/// A path segment as an id, or 404.
pub fn parse_id<T: std::str::FromStr>(id: &str) -> Result<T, ApiError> {
    id.parse().map_err(|_| ApiError::NotFound)
}

/// Ends every session of the account but the one asking.
pub async fn revoke_other_sessions(
    State(state): State<AppState>,
    Auth(identity): Auth,
) -> Result<Json<Revoked>, ApiError> {
    let current = identity.session()?.id;
    let revoked = state
        .sessions
        .revoke_all_for(identity.user.id, Some(current))
        .await?;
    Ok(Json(Revoked { revoked }))
}
