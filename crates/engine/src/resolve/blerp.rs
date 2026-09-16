//! Blerp sound bites, through the GraphQL API the site's bite page calls.

use std::sync::LazyLock;

use async_trait::async_trait;
use regex::Regex;
use serde_json::{Value, json};
use url::Url;

use super::{
    MAX_PAGE, Platform, Resolution, ResolveError, Resolved, Resolver, SessionSupport, Variant,
    clean_title, status_error, util,
};
use crate::http::{BROWSER_UA, Http};
use crate::media::{AudioCodec, Container};

pub const PLATFORM: &str = "blerp";
const GRAPHQL: &str = "https://api.blerp.com/graphql";
const OPERATION: &str = "webBitePageGetBite";
/// The query the bite page sends, whole: the API refuses unknown selections.
const QUERY: &str = r#"query webBitePageGetBite($_id: MongoID!) {
            web {
                biteById(_id: $_id) {
                    ...bitePageFrag
                    __typename
                }
                __typename
            }
        }

        fragment bitePageFrag on Bite {
            _id
            title
            userKeywords
            keywords
            color
            visibility
            isPremium
            owned
            price
            extraReview
            isAudioExists
            image {
                filename
                original {
                    url
                    __typename
                }
                __typename
            }
            userReactions {
                _id
                reactions
                createdAt
                __typename
            }
            topReactions
            totalSaveCount
            saved
            blerpLibraryType
            license
            licenseMetaData
            playCount
            totalShareCount
            totalFavoriteCount
            totalAddedToBoardCount
            userCategory
            userAudioQuality
            audioCreationState
            transcription
            userTranscription
            description
            createdAt
            updatedAt
            author
            listingType
            ownerObject {
                _id
                username
                profileImage {
                    filename
                    original {
                        url
                        __typename
                    }
                    __typename
                }
                __typename
            }
            transcription
            favorited
            visibility
            isCurated
            sourceUrl
            audienceRating
            strictAudienceRating
            ownerId
            reportObject {
                reportedContentStatus
                __typename
            }
            giphy {
                mp4
                gif
                __typename
            }
            audio {
                filename
                original {
                    url
                    __typename
                }
                mp3 {
                    url
                    __typename
                }
                __typename
            }
            __typename
        }

        "#;

/// `/soundbites/{id}`; the id is the leading run of letters and digits.
static RE_BITE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^/soundbites/([0-9a-zA-Z]+)").unwrap());

/// The bite id a link names.
pub fn bite_id(url: &Url) -> Option<String> {
    if !matches!(url.scheme(), "http" | "https") {
        return None;
    }
    let host = url.host_str()?.to_ascii_lowercase();
    if host != "blerp.com" && host != "www.blerp.com" {
        return None;
    }
    RE_BITE.captures(url.path()).map(|c| c[1].to_string())
}

pub struct BlerpResolver {
    http: Http,
}

impl BlerpResolver {
    pub fn new(http: Http) -> Self {
        Self { http }
    }

    /// The bite the GraphQL API describes, or why it does not.
    async fn bite(&self, id: &str, origin: &Url) -> Result<Value, ResolveError> {
        let api = Url::parse(GRAPHQL).expect("valid");
        let response = self
            .http
            .post(api)
            .platform(PLATFORM)
            .user_agent(BROWSER_UA)
            .header("accept", "application/json")
            .header("origin", "https://blerp.com")
            .header("referer", "https://blerp.com/")
            .json(&json!({
                "operationName": OPERATION,
                "query": QUERY,
                "variables": {"_id": id},
            }))
            .send()
            .await?;
        if let Some(error) = status_error(response.status, origin) {
            return Err(error);
        }
        let body = response.text(MAX_PAGE).await?;
        let answer: Value = serde_json::from_str(&body)
            .map_err(|e| ResolveError::malformed(origin, format!("GraphQL JSON: {e}")))?;
        if let Some(message) = answer["errors"][0]["message"].as_str() {
            return Err(ResolveError::unavailable(origin, message.to_string()));
        }
        let bite = answer["data"]["web"]["biteById"].clone();
        if !bite.is_object() {
            return Err(ResolveError::NotFound(origin.clone()));
        }
        Ok(bite)
    }
}

