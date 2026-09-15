//! The public NG Guard protocol used by the site's browser client: fetch a challenge,
//! hash the decoded payload followed by `:<decimal nonce>`, submit the proof, then retry
//! with the clearance cookie. No account or browser process is needed for this flow.

use std::time::{Duration, Instant};

use argon2::{Argon2, Block, Params, Version};
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;
use url::Url;

use super::{MAX_PAGE, PLATFORM, SITE, is_guard};
use crate::http::{BROWSER_UA, Http, Response};
use crate::resolve::{Fetched, ResolveError, fetch};

const SOLVE_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Default)]
pub(super) struct Guard {
    // Concurrent jobs share the platform's cookie jar. Complete one clearance exchange
    // before another job checks whether that same session still needs a challenge.
    session: Mutex<()>,
}

impl Guard {
    pub(super) async fn fetch(
        &self,
        http: &Http,
        url: &Url,
        headers: &[(String, String)],
    ) -> Result<Fetched, ResolveError> {
        let _session = self.session.lock().await;
        let answer = fetch(http, url, PLATFORM, BROWSER_UA, headers, MAX_PAGE).await?;
        if !matches!(answer.status.as_u16(), 200..=299 | 403) || !is_guard(&answer.text()) {
            return Ok(answer);
        }

        let challenge_url = Url::parse(&format!("{SITE}_guard/api/v1/challenge")).expect("valid");
        let response = http
            .get(challenge_url)
            .platform(PLATFORM)
            .user_agent(BROWSER_UA)
            .header("content-type", "application/json")
            .header("referer", url.as_str())
            .send()
            .await?;
        check_status(&response, url, "challenge")?;
        let challenge: Challenge = response.json(64 * 1024).await?;
        let cancelled = CancellationToken::new();
        let _cancel_on_drop = cancelled.clone().drop_guard();
        let solution =
            tokio::task::spawn_blocking(move || challenge.solve(SOLVE_TIMEOUT, &cancelled))
                .await
                .map_err(|e| ResolveError::unavailable(url, format!("NG Guard solver: {e}")))?
                .map_err(|e| ResolveError::unavailable(url, format!("NG Guard: {e}")))?;

        let verify_url = Url::parse(&format!("{SITE}_guard/api/v1/verify")).expect("valid");
        let response = http
            .post(verify_url)
            .platform(PLATFORM)
            .user_agent(BROWSER_UA)
            .header("origin", SITE.trim_end_matches('/'))
            .header("referer", url.as_str())
            .json(&solution)
            .send()
            .await?;
        check_status(&response, url, "verification")?;
        if response.json::<serde_json::Value>(64 * 1024).await?["ok"] != true {
            return Err(ResolveError::unavailable(
                url,
                "NG Guard rejected the proof of work",
            ));
        }

        // Http stores Set-Cookie in the Newgrounds jar, including any existing login.
        // Retry only once so an unsuccessful clearance cannot loop indefinitely.
        let answer = fetch(http, url, PLATFORM, BROWSER_UA, headers, MAX_PAGE).await?;
        if is_guard(&answer.text()) {
            return Err(ResolveError::unavailable(
                url,
                "NG Guard still requests a browser challenge after accepting the proof of work",
            ));
        }
        Ok(answer)
    }
}

fn check_status(response: &Response, url: &Url, step: &str) -> Result<(), ResolveError> {
    if response.status.as_u16() == 429 {
        return Err(ResolveError::RateLimited(url.clone()));
    }
    if !response.status.is_success() {
        return Err(ResolveError::unavailable(
            url,
            format!("NG Guard {step} answered HTTP {}", response.status.as_u16()),
        ));
    }
    Ok(())
}

