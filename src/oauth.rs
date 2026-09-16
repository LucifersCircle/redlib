use crate::{
	client::{install_oauth_generation, record_oauth_send, CLIENT, OAUTH_CLIENT, OAUTH_IS_ROLLING_OVER},
	oauth_resources::ANDROID_APP_VERSION_LIST,
};
use base64::{engine::general_purpose, Engine as _};
use log::{error, info, trace, warn};
use serde_json::json;
use std::{collections::HashMap, fmt, sync::atomic::Ordering, sync::LazyLock, sync::Mutex, time::Duration, time::Instant, time::SystemTime};
use tegen::tegen::TextGenerator;
use tokio::sync::Notify;
use tokio::time::timeout;

const REDDIT_ANDROID_OAUTH_CLIENT_ID: &str = "ohXpoqrZYub1kg";

const AUTH_ENDPOINT: &str = "https://www.reddit.com";

const OAUTH_TIMEOUT: Duration = Duration::from_secs(5);
const INITIAL_REFRESH_RETRY_DELAY: Duration = Duration::from_secs(5);
const MAX_REFRESH_RETRY_DELAY: Duration = Duration::from_secs(300);
const MAX_SERVER_RETRY_DELAY: Duration = Duration::from_secs(600);
const TOKEN_REFRESH_EARLY_BY: u64 = 120;
static REFRESH_BACKOFF: LazyLock<Mutex<RefreshBackoff>> = LazyLock::new(|| Mutex::new(RefreshBackoff::default()));
static TOKEN_REFRESH_NOTIFY: LazyLock<Notify> = LazyLock::new(Notify::new);

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub(crate) enum RefreshReason {
	Scheduled,
	Unauthorized,
}

impl RefreshReason {
	fn label(self) -> &'static str {
		match self {
			Self::Scheduled => "scheduled",
			Self::Unauthorized => "unauthorized",
		}
	}
}

// Response from OAuth backend authentication
#[derive(Debug, Clone)]
pub struct OauthResponse {
	pub token: String,
	pub expires_in: u64,
	pub additional_headers: HashMap<String, String>,
}

// Trait for OAuth backend implementations
trait OauthBackend: Send + Sync {
	fn authenticate(&mut self) -> impl std::future::Future<Output = Result<OauthResponse, AuthError>> + Send;
	fn user_agent(&self) -> &str;
	fn get_headers(&self) -> HashMap<String, String>;
}

// OAuth backend implementations
#[derive(Debug, Clone)]
pub(crate) enum OauthBackendImpl {
	MobileSpoof(MobileSpoofAuth),
	GenericWeb(GenericWebAuth),
}

impl OauthBackend for OauthBackendImpl {
	async fn authenticate(&mut self) -> Result<OauthResponse, AuthError> {
		match self {
			OauthBackendImpl::MobileSpoof(backend) => backend.authenticate().await,
			OauthBackendImpl::GenericWeb(backend) => backend.authenticate().await,
		}
	}

	fn user_agent(&self) -> &str {
		match self {
			OauthBackendImpl::MobileSpoof(backend) => backend.user_agent(),
			OauthBackendImpl::GenericWeb(backend) => backend.user_agent(),
		}
	}

	fn get_headers(&self) -> HashMap<String, String> {
		match self {
			OauthBackendImpl::MobileSpoof(backend) => backend.get_headers(),
			OauthBackendImpl::GenericWeb(backend) => backend.get_headers(),
		}
	}
}

impl OauthBackendImpl {
	fn name(&self) -> &'static str {
		match self {
			Self::MobileSpoof(_) => "MobileSpoofAuth",
			Self::GenericWeb(_) => "GenericWebAuth",
		}
	}

	fn alternate(&self) -> Self {
		match self {
			Self::MobileSpoof(_) => Self::GenericWeb(GenericWebAuth::new()),
			Self::GenericWeb(_) => Self::MobileSpoof(MobileSpoofAuth::new()),
		}
	}
}

