use crate::{
	client::{
		claim_quota_rotation, client_for_oauth_profile, install_oauth_client, oauth_client, quota_rotation_still_needed, random_oauth_profile_except, record_oauth_send,
		OauthTransportProfile, QuotaRotationTicket, OAUTH_BROWSER_PROFILES, OAUTH_IS_ROLLING_OVER, TOR_OAUTH_IS_ROLLING_OVER,
	},
	oauth_resources::ANDROID_APP_VERSION_LIST,
	reddit_lane::RedditLane,
	timing::{positive_jitter, proportional_positive_jitter},
};
use base64::{engine::general_purpose, Engine as _};
use log::{error, info, trace, warn};
use serde_json::json;
use std::{collections::HashMap, fmt, sync::atomic::Ordering, sync::Arc, sync::LazyLock, sync::Mutex, time::Duration, time::Instant, time::SystemTime};
use tegen::tegen::TextGenerator;
use tokio::sync::Notify;
use tokio::time::timeout;

const REDDIT_ANDROID_OAUTH_CLIENT_ID: &str = "ohXpoqrZYub1kg";

const OAUTH_TIMEOUT: Duration = Duration::from_secs(5);
const TOR_OAUTH_TIMEOUT: Duration = Duration::from_secs(45);
const INITIAL_REFRESH_RETRY_DELAY: Duration = Duration::from_secs(5);
const MAX_REFRESH_RETRY_DELAY: Duration = Duration::from_secs(300);
const MAX_SERVER_RETRY_DELAY: Duration = Duration::from_secs(600);
const ANDROID_APP_VERSION_COHORT_WEEKS: u32 = 8;
const TOKEN_REFRESH_MIN_EARLY_BY: u64 = 120;
const TOKEN_REFRESH_MAX_EARLY_BY: u64 = 240;
static DIRECT_REFRESH_BACKOFF: LazyLock<Mutex<RefreshBackoff>> = LazyLock::new(|| Mutex::new(RefreshBackoff::default()));
static TOR_REFRESH_BACKOFF: LazyLock<Mutex<RefreshBackoff>> = LazyLock::new(|| Mutex::new(RefreshBackoff::default()));
static DIRECT_TOKEN_REFRESH_NOTIFY: LazyLock<Notify> = LazyLock::new(Notify::new);
static TOR_TOKEN_REFRESH_NOTIFY: LazyLock<Notify> = LazyLock::new(Notify::new);
static DIRECT_ACTIVE_QUOTA_ROTATION: LazyLock<Mutex<Option<QuotaRotationTicket>>> = LazyLock::new(|| Mutex::new(None));
static TOR_ACTIVE_QUOTA_ROTATION: LazyLock<Mutex<Option<QuotaRotationTicket>>> = LazyLock::new(|| Mutex::new(None));

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub(crate) enum RefreshReason {
	Scheduled,
	Unauthorized,
	LowRateLimit,
}

impl RefreshReason {
	fn label(self) -> &'static str {
		match self {
			Self::Scheduled => "scheduled",
			Self::Unauthorized => "unauthorized",
			Self::LowRateLimit => "low_rate_limit",
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
	fn authenticate(&mut self, client: &wreq::Client) -> impl std::future::Future<Output = Result<OauthResponse, AuthError>> + Send;
	fn user_agent(&self) -> &str;
	fn get_headers(&self) -> HashMap<String, String>;
}

// Spoofed client for Android devices
#[derive(Clone)]
pub struct Oauth {
	pub(crate) headers_map: HashMap<String, String>,
	pub(crate) http_client: Arc<wreq::Client>,
	refresh_at: Instant,
	backend: MobileSpoofAuth,
	transport_profile: OauthTransportProfile,
	pub(crate) generation: u64,
	pub(crate) lane: RedditLane,
}

struct RefreshedOauth {
	oauth: Oauth,
	fresh_identity: bool,
}

fn shuffled_oauth_profiles() -> [OauthTransportProfile; OAUTH_BROWSER_PROFILES.len()] {
	let mut profiles = OAUTH_BROWSER_PROFILES;
	fastrand::shuffle(&mut profiles);
	profiles
}

impl Oauth {
	/// Create a new OAuth client
	pub(crate) async fn new(lane: RedditLane) -> Self {
		let mut failure_cycles = 0_u32;
		let mut attempt = 0_u32;
		let mut identity_generation = 0_u32;

		loop {
			let profiles = shuffled_oauth_profiles();
			let mut retry_after = None;
			let mut exhausted_policy_profiles = true;

			for (profile_index, transport_profile) in profiles.into_iter().enumerate() {
				let http_client = match client_for_oauth_profile(lane, transport_profile) {
					Ok(client) => client,
					Err(error) => {
						error!(
							"[⛔] OAuth startup transport construction failed: lane={} backend=MobileSpoofAuth transport={} class=configuration error={error}",
							lane.label(),
							transport_profile.label(),
						);
						exhausted_policy_profiles = false;
						break;
					}
				};
				let mut backend = MobileSpoofAuth::new(lane);
				attempt = attempt.saturating_add(1);

				match Self::authenticate_with_backend(&mut backend, http_client, transport_profile).await {
					Ok(oauth) => {
						info!(
							"[✅] Successfully created OAuth client: lane={} backend=MobileSpoofAuth identity_profile=mobile_android transport={} attempt={attempt} identity_generation={identity_generation}",
							lane.label(),
							transport_profile.label(),
						);
						return oauth;
					}
					Err(error) => {
						retry_after = max_duration(retry_after, error.retry_after());
						let policy_forbidden = error.is_identity_policy_forbidden();
						error!(
							"[⛔] OAuth startup authentication failed: lane={} backend=MobileSpoofAuth identity_profile=mobile_android transport={} attempt={attempt} identity_generation={identity_generation} class={} error={error}",
							lane.label(),
							transport_profile.label(),
							error.failure_class(),
						);
						identity_generation = identity_generation.saturating_add(1);

						if policy_forbidden && profile_index + 1 < OAUTH_BROWSER_PROFILES.len() {
							warn!(
								"[🔄] Rotating OAuth identity and browser transport immediately after a policy 403: lane={} next_identity_generation={identity_generation}",
								lane.label(),
							);
							continue;
						}

						exhausted_policy_profiles = policy_forbidden;
						break;
					}
				}
			}

			failure_cycles = failure_cycles.saturating_add(1);
			let delay = refresh_retry_delay(failure_cycles, retry_after);
			warn!(
				"[⏳] OAuth startup cycle failed: lane={} attempts={attempt} exhausted_browser_profiles={exhausted_policy_profiles} retrying_in={delay:?}",
				lane.label(),
			);
			tokio::time::sleep(delay).await;
		}
	}

