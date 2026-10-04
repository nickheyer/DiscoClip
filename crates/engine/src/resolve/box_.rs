//! Box shared links (`{account}.app.box.com/s/{name}`): the shared page names its file
//! and a request token, the token endpoint trades that for a read token, and the files
//! API then names the video's HLS representation, whose playlists and segments the
//! token signs, or redirects to the signed download of any other file.

use std::sync::LazyLock;

use async_trait::async_trait;
use regex::Regex;
use serde_json::Value;
use url::Url;

use super::{
    MAX_PAGE, Platform, Resolution, ResolveError, Resolved, Resolver, SessionSupport, Tag, Variant,
    clean_title, fetch, hls, navigation_headers, status_error, util,
};
use crate::http::{BROWSER_UA, Http};
use crate::media::{Container, MediaKind};

pub const PLATFORM: &str = "box";
const FILES_API: &str = "https://api.box.com/2.0/files/";
const FILE_FIELDS: &str = "authenticated_download_url,created_at,created_by,description,extension,name,representations,size";

static RE_HOST: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^(?:[^.]+\.)?(app|ent)\.box\.com$").unwrap());
/// `/s/{shared name}` with an optional `/file/{id}`.
static RE_PATH: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^/s/([^/?#]+)(?:/file/(\d+))?").unwrap());
static RE_REQUEST_TOKEN: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#""requestToken"\s*:\s*"([0-9a-f]+)""#).unwrap());
static RE_POST_STREAM: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"Box\.postStreamData\s*=\s*").unwrap());

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Link {
    /// The site the link is on, `{account}.app.box.com` or `{account}.ent.box.com`.
    pub host: String,
    pub shared_name: String,
    pub file_id: Option<String>,
}

pub fn parse_link(url: &Url) -> Option<Link> {
    if !matches!(url.scheme(), "http" | "https") {
        return None;
    }
    let host = url.host_str()?.to_ascii_lowercase();
    if !RE_HOST.is_match(&host) {
        return None;
    }
    let caps = RE_PATH.captures(url.path())?;
    Some(Link {
        host,
        shared_name: caps[1].to_string(),
        file_id: caps.get(2).map(|m| m.as_str().to_string()),
    })
}

/// The file a shared page without a file id in its link is for.
pub fn shared_file_id(html: &str) -> Option<String> {
    let start = RE_POST_STREAM.find(html)?.end();
    let rest = &html[start..];
    let end = util::balanced_js_end(rest)?;
    let data: Value = serde_json::from_str(&rest[..end])
        .ok()
        .or_else(|| util::parse_js(&rest[..end]))?;
    let item = &data["/app-api/enduserapp/shared-item"];
    if item["itemType"].as_str() != Some("file") {
        return None;
    }
    util::text(&item["itemID"])
}

/// The HLS master of the file's representations, from the template the files API names
/// for its `hls` entry.
pub fn hls_master(file: &Value) -> Option<Url> {
    file["representations"]["entries"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|entry| entry["representation"].as_str() == Some("hls"))
        .find_map(|entry| entry["content"]["url_template"].as_str())
        .map(|template| template.replace("{+asset_path}", "master.m3u8"))
        .and_then(|manifest| util::join_url(None, &manifest))
}

pub struct BoxResolver {
    http: Http,
}

impl BoxResolver {
    pub fn new(http: Http) -> Self {
        Self { http }
    }

