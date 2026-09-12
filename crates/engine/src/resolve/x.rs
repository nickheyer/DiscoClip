//! Twitter and X posts through the site's own GraphQL API as the web app uses it, as a
//! guest or with a logged-in session, and through the fxtwitter mirror when the API
//! refuses a guest.

use std::time::Duration;

use async_trait::async_trait;
use serde_json::{Value, json};
use url::Url;

use super::twitter::{TwitterResolver, status_id};
use super::{
    MAX_PAGE, Platform, Resolution, ResolveError, Resolved, Resolver, SessionCheck, SessionSupport,
    Variant, VariantKind, clean_title,
};
use crate::http::{BROWSER_UA, Http};
use crate::media::{AudioCodec, Container, VideoCodec};

pub const PLATFORM: &str = "twitter";
/// The bearer token the web app itself carries.
const BEARER: &str = "Bearer AAAAAAAAAAAAAAAAAAAAANRILgAAAAAAnNwIzUejRCOuH5E6I8xnZz4puTs%3D1Zv7ttfk8LF81IUq16cHjhLTvJu4FA33AGWWjCpTnA";
const GUEST_ACTIVATE: &str = "https://api.x.com/1.1/guest/activate.json";
const SETTINGS: &str = "https://x.com/i/api/1.1/account/settings.json";
/// The `TweetResultByRestId` query, by the id the web app registers it under.
const TWEET_QUERY: &str = "https://x.com/i/api/graphql/0hWvDhmW8YQ-S_ib3azIrw/TweetResultByRestId";

fn features() -> Value {
    json!({
        "creator_subscriptions_tweet_preview_api_enabled": true,
        "communities_web_enable_tweet_community_results_fetch": true,
        "c9s_tweet_anatomy_moderator_badge_enabled": true,
        "articles_preview_enabled": true,
        "tweetypie_unmention_optimization_enabled": true,
        "responsive_web_edit_tweet_api_enabled": true,
        "graphql_is_translatable_rweb_tweet_is_translatable_enabled": true,
        "view_counts_everywhere_api_enabled": true,
        "longform_notetweets_consumption_enabled": true,
        "responsive_web_twitter_article_tweet_consumption_enabled": true,
        "tweet_awards_web_tipping_enabled": false,
        "creator_subscriptions_quote_tweet_preview_enabled": false,
        "freedom_of_speech_not_reach_fetch_enabled": true,
        "standardized_nudges_misinfo": true,
        "tweet_with_visibility_results_prefer_gql_limited_actions_policy_enabled": true,
        "rweb_video_timestamps_enabled": true,
        "longform_notetweets_rich_text_read_enabled": true,
        "longform_notetweets_inline_media_enabled": true,
        "rweb_tipjar_consumption_enabled": true,
        "responsive_web_graphql_exclude_directive_enabled": true,
        "verified_phone_label_enabled": false,
        "responsive_web_graphql_skip_user_profile_image_extensions_enabled": false,
        "responsive_web_graphql_timeline_navigation_enabled": true,
        "responsive_web_enhance_cards_enabled": false
    })
}

pub struct XResolver {
    http: Http,
    mirror: TwitterResolver,
}

/// How a request to the API is signed: as a logged-in session, or as a guest.
enum Auth {
    Session { csrf: String },
    Guest { token: String },
}

impl XResolver {
    pub fn new(http: Http) -> Self {
        Self {
            mirror: TwitterResolver::new(http.clone()),
            http,
        }
    }

    fn session_csrf(&self) -> Option<String> {
        let jar = self.http.jar(PLATFORM);
        jar.get("auth_token")?;
        jar.get("ct0").map(|c| c.value.clone())
    }

    async fn guest_token(&self, origin: &Url) -> Result<String, ResolveError> {
        let activate = Url::parse(GUEST_ACTIVATE).expect("valid");
        let response = self
            .http
            .post(activate)
            .platform(PLATFORM)
            .user_agent(BROWSER_UA)
            .header("authorization", BEARER)
            .no_cookies()
            .send()
            .await?;
        if !response.is_success() {
            return Err(ResolveError::unavailable(
                origin,
                format!("guest activation answered HTTP {}", response.status),
            ));
        }
        let value: Value = response
            .json(MAX_PAGE)
            .await
            .map_err(|e| ResolveError::malformed(origin, e.to_string()))?;
        value["guest_token"]
            .as_str()
            .map(String::from)
            .ok_or_else(|| ResolveError::malformed(origin, "guest activation gave no token"))
    }

