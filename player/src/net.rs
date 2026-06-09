//! HTTP transport + interceptor primitives consumed by `Player` and exported
//! to downstream clients (see PLAYER_INTEGRATION.md §3).
//!
//! Two layers:
//!   1. `HttpClient` — single owner of `reqwest::Client`, applied to every
//!      manifest / segment / license fetch. Wraps each request with the
//!      configured `RequestInterceptor` (default: `NoopInterceptor`) and
//!      `RetryPolicy` (default: 3 attempts, exponential backoff with jitter).
//!   2. Public traits / structs (`RequestInterceptor`, `LicenseResolver`,
//!      `PreparedRequest`, `RequestKind`, `RetryPolicy`, `BoxError`) that
//!      downstream consumers implement to inject auth headers, rewrite URLs,
//!      or resolve KIDs to keys without putting any provider‑specific code
//!      into the player crate.

use std::error::Error;
use std::sync::Arc;
use std::time::{Duration, Instant};

use arc_swap::ArcSwap;
use async_trait::async_trait;
use bytes::Bytes;
use rand::Rng;
use reqwest::{header::RANGE, Client, Method, StatusCode};

pub type BoxError = Box<dyn Error + Send + Sync>;

/// Tag attached to every outgoing request so interceptors can branch on
/// purpose (e.g. add bearer only to manifest, rewrite only to segments).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RequestKind {
    /// DASH MPD document.
    Manifest,
    /// MP4 init segment (moov + sidx, no media data).
    InitSegment,
    /// MP4 media segment (a few seconds of A/V).
    Segment,
    /// ClearKey / Widevine license POST.
    License,
}

/// What the interceptor wants `HttpClient` to actually send.
#[derive(Default, Debug, Clone)]
pub struct PreparedRequest {
    /// Final URL to fetch. Interceptor may rewrite this completely.
    pub url: String,
    /// Headers to ADD (existing client defaults are not removed).
    pub headers: Vec<(String, String)>,
    /// Optional method override (defaults: GET for everything except
    /// License = POST).
    pub method: Option<Method>,
    /// Optional body substitution (for License only).
    pub body: Option<Bytes>,
}

/// Implemented by downstream consumers to add auth headers, rewrite
/// pseudo‑URI segment URLs, transform license bodies, etc. The player
/// crate itself ships only `NoopInterceptor` so the `app/` example
/// keeps working without any consumer code.
#[async_trait]
pub trait RequestInterceptor: Send + Sync + 'static {
    /// Called once per outgoing request, BEFORE it is sent. Returning
    /// `Err` aborts the request — the original caller sees an
    /// `Error{kind: Interceptor, ...}`.
    async fn intercept(
        &self,
        url: String,
        kind: RequestKind,
    ) -> Result<PreparedRequest, BoxError>;
}

/// Pass URL through, no headers added. The crate-wide default.
pub struct NoopInterceptor;

#[async_trait]
impl RequestInterceptor for NoopInterceptor {
    async fn intercept(
        &self,
        url: String,
        _kind: RequestKind,
    ) -> Result<PreparedRequest, BoxError> {
        Ok(PreparedRequest {
            url,
            ..Default::default()
        })
    }
}

/// Async ClearKey lookup. The player caches every successful
/// `(kid → key)` for the rest of the session. If the key is permanently
/// unavailable, return `Err` — the player treats this as a fatal stream
/// error (`PlayerErrorKind::LicenseResolver`).
#[async_trait]
pub trait LicenseResolver: Send + Sync + 'static {
    async fn resolve(&self, kid: [u8; 16]) -> Result<[u8; 16], BoxError>;
}

/// Retry behaviour for transient network / 5xx failures. See
/// PLAYER_INTEGRATION.md §7.1.
#[derive(Clone, Copy, Debug)]
pub struct RetryPolicy {
    /// Total attempts including the first try.
    pub max_attempts: u32,
    pub initial_delay: Duration,
    pub max_delay: Duration,
    pub multiplier: f32,
    /// Relative jitter band: `0.2` = ±20% around the computed delay.
    pub jitter: f32,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            max_attempts: 3,
            initial_delay: Duration::from_millis(250),
            max_delay: Duration::from_secs(4),
            multiplier: 2.0,
            jitter: 0.2,
        }
    }
}

