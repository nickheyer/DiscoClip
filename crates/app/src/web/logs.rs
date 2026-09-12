//! The server's log as the app shows it: the last lines kept, filtered and paged, and
//! new ones as they are written.

use std::convert::Infallible;

use axum::Json;
use axum::extract::{Query, State};
use axum::response::sse::{Event, KeepAlive, Sse};
use futures::{Stream, StreamExt};
use serde::{Deserialize, Serialize};
use tokio_stream::wrappers::BroadcastStream;
use tokio_stream::wrappers::errors::BroadcastStreamRecvError;

use super::AppState;
use super::auth::Auth;
use super::error::ApiError;
use crate::telemetry::{LOG_CAPACITY, LogFilter, LogLine};
use crate::users::Permission;

const DEFAULT_LIMIT: usize = 200;
const MAX_LIMIT: usize = 1000;

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct LogQuery {
    /// The lowest level shown.
    pub level: Option<String>,
    /// Part of the target, such as a crate or module name.
    pub target: Option<String>,
    /// Part of the message or a field.
    pub q: Option<String>,
    /// Lines before this id: the `next` of the previous page.
    pub before: Option<u64>,
    pub limit: Option<usize>,
}

impl LogQuery {
    fn filter(self) -> Result<LogFilter, ApiError> {
        let level = self
            .level
            .filter(|l| !l.trim().is_empty())
            .map(|l| l.parse().map_err(ApiError::BadRequest))
            .transpose()?;
        let limit = self.limit.unwrap_or(DEFAULT_LIMIT);
        if limit == 0 || limit > MAX_LIMIT {
            return Err(ApiError::BadRequest(format!("limit is 1 to {MAX_LIMIT}")));
        }
        Ok(LogFilter {
            level,
            target: self.target.filter(|t| !t.trim().is_empty()),
            q: self.q.filter(|q| !q.trim().is_empty()),
            before: self.before,
            limit,
        })
    }
}

/// One page of lines, newest first.
#[derive(Debug, Serialize)]
pub struct LogPage {
    pub lines: Vec<LogLine>,
    /// Pass as `before` for the page of older lines; nothing when this was the last.
    pub next: Option<u64>,
    /// How many lines the server keeps right now, and at most.
    pub buffered: usize,
    pub capacity: usize,
    pub oldest_id: Option<u64>,
}

pub async fn list(
    State(state): State<AppState>,
    Auth(identity): Auth,
    Query(query): Query<LogQuery>,
) -> Result<Json<LogPage>, ApiError> {
    identity.require(Permission::ViewLogs)?;
    let filter = query.filter()?;
    let buffer = &state.live.log.buffer;
    let lines: Vec<LogLine> = buffer
        .query(&filter)
        .into_iter()
        .map(|line| (*line).clone())
        .collect();
    let oldest_id = buffer.oldest_id();
    let next = lines
        .last()
        .filter(|last| lines.len() >= filter.limit && oldest_id.is_some_and(|o| o < last.id))
        .map(|last| last.id);
    Ok(Json(LogPage {
        lines,
        next,
        buffered: buffer.len(),
        capacity: LOG_CAPACITY,
        oldest_id,
    }))
}

/// How many lines a follower missed while it was not reading.
#[derive(Debug, Serialize)]
struct Skipped {
    count: u64,
}

