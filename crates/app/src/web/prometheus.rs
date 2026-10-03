//! The server's numbers in the Prometheus text exposition, at `/metrics` for a scraper.
//! A browser asking the same path for HTML gets the web app's Metrics page instead.

use axum::extract::{Request, State};
use axum::http::{HeaderMap, HeaderValue, header};
use axum::response::{IntoResponse, Response};
use jiff::Timestamp;

use super::AppState;
use super::auth::MaybeAuth;
use super::error::ApiError;
use super::health::{Health, Status, read_health};
use super::metrics::{Metrics, read_metrics};

pub const CONTENT_TYPE: &str = "text/plain; version=0.0.4; charset=utf-8";

/// Whether the request is a browser after a page rather than a scraper after numbers.
pub fn wants_html(headers: &HeaderMap) -> bool {
    headers
        .get(header::ACCEPT)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|accept| accept.contains("text/html"))
}

pub async fn get(
    State(state): State<AppState>,
    MaybeAuth(identity): MaybeAuth,
    headers: HeaderMap,
    request: Request,
) -> Response {
    if wants_html(&headers) {
        return super::assets::serve(request).await;
    }
    if identity.is_none() {
        return ApiError::Unauthorized.into_response();
    }
    let metrics = match read_metrics(&state).await {
        Ok(metrics) => metrics,
        Err(error) => return error.into_response(),
    };
    let health = read_health(&state).await;
    let text = render(&metrics, &health);
    (
        [(header::CONTENT_TYPE, HeaderValue::from_static(CONTENT_TYPE))],
        text,
    )
        .into_response()
}

/// Writes samples in the text exposition, one family at a time.
struct Exposition {
    out: String,
}

impl Exposition {
    fn family(&mut self, name: &str, kind: &str, help: &str) {
        self.out
            .push_str(&format!("# HELP {name} {help}\n# TYPE {name} {kind}\n"));
    }

    fn sample(&mut self, name: &str, labels: &[(&str, &str)], value: f64) {
        self.out.push_str(name);
        if !labels.is_empty() {
            self.out.push('{');
            for (index, (key, value)) in labels.iter().enumerate() {
                if index > 0 {
                    self.out.push(',');
                }
                self.out.push_str(&format!("{key}=\"{}\"", escape(value)));
            }
            self.out.push('}');
        }
        self.out.push_str(&format!(" {}\n", number(value)));
    }

    /// One family with one unlabelled sample.
    fn single(&mut self, name: &str, kind: &str, help: &str, value: f64) {
        self.family(name, kind, help);
        self.sample(name, &[], value);
    }
}

fn escape(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
}

fn number(value: f64) -> String {
    if value.is_nan() {
        "NaN".into()
    } else if value.is_infinite() {
        if value > 0.0 { "+Inf" } else { "-Inf" }.into()
    } else if value.fract() == 0.0 && value.abs() < 1e15 {
        format!("{}", value as i64)
    } else {
        format!("{value}")
    }
}

fn seconds(at: Timestamp) -> f64 {
    at.as_second() as f64
}

fn status_value(status: Status) -> f64 {
    match status {
        Status::Ok => 0.0,
        Status::Warn => 1.0,
        Status::Fail => 2.0,
    }
}

