//! The player script: found through the iframe API, fetched once per version, and run
//! whole in the JavaScript interpreter, where the player's own URL builder applies the
//! two transforms that unlock format URLs, the signature cipher and the throttling
//! transform, exactly as it would in a browser. The script is several megabytes of
//! obfuscated code whose transforms can no longer be cut out of it, so it stays loaded on
//! a thread of its own between calls and is let go after a while without any.

use std::collections::HashMap;
use std::sync::{Arc, LazyLock, Mutex};
use std::time::Duration;

use regex::Regex;
use tokio::sync::Mutex as AsyncMutex;
use url::Url;

use super::innertube::{ORIGIN, PLATFORM};
use crate::http::{BROWSER_UA, Http};
use crate::js::{JsError, Resident, literal};
use crate::resolve::{MAX_PAGE, ResolveError, fetch_ok};

const IFRAME_API: &str = "https://www.youtube.com/iframe_api";
/// The player script can be several megabytes.
const MAX_SCRIPT: usize = 12 * 1024 * 1024;
/// How long the loaded script is kept after its last call.
const IDLE: Duration = Duration::from_secs(10 * 60);

#[derive(Debug, Clone, thiserror::Error)]
pub enum PlayerError {
    #[error("the player version is not in the iframe API")]
    NoVersion,
    #[error("the player script has no signature timestamp")]
    NoTimestamp,
    #[error("the player script has no URL builder")]
    NoBuilder,
    #[error("the player script does not end in the call that runs it")]
    NoClosing,
    #[error("the player script did not load: {0}")]
    Load(JsError),
    #[error("the player script's {0} transform failed: {1}")]
    Failed(&'static str, JsError),
    #[error("the player script's {0} transform handed back nothing")]
    Empty(&'static str),
    #[error("the player script's throttling transform threw and handed back {0}")]
    Untransformed(String),
    #[error("the format names no URL")]
    NoUrl,
    #[error("the signature cipher carries no signature")]
    NoSignature,
}

static RE_VERSION: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"player\\?/([0-9a-f]{8})\\?/").unwrap());
static RE_STS: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?:signatureTimestamp|sts)\s*:\s*(\d{5})").unwrap());

/// The function that builds a format URL, named as `function NAME(` or `NAME = function(`
/// (with `NAME` possibly a member such as `g.NAME`): the one with a statement of its own
/// that marks the URL with `.set("alr","yes")` before applying the transforms.
static RE_BUILDER: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r#"(?:^|[;{}\s])(?:function\s+([A-Za-z0-9_$]+)\s*\(|((?:[A-Za-z0-9_$]+\.)*[A-Za-z0-9_$]+)\s*=\s*function\s*\()[^)]*\)\s*\{(?:[^{}]*;)?\s*[A-Za-z0-9_$]+(?:\.[A-Za-z0-9_$]+)*\.set\("alr","yes"\)"#,
    )
    .unwrap()
});

/// How the script's enclosing function is called at its end: the web player passes the
/// `_yt_player` object, the TV player calls itself on `this`.
const CLOSINGS: [&str; 2] = ["})(_yt_player);", "}).call(this);"];

/// What the script expects of a browser before it runs: the objects it reads at load,
/// each doing nothing, as yt-dlp's solver provides them.
const SHIMS: &str = r#"var _result={n:null,sig:null};
if (typeof globalThis.XMLHttpRequest === "undefined") { globalThis.XMLHttpRequest = { prototype: {} }; }
if (typeof URL === "undefined") { globalThis.location = { hash: "", host: "www.youtube.com", hostname: "www.youtube.com", href: "https://www.youtube.com/watch?v=yt-dlp-wins", origin: "https://www.youtube.com", password: "", pathname: "/watch", port: "", protocol: "https:", search: "?v=yt-dlp-wins", username: "" }; } else { globalThis.location = new URL("https://www.youtube.com/watch?v=yt-dlp-wins"); }
if (typeof globalThis.document === "undefined") { globalThis.document = Object.create(null); }
if (typeof globalThis.navigator === "undefined") { globalThis.navigator = Object.create(null); }
if (typeof globalThis.self === "undefined") { globalThis.self = globalThis; }
if (typeof globalThis.window === "undefined") { globalThis.window = globalThis; }
if (typeof globalThis.Intl === "undefined") { globalThis.Intl = { NumberFormat: { supportedLocalesOf: function () { return []; } }, DateTimeFormat: { supportedLocalesOf: function () { return []; } } }; }
"#;

