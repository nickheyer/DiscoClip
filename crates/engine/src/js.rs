//! A JavaScript interpreter for the scripts platforms guard their media with: the
//! signature and throttling ciphers a player script computes are run as a browser would
//! run them, in a sandbox with nothing but the script and bounds on how long it may run.
//! Small scripts run afresh for every call; a player script of several megabytes stays
//! loaded in an interpreter on a thread of its own, answering calls until it has been
//! idle for a while.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use boa_engine::property::PropertyKey;
use boa_engine::{Context, JsString, JsValue, Source};
use tokio::sync::oneshot;

/// How many loop iterations a call may take before it is stopped.
const LOOP_LIMIT: u64 = 5_000_000;
/// How deep calls may nest.
const RECURSION_LIMIT: usize = 400;
/// A resident script's bounds: loading a player script runs far longer loops and deeper
/// calls than a cipher does.
const RESIDENT_LOOP_LIMIT: u64 = 500_000_000;
const RESIDENT_RECURSION_LIMIT: usize = 2_000;
/// How long a resident script may take to load, and how long one call may take.
pub const RESIDENT_LOAD_TIMEOUT: Duration = Duration::from_secs(120);
pub const RESIDENT_CALL_TIMEOUT: Duration = Duration::from_secs(30);
/// The stack of the resident thread: parsing and running a player script nests far
/// deeper than a thread's default allows.
const RESIDENT_STACK: usize = 256 * 1024 * 1024;

#[derive(Debug, Clone, thiserror::Error)]
pub enum JsError {
    #[error("the script does not parse: {0}")]
    Parse(String),
    #[error("the script failed: {0}")]
    Run(String),
    #[error("{0} is not a function in the script")]
    NotAFunction(String),
    #[error("the interpreter thread stopped: {0}")]
    Thread(String),
    #[error("the script took longer than {0:?}")]
    Timeout(Duration),
}

/// A script, run afresh for every call so nothing leaks between them.
#[derive(Debug, Clone)]
pub struct Script {
    source: Arc<str>,
}

impl Script {
    pub fn new(source: impl Into<Arc<str>>) -> Self {
        Self {
            source: source.into(),
        }
    }

    pub fn source(&self) -> &str {
        &self.source
    }

    fn context() -> Context {
        let mut context = Context::default();
        let limits = context.runtime_limits_mut();
        limits.set_loop_iteration_limit(LOOP_LIMIT);
        limits.set_recursion_limit(RECURSION_LIMIT);
        context
    }

    fn load(&self, context: &mut Context) -> Result<(), JsError> {
        context
            .eval(Source::from_bytes(self.source.as_bytes()))
            .map(drop)
            .map_err(|error| {
                let text = error.to_string();
                if error.as_opaque().is_none() && text.contains("SyntaxError") {
                    JsError::Parse(text)
                } else {
                    JsError::Run(text)
                }
            })
    }

    /// Calls the global `function` with string arguments and returns what it returns, as a
    /// string; on the calling thread.
    pub fn call_blocking(&self, function: &str, args: &[String]) -> Result<String, JsError> {
        let mut context = Self::context();
        self.load(&mut context)?;
        let key = PropertyKey::from(JsString::from(function));
        let value = context
            .global_object()
            .get(key, &mut context)
            .map_err(|e| JsError::Run(e.to_string()))?;
        let callable = value
            .as_callable()
            .ok_or_else(|| JsError::NotAFunction(function.to_string()))?;
        let args: Vec<JsValue> = args
            .iter()
            .map(|arg| JsValue::from(JsString::from(arg.as_str())))
            .collect();
        let result = callable
            .call(&JsValue::undefined(), &args, &mut context)
            .map_err(|e| JsError::Run(e.to_string()))?;
        stringify(result, &mut context)
    }

    /// Evaluates `expression` once the script has run, as a string; on the calling thread.
    pub fn eval_blocking(&self, expression: &str) -> Result<String, JsError> {
        let mut context = Self::context();
        self.load(&mut context)?;
        let result = context
            .eval(Source::from_bytes(expression.as_bytes()))
            .map_err(|e| JsError::Run(e.to_string()))?;
        stringify(result, &mut context)
    }

    /// [`Self::call_blocking`] off the async runtime's threads.
    pub async fn call(&self, function: &str, args: &[String]) -> Result<String, JsError> {
        let script = self.clone();
        let function = function.to_string();
        let args = args.to_vec();
        tokio::task::spawn_blocking(move || script.call_blocking(&function, &args))
            .await
            .map_err(|e| JsError::Thread(e.to_string()))?
    }

