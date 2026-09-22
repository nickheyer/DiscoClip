//! InnerTube, the API YouTube's own apps speak: which app to be, how to ask, and how a
//! logged-in session signs what it asks.

use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};
use url::Url;

use crate::http::{BROWSER_UA, Http};
use crate::resolve::{MAX_PAGE, ResolveError, check_status};

pub const PLATFORM: &str = "youtube";
pub const ORIGIN: &str = "https://www.youtube.com";

/// An app YouTube's API knows. Each is shown somewhat different formats and gates.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Client {
    pub id: &'static str,
    pub name: &'static str,
    pub version: &'static str,
    /// The `X-YouTube-Client-Name` header.
    pub number: u32,
    pub user_agent: &'static str,
    /// Its formats carry signature ciphers the player script deciphers.
    pub needs_player: bool,
    /// Asks as a player embedded on another page, which age gates let through for
    /// videos that may be embedded.
    pub embedded: bool,
    /// Whether a logged-in session's credentials go with its requests.
    pub takes_session: bool,
    pub device: Option<(&'static str, &'static str)>,
    pub os: Option<(&'static str, &'static str)>,
}

const TV_UA: &str = "Mozilla/5.0 (ChromiumStylePlatform) Cobalt/Version";

pub const TV: Client = Client {
    id: "tv",
    name: "TVHTML5",
    version: "7.20250312.16.00",
    number: 7,
    user_agent: TV_UA,
    needs_player: true,
    embedded: false,
    takes_session: false,
    device: None,
    os: None,
};

/// The native VisionOS player exposes media without the iOS client's required
/// proof-of-origin token. Keep its API identity and media user agent together.
pub const VISIONOS: Client = Client {
    id: "visionos",
    name: "VISIONOS",
    version: "1.02",
    number: 101,
    user_agent: "Mozilla/5.0 (Macintosh; Intel Mac OS X 15_7_3) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/26.0 Safari/605.1.15",
    needs_player: false,
    embedded: false,
    takes_session: false,
    device: Some(("Apple", "RealityDevice17,1")),
    os: Some(("visionOS", "26.5.23O471")),
};

pub const WEB: Client = Client {
    id: "web",
    name: "WEB",
    version: "2.20250312.04.00",
    number: 1,
    user_agent: BROWSER_UA,
    needs_player: true,
    embedded: false,
    takes_session: true,
    device: None,
    os: None,
};

pub const TV_EMBEDDED: Client = Client {
    id: "tv_embedded",
    name: "TVHTML5_SIMPLY_EMBEDDED_PLAYER",
    version: "2.0",
    number: 85,
    user_agent: TV_UA,
    needs_player: true,
    embedded: true,
    takes_session: true,
    device: None,
    os: None,
};

/// The apps asked for a video, in order, until one plays it.
pub const CLIENTS: [Client; 3] = [TV, VISIONOS, WEB];

/// Talks to the API as one app or another, with the platform's cookies.
#[derive(Clone)]
pub struct InnerTube {
    http: Http,
}

impl InnerTube {
    pub fn new(http: Http) -> Self {
        Self { http }
    }

    pub fn http(&self) -> &Http {
        &self.http
    }

    fn context(client: &Client, watch_url: &str) -> Value {
        let mut inner = json!({
            "clientName": client.name,
            "clientVersion": client.version,
            "hl": "en",
            "gl": "US",
            "userAgent": client.user_agent,
        });
        if let Some((make, model)) = client.device {
            inner["deviceMake"] = json!(make);
            inner["deviceModel"] = json!(model);
        }
        if let Some((name, version)) = client.os {
            inner["osName"] = json!(name);
            inner["osVersion"] = json!(version);
        }
        if client.embedded {
            inner["clientScreen"] = json!("EMBED");
        }
        let mut context = json!({ "client": inner });
        if client.embedded {
            context["thirdParty"] = json!({ "embedUrl": watch_url });
        }
        context
    }

    /// POSTs to `endpoint`, such as `player` or `browse`, as `client`, with `body` beside
    /// the app's context, and returns the JSON answer.
    pub async fn call(
        &self,
        endpoint: &str,
        client: &Client,
        body: Value,
        watch_url: &Url,
    ) -> Result<Value, ResolveError> {
        self.call_with_visitor(endpoint, client, body, watch_url, None)
            .await
    }

