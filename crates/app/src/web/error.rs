//! Errors as the API reports them: a status and a JSON body with one `error` message.

use std::time::Duration;

use axum::Json;
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use discoclip_engine::StoreError;
use serde_json::json;

use crate::applications::ApplicationError;
use crate::cookies::CookieError;
use crate::oauth::OAuthError;
use crate::rules::RuleError;
use crate::tokens::TokenError;
use crate::users::UserError;

#[derive(Debug)]
pub enum ApiError {
    BadRequest(String),
    Unauthorized,
    Forbidden(String),
    NotFound,
    Conflict(String),
    TooManyRequests(Duration),
    /// Another service the request depends on failed.
    BadGateway(String),
    /// The server cannot take the request right now.
    Unavailable(String),
    /// A `Range` header asks for bytes outside a file of this length.
    RangeNotSatisfiable(u64),
    Internal(String),
}

impl ApiError {
    /// The message the response carries.
    pub fn message(&self) -> String {
        self.status_and_message().1
    }

    fn status_and_message(&self) -> (StatusCode, String) {
        match self {
            ApiError::BadRequest(message) => (StatusCode::BAD_REQUEST, message.clone()),
            ApiError::Unauthorized => (StatusCode::UNAUTHORIZED, "not logged in".to_string()),
            ApiError::Forbidden(message) => (StatusCode::FORBIDDEN, message.clone()),
            ApiError::NotFound => (StatusCode::NOT_FOUND, "not found".to_string()),
            ApiError::Conflict(message) => (StatusCode::CONFLICT, message.clone()),
            ApiError::TooManyRequests(retry) => (
                StatusCode::TOO_MANY_REQUESTS,
                format!(
                    "too many attempts; try again in {} seconds",
                    retry_secs(*retry)
                ),
            ),
            ApiError::BadGateway(message) => {
                tracing::warn!("{message}");
                (StatusCode::BAD_GATEWAY, message.clone())
            }
            ApiError::Unavailable(message) => (StatusCode::SERVICE_UNAVAILABLE, message.clone()),
            ApiError::RangeNotSatisfiable(len) => (
                StatusCode::RANGE_NOT_SATISFIABLE,
                format!("the range asked for lies outside the {len} bytes of the file"),
            ),
            ApiError::Internal(message) => {
                tracing::error!("{message}");
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "internal error".to_string(),
                )
            }
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let (status, message) = self.status_and_message();
        let mut response = (status, Json(json!({ "error": message }))).into_response();
        match self {
            ApiError::TooManyRequests(retry) => {
                response
                    .headers_mut()
                    .insert(header::RETRY_AFTER, HeaderValue::from(retry_secs(retry)));
            }
            ApiError::RangeNotSatisfiable(len) => {
                if let Ok(value) = HeaderValue::from_str(&format!("bytes */{len}")) {
                    response.headers_mut().insert(header::CONTENT_RANGE, value);
                }
            }
            _ => {}
        }
        response
    }
}

fn retry_secs(retry: Duration) -> u64 {
    retry.as_secs_f64().ceil() as u64
}

impl From<StoreError> for ApiError {
    fn from(error: StoreError) -> Self {
        ApiError::Internal(error.to_string())
    }
}

impl From<UserError> for ApiError {
    fn from(error: UserError) -> Self {
        match error {
            UserError::InvalidUsername | UserError::InvalidPassword => {
                ApiError::BadRequest(error.to_string())
            }
            UserError::Taken(_) | UserError::AlreadySetUp | UserError::LastAdmin => {
                ApiError::Conflict(error.to_string())
            }
            UserError::NotFound(_) => ApiError::NotFound,
            UserError::Store(_) | UserError::Hash(_) => ApiError::Internal(error.to_string()),
        }
    }
}

impl From<OAuthError> for ApiError {
    fn from(error: OAuthError) -> Self {
        match error {
            OAuthError::UnknownProvider(_) | OAuthError::NotLinked(_) | OAuthError::Gone(_) => {
                ApiError::NotFound
            }
            OAuthError::AlreadyLinked(_)
            | OAuthError::ProviderLinked(_)
            | OAuthError::LastLogin
            | OAuthError::NoRefreshToken(_) => ApiError::Conflict(error.to_string()),
            OAuthError::Discovery { .. }
            | OAuthError::Http { .. }
            | OAuthError::Refused { .. }
            | OAuthError::Malformed { .. } => ApiError::BadGateway(error.to_string()),
            OAuthError::Store(_) | OAuthError::Secret(_) => ApiError::Internal(error.to_string()),
        }
    }
}