// Spoofed client for Android devices
#[derive(Debug, Clone)]
pub struct Oauth {
	pub(crate) headers_map: HashMap<String, String>,
	expires_in: u64,
	pub(crate) backend: OauthBackendImpl,
	pub(crate) generation: u64,
}

struct RefreshedOauth {
	oauth: Oauth,
	fresh_identity: bool,
}

impl Oauth {
	/// Create a new OAuth client
	pub(crate) async fn new() -> Self {
		// Keep both identities stable across startup retries. Startup cannot serve
		// requests without a token, so retry indefinitely with bounded backoff.
		let mut primary = OauthBackendImpl::MobileSpoof(MobileSpoofAuth::new());
		let mut fallback = OauthBackendImpl::GenericWeb(GenericWebAuth::new());
		let mut failure_count = 0_u32;

		loop {
			let mut retry_after = None;
			for backend in [&mut primary, &mut fallback] {
				match Self::authenticate_with_backend(backend).await {
					Ok(oauth) => {
						info!("[✅] Successfully created OAuth client with {}", backend.name());
						return oauth;
					}
					Err(error) => {
						retry_after = max_duration(retry_after, error.retry_after());
						error!("[⛔] Failed to create OAuth client with {}: {error}", backend.name());
					}
				}
			}

			failure_count = failure_count.saturating_add(1);
			let delay = refresh_retry_delay(failure_count, retry_after);
			warn!("[⏳] Both OAuth backends failed; retrying startup authentication in {delay:?}");
			tokio::time::sleep(delay).await;
		}
	}

	async fn authenticate_with_backend(backend: &mut OauthBackendImpl) -> Result<Self, AuthError> {
		let response = timeout(OAUTH_TIMEOUT, backend.authenticate()).await.map_err(|_| AuthError::Timeout)??;

		// Build headers_map from backend headers + Authorization header
		let mut headers_map = backend.get_headers();
		headers_map.insert("Authorization".to_owned(), format!("Bearer {}", response.token));
		headers_map.extend(response.additional_headers);

		Ok(Self {
			headers_map,
			expires_in: response.expires_in,
			backend: backend.clone(),
			generation: 0,
		})
	}

	fn refresh_backend(&self, reason: RefreshReason, fallback: bool) -> (OauthBackendImpl, bool) {
		match (reason, fallback) {
			(RefreshReason::Scheduled | RefreshReason::Unauthorized, false) => (self.backend.clone(), false),
			(RefreshReason::Scheduled | RefreshReason::Unauthorized, true) => (self.backend.alternate(), true),
		}
	}

	async fn refreshed(&self, reason: RefreshReason) -> Result<RefreshedOauth, RefreshError> {
		let (mut primary, primary_is_fresh) = self.refresh_backend(reason, false);
		let primary_name = primary.name();
		match Self::authenticate_with_backend(&mut primary).await {
			Ok(oauth) => Ok(RefreshedOauth {
				oauth,
				fresh_identity: primary_is_fresh,
			}),
			Err(primary_error) => {
				warn!("OAuth {} refresh with {primary_name} failed: {primary_error}", reason.label());
				let (mut fallback, fallback_is_fresh) = self.refresh_backend(reason, true);
				let fallback_name = fallback.name();
				match Self::authenticate_with_backend(&mut fallback).await {
					Ok(oauth) => Ok(RefreshedOauth {
						oauth,
						fresh_identity: fallback_is_fresh,
					}),
					Err(fallback_error) => Err(RefreshError {
						primary_name,
						primary_error,
						fallback_name,
						fallback_error,
					}),
				}
			}
		}
	}

	pub fn user_agent(&self) -> &str {
		self.backend.user_agent()
	}
}