#[derive(Clone, Copy, Default, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
enum Algorithm {
    #[default]
    Sha256,
    Argon2id,
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct Parameters {
    hash_length: usize,
    iterations: u32,
    memory_size: u32,
    parallelism: u32,
}

#[derive(Clone, Deserialize, Serialize)]
struct Challenge {
    bits: u32,
    payload: String,
    sig: String,
    #[serde(default)]
    algo: Algorithm,
    #[serde(skip_serializing_if = "Option::is_none")]
    params: Option<Parameters>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Solution {
    #[serde(flatten)]
    challenge: Challenge,
    nonce: String,
    solve_time_ms: u64,
    demo: bool,
}

impl Challenge {
    fn solve(self, timeout: Duration, cancelled: &CancellationToken) -> Result<Solution, String> {
        if self.payload.len() > 8192 || self.bits > 256 {
            return Err("challenge exceeds the solver's limits".into());
        }
        let mut input = URL_SAFE_NO_PAD
            .decode(self.payload.trim_end_matches('='))
            .map_err(|_| "challenge payload is not base64url")?;
        input.push(b':');
        let prefix_len = input.len();
        let (argon, mut digest) = match self.algo {
            Algorithm::Sha256 => (None, vec![0; 32]),
            Algorithm::Argon2id => {
                let p = self.params.as_ref().ok_or("missing Argon2id parameters")?;
                // Parameters come from the network; bound each hash as well as the loop.
                if p.memory_size > 64 * 1024
                    || p.iterations > 4
                    || p.parallelism > 4
                    || !(4..=64).contains(&p.hash_length)
                    || self.bits > (p.hash_length * 8) as u32
                {
                    return Err("Argon2id challenge exceeds the solver's limits".into());
                }
                let params = Params::new(
                    p.memory_size,
                    p.iterations,
                    p.parallelism,
                    Some(p.hash_length),
                )
                .map_err(|e| format!("invalid Argon2id parameters: {e}"))?;
                (
                    Some(Argon2::new(
                        argon2::Algorithm::Argon2id,
                        Version::V0x13,
                        params,
                    )),
                    vec![0; p.hash_length],
                )
            }
        };
        let mut memory = argon
            .as_ref()
            .map(|a| vec![Block::default(); a.params().block_count()])
            .unwrap_or_default();
        let started = Instant::now();
        for nonce in 0_u64.. {
            if cancelled.is_cancelled() || started.elapsed() >= timeout {
                return Err("proof of work did not finish within its time limit".into());
            }
            let nonce = nonce.to_string();
            input.truncate(prefix_len);
            input.extend_from_slice(nonce.as_bytes());
            if let Some(argon) = &argon {
                // The site's client uses Argon2id v19 with an eight-byte zero salt.
                argon
                    .hash_password_into_with_memory(&input, &[0; 8], &mut digest, &mut memory)
                    .map_err(|e| format!("Argon2id: {e}"))?;
            } else {
                digest.copy_from_slice(&Sha256::digest(&input));
            }
            if leading_zero_bits(&digest) >= self.bits {
                return Ok(Solution {
                    challenge: self,
                    nonce,
                    solve_time_ms: started.elapsed().as_millis() as u64,
                    demo: false,
                });
            }
        }
        Err("proof of work exhausted its nonce range".into())
    }
}

fn leading_zero_bits(bytes: &[u8]) -> u32 {
    let mut bits = 0;
    for &byte in bytes {
        bits += byte.leading_zeros();
        if byte != 0 {
            break;
        }
    }
    bits
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex as StdMutex};

    use async_trait::async_trait;
    use serde_json::{Value, json};

    use super::*;
    use crate::http::{
        Exchange, Fixture, HttpConfig, HttpError, RecordedBody, RecordedRequest, RecordedResponse,
        ReplayTransport, Transport, TransportRequest, TransportResponse,
    };

    const PAGE: &str = "https://www.newgrounds.com/portal/view/1004201";
    const VIDEO: &str = "https://www.newgrounds.com/portal/video/1004201";
    const CHALLENGE: &str = "https://www.newgrounds.com/_guard/api/v1/challenge";
    const VERIFY: &str = "https://www.newgrounds.com/_guard/api/v1/verify";
    const GUARD: &str =
        "<html><title>NG Guard</title><script src=\"/_guard/assets/main.js\"></script></html>";

