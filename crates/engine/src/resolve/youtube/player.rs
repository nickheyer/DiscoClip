//! The player script: found through the iframe API, fetched once per version, and mined
//! for its signature timestamp and the two functions that unlock format URLs, the
//! signature cipher and the throttling parameter, which run in the JavaScript
//! interpreter as the browser would run them.

use std::collections::HashMap;
use std::sync::{Arc, LazyLock, Mutex};

use regex::Regex;
use url::Url;

use super::innertube::{ORIGIN, PLATFORM};
use crate::http::{BROWSER_UA, Http};
use crate::js::{JsError, Script};
use crate::resolve::{MAX_PAGE, ResolveError, fetch_ok};

const IFRAME_API: &str = "https://www.youtube.com/iframe_api";
/// The player script can be several megabytes.
const MAX_SCRIPT: usize = 12 * 1024 * 1024;

#[derive(Debug, Clone, thiserror::Error)]
pub enum PlayerError {
    #[error("the player version is not in the iframe API")]
    NoVersion,
    #[error("the player script has no signature timestamp")]
    NoTimestamp,
    #[error("the player script's {0} function was not found")]
    NotFound(&'static str),
    #[error("the player script's {what} function {name} has no source")]
    NoSource { what: &'static str, name: String },
    #[error("the player script's {0} function failed: {1}")]
    Failed(&'static str, JsError),
}

/// One unlocking function, ready to run.
#[derive(Debug, Clone)]
struct Function {
    name: String,
    script: Script,
}

/// One version of the player script, mined.
#[derive(Debug)]
pub struct Player {
    pub version: String,
    pub sts: u64,
    signature: Option<Function>,
    throttle: Option<Function>,
    /// Throttling parameters already transformed: the same one comes with every format
    /// of an answer.
    transformed: Mutex<HashMap<String, String>>,
}

static RE_VERSION: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"player\\?/([0-9a-f]{8})\\?/").unwrap());
static RE_STS: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?:signatureTimestamp|sts)\s*:\s*(\d{5})").unwrap());

/// Where the signature cipher function is named in the script, one pattern per player
/// generation seen.
static RE_SIGNATURE: LazyLock<Vec<Regex>> = LazyLock::new(|| {
    [
        r#"\b[cs]\s*&&\s*[adf]\.set\([^,]+\s*,\s*encodeURIComponent\s*\(\s*([a-zA-Z0-9$]+)\("#,
        r#"\b[a-zA-Z0-9]+\s*&&\s*[a-zA-Z0-9]+\.set\([^,]+\s*,\s*encodeURIComponent\s*\(\s*([a-zA-Z0-9$]+)\("#,
        r#"\bm=([a-zA-Z0-9$]{2,})\(decodeURIComponent\(h\.s\)\)"#,
        r#"\bc&&\(c=([a-zA-Z0-9$]{2,})\(decodeURIComponent\(c\)\)"#,
        r#"(?:^|[^a-zA-Z0-9$])([a-zA-Z0-9$]{2,})\s*=\s*function\(\s*a\s*\)\s*\{\s*a\s*=\s*a\.split\(\s*""\s*\)"#,
        r#"(["'])signature["']\s*,\s*([a-zA-Z0-9$]+)\("#,
        r#"\.sig\|\|([a-zA-Z0-9$]+)\("#,
        r#"\b[cs]\s*&&\s*[adf]\.set\([^,]+\s*,\s*([a-zA-Z0-9$]+)\("#,
        r#"\b[a-zA-Z0-9]+\s*&&\s*[a-zA-Z0-9]+\.set\([^,]+\s*,\s*([a-zA-Z0-9$]+)\("#,
        r#"\bc\s*&&\s*[a-zA-Z0-9]+\.set\([^,]+\s*,\s*\([^)]*\)\s*\(\s*([a-zA-Z0-9$]+)\("#,
    ]
    .into_iter()
    .map(|pattern| Regex::new(pattern).expect("player patterns are valid"))
    .collect()
});