#[derive(Debug)]
enum AuthError {
	Wreq(wreq::Error),
	SerdeDeserialize(serde_json::Error),
	Field(&'static str),
	HttpStatus { status: u16, retry_after: Option<Duration> },
	Timeout,
}

impl AuthError {
	fn retry_after(&self) -> Option<Duration> {
		match self {
			Self::HttpStatus { retry_after, .. } => *retry_after,
			_ => None,
		}
	}
}

impl fmt::Display for AuthError {
	fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
		match self {
			Self::Wreq(error) => write!(formatter, "request failed: {error}"),
			Self::SerdeDeserialize(error) => write!(formatter, "invalid response body: {error}"),
			Self::Field(field) => write!(formatter, "OAuth response is missing or has an invalid {field} field"),
			Self::HttpStatus { status, retry_after } => match retry_after {
				Some(delay) => write!(formatter, "HTTP {status} (Retry-After {delay:?})"),
				None => write!(formatter, "HTTP {status}"),
			},
			Self::Timeout => write!(formatter, "request timed out after {OAUTH_TIMEOUT:?}"),
		}
	}
}

#[derive(Debug)]
struct RefreshError {
	primary_name: &'static str,
	primary_error: AuthError,
	fallback_name: &'static str,
	fallback_error: AuthError,
}

impl RefreshError {
	fn retry_after(&self) -> Option<Duration> {
		max_duration(self.primary_error.retry_after(), self.fallback_error.retry_after())
	}
}

impl fmt::Display for RefreshError {
	fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
		write!(
			formatter,
			"{} failed: {}; {} failed: {}",
			self.primary_name, self.primary_error, self.fallback_name, self.fallback_error
		)
	}
}

impl From<wreq::Error> for AuthError {
	fn from(err: wreq::Error) -> Self {
		AuthError::Wreq(err)
	}
}

impl From<serde_json::Error> for AuthError {
	fn from(err: serde_json::Error) -> Self {
		AuthError::SerdeDeserialize(err)
	}
}

#[derive(Debug, Default)]
struct RefreshBackoff {
	consecutive_failures: u32,
	retry_not_before: Option<Instant>,
}

impl RefreshBackoff {
	fn retry_remaining(&self, now: Instant) -> Option<Duration> {
		self.retry_not_before.and_then(|deadline| deadline.checked_duration_since(now))
	}

	fn record_failure(&mut self, now: Instant, retry_after: Option<Duration>) -> Duration {
		self.consecutive_failures = self.consecutive_failures.saturating_add(1);
		let delay = refresh_retry_delay(self.consecutive_failures, retry_after);
		self.retry_not_before = Some(now + delay);
		delay
	}

	fn record_success(&mut self) {
		self.consecutive_failures = 0;
		self.retry_not_before = None;
	}
}

fn refresh_backoff() -> std::sync::MutexGuard<'static, RefreshBackoff> {
	REFRESH_BACKOFF.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn refresh_retry_delay(failure_count: u32, retry_after: Option<Duration>) -> Duration {
	let exponent = failure_count.saturating_sub(1).min(31);
	let multiplier = 1_u64.checked_shl(exponent).unwrap_or(u64::MAX);
	let exponential = Duration::from_secs(INITIAL_REFRESH_RETRY_DELAY.as_secs().saturating_mul(multiplier)).min(MAX_REFRESH_RETRY_DELAY);
	let retry_after = retry_after.unwrap_or_default().min(MAX_SERVER_RETRY_DELAY);
	exponential.max(retry_after)
}

fn max_duration(first: Option<Duration>, second: Option<Duration>) -> Option<Duration> {
	match (first, second) {
		(Some(first), Some(second)) => Some(first.max(second)),
		(Some(duration), None) | (None, Some(duration)) => Some(duration),
		(None, None) => None,
	}
}

fn response_retry_after(headers: &wreq::header::HeaderMap) -> Option<Duration> {
	let value = headers.get(wreq::header::RETRY_AFTER)?.to_str().ok()?;
	if let Ok(seconds) = value.parse::<f64>() {
		if seconds.is_finite() && seconds >= 0.0 {
			return Some(Duration::from_secs_f64(seconds.min(MAX_SERVER_RETRY_DELAY.as_secs_f64())));
		}
	}
	httpdate::parse_http_date(value)
		.ok()
		.and_then(|deadline| deadline.duration_since(SystemTime::now()).ok())
		.map(|delay| delay.min(MAX_SERVER_RETRY_DELAY))
}

