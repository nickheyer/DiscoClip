//! The audit log, for admins: who changed which setting, application, rule or bot, newest
//! first, filtered and paged.

use axum::Json;
use axum::extract::{Query, State};
use serde::Deserialize;

use super::AppState;
use super::auth::Auth;
use super::error::ApiError;
use crate::audit::{Filter, Page};
use crate::users::Permission;

/// Every field is optional; see [`Filter`] for what each narrows the listing to.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct ListQuery {
    /// An account id.
    pub actor: Option<String>,
    /// An action such as `settings.set` or `bot.stop`.
    pub action: Option<String>,
    /// `setting`, `application` or `rule`.
    pub target_kind: Option<String>,
    /// With `target_kind`: the settings key, or the application's or rule's id.
    pub target_id: Option<String>,
    /// RFC 3339 timestamps.
    pub since: Option<String>,
    pub until: Option<String>,
    pub limit: Option<usize>,
    /// The `next` of the previous page.
    pub before: Option<String>,
}

/// A query parameter parsed as `T`, or 400 naming the parameter.
fn parse<T>(name: &str, value: Option<String>) -> Result<Option<T>, ApiError>
where
    T: std::str::FromStr,
    T::Err: std::fmt::Display,
{
    value
        .map(|text| {
            text.parse()
                .map_err(|e| ApiError::BadRequest(format!("{name}: {e}")))
        })
        .transpose()
}

pub async fn list(
    State(state): State<AppState>,
    Auth(identity): Auth,
    Query(query): Query<ListQuery>,
) -> Result<Json<Page>, ApiError> {
    identity.require(Permission::ViewAuditLog)?;
    let filter = Filter {
        actor: parse("actor", query.actor)?,
        action: parse("action", query.action)?,
        target_kind: parse("target_kind", query.target_kind)?,
        target_id: query.target_id,
        since: parse("since", query.since)?,
        until: parse("until", query.until)?,
        limit: query.limit,
        before: parse("before", query.before)?,
    };
    if filter.target_id.is_some() && filter.target_kind.is_none() {
        return Err(ApiError::BadRequest("target_id needs target_kind".into()));
    }
    Ok(Json(state.audit.list(&filter).await?))
}