/// Every number the metrics and health pages show, as Prometheus reads them.
pub fn render(metrics: &Metrics, health: &Health) -> String {
    let mut e = Exposition {
        out: String::with_capacity(8 * 1024),
    };
    e.family("discoclip_info", "gauge", "The server's version.");
    e.sample("discoclip_info", &[("version", metrics.version)], 1.0);
    e.single(
        "discoclip_uptime_seconds",
        "gauge",
        "Seconds since the server started.",
        metrics.uptime_secs as f64,
    );
    e.single(
        "discoclip_start_time_seconds",
        "gauge",
        "When the server started, as seconds since the Unix epoch.",
        seconds(metrics.started_at),
    );

    if let Some(process) = &metrics.process {
        e.single(
            "discoclip_process_resident_memory_bytes",
            "gauge",
            "Resident memory of the server process.",
            process.rss_bytes as f64,
        );
        e.single(
            "discoclip_process_virtual_memory_bytes",
            "gauge",
            "Virtual memory of the server process.",
            process.virtual_bytes as f64,
        );
        e.single(
            "discoclip_process_cpu_percent",
            "gauge",
            "CPU use of the server process since the previous reading, of one core.",
            f64::from(process.cpu_percent),
        );
    }
    e.single(
        "discoclip_system_memory_total_bytes",
        "gauge",
        "Memory of the machine.",
        metrics.system.total_memory_bytes as f64,
    );
    e.single(
        "discoclip_system_memory_available_bytes",
        "gauge",
        "Memory of the machine not in use.",
        metrics.system.available_memory_bytes as f64,
    );
    e.single(
        "discoclip_system_cpus",
        "gauge",
        "CPUs of the machine.",
        metrics.system.cpus as f64,
    );
    e.family(
        "discoclip_system_load_average",
        "gauge",
        "Load average of the machine.",
    );
    for (period, value) in ["1m", "5m", "15m"].iter().zip(metrics.system.load_average) {
        e.sample(
            "discoclip_system_load_average",
            &[("period", period)],
            value,
        );
    }
    e.family(
        "discoclip_disk_total_bytes",
        "gauge",
        "Size of each disk that holds a server directory.",
    );
    for disk in &metrics.system.disks {
        e.sample(
            "discoclip_disk_total_bytes",
            &[("mount", &disk.mount)],
            disk.total_bytes as f64,
        );
    }
    e.family(
        "discoclip_disk_available_bytes",
        "gauge",
        "Free space on each disk that holds a server directory.",
    );
    for disk in &metrics.system.disks {
        e.sample(
            "discoclip_disk_available_bytes",
            &[("mount", &disk.mount)],
            disk.available_bytes as f64,
        );
    }

    e.family("discoclip_jobs", "gauge", "Jobs by status.");
    let counts = &metrics.jobs.counts;
    for (status, count) in [
        ("queued", counts.queued),
        ("running", counts.running),
        ("done", counts.done),
        ("failed", counts.failed),
        ("cancelled", counts.cancelled),
    ] {
        e.sample("discoclip_jobs", &[("status", status)], count as f64);
    }
    e.family(
        "discoclip_jobs_last_24h",
        "gauge",
        "Jobs created in the last 24 hours, by status.",
    );
    let recent = &metrics.jobs.last_24h;
    for (status, count) in [
        ("queued", recent.queued),
        ("running", recent.running),
        ("done", recent.done),
        ("failed", recent.failed),
        ("cancelled", recent.cancelled),
    ] {
        e.sample(
            "discoclip_jobs_last_24h",
            &[("status", status)],
            count as f64,
        );
    }
    e.single(
        "discoclip_engine_workers",
        "gauge",
        "Jobs the engine runs at once.",
        metrics.jobs.utilisation.workers as f64,
    );
    e.single(
        "discoclip_engine_active_jobs",
        "gauge",
        "Jobs on a worker now.",
        metrics.jobs.utilisation.active as f64,
    );
    e.single(
        "discoclip_engine_waiting_jobs",
        "gauge",
        "Jobs dispatched and waiting for a worker.",
        metrics.jobs.utilisation.waiting as f64,
    );
    e.family(
        "discoclip_resolver_jobs_total",
        "counter",
        "Jobs finished per resolver, by outcome.",
    );
    for resolver in &metrics.jobs.resolvers {
        e.sample(
            "discoclip_resolver_jobs_total",
            &[("resolver", &resolver.resolver), ("outcome", "done")],
            resolver.done as f64,
        );
        e.sample(
            "discoclip_resolver_jobs_total",
            &[("resolver", &resolver.resolver), ("outcome", "failed")],
            resolver.failed as f64,
        );
    }

    e.family(
        "discoclip_http_requests_total",
        "counter",
        "Outgoing HTTP requests by host and status.",
    );
    for request in &metrics.http.requests {
        e.sample(
            "discoclip_http_requests_total",
            &[
                ("host", &request.host),
                ("status", &request.status.to_string()),
            ],
            request.count as f64,
        );
    }
    e.single(
        "discoclip_http_retries_total",
        "counter",
        "Outgoing HTTP requests retried.",
        metrics.http.retries as f64,
    );
    e.single(
        "discoclip_http_rate_limit_waits_total",
        "counter",
        "Outgoing HTTP requests held back by a rate limit.",
        metrics.http.rate_limit_waits as f64,
    );
    e.single(
        "discoclip_http_received_bytes_total",
        "counter",
        "Bytes received over outgoing HTTP.",
        metrics.http.bytes_received as f64,
    );

    e.single(
        "discoclip_bot_applications",
        "gauge",
        "Discord applications with a bot.",
        metrics.bots.applications as f64,
    );
    e.family("discoclip_bots", "gauge", "Bots by state.");
    for (state, count) in &metrics.bots.by_state {
        e.sample("discoclip_bots", &[("state", state)], *count as f64);
    }

    e.single(
        "discoclip_cache_bytes",
        "gauge",
        "Bytes of job files in the cache.",
        metrics.cache.bytes as f64,
    );
    e.single(
        "discoclip_cache_jobs",
        "gauge",
        "Job directories in the cache.",
        metrics.cache.jobs as f64,
    );
    e.single(
        "discoclip_database_bytes",
        "gauge",
        "Size of the database on disk.",
        metrics.database.bytes as f64,
    );
    e.family(
        "discoclip_fixture_platforms",
        "gauge",
        "Platforms by what their checks and finished jobs show of them.",
    );
    for (state, count) in [
        ("working", metrics.fixtures.counts.working),
        ("failing", metrics.fixtures.counts.failing),
        ("login_required", metrics.fixtures.counts.login_required),
        ("unknown", metrics.fixtures.counts.unknown),
        ("running", metrics.fixtures.counts.running),
    ] {
        e.sample(
            "discoclip_fixture_platforms",
            &[("state", state)],
            count as f64,
        );
    }
    e.single(
        "discoclip_log_lines_buffered",
        "gauge",
        "Log lines kept for the Logs page.",
        metrics.logs.buffered as f64,
    );
    e.single(
        "discoclip_log_lines_capacity",
        "gauge",
        "Log lines the buffer holds at most.",
        metrics.logs.capacity as f64,
    );

    e.family(
        "discoclip_encoder_info",
        "gauge",
        "The video encoder in use, the ffmpeg build it comes from, and the choice made.",
    );
    e.sample(
        "discoclip_encoder_info",
        &[
            ("choice", metrics.transcode.choice),
            ("hardware", metrics.transcode.hardware.unwrap_or("none")),
            (
                "encoder",
                metrics.transcode.h264_encoder.as_deref().unwrap_or("none"),
            ),
            ("source", metrics.transcode.source),
        ],
        1.0,
    );
    e.single(
        "discoclip_encoder_shortfall",
        "gauge",
        "1 when the chosen encoder family is not available and software encodes instead.",
        if metrics.transcode.shortfall.is_some() {
            1.0
        } else {
            0.0
        },
    );

    e.single(
        "discoclip_retention_sweeps_total",
        "counter",
        "Retention sweeps run.",
        metrics.retention.sweeps as f64,
    );
    e.single(
        "discoclip_retention_jobs_removed_total",
        "counter",
        "Jobs removed by retention.",
        metrics.retention.jobs_removed_total as f64,
    );
    e.single(
        "discoclip_retention_bytes_freed_total",
        "counter",
        "Cache bytes freed by retention.",
        metrics.retention.bytes_freed_total as f64,
    );
    if let Some(last) = &metrics.retention.last {
        e.single(
            "discoclip_retention_last_sweep_timestamp_seconds",
            "gauge",
            "When the last retention sweep ran.",
            seconds(last.at),
        );
        e.single(
            "discoclip_retention_last_sweep_ok",
            "gauge",
            "1 when the last retention sweep went through without problems.",
            if last.error.is_none() { 1.0 } else { 0.0 },
        );
    }

    e.single(
        "discoclip_backups_enabled",
        "gauge",
        "1 when database backups are turned on.",
        if metrics.backups.enabled { 1.0 } else { 0.0 },
    );
    e.single(
        "discoclip_backups",
        "gauge",
        "Backups kept on disk.",
        metrics.backups.count as f64,
    );
    e.single(
        "discoclip_backups_bytes",
        "gauge",
        "Bytes of every backup kept.",
        metrics.backups.bytes as f64,
    );
    e.single(
        "discoclip_backup_runs_total",
        "counter",
        "Backups attempted since the server started.",
        metrics.backups.runs as f64,
    );
    if let Some(newest) = metrics.backups.newest_at {
        e.single(
            "discoclip_backup_newest_timestamp_seconds",
            "gauge",
            "When the newest backup was made.",
            seconds(newest),
        );
    }
    e.single(
        "discoclip_backup_last_ok",
        "gauge",
        "1 when the last backup attempt succeeded, or none has run.",
        if metrics.backups.last_error.is_none() {
            1.0
        } else {
            0.0
        },
    );

    e.family(
        "discoclip_health_check",
        "gauge",
        "Each health check: 0 ok, 1 warn, 2 fail.",
    );
    for check in &health.checks {
        e.sample(
            "discoclip_health_check",
            &[("check", check.name)],
            status_value(check.status),
        );
    }
    e.single(
        "discoclip_health",
        "gauge",
        "The server's health: 0 ok, 1 warn, 2 fail.",
        status_value(health.status),
    );
    e.out
}