fn token_refresh_delay(expires_in: u64) -> Duration {
	Duration::from_secs(expires_in.saturating_sub(TOKEN_REFRESH_EARLY_BY).max(1))
}

fn refresh_backoff_remaining() -> Option<Duration> {
	refresh_backoff().retry_remaining(Instant::now())
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum RefreshOutcome {
	Refreshed,
	InProgress,
	BackingOff(Duration),
	Failed(Duration),
}

impl RefreshOutcome {
	pub fn retry_after(self) -> Option<Duration> {
		match self {
			Self::BackingOff(delay) | Self::Failed(delay) => Some(delay),
			Self::Refreshed | Self::InProgress => None,
		}
	}
}

struct RolloverGuard;

impl RolloverGuard {
	fn acquire() -> Option<Self> {
		OAUTH_IS_ROLLING_OVER.compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst).ok().map(|_| Self)
	}
}

impl Drop for RolloverGuard {
	fn drop(&mut self) {
		OAUTH_IS_ROLLING_OVER.store(false, Ordering::SeqCst);
	}
}

pub async fn token_daemon() {
	loop {
		let (duration, reason) = match refresh_backoff_remaining() {
			Some(duration) => (duration, "OAuth refresh retry"),
			None => {
				let expires_in = OAUTH_CLIENT.load_full().expires_in;
				(token_refresh_delay(expires_in), "scheduled OAuth refresh")
			}
		};

		info!("[⏳] Waiting {duration:?} for {reason}");
		tokio::select! {
			_ = tokio::time::sleep(duration) => {
				let _ = force_refresh_token(RefreshReason::Scheduled).await;
			}
			_ = TOKEN_REFRESH_NOTIFY.notified() => {
				trace!("OAuth refresh schedule changed; recalculating");
			}
		}
	}
}

pub(crate) async fn force_refresh_token(reason: RefreshReason) -> RefreshOutcome {
	if let Some(delay) = refresh_backoff_remaining() {
		trace!("Skipping {} OAuth refresh during backoff ({delay:?} remaining)", reason.label());
		return RefreshOutcome::BackingOff(delay);
	}

	let Some(rollover_guard) = RolloverGuard::acquire() else {
		trace!("Skipping refresh token roll over, already in progress");
		return RefreshOutcome::InProgress;
	};

	// The backoff may have started between the first check and acquiring the
	// single-refresh guard.
	if let Some(delay) = refresh_backoff_remaining() {
		return RefreshOutcome::BackingOff(delay);
	}

	refresh_token_with_guard(reason, rollover_guard).await
}

async fn refresh_token_with_guard(reason: RefreshReason, _rollover_guard: RolloverGuard) -> RefreshOutcome {
	trace!("Refreshing OAuth token: reason={}", reason.label());
	let current_client = OAUTH_CLIENT.load_full();
	match current_client.refreshed(reason).await {
		Ok(mut refreshed) => {
			refreshed.oauth.generation = current_client.generation.wrapping_add(1);
			let generation = refreshed.oauth.generation;
			OAUTH_CLIENT.swap(refreshed.oauth.into());
			install_oauth_generation(generation, refreshed.fresh_identity);
			refresh_backoff().record_success();
			TOKEN_REFRESH_NOTIFY.notify_waiters();
			info!(
				"[✅] OAuth token refreshed successfully: reason={} fresh_identity={}",
				reason.label(),
				refreshed.fresh_identity
			);
			RefreshOutcome::Refreshed
		}
		Err(error) => {
			let delay = refresh_backoff().record_failure(Instant::now(), error.retry_after());
			TOKEN_REFRESH_NOTIFY.notify_waiters();
			error!(
				"OAuth token refresh failed: reason={}; retaining the current client and retrying in {delay:?}: {error}",
				reason.label()
			);
			RefreshOutcome::Failed(delay)
		}
	}
}

