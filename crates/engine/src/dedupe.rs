//! Telling media already fetched for an earlier job from new media.

use std::path::Path;

use sha2::{Digest, Sha256};
use url::Url;

use crate::resolve::Resolved;

/// Query parameters that track the visitor and never change what the link plays
const TRACKING: [&str; 13] = [
    "fbclid", "gclid", "igsh", "igshid", "si", "feature", "ref", "ref_src", "ref_url", "mc_cid",
    "mc_eid", "_ga", "share_id",
];

fn is_tracking(key: &str) -> bool {
    key.starts_with("utm_") || TRACKING.contains(&key)
}

/// The form a link is compared in, with what makes no difference to the media stripped
pub fn url_key(url: &Url) -> String {
    let scheme = url.scheme().to_ascii_lowercase();
    let host = url.host_str().unwrap_or("").to_ascii_lowercase();
    let host = ["www.", "m.", "mobile."]
        .iter()
        .find_map(|prefix| host.strip_prefix(prefix))
        .unwrap_or(&host);
    let port = url.port().map(|p| format!(":{p}")).unwrap_or_default();
    let path = url.path().trim_end_matches('/');
    let path = if path.is_empty() { "/" } else { path };
    let mut pairs: Vec<(String, String)> = url
        .query_pairs()
        .filter(|(key, _)| !is_tracking(key))
        .map(|(key, value)| (key.into_owned(), value.into_owned()))
        .collect();
    pairs.sort();
    let query = if pairs.is_empty() {
        String::new()
    } else {
        let joined: Vec<String> = pairs
            .iter()
            .map(|(key, value)| format!("{key}={value}"))
            .collect();
        format!("?{}", joined.join("&"))
    };
    format!("{scheme}://{host}{port}{path}{query}")
}

/// The platform's own name for the media, when it gave one
pub fn media_key(resolved: &Resolved) -> Option<String> {
    resolved
        .id
        .as_ref()
        .filter(|id| !id.is_empty())
        .map(|id| format!("{}:{id}", resolved.resolver))
}

/// Hex SHA-256 of a file, read in pieces off the async threads
pub async fn sha256_file(path: &Path) -> std::io::Result<String> {
    let path = path.to_path_buf();
    tokio::task::spawn_blocking(move || {
        use std::io::Read;
        let mut file = std::fs::File::open(path)?;
        let mut hasher = Sha256::new();
        let mut buffer = vec![0u8; 1024 * 1024];
        loop {
            let read = file.read(&mut buffer)?;
            if read == 0 {
                break;
            }
            hasher.update(&buffer[..read]);
        }
        Ok(hex::encode(hasher.finalize()))
    })
    .await
    .map_err(|e| std::io::Error::other(e.to_string()))?
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(url: &str) -> String {
        url_key(&Url::parse(url).unwrap())
    }

    #[test]
    fn links_to_the_same_media_share_a_key() {
        let plain = key("https://youtube.com/watch?v=abc");
        for same in [
            "https://www.youtube.com/watch?v=abc&feature=share",
            "https://m.youtube.com/watch?v=abc",
            "HTTPS://YouTube.com/watch?v=abc#t=20",
            "https://youtube.com:443/watch?utm_source=x&v=abc&si=track",
            "https://youtube.com/watch/?v=abc",
        ] {
            assert_eq!(key(same), plain, "{same}");
        }
        assert_ne!(key("https://youtube.com/watch?v=abd"), plain);
        assert_ne!(key("https://youtube.com/watch?v=abc&t=30"), plain);
        assert_eq!(key("https://a.test/"), "https://a.test/");
        assert_eq!(key("https://a.test:8443/x/"), "https://a.test:8443/x");
        assert_eq!(key("https://a.test/?b=2&a=1"), "https://a.test/?a=1&b=2");
    }

    #[test]
    fn media_keys_name_the_platform_and_its_id() {
        let mut resolved = Resolved::new("youtube");
        assert_eq!(media_key(&resolved), None);
        resolved.id = Some("abc".into());
        assert_eq!(media_key(&resolved).as_deref(), Some("youtube:abc"));
        resolved.id = Some(String::new());
        assert_eq!(media_key(&resolved), None);
    }

    #[tokio::test]
    async fn files_hash_to_sha256() {
        let dir = std::env::temp_dir().join(format!("discoclip-hash-{}", uuid::Uuid::now_v7()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("abc.bin");
        std::fs::write(&path, b"abc").unwrap();
        assert_eq!(
            sha256_file(&path).await.unwrap(),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
