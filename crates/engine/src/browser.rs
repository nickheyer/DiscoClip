//! A headless Chromium or Chrome run for one capture and spoken to over the DevTools
//! protocol: where the executable is found, how it is started with a profile of its own,
//! and the commands and events of the pages it opens.

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::{mpsc, oneshot};
use tokio_websockets::{ClientBuilder, Message};
use url::Url;

use crate::config::BrowserConfig;

/// The names a Chromium build goes by on `PATH`, the plain builds first.
const NAMES: [&str; 10] = [
    "chromium",
    "chromium-browser",
    "google-chrome",
    "google-chrome-stable",
    "google-chrome-beta",
    "google-chrome-unstable",
    "chrome",
    "brave-browser",
    "brave",
    "microsoft-edge",
];

/// Where installers put the browser outside `PATH`.
const PLACES: [&str; 12] = [
    "/opt/google/chrome/chrome",
    "/snap/bin/chromium",
    "/usr/lib/chromium/chromium",
    "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
    "/Applications/Chromium.app/Contents/MacOS/Chromium",
    "/Applications/Brave Browser.app/Contents/MacOS/Brave Browser",
    "/Applications/Microsoft Edge.app/Contents/MacOS/Microsoft Edge",
    r"C:\Program Files\Google\Chrome\Application\chrome.exe",
    r"C:\Program Files (x86)\Google\Chrome\Application\chrome.exe",
    r"C:\Program Files\Chromium\Application\chrome.exe",
    r"C:\Program Files (x86)\Microsoft\Edge\Application\msedge.exe",
    r"C:\Program Files\BraveSoftware\Brave-Browser\Application\brave.exe",
];

/// How long the browser may take to print its DevTools address.
const LAUNCH_TIMEOUT: Duration = Duration::from_secs(120);
/// How long one command may take to be answered.
const COMMAND_TIMEOUT: Duration = Duration::from_secs(30);
/// How long `--version` may take.
const VERSION_TIMEOUT: Duration = Duration::from_secs(15);
/// How long a closing browser gets before it is killed.
const CLOSE_TIMEOUT: Duration = Duration::from_secs(5);
/// The lines of the browser's stderr kept for an error report
const STDERR_TAIL: usize = 60;
/// Events waiting for the capture to take them before the reader stops reading.
const EVENT_QUEUE: usize = 4096;