#[derive(Debug, Clone, Default)]
struct Device {
	oauth_id: String,
	initial_headers: HashMap<String, String>,
	headers: HashMap<String, String>,
	user_agent: String,
}

// MobileSpoofAuth backend - spoofs an Android mobile device
#[derive(Debug, Clone)]
pub struct MobileSpoofAuth {
	device: Device,
	additional_headers: HashMap<String, String>,
}

impl MobileSpoofAuth {
	fn new() -> Self {
		Self {
			device: Device::new(),
			additional_headers: HashMap::new(),
		}
	}
}

impl OauthBackend for MobileSpoofAuth {
	async fn authenticate(&mut self) -> Result<OauthResponse, AuthError> {
		// Construct URL for OAuth token
		let url = format!("{AUTH_ENDPOINT}/auth/v2/oauth/access-token/loid");
		record_oauth_send();
		let mut builder = CLIENT.post(&url);

		// Add headers from spoofed client
		for (key, value) in &self.device.initial_headers {
			builder = builder.header(key, value);
		}
		for (key, value) in &self.additional_headers {
			if key == "x-reddit-loid" || key == "x-reddit-session" {
				builder = builder.header(key, value);
			}
		}
		// Set up HTTP Basic Auth - basically just the const OAuth ID's with no password,
		// Base64-encoded. https://en.wikipedia.org/wiki/Basic_access_authentication
		// This could be constant, but I don't think it's worth it. OAuth ID's can change
		// over time and we want to be flexible.
		let auth = general_purpose::STANDARD.encode(format!("{}:", self.device.oauth_id));
		builder = builder.header("Authorization", format!("Basic {auth}"));

		// Set JSON body. I couldn't tell you what this means. But that's what the client sends
		let json = json!({
				"scopes": ["*","email", "pii"]
		});

		trace!("Sending token request to {url}...");

		// Send request
		let resp = builder.json(&json).send().await?;

		let status = resp.status();
		trace!("Received response with status {} and length {:?}", status, resp.headers().get("content-length"));
		if !status.is_success() {
			return Err(AuthError::HttpStatus {
				status: status.as_u16(),
				retry_after: response_retry_after(resp.headers()),
			});
		}

		// Parse headers - loid header _should_ be saved sent on subsequent token refreshes.
		// Technically it's not needed, but it's easy for Reddit API to check for this.
		// It's some kind of header that uniquely identifies the device.
		// Not worried about the privacy implications, since this is randomly changed
		// and really only as privacy-concerning as the OAuth token itself.
		if let Some(header) = resp.headers().get("x-reddit-loid") {
			let header_val: &wreq::header::HeaderValue = header;
			if let Ok(value) = header_val.to_str() {
				self.additional_headers.insert("x-reddit-loid".to_owned(), value.to_owned());
			}
		}

		// Same with x-reddit-session
		if let Some(header) = resp.headers().get("x-reddit-session") {
			let header_val: &wreq::header::HeaderValue = header;
			if let Ok(value) = header_val.to_str() {
				self.additional_headers.insert("x-reddit-session".to_owned(), value.to_owned());
			}
		}

		trace!("Serializing response...");

		// Serialize response
		let json: serde_json::Value = resp.json().await?;

		trace!("Accessing relevant fields...");

		// Save token and expiry
		let token = json
			.get("access_token")
			.ok_or(AuthError::Field("access_token"))?
			.as_str()
			.ok_or(AuthError::Field("access_token"))?
			.to_string();
		let expires_in = json
			.get("expires_in")
			.ok_or(AuthError::Field("expires_in"))?
			.as_u64()
			.ok_or(AuthError::Field("expires_in"))?;

		info!("[✅] MobileSpoofAuth retrieved an OAuth token that expires in {expires_in} seconds");

		Ok(OauthResponse {
			token,
			expires_in,
			additional_headers: self.additional_headers.clone(),
		})
	}

	fn user_agent(&self) -> &str {
		&self.device.user_agent
	}