    fn challenge() -> Value {
        json!({"algo":"sha256","bits":9,"payload":"AP9OZXdncm91bmRzgA","sig":"fixture-signature"})
    }

    #[test]
    fn proofs_match_the_sites_browser_client_for_both_algorithms() {
        // Expected nonces computed independently by the site's public JS/WASM solver.
        // The decoded payload contains binary bytes and the work is not byte-aligned.
        for (algo, nonce) in [("sha256", "184"), ("argon2id", "377")] {
            let mut value = challenge();
            value["algo"] = json!(algo);
            if algo == "argon2id" {
                value["params"] =
                    json!({"hashLength":32,"iterations":1,"memorySize":32,"parallelism":1});
            }
            let challenge: Challenge = serde_json::from_value(value).unwrap();
            let solution = challenge
                .solve(SOLVE_TIMEOUT, &CancellationToken::new())
                .unwrap();
            assert_eq!(solution.nonce, nonce, "{algo}");
            let submitted = serde_json::to_value(solution).unwrap();
            assert_eq!(submitted["demo"], false);
            assert_eq!(submitted["sig"], "fixture-signature");
            assert!(submitted["solveTimeMs"].is_number());
        }
    }

    #[test]
    fn hostile_parameters_and_cancelled_work_are_bounded() {
        let mut value = challenge();
        value["algo"] = json!("argon2id");
        value["params"] =
            json!({"hashLength":32,"iterations":1,"memorySize":u32::MAX,"parallelism":1});
        let challenge: Challenge = serde_json::from_value(value).unwrap();
        assert!(
            challenge
                .solve(SOLVE_TIMEOUT, &CancellationToken::new())
                .err()
                .unwrap()
                .contains("limits")
        );
        let challenge: Challenge = serde_json::from_value(self::challenge()).unwrap();
        assert!(
            challenge
                .clone()
                .solve(Duration::ZERO, &CancellationToken::new())
                .is_err()
        );
        let cancelled = CancellationToken::new();
        cancelled.cancel();
        assert!(challenge.solve(SOLVE_TIMEOUT, &cancelled).is_err());
    }

    fn exchange(method: &str, url: &str, status: u16, body: &str) -> Exchange {
        Exchange {
            request: RecordedRequest {
                method: method.into(),
                url: url.into(),
                headers: vec![],
                body: None,
            },
            response: RecordedResponse {
                status,
                url: url.into(),
                headers: vec![],
                body: RecordedBody::Text(body.into()),
                truncated: false,
            },
        }
    }

    fn fixture(url: &str, guard_status: u16, after: &str) -> Fixture {
        let mut fixture = Fixture::new("newgrounds-guard", None);
        let mut verify = exchange("POST", VERIFY, 200, r#"{"ok":true}"#);
        verify.response.headers.push((
            "set-cookie".into(),
            "ng_clearance=accepted; Path=/; Secure; HttpOnly".into(),
        ));
        fixture.exchanges = vec![
            exchange("GET", url, guard_status, GUARD),
            exchange("GET", CHALLENGE, 200, &challenge().to_string()),
            verify,
            exchange("GET", url, 200, after),
        ];
        fixture
    }

    struct Observed {
        replay: ReplayTransport,
        requests: StdMutex<Vec<RecordedRequest>>,
    }

    #[async_trait]
    impl Transport for Observed {
        async fn send(&self, request: TransportRequest) -> Result<TransportResponse, HttpError> {
            self.requests.lock().unwrap().push(RecordedRequest {
                method: request.method.to_string(),
                url: request.url.to_string(),
                headers: request
                    .headers
                    .iter()
                    .map(|(k, v)| (k.to_string(), v.to_str().unwrap().to_string()))
                    .collect(),
                body: request.body.as_ref().map(|b| RecordedBody::from_bytes(b)),
            });
            self.replay.send(request).await
        }
        fn name(&self) -> &'static str {
            "observed-guard"
        }
    }

