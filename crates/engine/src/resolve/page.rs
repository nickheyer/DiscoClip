//! Reading HTML the way resolvers need to: metadata tags, JSON-LD, the JSON blobs pages
//! hand their scripts, players in iframes, and plain `<video>` sources.

use std::collections::HashSet;

use scraper::{Html, Selector};
use serde_json::Value;
use url::Url;

use super::clean_title;

pub struct Page {
    url: Url,
    html: String,
    document: Html,
}

fn selector(text: &str) -> Selector {
    Selector::parse(text).expect("selectors in this module are valid")
}

impl Page {
    pub fn parse(html: &str, url: &Url) -> Self {
        Self {
            url: url.clone(),
            html: html.to_string(),
            document: Html::parse_document(html),
        }
    }

    pub fn url(&self) -> &Url {
        &self.url
    }

    pub fn html(&self) -> &str {
        &self.html
    }

    pub fn document(&self) -> &Html {
        &self.document
    }

    /// The `content` of the first `<meta>` whose `property`, `name` or `itemprop` is `key`.
    pub fn meta(&self, key: &str) -> Option<String> {
        self.meta_all(key).into_iter().next()
    }

    pub fn meta_all(&self, key: &str) -> Vec<String> {
        let wanted = key.to_ascii_lowercase();
        self.document
            .select(&selector("meta"))
            .filter(|element| {
                ["property", "name", "itemprop"].iter().any(|attribute| {
                    element
                        .value()
                        .attr(attribute)
                        .is_some_and(|v| v.eq_ignore_ascii_case(&wanted))
                })
            })
            .filter_map(|element| element.value().attr("content").map(str::trim))
            .filter(|content| !content.is_empty())
            .map(str::to_string)
            .collect()
    }

    /// `og:title`, else `twitter:title`, else `<title>`.
    pub fn title(&self) -> Option<String> {
        self.meta("og:title")
            .or_else(|| self.meta("twitter:title"))
            .and_then(|t| clean_title(&t))
            .or_else(|| {
                self.document
                    .select(&selector("title"))
                    .next()
                    .and_then(|t| clean_title(&t.text().collect::<String>()))
            })
    }

    pub fn canonical(&self) -> Option<Url> {
        self.document
            .select(&selector("link[rel='canonical']"))
            .next()
            .and_then(|l| l.value().attr("href"))
            .and_then(|href| self.url.join(href).ok())
            .or_else(|| self.meta("og:url").and_then(|u| self.url.join(&u).ok()))
    }

    /// Every JSON-LD block, parsed.
    pub fn ld_json(&self) -> Vec<Value> {
        self.document
            .select(&selector("script[type='application/ld+json']"))
            .filter_map(|element| serde_json::from_str(&element.text().collect::<String>()).ok())
            .collect()
    }

    /// The bodies of inline scripts, in document order.
    pub fn scripts(&self) -> Vec<String> {
        self.document
            .select(&selector("script"))
            .filter(|s| s.value().attr("src").is_none())
            .map(|s| s.text().collect::<String>())
            .filter(|s| !s.trim().is_empty())
            .collect()
    }

    /// The JSON object or array that follows the first occurrence of `prefix` in any
    /// inline script, such as `window.__INITIAL_STATE__=`.
    pub fn script_json(&self, prefix: &str) -> Option<Value> {
        json_after(&self.html, prefix)
    }

    /// Every JSON object following `prefix`, in document order.
    pub fn script_json_all(&self, prefix: &str) -> Vec<Value> {
        let mut found = Vec::new();
        let mut rest = self.html.as_str();
        while let Some(index) = rest.find(prefix) {
            let after = &rest[index + prefix.len()..];
            if let Some((value, consumed)) = leading_json(after) {
                found.push(value);
                rest = &after[consumed..];
            } else {
                rest = &after[after.len().min(1)..];
            }
        }
        found
    }

    /// `<iframe src>` and `<embed src>` targets, resolved against the page.
    pub fn iframes(&self) -> Vec<Url> {
        let mut seen = HashSet::new();
        self.document
            .select(&selector("iframe[src], iframe[data-src], embed[src]"))
            .filter_map(|element| {
                let src = element
                    .value()
                    .attr("src")
                    .filter(|s| !s.trim().is_empty() && !s.starts_with("about:"))
                    .or_else(|| element.value().attr("data-src"))?;
                self.url.join(src.trim()).ok()
            })
            .filter(|url| matches!(url.scheme(), "http" | "https") && seen.insert(url.to_string()))
            .collect()
    }