	fn get_headers(&self) -> HashMap<String, String> {
		let mut headers = self.device.headers.clone();
		headers.extend(self.additional_headers.clone());
		headers
	}
}

// GenericWebAuth backend - simple web-based authentication
#[derive(Debug, Clone)]
pub struct GenericWebAuth {
	device_id: String,
	user_agent: String,
	additional_headers: HashMap<String, String>,
}

impl GenericWebAuth {
	fn new() -> Self {
		// Generate random 20-character alphanumeric device_id
		let device_id: String = (0..20)
			.map(|_| {
				let idx = fastrand::usize(..62);
				let chars = b"abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789";
				chars[idx] as char
			})
			.collect();

		info!("[🔄] Using GenericWebAuth");

		Self {
			device_id,
			user_agent: fake_user_agent::get_rua().to_owned(),
			additional_headers: HashMap::new(),
		}
	}
}

impl OauthBackend for GenericWebAuth {
	async fn authenticate(&mut self) -> Result<OauthResponse, AuthError> {
		// Construct URL for OAuth token
		let url = "https://www.reddit.com/api/v1/access_token";
		record_oauth_send();
		let mut builder = CLIENT.post(url);

		// Add minimal headers
		builder = builder.header("Host", "www.reddit.com");
		builder = builder.header("User-Agent", &self.user_agent);
		builder = builder.header("Accept", "*/*");
		builder = builder.header("Accept-Language", "en-US,en;q=0.5");
		// builder = builder.header("Accept-Encoding", "gzip, deflate, br, zstd");
		builder = builder.header("Authorization", "Basic M1hmQkpXbGlIdnFBQ25YcmZJWWxMdzo=");
		builder = builder.header("Content-Type", "application/x-www-form-urlencoded");
		builder = builder.header("Sec-GPC", "1");
		builder = builder.header("Connection", "keep-alive");
		for (key, value) in &self.additional_headers {
			if key == "x-reddit-loid" || key == "x-reddit-session" {
				builder = builder.header(key, value);
			}
		}

		// Set up form body
		let body_str = format!("grant_type=https%3A%2F%2Foauth.reddit.com%2Fgrants%2Finstalled_client&device_id={}", self.device_id);

		trace!("Sending GenericWebAuth token request to {url}...");

		// Send request
		let resp: wreq::Response = builder.body(body_str).send().await?;

		let status = resp.status();
		trace!("Received response with status {} and length {:?}", status, resp.headers().get("content-length"));
		if !status.is_success() {
			return Err(AuthError::HttpStatus {
				status: status.as_u16(),
				retry_after: response_retry_after(resp.headers()),
			});
		}

		// Parse headers - loid header _should_ be saved sent on subsequent token refreshes.
		// Technically it's not needed, but it's easy for Reddit API to check for this.
		// It's some kind of header that uniquely identifies the device.
		// Not worried about the privacy implications, since this is randomly changed
		// and really only as privacy-concerning as the OAuth token itself.
		if let Some(header) = resp.headers().get("x-reddit-loid") {
			let header_val: &wreq::header::HeaderValue = header;
			if let Ok(value) = header_val.to_str() {
				self.additional_headers.insert("x-reddit-loid".to_owned(), value.to_owned());
			}
		}

		// Same with x-reddit-session
		if let Some(header) = resp.headers().get("x-reddit-session") {
			let header_val: &wreq::header::HeaderValue = header;
			if let Ok(value) = header_val.to_str() {
				self.additional_headers.insert("x-reddit-session".to_owned(), value.to_owned());
			}
		}

		trace!("Serializing GenericWebAuth response...");

		// Serialize response
		let json: serde_json::Value = resp.json().await?;

		trace!("Accessing relevant fields...");

		// Parse response - access_token, token_type, device_id, expires_in, scope
		let token = json
			.get("access_token")
			.ok_or(AuthError::Field("access_token"))?
			.as_str()
			.ok_or(AuthError::Field("access_token"))?
			.to_string();
		let expires_in = json
			.get("expires_in")
			.ok_or(AuthError::Field("expires_in"))?
			.as_u64()
			.ok_or(AuthError::Field("expires_in"))?;

		info!("[✅] GenericWebAuth retrieved an OAuth token that expires in {expires_in} seconds");

		// Insert a few necessary headers
		self.additional_headers.insert("Origin".to_owned(), "https://www.reddit.com".to_owned());
		self.additional_headers.insert("User-Agent".to_owned(), self.user_agent.to_owned());

		Ok(OauthResponse {
			token,
			expires_in,
			additional_headers: self.additional_headers.clone(),
		})
	}