/// The name of the URL builder function in `script`.
fn builder_name(script: &str) -> Option<String> {
    RE_BUILDER.captures(script).and_then(|c| {
        c.get(1)
            .or_else(|| c.get(2))
            .map(|m| m.as_str().to_string())
    })
}

/// The solver, placed inside the script's enclosing function where `builder` is in
/// scope: builds a URL with the signature, sets the throttling parameter, then runs the
/// URL's serializer, the one method of its class that is not `set`, `get` or `clone`,
/// which is where the player transforms the parameter.
fn solver(builder: &str) -> String {
    format!(
        r#"
var _solve = function (sig, n) {{
  var url = {builder}("https://youtube.com/watch?v=yt-dlp-wins", "s", sig === undefined ? undefined : encodeURIComponent(sig));
  url.set("n", n);
  var proto = Object.getPrototypeOf(url);
  var keys = Object.keys(proto).concat(Object.getOwnPropertyNames(proto));
  for (var i = 0; i < keys.length; i++) {{
    if (["constructor", "set", "get", "clone"].indexOf(keys[i]) === -1) {{ url[keys[i]](); break; }}
  }}
  var s = url.get("s");
  return {{ sig: s ? decodeURIComponent(s) : null, n: url.get("n") }};
}};
_result.n = function (n) {{ return _solve(undefined, n).n; }};
_result.sig = function (sig) {{ return _solve(sig, undefined).sig; }};
"#
    )
}

/// The script as the interpreter loads it: the shims, then the player with the solver
/// placed before its closing call.
fn assemble(script: &str) -> Result<String, PlayerError> {
    let builder = builder_name(script).ok_or(PlayerError::NoBuilder)?;
    let (at, _) = CLOSINGS
        .iter()
        .filter_map(|closing| script.rfind(closing).map(|at| (at, *closing)))
        .max_by_key(|(at, _)| *at)
        .ok_or(PlayerError::NoClosing)?;
    let mut out = String::with_capacity(SHIMS.len() + script.len() + 1024);
    out.push_str(SHIMS);
    out.push_str(&script[..at]);
    out.push_str(&solver(&builder));
    out.push_str(&script[at..]);
    Ok(out)
}

/// The assembled script, and the interpreter it is loaded in while one is up.
struct Solver {
    source: Arc<str>,
    resident: AsyncMutex<Option<Resident>>,
}

impl Solver {
    async fn load(source: Arc<str>) -> Result<Self, PlayerError> {
        let resident = Resident::load(source.clone(), IDLE)
            .await
            .map_err(PlayerError::Load)?;
        Ok(Self {
            source,
            resident: AsyncMutex::new(Some(resident)),
        })
    }

    /// Runs `_result.<name>(input)` in the loaded script, loading it again when the
    /// interpreter was let go, and returns the string it hands back.
    async fn call(
        &self,
        name: &str,
        what: &'static str,
        input: &str,
    ) -> Result<String, PlayerError> {
        let expression = format!("JSON.stringify(_result.{name}({}))", literal(input));
        let mut slot = self.resident.lock().await;
        let mut reloaded = false;
        loop {
            if !slot.as_ref().is_some_and(Resident::is_alive) {
                *slot = Some(
                    Resident::load(self.source.clone(), IDLE)
                        .await
                        .map_err(PlayerError::Load)?,
                );
                reloaded = true;
            }
            let answer = slot
                .as_ref()
                .expect("an interpreter was just loaded")
                .eval(&expression)
                .await;
            let json = match answer {
                Ok(json) => json,
                // The thread went idle between the check and the call.
                Err(JsError::Thread(_)) if !reloaded => {
                    *slot = None;
                    continue;
                }
                Err(error) => return Err(PlayerError::Failed(what, error)),
            };
            let value: Option<String> = serde_json::from_str(&json)
                .map_err(|e| PlayerError::Failed(what, JsError::Run(e.to_string())))?;
            return value
                .filter(|v| !v.is_empty())
                .ok_or(PlayerError::Empty(what));
        }
    }
}

/// One version of the player script, loaded.
pub struct Player {
    pub version: String,
    pub sts: u64,
    solver: Solver,
    deciphered: Mutex<HashMap<String, String>>,
    /// Throttling parameters already transformed: the same one comes with every format
    /// of an answer.
    transformed: Mutex<HashMap<String, String>>,
}

impl std::fmt::Debug for Player {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Player")
            .field("version", &self.version)
            .field("sts", &self.sts)
            .finish_non_exhaustive()
    }
}