    /// The file itself for a share Box streams no video of, from the signed link its download redirects to
    async fn original(
        &self,
        file: &Value,
        file_id: &str,
        access_token: &str,
        shared_link: &str,
        url: &Url,
    ) -> Result<(MediaKind, Variant), ResolveError> {
        let download =
            util::url_of(&file["authenticated_download_url"], None).unwrap_or_else(|| {
                Url::parse(&format!("{FILES_API}{file_id}/content")).expect("valid")
            });
        let headers = vec![
            (
                "authorization".to_string(),
                format!("Bearer {access_token}"),
            ),
            ("boxapi".to_string(), format!("shared_link={shared_link}")),
        ];
        let response = self
            .http
            .get(download.clone())
            .platform(PLATFORM)
            .user_agent(BROWSER_UA)
            .headers(&headers)
            .follow_redirects(false)
            .send()
            .await?;
        let signed = response
            .header("location")
            .and_then(|location| download.join(location).ok());
        let mut variant = match signed {
            Some(signed) if response.status.is_redirection() => Variant::file(signed),
            _ if response.status.is_success() => {
                let mut variant = Variant::file(download);
                variant.headers = headers;
                variant
            }
            _ => {
                return Err(status_error(response.status, url).unwrap_or_else(|| {
                    ResolveError::malformed(url, "the download link leads nowhere")
                }));
            }
        };
        let extension = file["extension"]
            .as_str()
            .filter(|e| !e.is_empty())
            .map(str::to_ascii_lowercase)
            .or_else(|| {
                file["name"]
                    .as_str()
                    .and_then(|name| name.rsplit_once('.'))
                    .map(|(_, e)| e.to_ascii_lowercase())
            });
        variant.container = extension
            .as_deref()
            .and_then(Container::from_extension)
            .or_else(|| extension.clone().map(Container::Other));
        variant.size = util::uint(&file["size"]);
        variant.format_id = Some("original".to_string());
        variant.label = file["name"].as_str().map(str::to_string);
        let kind = extension
            .as_deref()
            .map(MediaKind::from_extension)
            .unwrap_or(MediaKind::File);
        Ok((kind, variant))
    }
}