/// HTTP status codes that we retry. Everything else (401, 403, 404, …)
/// is surfaced after the first failure.
fn is_retryable_status(s: StatusCode) -> bool {
    matches!(
        s.as_u16(),
        408 | 425 | 429 | 500 | 502 | 503 | 504
    )
}

/// Outcome bubbled from one HTTP attempt — distinguishes "retry me" from
/// "this is final, stop trying".
enum Attempt {
    Ok(Bytes),
    Retry(BoxError),
    Fatal(BoxError),
}

/// Centralised HTTP entry point. Single owner of `reqwest::Client` so
/// connection pools are shared across manifest / segment / license fetches.
pub struct HttpClient {
    client: Client,
    interceptor: ArcSwap<Box<dyn RequestInterceptor>>,
    retry: ArcSwap<RetryPolicy>,
    callback_timeout: ArcSwap<Duration>,
}

impl HttpClient {
    pub fn new() -> Self {
        Self {
            client: Client::new(),
            interceptor: ArcSwap::from_pointee(
                Box::new(NoopInterceptor) as Box<dyn RequestInterceptor>
            ),
            retry: ArcSwap::from_pointee(RetryPolicy::default()),
            callback_timeout: ArcSwap::from_pointee(Duration::from_secs(10)),
        }
    }

    /// Replace the active interceptor. Safe to call at any time; subsequent
    /// requests use the new one, in‑flight requests keep the old one.
    pub fn set_interceptor(&self, interceptor: Arc<dyn RequestInterceptor>) {
        // Re-box so we own the trait object behind ArcSwap.
        let boxed: Box<dyn RequestInterceptor> = Box::new(InterceptorHandle(interceptor));
        self.interceptor.store(Arc::new(boxed));
    }

    pub fn set_retry_policy(&self, policy: RetryPolicy) {
        self.retry.store(Arc::new(policy));
    }

    pub fn set_callback_timeout(&self, timeout: Duration) {
        self.callback_timeout.store(Arc::new(timeout));
    }

    pub async fn get(&self, url: String, kind: RequestKind) -> Result<Bytes, BoxError> {
        self.dispatch(url, kind, None, None).await
    }

    /// HTTP byte range request, e.g. `bytes=START-END` for an MP4 sidx slice.
    pub async fn get_range(
        &self,
        url: String,
        kind: RequestKind,
        start: u64,
        end: u64,
    ) -> Result<Bytes, BoxError> {
        let range = format!("bytes={}-{}", start, end);
        self.dispatch(url, kind, None, Some(range)).await
    }

    pub async fn get_text(
        &self,
        url: String,
        kind: RequestKind,
    ) -> Result<String, BoxError> {
        let bytes = self.dispatch(url, kind, None, None).await?;
        String::from_utf8(bytes.to_vec())
            .map_err(|e| -> BoxError { format!("response not utf-8: {}", e).into() })
    }

    pub async fn post(
        &self,
        url: String,
        kind: RequestKind,
        body: Bytes,
        content_type: &str,
    ) -> Result<Bytes, BoxError> {
        self.dispatch(
            url,
            kind,
            Some((Method::POST, body, content_type.to_string())),
            None,
        )
        .await
    }

