//! Box shared links (`{account}.app.box.com/s/{name}`): the shared page names its file
//! and a request token, the token endpoint trades that for a read token, and the files
//! API then names the file's download link and its HLS and DASH representations.

use std::sync::LazyLock;

use async_trait::async_trait;
use regex::Regex;
use serde_json::Value;
use url::Url;

use super::{
    MAX_PAGE, Platform, Resolution, ResolveError, Resolved, Resolver, SessionSupport, Variant,
    clean_title, dash, fetch, hls, navigation_headers, status_error, util,
};
use crate::http::{BROWSER_UA, Http};
use crate::media::Container;

pub const PLATFORM: &str = "box";
const FILES_API: &str = "https://api.box.com/2.0/files/";
const FILE_FIELDS: &str = "authenticated_download_url,created_at,created_by,description,extension,is_download_available,name,representations,size";

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

pub struct BoxResolver {
    http: Http,
}

impl BoxResolver {
    pub fn new(http: Http) -> Self {
        Self { http }
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
            features: &["files"],
            formats: &["mp4", "hls", "dash"],
            session: SessionSupport::None,
            examples: &[
                "https://mlssoccer.app.box.com/s/0evd2o3e08l60lr4ygukepvnkord1o1x/file/510727257538",
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
            ("x-rep-hints".to_string(), "[hls][dash]".to_string()),
        ];
        let described = fetch(&self.http, &api, PLATFORM, BROWSER_UA, &headers, MAX_PAGE).await?;
        if let Some(error) = status_error(described.status, url) {
            return Err(error);
        }
        let file = described.json(url)?;
        let query = [
            ("access_token", access_token.as_str()),
            ("shared_link", shared_link.as_str()),
        ];
        let extension = file["extension"]
            .as_str()
            .unwrap_or("")
            .to_ascii_lowercase();

        let mut resolved = Resolved::new(PLATFORM);
        let mut failure = None;
        for entry in file["representations"]["entries"]
            .as_array()
            .into_iter()
            .flatten()
        {
            let Some(template) = entry["content"]["url_template"].as_str() else {
                continue;
            };
            let manifest = match entry["representation"].as_str() {
                Some("hls") => template.replace("{+asset_path}", "master.m3u8"),
                Some("dash") => template.replace("{+asset_path}", "manifest.mpd"),
                _ => continue,
            };
            let Some(manifest) = util::join_url(None, &manifest) else {
                continue;
            };
            let manifest = util::with_query(&manifest, &query);
            // The token signs every playlist and segment request, not the manifest alone.
            let signature: Vec<(String, String)> = query
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect();
            if manifest.path().ends_with(".m3u8") {
                match hls::expand(&self.http, &manifest, PLATFORM, BROWSER_UA, &[]).await {
                    Ok(expanded) => {
                        for mut variant in expanded.variants {
                            variant.url = variant.signed_with(&signature);
                            variant.query = signature.clone();
                            resolved.variants.push(variant);
                        }
                        for mut track in expanded.subtitles {
                            track.url = super::signed_url(&track.url, &signature);
                            resolved.subtitles.push(track);
                        }
                        resolved.duration = resolved.duration.or(expanded.duration);
                    }
                    Err(error) => failure = Some(error),
                }
            } else {
                match dash::expand(&self.http, &manifest, PLATFORM, BROWSER_UA, &[]).await {
                    Ok(expanded) => {
                        for mut variant in expanded.variants {
                            variant.url = variant.signed_with(&signature);
                            variant.query = signature.clone();
                            resolved.variants.push(variant);
                        }
                        for mut track in expanded.subtitles {
                            track.url = super::signed_url(&track.url, &signature);
                            resolved.subtitles.push(track);
                        }
                        resolved.duration = resolved.duration.or(expanded.duration);
                    }
                    Err(error) => failure = Some(error),
                }
            }
        }
        if file["is_download_available"].as_bool() == Some(true)
            && let Some(download) = util::url_of(&file["authenticated_download_url"], None)
        {
            let mut variant = Variant::file(util::with_query(&download, &query));
            variant.size = util::uint(&file["size"]);
            variant.container = Some(
                Container::from_extension(&extension)
                    .unwrap_or(Container::Other(extension.clone())),
            );
            variant.format_id = Some("download".to_string());
            variant.label = Some("original".to_string());
            resolved.variants.push(variant);
        }
        if resolved.variants.is_empty() {
            return Err(failure.unwrap_or_else(|| ResolveError::NotFound(url.clone())));
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
    async fn shared_files_resolve_through_the_token_and_files_api() {
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
            "https://api.box.com/2.0/files/510727257538?fields=authenticated_download_url%2Ccreated_at%2Ccreated_by%2Cdescription%2Cextension%2Cis_download_available%2Cname%2Crepresentations%2Csize",
            200,
            "application/json",
            json!({"name": "Garber St. Louis will be 28th MLS team.mp4", "extension": "mp4", "size": 33730279, "is_download_available": true,
                   "created_at": "2019-08-20T14:00:00-07:00", "created_by": {"name": "MLS Digital", "id": "1"}, "description": "",
                   "authenticated_download_url": "https://public.boxcloud.com/api/2.0/files/510727257538/content",
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
            "https://public.boxcloud.com/api/2.0/internal_files/510727257538/versions/1/representations/hls/content/720.m3u8",
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
        assert_eq!(resolved.variants.len(), 2);
        assert_eq!(resolved.variants[0].kind, VariantKind::Hls);
        assert_eq!(resolved.variants[0].height, Some(720));
        let download = &resolved.variants[1];
        assert_eq!(download.kind, VariantKind::File);
        assert_eq!(download.size, Some(33730279));
        assert!(download.url.as_str().starts_with("https://public.boxcloud.com/api/2.0/files/510727257538/content?access_token=1%21readtoken&shared_link="));
    }
}