/// Where the throttling function is named: called on the `n` query parameter.
static RE_THROTTLE: LazyLock<Vec<Regex>> = LazyLock::new(|| {
    [
        r#"\.get\("n"\)\)&&\(b=([a-zA-Z0-9_$]+)(?:\[(\d+)\])?\([a-zA-Z0-9]\)"#,
        r#"b=String\.fromCharCode\(110\),c=a\.get\(b\)\)&&\(a=([a-zA-Z0-9_$]+)(?:\[(\d+)\])?\([a-zA-Z]\)"#,
        r#"[a-zA-Z0-9_$.]+\[\d+\]\s*&&\s*\(b=a\.get\(b\)\)\s*&&\s*\(a=([a-zA-Z0-9_$]+)(?:\[(\d+)\])?\([a-zA-Z]\)"#,
        r#"[a-zA-Z0-9_$.]+\[\d+\],c=a\.get\(b\)\)&&\(a=([a-zA-Z0-9_$]+)(?:\[(\d+)\])?\([a-zA-Z]\)"#,
    ]
    .into_iter()
    .map(|pattern| Regex::new(pattern).expect("player patterns are valid"))
    .collect()
});

/// An early return the throttling function makes when a global it expects is missing,
/// which is never the case in a browser and always the case in the interpreter.
static RE_THROTTLE_GUARD: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r#";\s*if\s*\(\s*typeof\s+[a-zA-Z0-9_$]+\s*===?\s*(?:["']undefined["']|[a-zA-Z0-9_$]+\[\d+\])\s*\)\s*return\s+[a-zA-Z0-9_$]+;"#,
    )
    .unwrap()
});

impl Player {
    /// Mines `script`, the player version `version`.
    pub fn parse(version: &str, script: &str) -> Result<Self, PlayerError> {
        let sts = RE_STS
            .captures(script)
            .and_then(|c| c[1].parse().ok())
            .ok_or(PlayerError::NoTimestamp)?;
        let globals = global_variable(script).unwrap_or_default();
        let signature = signature_name(script).map(|name| {
            let source = function_source(script, &name).ok_or_else(|| PlayerError::NoSource {
                what: "signature",
                name: name.clone(),
            })?;
            let helper = helper_object(script, &source).unwrap_or_default();
            Ok::<_, PlayerError>(Function {
                script: Script::new(format!("{globals}\n{helper}\nvar {name}={source};")),
                name,
            })
        });
        let throttle = throttle_name(script).map(|name| {
            let source = function_source(script, &name).ok_or_else(|| PlayerError::NoSource {
                what: "throttling",
                name: name.clone(),
            })?;
            let source = RE_THROTTLE_GUARD.replace_all(&source, ";").into_owned();
            Ok::<_, PlayerError>(Function {
                script: Script::new(format!("{globals}\nvar {name}={source};")),
                name,
            })
        });
        Ok(Self {
            version: version.to_string(),
            sts,
            signature: signature.transpose()?,
            throttle: throttle.transpose()?,
            transformed: Mutex::new(HashMap::new()),
        })
    }

    pub fn has_signature(&self) -> bool {
        self.signature.is_some()
    }

    pub fn has_throttle(&self) -> bool {
        self.throttle.is_some()
    }

    /// Runs the signature cipher on `s`.
    pub async fn decipher(&self, s: &str) -> Result<String, PlayerError> {
        let function = self
            .signature
            .as_ref()
            .ok_or(PlayerError::NotFound("signature"))?;
        function
            .script
            .call(&function.name, &[s.to_string()])
            .await
            .map_err(|e| PlayerError::Failed("signature", e))
    }

    /// Runs the throttling transform on `n`.
    pub async fn throttle(&self, n: &str) -> Result<String, PlayerError> {
        if let Some(done) = self
            .transformed
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(n)
        {
            return Ok(done.clone());
        }
        let function = self
            .throttle
            .as_ref()
            .ok_or(PlayerError::NotFound("throttling"))?;
        let out = function
            .script
            .call(&function.name, &[n.to_string()])
            .await
            .map_err(|e| PlayerError::Failed("throttling", e))?;
        self.transformed
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(n.to_string(), out.clone());
        Ok(out)
    }

