//! The embedded web app: every file the SvelteKit build produced, served from memory. The
//! app's page answers any path the API does not, and the browser's router takes it from
//! there.

use std::collections::HashMap;
use std::sync::LazyLock;

use axum::body::Body;
use axum::extract::Request;
use axum::http::{HeaderValue, Method, StatusCode, header};
use axum::response::{IntoResponse, Response};

pub struct Asset {
    /// Relative to the app's root, without a leading slash.
    pub path: &'static str,
    pub content_type: &'static str,
    /// A strong validator over the bytes, already quoted.
    pub etag: &'static str,
    /// Whether the file's name carries its content hash, so it never changes.
    pub immutable: bool,
    pub bytes: &'static [u8],
}

include!(concat!(env!("OUT_DIR"), "/ui_embed.rs"));

static BY_PATH: LazyLock<HashMap<&'static str, &'static Asset>> =
    LazyLock::new(|| ASSETS.iter().map(|asset| (asset.path, asset)).collect());

static PAGE: LazyLock<&'static Asset> = LazyLock::new(|| {
    BY_PATH
        .get("index.html")
        .copied()
        .expect("the embedded web app has an index.html")
});

/// The page loads scripts and styles from this host only, images also from Discord's CDN
/// for guild icons, and talks to nothing but this host.
static CONTENT_SECURITY_POLICY: LazyLock<HeaderValue> = LazyLock::new(|| {
    let inline: String = INLINE_SCRIPT_HASHES
        .iter()
        .map(|hash| format!(" '{hash}'"))
        .collect();
    HeaderValue::from_str(&format!(
        "default-src 'self'; script-src 'self'{inline}; style-src 'self' 'unsafe-inline'; \
         img-src 'self' data: https://cdn.discordapp.com; font-src 'self'; connect-src 'self'; \
         frame-ancestors 'none'; base-uri 'self'; form-action 'self'; object-src 'none'"
    ))
    .expect("a CSP is a valid header value")
});

const IMMUTABLE: &str = "public, max-age=31536000, immutable";
const REVALIDATE: &str = "no-cache";

/// Whether an `If-None-Match` header names `etag`.
fn matches(header: &HeaderValue, etag: &str) -> bool {
    header.to_str().is_ok_and(|value| {
        value
            .split(',')
            .map(|tag| tag.trim().trim_start_matches("W/"))
            .any(|tag| tag == "*" || tag == etag)
    })
}

/// Serves the file at the request's path, or the app's page for a path that is no file.
pub async fn serve(request: Request) -> Response {
    if !matches!(*request.method(), Method::GET | Method::HEAD) {
        return (
            StatusCode::METHOD_NOT_ALLOWED,
            [(header::ALLOW, HeaderValue::from_static("GET, HEAD"))],
        )
            .into_response();
    }
    let path = request.uri().path().trim_start_matches('/');
    let asset: &Asset = BY_PATH.get(path).copied().unwrap_or(*PAGE);
    let cache = if asset.immutable { IMMUTABLE } else { REVALIDATE };

    let mut response = Response::builder()
        .header(header::ETAG, asset.etag)
        .header(header::CACHE_CONTROL, cache)
        .header(header::X_CONTENT_TYPE_OPTIONS, "nosniff");
    if asset.content_type.starts_with("text/html") {
        response = response
            .header(header::CONTENT_SECURITY_POLICY, CONTENT_SECURITY_POLICY.clone())
            .header(header::REFERRER_POLICY, "same-origin")
            .header(header::X_FRAME_OPTIONS, "DENY");
    }

    let fresh = request
        .headers()
        .get(header::IF_NONE_MATCH)
        .is_some_and(|value| matches(value, asset.etag));
    if fresh {
        return response
            .status(StatusCode::NOT_MODIFIED)
            .body(Body::empty())
            .expect("a response with valid headers");
    }
    let body = if *request.method() == Method::HEAD {
        Body::empty()
    } else {
        Body::from(asset.bytes)
    };
    response
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, asset.content_type)
        .header(header::CONTENT_LENGTH, asset.bytes.len())
        .body(body)
        .expect("a response with valid headers")
}
