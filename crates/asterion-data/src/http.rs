use std::time::Duration;

use tokio::time::{Instant, sleep};

use crate::error::DataError;
use crate::redact::redact_secret;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetryPolicy {
    pub min_interval: Duration,
    // Total tries for network errors, 5xx and 429. Other 4xx are never retried.
    pub max_attempts: u32,
    pub initial_backoff: Duration,
    pub timeout: Duration,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        RetryPolicy {
            min_interval: Duration::from_millis(500),
            max_attempts: 3,
            initial_backoff: Duration::from_secs(1),
            timeout: Duration::from_secs(30),
        }
    }
}

// Longest wait taken from a server-sent `x-ratelimit-reset`.
const MAX_RESET_WAIT: Duration = Duration::from_secs(60);

/// Outcome of inspecting a 2xx body in `Http::send_with`.
#[derive(Debug)]
pub(crate) enum BodyVerdict {
    Accept,
    Retry(String),
    Fatal(DataError),
}

fn reset_after(resp: &reqwest::Response) -> Option<Duration> {
    let secs: u64 = resp
        .headers()
        .get("x-ratelimit-reset")?
        .to_str()
        .ok()?
        .trim()
        .parse()
        .ok()?;
    Some(Duration::from_secs(secs).min(MAX_RESET_WAIT))
}

// Throttled GET with bounded retries; error text never contains the secret.
pub(crate) struct Http {
    client: reqwest::Client,
    secret: String,
    policy: RetryPolicy,
    last_sent: Option<Instant>,
    requests_sent: u32,
}

impl std::fmt::Debug for Http {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Http")
            .field("policy", &self.policy)
            .field("requests_sent", &self.requests_sent)
            .finish_non_exhaustive()
    }
}

impl Http {
    pub(crate) fn new(secret: &str, policy: RetryPolicy) -> Result<Self, DataError> {
        Self::build(secret, policy, false)
    }

    // For servers that only offer TLS 1.2 with RSA key exchange, which rustls
    // refuses by design; the OS TLS stack still verifies the certificate chain.
    pub(crate) fn with_native_tls(secret: &str, policy: RetryPolicy) -> Result<Self, DataError> {
        Self::build(secret, policy, true)
    }

    fn build(secret: &str, policy: RetryPolicy, native_tls: bool) -> Result<Self, DataError> {
        // OpenDART answers requests without a User-Agent with a redirect to an error page.
        let mut builder = reqwest::Client::builder()
            .timeout(policy.timeout)
            .user_agent("rust-http-client/1.0");
        if native_tls {
            builder = builder.tls_backend_native();
        }
        let client = builder.build().map_err(|e| DataError::Network {
            attempts: 0,
            detail: redact_secret(&e.without_url().to_string(), secret),
        })?;
        Ok(Http {
            client,
            secret: secret.to_owned(),
            policy,
            last_sent: None,
            requests_sent: 0,
        })
    }

    pub(crate) fn requests_sent(&self) -> u32 {
        self.requests_sent
    }

    pub(crate) async fn get(
        &mut self,
        url: &str,
        query: &[(&str, &str)],
    ) -> Result<String, DataError> {
        self.send(|c| c.get(url).query(query)).await
    }

    // Same throttle and retry rules for any request the closure builds.
    pub(crate) async fn send(
        &mut self,
        make: impl Fn(&reqwest::Client) -> reqwest::RequestBuilder,
    ) -> Result<String, DataError> {
        self.send_with(make, |_| BodyVerdict::Accept).await
    }

    // Like `send`, but `classify` can reject a 2xx body as retryable or fatal.
    // Retry waits honour `x-ratelimit-reset` (seconds) when the server sends it.
    pub(crate) async fn send_with(
        &mut self,
        make: impl Fn(&reqwest::Client) -> reqwest::RequestBuilder,
        classify: impl Fn(&str) -> BodyVerdict,
    ) -> Result<String, DataError> {
        let max_attempts = self.policy.max_attempts.max(1);
        let mut backoff = self.policy.initial_backoff;
        let mut attempt = 0;
        loop {
            attempt += 1;
            self.throttle().await;
            self.requests_sent += 1;

            let result = make(&self.client).send().await;
            let mut reset = None;
            let failure = match result {
                Ok(resp) if resp.status().is_success() => {
                    reset = reset_after(&resp);
                    let body = resp.text().await.map_err(|e| self.network(attempt, e))?;
                    match classify(&body) {
                        BodyVerdict::Accept => return Ok(body),
                        BodyVerdict::Fatal(e) => return Err(e),
                        BodyVerdict::Retry(message) => DataError::Busy {
                            attempts: attempt,
                            message,
                        },
                    }
                }
                Ok(resp) if resp.status().is_server_error() || resp.status().as_u16() == 429 => {
                    reset = reset_after(&resp);
                    DataError::HttpStatus {
                        status: resp.status().as_u16(),
                    }
                }
                Ok(resp) => {
                    // Other 4xx fail fast; only a 422 whose body `classify` marks
                    // retryable (Arctic Shift's overload reply) is retried.
                    let status = resp.status().as_u16();
                    reset = reset_after(&resp);
                    let body = resp.text().await.unwrap_or_default();
                    match classify(&body) {
                        BodyVerdict::Retry(message) if status == 422 => DataError::Busy {
                            attempts: attempt,
                            message,
                        },
                        BodyVerdict::Fatal(e) => return Err(e),
                        _ => return Err(DataError::HttpStatus { status }),
                    }
                }
                Err(e) => self.network(attempt, e),
            };

            if attempt >= max_attempts {
                return Err(failure);
            }
            sleep(reset.map_or(backoff, |r| r.max(backoff))).await;
            backoff = backoff.saturating_mul(2);
        }
    }