	async fn authenticate_with_backend(backend: &mut MobileSpoofAuth, http_client: Arc<wreq::Client>, transport_profile: OauthTransportProfile) -> Result<Self, AuthError> {
		let oauth_timeout = match backend.lane {
			RedditLane::Direct => OAUTH_TIMEOUT,
			RedditLane::Tor => TOR_OAUTH_TIMEOUT,
		};
		let response = timeout(oauth_timeout, backend.authenticate(&http_client))
			.await
			.map_err(|_| AuthError::Timeout(oauth_timeout))??;

		// Build headers_map from backend headers + Authorization header
		let mut headers_map = backend.get_headers();
		headers_map.insert("Authorization".to_owned(), format!("Bearer {}", response.token));
		headers_map.extend(response.additional_headers);

		let refresh_at = Instant::now() + sampled_token_refresh_delay(response.expires_in);
		Ok(Self {
			headers_map,
			http_client,
			refresh_at,
			backend: backend.clone(),
			transport_profile,
			generation: 0,
			lane: backend.lane,
		})
	}

	fn refresh_backend(&self, reason: RefreshReason) -> (MobileSpoofAuth, bool) {
		match reason {
			RefreshReason::LowRateLimit => (MobileSpoofAuth::new(self.lane), true),
			RefreshReason::Scheduled | RefreshReason::Unauthorized => (self.backend.clone(), false),
		}
	}

	async fn refreshed(&self, reason: RefreshReason) -> Result<RefreshedOauth, RefreshError> {
		let (mut backend, fresh_identity) = self.refresh_backend(reason);
		let (http_client, transport_profile) = self
			.http_client_for_refresh(fresh_identity)
			.map_err(|error| RefreshError::configuration("MobileSpoofAuth", error))?;
		Self::authenticate_with_backend(&mut backend, http_client, transport_profile)
			.await
			.map(|oauth| RefreshedOauth { oauth, fresh_identity })
			.map_err(|error| RefreshError {
				backend_name: "MobileSpoofAuth",
				error,
			})
	}

	fn http_client_for_refresh(&self, fresh_identity: bool) -> Result<(Arc<wreq::Client>, OauthTransportProfile), String> {
		if !fresh_identity {
			Ok((self.http_client.clone(), self.transport_profile))
		} else {
			let transport_profile = random_oauth_profile_except(self.transport_profile);
			client_for_oauth_profile(self.lane, transport_profile).map(|client| (client, transport_profile))
		}
	}

	pub fn user_agent(&self) -> &str {
		self.backend.user_agent()
	}
}

#[derive(Debug)]
enum AuthError {
	Configuration(String),
	Wreq(wreq::Error),
	SerdeDeserialize(serde_json::Error),
	Field(&'static str),
	HttpStatus {
		status: u16,
		retry_after: Option<Duration>,
		retry_after_present: bool,
		quota_headers_present: bool,
		www_authenticate_present: bool,
	},
	Timeout(Duration),
}

impl AuthError {
	fn retry_after(&self) -> Option<Duration> {
		match self {
			Self::HttpStatus { retry_after, .. } => *retry_after,
			_ => None,
		}
	}

