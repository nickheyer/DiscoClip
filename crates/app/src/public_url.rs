//! The address browsers reach the app at. Login providers send browsers back under it,
//! and the pages the bots post instead of a file are under it. It is `web.public_url`
//! when that is set, else the origin the operators' own requests arrive at, which the
//! server learns from the browser's own view of it and keeps across restarts.

use std::sync::RwLock;

use discoclip_engine::StoreError;
use discoclip_engine::rusqlite::{OptionalExtension, params};
use discoclip_engine::store::sqlite::SqliteStore;
use jiff::Timestamp;
use serde::Serialize;
use url::{Host, Url};

use crate::db::{nanos, transact};

const KEY: &str = "public_url";

/// Where the address in force came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PublicUrlSource {
    /// `web.public_url` in the settings.
    Configured,
    /// The origin the operators' requests arrive at.
    Learned,
}

pub struct PublicUrl {
    configured: RwLock<Option<Url>>,
    learned: RwLock<Option<Url>>,
    db: SqliteStore,
}

impl PublicUrl {
    /// Over a database the application's migrations have been applied to. `load` reads
    /// what was learned before.
    pub fn new(configured: Option<Url>, db: SqliteStore) -> Self {
        Self {
            configured: RwLock::new(configured),
            learned: RwLock::new(None),
            db,
        }
    }

    /// Reads the origin learned in earlier runs.
    pub async fn load(&self) -> Result<(), StoreError> {
        let stored: Option<String> = transact(&self.db, |tx| {
            Ok::<_, StoreError>(
                tx.query_row(
                    "SELECT value FROM remembered WHERE key = ?1",
                    params![KEY],
                    |row| row.get(0),
                )
                .optional()?,
            )
        })
        .await?;
        let learned = stored
            .as_deref()
            .map(Url::parse)
            .transpose()
            .map_err(|e| StoreError::Corrupt(format!("remembered.{KEY}: {e}")))?;
        *self.learned.write().unwrap_or_else(|e| e.into_inner()) = learned;
        Ok(())
    }

    /// The address in force: the configured one, else the learned one.
    pub fn get(&self) -> Option<Url> {
        self.configured().or_else(|| self.learned())
    }

    pub fn source(&self) -> Option<PublicUrlSource> {
        if self.configured().is_some() {
            Some(PublicUrlSource::Configured)
        } else if self.learned().is_some() {
            Some(PublicUrlSource::Learned)
        } else {
            None
        }
    }