    async fn auth(&self, origin: &Url) -> Result<Auth, ResolveError> {
        match self.session_csrf() {
            Some(csrf) => Ok(Auth::Session { csrf }),
            None => Ok(Auth::Guest {
                token: self.guest_token(origin).await?,
            }),
        }
    }

    async fn api_get(&self, url: Url, auth: &Auth, origin: &Url) -> Result<Value, ResolveError> {
        let mut request = self
            .http
            .get(url)
            .platform(PLATFORM)
            .user_agent(BROWSER_UA)
            .header("authorization", BEARER)
            .header("x-twitter-active-user", "yes")
            .header("x-twitter-client-language", "en");
        request = match auth {
            Auth::Session { csrf } => request
                .header("x-csrf-token", csrf)
                .header("x-twitter-auth-type", "OAuth2Session"),
            Auth::Guest { token } => request.header("x-guest-token", token).no_cookies(),
        };
        let response = request.send().await?;
        let status = response.status;
        let value: Value = response
            .json(MAX_PAGE)
            .await
            .map_err(|e| ResolveError::malformed(origin, e.to_string()))?;
        if !status.is_success() {
            let message = value["errors"][0]["message"]
                .as_str()
                .unwrap_or("no message")
                .to_string();
            return Err(match status.as_u16() {
                429 => ResolveError::RateLimited(origin.clone()),
                404 => ResolveError::NotFound(origin.clone()),
                _ => ResolveError::unavailable(origin, format!("HTTP {status}: {message}")),
            });
        }
        Ok(value)
    }

    /// The post through the API, as the session or a guest.
    async fn native(&self, id: &str, url: &Url) -> Result<Resolved, ResolveError> {
        let auth = self.auth(url).await?;
        let mut query = Url::parse(TWEET_QUERY).expect("valid");
        let variables = json!({
            "tweetId": id,
            "withCommunity": false,
            "includePromotedContent": false,
            "withVoice": false
        });
        query
            .query_pairs_mut()
            .append_pair("variables", &variables.to_string())
            .append_pair("features", &features().to_string());
        let answer = self.api_get(query, &auth, url).await?;
        let mut result = answer
            .pointer("/data/tweetResult/result")
            .cloned()
            .ok_or_else(|| ResolveError::NotFound(url.clone()))?;
        if result["__typename"].as_str() == Some("TweetWithVisibilityResults") {
            result = result["tweet"].clone();
        }
        if result["__typename"].as_str() == Some("TweetUnavailable") {
            let reason = result["reason"].as_str().unwrap_or("unavailable");
            return Err(if reason == "NsfwLoggedOut" || reason == "Protected" {
                ResolveError::login_required(url, PLATFORM, reason)
            } else {
                ResolveError::unavailable(url, reason)
            });
        }
        let legacy = &result["legacy"];
        let user = &result["core"]["user_results"]["result"]["legacy"];
        let handle = user["screen_name"].as_str().unwrap_or("");
        let name = user["name"].as_str().unwrap_or("");
        let text = legacy["full_text"].as_str().unwrap_or("");
        let media = legacy["extended_entities"]["media"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        let video = media
            .iter()
            .find(|m| matches!(m["type"].as_str(), Some("video") | Some("animated_gif")))
            .ok_or_else(|| ResolveError::NotFound(url.clone()))?;
        let duration = video["video_info"]["duration_millis"]
            .as_u64()
            .filter(|d| *d > 0)
            .map(Duration::from_millis);
        let width = video["original_info"]["width"].as_u64().map(|w| w as u32);
        let height = video["original_info"]["height"].as_u64().map(|h| h as u32);
        let mut variants = Vec::new();
        for entry in video["video_info"]["variants"]
            .as_array()
            .into_iter()
            .flatten()
        {
            let Some(variant_url) = entry["url"].as_str().and_then(|u| Url::parse(u).ok()) else {
                continue;
            };
            let content_type = entry["content_type"].as_str().unwrap_or("");
            if content_type == "video/mp4" {
                let mut v = Variant::new(variant_url, VariantKind::File);
                v.container = Some(Container::Mp4);
                v.video = Some(VideoCodec::H264);
                v.audio = Some(AudioCodec::Aac);
                v.bitrate = entry["bitrate"].as_u64().filter(|b| *b > 0);
                v.duration = duration;
                let dims = super::twitter::dimensions(&v.url);
                v.width = dims.map(|d| d.0).or(width);
                v.height = dims.map(|d| d.1).or(height);
                variants.push(v);
            } else if content_type.to_ascii_lowercase().contains("mpegurl") {
                let mut v = Variant::new(variant_url, VariantKind::Hls);
                v.duration = duration;
                variants.push(v);
            }
        }
        if variants.is_empty() {
            return Err(ResolveError::NotFound(url.clone()));
        }
        let mut resolved = Resolved::new(PLATFORM);
        resolved.id = Some(id.to_string());
        resolved.title = clean_title(&format!("{name} @{handle} {text}"));
        resolved.description = (!text.is_empty()).then(|| text.to_string());
        resolved.uploader = (!handle.is_empty()).then(|| format!("@{handle}"));
        resolved.uploader_url = (!handle.is_empty())
            .then(|| Url::parse(&format!("https://x.com/{handle}")).ok())
            .flatten();
        resolved.uploaded_at = legacy["created_at"]
            .as_str()
            .and_then(crate::http::cookies::parse_http_date);
        resolved.duration = duration;
        resolved.thumbnail = video["media_url_https"]
            .as_str()
            .and_then(|t| Url::parse(t).ok());
        resolved.webpage_url = (!handle.is_empty())
            .then(|| Url::parse(&format!("https://x.com/{handle}/status/{id}")).ok())
            .flatten();
        resolved.age_limit = legacy["possibly_sensitive"]
            .as_bool()
            .filter(|s| *s)
            .map(|_| 18);
        resolved.variants = variants;
        Ok(resolved)
    }
}

#[async_trait]
impl Resolver for XResolver {
    fn id(&self) -> &'static str {
        PLATFORM
    }

