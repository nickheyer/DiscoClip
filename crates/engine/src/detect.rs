use linkify::{LinkFinder, LinkKind};
use url::Url;

/// Finds http and https URLs in free text, in order of appearance, without duplicates.
pub fn find_urls(text: &str) -> Vec<Url> {
    let mut finder = LinkFinder::new();
    finder.kinds(&[LinkKind::Url]);
    finder.url_must_have_scheme(true);
    let mut seen = Vec::new();
    for link in finder.links(text) {
        let Ok(url) = Url::parse(link.as_str()) else {
            continue;
        };
        if !matches!(url.scheme(), "http" | "https") {
            continue;
        }
        if url.host_str().is_none() {
            continue;
        }
        if !seen.contains(&url) {
            seen.push(url);
        }
    }
    seen
}