	fn user_agent(&self) -> &str {
		&self.user_agent
	}

	fn get_headers(&self) -> HashMap<String, String> {
		self.additional_headers.clone()
	}
}

impl Device {
	fn android() -> Self {
		// Generate uuid
		let uuid = uuid::Uuid::new_v4().to_string();

		// Generate random user-agent
		let android_app_version = choose(ANDROID_APP_VERSION_LIST).to_string();
		let android_version = fastrand::u8(9..=14);

		let android_user_agent = format!("Reddit/{android_app_version}/Android {android_version}");

		let qos = fastrand::u32(1000..=100_000);
		let qos: f32 = qos as f32 / 1000.0;
		let qos = format!("{qos:.3}");

		let codecs = TextGenerator::new().generate("available-codecs=video/avc, video/hevc{, video/x-vnd.on2.vp9|}");

		// Android device headers
		let headers: HashMap<String, String> = HashMap::from([
			("User-Agent".into(), android_user_agent.clone()),
			("x-reddit-retry".into(), "algo=no-retries".into()),
			("x-reddit-compression".into(), "1".into()),
			("x-reddit-qos".into(), qos),
			("x-reddit-media-codecs".into(), codecs),
			("Content-Type".into(), "application/json; charset=UTF-8".into()),
			("client-vendor-id".into(), uuid.clone()),
			("X-Reddit-Device-Id".into(), uuid.clone()),
		]);

		info!("[🔄] Created a stable spoofed Android identity for OAuth");

		Self {
			oauth_id: REDDIT_ANDROID_OAUTH_CLIENT_ID.to_string(),
			headers: headers.clone(),
			initial_headers: headers,
			user_agent: android_user_agent,
		}
	}
	fn new() -> Self {
		// See https://github.com/redlib-org/redlib/issues/8
		Self::android()
	}
}

fn choose<T: Copy>(list: &[T]) -> T {
	*fastrand::choose_multiple(list.iter(), 1)[0]
}

#[cfg(test)]
mod tests {
	use super::*;

	#[tokio::test(flavor = "multi_thread")]
	async fn test_mobile_spoof_backend() {
		// Test MobileSpoofAuth backend specifically
		let mut backend = MobileSpoofAuth::new();
		let response = backend.authenticate().await;
		assert!(response.is_ok());
		let response = response.unwrap();
		assert!(!response.token.is_empty());
		assert!(response.expires_in > 0);
		assert!(!backend.user_agent().is_empty());
		assert!(!backend.get_headers().is_empty());
	}

	#[tokio::test(flavor = "multi_thread")]
	#[ignore = "requires live Reddit GenericWeb OAuth access"]
	async fn test_generic_web_backend() {
		// Test GenericWebAuth backend specifically
		let mut backend = GenericWebAuth::new();
		let response = backend.authenticate().await;
		assert!(response.is_ok());
		let response = response.unwrap();
		assert!(!response.token.is_empty());
		assert!(response.expires_in > 0);
		assert!(!backend.user_agent().is_empty());
	}

	#[tokio::test(flavor = "multi_thread")]
	async fn test_oauth_client() {
		// Integration test - tests the overall Oauth client
		assert!(OAUTH_CLIENT.load_full().headers_map.contains_key("Authorization"));
	}

	#[tokio::test(flavor = "multi_thread")]
	async fn test_oauth_client_refresh() {
		force_refresh_token(RefreshReason::Scheduled).await;
	}

