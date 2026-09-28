use crate::{
	client::{
		claim_quota_rotation, client_for_new_identity, direct_oauth_compatibility_client, install_oauth_client, oauth_client, quota_rotation_still_needed, record_oauth_send,
		OauthTransportProfile, QuotaRotationTicket, GENERIC_WEB_USER_AGENT, OAUTH_IS_ROLLING_OVER, TOR_OAUTH_IS_ROLLING_OVER,
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
const STARTUP_MOBILE_ROTATION_THRESHOLD: u32 = 3;
const MAX_STARTUP_MOBILE_IDENTITY_ROTATIONS: u8 = 1;
const GENERIC_WEB_QUARANTINE_THRESHOLD: u32 = 2;
const GENERIC_WEB_QUARANTINE_DURATION: Duration = Duration::from_secs(60 * 60);
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

// OAuth backend implementations
#[derive(Debug, Clone)]
pub(crate) enum OauthBackendImpl {
	MobileSpoof(MobileSpoofAuth),
	GenericWeb(GenericWebAuth),
}

impl OauthBackend for OauthBackendImpl {
	async fn authenticate(&mut self, client: &wreq::Client) -> Result<OauthResponse, AuthError> {
		match self {
			OauthBackendImpl::MobileSpoof(backend) => backend.authenticate(client).await,
			OauthBackendImpl::GenericWeb(backend) => backend.authenticate(client).await,
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
	fn transport_profile(&self) -> OauthTransportProfile {
		match self {
			Self::MobileSpoof(_) => OauthTransportProfile::MobileAndroid,
			Self::GenericWeb(_) => OauthTransportProfile::GenericWeb,
		}
	}

	fn lane(&self) -> RedditLane {
		match self {
			Self::MobileSpoof(backend) => backend.lane,
			Self::GenericWeb(backend) => backend.lane,
		}
	}

	fn name(&self) -> &'static str {
		match self {
			Self::MobileSpoof(_) => "MobileSpoofAuth",
			Self::GenericWeb(_) => "GenericWebAuth",
		}
	}

	fn alternate(&self) -> Self {
		match self {
			Self::MobileSpoof(backend) => Self::GenericWeb(GenericWebAuth::new(backend.lane)),
			Self::GenericWeb(backend) => Self::MobileSpoof(MobileSpoofAuth::new(backend.lane)),
		}
	}
}

// Spoofed client for Android devices
#[derive(Clone)]
pub struct Oauth {
	pub(crate) headers_map: HashMap<String, String>,
	pub(crate) http_client: Arc<wreq::Client>,
	refresh_at: Instant,
	pub(crate) backend: OauthBackendImpl,
	transport_mode: OauthTransportMode,
	pub(crate) generation: u64,
	pub(crate) lane: RedditLane,
}

struct RefreshedOauth {
	oauth: Oauth,
	fresh_identity: bool,
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
enum OauthTransportMode {
	Aligned(OauthTransportProfile),
	DirectCompatibility,
}

impl OauthTransportMode {
	fn label(self) -> &'static str {
		match self {
			Self::Aligned(profile) => profile.label(),
			Self::DirectCompatibility => "direct_legacy_compat",
		}
	}
}

#[derive(Debug, Default)]
struct StartupRecovery {
	consecutive_mobile_forbidden: u32,
	mobile_identity_generation: u8,
	direct_compatibility_probed: bool,
	consecutive_generic_unauthorized: u32,
	generic_retry_not_before: Option<Instant>,
}

impl StartupRecovery {
	fn record_mobile_failure(&mut self, error: &AuthError) -> bool {
		if error.is_identity_policy_forbidden() {
			self.consecutive_mobile_forbidden = self.consecutive_mobile_forbidden.saturating_add(1);
		} else {
			self.consecutive_mobile_forbidden = 0;
		}

		self.consecutive_mobile_forbidden >= STARTUP_MOBILE_ROTATION_THRESHOLD && self.mobile_identity_generation < MAX_STARTUP_MOBILE_IDENTITY_ROTATIONS
	}

	fn claim_direct_compatibility_probe(&mut self, lane: RedditLane) -> bool {
		if lane != RedditLane::Direct || self.direct_compatibility_probed || self.consecutive_mobile_forbidden < STARTUP_MOBILE_ROTATION_THRESHOLD {
			return false;
		}
		self.direct_compatibility_probed = true;
		true
	}

	fn should_try_generic(&self, now: Instant) -> bool {
		match self.generic_retry_not_before {
			Some(deadline) => now >= deadline,
			None => true,
		}
	}

	fn generic_web_quarantined(&self, now: Instant) -> bool {
		!self.should_try_generic(now)
	}

	fn record_generic_failure(&mut self, error: &AuthError, now: Instant) -> bool {
		if error.is_credential_or_grant_rejected() {
			self.consecutive_generic_unauthorized = self.consecutive_generic_unauthorized.saturating_add(1);
		} else {
			self.consecutive_generic_unauthorized = 0;
			self.generic_retry_not_before = None;
		}

		if self.consecutive_generic_unauthorized >= GENERIC_WEB_QUARANTINE_THRESHOLD {
			self.generic_retry_not_before = Some(now + GENERIC_WEB_QUARANTINE_DURATION);
			true
		} else {
			false
		}
	}

	fn defer_generic(&mut self, now: Instant) {
		self.generic_retry_not_before = Some(now + GENERIC_WEB_QUARANTINE_DURATION);
	}

	fn generic_transport_ready(&mut self) {
		self.generic_retry_not_before = None;
	}

	fn complete_mobile_rotation(&mut self) {
		self.consecutive_mobile_forbidden = 0;
		self.mobile_identity_generation += 1;
	}
}

fn new_startup_mobile_identity(lane: RedditLane) -> Result<(OauthBackendImpl, Arc<wreq::Client>), String> {
	let http_client = client_for_new_identity(lane, OauthTransportProfile::MobileAndroid)?;
	let backend = OauthBackendImpl::MobileSpoof(MobileSpoofAuth::new(lane));
	Ok((backend, http_client))
}

fn client_for_transport_mode(lane: RedditLane, mode: OauthTransportMode) -> Result<Arc<wreq::Client>, String> {
	match mode {
		OauthTransportMode::Aligned(profile) => client_for_new_identity(lane, profile),
		OauthTransportMode::DirectCompatibility if lane == RedditLane::Direct => Ok(direct_oauth_compatibility_client()),
		OauthTransportMode::DirectCompatibility => Err("the legacy OAuth compatibility transport is direct-only".to_string()),
	}
}

async fn wait_for_startup_mobile_identity(lane: RedditLane) -> (OauthBackendImpl, Arc<wreq::Client>) {
	let mut failure_count = 0_u32;
	loop {
		match new_startup_mobile_identity(lane) {
			Ok(identity) => return identity,
			Err(error) => {
				failure_count = failure_count.saturating_add(1);
				let delay = refresh_retry_delay(failure_count, None);
				error!(
					"[⛔] OAuth startup transport construction failed: lane={} backend=MobileSpoofAuth profile={} attempt={failure_count} class=configuration error={error}; retrying_in={delay:?}",
					lane.label(),
					OauthTransportProfile::MobileAndroid.label(),
				);
				tokio::time::sleep(delay).await;
			}
		}
	}
}

impl Oauth {
	/// Create a new OAuth client
	pub(crate) async fn new(lane: RedditLane) -> Self {
		// Keep identities stable across ordinary startup retries. Direct startup
		// compares one legacy transport using the same identity before a single
		// bounded identity/client replacement, avoiding uncontrolled churn.
		let (mut primary, mut primary_http_client) = wait_for_startup_mobile_identity(lane).await;
		let mut fallback = OauthBackendImpl::GenericWeb(GenericWebAuth::new(lane));
		let mut failure_count = 0_u32;
		let mut recovery = StartupRecovery::default();
		let mut fallback_http_client = match client_for_new_identity(lane, fallback.transport_profile()) {
			Ok(client) => Some(client),
			Err(error) => {
				warn!(
					"GenericWebAuth is unavailable because its transport could not be built; MobileSpoofAuth startup will continue: lane={} class=configuration error={error}",
					lane.label(),
				);
				recovery.defer_generic(Instant::now());
				None
			}
		};

		loop {
			let attempt = failure_count.saturating_add(1);
			let mut retry_after = None;
			let aligned_mode = OauthTransportMode::Aligned(primary.transport_profile());
			let repeated_mobile_forbidden = match Self::authenticate_with_backend(&mut primary, primary_http_client.clone(), aligned_mode).await {
				Ok(oauth) => {
					info!(
						"[✅] Successfully created OAuth client: lane={} backend={} identity_profile={} transport={} identity_generation={}",
						lane.label(),
						primary.name(),
						primary.transport_profile().label(),
						aligned_mode.label(),
						recovery.mobile_identity_generation,
					);
					return oauth;
				}
				Err(error) => {
					retry_after = max_duration(retry_after, error.retry_after());
					error!(
						"[⛔] OAuth startup authentication failed: lane={} backend={} identity_profile={} transport={} attempt={attempt} identity_generation={} class={} error={error}",
						lane.label(),
						primary.name(),
						primary.transport_profile().label(),
						aligned_mode.label(),
						recovery.mobile_identity_generation,
						error.failure_class(),
					);
					recovery.record_mobile_failure(&error)
				}
			};

			if recovery.should_try_generic(Instant::now()) {
				if fallback_http_client.is_none() {
					match client_for_new_identity(lane, fallback.transport_profile()) {
						Ok(client) => {
							fallback_http_client = Some(client);
							recovery.generic_transport_ready();
							info!("GenericWebAuth transport recovered: lane={}", lane.label());
						}
						Err(error) => {
							recovery.defer_generic(Instant::now());
							warn!(
								"GenericWebAuth transport remains unavailable; retrying it in one hour: lane={} class=configuration error={error}",
								lane.label(),
							);
						}
					}
				}
				if let Some(fallback_http_client) = &fallback_http_client {
					let fallback_mode = OauthTransportMode::Aligned(fallback.transport_profile());
					match Self::authenticate_with_backend(&mut fallback, fallback_http_client.clone(), fallback_mode).await {
						Ok(oauth) => {
							info!(
								"[✅] Successfully created OAuth client: lane={} backend={} identity_profile={} transport={} identity_generation=0",
								lane.label(),
								fallback.name(),
								fallback.transport_profile().label(),
								fallback_mode.label(),
							);
							return oauth;
						}
						Err(error) => {
							retry_after = max_duration(retry_after, error.retry_after());
							error!(
								"[⛔] OAuth startup authentication failed: lane={} backend={} identity_profile={} transport={} attempt={attempt} identity_generation=0 class={} error={error}",
								lane.label(),
								fallback.name(),
								fallback.transport_profile().label(),
								fallback_mode.label(),
								error.failure_class(),
							);
							if recovery.record_generic_failure(&error, Instant::now()) {
								warn!(
									"Quarantining GenericWebAuth for one hour after repeated credential or grant rejections: lane={}; MobileSpoofAuth startup retries will continue",
									lane.label(),
								);
							}
						}
					}
				}
			}

			if repeated_mobile_forbidden && recovery.claim_direct_compatibility_probe(lane) {
				let compatibility_mode = OauthTransportMode::DirectCompatibility;
				info!(
					"[🔄] Trying one direct OAuth compatibility transport after {STARTUP_MOBILE_ROTATION_THRESHOLD} consecutive MobileSpoofAuth policy 403 responses: lane={} same_identity=true transport={}",
					lane.label(),
					compatibility_mode.label(),
				);
				match client_for_transport_mode(lane, compatibility_mode) {
					Ok(compatibility_client) => match Self::authenticate_with_backend(&mut primary, compatibility_client, compatibility_mode).await {
						Ok(oauth) => {
							info!(
								"[✅] Direct OAuth compatibility transport succeeded: lane={} backend={} identity_profile={} transport={} identity_generation={}",
								lane.label(),
								primary.name(),
								primary.transport_profile().label(),
								compatibility_mode.label(),
								recovery.mobile_identity_generation,
							);
							return oauth;
						}
						Err(error) => {
							retry_after = max_duration(retry_after, error.retry_after());
							warn!(
								"Direct OAuth compatibility transport was also rejected: lane={} backend={} identity_profile={} transport={} same_identity=true class={} error={error}",
								lane.label(),
								primary.name(),
								primary.transport_profile().label(),
								compatibility_mode.label(),
								error.failure_class(),
							);
						}
					},
					Err(error) => warn!(
						"Could not build the direct OAuth compatibility transport: lane={} class=configuration error={error}",
						lane.label()
					),
				}
			}

			if repeated_mobile_forbidden {
				match new_startup_mobile_identity(lane) {
					Ok((replacement, replacement_http_client)) => {
						primary = replacement;
						primary_http_client = replacement_http_client;
						recovery.complete_mobile_rotation();
						warn!(
							"[🔄] Rotated OAuth identity and transport after {STARTUP_MOBILE_ROTATION_THRESHOLD} consecutive MobileSpoofAuth 403 responses: lane={} identity_generation={}; startup backoff is unchanged",
							lane.label(),
							recovery.mobile_identity_generation,
						);
					}
					Err(error) => warn!(
						"Could not rotate OAuth identity and transport; retaining the current identity: lane={} class=configuration error={error}",
						lane.label(),
					),
				}
			}

			failure_count = failure_count.saturating_add(1);
			let delay = refresh_retry_delay(failure_count, retry_after);
			warn!(
				"[⏳] OAuth startup attempt failed: lane={} attempt={attempt} generic_web_available={} generic_web_quarantined={} retrying_in={delay:?}",
				lane.label(),
				fallback_http_client.is_some(),
				recovery.generic_web_quarantined(Instant::now()),
			);
			tokio::time::sleep(delay).await;
		}
	}

	async fn authenticate_with_backend(backend: &mut OauthBackendImpl, http_client: Arc<wreq::Client>, transport_mode: OauthTransportMode) -> Result<Self, AuthError> {
		let oauth_timeout = match backend.lane() {
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
			transport_mode,
			generation: 0,
			lane: backend.lane(),
		})
	}

	fn refresh_backend(&self, reason: RefreshReason, fallback: bool) -> (OauthBackendImpl, bool) {
		match (reason, fallback) {
			(RefreshReason::LowRateLimit, false) => (OauthBackendImpl::MobileSpoof(MobileSpoofAuth::new(self.lane)), true),
			(RefreshReason::LowRateLimit, true) => (OauthBackendImpl::GenericWeb(GenericWebAuth::new(self.lane)), true),
			(RefreshReason::Scheduled | RefreshReason::Unauthorized, false) => (self.backend.clone(), false),
			(RefreshReason::Scheduled | RefreshReason::Unauthorized, true) => (self.backend.alternate(), true),
		}
	}

	async fn refreshed(&self, reason: RefreshReason) -> Result<RefreshedOauth, RefreshError> {
		let (mut primary, primary_is_fresh) = self.refresh_backend(reason, false);
		let primary_name = primary.name();
		let primary_http_client = self
			.http_client_for_refresh(&primary, primary_is_fresh)
			.map_err(|error| RefreshError::configuration(primary_name, error))?;
		let primary_transport_mode = self.transport_mode_for_refresh(&primary);
		match Self::authenticate_with_backend(&mut primary, primary_http_client, primary_transport_mode).await {
			Ok(oauth) => Ok(RefreshedOauth {
				oauth,
				fresh_identity: primary_is_fresh,
			}),
			Err(primary_error) => {
				warn!("OAuth {} refresh with {primary_name} failed: {primary_error}", reason.label());
				let (mut fallback, fallback_is_fresh) = self.refresh_backend(reason, true);
				let fallback_name = fallback.name();
				let fallback_http_client = match self.http_client_for_refresh(&fallback, fallback_is_fresh) {
					Ok(client) => client,
					Err(error) => {
						return Err(RefreshError {
							primary_name,
							primary_error,
							fallback_name,
							fallback_error: AuthError::Configuration(error),
						});
					}
				};
				let fallback_transport_mode = self.transport_mode_for_refresh(&fallback);
				match Self::authenticate_with_backend(&mut fallback, fallback_http_client, fallback_transport_mode).await {
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

	fn http_client_for_refresh(&self, backend: &OauthBackendImpl, fresh_identity: bool) -> Result<Arc<wreq::Client>, String> {
		let target_mode = self.transport_mode_for_refresh(backend);
		if can_reuse_transport(self.transport_mode, target_mode, fresh_identity) {
			Ok(self.http_client.clone())
		} else {
			client_for_transport_mode(self.lane, target_mode)
		}
	}

	fn transport_mode_for_refresh(&self, backend: &OauthBackendImpl) -> OauthTransportMode {
		if self.lane == RedditLane::Direct && self.transport_mode == OauthTransportMode::DirectCompatibility && matches!(backend, OauthBackendImpl::MobileSpoof(_)) {
			OauthTransportMode::DirectCompatibility
		} else {
			OauthTransportMode::Aligned(backend.transport_profile())
		}
	}

	pub fn user_agent(&self) -> &str {
		self.backend.user_agent()
	}
}

fn can_reuse_transport(current: OauthTransportMode, target: OauthTransportMode, fresh_identity: bool) -> bool {
	!fresh_identity && current == target
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

	fn is_http_status(&self, expected: u16) -> bool {
		matches!(self, Self::HttpStatus { status, .. } if *status == expected)
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

	fn is_credential_or_grant_rejected(&self) -> bool {
		matches!(
			self,
			Self::HttpStatus { status: 401, .. }
				| Self::HttpStatus {
					status: 403,
					www_authenticate_present: true,
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
	primary_name: &'static str,
	primary_error: AuthError,
	fallback_name: &'static str,
	fallback_error: AuthError,
}

impl RefreshError {
	fn configuration(backend_name: &'static str, error: String) -> Self {
		Self {
			primary_name: backend_name,
			primary_error: AuthError::Configuration(error),
			fallback_name: "Tor transport",
			fallback_error: AuthError::Configuration("no isolated client was available".to_string()),
		}
	}

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

// GenericWebAuth backend - simple web-based authentication
#[derive(Debug, Clone)]
pub struct GenericWebAuth {
	lane: RedditLane,
	device_id: String,
	user_agent: String,
	additional_headers: HashMap<String, String>,
}

impl GenericWebAuth {
	fn new(lane: RedditLane) -> Self {
		// Generate random 20-character alphanumeric device_id
		let device_id: String = (0..20)
			.map(|_| {
				let idx = fastrand::usize(..62);
				let chars = b"abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789";
				chars[idx] as char
			})
			.collect();

		info!("[🔄] Created a stable GenericWebAuth identity: lane={}", lane.label());

		Self {
			lane,
			device_id,
			user_agent: GENERIC_WEB_USER_AGENT.to_owned(),
			additional_headers: HashMap::new(),
		}
	}
}

impl OauthBackend for GenericWebAuth {
	async fn authenticate(&mut self, client: &wreq::Client) -> Result<OauthResponse, AuthError> {
		// Construct URL for OAuth token
		let origin = self.lane.auth_origin();
		let url = format!("{}/api/v1/access_token", origin.base);
		record_oauth_send(self.lane);
		let mut builder = client.post(&url);

		// Add minimal headers
		builder = builder.header("Host", origin.host);
		builder = builder.header("User-Agent", &self.user_agent);
		builder = builder.header("Accept", "*/*");
		builder = builder.header("Accept-Language", "en-US,en;q=0.5");
		// builder = builder.header("Accept-Encoding", "gzip, deflate, br, zstd");
		builder = builder.header("Authorization", "Basic M1hmQkpXbGlIdnFBQ25YcmZJWWxMdzo=");
		builder = builder.header("Content-Type", "application/x-www-form-urlencoded");
		builder = builder.header("Sec-GPC", "1");
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
		self.additional_headers.insert("Origin".to_owned(), origin.base.to_owned());
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
	fn android(lane: RedditLane) -> Self {
		// Generate uuid
		let uuid = uuid::Uuid::new_v4().to_string();

		// Generate random user-agent
		let android_app_version = choose_newest_android_app_version(ANDROID_APP_VERSION_LIST).to_string();
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
		let client = client_for_new_identity(RedditLane::Direct, OauthTransportProfile::MobileAndroid).unwrap();
		let response = backend.authenticate(client.as_ref()).await;
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
		let mut backend = GenericWebAuth::new(RedditLane::Direct);
		let client = client_for_new_identity(RedditLane::Direct, OauthTransportProfile::GenericWeb).unwrap();
		let response = backend.authenticate(client.as_ref()).await;
		assert!(response.is_ok());
		let response = response.unwrap();
		assert!(!response.token.is_empty());
		assert!(response.expires_in > 0);
		assert!(!backend.user_agent().is_empty());
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
	fn startup_recovery_rotates_mobile_identity_once() {
		let forbidden = AuthError::HttpStatus {
			status: 403,
			retry_after: Some(Duration::ZERO),
			retry_after_present: true,
			quota_headers_present: false,
			www_authenticate_present: false,
		};
		let mut recovery = StartupRecovery::default();

		assert!(!recovery.record_mobile_failure(&forbidden));
		assert!(!recovery.record_mobile_failure(&forbidden));
		assert!(recovery.record_mobile_failure(&forbidden));
		assert!(recovery.claim_direct_compatibility_probe(RedditLane::Direct));
		assert!(!recovery.claim_direct_compatibility_probe(RedditLane::Direct));
		assert_eq!(
			refresh_retry_base_delay(STARTUP_MOBILE_ROTATION_THRESHOLD, forbidden.retry_after()),
			(Duration::from_secs(20), false),
		);
		recovery.complete_mobile_rotation();
		assert_eq!(recovery.mobile_identity_generation, 1);
		for _ in 0..STARTUP_MOBILE_ROTATION_THRESHOLD * 2 {
			assert!(!recovery.record_mobile_failure(&forbidden));
		}
		assert_eq!(recovery.mobile_identity_generation, MAX_STARTUP_MOBILE_IDENTITY_ROTATIONS);
	}

	#[test]
	fn startup_recovery_never_probes_legacy_transport_on_tor() {
		let forbidden = AuthError::HttpStatus {
			status: 403,
			retry_after: Some(Duration::ZERO),
			retry_after_present: true,
			quota_headers_present: false,
			www_authenticate_present: false,
		};
		let mut recovery = StartupRecovery::default();
		for _ in 0..STARTUP_MOBILE_ROTATION_THRESHOLD {
			recovery.record_mobile_failure(&forbidden);
		}
		assert!(!recovery.claim_direct_compatibility_probe(RedditLane::Tor));
		assert!(!recovery.direct_compatibility_probed);
	}

	#[test]
	fn startup_recovery_rotates_only_on_identity_policy_forbidden() {
		let quota_forbidden = AuthError::HttpStatus {
			status: 403,
			retry_after: Some(Duration::from_secs(60)),
			retry_after_present: true,
			quota_headers_present: true,
			www_authenticate_present: false,
		};
		let mut recovery = StartupRecovery::default();

		for _ in 0..STARTUP_MOBILE_ROTATION_THRESHOLD * 2 {
			assert!(!recovery.record_mobile_failure(&quota_forbidden));
		}
		assert_eq!(recovery.mobile_identity_generation, 0);
		assert_eq!(recovery.consecutive_mobile_forbidden, 0);

		let credential_forbidden = AuthError::HttpStatus {
			status: 403,
			retry_after: None,
			retry_after_present: false,
			quota_headers_present: false,
			www_authenticate_present: true,
		};
		for _ in 0..STARTUP_MOBILE_ROTATION_THRESHOLD * 2 {
			assert!(!recovery.record_mobile_failure(&credential_forbidden));
		}
		assert_eq!(recovery.mobile_identity_generation, 0);
	}

	#[test]
	fn startup_mobile_rotation_replaces_identity_and_transport() {
		let (original, original_client) = new_startup_mobile_identity(RedditLane::Direct).unwrap();
		let (replacement, replacement_client) = new_startup_mobile_identity(RedditLane::Direct).unwrap();
		let original_device_id = match original {
			OauthBackendImpl::MobileSpoof(backend) => backend.device.headers.get("X-Reddit-Device-Id").unwrap().clone(),
			OauthBackendImpl::GenericWeb(_) => unreachable!(),
		};
		let replacement_device_id = match replacement {
			OauthBackendImpl::MobileSpoof(backend) => backend.device.headers.get("X-Reddit-Device-Id").unwrap().clone(),
			OauthBackendImpl::GenericWeb(_) => unreachable!(),
		};

		assert_ne!(original_device_id, replacement_device_id);
		assert!(!Arc::ptr_eq(&original_client, &replacement_client));
	}

	#[test]
	fn startup_recovery_quarantines_repeated_generic_unauthorized() {
		let now = Instant::now();
		let unauthorized = AuthError::HttpStatus {
			status: 401,
			retry_after: None,
			retry_after_present: false,
			quota_headers_present: false,
			www_authenticate_present: true,
		};
		let mut recovery = StartupRecovery::default();

		assert!(!recovery.record_generic_failure(&unauthorized, now));
		assert!(recovery.record_generic_failure(&unauthorized, now));
		assert!(recovery.generic_web_quarantined(now));
		assert!(!recovery.should_try_generic(now + GENERIC_WEB_QUARANTINE_DURATION - Duration::from_secs(1)));
		assert!(recovery.should_try_generic(now + GENERIC_WEB_QUARANTINE_DURATION));

		let upstream = AuthError::HttpStatus {
			status: 503,
			retry_after: None,
			retry_after_present: false,
			quota_headers_present: false,
			www_authenticate_present: false,
		};
		assert!(!recovery.record_generic_failure(&upstream, now + GENERIC_WEB_QUARANTINE_DURATION));
		assert!(recovery.should_try_generic(now + GENERIC_WEB_QUARANTINE_DURATION));
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
			assert_eq!(error.is_credential_or_grant_rejected(), expected == "credential_or_grant_rejected");
		}
	}

	#[test]
	fn test_creating_backends() {
		// Test that both backends can be created
		MobileSpoofAuth::new(RedditLane::Direct);
		GenericWebAuth::new(RedditLane::Direct);
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
	fn oauth_transport_profiles_match_backend_identities() {
		let mobile = OauthBackendImpl::MobileSpoof(MobileSpoofAuth::new(RedditLane::Direct));
		let generic = OauthBackendImpl::GenericWeb(GenericWebAuth::new(RedditLane::Direct));

		assert_eq!(mobile.transport_profile(), OauthTransportProfile::MobileAndroid);
		assert!(!mobile.transport_profile().skips_emulation_headers());
		assert_eq!(
			mobile.transport_profile().emulation_profile(),
			(wreq_util::Emulation::OkHttp4_12, wreq_util::EmulationOS::Android)
		);
		assert!(mobile.user_agent().contains("Android"));
		assert_eq!(generic.transport_profile(), OauthTransportProfile::GenericWeb);
		assert!(generic.transport_profile().skips_emulation_headers());
		assert_eq!(
			generic.transport_profile().emulation_profile(),
			(wreq_util::Emulation::Firefox147, wreq_util::EmulationOS::Windows)
		);
		assert_eq!(generic.user_agent(), GENERIC_WEB_USER_AGENT);
	}

	#[test]
	fn oauth_refresh_reuses_only_stable_matching_transport() {
		let backend = OauthBackendImpl::MobileSpoof(MobileSpoofAuth::new(RedditLane::Direct));
		let http_client = client_for_new_identity(RedditLane::Direct, backend.transport_profile()).unwrap();
		let oauth = Oauth {
			headers_map: HashMap::new(),
			http_client: http_client.clone(),
			refresh_at: Instant::now() + Duration::from_secs(3480),
			backend: backend.clone(),
			transport_mode: OauthTransportMode::Aligned(OauthTransportProfile::MobileAndroid),
			generation: 4,
			lane: RedditLane::Direct,
		};

		let stable = oauth.http_client_for_refresh(&backend, false).unwrap();
		assert!(Arc::ptr_eq(&stable, &http_client));

		let fresh = oauth.http_client_for_refresh(&backend, true).unwrap();
		assert!(!Arc::ptr_eq(&fresh, &http_client));

		let alternate = backend.alternate();
		let alternate_client = oauth.http_client_for_refresh(&alternate, false).unwrap();
		assert!(!Arc::ptr_eq(&alternate_client, &http_client));

		assert!(can_reuse_transport(
			OauthTransportMode::Aligned(OauthTransportProfile::MobileAndroid),
			OauthTransportMode::Aligned(OauthTransportProfile::MobileAndroid),
			false
		));
		assert!(!can_reuse_transport(
			OauthTransportMode::Aligned(OauthTransportProfile::MobileAndroid),
			OauthTransportMode::Aligned(OauthTransportProfile::MobileAndroid),
			true
		));
		assert!(!can_reuse_transport(
			OauthTransportMode::Aligned(OauthTransportProfile::MobileAndroid),
			OauthTransportMode::Aligned(OauthTransportProfile::GenericWeb),
			false
		));
	}

	#[test]
	fn direct_compatibility_transport_persists_for_mobile_refreshes() {
		let backend = OauthBackendImpl::MobileSpoof(MobileSpoofAuth::new(RedditLane::Direct));
		let compatibility_client = direct_oauth_compatibility_client();
		let oauth = Oauth {
			headers_map: HashMap::new(),
			http_client: compatibility_client.clone(),
			refresh_at: Instant::now() + Duration::from_secs(3480),
			backend: backend.clone(),
			transport_mode: OauthTransportMode::DirectCompatibility,
			generation: 4,
			lane: RedditLane::Direct,
		};

		let stable = oauth.http_client_for_refresh(&backend, false).unwrap();
		assert!(Arc::ptr_eq(&stable, &compatibility_client));
		let fresh = oauth.http_client_for_refresh(&backend, true).unwrap();
		assert!(Arc::ptr_eq(&fresh, &compatibility_client));
		assert_eq!(oauth.transport_mode_for_refresh(&backend), OauthTransportMode::DirectCompatibility);

		let generic = backend.alternate();
		assert_eq!(oauth.transport_mode_for_refresh(&generic), OauthTransportMode::Aligned(OauthTransportProfile::GenericWeb));
	}

	#[test]
	fn newest_android_identity_cohort_excludes_older_versions() {
		let versions = [
			"Version 2022.40.0/Build 624782",
			"Version 2024.22.1/Build 1652272",
			"Version 2023.48.0/Build 1319123",
			"Version 2024.43.0/Build 1972250",
			"Version 2024.47.0/Build 2029755",
		];
		for _ in 0..20 {
			let selected = choose_newest_android_app_version(&versions);
			assert!(matches!(selected, "Version 2024.43.0/Build 1972250" | "Version 2024.47.0/Build 2029755"));
		}
	}

	#[test]
	fn test_refresh_reason_selects_stable_or_fresh_identity() {
		let original_backend = OauthBackendImpl::MobileSpoof(MobileSpoofAuth::new(RedditLane::Direct));
		let original_device_id = match &original_backend {
			OauthBackendImpl::MobileSpoof(backend) => backend.device.headers.get("X-Reddit-Device-Id").unwrap().clone(),
			OauthBackendImpl::GenericWeb(_) => unreachable!(),
		};
		let oauth = Oauth {
			headers_map: HashMap::new(),
			http_client: crate::client::CLIENT.clone(),
			refresh_at: Instant::now() + Duration::from_secs(3480),
			backend: original_backend,
			transport_mode: OauthTransportMode::DirectCompatibility,
			generation: 4,
			lane: RedditLane::Direct,
		};
		assert_eq!(oauth.clone().refresh_at, oauth.refresh_at);

		let (stable, stable_is_fresh) = oauth.refresh_backend(RefreshReason::Scheduled, false);
		let (_, stable_fallback_is_fresh) = oauth.refresh_backend(RefreshReason::Scheduled, true);
		let stable_device_id = match stable {
			OauthBackendImpl::MobileSpoof(backend) => backend.device.headers.get("X-Reddit-Device-Id").unwrap().clone(),
			OauthBackendImpl::GenericWeb(_) => unreachable!(),
		};
		assert!(!stable_is_fresh);
		assert!(stable_fallback_is_fresh);
		assert_eq!(stable_device_id, original_device_id);

		let (rotated, rotated_is_fresh) = oauth.refresh_backend(RefreshReason::LowRateLimit, false);
		let (_, rotated_fallback_is_fresh) = oauth.refresh_backend(RefreshReason::LowRateLimit, true);
		let rotated_device_id = match rotated {
			OauthBackendImpl::MobileSpoof(backend) => backend.device.headers.get("X-Reddit-Device-Id").unwrap().clone(),
			OauthBackendImpl::GenericWeb(_) => unreachable!(),
		};
		assert!(rotated_is_fresh);
		assert!(rotated_fallback_is_fresh);
		assert_ne!(rotated_device_id, original_device_id);
	}

	#[test]
	fn test_alternate_backend_changes_kind() {
		let mobile = OauthBackendImpl::MobileSpoof(MobileSpoofAuth::new(RedditLane::Direct));
		let generic = mobile.alternate();
		assert!(matches!(generic, OauthBackendImpl::GenericWeb(_)));
		assert!(matches!(generic.alternate(), OauthBackendImpl::MobileSpoof(_)));
	}
}