    /// `<video src>`, `<video><source src>` and `<source type="video/…">` targets.
    pub fn video_sources(&self) -> Vec<Url> {
        let mut seen = HashSet::new();
        self.document
            .select(&selector(
                "video[src], video source[src], source[type^='video/'][src], source[type*='mpegurl'][src], source[type*='dash'][src]",
            ))
            .filter_map(|element| element.value().attr("src"))
            .filter_map(|src| self.url.join(src.trim()).ok())
            .filter(|url| matches!(url.scheme(), "http" | "https") && seen.insert(url.to_string()))
            .collect()
    }

    /// `<video poster>` or `og:image`.
    pub fn poster(&self) -> Option<Url> {
        self.document
            .select(&selector("video[poster]"))
            .next()
            .and_then(|v| v.value().attr("poster"))
            .and_then(|p| self.url.join(p).ok())
            .or_else(|| self.meta("og:image").and_then(|u| self.url.join(&u).ok()))
    }

    /// The `href` of every `<a>` whose target host ends with one of `hosts`.
    pub fn links_to(&self, hosts: &[&str]) -> Vec<Url> {
        let mut seen = HashSet::new();
        self.document
            .select(&selector("a[href]"))
            .filter_map(|a| a.value().attr("href"))
            .filter_map(|href| self.url.join(href).ok())
            .filter(|url| {
                url.host_str().is_some_and(|host| {
                    hosts
                        .iter()
                        .any(|h| host == *h || host.ends_with(&format!(".{h}")))
                }) && seen.insert(url.to_string())
            })
            .collect()
    }

    /// Text between `start` and `end`, first occurrence.
    pub fn between(&self, start: &str, end: &str) -> Option<&str> {
        between(&self.html, start, end)
    }
}

pub fn between<'a>(text: &'a str, start: &str, end: &str) -> Option<&'a str> {
    let from = text.find(start)? + start.len();
    let to = text[from..].find(end)? + from;
    Some(&text[from..to])
}

/// The JSON value that follows `prefix` in `text`.
pub fn json_after(text: &str, prefix: &str) -> Option<Value> {
    let index = text.find(prefix)?;
    leading_json(&text[index + prefix.len()..]).map(|(value, _)| value)
}

/// Parses the JSON object or array that `text` starts with (after whitespace), returning
/// it and how much of `text` it took. Strings, escapes and nesting are respected, and a
/// trailing `;` or HTML is left alone.
pub fn leading_json(text: &str) -> Option<(Value, usize)> {
    let offset = text.len() - text.trim_start().len();
    let body = &text[offset..];
    let end = balanced_end(body)?;
    let candidate = &body[..end];
    serde_json::from_str(candidate)
        .ok()
        .map(|value| (value, offset + end))
}

/// The byte length of the balanced `{…}` or `[…]` that `text` begins with.
pub fn balanced_end(text: &str) -> Option<usize> {
    let bytes = text.as_bytes();
    let open = *bytes.first()?;
    let close = match open {
        b'{' => b'}',
        b'[' => b']',
        _ => return None,
    };
    let mut depth = 0i32;
    let mut in_string = false;
    let mut escaped = false;
    for (index, &byte) in bytes.iter().enumerate() {
        if in_string {
            if escaped {
                escaped = false;
            } else if byte == b'\\' {
                escaped = true;
            } else if byte == b'"' {
                in_string = false;
            }
            continue;
        }
        match byte {
            b'"' => in_string = true,
            b'{' | b'[' => depth += 1,
            b'}' | b']' => {
                depth -= 1;
                if depth == 0 {
                    return (byte == close).then_some(index + 1);
                }
            }
            _ => {}
        }
    }
    None
}

/// Unescapes the `\/`, `\"`, `<` sequences of a JSON string literal body.
pub fn unescape_json_string(text: &str) -> String {
    serde_json::from_str::<String>(&format!("\"{text}\"")).unwrap_or_else(|_| text.to_string())
}