    /// Shared inner dispatch. Runs the interceptor (with timeout), then
    /// retries the actual HTTP call per the configured `RetryPolicy`.
    async fn dispatch(
        &self,
        url: String,
        kind: RequestKind,
        post_body: Option<(Method, Bytes, String)>,
        range: Option<String>,
    ) -> Result<Bytes, BoxError> {
        let interceptor = self.interceptor.load_full();
        let timeout = **self.callback_timeout.load();
        let prep = match tokio::time::timeout(timeout, interceptor.intercept(url, kind)).await {
            Ok(Ok(p)) => p,
            Ok(Err(e)) => return Err(format!("interceptor: {}", e).into()),
            Err(_) => return Err(format!("interceptor timeout ({}ms)", timeout.as_millis()).into()),
        };

        let policy = **self.retry.load();
        let mut delay = policy.initial_delay;
        let mut last_err: Option<BoxError> = None;

        for attempt in 0..policy.max_attempts {
            let outcome = self
                .send_one(&prep, &post_body, range.as_deref(), kind)
                .await;
            match outcome {
                Attempt::Ok(b) => return Ok(b),
                Attempt::Fatal(e) => return Err(e),
                Attempt::Retry(e) => {
                    last_err = Some(e);
                    if attempt + 1 < policy.max_attempts {
                        let backoff = jittered(delay, policy.jitter);
                        tokio::time::sleep(backoff).await;
                        delay = scale_delay(delay, policy.multiplier, policy.max_delay);
                    }
                }
            }
        }

        Err(last_err.unwrap_or_else(|| "exhausted retries".into()))
    }

    async fn send_one(
        &self,
        prep: &PreparedRequest,
        post_body: &Option<(Method, Bytes, String)>,
        range: Option<&str>,
        _kind: RequestKind,
    ) -> Attempt {
        // Method precedence: interceptor override → caller-provided POST →
        // GET default.
        let method = prep
            .method
            .clone()
            .or_else(|| post_body.as_ref().map(|(m, _, _)| m.clone()))
            .unwrap_or(Method::GET);

        let mut req = self.client.request(method, &prep.url);

        for (k, v) in &prep.headers {
            req = req.header(k, v);
        }
        if let Some(r) = range {
            req = req.header(RANGE, r);
        }
        // Body precedence: interceptor body → caller POST body. License
        // interceptors typically REPLACE the caller body with a different
        // JSON envelope.
        if let Some(b) = prep.body.clone() {
            req = req.body(b);
        } else if let Some((_, b, ct)) = post_body {
            req = req.header(reqwest::header::CONTENT_TYPE, ct);
            req = req.body(b.clone());
        }

        match req.send().await {
            Ok(resp) => {
                let status = resp.status();
                if status.is_success() {
                    match resp.bytes().await {
                        Ok(b) => Attempt::Ok(b),
                        Err(e) => Attempt::Retry(format!("body read: {}", e).into()),
                    }
                } else if is_retryable_status(status) {
                    Attempt::Retry(format!("http {}", status.as_u16()).into())
                } else {
                    Attempt::Fatal(format!("http {}", status.as_u16()).into())
                }
            }
            Err(e) => {
                // reqwest::Error covers DNS, TCP, TLS, timeout — all
                // transient by definition.
                Attempt::Retry(format!("{}", e).into())
            }
        }
    }
}

impl Default for HttpClient {
    fn default() -> Self {
        Self::new()
    }
}

/// `Arc<dyn>` storage adapter: `ArcSwap<Box<T>>` wants `Box`, not `Arc`,
/// so wrap the consumer's `Arc<dyn>` in a forwarder.
struct InterceptorHandle(Arc<dyn RequestInterceptor>);

#[async_trait]
impl RequestInterceptor for InterceptorHandle {
    async fn intercept(
        &self,
        url: String,
        kind: RequestKind,
    ) -> Result<PreparedRequest, BoxError> {
        self.0.intercept(url, kind).await
    }
}

fn jittered(base: Duration, jitter: f32) -> Duration {
    if jitter <= 0.0 {
        return base;
    }
    let j = jitter.clamp(0.0, 1.0) as f64;
    let factor = 1.0 + (rand::thread_rng().gen::<f64>() * 2.0 - 1.0) * j;
    let ns = (base.as_nanos() as f64 * factor).max(0.0) as u128;
    Duration::from_nanos(ns.min(u64::MAX as u128) as u64)
}

fn scale_delay(current: Duration, multiplier: f32, cap: Duration) -> Duration {
    let next = (current.as_secs_f32() * multiplier).clamp(0.0, cap.as_secs_f32());
    Duration::from_secs_f32(next)
}

// Suppress unused-import warnings on platforms where Instant isn't needed.
#[allow(dead_code)]
fn _instant_marker(_: Instant) {}