    /// [`Self::eval_blocking`] off the async runtime's threads.
    pub async fn eval(&self, expression: &str) -> Result<String, JsError> {
        let script = self.clone();
        let expression = expression.to_string();
        tokio::task::spawn_blocking(move || script.eval_blocking(&expression))
            .await
            .map_err(|e| JsError::Thread(e.to_string()))?
    }
}

/// A request to a resident script: an expression, and where its value goes.
struct Call {
    expression: String,
    reply: oneshot::Sender<Result<String, JsError>>,
}

/// A script loaded once into an interpreter on a thread of its own, which evaluates
/// expressions against it as they come. The thread ends when the handle is dropped or
/// when no call has come for `idle`; a call after that finds it stopped.
pub struct Resident {
    calls: Mutex<mpsc::Sender<Call>>,
    alive: Arc<AtomicBool>,
    idle: Duration,
}

impl std::fmt::Debug for Resident {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Resident")
            .field("idle", &self.idle)
            .field("alive", &self.is_alive())
            .finish()
    }
}

impl Resident {
    /// Loads `source` on a new thread and resolves once it has run, with what went wrong
    /// when it did not. The thread stays for calls until `idle` passes without one.
    pub async fn load(source: Arc<str>, idle: Duration) -> Result<Self, JsError> {
        let (calls, requests) = mpsc::channel::<Call>();
        let (loaded, ready) = oneshot::channel::<Result<(), JsError>>();
        let alive = Arc::new(AtomicBool::new(true));
        let flag = alive.clone();
        thread::Builder::new()
            .name("js-resident".into())
            .stack_size(RESIDENT_STACK)
            .spawn(move || {
                serve(source, idle, requests, loaded);
                flag.store(false, Ordering::Release);
            })
            .map_err(|e| JsError::Thread(e.to_string()))?;
        let outcome = tokio::time::timeout(RESIDENT_LOAD_TIMEOUT, ready)
            .await
            .map_err(|_| JsError::Timeout(RESIDENT_LOAD_TIMEOUT))?
            .map_err(|_| JsError::Thread("stopped while loading".into()))?;
        outcome?;
        Ok(Self {
            calls: Mutex::new(calls),
            alive,
            idle,
        })
    }

    /// Whether the thread is still there to take calls.
    pub fn is_alive(&self) -> bool {
        self.alive.load(Ordering::Acquire)
    }

    /// Evaluates `expression` against the loaded script, as a string.
    pub async fn eval(&self, expression: &str) -> Result<String, JsError> {
        let (reply, answer) = oneshot::channel();
        let call = Call {
            expression: expression.to_string(),
            reply,
        };
        let sent = self
            .calls
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .send(call)
            .is_ok();
        if !sent {
            return Err(JsError::Thread(format!(
                "stopped after {:?} without a call",
                self.idle
            )));
        }
        tokio::time::timeout(RESIDENT_CALL_TIMEOUT, answer)
            .await
            .map_err(|_| JsError::Timeout(RESIDENT_CALL_TIMEOUT))?
            .map_err(|_| JsError::Thread("stopped during the call".into()))?
    }
}

/// The resident thread: loads the script, reports how that went, then answers calls
/// until none has come for `idle`.
fn serve(
    source: Arc<str>,
    idle: Duration,
    requests: mpsc::Receiver<Call>,
    loaded: oneshot::Sender<Result<(), JsError>>,
) {
    let mut context = Context::default();
    let limits = context.runtime_limits_mut();
    limits.set_loop_iteration_limit(RESIDENT_LOOP_LIMIT);
    limits.set_recursion_limit(RESIDENT_RECURSION_LIMIT);
    let started = Instant::now();
    let outcome = context
        .eval(Source::from_bytes(source.as_bytes()))
        .map(drop)
        .map_err(|error| {
            let text = error.to_string();
            if error.as_opaque().is_none() && text.contains("SyntaxError") {
                JsError::Parse(text)
            } else {
                JsError::Run(text)
            }
        });
    tracing::debug!(
        bytes = source.len(),
        took_ms = started.elapsed().as_millis(),
        ok = outcome.is_ok(),
        "resident script loaded"
    );
    let failed = outcome.is_err();
    let _ = loaded.send(outcome);
    if failed {
        return;
    }
    while let Ok(call) = requests.recv_timeout(idle) {
        let result = context
            .eval(Source::from_bytes(call.expression.as_bytes()))
            .map_err(|e| JsError::Run(e.to_string()))
            .and_then(|value| stringify(value, &mut context));
        let _ = call.reply.send(result);
    }
    tracing::debug!("resident script let go");
}

