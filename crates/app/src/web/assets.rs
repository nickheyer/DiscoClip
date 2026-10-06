//! The embedded web app: every file the SvelteKit build produced, served from memory. The
//! app's page answers any path the API does not, and the browser's router takes it from
//! there.

use std::collections::HashMap;
use std::io::Write;
use std::sync::LazyLock;

use axum::body::Body;
use axum::extract::Request;
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, header};
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
    /// The bytes gzipped at build time, for text that came out smaller that way.
    pub gzip: Option<&'static [u8]>,
    /// A strong validator over the gzipped bytes, already quoted.
    pub etag_gzip: &'static str,
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

/// The page loads scripts and styles from this host only, images also from any HTTPS host
/// for guild icons and video thumbnails, and talks to nothing but this host.
static CONTENT_SECURITY_POLICY: LazyLock<HeaderValue> = LazyLock::new(|| {
    let inline: String = INLINE_SCRIPT_HASHES
        .iter()
        .map(|hash| format!(" '{hash}'"))
        .collect();
    HeaderValue::from_str(&format!(
        "default-src 'self'; script-src 'self'{inline}; style-src 'self' 'unsafe-inline'; \
         img-src 'self' data: https:; media-src 'self'; font-src 'self'; connect-src 'self'; \
         frame-ancestors 'none'; base-uri 'self'; form-action 'self'; object-src 'none'"
    ))
    .expect("a CSP is a valid header value")
});

/// The app page with head markup inserted, served fresh and gzipped for a browser that takes it
pub fn page_with_head(head: &str, headers: &HeaderMap) -> Response {
    let shell = String::from_utf8_lossy(PAGE.bytes);
    let html = match shell.find("<head>") {
        Some(at) => {
            let split = at + "<head>".len();
            format!("{}{head}{}", &shell[..split], &shell[split..])
        }
        None => format!("{head}{shell}"),
    };
    let mut response = Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, PAGE.content_type)
        .header(header::CACHE_CONTROL, REVALIDATE)
        .header(header::VARY, ACCEPT_ENCODING)
        .header(header::X_CONTENT_TYPE_OPTIONS, "nosniff")
        .header(
            header::CONTENT_SECURITY_POLICY,
            CONTENT_SECURITY_POLICY.clone(),
        )
        .header(header::REFERRER_POLICY, "same-origin")
        .header(header::X_FRAME_OPTIONS, "DENY");
    let body = if accepts_gzip(headers) {
        response = response.header(header::CONTENT_ENCODING, GZIP);
        gzip(html.as_bytes())
    } else {
        html.into_bytes()
    };
    response
        .header(header::CONTENT_LENGTH, body.len())
        .body(Body::from(body))
        .expect("a response with valid headers")
}

const IMMUTABLE: &str = "public, max-age=31536000, immutable";
const REVALIDATE: &str = "no-cache";
const ACCEPT_ENCODING: &str = "Accept-Encoding";
const GZIP: &str = "gzip";

/// Whether the request takes a gzip body, by a gzip coding it lists with a weight above zero
fn accepts_gzip(headers: &HeaderMap) -> bool {
    headers
        .get_all(header::ACCEPT_ENCODING)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(','))
        .any(|coding| {
            let mut parts = coding.split(';').map(str::trim);
            let name = parts.next().unwrap_or("");
            let wanted = parts
                .find_map(|part| part.strip_prefix("q="))
                .and_then(|weight| weight.parse::<f32>().ok())
                .is_none_or(|weight| weight > 0.0);
            name.eq_ignore_ascii_case(GZIP) && wanted
        })
}

/// The bytes gzipped at the library's default level
fn gzip(bytes: &[u8]) -> Vec<u8> {
    let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    encoder
        .write_all(bytes)
        .expect("writing into memory does not fail");
    encoder
        .finish()
        .expect("finishing an in-memory gzip stream does not fail")
}

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
    let cache = if asset.immutable {
        IMMUTABLE
    } else {
        REVALIDATE
    };
    let gzipped = asset.gzip.filter(|_| accepts_gzip(request.headers()));
    let (bytes, etag) = match gzipped {
        Some(bytes) => (bytes, asset.etag_gzip),
        None => (asset.bytes, asset.etag),
    };

    let mut response = Response::builder()
        .header(header::ETAG, etag)
        .header(header::CACHE_CONTROL, cache)
        .header(header::X_CONTENT_TYPE_OPTIONS, "nosniff");
    if asset.gzip.is_some() {
        response = response.header(header::VARY, ACCEPT_ENCODING);
    }
    if gzipped.is_some() {
        response = response.header(header::CONTENT_ENCODING, GZIP);
    }
    if asset.content_type.starts_with("text/html") {
        response = response
            .header(
                header::CONTENT_SECURITY_POLICY,
                CONTENT_SECURITY_POLICY.clone(),
            )
            .header(header::REFERRER_POLICY, "same-origin")
            .header(header::X_FRAME_OPTIONS, "DENY");
    }

    let fresh = request
        .headers()
        .get(header::IF_NONE_MATCH)
        .is_some_and(|value| matches(value, etag));
    if fresh {
        return response
            .status(StatusCode::NOT_MODIFIED)
            .body(Body::empty())
            .expect("a response with valid headers");
    }
    let body = if *request.method() == Method::HEAD {
        Body::empty()
    } else {
        Body::from(bytes)
    };
    response
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, asset.content_type)
        .header(header::CONTENT_LENGTH, bytes.len())
        .body(body)
        .expect("a response with valid headers")
}