	fn is_identity_policy_forbidden(&self) -> bool {
		matches!(
			self,
			Self::HttpStatus {
				status: 403,
				quota_headers_present: false,
				www_authenticate_present: false,
				..
			}
		)
	}

	fn failure_class(&self) -> &'static str {
		match self {
			Self::Configuration(_) => "configuration",
			Self::Wreq(_) => "transport",
			Self::SerdeDeserialize(_) | Self::Field(_) => "invalid_response",
			Self::HttpStatus { status: 400, .. } => "request_rejected",
			Self::HttpStatus { status: 401, .. }
			| Self::HttpStatus {
				status: 403,
				www_authenticate_present: true,
				..
			} => "credential_or_grant_rejected",
			Self::HttpStatus {
				status: 403,
				quota_headers_present: true,
				..
			}
			| Self::HttpStatus { status: 429, .. } => "rate_limited",
			Self::HttpStatus { status: 403, .. } => "edge_egress_or_identity_policy",
			Self::HttpStatus { status, .. } if (500..=599).contains(status) => "upstream",
			Self::HttpStatus { .. } => "http_status",
			Self::Timeout(_) => "timeout",
		}
	}
}

impl fmt::Display for AuthError {
	fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
		match self {
			Self::Configuration(error) => write!(formatter, "transport configuration failed: {error}"),
			Self::Wreq(error) => write!(formatter, "request failed: {error}"),
			Self::SerdeDeserialize(error) => write!(formatter, "invalid response body: {error}"),
			Self::Field(field) => write!(formatter, "OAuth response is missing or has an invalid {field} field"),
			Self::HttpStatus {
				status,
				retry_after,
				retry_after_present,
				quota_headers_present,
				www_authenticate_present,
			} => {
				write!(
					formatter,
					"HTTP {status} (retry_after_present={retry_after_present} retry_after_seconds={} quota_headers_present={quota_headers_present} www_authenticate_present={www_authenticate_present})",
					retry_after.map_or(0.0, |delay| delay.as_secs_f64()),
				)
			}
			Self::Timeout(duration) => write!(formatter, "request timed out after {duration:?}"),
		}
	}
}

#[derive(Debug)]
struct RefreshError {
	backend_name: &'static str,
	error: AuthError,
}

impl RefreshError {
	fn configuration(backend_name: &'static str, error: String) -> Self {
		Self {
			backend_name,
			error: AuthError::Configuration(error),
		}
	}

	fn retry_after(&self) -> Option<Duration> {
		self.error.retry_after()
	}
}

impl fmt::Display for RefreshError {
	fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
		write!(formatter, "{} failed: {}", self.backend_name, self.error)
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

fn refresh_backoff(lane: RedditLane) -> std::sync::MutexGuard<'static, RefreshBackoff> {
	match lane {
		RedditLane::Direct => DIRECT_REFRESH_BACKOFF.lock(),
		RedditLane::Tor => TOR_REFRESH_BACKOFF.lock(),
	}
	.unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn token_refresh_notify(lane: RedditLane) -> &'static Notify {
	match lane {
		RedditLane::Direct => &DIRECT_TOKEN_REFRESH_NOTIFY,
		RedditLane::Tor => &TOR_TOKEN_REFRESH_NOTIFY,
	}
}

fn active_quota_rotation(lane: RedditLane) -> &'static Mutex<Option<QuotaRotationTicket>> {
	match lane {
		RedditLane::Direct => &DIRECT_ACTIVE_QUOTA_ROTATION,
		RedditLane::Tor => &TOR_ACTIVE_QUOTA_ROTATION,
	}
}

fn rollover_flag(lane: RedditLane) -> &'static std::sync::atomic::AtomicBool {
	match lane {
		RedditLane::Direct => &OAUTH_IS_ROLLING_OVER,
		RedditLane::Tor => &TOR_OAUTH_IS_ROLLING_OVER,
	}
}

fn refresh_retry_delay(failure_count: u32, retry_after: Option<Duration>) -> Duration {
	let (base, server_is_floor) = refresh_retry_base_delay(failure_count, retry_after);
	if server_is_floor {
		positive_jitter(base, Duration::from_secs(2))
	} else {
		proportional_positive_jitter(base)
	}
}