#[derive(Debug, thiserror::Error)]
pub enum BrowserError {
    #[error("{0}")]
    NotFound(String),
    #[error("{0}")]
    Launch(String),
    #[error("the browser connection closed: {0}")]
    Closed(String),
    #[error("{method}: {message}")]
    Command { method: String, message: String },
    #[error("{0}")]
    Timeout(String),
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

/// The executable the configuration names, else the first of the usual names on `PATH`,
/// else the first of the usual install locations.
pub fn locate(config: &BrowserConfig) -> Result<PathBuf, BrowserError> {
    let path = std::env::var_os("PATH").unwrap_or_default();
    let dirs: Vec<PathBuf> = std::env::split_paths(&path).collect();
    let mut places: Vec<PathBuf> = PLACES.iter().map(PathBuf::from).collect();
    if let Some(local) = std::env::var_os("LOCALAPPDATA") {
        places.push(PathBuf::from(local).join(r"Google\Chrome\Application\chrome.exe"));
    }
    locate_among(config, &dirs, &places)
}

/// [`locate`] over the given `PATH` directories and install locations.
pub fn locate_among(
    config: &BrowserConfig,
    path_dirs: &[PathBuf],
    places: &[PathBuf],
) -> Result<PathBuf, BrowserError> {
    if let Some(executable) = &config.executable {
        if executable.is_file() {
            return Ok(executable.clone());
        }
        return Err(BrowserError::NotFound(format!(
            "engine.browser.executable {} is not a file",
            executable.display()
        )));
    }
    for name in NAMES {
        for dir in path_dirs {
            let candidate = dir.join(name);
            if candidate.is_file() {
                return Ok(candidate);
            }
            let with_exe = dir.join(format!("{name}.exe"));
            if cfg!(windows) && with_exe.is_file() {
                return Ok(with_exe);
            }
        }
    }
    if let Some(found) = places.iter().find(|place| place.is_file()) {
        return Ok(found.clone());
    }
    Err(BrowserError::NotFound(
        "no Chromium or Chrome executable was found on PATH or in the usual install \
         locations. Install one or set engine.browser.executable."
            .into(),
    ))
}

/// What the browser calls itself, as `--version` prints it. A build that prints nothing
/// for `--version`, as Chrome on Windows does, is started headless and asked instead.
pub async fn version(executable: &Path) -> Result<String, BrowserError> {
    let output = tokio::time::timeout(
        VERSION_TIMEOUT,
        Command::new(executable)
            .arg("--version")
            .stdin(Stdio::null())
            .kill_on_drop(true)
            .output(),
    )
    .await
    .map_err(|_| {
        BrowserError::Timeout(format!(
            "{} --version did not finish within {}s",
            executable.display(),
            VERSION_TIMEOUT.as_secs()
        ))
    })?
    .map_err(|e| BrowserError::Launch(format!("{} did not run: {e}", executable.display())))?;
    let printed = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if output.status.success() && !printed.is_empty() {
        return Ok(printed);
    }
    let profile = std::env::temp_dir().join(format!("discoclip-browser-{}", uuid::Uuid::now_v7()));
    let browser = Browser::launch(executable, &[], &profile, None).await?;
    let product = browser.version().product.clone();
    browser.close().await;
    Ok(product)
}

/// The `--proxy-server` value for `proxy`, and the credentials it carries, which the
/// flag cannot take and are answered to the browser's authentication challenge instead.
pub fn proxy_flag(proxy: &Url) -> (String, Option<(String, String)>) {
    let scheme = match proxy.scheme() {
        "socks5h" => "socks5",
        "socks4a" => "socks4",
        other => other,
    };
    let host = proxy.host_str().unwrap_or("");
    let flag = match proxy.port() {
        Some(port) => format!("{scheme}://{host}:{port}"),
        None => format!("{scheme}://{host}"),
    };
    let credentials = (!proxy.username().is_empty()).then(|| {
        (
            percent_encoding::percent_decode_str(proxy.username())
                .decode_utf8_lossy()
                .into_owned(),
            proxy
                .password()
                .map(|p| {
                    percent_encoding::percent_decode_str(p)
                        .decode_utf8_lossy()
                        .into_owned()
                })
                .unwrap_or_default(),
        )
    });
    (flag, credentials)
}

/// Whether this process runs as root, where Chromium refuses to start its sandbox.
fn running_as_root() -> bool {
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::fs::MetadataExt;
        std::fs::metadata("/proc/self").is_ok_and(|m| m.uid() == 0)
    }
    #[cfg(not(target_os = "linux"))]
    {
        false
    }
}

/// The address in `DevTools listening on ws://…`, when `line` is that line.
pub fn devtools_endpoint(line: &str) -> Option<String> {
    let rest = line.trim().strip_prefix("DevTools listening on ")?;
    let address = rest.trim();
    address.starts_with("ws://").then(|| address.to_string())
}

/// What the browser reports about itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Version {
    /// `Chrome/153.0.8010.36`, or `HeadlessChrome/…` in a headless build.
    pub product: String,
    /// The user agent the browser would send.
    pub user_agent: String,
}

impl Version {
    /// The user agent with the headless build's mark taken out, so pages see the browser
    /// a person would run.
    pub fn plain_user_agent(&self) -> String {
        self.user_agent.replace("HeadlessChrome/", "Chrome/")
    }

    /// The full version, `153.0.8010.36`, and its major part.
    pub fn numbers(&self) -> (String, String) {
        let full = self
            .product
            .rsplit('/')
            .next()
            .unwrap_or("")
            .trim()
            .to_string();
        let major = full.split('.').next().unwrap_or("").to_string();
        (full, major)
    }
}

/// An event a target sent: the session it came from, for a page or a frame attached
/// through one, and the event's parameters.
#[derive(Debug, Clone)]
pub struct Event {
    pub session: Option<String>,
    pub method: String,
    pub params: Value,
}

type Pending = Mutex<HashMap<u64, oneshot::Sender<Result<Value, BrowserError>>>>;

struct Connection {
    next_id: AtomicU64,
    pending: Pending,
    outgoing: mpsc::Sender<String>,
}

/// The DevTools connection: commands answered by id, events queued for the capture.
#[derive(Clone)]
pub struct Cdp {
    inner: Arc<Connection>,
}

