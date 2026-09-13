//! One stream for everything the web app watches live: the engine's counts and job
//! events, and every bot's status, as server-sent events on a single connection. A
//! browser allows a host only a few connections at a time, and a stream per feed and per
//! tab would leave none for pages, downloads and video once a few tabs are open.

use std::convert::Infallible;

use axum::extract::State;
use axum::response::sse::{Event, KeepAlive, Sse};
use futures::Stream;

use super::AppState;
use super::auth::Auth;
use super::error::ApiError;

/// The `stats` and `job` events of the job feed and the `bot` events of the bot feed,
/// merged as they happen.
pub async fn feed(
    State(state): State<AppState>,
    Auth(_): Auth,
) -> Result<Sse<impl Stream<Item = Result<Event, Infallible>>>, ApiError> {
    let jobs = super::jobs::job_events(state.clone()).await?;
    let bots = super::applications::bot_stream(state).await;
    let merged = tokio_stream::StreamExt::merge(jobs, bots);
    Ok(Sse::new(merged).keep_alive(KeepAlive::default()))
}

#[cfg(test)]
mod tests {
    use axum::http::StatusCode;
    use discoclip_engine::job::{Origin, Request, SourceId};
    use url::Url;
    use uuid::Uuid;

    use crate::applications::ApplicationId;
    use crate::web::testing::{Client, SUPPORTED_HOST, app_with_admin_db};

    #[tokio::test]
    async fn the_feed_carries_stats_jobs_and_bots_on_one_connection() {
        let (app, _db) = app_with_admin_db().await;
        let mut admin = Client::new(&app);
        admin.login("nick", "correct horse").await;
        let feed = tokio::spawn({
            let mut client = Client::new(&app);
            client.cookie = admin.cookie.clone();
            async move { client.stream("/api/events").await }
        });
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        let origin = Origin {
            source: SourceId::new("discord"),
            reference: "x".into(),
            url: None,
        };
        let id = app
            .state
            .engine
            .submit(Request::new(
                origin,
                Url::parse(&format!("https://{SUPPORTED_HOST}/live")).unwrap(),
            ))
            .await
            .unwrap();
        let gone = ApplicationId(Uuid::now_v7());
        app.state.bots.retire(gone).await;
        let text = feed.await.unwrap();
        assert!(text.contains("event: stats"), "{text}");
        assert!(text.contains("event: job"), "{text}");
        assert!(text.contains(&format!("\"job\":\"{id}\"")), "{text}");
        assert!(text.contains("event: bot"), "{text}");
        assert!(
            text.contains(&format!("\"application\":\"{gone}\"")),
            "{text}"
        );
        assert!(text.contains("\"removed\":true"), "{text}");

        let mut anonymous = Client::new(&app);
        let (status, _) = anonymous.get("/api/events").await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
    }
}