fn refresh_retry_base_delay(failure_count: u32, retry_after: Option<Duration>) -> (Duration, bool) {
	let exponent = failure_count.saturating_sub(1).min(31);
	let multiplier = 1_u64.checked_shl(exponent).unwrap_or(u64::MAX);
	let exponential = Duration::from_secs(INITIAL_REFRESH_RETRY_DELAY.as_secs().saturating_mul(multiplier)).min(MAX_REFRESH_RETRY_DELAY);
	let retry_after = retry_after.unwrap_or_default().min(MAX_SERVER_RETRY_DELAY);
	(exponential.max(retry_after), retry_after >= exponential && !retry_after.is_zero())
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

fn response_has_quota_headers(headers: &wreq::header::HeaderMap) -> bool {
	["x-ratelimit-remaining", "x-ratelimit-reset", "x-ratelimit-used"]
		.iter()
		.any(|header| headers.contains_key(*header))
}

fn sampled_token_refresh_delay(expires_in: u64) -> Duration {
	let max_early_by = TOKEN_REFRESH_MAX_EARLY_BY.min(expires_in / 2);
	let min_early_by = TOKEN_REFRESH_MIN_EARLY_BY.min(max_early_by);
	let early_by = if min_early_by == max_early_by {
		min_early_by
	} else {
		fastrand::u64(min_early_by..=max_early_by)
	};
	token_refresh_delay(expires_in, early_by)
}

fn token_refresh_delay(expires_in: u64, early_by: u64) -> Duration {
	Duration::from_secs(expires_in.saturating_sub(early_by.min(expires_in / 2)).max(1))
}

fn refresh_backoff_remaining(lane: RedditLane) -> Option<Duration> {
	refresh_backoff(lane).retry_remaining(Instant::now())
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum RefreshOutcome {
	Refreshed,
	InProgress,
	Superseded,
	BackingOff(Duration),
	Failed(Duration),
}

impl RefreshOutcome {
	pub fn retry_after(self) -> Option<Duration> {
		match self {
			Self::BackingOff(delay) | Self::Failed(delay) => Some(delay),
			Self::Refreshed | Self::InProgress | Self::Superseded => None,
		}
	}
}

struct RolloverGuard {
	lane: RedditLane,
	quota_rotation: Option<QuotaRotationTicket>,
}

impl RolloverGuard {
	fn acquire(lane: RedditLane) -> Option<Self> {
		rollover_flag(lane)
			.compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
			.ok()
			.map(|_| Self { lane, quota_rotation: None })
	}

	fn track_quota_rotation(&mut self, ticket: QuotaRotationTicket) {
		self.quota_rotation = Some(ticket);
		*active_quota_rotation(self.lane).lock().unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(ticket);
	}
}

impl Drop for RolloverGuard {
	fn drop(&mut self) {
		if self.quota_rotation.is_some() {
			*active_quota_rotation(self.lane).lock().unwrap_or_else(|poisoned| poisoned.into_inner()) = None;
		}
		rollover_flag(self.lane).store(false, Ordering::SeqCst);
	}
}

pub(crate) fn quota_rotation_in_progress(lane: RedditLane, generation: u64, quota_epoch: u64) -> bool {
	active_quota_rotation(lane)
		.lock()
		.unwrap_or_else(|poisoned| poisoned.into_inner())
		.is_some_and(|ticket| ticket.lane == lane && ticket.generation == generation && ticket.quota_epoch == quota_epoch)
}

pub(crate) async fn token_daemon(lane: RedditLane) {
	loop {
		let Some(current_client) = oauth_client(lane) else {
			warn!("Stopping OAuth refresh daemon because the {} lane has no client", lane.label());
			return;
		};
		let (duration, reason) = match refresh_backoff_remaining(lane) {
			Some(duration) => (duration, "OAuth refresh retry"),
			None => (
				current_client.refresh_at.checked_duration_since(Instant::now()).unwrap_or_default(),
				"scheduled OAuth refresh",
			),
		};

		info!("[⏳] Waiting {duration:?} for {reason}: lane={}", lane.label());
		tokio::select! {
			_ = tokio::time::sleep(duration) => {
				if force_refresh_token(lane, RefreshReason::Scheduled).await == RefreshOutcome::InProgress {
					// Another request owns the refresh. Avoid a zero-delay loop while
					// its replacement token is still being fetched.
					tokio::time::sleep(OAUTH_TIMEOUT).await;
				}
			}
			_ = token_refresh_notify(lane).notified() => {
				trace!("OAuth refresh schedule changed; recalculating: lane={}", lane.label());
			}
		}
	}
}

pub(crate) fn spawn_rate_limit_refresh(ticket: QuotaRotationTicket) -> bool {
	let lane = ticket.lane;
	if refresh_backoff_remaining(lane).is_some() {
		return false;
	}

	let Some(mut rollover_guard) = RolloverGuard::acquire(lane) else {
		return false;
	};
	rollover_guard.track_quota_rotation(ticket);

	if refresh_backoff_remaining(lane).is_some() || !oauth_client(lane).is_some_and(|client| client.generation == ticket.generation) || !claim_quota_rotation(ticket) {
		drop(rollover_guard);
		return false;
	}

	tokio::spawn(async move {
		let _ = refresh_token_with_guard(RefreshReason::LowRateLimit, rollover_guard, Some(ticket)).await;
	});
	true
}

pub(crate) async fn force_refresh_token(lane: RedditLane, reason: RefreshReason) -> RefreshOutcome {
	if let Some(delay) = refresh_backoff_remaining(lane) {
		trace!("Skipping {} OAuth refresh during backoff ({delay:?} remaining): lane={}", reason.label(), lane.label());
		return RefreshOutcome::BackingOff(delay);
	}

	let Some(rollover_guard) = RolloverGuard::acquire(lane) else {
		trace!("Skipping refresh token roll over, already in progress: lane={}", lane.label());
		return RefreshOutcome::InProgress;
	};

	// The backoff may have started between the first check and acquiring the
	// single-refresh guard.
	if let Some(delay) = refresh_backoff_remaining(lane) {
		return RefreshOutcome::BackingOff(delay);
	}

	refresh_token_with_guard(reason, rollover_guard, None).await
}

async fn refresh_token_with_guard(reason: RefreshReason, rollover_guard: RolloverGuard, expected_rotation: Option<QuotaRotationTicket>) -> RefreshOutcome {
	let lane = rollover_guard.lane;
	trace!("Refreshing OAuth token: lane={} reason={}", lane.label(), reason.label());
	let Some(current_client) = oauth_client(lane) else {
		return RefreshOutcome::Superseded;
	};
	if let Some(ticket) = expected_rotation {
		if current_client.generation != ticket.generation || !quota_rotation_still_needed(ticket) {
			return RefreshOutcome::Superseded;
		}
	}
	match current_client.refreshed(reason).await {
		Ok(mut refreshed) => {
			refreshed.oauth.generation = current_client.generation.wrapping_add(1);
			if !install_oauth_client(refreshed.oauth, refreshed.fresh_identity, expected_rotation) {
				info!("Discarding completed low-budget OAuth refresh because the quota window recovered or changed");
				refresh_backoff(lane).record_success();
				return RefreshOutcome::Superseded;
			}
			refresh_backoff(lane).record_success();
			token_refresh_notify(lane).notify_waiters();
			info!(
				"[✅] OAuth token refreshed successfully: lane={} reason={} fresh_identity={}",
				lane.label(),
				reason.label(),
				refreshed.fresh_identity
			);
			RefreshOutcome::Refreshed
		}
		Err(error) => {
			let delay = refresh_backoff(lane).record_failure(Instant::now(), error.retry_after());
			if reason != RefreshReason::LowRateLimit {
				token_refresh_notify(lane).notify_waiters();
			}
			error!(
				"OAuth token refresh failed: lane={} reason={}; retaining the current client and retrying in {delay:?}: {error}",
				lane.label(),
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
	lane: RedditLane,
	device: Device,
	additional_headers: HashMap<String, String>,
}

impl MobileSpoofAuth {
	fn new(lane: RedditLane) -> Self {
		Self {
			lane,
			device: Device::new(lane),
			additional_headers: HashMap::new(),
		}
	}
}

impl OauthBackend for MobileSpoofAuth {
	async fn authenticate(&mut self, client: &wreq::Client) -> Result<OauthResponse, AuthError> {
		// Construct URL for OAuth token
		let origin = self.lane.auth_origin();
		let url = format!("{}/auth/v2/oauth/access-token/loid", origin.base);
		record_oauth_send(self.lane);
		let mut builder = client.post(&url);
		builder = builder.header("Host", origin.host);

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
				retry_after_present: resp.headers().contains_key(wreq::header::RETRY_AFTER),
				quota_headers_present: response_has_quota_headers(resp.headers()),
				www_authenticate_present: resp.headers().contains_key(wreq::header::WWW_AUTHENTICATE),
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

impl Device {
	fn android(lane: RedditLane) -> Self {
		// Generate uuid
		let uuid = uuid::Uuid::new_v4().to_string();

		// Generate random user-agent
		let android_app_version = choose_newest_android_app_version(ANDROID_APP_VERSION_LIST).to_string();
		let android_version = fastrand::u8(10..=16);

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

		let app_version_year = android_app_version.strip_prefix("Version ").and_then(|version| version.get(..4)).unwrap_or("unknown");
		info!(
			"[🔄] Created a stable spoofed Android identity for OAuth: lane={} app_version_year={app_version_year} android_major={android_version}",
			lane.label(),
		);

		Self {
			oauth_id: REDDIT_ANDROID_OAUTH_CLIENT_ID.to_string(),
			headers: headers.clone(),
			initial_headers: headers,
			user_agent: android_user_agent,
		}
	}
	fn new(lane: RedditLane) -> Self {
		// See https://github.com/redlib-org/redlib/issues/8
		Self::android(lane)
	}
}

fn choose<T: Copy>(list: &[T]) -> T {
	*fastrand::choose_multiple(list.iter(), 1)[0]
}

fn android_app_version_release_index(version: &str) -> Option<u32> {
	let version = version.strip_prefix("Version ")?.split('/').next()?;
	let mut parts = version.split('.');
	let year: u32 = parts.next()?.parse().ok()?;
	let week: u32 = parts.next()?.parse().ok()?;
	Some(year.saturating_mul(53).saturating_add(week))
}

fn choose_newest_android_app_version<'a>(versions: &'a [&'a str]) -> &'a str {
	let newest_release = versions.iter().filter_map(|version| android_app_version_release_index(version)).max();
	let Some(newest_release) = newest_release else {
		return choose(versions);
	};
	let is_current =
		|version: &str| android_app_version_release_index(version).is_some_and(|release| newest_release.saturating_sub(release) <= ANDROID_APP_VERSION_COHORT_WEEKS);
	let candidate_count = versions.iter().filter(|version| is_current(version)).count();
	let selected = fastrand::usize(..candidate_count);
	versions
		.iter()
		.filter(|version| is_current(version))
		.nth(selected)
		.copied()
		.unwrap_or_else(|| choose(versions))
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::client::OAUTH_CLIENT;

	#[tokio::test(flavor = "multi_thread")]
	#[ignore = "requires live Reddit MobileSpoof OAuth access"]
	async fn test_mobile_spoof_backend() {
		// Test MobileSpoofAuth backend specifically
		let mut backend = MobileSpoofAuth::new(RedditLane::Direct);
		let client = client_for_oauth_profile(RedditLane::Direct, OauthTransportProfile::Chrome145Android).unwrap();
		let response = backend.authenticate(client.as_ref()).await;
		assert!(response.is_ok());
		let response = response.unwrap();
		assert!(!response.token.is_empty());
		assert!(response.expires_in > 0);
		assert!(!backend.user_agent().is_empty());
		assert!(!backend.get_headers().is_empty());
	}

	#[tokio::test(flavor = "multi_thread")]
	#[ignore = "requires live Reddit MobileSpoof OAuth access"]
	async fn test_mobile_spoof_browser_transport_matrix() {
		use wreq::{redirect::Policy, EmulationFactory};
		use wreq_util::{EmulationOS, EmulationOption};

		let identity = MobileSpoofAuth::new(RedditLane::Direct);
		let mut failures = Vec::new();
		let proxy_url = std::env::var("HTTPS_PROXY").or_else(|_| std::env::var("https_proxy")).ok();

		for profile in OAUTH_BROWSER_PROFILES {
			let label = profile.label();
			let emulation = EmulationOption::builder()
				.emulation(profile.emulation())
				.emulation_os(EmulationOS::Android)
				.skip_headers(false)
				.build()
				.emulation();
			let mut client_builder = wreq::Client::builder().emulation(emulation).redirect(Policy::none());
			if let Some(proxy_url) = &proxy_url {
				client_builder = client_builder.proxy(wreq::Proxy::all(proxy_url).unwrap_or_else(|error| panic!("invalid HTTPS proxy: {error}")));
			}
			if let Ok(cert_path) = std::env::var("SSL_CERT_FILE") {
				let certs = std::fs::read(&cert_path).unwrap_or_else(|error| panic!("could not read {cert_path}: {error}"));
				let cert_store = wreq::tls::CertStore::builder()
					.add_stack_pem_certs(certs)
					.build()
					.unwrap_or_else(|error| panic!("could not build test certificate store: {error}"));
				client_builder = client_builder.cert_store(cert_store);
			}
			let client = client_builder.build().unwrap_or_else(|error| panic!("could not build {label}: {error}"));
			let mut backend = identity.clone();

			match timeout(Duration::from_secs(30), backend.authenticate(&client)).await {
				Ok(Ok(response)) => println!("LIVE_OAUTH_PROFILE {label} PASS expires_in={}", response.expires_in),
				Ok(Err(error)) => {
					println!("LIVE_OAUTH_PROFILE {label} FAIL {error:?}");
					failures.push(label);
				}
				Err(_) => {
					println!("LIVE_OAUTH_PROFILE {label} FAIL timeout");
					failures.push(label);
				}
			}
		}

		assert!(failures.is_empty(), "live OAuth profiles failed: {failures:?}");
	}

	#[tokio::test(flavor = "multi_thread")]
	#[ignore = "requires application-managed live OAuth startup"]
	async fn test_oauth_client() {
		// Integration test - tests the overall Oauth client
		assert!(OAUTH_CLIENT
			.load_full()
			.expect("OAuth client should be initialized")
			.headers_map
			.contains_key("Authorization"));
	}

	#[tokio::test(flavor = "multi_thread")]
	#[ignore = "requires application-managed live OAuth startup"]
	async fn test_oauth_client_refresh() {
		force_refresh_token(RedditLane::Direct, RefreshReason::Scheduled).await;
	}

	#[tokio::test(flavor = "multi_thread")]
	#[ignore = "requires application-managed live OAuth startup"]
	async fn test_oauth_token_exists() {
		let client = OAUTH_CLIENT.load_full().expect("OAuth client should be initialized");
		let auth_header = client.headers_map.get("Authorization").unwrap();
		assert!(auth_header.starts_with("Bearer "));
	}

	#[tokio::test(flavor = "multi_thread")]
	#[ignore = "requires application-managed live OAuth startup"]
	async fn test_oauth_headers_len() {
		assert!(OAUTH_CLIENT.load_full().expect("OAuth client should be initialized").headers_map.len() >= 3);
	}

	#[test]
	fn test_creating_device() {
		Device::new(RedditLane::Direct);
	}

	#[test]
	fn startup_rotates_immediately_only_for_identity_policy_forbidden() {
		let forbidden = AuthError::HttpStatus {
			status: 403,
			retry_after: Some(Duration::ZERO),
			retry_after_present: true,
			quota_headers_present: false,
			www_authenticate_present: false,
		};
		assert!(forbidden.is_identity_policy_forbidden());

		let quota_forbidden = AuthError::HttpStatus {
			status: 403,
			retry_after: Some(Duration::from_secs(60)),
			retry_after_present: true,
			quota_headers_present: true,
			www_authenticate_present: false,
		};
		assert!(!quota_forbidden.is_identity_policy_forbidden());

		let credential_forbidden = AuthError::HttpStatus {
			status: 403,
			retry_after: None,
			retry_after_present: false,
			quota_headers_present: false,
			www_authenticate_present: true,
		};
		assert!(!credential_forbidden.is_identity_policy_forbidden());
	}

	#[test]
	fn startup_browser_rotation_replaces_identity_and_transport() {
		let original = MobileSpoofAuth::new(RedditLane::Direct);
		let replacement = MobileSpoofAuth::new(RedditLane::Direct);
		let original_client = client_for_oauth_profile(RedditLane::Direct, OauthTransportProfile::Chrome143Android).unwrap();
		let replacement_client = client_for_oauth_profile(RedditLane::Direct, OauthTransportProfile::Firefox147Android).unwrap();
		let original_device_id = original.device.headers.get("X-Reddit-Device-Id").unwrap().clone();
		let replacement_device_id = replacement.device.headers.get("X-Reddit-Device-Id").unwrap().clone();

		assert_ne!(original_device_id, replacement_device_id);
		assert!(!Arc::ptr_eq(&original_client, &replacement_client));
	}

	#[test]
	fn oauth_failure_classes_are_privacy_safe_and_actionable() {
		for (status, quota_headers_present, www_authenticate_present, expected) in [
			(400, false, false, "request_rejected"),
			(401, false, true, "credential_or_grant_rejected"),
			(403, false, false, "edge_egress_or_identity_policy"),
			(403, false, true, "credential_or_grant_rejected"),
			(403, true, false, "rate_limited"),
			(429, false, false, "rate_limited"),
			(503, false, false, "upstream"),
		] {
			let error = AuthError::HttpStatus {
				status,
				retry_after: None,
				retry_after_present: false,
				quota_headers_present,
				www_authenticate_present,
			};
			assert_eq!(error.failure_class(), expected);
		}
	}

	#[test]
	fn test_creating_mobile_backend() {
		MobileSpoofAuth::new(RedditLane::Direct);
	}

	#[test]
	fn test_refresh_retry_delay_is_exponential_and_capped() {
		assert_eq!(refresh_retry_base_delay(1, None), (Duration::from_secs(5), false));
		assert_eq!(refresh_retry_base_delay(2, None), (Duration::from_secs(10), false));
		assert_eq!(refresh_retry_base_delay(3, None), (Duration::from_secs(20), false));
		assert_eq!(refresh_retry_base_delay(20, None), (MAX_REFRESH_RETRY_DELAY, false));
		let saturated = refresh_retry_delay(20, None);
		assert!((MAX_REFRESH_RETRY_DELAY..=Duration::from_secs(375)).contains(&saturated));
	}

	#[test]
	fn test_active_quota_rotation_matches_generation_and_epoch() {
		let ticket = QuotaRotationTicket {
			lane: RedditLane::Direct,
			generation: 12,
			quota_epoch: 34,
			mode: crate::client::QuotaRotationMode::Emergency,
		};
		OAUTH_IS_ROLLING_OVER.store(false, Ordering::SeqCst);
		{
			let mut rollover_guard = RolloverGuard::acquire(RedditLane::Direct).unwrap();
			rollover_guard.track_quota_rotation(ticket);
			assert!(OAUTH_IS_ROLLING_OVER.load(Ordering::SeqCst));
			assert!(quota_rotation_in_progress(RedditLane::Direct, 12, 34));
			assert!(!quota_rotation_in_progress(RedditLane::Direct, 11, 34));
			assert!(!quota_rotation_in_progress(RedditLane::Direct, 12, 35));
			assert!(!quota_rotation_in_progress(RedditLane::Tor, 12, 34));
		}
		assert!(!quota_rotation_in_progress(RedditLane::Direct, 12, 34));
		assert!(!OAUTH_IS_ROLLING_OVER.load(Ordering::SeqCst));
	}

	#[test]
	fn test_refresh_retry_delay_honors_server_delay() {
		assert_eq!(refresh_retry_base_delay(1, Some(Duration::from_secs(90))), (Duration::from_secs(90), true));
		assert_eq!(refresh_retry_base_delay(1, Some(Duration::from_secs(900))), (MAX_SERVER_RETRY_DELAY, true));
		let delay = refresh_retry_delay(1, Some(Duration::from_secs(90)));
		assert!((Duration::from_secs(90)..=Duration::from_secs(92)).contains(&delay));
	}

	#[test]
	fn test_refresh_backoff_recovers_after_success() {
		let now = Instant::now();
		let mut backoff = RefreshBackoff::default();
		let first = backoff.record_failure(now, None);
		assert!((Duration::from_secs(5)..=Duration::from_millis(6250)).contains(&first));
		assert_eq!(backoff.retry_remaining(now + Duration::from_secs(1)), Some(first - Duration::from_secs(1)));
		let second = backoff.record_failure(now + first, None);
		assert!((Duration::from_secs(10)..=Duration::from_millis(12_500)).contains(&second));
		backoff.record_success();
		assert_eq!(backoff.retry_remaining(now), None);
		assert_eq!(backoff.consecutive_failures, 0);
	}

	#[test]
	fn test_token_refresh_delay_cannot_underflow() {
		assert_eq!(token_refresh_delay(3600, 120), Duration::from_secs(3480));
		assert_eq!(token_refresh_delay(3600, 240), Duration::from_secs(3360));
		assert_eq!(token_refresh_delay(180, 240), Duration::from_secs(90));
		assert_eq!(token_refresh_delay(30, 240), Duration::from_secs(15));
		let sampled = sampled_token_refresh_delay(3600);
		assert!((Duration::from_secs(3360)..=Duration::from_secs(3480)).contains(&sampled));
	}

	#[test]
	fn oauth_transport_profile_matches_android_identity() {
		let mobile = MobileSpoofAuth::new(RedditLane::Direct);
		assert!(mobile.user_agent().contains("Android"));
		for profile in OAUTH_BROWSER_PROFILES {
			assert!(profile.label().ends_with("_android"));
			assert!(client_for_oauth_profile(RedditLane::Direct, profile).is_ok());
		}
	}

	#[test]
	fn oauth_refresh_reuses_only_stable_matching_transport() {
		let backend = MobileSpoofAuth::new(RedditLane::Direct);
		let transport_profile = OauthTransportProfile::Chrome145Android;
		let http_client = client_for_oauth_profile(RedditLane::Direct, transport_profile).unwrap();
		let oauth = Oauth {
			headers_map: HashMap::new(),
			http_client: http_client.clone(),
			refresh_at: Instant::now() + Duration::from_secs(3480),
			backend: backend.clone(),
			transport_profile,
			generation: 4,
			lane: RedditLane::Direct,
		};

		let (stable, stable_profile) = oauth.http_client_for_refresh(false).unwrap();
		assert!(Arc::ptr_eq(&stable, &http_client));
		assert_eq!(stable_profile, transport_profile);

		let (fresh, fresh_profile) = oauth.http_client_for_refresh(true).unwrap();
		assert!(!Arc::ptr_eq(&fresh, &http_client));
		assert_ne!(fresh_profile, transport_profile);
	}

	#[test]
	fn shuffled_oauth_profile_cycle_contains_each_profile_once() {
		let profiles = shuffled_oauth_profiles();
		for profile in OAUTH_BROWSER_PROFILES {
			assert_eq!(profiles.iter().filter(|candidate| **candidate == profile).count(), 1);
		}
	}

	#[test]
	fn newest_android_identity_cohort_excludes_older_versions() {
		for _ in 0..20 {
			let selected = choose_newest_android_app_version(ANDROID_APP_VERSION_LIST);
			assert!(matches!(
				selected,
				"Version 2026.37.0/Build 2637051" | "Version 2026.38.0/Build 2638050" | "Version 2026.39.0/Build 2639031"
			));
		}
	}

	#[test]
	fn test_refresh_reason_selects_stable_or_fresh_identity() {
		let original_backend = MobileSpoofAuth::new(RedditLane::Direct);
		let original_device_id = original_backend.device.headers.get("X-Reddit-Device-Id").unwrap().clone();
		let transport_profile = OauthTransportProfile::Firefox147Android;
		let oauth = Oauth {
			headers_map: HashMap::new(),
			http_client: client_for_oauth_profile(RedditLane::Direct, transport_profile).unwrap(),
			refresh_at: Instant::now() + Duration::from_secs(3480),
			backend: original_backend,
			transport_profile,
			generation: 4,
			lane: RedditLane::Direct,
		};
		assert_eq!(oauth.clone().refresh_at, oauth.refresh_at);

		let (stable, stable_is_fresh) = oauth.refresh_backend(RefreshReason::Scheduled);
		let stable_device_id = stable.device.headers.get("X-Reddit-Device-Id").unwrap().clone();
		assert!(!stable_is_fresh);
		assert_eq!(stable_device_id, original_device_id);

		let (rotated, rotated_is_fresh) = oauth.refresh_backend(RefreshReason::LowRateLimit);
		let rotated_device_id = rotated.device.headers.get("X-Reddit-Device-Id").unwrap().clone();
		assert!(rotated_is_fresh);
		assert_ne!(rotated_device_id, original_device_id);
	}
}