	#[tokio::test(flavor = "multi_thread")]
	async fn test_oauth_token_exists() {
		let client = OAUTH_CLIENT.load_full();
		let auth_header = client.headers_map.get("Authorization").unwrap();
		assert!(auth_header.starts_with("Bearer "));
	}

	#[tokio::test(flavor = "multi_thread")]
	async fn test_oauth_headers_len() {
		assert!(OAUTH_CLIENT.load_full().headers_map.len() >= 3);
	}

	#[test]
	fn test_creating_device() {
		Device::new();
	}

	#[test]
	fn test_creating_backends() {
		// Test that both backends can be created
		MobileSpoofAuth::new();
		GenericWebAuth::new();
	}

	#[test]
	fn test_refresh_retry_delay_is_exponential_and_capped() {
		assert_eq!(refresh_retry_delay(1, None), Duration::from_secs(5));
		assert_eq!(refresh_retry_delay(2, None), Duration::from_secs(10));
		assert_eq!(refresh_retry_delay(3, None), Duration::from_secs(20));
		assert_eq!(refresh_retry_delay(20, None), MAX_REFRESH_RETRY_DELAY);
	}

	#[test]
	fn test_refresh_retry_delay_honors_server_delay() {
		assert_eq!(refresh_retry_delay(1, Some(Duration::from_secs(90))), Duration::from_secs(90));
		assert_eq!(refresh_retry_delay(1, Some(Duration::from_secs(900))), MAX_SERVER_RETRY_DELAY);
	}

	#[test]
	fn test_refresh_backoff_recovers_after_success() {
		let now = Instant::now();
		let mut backoff = RefreshBackoff::default();
		assert_eq!(backoff.record_failure(now, None), Duration::from_secs(5));
		assert_eq!(backoff.retry_remaining(now + Duration::from_secs(1)), Some(Duration::from_secs(4)));
		assert_eq!(backoff.record_failure(now + Duration::from_secs(5), None), Duration::from_secs(10));
		backoff.record_success();
		assert_eq!(backoff.retry_remaining(now), None);
		assert_eq!(backoff.consecutive_failures, 0);
	}

	#[test]
	fn test_token_refresh_delay_cannot_underflow() {
		assert_eq!(token_refresh_delay(3600), Duration::from_secs(3480));
		assert_eq!(token_refresh_delay(120), Duration::from_secs(1));
		assert_eq!(token_refresh_delay(30), Duration::from_secs(1));
	}

	#[test]
	fn test_refresh_reason_selects_stable_or_fresh_identity() {
		let original_backend = OauthBackendImpl::MobileSpoof(MobileSpoofAuth::new());
		let original_device_id = match &original_backend {
			OauthBackendImpl::MobileSpoof(backend) => backend.device.headers.get("X-Reddit-Device-Id").unwrap().clone(),
			OauthBackendImpl::GenericWeb(_) => unreachable!(),
		};
		let oauth = Oauth {
			headers_map: HashMap::new(),
			expires_in: 3600,
			backend: original_backend,
			generation: 4,
		};

		let (stable, stable_is_fresh) = oauth.refresh_backend(RefreshReason::Scheduled, false);
		let (_, stable_fallback_is_fresh) = oauth.refresh_backend(RefreshReason::Scheduled, true);
		let stable_device_id = match stable {
			OauthBackendImpl::MobileSpoof(backend) => backend.device.headers.get("X-Reddit-Device-Id").unwrap().clone(),
			OauthBackendImpl::GenericWeb(_) => unreachable!(),
		};
		assert!(!stable_is_fresh);
		assert!(stable_fallback_is_fresh);
		assert_eq!(stable_device_id, original_device_id);
	}

	#[test]
	fn test_alternate_backend_changes_kind() {
		let mobile = OauthBackendImpl::MobileSpoof(MobileSpoofAuth::new());
		let generic = mobile.alternate();
		assert!(matches!(generic, OauthBackendImpl::GenericWeb(_)));
		assert!(matches!(generic.alternate(), OauthBackendImpl::MobileSpoof(_)));
	}
}