#[cfg(test)]
mod tests {
    use std::io::Read;

    use axum::body::to_bytes;
    use axum::http::{Request, StatusCode};

    use super::*;

    fn request(path: &str, encodings: Option<&str>) -> Request<Body> {
        let mut request = Request::builder().uri(path);
        if let Some(encodings) = encodings {
            request = request.header(header::ACCEPT_ENCODING, encodings);
        }
        request.body(Body::empty()).unwrap()
    }

    #[test]
    fn gzip_is_taken_when_listed_with_weight() {
        let mut headers = HeaderMap::new();
        assert!(!accepts_gzip(&headers));
        headers.insert(
            header::ACCEPT_ENCODING,
            HeaderValue::from_static("br, gzip;q=0.5"),
        );
        assert!(accepts_gzip(&headers));
        headers.insert(header::ACCEPT_ENCODING, HeaderValue::from_static("GZIP"));
        assert!(accepts_gzip(&headers));
        headers.insert(
            header::ACCEPT_ENCODING,
            HeaderValue::from_static("gzip;q=0, br"),
        );
        assert!(!accepts_gzip(&headers));
        headers.insert(
            header::ACCEPT_ENCODING,
            HeaderValue::from_static("identity"),
        );
        assert!(!accepts_gzip(&headers));
    }

    #[tokio::test]
    async fn the_page_is_gzipped_for_a_browser_that_takes_it_and_plain_otherwise() {
        let plain = serve(request("/", None)).await;
        assert_eq!(plain.status(), StatusCode::OK);
        assert!(plain.headers().get(header::CONTENT_ENCODING).is_none());
        assert_eq!(plain.headers()["vary"], "Accept-Encoding");
        let plain_etag = plain.headers()["etag"].clone();
        let plain_body = to_bytes(plain.into_body(), usize::MAX).await.unwrap();

        let zipped = serve(request("/", Some("gzip, deflate"))).await;
        assert_eq!(zipped.status(), StatusCode::OK);
        assert_eq!(zipped.headers()["content-encoding"], "gzip");
        assert_eq!(zipped.headers()["vary"], "Accept-Encoding");
        let zipped_etag = zipped.headers()["etag"].clone();
        assert_ne!(zipped_etag, plain_etag);
        assert!(zipped_etag.to_str().unwrap().ends_with("-gz\""));
        let zipped_body = to_bytes(zipped.into_body(), usize::MAX).await.unwrap();
        assert!(zipped_body.len() < plain_body.len());
        let mut unzipped = Vec::new();
        flate2::read::GzDecoder::new(zipped_body.as_ref())
            .read_to_end(&mut unzipped)
            .unwrap();
        assert_eq!(unzipped, plain_body);

        // Each form is asked about by its own tag.
        let mut again = request("/", Some("gzip"));
        again
            .headers_mut()
            .insert(header::IF_NONE_MATCH, zipped_etag.clone());
        assert_eq!(serve(again).await.status(), StatusCode::NOT_MODIFIED);
        let mut other = request("/", Some("gzip"));
        other
            .headers_mut()
            .insert(header::IF_NONE_MATCH, plain_etag);
        assert_eq!(serve(other).await.status(), StatusCode::OK);

        // A font is compressed already and goes out as it is.
        let font = ASSETS
            .iter()
            .find(|asset| asset.path.ends_with(".woff2"))
            .expect("the app ships a font");
        assert!(font.gzip.is_none());
        let served = serve(request(&format!("/{}", font.path), Some("gzip"))).await;
        assert!(served.headers().get(header::CONTENT_ENCODING).is_none());
        assert!(served.headers().get(header::VARY).is_none());
    }

    #[tokio::test]
    async fn a_page_with_a_head_is_gzipped_on_request() {
        let plain = page_with_head("<title>x</title>", &HeaderMap::new());
        assert!(plain.headers().get(header::CONTENT_ENCODING).is_none());
        let plain_body = to_bytes(plain.into_body(), usize::MAX).await.unwrap();
        assert!(plain_body.starts_with(b"<!doctype html>"));
        let mut headers = HeaderMap::new();
        headers.insert(header::ACCEPT_ENCODING, HeaderValue::from_static("gzip"));
        let zipped = page_with_head("<title>x</title>", &headers);
        assert_eq!(zipped.headers()["content-encoding"], "gzip");
        let length: usize = zipped.headers()["content-length"]
            .to_str()
            .unwrap()
            .parse()
            .unwrap();
        let zipped_body = to_bytes(zipped.into_body(), usize::MAX).await.unwrap();
        assert_eq!(zipped_body.len(), length);
        let mut unzipped = Vec::new();
        flate2::read::GzDecoder::new(zipped_body.as_ref())
            .read_to_end(&mut unzipped)
            .unwrap();
        assert_eq!(unzipped, plain_body);
    }
}