    async fn call_with_visitor(
        &self,
        endpoint: &str,
        client: &Client,
        mut body: Value,
        watch_url: &Url,
        visitor_data: Option<&str>,
    ) -> Result<Value, ResolveError> {
        let url = Url::parse(&format!(
            "{ORIGIN}/youtubei/v1/{endpoint}?prettyPrint=false"
        ))
        .expect("the endpoint url is valid");
        body["context"] = Self::context(client, watch_url.as_str());
        if let Some(visitor_data) = visitor_data {
            body["context"]["client"]["visitorData"] = json!(visitor_data);
        }
        let mut request = self
            .http
            .post(url)
            .platform(PLATFORM)
            .user_agent(client.user_agent)
            .header("x-youtube-client-name", &client.number.to_string())
            .header("x-youtube-client-version", client.version)
            .header("origin", ORIGIN)
            .header("referer", &format!("{ORIGIN}/"))
            .json(&body);
        if let Some(visitor_data) = visitor_data {
            request = request.header("x-goog-visitor-id", visitor_data);
        }
        if client.takes_session
            && let Some(authorization) = self.authorization()
        {
            request = request
                .header("authorization", &authorization)
                .header("x-origin", ORIGIN);
        }
        let response = request.send().await?;
        check_status(&response, watch_url)?;
        let value: Value = response
            .json(MAX_PAGE)
            .await
            .map_err(|e| ResolveError::malformed(watch_url, e.to_string()))?;
        Ok(value)
    }

    /// The `player` answer for `video_id` as `client`. `sts` is the player script's
    /// signature timestamp, which apps that need the player must send.
    pub async fn player(
        &self,
        client: &Client,
        video_id: &str,
        sts: Option<u64>,
        watch_url: &Url,
        visitor_data: Option<&str>,
    ) -> Result<Value, ResolveError> {
        let mut body = json!({
            "videoId": video_id,
            "contentCheckOk": true,
            "racyCheckOk": true,
        });
        if let Some(sts) = sts {
            body["playbackContext"] = json!({
                "contentPlaybackContext": { "signatureTimestamp": sts, "html5Preference": "HTML5_PREF_WANTS" }
            });
        }
        self.call_with_visitor("player", client, body, watch_url, visitor_data)
            .await
    }

    /// Whether the jar holds a logged-in session.
    pub fn logged_in(&self) -> bool {
        self.authorization().is_some()
    }

    /// The `SAPISIDHASH` a logged-in session signs its requests with, when the jar
    /// holds one.
    pub fn authorization(&self) -> Option<String> {
        let jar = self.http.jar(PLATFORM);
        let sapisid = jar
            .get("SAPISID")
            .or_else(|| jar.get("__Secure-3PAPISID"))?
            .value
            .clone();
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        Some(sapisid_hash(now, &sapisid))
    }
}

/// `SAPISIDHASH <time>_<sha1(time SAPISID origin)>`.
pub fn sapisid_hash(now: u64, sapisid: &str) -> String {
    let digest = sha1_smol::Sha1::from(format!("{now} {sapisid} {ORIGIN}"))
        .digest()
        .to_string();
    format!("SAPISIDHASH {now}_{digest}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_session_signs_with_its_sapisid() {
        let digest = sha1_smol::Sha1::from("1700000000 abc https://www.youtube.com").digest();
        assert_eq!(
            sapisid_hash(1_700_000_000, "abc"),
            format!("SAPISIDHASH 1700000000_{digest}")
        );
    }

    #[test]
    fn contexts_carry_the_app_and_the_embedding_page() {
        let context = InnerTube::context(&VISIONOS, "https://www.youtube.com/watch?v=x");
        assert_eq!(context["client"]["clientName"], "VISIONOS");
        assert_eq!(context["client"]["deviceModel"], "RealityDevice17,1");
        assert!(context.get("thirdParty").is_none());
        let context = InnerTube::context(&TV_EMBEDDED, "https://www.youtube.com/watch?v=x");
        assert_eq!(
            context["thirdParty"]["embedUrl"],
            "https://www.youtube.com/watch?v=x"
        );
        assert_eq!(context["client"]["clientScreen"], "EMBED");
    }
}