#[async_trait]
impl Resolver for BlerpResolver {
    fn id(&self) -> &'static str {
        PLATFORM
    }

    fn platform(&self) -> Platform {
        Platform {
            id: PLATFORM,
            name: "Blerp",
            hosts: &["blerp.com"],
            features: &["sound bites", "audio"],
            formats: &["mp3"],
            session: SessionSupport::None,
            examples: &[
                "https://blerp.com/soundbites/6320fe8745636cb4dd677a5a",
                "https://blerp.com/soundbites/5bc94ef4796001000498429f",
            ],
        }
    }

    fn matches(&self, url: &Url) -> bool {
        bite_id(url).is_some()
    }

    async fn resolve(&self, url: &Url) -> Result<Resolution, ResolveError> {
        let id = bite_id(url).ok_or_else(|| ResolveError::NotFound(url.clone()))?;
        let bite = self.bite(&id, url).await?;
        if bite["isAudioExists"].as_bool() == Some(false) {
            return Err(ResolveError::unavailable(url, "the bite has no audio"));
        }
        let mp3 = util::url_of(&bite["audio"]["mp3"]["url"], None)
            .ok_or_else(|| ResolveError::malformed(url, "the bite names no mp3 file"))?;
        let mut variant = Variant::file(mp3);
        variant.container = Some(Container::Other("mp3".into()));
        variant.audio = Some(AudioCodec::Mp3);
        variant.audio_only = true;
        variant.format_id = Some("mp3".into());
        let bite_id = util::text(&bite["_id"]).unwrap_or_else(|| id.clone());
        let mut resolved = Resolved::new(PLATFORM);
        resolved.id = Some(bite_id.clone());
        resolved.title = bite["title"].as_str().and_then(clean_title);
        resolved.description = bite["description"].as_str().and_then(clean_title);
        resolved.uploader = bite["ownerObject"]["username"]
            .as_str()
            .and_then(clean_title);
        resolved.uploaded_at = util::time(&bite["createdAt"]);
        resolved.thumbnail = util::url_of(&bite["image"]["original"]["url"], None);
        resolved.webpage_url = Url::parse(&format!("https://blerp.com/soundbites/{bite_id}")).ok();
        resolved.variants = vec![variant];
        Ok(Resolution::from(resolved))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::{Exchange, Fixture, RecordedBody, RecordedRequest, RecordedResponse};

    fn post(url: &str, status: u16, body: String) -> Exchange {
        Exchange {
            request: RecordedRequest {
                method: "POST".into(),
                url: url.into(),
                headers: Vec::new(),
                body: None,
            },
            response: RecordedResponse {
                status,
                url: url.into(),
                headers: vec![("content-type".into(), "application/json".into())],
                body: RecordedBody::Text(body),
                truncated: false,
            },
        }
    }

    #[test]
    fn links_are_read() {
        let id = |s: &str| bite_id(&Url::parse(s).unwrap());
        assert_eq!(
            id("https://blerp.com/soundbites/6320fe8745636cb4dd677a5a"),
            Some("6320fe8745636cb4dd677a5a".into())
        );
        assert_eq!(
            id("https://www.blerp.com/soundbites/5bc94ef4796001000498429f?x=1"),
            Some("5bc94ef4796001000498429f".into())
        );
        assert_eq!(id("https://blerp.com/soundbites/"), None);
        assert_eq!(id("https://blerp.com/users/luminousaj"), None);
        assert_eq!(id("https://example.com/soundbites/abc"), None);
    }

    #[tokio::test]
    async fn bites_resolve_to_mp3() {
        let mut fixture = Fixture::new(PLATFORM, None);
        fixture.exchanges.push(post(GRAPHQL, 200, json!({"data": {"web": {"biteById": {
            "_id": "6320fe8745636cb4dd677a5a",
            "title": "Samsung Galaxy S8 Over the Horizon Ringtone 2016",
            "description": "  the ringtone  ",
            "isAudioExists": true,
            "createdAt": "2022-09-13T20:26:31.000Z",
            "image": {"original": {"url": "https://cdn.blerp.com/image/abc.png"}},
            "ownerObject": {"_id": "5fb81e51aa66ae000c395478", "username": "luminousaj"},
            "audio": {"mp3": {"url": "https://audio.blerp.com/audio/abc.mp3"}, "original": {"url": "https://audio.blerp.com/audio/abc.wav"}}
        }}}}).to_string()));
        let resolver = BlerpResolver::new(Http::replay(fixture));
        let url = Url::parse("https://blerp.com/soundbites/6320fe8745636cb4dd677a5a").unwrap();
        assert!(resolver.matches(&url));
        let resolved = resolver.resolve(&url).await.unwrap().media().unwrap();
        assert_eq!(resolved.id.as_deref(), Some("6320fe8745636cb4dd677a5a"));
        assert_eq!(
            resolved.title.as_deref(),
            Some("Samsung Galaxy S8 Over the Horizon Ringtone 2016")
        );
        assert_eq!(resolved.description.as_deref(), Some("the ringtone"));
        assert_eq!(resolved.uploader.as_deref(), Some("luminousaj"));
        assert!(resolved.uploaded_at.is_some());
        assert_eq!(
            resolved.thumbnail.as_ref().unwrap().as_str(),
            "https://cdn.blerp.com/image/abc.png"
        );
        assert_eq!(
            resolved.webpage_url.as_ref().unwrap().as_str(),
            "https://blerp.com/soundbites/6320fe8745636cb4dd677a5a"
        );
        assert_eq!(resolved.variants.len(), 1);
        let audio = &resolved.variants[0];
        assert_eq!(audio.url.as_str(), "https://audio.blerp.com/audio/abc.mp3");
        assert!(audio.audio_only);
        assert_eq!(audio.audio, Some(AudioCodec::Mp3));
        assert_eq!(audio.container, Some(Container::Other("mp3".into())));
    }

    #[tokio::test]
    async fn missing_bites_say_so() {
        let mut fixture = Fixture::new(PLATFORM, None);
        fixture.exchanges.push(post(
            GRAPHQL,
            200,
            json!({"data": {"web": {"biteById": null}}}).to_string(),
        ));
        fixture.exchanges.push(post(
            GRAPHQL,
            200,
            json!({"errors": [{"message": "Bite is private"}], "data": null}).to_string(),
        ));
        fixture.exchanges.push(post(
            GRAPHQL,
            200,
            json!({"data": {"web": {"biteById": {"_id": "c", "title": "silent", "isAudioExists": false}}}}).to_string(),
        ));
        fixture
            .exchanges
            .push(post(GRAPHQL, 429, "slow down".into()));
        let resolver = BlerpResolver::new(Http::replay(fixture));
        let resolve = |id: &str| {
            let url = Url::parse(&format!("https://blerp.com/soundbites/{id}")).unwrap();
            let resolver = &resolver;
            async move { resolver.resolve(&url).await.unwrap_err() }
        };
        assert!(matches!(resolve("a").await, ResolveError::NotFound(_)));
        let error = resolve("b").await;
        assert!(
            matches!(&error, ResolveError::Unavailable { reason, .. } if reason == "Bite is private"),
            "{error}"
        );
        let error = resolve("c").await;
        assert!(
            matches!(&error, ResolveError::Unavailable { reason, .. } if reason.contains("no audio")),
            "{error}"
        );
        assert!(matches!(resolve("d").await, ResolveError::RateLimited(_)));
    }
}