    pub fn configured(&self) -> Option<Url> {
        self.configured
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    pub fn learned(&self) -> Option<Url> {
        self.learned
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// Takes `web.public_url` as it now stands.
    pub fn set_configured(&self, url: Option<Url>) {
        *self.configured.write().unwrap_or_else(|e| e.into_inner()) = url;
    }

    /// Takes `origin`, where an operator's request came from, as the learned address when
    /// it is at least as good as what is known: an HTTPS address with a name beats one
    /// with a bare address, which beats plain HTTP, which beats localhost. Between equals
    /// the newest wins, so a move to a new name follows along. Whether anything changed.
    pub async fn learn(&self, origin: &str) -> Result<bool, StoreError> {
        let Some(candidate) = parse_origin(origin) else {
            return Ok(false);
        };
        {
            let current = self.learned.read().unwrap_or_else(|e| e.into_inner());
            if let Some(current) = current.as_ref() {
                if *current == candidate || rank(&candidate) < rank(current) {
                    return Ok(false);
                }
            }
        }
        *self.learned.write().unwrap_or_else(|e| e.into_inner()) = Some(candidate.clone());
        let value = candidate.to_string();
        transact(&self.db, move |tx| {
            tx.execute(
                "INSERT INTO remembered (key, value, updated_at) VALUES (?1, ?2, ?3)
                 ON CONFLICT(key) DO UPDATE SET value = excluded.value, updated_at = excluded.updated_at",
                params![KEY, value, nanos(Timestamp::now())],
            )?;
            Ok::<_, StoreError>(())
        })
        .await?;
        tracing::info!(public_url = %candidate, "public address learned from a request");
        Ok(true)
    }
}

impl discoclip_bot::OwnLinks for PublicUrl {
    /// Under the configured or learned address, matching the host and the port when one is named
    fn is_own(&self, url: &Url) -> bool {
        [self.configured(), self.learned()]
            .into_iter()
            .flatten()
            .any(|own| {
                own.host_str() == url.host_str()
                    && (own.port().is_none() || own.port() == url.port())
            })
    }
}

/// `scheme://host[:port]` as a URL with a path of `/`, when it is one the app can be
/// reached at.
fn parse_origin(origin: &str) -> Option<Url> {
    let mut url = Url::parse(origin).ok()?;
    if !matches!(url.scheme(), "http" | "https") || url.host().is_none() {
        return None;
    }
    if url.path() != "/" || url.query().is_some() || url.fragment().is_some() {
        url.set_path("/");
        url.set_query(None);
        url.set_fragment(None);
    }
    Some(url)
}

/// How good an address is for people elsewhere to reach: 3 for HTTPS under a name, 2 for
/// HTTPS at an address, 1 for HTTP under a name, 0 for HTTP at an address or localhost.
fn rank(url: &Url) -> u8 {
    let named = match url.host() {
        Some(Host::Domain(domain)) => domain != "localhost" && !domain.ends_with(".localhost"),
        Some(Host::Ipv4(_)) | Some(Host::Ipv6(_)) | None => false,
    };
    match (url.scheme() == "https", named) {
        (true, true) => 3,
        (true, false) => 2,
        (false, true) => 1,
        (false, false) => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn fresh() -> PublicUrl {
        let db = SqliteStore::open_in_memory().await.unwrap();
        crate::migrations::apply(&db).await.unwrap();
        let url = PublicUrl::new(None, db);
        url.load().await.unwrap();
        url
    }

    #[tokio::test]
    async fn better_addresses_replace_worse_ones_and_equals_follow_the_newest() {
        let url = fresh().await;
        assert_eq!(url.get(), None);
        assert!(url.learn("http://localhost:8080").await.unwrap());
        assert_eq!(url.get().unwrap().as_str(), "http://localhost:8080/");
        assert_eq!(url.source(), Some(PublicUrlSource::Learned));
        assert!(url.learn("https://clips.example").await.unwrap());
        assert!(!url.learn("http://localhost:8080").await.unwrap());
        assert!(!url.learn("http://clips.example").await.unwrap());
        assert!(!url.learn("https://10.0.0.5").await.unwrap());
        assert!(
            !url.learn("https://clips.example").await.unwrap(),
            "unchanged"
        );
        assert!(url.learn("https://new.example").await.unwrap());
        assert_eq!(url.get().unwrap().as_str(), "https://new.example/");
        assert!(!url.learn("ftp://files.example").await.unwrap());
        assert!(!url.learn("not a url").await.unwrap());
        // A path or query on the origin is dropped.
        assert!(
            url.learn("https://other.example/some/page?x=1")
                .await
                .unwrap()
        );
        assert_eq!(url.get().unwrap().as_str(), "https://other.example/");
    }

    #[tokio::test]
    async fn links_under_the_apps_addresses_are_its_own() {
        use discoclip_bot::OwnLinks;
        let url = fresh().await;
        let link = |s: &str| Url::parse(s).unwrap();
        assert!(!url.is_own(&link("https://clips.example/f/clips/j/1")));
        url.learn("http://localhost:8080").await.unwrap();
        assert!(url.is_own(&link("http://localhost:8080/f/clips/j/1")));
        assert!(!url.is_own(&link("http://localhost:3000/f/clips/j/1")));
        url.set_configured(Some(link("https://clips.example")));
        assert!(url.is_own(&link("https://clips.example/f/clips/j/1")));
        assert!(url.is_own(&link("http://clips.example/f/clips/j/1")));
        assert!(
            url.is_own(&link("http://localhost:8080/")),
            "the learned address counts beside the configured one"
        );
        assert!(!url.is_own(&link("https://clips.example.net/")));
    }

    #[tokio::test]
    async fn the_learned_address_survives_a_restart_and_yields_to_the_configured_one() {
        let db = SqliteStore::open_in_memory().await.unwrap();
        crate::migrations::apply(&db).await.unwrap();
        let first = PublicUrl::new(None, db.clone());
        first.load().await.unwrap();
        first.learn("https://clips.example").await.unwrap();
        let second = PublicUrl::new(None, db);
        assert_eq!(second.get(), None, "nothing until loaded");
        second.load().await.unwrap();
        assert_eq!(second.get().unwrap().as_str(), "https://clips.example/");
        second.set_configured(Some(Url::parse("https://set.example").unwrap()));
        assert_eq!(second.get().unwrap().as_str(), "https://set.example/");
        assert_eq!(second.source(), Some(PublicUrlSource::Configured));
        assert_eq!(second.learned().unwrap().as_str(), "https://clips.example/");
        second.set_configured(None);
        assert_eq!(second.source(), Some(PublicUrlSource::Learned));
    }
}