/// Every value at `pointer` in a tree of JSON-LD objects, searching arrays and `@graph`.
pub fn ld_objects_of_type<'a>(values: &'a [Value], type_name: &str) -> Vec<&'a Value> {
    let mut found = Vec::new();
    fn walk<'a>(value: &'a Value, type_name: &str, found: &mut Vec<&'a Value>) {
        match value {
            Value::Array(items) => items.iter().for_each(|i| walk(i, type_name, found)),
            Value::Object(map) => {
                let is_type = map.get("@type").is_some_and(|t| match t {
                    Value::String(s) => s == type_name,
                    Value::Array(list) => list.iter().any(|s| s.as_str() == Some(type_name)),
                    _ => false,
                });
                if is_type {
                    found.push(value);
                }
                for (key, child) in map {
                    if key != "@context" {
                        walk(child, type_name, found);
                    }
                }
            }
            _ => {}
        }
    }
    values.iter().for_each(|v| walk(v, type_name, &mut found));
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    const HTML: &str = r#"<html><head><title>Fallback title</title>
      <meta property="og:title" content=" A video ">
      <meta name="twitter:player:stream" content="/media/clip.mp4">
      <link rel="canonical" href="https://site.test/watch/1">
      <script type="application/ld+json">{"@context":"https://schema.org","@graph":[{"@type":"VideoObject","contentUrl":"https://cdn.test/v.mp4","name":"LD name"}]}</script>
      <script>window.__STATE__ = {"a": {"b": [1, 2, "}"]}, "s": "x\"y"};</script>
      <script>var other = [1,2];</script>
      </head><body>
      <video poster="/p.jpg"><source src="//cdn.test/a.m3u8" type="application/x-mpegURL"></video>
      <iframe src="https://www.youtube.com/embed/abc"></iframe>
      <a href="https://twitter.com/x/status/1">tweet</a>
      </body></html>"#;

    #[test]
    fn reads_meta_title_canonical_and_ld() {
        let page = Page::parse(HTML, &Url::parse("https://site.test/watch/1?x=1").unwrap());
        assert_eq!(page.title().as_deref(), Some("A video"));
        assert_eq!(
            page.meta("twitter:player:stream").as_deref(),
            Some("/media/clip.mp4")
        );
        assert_eq!(page.meta("missing"), None);
        assert_eq!(
            page.canonical().unwrap().as_str(),
            "https://site.test/watch/1"
        );
        let ld = page.ld_json();
        let videos = ld_objects_of_type(&ld, "VideoObject");
        assert_eq!(videos.len(), 1);
        assert_eq!(videos[0]["contentUrl"], "https://cdn.test/v.mp4");
        assert_eq!(page.poster().unwrap().as_str(), "https://site.test/p.jpg");
    }

    #[test]
    fn extracts_script_json_sources_iframes_and_links() {
        let page = Page::parse(HTML, &Url::parse("https://site.test/watch/1").unwrap());
        let state = page.script_json("window.__STATE__ =").unwrap();
        assert_eq!(state["a"]["b"][2], "}");
        assert_eq!(state["s"], "x\"y");
        assert_eq!(page.script_json_all("var other =").len(), 1);
        assert!(page.script_json("nothing =").is_none());
        assert_eq!(
            page.video_sources()
                .iter()
                .map(|u| u.as_str())
                .collect::<Vec<_>>(),
            vec!["https://cdn.test/a.m3u8"]
        );
        assert_eq!(
            page.iframes()[0].as_str(),
            "https://www.youtube.com/embed/abc"
        );
        assert_eq!(page.links_to(&["twitter.com"]).len(), 1);
        assert!(page.links_to(&["reddit.com"]).is_empty());
        assert_eq!(page.scripts().len(), 3);
    }

    #[test]
    fn balanced_json_handles_strings_and_nesting() {
        assert_eq!(balanced_end(r#"{"a":"}"}rest"#), Some(9));
        assert_eq!(balanced_end(r#"[1,[2,3]] "#), Some(9));
        assert_eq!(balanced_end(r#"{"a":"\"}"}"#), Some(11));
        assert_eq!(balanced_end("{unclosed"), None);
        assert_eq!(balanced_end("x"), None);
        let (value, used) = leading_json("  {\"k\": 1};").unwrap();
        assert_eq!(value["k"], 1);
        assert_eq!(used, 10);
        assert_eq!(unescape_json_string(r#"a\/b<c"#), "a/b<c");
        assert_eq!(between("pre[x]post", "[", "]"), Some("x"));
    }
}