    #[tokio::test]
    async fn page_and_api_challenges_retry_with_clearance_and_preserve_login() {
        for (url, status, body) in [
            (PAGE, 403, "<html>Movie</html>"),
            (VIDEO, 200, r#"{"sources":{}}"#),
        ] {
            let mut fixture = fixture(url, status, body);
            fixture.exchanges.push(exchange(
                "GET",
                &format!("{SITE}portal/view/297383"),
                200,
                "other movie",
            ));
            let transport = Arc::new(Observed {
                replay: ReplayTransport::new(fixture),
                requests: StdMutex::new(vec![]),
            });
            let http = Http::with_transport(transport.clone(), HttpConfig::default());
            let url = Url::parse(url).unwrap();
            http.with_jar(PLATFORM, |jar| {
                jar.store_set_cookie("login=member; Path=/; Secure", &url)
            });
            let guard = Guard::default();
            let headers = vec![("x-requested-with".into(), "XMLHttpRequest".into())];
            assert_eq!(
                guard.fetch(&http, &url, &headers).await.unwrap().text(),
                body
            );
            let other = Url::parse(&format!("{SITE}portal/view/297383")).unwrap();
            assert_eq!(
                guard.fetch(&http, &other, &[]).await.unwrap().text(),
                "other movie"
            );

            let requests = transport.requests.lock().unwrap();
            assert_eq!(requests.len(), 5);
            let header = |i: usize, name: &str| {
                requests[i]
                    .headers
                    .iter()
                    .find(|(k, _)| k == name)
                    .unwrap()
                    .1
                    .clone()
            };
            for i in 0..requests.len() {
                assert_eq!(header(i, "user-agent"), BROWSER_UA);
                assert!(header(i, "cookie").contains("login=member"));
            }
            for i in [3, 4] {
                assert!(header(i, "cookie").contains("ng_clearance=accepted"));
            }
            assert_eq!(header(3, "x-requested-with"), "XMLHttpRequest");
            let proof: Value =
                serde_json::from_slice(&requests[2].body.as_ref().unwrap().to_bytes()).unwrap();
            assert_eq!(proof["nonce"], "184");
            assert_eq!(proof["payload"], challenge()["payload"]);
            assert_eq!(proof["demo"], false);
            assert!(proof.get("params").is_none());
            assert!(http.jar("youtube").is_empty());
            assert!(transport.replay.unused().is_empty());
        }
    }

    #[tokio::test]
    async fn rejected_proofs_and_repeated_guards_stop_without_looping() {
        for accepted in [false, true] {
            let mut fixture = fixture(PAGE, 403, GUARD);
            if !accepted {
                fixture.exchanges[2].response.body = RecordedBody::Text(r#"{"ok":false}"#.into());
            }
            let transport = Arc::new(Observed {
                replay: ReplayTransport::new(fixture),
                requests: StdMutex::new(vec![]),
            });
            let http = Http::with_transport(transport.clone(), HttpConfig::default());
            let url = Url::parse(PAGE).unwrap();
            let error = Guard::default()
                .fetch(&http, &url, &[])
                .await
                .err()
                .unwrap();
            assert!(matches!(error, ResolveError::Unavailable { .. }));
            assert_eq!(
                transport.requests.lock().unwrap().len(),
                if accepted { 4 } else { 3 }
            );
        }
    }

    #[tokio::test]
    async fn guard_rate_limits_remain_rate_limits() {
        let mut fixture = fixture(PAGE, 403, "movie");
        fixture.exchanges[1].response.status = 429;
        let url = Url::parse(PAGE).unwrap();
        let error = Guard::default()
            .fetch(&Http::replay(fixture), &url, &[])
            .await
            .err()
            .unwrap();
        assert!(matches!(error, ResolveError::RateLimited(_)));
    }
}