/// `text` as a JavaScript string literal.
pub fn literal(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 2);
    out.push('"');
    for c in text.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\u{2028}' => out.push_str("\\u2028"),
            '\u{2029}' => out.push_str("\\u2029"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

fn stringify(value: JsValue, context: &mut Context) -> Result<String, JsError> {
    value
        .to_string(context)
        .map(|s| s.to_std_string_escaped())
        .map_err(|e| JsError::Run(e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    const CIPHER: &str = r#"
        var helpers = {
            swap: function(a, b) { var c = a[0]; a[0] = a[b % a.length]; a[b % a.length] = c; },
            reverse: function(a) { a.reverse(); },
            slice: function(a, b) { a.splice(0, b); }
        };
        function decipher(s) {
            var a = s.split("");
            helpers.reverse(a);
            helpers.swap(a, 3);
            helpers.slice(a, 1);
            return a.join("");
        }
        function throttle(n) {
            var out = [];
            for (var i = 0; i < n.length; i++) out.push(n.charCodeAt(i) ^ 1);
            return String.fromCharCode.apply(null, out);
        }
    "#;

    #[tokio::test]
    async fn ciphers_run_as_a_browser_would() {
        let script = Script::new(CIPHER);
        assert_eq!(
            script.call("decipher", &["abcdef".into()]).await.unwrap(),
            "edfba"
        );
        assert_eq!(
            script.call("throttle", &["abc".into()]).await.unwrap(),
            "`cb"
        );
        assert_eq!(script.eval("decipher('xyz') + '!'").await.unwrap(), "yx!");
    }

    #[test]
    fn failures_are_named() {
        let script = Script::new(CIPHER);
        assert!(matches!(
            script.call_blocking("nothing", &[]),
            Err(JsError::NotAFunction(name)) if name == "nothing"
        ));
        assert!(matches!(
            Script::new("function (").call_blocking("f", &[]),
            Err(JsError::Parse(_))
        ));
        assert!(matches!(
            Script::new("function f() { throw new Error('nope'); }").call_blocking("f", &[]),
            Err(JsError::Run(message)) if message.contains("nope")
        ));
        assert!(matches!(
            Script::new("function f() { while (true) {} }").call_blocking("f", &[]),
            Err(JsError::Run(_))
        ));
        assert!(matches!(
            Script::new("function f() { return f(); }").call_blocking("f", &[]),
            Err(JsError::Run(_))
        ));
    }

    #[tokio::test]
    async fn a_resident_script_answers_calls_until_it_is_idle() {
        let resident = Resident::load(
            Arc::from(format!("{CIPHER}\nvar calls = 0;")),
            Duration::from_millis(400),
        )
        .await
        .unwrap();
        assert!(resident.is_alive());
        assert_eq!(resident.eval("decipher('abcdef')").await.unwrap(), "edfba");
        // State stays between calls, unlike a script run afresh.
        assert_eq!(resident.eval("++calls").await.unwrap(), "1");
        assert_eq!(resident.eval("++calls").await.unwrap(), "2");
        assert!(matches!(
            resident.eval("nothing()").await,
            Err(JsError::Run(message)) if message.contains("nothing")
        ));
        tokio::time::sleep(Duration::from_millis(900)).await;
        assert!(!resident.is_alive());
        assert!(matches!(
            resident.eval("1").await,
            Err(JsError::Thread(_))
        ));

        assert!(matches!(
            Resident::load(Arc::from("function ("), Duration::from_secs(1)).await,
            Err(JsError::Parse(_))
        ));
        assert!(matches!(
            Resident::load(Arc::from("throw new Error('boom')"), Duration::from_secs(1)).await,
            Err(JsError::Run(message)) if message.contains("boom")
        ));
    }

    #[test]
    fn literals_escape_what_a_script_would_misread() {
        assert_eq!(literal("plain"), "\"plain\"");
        assert_eq!(literal("a\"b\\c\n"), "\"a\\\"b\\\\c\\n\"");
        assert_eq!(literal("\u{1}"), "\"\\u0001\"");
        assert_eq!(
            Script::new("").eval_blocking(&format!("{} + '!'", literal("x\"y"))).unwrap(),
            "x\"y!"
        );
    }
}