#[async_trait]
impl Resolver for BoxResolver {
    fn id(&self) -> &'static str {
        PLATFORM
    }

    fn platform(&self) -> Platform {
        Platform {
            id: PLATFORM,
            name: "Box",
            hosts: &["app.box.com", "ent.box.com"],
            features: &["videos", "images", "files"],
            formats: &["hls", "any file"],
            media: &[
                MediaKind::Video,
                MediaKind::Audio,
                MediaKind::Image,
                MediaKind::File,
            ],
            tags: &[Tag::Files],
            session: SessionSupport::None,
            on_by_default: true,
            examples: &[
                "https://mlssoccer.app.box.com/s/0evd2o3e08l60lr4ygukepvnkord1o1x/file/510727257538",
                "https://app.box.com/s/g23lwdrwzbe3h4xfq1r8t7sr07ko40cc",
                "https://app.box.com/s/j6bpptvsedhmoezmuso6",
            ],
        }
    }

    fn matches(&self, url: &Url) -> bool {
        parse_link(url).is_some()
    }

    async fn resolve(&self, url: &Url) -> Result<Resolution, ResolveError> {
        let link = parse_link(url).ok_or_else(|| ResolveError::NotFound(url.clone()))?;
        let shared_link = format!("https://{}/s/{}", link.host, link.shared_name);
        let page_url = match &link.file_id {
            Some(id) => format!("{shared_link}/file/{id}"),
            None => shared_link.clone(),
        };
        let page_url =
            Url::parse(&page_url).map_err(|e| ResolveError::malformed(url, e.to_string()))?;
        let fetched = fetch(
            &self.http,
            &page_url,
            PLATFORM,
            BROWSER_UA,
            &navigation_headers(),
            MAX_PAGE,
        )
        .await?;
        if let Some(error) = status_error(fetched.status, url) {
            return Err(error);
        }
        let html = fetched.text();
        let file_id = match link.file_id.clone().or_else(|| shared_file_id(&html)) {
            Some(id) => id,
            None => {
                return Err(ResolveError::unavailable(
                    url,
                    "the shared link is not a file",
                ));
            }
        };
        let request_token = util::search(&RE_REQUEST_TOKEN, &html)
            .ok_or_else(|| ResolveError::malformed(url, "the page names no request token"))?;

        // The read token, minted for the file with the page's own request token and
        // session cookies.
        let tokens_url = Url::parse(&format!(
            "https://{}/app-api/enduserapp/elements/tokens",
            link.host
        ))
        .map_err(|e| ResolveError::malformed(url, e.to_string()))?;
        let response = self
            .http
            .post(tokens_url)
            .platform(PLATFORM)
            .user_agent(BROWSER_UA)
            .header("accept", "application/json")
            .header("origin", &format!("https://{}", link.host))
            .header("referer", page_url.as_str())
            .header("x-request-token", &request_token)
            .header(
                "x-box-enduser-api",
                &format!("sharedName={}", link.shared_name),
            )
            .json(&serde_json::json!({"fileIDs": [file_id]}))
            .send()
            .await?;
        if let Some(error) = status_error(response.status, url) {
            return Err(error);
        }
        let tokens: Value = response
            .json(MAX_PAGE)
            .await
            .map_err(|e| ResolveError::malformed(url, format!("token JSON: {e}")))?;
        let access_token = util::text(&tokens[&file_id]["read"])
            .ok_or_else(|| ResolveError::malformed(url, "no read token for the file"))?;

        let api = util::with_query(
            &Url::parse(&format!("{FILES_API}{file_id}")).expect("valid"),
            &[("fields", FILE_FIELDS)],
        );
        let headers = [
            ("accept".to_string(), "application/json".to_string()),
            (
                "authorization".to_string(),
                format!("Bearer {access_token}"),
            ),
            ("boxapi".to_string(), format!("shared_link={shared_link}")),
            ("x-rep-hints".to_string(), "[hls]".to_string()),
        ];
        let described = fetch(&self.http, &api, PLATFORM, BROWSER_UA, &headers, MAX_PAGE).await?;
        if let Some(error) = status_error(described.status, url) {
            return Err(error);
        }
        let file = described.json(url)?;
        let mut resolved = Resolved::new(PLATFORM);
        match hls_master(&file) {
            Some(master) => {
                // The token signs every playlist and segment request, not the master alone
                let signature: Vec<(String, String)> = vec![
                    ("access_token".to_string(), access_token.clone()),
                    ("shared_link".to_string(), shared_link.clone()),
                ];
                let expanded =
                    hls::expand_signed(&self.http, &master, PLATFORM, BROWSER_UA, &[], &signature)
                        .await
                        .map_err(|e| e.at(url))?;
                if expanded.variants.is_empty() {
                    return Err(ResolveError::NotFound(url.clone()));
                }
                resolved.variants = expanded.variants;
                resolved.subtitles = expanded.subtitles;
                resolved.duration = expanded.duration;
            }
            None => {
                let (kind, variant) = self
                    .original(&file, &file_id, &access_token, &shared_link, url)
                    .await?;
                resolved.media = kind;
                resolved.variants.push(variant);
            }
        }
        resolved.id = Some(file_id);
        resolved.title = file["name"].as_str().and_then(clean_title);
        resolved.description = file["description"].as_str().and_then(clean_title);
        resolved.uploaded_at = util::time(&file["created_at"]);
        resolved.uploader = file["created_by"]["name"].as_str().and_then(clean_title);
        resolved.webpage_url = Some(page_url);
        Ok(Resolution::from(resolved))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::{Exchange, Fixture, RecordedBody, RecordedRequest, RecordedResponse};
    use crate::resolve::VariantKind;
    use serde_json::json;

    fn exchange(
        method: &str,
        url: &str,
        status: u16,
        content_type: &str,
        body: String,
    ) -> Exchange {
        Exchange {
            request: RecordedRequest {
                method: method.into(),
                url: url.into(),
                headers: Vec::new(),
                body: None,
            },
            response: RecordedResponse {
                status,
                url: url.into(),
                headers: vec![("content-type".into(), content_type.into())],
                body: RecordedBody::Text(body),
                truncated: false,
            },
        }
    }

    const SHARED: &str =
        "https://mlssoccer.app.box.com/s/0evd2o3e08l60lr4ygukepvnkord1o1x/file/510727257538";

    #[test]
    fn links_are_read() {
        let link = |s: &str| parse_link(&Url::parse(s).unwrap());
        assert_eq!(
            link(SHARED),
            Some(Link {
                host: "mlssoccer.app.box.com".into(),
                shared_name: "0evd2o3e08l60lr4ygukepvnkord1o1x".into(),
                file_id: Some("510727257538".into())
            })
        );
        assert_eq!(
            link("https://utexas.app.box.com/s/2x6vanv85fdl8j2eqlcxmv0gp1wvps6e"),
            Some(Link {
                host: "utexas.app.box.com".into(),
                shared_name: "2x6vanv85fdl8j2eqlcxmv0gp1wvps6e".into(),
                file_id: None
            })
        );
        assert_eq!(
            link("https://thejacksonlaboratory.ent.box.com/s/2x09dm6vcg6y28o0oox1so4l0t8wzt6l/file/1536173056065").map(|l| l.host),
            Some("thejacksonlaboratory.ent.box.com".into())
        );
        assert_eq!(link("https://app.box.com/folder/1"), None);
        assert_eq!(link("https://www.box.com/s/abc"), None);
        assert_eq!(
            shared_file_id(
                r#"<script>Box.postStreamData = {"/app-api/enduserapp/shared-item":{"itemType":"file","itemID":510727257538,"sharedName":"x"}};</script>"#
            ),
            Some("510727257538".into())
        );
        assert_eq!(
            shared_file_id(
                r#"Box.postStreamData = {"/app-api/enduserapp/shared-item":{"itemType":"folder","itemID":1}};"#
            ),
            None
        );
    }

    #[tokio::test]
    async fn shared_videos_resolve_through_the_token_and_files_api() {
        let mut fixture = Fixture::new(PLATFORM, None);
        fixture.exchanges.push(exchange(
            "GET",
            SHARED,
            200,
            "text/html",
            r#"<html><script>Box.config = {"requestToken":"65577221b6bae3245562f4c46d3c46fc"};</script></html>"#.into(),
        ));
        fixture.exchanges.push(exchange(
            "POST",
            "https://mlssoccer.app.box.com/app-api/enduserapp/elements/tokens",
            200,
            "application/json",
            json!({"510727257538": {"read": "1!readtoken", "write": null}}).to_string(),
        ));
        fixture.exchanges.push(exchange(
            "GET",
            "https://api.box.com/2.0/files/510727257538?fields=authenticated_download_url%2Ccreated_at%2Ccreated_by%2Cdescription%2Cextension%2Cname%2Crepresentations%2Csize",
            200,
            "application/json",
            json!({"name": "Garber St. Louis will be 28th MLS team.mp4",
                   "created_at": "2019-08-20T14:00:00-07:00", "created_by": {"name": "MLS Digital", "id": "1"}, "description": "",
                   "representations": {"entries": [
                       {"representation": "hls", "content": {"url_template": "https://public.boxcloud.com/api/2.0/internal_files/510727257538/versions/1/representations/hls/content/{+asset_path}"}, "status": {"state": "success"}},
                       {"representation": "jpg", "content": {"url_template": "https://public.boxcloud.com/x/{+asset_path}"}}
                   ]}}).to_string(),
        ));
        fixture.exchanges.push(exchange(
            "GET",
            "https://public.boxcloud.com/api/2.0/internal_files/510727257538/versions/1/representations/hls/content/master.m3u8?access_token=1%21readtoken&shared_link=https%3A%2F%2Fmlssoccer.app.box.com%2Fs%2F0evd2o3e08l60lr4ygukepvnkord1o1x",
            200,
            "application/vnd.apple.mpegurl",
            "#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=2000000,RESOLUTION=1280x720\n720.m3u8\n".into(),
        ));
        fixture.exchanges.push(exchange(
            "GET",
            "https://public.boxcloud.com/api/2.0/internal_files/510727257538/versions/1/representations/hls/content/720.m3u8?access_token=1%21readtoken&shared_link=https%3A%2F%2Fmlssoccer.app.box.com%2Fs%2F0evd2o3e08l60lr4ygukepvnkord1o1x",
            200,
            "application/vnd.apple.mpegurl",
            "#EXTM3U\n#EXT-X-TARGETDURATION:6\n#EXTINF:6.0,\n0.ts\n#EXT-X-ENDLIST\n".into(),
        ));
        let resolver = BoxResolver::new(Http::replay(fixture));
        let url = Url::parse(SHARED).unwrap();
        assert!(resolver.matches(&url));
        let resolved = resolver.resolve(&url).await.unwrap().media().unwrap();
        assert_eq!(resolved.id.as_deref(), Some("510727257538"));
        assert_eq!(
            resolved.title.as_deref(),
            Some("Garber St. Louis will be 28th MLS team.mp4")
        );
        assert_eq!(resolved.uploader.as_deref(), Some("MLS Digital"));
        assert_eq!(
            resolved.uploaded_at.map(|t| t.as_second()),
            Some(1566334800)
        );
        assert_eq!(resolved.duration, Some(std::time::Duration::from_secs(6)));
        crate::resolve::assert_one_family(&resolved.variants);
        assert_eq!(resolved.variants.len(), 1);
        let rendition = &resolved.variants[0];
        assert_eq!(rendition.kind, VariantKind::Hls);
        assert_eq!(rendition.height, Some(720));
        assert_eq!(
            rendition.url.as_str(),
            "https://public.boxcloud.com/api/2.0/internal_files/510727257538/versions/1/representations/hls/content/720.m3u8?access_token=1%21readtoken&shared_link=https%3A%2F%2Fmlssoccer.app.box.com%2Fs%2F0evd2o3e08l60lr4ygukepvnkord1o1x"
        );
        assert_eq!(
            rendition.query,
            vec![
                ("access_token".to_string(), "1!readtoken".to_string()),
                (
                    "shared_link".to_string(),
                    "https://mlssoccer.app.box.com/s/0evd2o3e08l60lr4ygukepvnkord1o1x".to_string()
                )
            ]
        );
        assert_eq!(resolved.media, MediaKind::Video);
    }

    /// A share of one file: the page, the token, and the files API's description.
    fn share(fixture: &mut Fixture, shared: &str, id: &str, file: serde_json::Value) {
        fixture.exchanges.push(exchange(
            "GET",
            &format!("https://app.box.com/s/{shared}"),
            200,
            "text/html",
            format!(
                r#"<html><script>Box.config = {{"requestToken":"65577221b6bae3245562f4c46d3c46fc"}};Box.postStreamData = {{"/app-api/enduserapp/shared-item":{{"itemType":"file","itemID":{id},"sharedName":"{shared}"}}}};</script></html>"#
            ),
        ));
        fixture.exchanges.push(exchange(
            "POST",
            "https://app.box.com/app-api/enduserapp/elements/tokens",
            200,
            "application/json",
            json!({id: {"read": "1!readtoken", "write": null}}).to_string(),
        ));
        fixture.exchanges.push(exchange(
            "GET",
            &format!("https://api.box.com/2.0/files/{id}?fields=authenticated_download_url%2Ccreated_at%2Ccreated_by%2Cdescription%2Cextension%2Cname%2Crepresentations%2Csize"),
            200,
            "application/json",
            file.to_string(),
        ));
    }

    /// A public PNG share whose representations are thumbnails alone, so the file itself is taken
    #[tokio::test]
    async fn files_without_a_video_stream_are_the_file_itself() {
        let mut fixture = Fixture::new(PLATFORM, None);
        share(
            &mut fixture,
            "g23lwdrwzbe3h4xfq1r8t7sr07ko40cc",
            "390687863449",
            json!({"name": "Screenshot_20190126_195858.png", "extension": "png", "size": 47772,
            "authenticated_download_url": "https://public.boxcloud.com/api/2.0/files/390687863449/content",
            "created_at": "2019-01-26T11:00:41-08:00", "created_by": {"name": "A User", "id": "2"}, "description": "",
            "representations": {"entries": [
                {"representation": "jpg", "content": {"url_template": "https://public.boxcloud.com/api/2.0/internal_files/390687863449/versions/1/representations/jpg_320x320/content/{+asset_path}"}}
            ]}}),
        );
        fixture.exchanges.push(Exchange {
            request: RecordedRequest {
                method: "GET".into(),
                url: "https://public.boxcloud.com/api/2.0/files/390687863449/content".into(),
                headers: Vec::new(),
                body: None,
            },
            response: RecordedResponse {
                status: 302,
                url: "https://public.boxcloud.com/api/2.0/files/390687863449/content".into(),
                headers: vec![
                    ("content-type".into(), "text/html".into()),
                    (
                        "location".into(),
                        "https://public.boxcloud.com/d/1/b1!signed/download".into(),
                    ),
                ],
                body: RecordedBody::Text(String::new()),
                truncated: false,
            },
        });
        let resolver = BoxResolver::new(Http::replay(fixture));
        let resolved = resolver
            .resolve(&Url::parse("https://app.box.com/s/g23lwdrwzbe3h4xfq1r8t7sr07ko40cc").unwrap())
            .await
            .unwrap()
            .media()
            .unwrap();
        assert_eq!(resolved.media, MediaKind::Image);
        assert_eq!(
            resolved.title.as_deref(),
            Some("Screenshot_20190126_195858.png")
        );
        crate::resolve::assert_one_family(&resolved.variants);
        assert_eq!(resolved.variants.len(), 1);
        let file = &resolved.variants[0];
        assert_eq!(file.kind, VariantKind::File);
        assert_eq!(
            file.url.as_str(),
            "https://public.boxcloud.com/d/1/b1!signed/download"
        );
        assert!(file.headers.is_empty(), "the signed link needs no token");
        assert_eq!(file.container, Some(Container::Png));
        assert_eq!(file.size, Some(47772));
        assert_eq!(file.format_id.as_deref(), Some("original"));
    }

    /// Live, the example video resolves to HLS renditions and the image and PDF to the files themselves
    #[ignore = "reaches the live site: cargo test -- --ignored"]
    #[tokio::test]
    async fn live_examples_resolve_to_hls_or_the_file() {
        use std::time::Duration;

        let resolver = BoxResolver::new(Http::new(crate::http::HttpConfig::default()));
        let mut videos = 0;
        let mut files = 0;
        for link in resolver.platform().examples {
            let url = Url::parse(link).unwrap();
            let resolved = tokio::time::timeout(Duration::from_secs(60), resolver.resolve(&url))
                .await
                .expect("resolution timed out")
                .unwrap_or_else(|error| panic!("{link}: {error}"))
                .media()
                .unwrap();
            assert!(!resolved.variants.is_empty(), "{link}: no variants");
            crate::resolve::assert_one_family(&resolved.variants);
            if resolved.media == MediaKind::Video {
                assert!(
                    resolved.variants.iter().all(|v| v.kind == VariantKind::Hls),
                    "{link}: a variant other than HLS"
                );
                videos += 1;
            } else {
                assert_eq!(resolved.variants.len(), 1, "{link}: one file");
                assert_eq!(resolved.variants[0].kind, VariantKind::File);
                files += 1;
            }
        }
        assert_eq!(videos, 1, "one example is a video");
        assert_eq!(files, 2, "an image and a PDF");
    }
}