impl From<TokenError> for ApiError {
    fn from(error: TokenError) -> Self {
        match error {
            TokenError::InvalidName | TokenError::InvalidExpiry => {
                ApiError::BadRequest(error.to_string())
            }
            TokenError::NotFound(_) => ApiError::NotFound,
            TokenError::Store(_) => ApiError::Internal(error.to_string()),
        }
    }
}

impl From<ApplicationError> for ApiError {
    fn from(error: ApplicationError) -> Self {
        match error {
            ApplicationError::NotFound(_) => ApiError::NotFound,
            ApplicationError::InvalidName
            | ApplicationError::LoginNeedsSecret
            | ApplicationError::InvalidGuild(_) => ApiError::BadRequest(error.to_string()),
            ApplicationError::Duplicate(_) => ApiError::Conflict(error.to_string()),
            ApplicationError::Store(_) | ApplicationError::Secret(_) => {
                ApiError::Internal(error.to_string())
            }
        }
    }
}

impl From<super::WebError> for ApiError {
    fn from(error: super::WebError) -> Self {
        ApiError::Internal(error.to_string())
    }
}

impl From<RuleError> for ApiError {
    fn from(error: RuleError) -> Self {
        match error {
            RuleError::NotFound(_) => ApiError::NotFound,
            RuleError::Invalid(_) => ApiError::BadRequest(error.to_string()),
            RuleError::Duplicate(_) => ApiError::Conflict(error.to_string()),
            RuleError::Store(_) => ApiError::Internal(error.to_string()),
        }
    }
}

impl From<discoclip_bot::ControlError> for ApiError {
    fn from(error: discoclip_bot::ControlError) -> Self {
        match error {
            discoclip_bot::ControlError::Gone => ApiError::NotFound,
            discoclip_bot::ControlError::Disabled => ApiError::Conflict(error.to_string()),
        }
    }
}

impl From<discoclip_engine::SubmitError> for ApiError {
    fn from(error: discoclip_engine::SubmitError) -> Self {
        use discoclip_engine::SubmitError;
        match error {
            SubmitError::Unsupported(_) => ApiError::BadRequest(error.to_string()),
            SubmitError::UnknownSource(_) => ApiError::Internal(error.to_string()),
            SubmitError::Closed => ApiError::Unavailable(error.to_string()),
            SubmitError::Store(_) => ApiError::Internal(error.to_string()),
        }
    }
}

impl From<discoclip_engine::RetryError> for ApiError {
    fn from(error: discoclip_engine::RetryError) -> Self {
        use discoclip_engine::RetryError;
        match error {
            RetryError::NotFound(_) => ApiError::NotFound,
            RetryError::NotFinished(_) => ApiError::Conflict(error.to_string()),
            RetryError::Submit(inner) => inner.into(),
            RetryError::Store(_) => ApiError::Internal(error.to_string()),
        }
    }
}

impl From<discoclip_engine::CancelError> for ApiError {
    fn from(error: discoclip_engine::CancelError) -> Self {
        use discoclip_engine::CancelError;
        match error {
            CancelError::NotFound(_) => ApiError::NotFound,
            CancelError::Finished(_) => ApiError::Conflict(error.to_string()),
            CancelError::Store(_) => ApiError::Internal(error.to_string()),
        }
    }
}

impl From<discoclip_engine::DeleteError> for ApiError {
    fn from(error: discoclip_engine::DeleteError) -> Self {
        use discoclip_engine::DeleteError;
        match error {
            DeleteError::NotFound(_) => ApiError::NotFound,
            DeleteError::NotFinished(_) => ApiError::Conflict(error.to_string()),
            DeleteError::Store(_) => ApiError::Internal(error.to_string()),
        }
    }
}

impl From<CookieError> for ApiError {
    fn from(error: CookieError) -> Self {
        ApiError::Internal(error.to_string())
    }
}