    /// A format URL made playable: the signature cipher, when the format came with one,
    /// deciphered into it, and its throttling parameter transformed.
    pub async fn unlock(
        &self,
        url: Option<&str>,
        signature_cipher: Option<&str>,
    ) -> Result<Url, PlayerError> {
        let mut target = match (url, signature_cipher) {
            (_, Some(cipher)) => {
                let mut base: Option<Url> = None;
                let mut s: Option<String> = None;
                let mut sp = "signature".to_string();
                for (key, value) in url::form_urlencoded::parse(cipher.as_bytes()) {
                    match &*key {
                        "url" => base = Url::parse(&value).ok(),
                        "s" => s = Some(value.into_owned()),
                        "sp" => sp = value.into_owned(),
                        _ => {}
                    }
                }
                let mut base = base.ok_or(PlayerError::NotFound("signature"))?;
                let s = s.ok_or(PlayerError::NotFound("signature"))?;
                let signature = self.decipher(&s).await?;
                base.query_pairs_mut().append_pair(&sp, &signature);
                base
            }
            (Some(url), None) => Url::parse(url).map_err(|_| PlayerError::NotFound("signature"))?,
            (None, None) => return Err(PlayerError::NotFound("signature")),
        };
        let n = target
            .query_pairs()
            .find(|(k, _)| k == "n")
            .map(|(_, v)| v.into_owned());
        if let Some(n) = n
            && self.throttle.is_some()
        {
            let transformed = self.throttle(&n).await?;
            let pairs: Vec<(String, String)> = target
                .query_pairs()
                .map(|(k, v)| {
                    if k == "n" {
                        (k.into_owned(), transformed.clone())
                    } else {
                        (k.into_owned(), v.into_owned())
                    }
                })
                .collect();
            target.query_pairs_mut().clear().extend_pairs(pairs);
        }
        Ok(target)
    }
}

/// The name of the signature cipher function.
fn signature_name(script: &str) -> Option<String> {
    RE_SIGNATURE.iter().find_map(|re| {
        re.captures(script).and_then(|c| {
            c.iter()
                .skip(1)
                .flatten()
                .last()
                .map(|m| m.as_str().to_string())
        })
    })
}

/// The name of the throttling function, following an index into an array of functions
/// when it is named through one.
fn throttle_name(script: &str) -> Option<String> {
    for re in RE_THROTTLE.iter() {
        if let Some(captures) = re.captures(script) {
            let name = captures[1].to_string();
            let Some(index) = captures.get(2) else {
                return Some(name);
            };
            let index: usize = index.as_str().parse().ok()?;
            let list = Regex::new(&format!(
                r"var\s+{}\s*=\s*\[([^\]]+)\]",
                regex::escape(&name)
            ))
            .ok()?;
            let items = list.captures(script)?[1].to_string();
            return items
                .split(',')
                .nth(index)
                .map(|item| item.trim().to_string());
        }
    }
    None
}

/// The source of `function NAME(...) {...}` or `NAME = function(...) {...}`, braces
/// balanced, as a function expression.
pub fn function_source(script: &str, name: &str) -> Option<String> {
    let escaped = regex::escape(name);
    let patterns = [
        format!(
            r"(?:^|[^a-zA-Z0-9_$.])(?:var\s+|let\s+|const\s+)?{escaped}\s*=\s*function\s*\(([^)]*)\)\s*\{{"
        ),
        format!(r"function\s+{escaped}\s*\(([^)]*)\)\s*\{{"),
    ];
    for pattern in patterns {
        let re = Regex::new(&pattern).ok()?;
        if let Some(captures) = re.captures(script) {
            let whole = captures.get(0)?;
            let args = captures[1].to_string();
            let open = whole.end() - 1;
            let close = matching_brace(script, open)?;
            let body = &script[open..=close];
            return Some(format!("function({args}){body}"));
        }
    }
    None
}

/// The index of the `}` closing the `{` at `open`, stepping over strings, template
/// literals and comments.
pub fn matching_brace(text: &str, open: usize) -> Option<usize> {
    let bytes = text.as_bytes();
    if bytes.get(open) != Some(&b'{') {
        return None;
    }
    let mut depth = 0usize;
    let mut i = open;
    while i < bytes.len() {
        match bytes[i] {
            b'"' | b'\'' | b'`' => {
                let quote = bytes[i];
                i += 1;
                while i < bytes.len() && bytes[i] != quote {
                    if bytes[i] == b'\\' {
                        i += 1;
                    }
                    i += 1;
                }
            }
            b'/' if bytes.get(i + 1) == Some(&b'/') => {
                while i < bytes.len() && bytes[i] != b'\n' {
                    i += 1;
                }
            }
            b'/' if bytes.get(i + 1) == Some(&b'*') => {
                i += 2;
                while i + 1 < bytes.len() && !(bytes[i] == b'*' && bytes[i + 1] == b'/') {
                    i += 1;
                }
                i += 1;
            }
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(i);
                }
            }
            _ => {}
        }
        i += 1;
    }
    None
}