impl Cdp {
    async fn connect(endpoint: &str) -> Result<(Self, mpsc::Receiver<Event>), BrowserError> {
        let (stream, _) = ClientBuilder::new()
            .uri(endpoint)
            .map_err(|e| BrowserError::Launch(format!("DevTools address {endpoint}: {e}")))?
            .connect()
            .await
            .map_err(|e| BrowserError::Launch(format!("connecting to {endpoint}: {e}")))?;
        let (mut sink, mut source) = stream.split();
        let (outgoing, mut to_send) = mpsc::channel::<String>(256);
        let (events, event_queue) = mpsc::channel::<Event>(EVENT_QUEUE);
        let inner = Arc::new(Connection {
            next_id: AtomicU64::new(1),
            pending: Mutex::new(HashMap::new()),
            outgoing,
        });
        tokio::spawn(async move {
            while let Some(text) = to_send.recv().await {
                if sink.send(Message::text(text)).await.is_err() {
                    break;
                }
            }
            let _ = sink.close().await;
        });
        let reader = inner.clone();
        tokio::spawn(async move {
            let why = loop {
                match source.next().await {
                    Some(Ok(message)) => {
                        if message.is_close() {
                            break "the browser closed the connection".to_string();
                        }
                        let Some(text) = message.as_text() else {
                            continue;
                        };
                        let Ok(value) = serde_json::from_str::<Value>(text) else {
                            continue;
                        };
                        if let Some(id) = value["id"].as_u64() {
                            let waiting = reader
                                .pending
                                .lock()
                                .unwrap_or_else(|e| e.into_inner())
                                .remove(&id);
                            if let Some(waiting) = waiting {
                                let answer = match value.get("error") {
                                    Some(error) => Err(BrowserError::Command {
                                        method: String::new(),
                                        message: error["message"]
                                            .as_str()
                                            .unwrap_or("unknown error")
                                            .to_string(),
                                    }),
                                    None => Ok(value["result"].clone()),
                                };
                                let _ = waiting.send(answer);
                            }
                        } else if let Some(method) = value["method"].as_str() {
                            let event = Event {
                                session: value["sessionId"].as_str().map(str::to_owned),
                                method: method.to_string(),
                                params: value["params"].clone(),
                            };
                            if events.send(event).await.is_err() {
                                break "the capture stopped listening".to_string();
                            }
                        }
                    }
                    Some(Err(error)) => break error.to_string(),
                    None => break "the browser closed the connection".to_string(),
                }
            };
            let waiting: Vec<_> = reader
                .pending
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .drain()
                .map(|(_, sender)| sender)
                .collect();
            for sender in waiting {
                let _ = sender.send(Err(BrowserError::Closed(why.clone())));
            }
        });
        Ok((Self { inner }, event_queue))
    }

    /// Sends `method` with `params` to the browser, or to `session`, and waits for the
    /// answer.
    pub async fn call(
        &self,
        session: Option<&str>,
        method: &str,
        params: Value,
    ) -> Result<Value, BrowserError> {
        let id = self.inner.next_id.fetch_add(1, Ordering::Relaxed);
        let mut message = json!({"id": id, "method": method, "params": params});
        if let Some(session) = session {
            message["sessionId"] = Value::String(session.to_string());
        }
        let (sender, receiver) = oneshot::channel();
        self.inner
            .pending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(id, sender);
        if self.inner.outgoing.send(message.to_string()).await.is_err() {
            self.inner
                .pending
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .remove(&id);
            return Err(BrowserError::Closed("the browser went away".into()));
        }
        let answer = tokio::time::timeout(COMMAND_TIMEOUT, receiver).await;
        match answer {
            Ok(Ok(Ok(result))) => Ok(result),
            Ok(Ok(Err(BrowserError::Command { message, .. }))) => Err(BrowserError::Command {
                method: method.to_string(),
                message,
            }),
            Ok(Ok(Err(error))) => Err(error),
            Ok(Err(_)) => Err(BrowserError::Closed("the browser went away".into())),
            Err(_) => {
                self.inner
                    .pending
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .remove(&id);
                Err(BrowserError::Timeout(format!(
                    "{method} was not answered within {}s",
                    COMMAND_TIMEOUT.as_secs()
                )))
            }
        }
    }
}

/// A page opened in the browser, addressed by its session.
#[derive(Debug, Clone)]
pub struct Page {
    pub session: String,
    pub target: String,
}

/// A running headless browser with a profile of its own, gone when closed.
pub struct Browser {
    child: Child,
    cdp: Cdp,
    events: Option<mpsc::Receiver<Event>>,
    profile: PathBuf,
    version: Version,
    stderr: Arc<Mutex<VecDeque<String>>>,
}

