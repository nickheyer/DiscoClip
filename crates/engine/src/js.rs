//! A JavaScript interpreter for the scripts platforms guard their media with: the
//! signature and throttling ciphers a player script computes are run as a browser would
//! run them, in a sandbox with nothing but the script and bounds on how long it may run.

use std::sync::Arc;

use boa_engine::property::PropertyKey;
use boa_engine::{Context, JsString, JsValue, Source};

/// How many loop iterations a call may take before it is stopped.
const LOOP_LIMIT: u64 = 5_000_000;
/// How deep calls may nest.
const RECURSION_LIMIT: usize = 400;

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
}