/// The `var NAME={...};` helper object a cipher function calls methods of.
fn helper_object(script: &str, function: &str) -> Option<String> {
    let call = Regex::new(r"([a-zA-Z0-9_$]{2,})\.[a-zA-Z0-9_$]+\(a,").ok()?;
    let name = call.captures(function)?[1].to_string();
    let definition = Regex::new(&format!(
        r"(?:^|[^a-zA-Z0-9_$.])var\s+{}\s*=\s*\{{",
        regex::escape(&name)
    ))
    .ok()?;
    let start = definition.find(script)?;
    let open = start.end() - 1;
    let close = matching_brace(script, open)?;
    Some(format!("var {name}={};", &script[open..=close]))
}

/// The array of strings the script declares right after `'use strict'`, which the
/// throttling function reads; as a statement, or nothing when the script has none.
pub fn global_variable(script: &str) -> Option<String> {
    let strict = script
        .find("'use strict';")
        .or_else(|| script.find("\"use strict\";"))?;
    let rest = &script[strict..];
    let rest = &rest[rest.find(';')? + 1..];
    let trimmed = rest.trim_start();
    let after_var = trimmed.strip_prefix("var ")?;
    let name_end =
        after_var.find(|c: char| !(c.is_ascii_alphanumeric() || c == '_' || c == '$'))?;
    let name = &after_var[..name_end];
    let after_name = after_var[name_end..].trim_start();
    let value = after_name.strip_prefix('=')?.trim_start();
    let value_end = if value.starts_with('"') || value.starts_with('\'') {
        let quote = value.as_bytes()[0];
        let mut i = 1;
        let bytes = value.as_bytes();
        while i < bytes.len() && bytes[i] != quote {
            if bytes[i] == b'\\' {
                i += 1;
            }
            i += 1;
        }
        let literal_end = i + 1;
        let tail = &value[literal_end..];
        let split = tail.strip_prefix(".split(")?;
        let close = split.find(')')?;
        literal_end + ".split(".len() + close + 1
    } else if value.starts_with('[') {
        let bytes = value.as_bytes();
        let mut i = 1;
        while i < bytes.len() && bytes[i] != b']' {
            if bytes[i] == b'"' || bytes[i] == b'\'' {
                let quote = bytes[i];
                i += 1;
                while i < bytes.len() && bytes[i] != quote {
                    if bytes[i] == b'\\' {
                        i += 1;
                    }
                    i += 1;
                }
            }
            i += 1;
        }
        i + 1
    } else {
        return None;
    };
    Some(format!("var {name}={};", &value[..value_end]))
}

/// The player script as it stands, fetched once per version.
pub struct PlayerCache {
    http: Http,
    cached: Mutex<Option<Arc<Player>>>,
}

impl PlayerCache {
    pub fn new(http: Http) -> Self {
        Self {
            http,
            cached: Mutex::new(None),
        }
    }