impl Browser {
    /// Starts `executable` headless on a fresh profile at `profile`, through `proxy` when
    /// one is given, with `extra` arguments after the engine's own.
    pub async fn launch(
        executable: &Path,
        extra: &[String],
        profile: &Path,
        proxy: Option<&Url>,
    ) -> Result<Self, BrowserError> {
        tokio::fs::create_dir_all(profile).await?;
        let mut command = Command::new(executable);
        command.args([
            "--headless=new",
            "--remote-debugging-port=0",
            "--no-first-run",
            "--no-default-browser-check",
            "--disable-gpu",
            "--mute-audio",
            "--autoplay-policy=no-user-gesture-required",
            "--disable-background-timer-throttling",
            "--disable-renderer-backgrounding",
            "--disable-backgrounding-occluded-windows",
            "--disable-background-networking",
            "--disable-sync",
            "--disable-extensions",
            "--disable-component-update",
            "--disable-default-apps",
            "--disable-breakpad",
            "--disable-crash-reporter",
            "--disable-dev-shm-usage",
            "--no-service-autorun",
            "--password-store=basic",
            "--use-mock-keychain",
            "--hide-scrollbars",
            "--window-size=1920,1080",
            "--disable-features=Translate,MediaRouter,OptimizationHints,InterestFeedContentSuggestions",
        ]);
        command.arg(format!("--user-data-dir={}", profile.display()));
        if let Some(proxy) = proxy {
            command.arg(format!("--proxy-server={}", proxy_flag(proxy).0));
        }
        if running_as_root() {
            command.arg("--no-sandbox");
        }
        command.args(extra);
        command.arg("about:blank");
        command
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        let mut child = command.spawn().map_err(|e| {
            BrowserError::Launch(format!("{} did not start: {e}", executable.display()))
        })?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| BrowserError::Launch("the browser has no stderr".into()))?;
        let tail: Arc<Mutex<VecDeque<String>>> = Arc::new(Mutex::new(VecDeque::new()));
        let (found, endpoint) = oneshot::channel::<String>();
        let kept = tail.clone();
        tokio::spawn(async move {
            let mut found = Some(found);
            let mut stderr = BufReader::new(stderr);
            let mut bytes = Vec::new();
            while matches!(stderr.read_until(b'\n', &mut bytes).await, Ok(read) if read > 0) {
                let line = String::from_utf8_lossy(&bytes)
                    .trim_end_matches(['\n', '\r'])
                    .to_string();
                bytes.clear();
                if let Some(address) = devtools_endpoint(&line)
                    && let Some(sender) = found.take()
                {
                    let _ = sender.send(address);
                    continue;
                }
                let mut kept = kept.lock().unwrap_or_else(|e| e.into_inner());
                if kept.len() == STDERR_TAIL {
                    kept.pop_front();
                }
                kept.push_back(line);
            }
        });
        let said = |tail: &Mutex<VecDeque<String>>| -> String {
            let lines = tail.lock().unwrap_or_else(|e| e.into_inner());
            let text = lines.iter().cloned().collect::<Vec<_>>().join("\n");
            if text.is_empty() {
                String::new()
            } else {
                format!(": {text}")
            }
        };
        let failed = |what: String| -> BrowserError {
            BrowserError::Launch(format!("{} {what}{}", executable.display(), said(&tail)))
        };
        let exited = |status: std::io::Result<std::process::ExitStatus>| -> String {
            let status = status
                .map(|s| s.to_string())
                .unwrap_or_else(|e| e.to_string());
            format!("exited before it was ready ({status})")
        };
        let endpoint = tokio::select! {
            found = tokio::time::timeout(LAUNCH_TIMEOUT, endpoint) => match found {
                Ok(Ok(address)) => address,
                Ok(Err(_)) => {
                    // Its stderr closed without the address: the browser is on its way out.
                    let what = match tokio::time::timeout(CLOSE_TIMEOUT, child.wait()).await {
                        Ok(status) => exited(status),
                        Err(_) => {
                            let _ = child.kill().await;
                            "closed its stderr without printing a DevTools address".to_string()
                        }
                    };
                    let _ = tokio::fs::remove_dir_all(profile).await;
                    return Err(failed(what));
                }
                Err(_) => {
                    let _ = child.kill().await;
                    let _ = tokio::fs::remove_dir_all(profile).await;
                    return Err(failed(format!(
                        "printed no DevTools address within {}s",
                        LAUNCH_TIMEOUT.as_secs()
                    )));
                }
            },
            status = child.wait() => {
                let _ = tokio::fs::remove_dir_all(profile).await;
                return Err(failed(exited(status)));
            }
        };
        let (cdp, events) = match Cdp::connect(&endpoint).await {
            Ok(connected) => connected,
            Err(error) => {
                let _ = child.kill().await;
                let _ = tokio::fs::remove_dir_all(profile).await;
                return Err(error);
            }
        };
        let version = match cdp.call(None, "Browser.getVersion", json!({})).await {
            Ok(answer) => Version {
                product: answer["product"].as_str().unwrap_or("").to_string(),
                user_agent: answer["userAgent"].as_str().unwrap_or("").to_string(),
            },
            Err(error) => {
                let _ = child.kill().await;
                let _ = tokio::fs::remove_dir_all(profile).await;
                return Err(error);
            }
        };
        Ok(Self {
            child,
            cdp,
            events: Some(events),
            profile: profile.to_path_buf(),
            version,
            stderr: tail,
        })
    }

    pub fn version(&self) -> &Version {
        &self.version
    }

    /// The connection, for commands.
    pub fn cdp(&self) -> &Cdp {
        &self.cdp
    }

    /// The queue of events the browser and its pages send. Taken once.
    pub fn take_events(&mut self) -> Option<mpsc::Receiver<Event>> {
        self.events.take()
    }

    /// What the browser printed to stderr lately.
    pub fn stderr_tail(&self) -> Vec<String> {
        self.stderr
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .cloned()
            .collect()
    }

    /// Opens a blank page and attaches to it.
    pub async fn open_page(&self) -> Result<Page, BrowserError> {
        let created = self
            .cdp
            .call(None, "Target.createTarget", json!({"url": "about:blank"}))
            .await?;
        let target = created["targetId"]
            .as_str()
            .ok_or_else(|| BrowserError::Command {
                method: "Target.createTarget".into(),
                message: "no targetId in the answer".into(),
            })?
            .to_string();
        let attached = self
            .cdp
            .call(
                None,
                "Target.attachToTarget",
                json!({"targetId": target, "flatten": true}),
            )
            .await?;
        let session = attached["sessionId"]
            .as_str()
            .ok_or_else(|| BrowserError::Command {
                method: "Target.attachToTarget".into(),
                message: "no sessionId in the answer".into(),
            })?
            .to_string();
        Ok(Page { session, target })
    }

    /// Asks the browser to quit, kills it when it lingers, and removes its profile.
    pub async fn close(mut self) {
        let _ = tokio::time::timeout(
            Duration::from_secs(3),
            self.cdp.call(None, "Browser.close", json!({})),
        )
        .await;
        if tokio::time::timeout(CLOSE_TIMEOUT, self.child.wait())
            .await
            .is_err()
        {
            let _ = self.child.kill().await;
            let _ = self.child.wait().await;
        }
        let _ = tokio::fs::remove_dir_all(&self.profile).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dir(tag: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("discoclip-browser-{tag}-{}", uuid::Uuid::now_v7()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn the_executable_is_the_configured_one_or_the_first_usual_one() {
        let path_dir = dir("path");
        let place_dir = dir("place");
        let on_path = std::slice::from_ref(&path_dir);
        let nothing = BrowserConfig::default();
        let missing = locate_among(&nothing, on_path, &[place_dir.join("chrome")]);
        assert!(
            matches!(&missing, Err(BrowserError::NotFound(reason)) if reason.contains("engine.browser.executable")),
            "{missing:?}"
        );

        std::fs::write(place_dir.join("chrome"), b"").unwrap();
        assert_eq!(
            locate_among(&nothing, on_path, &[place_dir.join("chrome")]).unwrap(),
            place_dir.join("chrome")
        );
        std::fs::write(path_dir.join("google-chrome"), b"").unwrap();
        std::fs::write(path_dir.join("chromium"), b"").unwrap();
        assert_eq!(
            locate_among(&nothing, on_path, &[place_dir.join("chrome")]).unwrap(),
            path_dir.join("chromium")
        );

        let configured = BrowserConfig {
            executable: Some(place_dir.join("chrome")),
            args: vec![],
        };
        assert_eq!(
            locate_among(&configured, on_path, &[]).unwrap(),
            place_dir.join("chrome")
        );
        let wrong = BrowserConfig {
            executable: Some(place_dir.join("nowhere")),
            args: vec![],
        };
        let refused = locate_among(&wrong, on_path, &[]);
        assert!(
            matches!(&refused, Err(BrowserError::NotFound(reason)) if reason.contains("nowhere")),
            "{refused:?}"
        );
        let _ = std::fs::remove_dir_all(&path_dir);
        let _ = std::fs::remove_dir_all(&place_dir);
    }

    #[test]
    fn the_devtools_address_is_read_from_the_log_line() {
        assert_eq!(
            devtools_endpoint(
                "DevTools listening on ws://127.0.0.1:41095/devtools/browser/31275a4f-2592-4343"
            )
            .as_deref(),
            Some("ws://127.0.0.1:41095/devtools/browser/31275a4f-2592-4343")
        );
        assert_eq!(
            devtools_endpoint("[1:1:0924/034041:ERROR:x] something"),
            None
        );
        assert_eq!(devtools_endpoint("DevTools listening on http://x"), None);
    }

    #[test]
    fn proxies_become_flags_with_their_credentials_set_aside() {
        let plain = Url::parse("http://proxy.test:3128").unwrap();
        assert_eq!(proxy_flag(&plain), ("http://proxy.test:3128".into(), None));
        let socks = Url::parse("socks5h://user:p%40ss@10.0.0.1:1080").unwrap();
        assert_eq!(
            proxy_flag(&socks),
            (
                "socks5://10.0.0.1:1080".into(),
                Some(("user".into(), "p@ss".into()))
            )
        );
    }

    #[test]
    fn headless_versions_read_as_the_plain_browser() {
        let version = Version {
            product: "HeadlessChrome/153.0.8010.36".into(),
            user_agent: "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) HeadlessChrome/153.0.8010.36 Safari/537.36".into(),
        };
        assert_eq!(
            version.plain_user_agent(),
            "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/153.0.8010.36 Safari/537.36"
        );
        assert_eq!(version.numbers(), ("153.0.8010.36".into(), "153".into()));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_browser_that_exits_is_reported_with_its_status_and_output() {
        use std::os::unix::fs::PermissionsExt;
        let executable = dir("fake").join("chromium");
        std::fs::write(
            &executable,
            b"#!/bin/sh\nprintf 'no bus \\377 here\\n' >&2\nexit 3\n",
        )
        .unwrap();
        std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o755)).unwrap();
        let profile = dir("profile");
        let message = match Browser::launch(&executable, &[], &profile, None).await {
            Ok(_) => panic!("a browser that exits at once was launched"),
            Err(error) => error.to_string(),
        };
        assert!(
            message.contains("exited before it was ready (exit status: 3)"),
            "{message}"
        );
        assert!(message.contains("no bus \u{FFFD} here"), "{message}");
        assert!(!profile.exists());
    }

    #[tokio::test]
    async fn a_browser_is_launched_spoken_to_and_closed() {
        let executable = locate(&BrowserConfig::default()).unwrap();
        let profile = dir("profile");
        let mut browser = Browser::launch(&executable, &[], &profile, None)
            .await
            .unwrap();
        assert!(
            browser.version().product.contains("Chrome/"),
            "{:?}",
            browser.version()
        );
        let mut events = browser.take_events().unwrap();
        let page = browser.open_page().await.unwrap();
        browser
            .cdp()
            .call(Some(&page.session), "Runtime.enable", json!({}))
            .await
            .unwrap();
        let answer = browser
            .cdp()
            .call(
                Some(&page.session),
                "Runtime.evaluate",
                json!({"expression": "6 * 7", "returnByValue": true}),
            )
            .await
            .unwrap();
        assert_eq!(answer["result"]["value"], 42);
        let refused = browser
            .cdp()
            .call(Some(&page.session), "Runtime.noSuchMethod", json!({}))
            .await
            .unwrap_err();
        assert!(
            matches!(&refused, BrowserError::Command { method, .. } if method == "Runtime.noSuchMethod"),
            "{refused:?}"
        );
        let context_created = tokio::time::timeout(Duration::from_secs(10), async {
            while let Some(event) = events.recv().await {
                if event.method == "Runtime.executionContextCreated" {
                    return true;
                }
            }
            false
        })
        .await
        .unwrap();
        assert!(context_created);
        let printed = version(&executable).await.unwrap();
        assert!(printed.contains("Chrom"), "{printed}");
        browser.close().await;
        assert!(!profile.exists());
    }
}