    fn platform(&self) -> Platform {
        Platform {
            id: PLATFORM,
            name: "Twitter / X",
            hosts: &[
                "twitter.com",
                "x.com",
                "fxtwitter.com",
                "vxtwitter.com",
                "fixupx.com",
                "fixvx.com",
            ],
            features: &["videos", "gifs", "sensitive media with a session"],
            formats: &["mp4", "hls"],
            session: SessionSupport::Optional,
            examples: &["https://x.com/SpaceX/status/1732824684683784516"],
        }
    }

    fn matches(&self, url: &Url) -> bool {
        self.mirror.matches(url)
    }

    /// The API first; when it refuses, the mirror, whose answer stands unless the API's
    /// refusal was the more telling one.
    async fn resolve(&self, url: &Url) -> Result<Resolution, ResolveError> {
        let id = status_id(url).ok_or_else(|| ResolveError::NotFound(url.clone()))?;
        let refusal = match self.native(&id, url).await {
            Ok(resolved) => return Ok(Resolution::from(resolved)),
            Err(error @ ResolveError::LoginRequired { .. }) => return Err(error),
            Err(error) => error,
        };
        tracing::debug!(%url, "x api refused: {refusal}; asking the mirror");
        match self.mirror.resolve(url).await {
            Ok(resolution) => Ok(resolution),
            Err(mirror) if mirror.is_expected() && refusal.is_expected() => Err(mirror),
            Err(mirror) => Err(ResolveError::unavailable(
                url,
                format!("the API refused ({refusal}) and so did the mirror ({mirror})"),
            )),
        }
    }