    fn network(&self, attempts: u32, e: reqwest::Error) -> DataError {
        DataError::Network {
            attempts,
            detail: redact_secret(&e.without_url().to_string(), &self.secret),
        }
    }

    async fn throttle(&mut self) {
        if let Some(last) = self.last_sent {
            let elapsed = last.elapsed();
            if elapsed < self.policy.min_interval {
                sleep(self.policy.min_interval - elapsed).await;
            }
        }
        self.last_sent = Some(Instant::now());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::method;
    use wiremock::{Mock, MockServer, ResponseTemplate};

    const BUSY: &str = r#"{"data":null,"error":"Timeout. Maybe slow down a bit"}"#;
    const OK: &str = r#"{"data":[]}"#;

    fn policy(max_attempts: u32) -> RetryPolicy {
        RetryPolicy {
            min_interval: Duration::ZERO,
            max_attempts,
            initial_backoff: Duration::from_millis(1),
            timeout: Duration::from_secs(5),
        }
    }

    fn busy_check(body: &str) -> BodyVerdict {
        if body.contains("\"data\":null") {
            BodyVerdict::Retry(body.to_owned())
        } else {
            BodyVerdict::Accept
        }
    }

    #[tokio::test]
    async fn retry_verdict_retries_then_succeeds() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200).set_body_string(BUSY))
            .up_to_n_times(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200).set_body_string(OK))
            .mount(&server)
            .await;
        let mut http = Http::new("", policy(3)).expect("client");
        let url = server.uri();
        let body = http
            .send_with(|c| c.get(&url), busy_check)
            .await
            .expect("body");
        assert_eq!(body, OK);
        assert_eq!(http.requests_sent(), 2);
    }

    // Arctic Shift sends its overload message with HTTP 422.
    #[tokio::test]
    async fn retry_verdict_on_4xx_body_is_retried() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(422).set_body_string(BUSY))
            .up_to_n_times(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200).set_body_string(OK))
            .mount(&server)
            .await;
        let mut http = Http::new("", policy(3)).expect("client");
        let url = server.uri();
        let body = http
            .send_with(|c| c.get(&url), busy_check)
            .await
            .expect("body");
        assert_eq!(body, OK);
        assert_eq!(http.requests_sent(), 2);
    }

    #[tokio::test]
    async fn plain_send_still_fails_fast_on_4xx() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(422).set_body_string(BUSY))
            .mount(&server)
            .await;
        let mut http = Http::new("", policy(3)).expect("client");
        let url = server.uri();
        let err = http.send(|c| c.get(&url)).await.expect_err("422");
        assert!(
            matches!(err, DataError::HttpStatus { status: 422 }),
            "{err:?}"
        );
        assert_eq!(http.requests_sent(), 1);
    }

    #[tokio::test]
    async fn retry_verdict_exhausted_is_busy() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200).set_body_string(BUSY))
            .mount(&server)
            .await;
        let mut http = Http::new("", policy(2)).expect("client");
        let url = server.uri();
        let err = http
            .send_with(|c| c.get(&url), busy_check)
            .await
            .expect_err("busy");
        assert!(
            matches!(err, DataError::Busy { attempts: 2, .. }),
            "{err:?}"
        );
        assert!(!err.is_fatal_for_batch());
    }

    #[tokio::test]
    async fn http_400_is_never_retried_even_if_body_looks_busy() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(
                ResponseTemplate::new(400)
                    .set_body_string(r#"{"data":null,"error":"something new"}"#),
            )
            .mount(&server)
            .await;
        let mut http = Http::new("", policy(3)).expect("client");
        let url = server.uri();
        let err = http
            .send_with(|c| c.get(&url), busy_check)
            .await
            .expect_err("400");
        assert!(
            matches!(err, DataError::HttpStatus { status: 400 }),
            "{err:?}"
        );
        assert_eq!(http.requests_sent(), 1);
    }

    #[tokio::test]
    async fn fatal_verdict_on_4xx_keeps_message() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(400).set_body_string("bad field"))
            .mount(&server)
            .await;
        let mut http = Http::new("", policy(3)).expect("client");
        let url = server.uri();
        let err = http
            .send_with(
                |c| c.get(&url),
                |b| {
                    BodyVerdict::Fatal(DataError::Api {
                        code: "x".into(),
                        message: b.to_owned(),
                    })
                },
            )
            .await
            .expect_err("fatal");
        assert!(
            matches!(&err, DataError::Api { message, .. } if message == "bad field"),
            "{err:?}"
        );
        assert_eq!(http.requests_sent(), 1);
    }

    #[tokio::test]
    async fn fatal_verdict_is_not_retried() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200).set_body_string("bad"))
            .mount(&server)
            .await;
        let mut http = Http::new("", policy(3)).expect("client");
        let url = server.uri();
        let err = http
            .send_with(
                |c| c.get(&url),
                |b| {
                    BodyVerdict::Fatal(DataError::Api {
                        code: "x".into(),
                        message: b.to_owned(),
                    })
                },
            )
            .await
            .expect_err("fatal");
        assert!(matches!(err, DataError::Api { .. }));
        assert_eq!(http.requests_sent(), 1);
    }

    #[tokio::test]
    async fn ratelimit_reset_header_extends_wait() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(429).insert_header("x-ratelimit-reset", "1"))
            .up_to_n_times(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200).set_body_string(OK))
            .mount(&server)
            .await;
        let mut http = Http::new("", policy(3)).expect("client");
        let url = server.uri();
        let start = Instant::now();
        let body = http
            .send_with(|c| c.get(&url), busy_check)
            .await
            .expect("body");
        assert_eq!(body, OK);
        assert!(
            start.elapsed() >= Duration::from_millis(900),
            "{:?}",
            start.elapsed()
        );
    }
}