impl Player {
    /// Loads `script`, the player version `version`, into the interpreter.
    pub async fn load(version: &str, script: &str) -> Result<Self, PlayerError> {
        let sts = RE_STS
            .captures(script)
            .and_then(|c| c[1].parse().ok())
            .ok_or(PlayerError::NoTimestamp)?;
        let source: Arc<str> = Arc::from(assemble(script)?);
        Ok(Self {
            version: version.to_string(),
            sts,
            solver: Solver::load(source).await?,
            deciphered: Mutex::new(HashMap::new()),
            transformed: Mutex::new(HashMap::new()),
        })
    }

    /// Runs the signature cipher on `s`.
    pub async fn decipher(&self, s: &str) -> Result<String, PlayerError> {
        if let Some(done) = self
            .deciphered
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(s)
        {
            return Ok(done.clone());
        }
        let out = self.solver.call("sig", "signature", s).await?;
        self.deciphered
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(s.to_string(), out.clone());
        Ok(out)
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
        let out = self.solver.call("n", "throttling", n).await?;
        // The transform catches its own failures and hands back a marker followed by the
        // parameter untouched, which the servers then throttle.
        if out.ends_with(n) {
            return Err(PlayerError::Untransformed(out));
        }
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
                let mut base = base.ok_or(PlayerError::NoUrl)?;
                let s = s.ok_or(PlayerError::NoSignature)?;
                let signature = self.decipher(&s).await?;
                base.query_pairs_mut().append_pair(&sp, &signature);
                base
            }
            (Some(url), None) => Url::parse(url).map_err(|_| PlayerError::NoUrl)?,
            (None, None) => return Err(PlayerError::NoUrl),
        };
        let n = target
            .query_pairs()
            .find(|(k, _)| k == "n")
            .map(|(_, v)| v.into_owned());
        if let Some(n) = n {
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

/// The player script as it stands, fetched once per version.
pub struct PlayerCache {
    http: Http,
    cached: AsyncMutex<Option<Arc<Player>>>,
}

impl PlayerCache {
    pub fn new(http: Http) -> Self {
        Self {
            http,
            cached: AsyncMutex::new(None),
        }
    }

    /// The current player script, loaded; fetched when its version changed.
    pub async fn get(&self, origin: &Url) -> Result<Arc<Player>, ResolveError> {
        let api = Url::parse(IFRAME_API).expect("valid");
        let iframe = fetch_ok(&self.http, &api, PLATFORM, BROWSER_UA, &[], MAX_PAGE).await?;
        let text = iframe.text();
        let version = RE_VERSION
            .captures(&text)
            .map(|c| c[1].to_string())
            .ok_or_else(|| ResolveError::malformed(origin, PlayerError::NoVersion.to_string()))?;
        let mut cached = self.cached.lock().await;
        if let Some(player) = cached.as_ref().filter(|p| p.version == version) {
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
        let started = std::time::Instant::now();
        let player = Arc::new(
            Player::load(&version, &fetched.text())
                .await
                .map_err(|e| ResolveError::malformed(origin, e.to_string()))?,
        );
        tracing::info!(
            version,
            sts = player.sts,
            took_ms = started.elapsed().as_millis(),
            "youtube player script loaded"
        );
        *cached = Some(player.clone());
        Ok(player)
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A player script with the shapes the real one has: an enclosing function called
    /// with `_yt_player`, a string table, a signature cipher over a helper object, a
    /// throttling transform guarded by a global and wrapped in a catch that hands the
    /// parameter back marked, a URL class whose serializer runs the transform, and the
    /// URL builder that marks URLs with `alr` and enciphers the signature.
    pub const SCRIPT: &str = r#"var _yt_player={};(function(g){var window=this;
'use strict';var Xz="fromCharCode;split;join".split(";");
var Ya={reverse:function(a){a.reverse()},splice:function(a,b){a.splice(0,b)},swap:function(a,b){var c=a[0];a[0]=a[b%a.length];a[b%a.length]=c}};
var Xa=function(a){a=a.split("");Ya.reverse(a,1);Ya.splice(a,2);Ya.swap(a,3);return a.join("")};
var Qz=1998932147;
var Wa=function(a){var b=a.split(""),c=[];if(typeof Qz==="undefined")return a;try{if(a==="boom")throw Error("x");for(var d=0;d<b.length;d++)c.push(b[b.length-1-d]);return c.join("")+"_w8_"+Xz[1]}catch(e){return "enhanced_except_"+a}};
g.jZ=function(a,b){this.Y=a;this.K={};this.url=""};
g.jZ.prototype.set=function(a,b){this.K[a]!==b&&(this.K[a]=b,this.url="")};
g.jZ.prototype.get=function(a){return this.K[a]||null};
g.jZ.prototype.pB=function(){if(!this.url){var n=this.K.n;n&&(this.K.n=Wa(n));this.url=this.Y+"?s="+this.K.s}return this.url};
g.jZ.prototype.clone=function(){return new g.jZ(this.Y)};
Ix=function(K,H,r){H=H===void 0?"":H;r=r===void 0?"":r;K=new g.jZ(K,!0);K.set("alr","yes");r&&K.set(H,encodeURIComponent(Xa(decodeURIComponent(r))));return K};
g.cfg={signatureTimestamp:19999,x:1};
})(_yt_player);"#;

    #[tokio::test]
    async fn the_script_is_loaded_and_its_transforms_run() {
        let player = Player::load("abcdef12", SCRIPT).await.unwrap();
        assert_eq!(player.sts, 19999);
        assert_eq!(player.decipher("abcdefgh").await.unwrap(), "cedfba");
        assert_eq!(player.throttle("xyz").await.unwrap(), "zyx_w8_split");
        assert_eq!(player.throttle("xyz").await.unwrap(), "zyx_w8_split");
        assert!(matches!(
            player.throttle("boom").await,
            Err(PlayerError::Untransformed(out)) if out == "enhanced_except_boom"
        ));

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
        assert!(matches!(
            player.unlock(None, None).await,
            Err(PlayerError::NoUrl)
        ));
        assert!(matches!(
            player.unlock(None, Some("url=https%3A%2F%2Fh%2Fv")).await,
            Err(PlayerError::NoSignature)
        ));
    }

    #[tokio::test]
    async fn the_interpreter_is_loaded_again_after_going_idle() {
        let player = Player::load("abcdef12", SCRIPT).await.unwrap();
        let resident = player.solver.resident.lock().await.take().unwrap();
        drop(resident);
        assert_eq!(player.decipher("abcdefgh").await.unwrap(), "cedfba");
        assert!(player.solver.resident.lock().await.is_some());
    }

    #[test]
    fn the_builder_is_found_in_every_shape_it_is_declared_in() {
        assert_eq!(builder_name(SCRIPT).as_deref(), Some("Ix"));
        assert_eq!(
            builder_name(r#"x=1;function wM(K,H,r){H=H===void 0?"":H;K=new v5(K,!0);K.set("alr","yes");return K}"#)
                .as_deref(),
            Some("wM")
        );
        assert_eq!(
            builder_name(r#";g.vm=function(K,H,r){K=new g.jZ(K,!0);K.set("alr","yes");return K};"#)
                .as_deref(),
            Some("g.vm")
        );
        // A call that is not a whole statement of a function body is another use of the
        // same marker, not the builder.
        assert_eq!(
            builder_name(r#"var f=function(O){return O.get("alr")||O.set("alr","yes")}"#),
            None
        );
        assert!(builder_name("nothing here").is_none());
    }

    #[test]
    fn the_solver_goes_inside_the_closing_call() {
        let assembled = assemble(SCRIPT).unwrap();
        assert!(assembled.starts_with(SHIMS));
        let solver_at = assembled.find("_result.sig = function").unwrap();
        let closing_at = assembled.rfind("})(_yt_player);").unwrap();
        assert!(solver_at < closing_at);
        assert!(assembled.contains(r#"var url = Ix("https://youtube.com/watch?v=yt-dlp-wins""#));
        let tv = SCRIPT.replace("var _yt_player={};(function(g){", "(function(){var g={};")
            .replace("})(_yt_player);", "}).call(this);");
        assert!(assemble(&tv).unwrap().ends_with("}).call(this);"));
        assert!(matches!(
            assemble("sts:12345;"),
            Err(PlayerError::NoBuilder)
        ));
        assert!(matches!(
            assemble(r#"Ix=function(K){K.set("alr","yes");return K};"#),
            Err(PlayerError::NoClosing)
        ));
    }

    #[tokio::test]
    async fn scripts_that_cannot_be_loaded_say_why() {
        assert!(matches!(
            Player::load("v", "nothing here").await,
            Err(PlayerError::NoTimestamp)
        ));
        assert!(matches!(
            Player::load("v", "sts:12345;").await,
            Err(PlayerError::NoBuilder)
        ));
        let broken = SCRIPT.replace("g.cfg={", "g.cfg=throw {");
        assert!(matches!(
            Player::load("v", &broken).await,
            Err(PlayerError::Load(JsError::Parse(_)))
        ));
        let throwing = SCRIPT.replace("var Qz=1998932147;", "var Qz=missing();");
        assert!(matches!(
            Player::load("v", &throwing).await,
            Err(PlayerError::Load(JsError::Run(_)))
        ));
    }
}