    async fn check_session(&self) -> Result<SessionCheck, ResolveError> {
        let Some(csrf) = self.session_csrf() else {
            return Ok(SessionCheck::LoggedOut);
        };
        let settings = Url::parse(SETTINGS).expect("valid");
        let answer = self
            .api_get(settings.clone(), &Auth::Session { csrf }, &settings)
            .await;
        Ok(match answer {
            Ok(value) => match value["screen_name"].as_str() {
                Some(name) => SessionCheck::LoggedIn {
                    account: format!("@{name}"),
                },
                None => SessionCheck::LoggedOut,
            },
            Err(ResolveError::Unavailable { .. }) => SessionCheck::LoggedOut,
            Err(error) => return Err(error),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::Cookie;
    use crate::http::transport::{
        Exchange, Fixture, RecordedBody, RecordedRequest, RecordedResponse,
    };

    fn exchange(method: &str, url: &str, status: u16, body: Value) -> Exchange {
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
                headers: vec![("content-type".into(), "application/json".into())],
                body: RecordedBody::Text(body.to_string()),
                truncated: false,
            },
        }
    }

    fn tweet(kind: &str) -> Value {
        json!({"data": {"tweetResult": {"result": {
            "__typename": "Tweet",
            "core": {"user_results": {"result": {"legacy": {"screen_name": "SpaceX", "name": "SpaceX"}}}},
            "legacy": {
                "full_text": "Liftoff!", "created_at": "Thu Dec 07 18:30:00 +0000 2023", "possibly_sensitive": false,
                "extended_entities": {"media": [{
                    "type": kind, "media_url_https": "https://pbs.twimg.com/thumb.jpg",
                    "original_info": {"width": 1280, "height": 720},
                    "video_info": {"duration_millis": 42000, "variants": [
                        {"content_type": "application/x-mpegURL", "url": "https://video.twimg.com/ext_tw_video/1/pl/x.m3u8"},
                        {"content_type": "video/mp4", "bitrate": 832000, "url": "https://video.twimg.com/ext_tw_video/1/vid/640x360/a.mp4"},
                        {"content_type": "video/mp4", "bitrate": 2176000, "url": "https://video.twimg.com/ext_tw_video/1/vid/1280x720/b.mp4"}
                    ]}
                }]}
            }
        }}}})
    }

    #[tokio::test]
    async fn guests_read_posts_through_the_api() {
        let mut fixture = Fixture::new("x", None);
        fixture.exchanges.push(exchange(
            "POST",
            GUEST_ACTIVATE,
            200,
            json!({"guest_token": "g123"}),
        ));
        fixture
            .exchanges
            .push(exchange("GET", TWEET_QUERY, 200, tweet("video")));
        let resolver = XResolver::new(Http::replay(fixture));
        let resolved = resolver
            .resolve(&Url::parse("https://x.com/SpaceX/status/1732824684683784516").unwrap())
            .await
            .unwrap()
            .media()
            .unwrap();
        assert_eq!(resolved.title.as_deref(), Some("SpaceX @SpaceX Liftoff!"));
        assert_eq!(resolved.uploader.as_deref(), Some("@SpaceX"));
        assert_eq!(resolved.duration, Some(Duration::from_secs(42)));
        assert!(resolved.uploaded_at.is_some());
        assert_eq!(resolved.variants.len(), 3);
        assert_eq!(resolved.variants[0].kind, VariantKind::Hls);
        assert_eq!(resolved.variants[2].height, Some(720));
        assert_eq!(resolved.variants[2].bitrate, Some(2176000));
        assert_eq!(
            resolved.webpage_url.unwrap().as_str(),
            "https://x.com/SpaceX/status/1732824684683784516"
        );
    }

    #[tokio::test]
    async fn refusals_fall_back_to_the_mirror_and_gates_need_a_session() {
        let mut fixture = Fixture::new("x", None);
        fixture.exchanges.push(exchange(
            "POST",
            GUEST_ACTIVATE,
            403,
            json!({"errors": [{"message": "Forbidden"}]}),
        ));
        fixture.exchanges.push(exchange(
            "GET",
            "https://api.fxtwitter.com/status/1732824684683784516",
            200,
            json!({"code": 200, "tweet": {"author": {"name": "SpaceX", "screen_name": "SpaceX"}, "text": "Liftoff!", "media": {"videos": [{"url": "https://video.twimg.com/a.mp4", "duration": 42, "width": 1280, "height": 720, "variants": []}]}}}),
        ));
        let resolver = XResolver::new(Http::replay(fixture));
        let resolved = resolver
            .resolve(&Url::parse("https://twitter.com/SpaceX/status/1732824684683784516").unwrap())
            .await
            .unwrap()
            .media()
            .unwrap();
        assert_eq!(resolved.variants.len(), 1);

        let mut fixture = Fixture::new("x", None);
        fixture.exchanges.push(exchange(
            "POST",
            GUEST_ACTIVATE,
            200,
            json!({"guest_token": "g123"}),
        ));
        fixture.exchanges.push(exchange(
            "GET",
            TWEET_QUERY,
            200,
            json!({"data": {"tweetResult": {"result": {"__typename": "TweetUnavailable", "reason": "NsfwLoggedOut"}}}}),
        ));
        let resolver = XResolver::new(Http::replay(fixture));
        let error = resolver
            .resolve(&Url::parse("https://x.com/someone/status/1732824684683784516").unwrap())
            .await
            .unwrap_err();
        assert!(
            matches!(error, ResolveError::LoginRequired { .. }),
            "{error}"
        );
    }

    #[tokio::test]
    async fn sessions_sign_with_their_csrf_token() {
        let mut fixture = Fixture::new("x", None);
        fixture.exchanges.push(exchange(
            "GET",
            SETTINGS,
            200,
            json!({"screen_name": "nick"}),
        ));
        let http = Http::replay(fixture);
        let resolver = XResolver::new(http.clone());
        assert_eq!(
            resolver.check_session().await.unwrap(),
            SessionCheck::LoggedOut
        );
        http.with_jar(PLATFORM, |jar| {
            jar.insert(Cookie::new("auth_token", "a", "x.com"));
            jar.insert(Cookie::new("ct0", "c", "x.com"));
        });
        assert_eq!(
            resolver.check_session().await.unwrap(),
            SessionCheck::LoggedIn {
                account: "@nick".into()
            }
        );
    }
}