#[cfg(test)]
mod tests {
    use axum::http::StatusCode;

    use super::{escape, number};
    use crate::web::testing::{Client, app_with_admin};

    #[test]
    fn values_and_labels_are_written_as_prometheus_reads_them() {
        assert_eq!(number(3.0), "3");
        assert_eq!(number(2.5), "2.5");
        assert_eq!(number(f64::INFINITY), "+Inf");
        assert_eq!(number(f64::NAN), "NaN");
        assert_eq!(escape("a\"b\\c\nd"), "a\\\"b\\\\c\\nd");
    }

    #[tokio::test]
    async fn the_exposition_is_served_to_scrapers_and_the_page_to_browsers() {
        let app = app_with_admin().await;
        let mut client = Client::new(&app);
        client.headers = vec![("accept".into(), "text/plain;version=0.0.4".into())];
        let (status, _, _) = client.raw("/metrics").await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        client.login("nick", "correct horse").await;
        let (status, headers, body) = client.raw("/metrics").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(headers["content-type"], super::CONTENT_TYPE);
        let text = String::from_utf8(body).unwrap();
        assert!(text.contains("# TYPE discoclip_jobs gauge\n"), "{text}");
        assert!(
            text.contains("discoclip_jobs{status=\"queued\"} 0\n"),
            "{text}"
        );
        assert!(text.contains("discoclip_engine_workers 2\n"), "{text}");
        assert!(text.contains("discoclip_health 0\n"), "{text}");
        assert!(
            text.contains("discoclip_health_check{check=\"database\"} 0\n"),
            "{text}"
        );
        assert!(text.contains("discoclip_encoder_info{choice=\"software\",hardware=\"none\",encoder=\"libx264\",source=\"embedded\"} 1\n"), "{text}");
        assert!(text.contains("discoclip_backups_enabled 1\n"), "{text}");
        assert!(text.contains("discoclip_info{version=\""), "{text}");
        client.headers = vec![("accept".into(), "text/html,application/xhtml+xml".into())];
        let (status, headers, body) = client.raw("/metrics").await;
        assert_eq!(status, StatusCode::OK);
        assert!(
            headers["content-type"]
                .to_str()
                .unwrap()
                .starts_with("text/html")
        );
        assert!(String::from_utf8_lossy(&body).contains("<html"));
        // A bearer token scrapes too, which is how Prometheus is given access.
        let (_, minted) = client
            .post(
                "/api/tokens",
                serde_json::json!({"name": "prometheus", "scopes": []}),
            )
            .await;
        let mut scraper = Client::new(&app);
        scraper.bearer = Some(minted["secret"].as_str().unwrap().to_string());
        scraper.headers = vec![("accept".into(), "*/*".into())];
        let (status, _, body) = scraper.raw("/metrics").await;
        assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&body));
    }
}