    /// The current player script, mined; fetched when its version changed.
    pub async fn get(&self, origin: &Url) -> Result<Arc<Player>, ResolveError> {
        let api = Url::parse(IFRAME_API).expect("valid");
        let iframe = fetch_ok(&self.http, &api, PLATFORM, BROWSER_UA, &[], MAX_PAGE).await?;
        let text = iframe.text();
        let version = RE_VERSION
            .captures(&text)
            .map(|c| c[1].to_string())
            .ok_or_else(|| ResolveError::malformed(origin, PlayerError::NoVersion.to_string()))?;
        if let Some(player) = self
            .cached
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .filter(|p| p.version == version)
        {
            return Ok(player.clone());
        }
        let script_url = Url::parse(&format!(
            "{ORIGIN}/s/player/{version}/player_ias.vflset/en_US/base.js"
        ))
        .expect("valid");
        let fetched = fetch_ok(
            &self.http,
            &script_url,
            PLATFORM,
            BROWSER_UA,
            &[],
            MAX_SCRIPT,
        )
        .await?;
        let player = Arc::new(
            Player::parse(&version, &fetched.text())
                .map_err(|e| ResolveError::malformed(origin, e.to_string()))?,
        );
        tracing::info!(
            version,
            sts = player.sts,
            signature = player.has_signature(),
            throttle = player.has_throttle(),
            "youtube player script mined"
        );
        *self.cached.lock().unwrap_or_else(|e| e.into_inner()) = Some(player.clone());
        Ok(player)
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A player script with the shapes the real one has: a helper object, a signature
    /// cipher, a throttling function guarded by a missing global and named through an
    /// array, and the string table both may read.
    pub const SCRIPT: &str = r#"var _yt_player={};(function(g){var window=this;
'use strict';var Xz="fromCharCode;split;join".split(";");
var Ya={reverse:function(a){a.reverse()},splice:function(a,b){a.splice(0,b)},swap:function(a,b){var c=a[0];a[0]=a[b%a.length];a[b%a.length]=c}};
Xa=function(a){a=a.split("");Ya.reverse(a,1);Ya.splice(a,2);Ya.swap(a,3);return a.join("")};
Wa=function(a){var b=a.split(""),c=[];if(typeof Qz==="undefined")return a;for(var d=0;d<b.length;d++)c.push(b[b.length-1-d]);return c.join("")+"_w8_"+Xz[1]};
var Za=[Wa];
g.foo=function(a,c){c&&(c=Xa(decodeURIComponent(c)));var b;c&&(b=a.get("n"))&&(b=Za[0](b),a.set("n",b))};
g.cfg={signatureTimestamp:19999,x:1};
})(_yt_player);"#;

    #[tokio::test]
    async fn the_script_is_mined_and_its_functions_run() {
        let player = Player::parse("abcdef12", SCRIPT).unwrap();
        assert_eq!(player.sts, 19999);
        assert!(player.has_signature());
        assert!(player.has_throttle());
        assert_eq!(player.decipher("abcdefgh").await.unwrap(), "cedfba");
        assert_eq!(player.throttle("xyz").await.unwrap(), "zyx_w8_split");
        assert_eq!(player.throttle("xyz").await.unwrap(), "zyx_w8_split");

        let unlocked = player
            .unlock(
                None,
                Some("s=abcdefgh&sp=sig&url=https%3A%2F%2Frr1.googlevideo.com%2Fvideoplayback%3Fid%3D2%26n%3Dxyz"),
            )
            .await
            .unwrap();
        assert_eq!(unlocked.host_str(), Some("rr1.googlevideo.com"));
        let query: Vec<(String, String)> = unlocked.query_pairs().into_owned().collect();
        assert!(
            query.contains(&("sig".into(), "cedfba".into())),
            "{unlocked}"
        );
        assert!(
            query.contains(&("n".into(), "zyx_w8_split".into())),
            "{unlocked}"
        );
        let plain = player
            .unlock(
                Some("https://rr1.googlevideo.com/videoplayback?id=3&n=xyz"),
                None,
            )
            .await
            .unwrap();
        assert!(plain.as_str().contains("n=zyx_w8_split"), "{plain}");
    }

    #[test]
    fn pieces_are_found_or_missed_by_name() {
        assert_eq!(
            global_variable(SCRIPT).unwrap(),
            r#"var Xz="fromCharCode;split;join".split(";");"#
        );
        assert_eq!(
            global_variable("'use strict';var Q=[\"a\",'b,c'];x()").unwrap(),
            "var Q=[\"a\",'b,c'];"
        );
        assert!(global_variable("var a=1;").is_none());
        assert_eq!(signature_name(SCRIPT).as_deref(), Some("Xa"));
        assert_eq!(throttle_name(SCRIPT).as_deref(), Some("Wa"));
        assert!(function_source(SCRIPT, "Nope").is_none());
        assert_eq!(matching_brace("{a{b}\"}\"}", 0), Some(8));
        assert!(matches!(
            Player::parse("v", "nothing here"),
            Err(PlayerError::NoTimestamp)
        ));
        let no_functions = Player::parse("v", "sts:12345;").unwrap();
        assert!(!no_functions.has_signature());
        assert!(!no_functions.has_throttle());
    }
}