/// New lines as they are written, filtered the same way as a listing.
pub async fn events(
    State(state): State<AppState>,
    Auth(identity): Auth,
    Query(query): Query<LogQuery>,
) -> Result<Sse<impl Stream<Item = Result<Event, Infallible>>>, ApiError> {
    identity.require(Permission::ViewLogs)?;
    let filter = query.filter()?;
    let feed = BroadcastStream::new(state.live.log.buffer.subscribe());
    let stream = feed.filter_map(move |item| {
        let event = match item {
            Ok(line) if filter.matches(&line) => Some(
                Event::default()
                    .event("log")
                    .json_data(&*line)
                    .expect("log lines serialize"),
            ),
            Ok(_) => None,
            Err(BroadcastStreamRecvError::Lagged(count)) => Some(
                Event::default()
                    .event("skipped")
                    .json_data(Skipped { count })
                    .expect("counts serialize"),
            ),
        };
        async move { event.map(Ok) }
    });
    Ok(Sse::new(stream).keep_alive(KeepAlive::default()))
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use axum::http::StatusCode;

    use crate::telemetry::LogLevel;
    use crate::users::Role;
    use crate::web::testing::{Client, app_with_admin};

    #[tokio::test]
    async fn the_log_is_read_by_admins_filtered_and_paged() {
        let app = app_with_admin().await;
        app.state
            .users
            .create("op", Some("battery staple"), Role::Operator)
            .await
            .unwrap();
        let buffer = app.state.live.log.buffer.clone();
        let mut ids = Vec::new();
        for i in 0..5 {
            ids.push(
                buffer
                    .record(
                        if i % 2 == 0 {
                            LogLevel::Info
                        } else {
                            LogLevel::Warn
                        },
                        "discoclip::web",
                        format!("line {i}"),
                        BTreeMap::from([("n".to_string(), i.to_string())]),
                    )
                    .id,
            );
        }
        buffer.record(
            LogLevel::Error,
            "discoclip_engine::http",
            "request to https://a.test failed".into(),
            BTreeMap::new(),
        );

        let mut op = Client::new(&app);
        op.login("op", "battery staple").await;
        assert_eq!(op.get("/api/logs").await.0, StatusCode::FORBIDDEN);
        assert_eq!(op.get("/api/logs/events").await.0, StatusCode::FORBIDDEN);

        let mut admin = Client::new(&app);
        admin.login("nick", "correct horse").await;
        let (status, body) = admin.get("/api/logs").await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let lines = body["lines"].as_array().unwrap();
        assert_eq!(lines.len(), 6);
        assert_eq!(lines[0]["message"], "request to https://a.test failed");
        assert_eq!(lines[0]["level"], "error");
        assert_eq!(lines[5]["fields"]["n"], "0");
        assert!(body["next"].is_null());
        assert_eq!(body["buffered"], 6);
        assert_eq!(body["capacity"], crate::telemetry::LOG_CAPACITY);
        assert_eq!(body["oldest_id"], ids[0]);

        let (_, body) = admin.get("/api/logs?level=warn").await;
        assert_eq!(body["lines"].as_array().unwrap().len(), 3);
        let (_, body) = admin.get("/api/logs?target=engine&q=A.TEST").await;
        assert_eq!(body["lines"].as_array().unwrap().len(), 1);
        let (_, body) = admin.get("/api/logs?limit=2").await;
        assert_eq!(body["lines"].as_array().unwrap().len(), 2);
        let next = body["next"].as_u64().unwrap();
        assert_eq!(next, ids[4]);
        let (_, body) = admin.get(&format!("/api/logs?limit=2&before={next}")).await;
        let page = body["lines"].as_array().unwrap();
        assert_eq!(page[0]["message"], "line 3");
        assert_eq!(page[1]["message"], "line 2");
        let (_, body) = admin
            .get(&format!("/api/logs?limit=10&before={next}"))
            .await;
        assert_eq!(body["lines"].as_array().unwrap().len(), 4);
        assert!(body["next"].is_null());
        assert_eq!(
            admin.get("/api/logs?level=loud").await.0,
            StatusCode::BAD_REQUEST
        );
        assert_eq!(
            admin.get("/api/logs?limit=0").await.0,
            StatusCode::BAD_REQUEST
        );

        // Following: lines written after the stream opens arrive on it, filtered.
        let writer = buffer.clone();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            writer.record(LogLevel::Info, "t", "quiet".into(), BTreeMap::new());
            writer.record(LogLevel::Error, "t", "loud".into(), BTreeMap::new());
        });
        let stream = admin.stream("/api/logs/events?level=warn").await;
        assert!(stream.contains("event: log\n"), "{stream}");
        assert!(stream.contains("\"message\":\"loud\""), "{stream}");
        assert!(!stream.contains("quiet"), "{stream}");
    }
}
