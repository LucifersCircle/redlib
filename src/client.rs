use crate::dbg_msg;
use crate::oauth::{
	force_refresh_token, quota_rotation_in_progress, refresh_backoff_remaining, spawn_rate_limit_refresh, token_daemon, token_refresh_notify, Oauth, RefreshReason,
};
use crate::reddit_graphql::{CommentsRequest, ErrorKind as GraphqlErrorKind, GraphqlError};
use crate::reddit_lane::{RedditLane, RedditOrigin, PRIMARY_REDDIT_LANE, TOR_FALLBACK_CONFIG};
use crate::server::RequestExt;
use crate::timing::{positive_jitter, proportional_positive_jitter};
use crate::utils::format_url;
use arc_swap::ArcSwapOption;
use cached::proc_macro::cached;
use futures_lite::{future::Boxed, FutureExt};
use hyper::{body::Buf, header, Body, Request as HyperRequest, Response as HyperResponse};
use log::{error, info, trace, warn};
use percent_encoding::{percent_encode, CONTROLS};
use serde_json::Value;
use std::borrow::Cow;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::env;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::result::Result;
use std::sync::atomic::Ordering;
use std::sync::atomic::{AtomicBool, AtomicU64};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, Instant, SystemTime};
use tokio::sync::{watch, Mutex as AsyncMutex, Notify, Semaphore, SemaphorePermit};
use wreq::dns::{Addrs, Name, Resolve, Resolving};
use wreq::redirect::Policy;
use wreq::{header as wreq_header, Client as WreqClient, Method, Proxy, Response as WreqResponse};
use wreq_util::{Emulation, Platform, Profile};

const REDDIT_URL_BASE: &str = "https://oauth.reddit.com";

const REDDIT_SHORT_URL_BASE: &str = "https://redd.it";
const REDDIT_SHORT_URL_BASE_HOST: &str = "redd.it";

const ALTERNATIVE_REDDIT_URL_BASE: &str = "https://www.reddit.com";
const ALTERNATIVE_REDDIT_URL_BASE_HOST: &str = "www.reddit.com";
const ALTERNATIVE_REDDIT_DNS_HOST: &str = "dualstack.reddit.map.fastly.net";

pub static CLIENT: LazyLock<Arc<WreqClient>> = LazyLock::new(|| Arc::new(build_client()));

pub static OAUTH_CLIENT: LazyLock<ArcSwapOption<Oauth>> = LazyLock::new(ArcSwapOption::empty);

pub(crate) static TOR_OAUTH_CLIENT: LazyLock<ArcSwapOption<Oauth>> = LazyLock::new(ArcSwapOption::empty);

pub static OAUTH_IS_ROLLING_OVER: AtomicBool = AtomicBool::new(false);
pub(crate) static TOR_OAUTH_IS_ROLLING_OVER: AtomicBool = AtomicBool::new(false);
static TOR_WARMUP_STARTED: AtomicBool = AtomicBool::new(false);
static DIRECT_OAUTH_WARMUP_STARTED: AtomicBool = AtomicBool::new(false);

const DEFAULT_MAX_CONCURRENT_API_REQUESTS: usize = 8;
const MAX_CONFIGURED_API_REQUESTS: usize = 64;
const FAILURE_WINDOW: Duration = Duration::from_secs(10);
const FAILURE_COOLDOWN: Duration = Duration::from_secs(10);
const FAILURE_THRESHOLD: u8 = 3;
const MAX_TRANSPORT_RETRIES: u8 = 2;
const TRANSPORT_RETRY_BASE_DELAY: Duration = Duration::from_millis(250);
const TRANSPORT_RETRY_TIMEOUT: Duration = Duration::from_secs(15);
const MAX_UPSTREAM_COOLDOWN_WAITS: u8 = 16;
const UPSTREAM_RECOVERY_BUDGET: Duration = Duration::from_secs(45);
const CAPACITY_WAIT_BUDGET: Duration = Duration::from_secs(15);
static DIRECT_RECOVERY_PROBE: Semaphore = Semaphore::const_new(1);
static TOR_RECOVERY_PROBE: Semaphore = Semaphore::const_new(1);
const DEFAULT_RATE_LIMIT_COOLDOWN: Duration = Duration::from_secs(10);
const MAX_RATE_LIMIT_COOLDOWN: Duration = Duration::from_secs(600);
const RATE_LIMIT_COOLDOWN_MARGIN: Duration = Duration::from_secs(2);
const LOW_RATE_LIMIT_THRESHOLD: u16 = 10;
const QUOTA_ROTATION_MIN_RESET_REMAINING: Duration = Duration::from_secs(120);
const EMERGENCY_QUOTA_ROTATION_MIN_RESET_REMAINING: Duration = Duration::from_secs(30);
// Leave enough allowance to serve through a slow Tor OAuth refresh. Only
// preserve the current Tor identity when the reset is genuinely imminent.
const TOR_LOW_RATE_LIMIT_THRESHOLD: u16 = 30;
const TOR_QUOTA_ROTATION_MIN_RESET_REMAINING: Duration = Duration::from_secs(5);
const QUOTA_SAFETY_RESERVE: u16 = 5;
const LOCAL_QUOTA_RETRY_BUDGET: Duration = Duration::from_secs(2);
const LOCAL_QUOTA_RETRY_INTERVAL: Duration = Duration::from_millis(650);
const MAX_LOCAL_QUOTA_RETRIES: u8 = 3;
// Tor token creation can take several seconds. Extend only a wait owned by an
// active quota refresh; ordinary quota resets still use the short local budget.
const TOR_QUOTA_REFRESH_RETRY_BUDGET: Duration = Duration::from_secs(10);
const MAX_TOR_QUOTA_REFRESH_RETRIES: u8 = 16;
const TOR_QUOTA_SPILLOVER_TIMEOUT: Duration = Duration::from_secs(3);
const DIRECT_STANDBY_TIMEOUT: Duration = Duration::from_secs(15);
const TOR_PRIMARY_STANDBY_DELAY: Duration = Duration::from_secs(8);
const TOR_STANDBY_RECOVERY_TIMEOUT: Duration = Duration::from_secs(30);
const TOR_FIRST_PAGE_BUDGET: Duration = Duration::from_secs(75);
const REJECTED_SESSION_REFRESH_INTERVAL: Duration = Duration::from_secs(5);
static DIRECT_STANDBY_CAPACITY: Semaphore = Semaphore::const_new(2);
static DIRECT_STANDBY_ATTEMPTS: AtomicU64 = AtomicU64::new(0);
const EMERGENCY_QUOTA_REFRESH_RETRY: Duration = Duration::from_secs(2);
const OAUTH_STARTUP_RETRY: Duration = Duration::from_secs(5);
const JSON_FLIGHT_ABORTED_ERROR: &str = "The shared Reddit request ended before producing a response";
const EDGE_THROTTLE_INITIAL_COOLDOWN: Duration = Duration::from_secs(5);
const EDGE_THROTTLE_MAX_COOLDOWN: Duration = Duration::from_secs(300);
const MAX_API_REDIRECTS: usize = 3;
const TRAFFIC_SUMMARY_INTERVAL: Duration = Duration::from_secs(300);

static DIRECT_REDDIT_API_CONCURRENCY: LazyLock<Semaphore> = LazyLock::new(|| {
	let configured = max_concurrent_api_requests();
	info!("Reddit API concurrency limit: lane=direct limit={configured}");
	Semaphore::new(configured)
});
static TOR_REDDIT_API_CONCURRENCY: LazyLock<Semaphore> = LazyLock::new(|| {
	let configured = max_concurrent_api_requests();
	info!("Reddit API concurrency limit: lane=tor limit={configured}");
	Semaphore::new(configured)
});
static DIRECT_UPSTREAM_GUARD: LazyLock<Mutex<UpstreamGuard>> = LazyLock::new(|| Mutex::new(UpstreamGuard::new(RedditLane::Direct)));
static TOR_UPSTREAM_GUARD: LazyLock<Mutex<UpstreamGuard>> = LazyLock::new(|| Mutex::new(UpstreamGuard::new(RedditLane::Tor)));
static DIRECT_QUOTA_NOTIFY: LazyLock<Notify> = LazyLock::new(Notify::new);
static TOR_QUOTA_NOTIFY: LazyLock<Notify> = LazyLock::new(Notify::new);
static LOGICAL_JSON_COUNTS: LazyLock<[AtomicU64; 7]> = LazyLock::new(|| std::array::from_fn(|_| AtomicU64::new(0)));
static ADMITTED_JSON_COUNTS: LazyLock<[AtomicU64; 7]> = LazyLock::new(|| std::array::from_fn(|_| AtomicU64::new(0)));
static API_SEND_COUNTS: LazyLock<[AtomicU64; 7]> = LazyLock::new(|| std::array::from_fn(|_| AtomicU64::new(0)));
static API_LANE_SENDS: LazyLock<[AtomicU64; 2]> = LazyLock::new(|| std::array::from_fn(|_| AtomicU64::new(0)));
static INBOUND_ROUTE_COUNTS: LazyLock<[AtomicU64; 9]> = LazyLock::new(|| std::array::from_fn(|_| AtomicU64::new(0)));
static INBOUND_METHOD_COUNTS: LazyLock<[AtomicU64; 3]> = LazyLock::new(|| std::array::from_fn(|_| AtomicU64::new(0)));
static INBOUND_STATUS_COUNTS: LazyLock<[AtomicU64; 5]> = LazyLock::new(|| std::array::from_fn(|_| AtomicU64::new(0)));
static LOCAL_DENIAL_COUNTS: LazyLock<[AtomicU64; 3]> = LazyLock::new(|| std::array::from_fn(|_| AtomicU64::new(0)));
static REDIRECT_HOPS: AtomicU64 = AtomicU64::new(0);
static CANONICAL_HEAD_SENDS: AtomicU64 = AtomicU64::new(0);
static MEDIA_SENDS: AtomicU64 = AtomicU64::new(0);
static MEDIA_DESTINATION_SENDS: LazyLock<[AtomicU64; 6]> = LazyLock::new(|| std::array::from_fn(|_| AtomicU64::new(0)));
static MEDIA_RESULT_COUNTS: LazyLock<[AtomicU64; 6]> = LazyLock::new(|| std::array::from_fn(|_| AtomicU64::new(0)));
static OAUTH_LANE_SENDS: LazyLock<[AtomicU64; 2]> = LazyLock::new(|| std::array::from_fn(|_| AtomicU64::new(0)));
static TOR_FALLBACKS: AtomicU64 = AtomicU64::new(0);
static TOR_QUOTA_SPILLOVER_ATTEMPTS: AtomicU64 = AtomicU64::new(0);
static TOR_QUOTA_SPILLOVER_SUCCESSES: AtomicU64 = AtomicU64::new(0);
static LAST_TRAFFIC_SUMMARY: LazyLock<Mutex<Instant>> = LazyLock::new(|| Mutex::new(Instant::now()));
static COMMENT_JSON_KEYS: LazyLock<Mutex<HashSet<u64>>> = LazyLock::new(|| Mutex::new(HashSet::new()));

type JsonRequestKey = (String, bool);
type JsonRequestResult = Result<Value, String>;
type JsonFlightMap = AsyncMutex<HashMap<JsonRequestKey, JsonFlight>>;

#[derive(Clone)]
struct JsonFlight {
	id: u64,
	receiver: watch::Receiver<Option<JsonRequestResult>>,
}

static JSON_FLIGHTS: LazyLock<JsonFlightMap> = LazyLock::new(|| AsyncMutex::new(HashMap::new()));
static NEXT_JSON_FLIGHT_ID: AtomicU64 = AtomicU64::new(1);

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
enum CooldownReason {
	RateLimit,
	EdgeThrottle,
	UpstreamFailures,
}

impl CooldownReason {
	fn message(self) -> &'static str {
		match self {
			Self::RateLimit => "Reddit requests are temporarily paused until the current rate-limit window resets",
			Self::EdgeThrottle => "Reddit is temporarily rejecting this instance; upstream retries are being slowed",
			Self::UpstreamFailures => "Reddit requests are temporarily paused after repeated upstream failures",
		}
	}
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
struct AdmissionDenied {
	delay: Duration,
	reason: CooldownReason,
	reserve_exhausted: bool,
	local_quota_retry: bool,
	source: &'static str,
}

// Reddit's REST and GraphQL APIs report independent allowances for one token.
// Endpoint controls and transport capacity stay on their original lane.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
enum QuotaBudget {
	Rest,
	Graphql,
}
impl QuotaBudget {
	fn for_endpoint(endpoint: usize) -> Self {
		if endpoint == 4 {
			Self::Graphql
		} else {
			Self::Rest
		}
	}
	fn endpoint(self) -> usize {
		match self {
			Self::Rest => 0,
			Self::Graphql => 4,
		}
	}
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
struct QuotaSpilloverTicket {
	budget: QuotaBudget,
	generation: u64,
	quota_epoch: u64,
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
enum TorRetryReason {
	None,
	TransportUnavailable,
	SessionUnavailable,
	EdgeRejected,
	QuotaReserve(QuotaSpilloverTicket),
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
enum LaneRecoveryReason {
	None,
	TransportFailure,
	UpstreamCooldown(Duration),
	QuotaCooldown(Duration),
	TorQuotaRefreshWait(QuotaRefreshWaitTicket),
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
struct EdgeThrottleDecision {
	delay: Duration,
	consecutive_failures: u8,
	started_cooldown: bool,
	episode_seconds: u64,
	current_generation: u64,
	identity_age_seconds: u64,
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
struct EdgeRecovery {
	consecutive_failures: u8,
	episode_seconds: u64,
	current_generation: u64,
	identity_age_seconds: u64,
}

#[derive(Debug, Clone, Copy, Default, Eq, PartialEq)]
enum EdgeCircuitState {
	#[default]
	Closed,
	Open {
		until: Instant,
	},
	HalfOpen {
		epoch: u64,
		expires_at: Instant,
	},
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
struct EdgeAttempt {
	endpoint: usize,
	epoch: u64,
	half_open: bool,
}

#[derive(Debug)]
struct UpstreamAttempt {
	lane: RedditLane,
	edge: EdgeAttempt,
	generation: u64,
	quota_epoch: u64,
	request_id: u64,
	quota_consumption_watermark: u64,
	discovery_probe: bool,
	sent: bool,
	quota_reconciled: bool,
	completed: bool,
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub(crate) enum QuotaRotationMode {
	Proactive,
	Emergency,
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub(crate) struct QuotaRotationTicket {
	pub(crate) lane: RedditLane,
	pub(crate) generation: u64,
	pub(crate) quota_epoch: u64,
	pub(crate) mode: QuotaRotationMode,
}

impl UpstreamAttempt {
	fn mark_sent(&mut self) {
		self.sent = true;
	}

	fn complete(&mut self) {
		self.completed = true;
	}
}

impl Drop for UpstreamAttempt {
	fn drop(&mut self) {
		if !self.completed || !self.quota_reconciled {
			let should_notify = self.discovery_probe;
			upstream_guard(self.lane).abandon_attempt(Instant::now(), self);
			if should_notify {
				quota_notify(self.lane).notify_waiters();
			}
		}
	}
}

#[derive(Debug, Clone, Copy)]
enum QuotaWindow {
	Unknown { probe_in_flight: bool },
	Unreported,
	Known { available: u16, reset_at: Instant },
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
enum QuotaReserveError {
	ReserveExhausted(Duration),
	StaleGeneration,
}

#[derive(Debug)]
struct QuotaGovernor {
	generation: u64,
	epoch: u64,
	next_request_id: u64,
	outstanding: u16,
	rollover_reserve: u16,
	headerless_consumption: u64,
	window: QuotaWindow,
}

impl Default for QuotaGovernor {
	fn default() -> Self {
		Self {
			generation: 0,
			epoch: 0,
			next_request_id: 0,
			outstanding: 0,
			rollover_reserve: 0,
			headerless_consumption: 0,
			window: QuotaWindow::Unknown { probe_in_flight: false },
		}
	}
}

impl QuotaGovernor {
	fn reconcile_same_window_reset(known_reset: &mut Instant, observed_reset: Option<Instant>) {
		let Some(observed_reset) = observed_reset else {
			return;
		};
		// A response from Reddit's next quota window can arrive just before our
		// latency-inflated estimate of the current boundary. Do not let that
		// response move a nearly exhausted allowance an entire window forward.
		// Small drift is still accepted, while larger jumps are rediscovered once
		// the existing boundary has passed.
		if observed_reset <= *known_reset + RATE_LIMIT_COOLDOWN_MARGIN {
			*known_reset = (*known_reset).max(observed_reset);
		}
	}

	fn install_generation(&mut self, generation: u64, fresh_identity: bool) {
		self.generation = generation;
		if fresh_identity {
			// A newly generated anonymous device has its own quota window. Advance
			// the epoch so late responses from the previous identity cannot alter it.
			self.epoch = self.epoch.wrapping_add(1);
			self.outstanding = 0;
			self.rollover_reserve = 0;
			self.headerless_consumption = 0;
			self.window = QuotaWindow::Unknown { probe_in_flight: false };
		}
	}

	fn reserve(&mut self, now: Instant, generation: u64) -> Result<(u64, u64, bool), QuotaReserveError> {
		if generation != self.generation {
			return Err(QuotaReserveError::StaleGeneration);
		}
		if matches!(self.window, QuotaWindow::Known { reset_at, .. } if now >= reset_at + RATE_LIMIT_COOLDOWN_MARGIN) {
			self.epoch = self.epoch.wrapping_add(1);
			self.rollover_reserve = self.rollover_reserve.saturating_add(self.outstanding);
			self.outstanding = 0;
			self.headerless_consumption = 0;
			self.window = QuotaWindow::Unknown { probe_in_flight: false };
		}

		let discovery_probe = match &mut self.window {
			QuotaWindow::Unknown { probe_in_flight } => {
				// Every caller reaches quota admission only after acquiring a
				// transport permit. Let those already-bounded requests proceed
				// while the first response discovers the new identity's quota,
				// instead of returning a recoverable error to the user.
				let discovery_probe = !*probe_in_flight;
				*probe_in_flight = true;
				discovery_probe
			}
			QuotaWindow::Unreported => false,
			QuotaWindow::Known { available, reset_at } => {
				let reset_remaining = reset_at.saturating_duration_since(now);
				let safety_reserve = if reset_remaining <= EMERGENCY_QUOTA_ROTATION_MIN_RESET_REMAINING {
					0
				} else {
					QUOTA_SAFETY_RESERVE
				};
				if *available <= safety_reserve {
					let delay = reset_at
						.checked_duration_since(now)
						.unwrap_or_default()
						.saturating_add(RATE_LIMIT_COOLDOWN_MARGIN)
						.min(MAX_RATE_LIMIT_COOLDOWN);
					return Err(QuotaReserveError::ReserveExhausted(delay.max(Duration::from_secs(1))));
				}
				*available = available.saturating_sub(1);
				false
			}
		};

		self.outstanding = self.outstanding.saturating_add(1);
		self.next_request_id = self.next_request_id.wrapping_add(1);
		Ok((self.epoch, self.next_request_id, discovery_probe))
	}

	fn reserve_exhausted(&self, now: Instant) -> bool {
		let QuotaWindow::Known { available, reset_at } = self.window else {
			return false;
		};
		if now >= reset_at + RATE_LIMIT_COOLDOWN_MARGIN {
			return false;
		}
		let safety_reserve = if reset_at.saturating_duration_since(now) <= EMERGENCY_QUOTA_ROTATION_MIN_RESET_REMAINING {
			0
		} else {
			QUOTA_SAFETY_RESERVE
		};
		available <= safety_reserve
	}

	fn reconcile(&mut self, now: Instant, attempt: &UpstreamAttempt, remaining: Option<u16>, reset: Option<Duration>, quota_exhausted: bool) -> bool {
		if attempt.quota_epoch != self.epoch {
			return false;
		}
		self.outstanding = self.outstanding.saturating_sub(1);
		if attempt.generation != self.generation {
			if matches!(self.window, QuotaWindow::Unknown { .. } | QuotaWindow::Unreported) {
				// A stable token refresh changes generations without changing the
				// Reddit identity. Preserve conservative debt for a response from
				// the previous token until current-generation quota headers arrive.
				self.headerless_consumption = self.headerless_consumption.saturating_add(1);
				if attempt.discovery_probe {
					self.window = QuotaWindow::Unknown { probe_in_flight: false };
				}
			}
			return false;
		}

		let reset_at = reset.map(|delay| now + delay.min(MAX_RATE_LIMIT_COOLDOWN));
		if quota_exhausted {
			// Missing headers are not evidence that an observed window ends sooner.
			let known_reset = match self.window {
				QuotaWindow::Known { reset_at, .. } if reset_at > now => Some(reset_at),
				_ => None,
			};
			self.rollover_reserve = 0;
			self.headerless_consumption = 0;
			self.window = QuotaWindow::Known {
				available: 0,
				reset_at: reset_at.or(known_reset).unwrap_or(now + DEFAULT_RATE_LIMIT_COOLDOWN),
			};
			return true;
		}

		if let (QuotaWindow::Known { reset_at: known_reset, .. }, Some(remaining), Some(observed_reset)) = (&self.window, remaining, reset_at) {
			if now >= *known_reset && observed_reset > *known_reset + RATE_LIMIT_COOLDOWN_MARGIN {
				let unresolved = self.outstanding.saturating_add(self.rollover_reserve);
				self.epoch = self.epoch.wrapping_add(1);
				self.outstanding = 0;
				self.rollover_reserve = 0;
				self.headerless_consumption = 0;
				self.window = QuotaWindow::Known {
					available: remaining.saturating_sub(unresolved),
					reset_at: observed_reset,
				};
				return true;
			}
		}

		let overlapping_headerless = self.headerless_consumption.saturating_sub(attempt.quota_consumption_watermark).min(u64::from(u16::MAX)) as u16;
		match (&mut self.window, remaining) {
			(QuotaWindow::Unknown { .. }, Some(remaining)) => {
				self.window = QuotaWindow::Known {
					available: remaining
						.saturating_sub(self.outstanding)
						.saturating_sub(self.rollover_reserve)
						.saturating_sub(overlapping_headerless),
					reset_at: reset_at.unwrap_or(now + MAX_RATE_LIMIT_COOLDOWN),
				};
				self.rollover_reserve = 0;
				self.headerless_consumption = 0;
			}
			(QuotaWindow::Unknown { .. }, None) => {
				// Quota discovery is not complete until the response body is known
				// to be a successful headerless JSON response. Keep the probe owned
				// through body parsing so other requests remain bounded followers.
				// Also retain conservative debt for this completed request: a later
				// quota snapshot may have been produced before this response.
				self.headerless_consumption = self.headerless_consumption.saturating_add(1);
			}
			(QuotaWindow::Unreported, Some(remaining)) => {
				self.window = QuotaWindow::Known {
					available: remaining
						.saturating_sub(self.outstanding)
						.saturating_sub(self.rollover_reserve)
						.saturating_sub(overlapping_headerless),
					reset_at: reset_at.unwrap_or(now + MAX_RATE_LIMIT_COOLDOWN),
				};
				self.rollover_reserve = 0;
				self.headerless_consumption = 0;
			}
			(QuotaWindow::Unreported, None) => {
				// Continue carrying headerless consumption debt until Reddit sends
				// the first authoritative quota snapshot for this identity.
				self.headerless_consumption = self.headerless_consumption.saturating_add(1);
			}
			(QuotaWindow::Known { available, reset_at: known_reset }, Some(remaining)) => {
				// The local allowance already excludes every admitted request.
				// Therefore an out-of-order response may lower, but never raise it.
				*available = (*available).min(remaining.saturating_sub(self.outstanding));
				Self::reconcile_same_window_reset(known_reset, reset_at);
			}
			(QuotaWindow::Known { reset_at: known_reset, .. }, None) => {
				Self::reconcile_same_window_reset(known_reset, reset_at);
			}
		}
		true
	}

	fn confirm_headerless_success(&mut self, attempt: &UpstreamAttempt) {
		if attempt.generation == self.generation && attempt.quota_epoch == self.epoch && matches!(self.window, QuotaWindow::Unknown { .. }) {
			self.window = QuotaWindow::Unreported;
		}
	}

	fn continue_headerless_discovery_after_redirect(&mut self, _now: Instant, attempt: &UpstreamAttempt) {
		if attempt.discovery_probe && attempt.generation == self.generation && attempt.quota_epoch == self.epoch && matches!(self.window, QuotaWindow::Unknown { .. }) {
			// The redirect response is accounted for, but it is not evidence that
			// the final JSON endpoint omits quota headers. Let its next hop inherit
			// probe ownership immediately.
			self.window = QuotaWindow::Unknown { probe_in_flight: false };
		}
	}

	fn abandon(&mut self, _now: Instant, attempt: &UpstreamAttempt) {
		if attempt.quota_epoch != self.epoch {
			return;
		}
		if attempt.quota_reconciled {
			if attempt.discovery_probe && matches!(self.window, QuotaWindow::Unknown { .. }) {
				self.window = QuotaWindow::Unknown { probe_in_flight: false };
			}
			return;
		}
		self.outstanding = self.outstanding.saturating_sub(1);
		if attempt.sent && matches!(self.window, QuotaWindow::Unknown { .. } | QuotaWindow::Unreported) {
			// The request may finish upstream after a later reservation captures
			// its watermark. Keep this uncertain consumption outside the completed
			// headerless counter so the first quota snapshot always reserves it.
			self.rollover_reserve = self.rollover_reserve.saturating_add(1);
		}
		match &mut self.window {
			QuotaWindow::Known { available, .. } if !attempt.sent => {
				*available = available.saturating_add(1);
			}
			QuotaWindow::Unknown { .. } if attempt.discovery_probe => {
				self.window = QuotaWindow::Unknown { probe_in_flight: false };
			}
			QuotaWindow::Unknown { .. } => {}
			QuotaWindow::Unreported => {}
			QuotaWindow::Known { .. } => {}
		}
	}
}

#[derive(Debug, Default)]
struct EdgeCircuit {
	failures: u8,
	epoch: u64,
	state: EdgeCircuitState,
	episode_started_at: Option<Instant>,
}

#[derive(Debug)]
struct UpstreamGuard {
	lane: RedditLane,
	quota: QuotaGovernor,
	graphql_quota: QuotaGovernor,
	graphql_rate_limit_blocked_until: Option<Instant>,
	quota_rotation_armed: bool,
	quota_wait_logged_epoch: Option<u64>,
	emergency_rotation_claimed_epoch: Option<u64>,
	failure_window_started: Option<Instant>,
	failures_in_window: u8,
	upstream_failure_blocked_until: Option<Instant>,
	rate_limit_blocked_until: Option<Instant>,
	edges: [EdgeCircuit; 7],
	identity_installed_at: Instant,
	rejected_oauth_generation: Option<u64>,
	rejected_session_refresh_after: Instant,
}

impl Default for UpstreamGuard {
	fn default() -> Self {
		Self::new(RedditLane::Direct)
	}
}

impl UpstreamGuard {
	fn new(lane: RedditLane) -> Self {
		Self {
			lane,
			quota: QuotaGovernor::default(),
			graphql_quota: QuotaGovernor::default(),
			graphql_rate_limit_blocked_until: None,
			quota_rotation_armed: false,
			quota_wait_logged_epoch: None,
			emergency_rotation_claimed_epoch: None,
			failure_window_started: None,
			failures_in_window: 0,
			upstream_failure_blocked_until: None,
			rate_limit_blocked_until: None,
			edges: std::array::from_fn(|_| EdgeCircuit::default()),
			identity_installed_at: Instant::now(),
			rejected_oauth_generation: None,
			rejected_session_refresh_after: Instant::now(),
		}
	}

	fn quota_budget(&self, budget: QuotaBudget) -> &QuotaGovernor {
		match budget {
			QuotaBudget::Rest => &self.quota,
			QuotaBudget::Graphql => &self.graphql_quota,
		}
	}
	fn quota_budget_mut(&mut self, budget: QuotaBudget) -> &mut QuotaGovernor {
		match budget {
			QuotaBudget::Rest => &mut self.quota,
			QuotaBudget::Graphql => &mut self.graphql_quota,
		}
	}
	fn rate_limit_deadline(&self, budget: QuotaBudget) -> Option<Instant> {
		match budget {
			QuotaBudget::Rest => self.rate_limit_blocked_until,
			QuotaBudget::Graphql => self.graphql_rate_limit_blocked_until,
		}
	}

	fn routing_availability(&self, now: Instant, generation: u64, endpoint: usize) -> LaneAvailability {
		if generation != self.quota.generation {
			return LaneAvailability::Unavailable;
		}
		if self.rate_limit_deadline(QuotaBudget::for_endpoint(endpoint)).is_some_and(|until| until > now) {
			return LaneAvailability::UpstreamQuotaCooldown;
		}
		if self.upstream_failure_blocked_until.is_some_and(|until| until > now) {
			return LaneAvailability::TransportCooldown;
		}
		if self.rejected_oauth_generation == Some(generation) || self.active_endpoint_cooldown(now, endpoint).is_some() {
			return LaneAvailability::Unavailable;
		}
		if self.quota_budget(QuotaBudget::for_endpoint(endpoint)).reserve_exhausted(now) {
			return LaneAvailability::LocalQuotaPause;
		}
		LaneAvailability::Ready
	}

	fn reject_oauth_generation(&mut self, generation: u64) -> bool {
		if self.quota.generation != generation || self.rejected_oauth_generation == Some(generation) {
			return false;
		}
		self.rejected_oauth_generation = Some(generation);
		true
	}

	fn rejected_session_refresh_delay(&mut self, now: Instant) -> Duration {
		let delay = self.rejected_session_refresh_after.saturating_duration_since(now);
		if delay.is_zero() {
			self.rejected_session_refresh_after = now + REJECTED_SESSION_REFRESH_INTERVAL;
		}
		delay
	}

	fn known_quota_state(&self, now: Instant, generation: u64) -> Option<(u16, Duration)> {
		if generation != self.quota.generation {
			return None;
		}
		let QuotaWindow::Known { available, reset_at } = self.quota.window else {
			return None;
		};
		Some((available, reset_at.saturating_duration_since(now)))
	}

	fn install_oauth_generation(&mut self, generation: u64, fresh_identity: bool) {
		self.quota.install_generation(generation, fresh_identity);
		self.graphql_quota.install_generation(generation, fresh_identity);
		self.rejected_oauth_generation = None;
		if fresh_identity {
			self.quota_rotation_armed = false;
			self.quota_wait_logged_epoch = None;
			self.emergency_rotation_claimed_epoch = None;
			self.rate_limit_blocked_until = None;
			self.graphql_rate_limit_blocked_until = None;
			self.identity_installed_at = Instant::now();
		}
	}

	fn quota_rotation_allowed(&self, now: Instant, generation: u64) -> bool {
		if !self.quota_rotation_armed || generation != self.quota.generation {
			return false;
		}
		if self.rate_limit_blocked_until.is_some_and(|deadline| deadline > now)
			|| self.upstream_failure_blocked_until.is_some_and(|deadline| deadline > now)
			|| self.edges.iter().any(|edge| !matches!(edge.state, EdgeCircuitState::Closed))
		{
			return false;
		}
		true
	}

	fn low_rate_limit_threshold(&self) -> u16 {
		match self.lane {
			RedditLane::Direct => LOW_RATE_LIMIT_THRESHOLD,
			RedditLane::Tor => TOR_LOW_RATE_LIMIT_THRESHOLD,
		}
	}

	fn quota_rotation_min_reset_remaining(&self, mode: QuotaRotationMode) -> Duration {
		match (self.lane, mode) {
			(RedditLane::Tor, _) => TOR_QUOTA_ROTATION_MIN_RESET_REMAINING,
			(RedditLane::Direct, QuotaRotationMode::Proactive) => QUOTA_ROTATION_MIN_RESET_REMAINING,
			(RedditLane::Direct, QuotaRotationMode::Emergency) => EMERGENCY_QUOTA_ROTATION_MIN_RESET_REMAINING,
		}
	}

	fn quota_rotation_window_matches(&self, now: Instant, mode: QuotaRotationMode) -> bool {
		let QuotaWindow::Known { available, reset_at } = self.quota.window else {
			return false;
		};
		let Some(remaining) = reset_at.checked_duration_since(now) else {
			return false;
		};
		if remaining <= self.quota_rotation_min_reset_remaining(mode) {
			return false;
		}
		match mode {
			QuotaRotationMode::Proactive => available < self.low_rate_limit_threshold(),
			QuotaRotationMode::Emergency => available <= QUOTA_SAFETY_RESERVE,
		}
	}

	fn quota_rotation_candidate(&self, now: Instant, generation: u64, mode: QuotaRotationMode) -> Option<QuotaRotationTicket> {
		if !self.quota_rotation_allowed(now, generation) || !self.quota_rotation_window_matches(now, mode) {
			return None;
		}
		if mode == QuotaRotationMode::Emergency && self.emergency_rotation_claimed_epoch == Some(self.quota.epoch) {
			return None;
		}
		Some(QuotaRotationTicket {
			lane: self.lane,
			generation,
			quota_epoch: self.quota.epoch,
			mode,
		})
	}

	fn claim_quota_rotation(&mut self, now: Instant, ticket: QuotaRotationTicket) -> bool {
		if self.quota_rotation_candidate(now, ticket.generation, ticket.mode) != Some(ticket) {
			return false;
		}
		if ticket.mode == QuotaRotationMode::Emergency {
			self.emergency_rotation_claimed_epoch = Some(ticket.quota_epoch);
		}
		true
	}

	fn quota_rotation_still_needed(&self, now: Instant, ticket: QuotaRotationTicket) -> bool {
		if ticket.quota_epoch != self.quota.epoch || !self.quota_rotation_allowed(now, ticket.generation) || !self.quota_rotation_window_matches(now, ticket.mode) {
			return false;
		}
		ticket.mode != QuotaRotationMode::Emergency || self.emergency_rotation_claimed_epoch == Some(ticket.quota_epoch)
	}

	fn completed_quota_rotation_still_valid(&self, now: Instant, ticket: QuotaRotationTicket) -> bool {
		if ticket.quota_epoch != self.quota.epoch || !self.quota_rotation_allowed(now, ticket.generation) {
			return false;
		}
		if ticket.mode == QuotaRotationMode::Emergency && self.emergency_rotation_claimed_epoch != Some(ticket.quota_epoch) {
			return false;
		}
		let QuotaWindow::Known { available, reset_at } = self.quota.window else {
			return false;
		};
		if now >= reset_at {
			return false;
		}
		match ticket.mode {
			QuotaRotationMode::Proactive => available < self.low_rate_limit_threshold(),
			QuotaRotationMode::Emergency => available <= QUOTA_SAFETY_RESERVE,
		}
	}

	fn quota_spillover_still_needed(&self, now: Instant, ticket: QuotaSpilloverTicket) -> bool {
		self.lane == RedditLane::Direct
			&& ticket.generation == self.quota_budget(ticket.budget).generation
			&& ticket.quota_epoch == self.quota_budget(ticket.budget).epoch
			&& self.active_endpoint_cooldown(now, ticket.budget.endpoint()).is_none()
			&& self.edges.iter().all(|edge| matches!(edge.state, EdgeCircuitState::Closed))
			&& self.quota_budget(ticket.budget).reserve_exhausted(now)
	}

	fn take_short_reset_notice(&mut self, now: Instant, generation: u64) -> Option<(u16, Duration)> {
		if !self.quota_rotation_armed || generation != self.quota.generation || self.quota_wait_logged_epoch == Some(self.quota.epoch) {
			return None;
		}
		let QuotaWindow::Known { available, reset_at } = self.quota.window else {
			return None;
		};
		let reset_remaining = reset_at.saturating_duration_since(now);
		if available >= self.low_rate_limit_threshold() || reset_remaining > self.quota_rotation_min_reset_remaining(QuotaRotationMode::Proactive) {
			return None;
		}
		self.quota_wait_logged_epoch = Some(self.quota.epoch);
		Some((available, reset_remaining))
	}

	#[cfg(test)]
	fn try_admit(&mut self, now: Instant, generation: u64) -> Result<UpstreamAttempt, AdmissionDenied> {
		self.try_admit_endpoint(now, generation, 0)
	}

	fn try_admit_endpoint(&mut self, now: Instant, generation: u64, endpoint: usize) -> Result<UpstreamAttempt, AdmissionDenied> {
		if let Some((delay, reason)) = self.active_endpoint_cooldown(now, endpoint) {
			return Err(AdmissionDenied {
				delay,
				reason,
				reserve_exhausted: false,
				local_quota_retry: reason == CooldownReason::RateLimit,
				source: "active_cooldown",
			});
		}

		let budget = QuotaBudget::for_endpoint(endpoint);
		let (quota_epoch, request_id, discovery_probe) = self.quota_budget_mut(budget).reserve(now, generation).map_err(|error| match error {
			QuotaReserveError::ReserveExhausted(delay) => AdmissionDenied {
				delay,
				reason: CooldownReason::RateLimit,
				reserve_exhausted: true,
				local_quota_retry: true,
				source: "quota_reserve",
			},
			QuotaReserveError::StaleGeneration => AdmissionDenied {
				delay: Duration::from_secs(1),
				reason: CooldownReason::RateLimit,
				reserve_exhausted: false,
				local_quota_retry: true,
				source: "stale_generation",
			},
		})?;
		let quota_consumption_watermark = self.quota_budget(budget).headerless_consumption;
		let edge = match self.begin_endpoint_attempt(now, endpoint) {
			Ok(edge) => edge,
			Err(error) => {
				let mut placeholder = UpstreamAttempt {
					lane: self.lane,
					edge: EdgeAttempt {
						endpoint,
						epoch: self.edges[endpoint].epoch,
						half_open: false,
					},
					generation,
					quota_epoch,
					request_id,
					quota_consumption_watermark,
					discovery_probe,
					sent: false,
					quota_reconciled: false,
					completed: true,
				};
				self.quota_budget_mut(budget).abandon(now, &placeholder);
				placeholder.quota_reconciled = true;
				return Err(AdmissionDenied {
					delay: error.0,
					reason: error.1,
					reserve_exhausted: false,
					local_quota_retry: false,
					source: "edge_circuit",
				});
			}
		};

		Ok(UpstreamAttempt {
			lane: self.lane,
			edge,
			generation,
			quota_epoch,
			request_id,
			quota_consumption_watermark,
			discovery_probe,
			sent: false,
			quota_reconciled: false,
			completed: false,
		})
	}

	fn reconcile_quota(&mut self, now: Instant, attempt: &mut UpstreamAttempt, remaining: Option<u16>, reset: Option<Duration>, quota_exhausted: bool) {
		if attempt.quota_reconciled {
			return;
		}
		let budget = QuotaBudget::for_endpoint(attempt.edge.endpoint);
		let applied = self.quota_budget_mut(budget).reconcile(now, attempt, remaining, reset, quota_exhausted);
		if budget == QuotaBudget::Rest && applied && !quota_exhausted && remaining.is_some_and(|remaining| remaining >= self.low_rate_limit_threshold()) {
			self.quota_rotation_armed = true;
		}
		attempt.quota_reconciled = true;
	}

	fn abandon_attempt(&mut self, now: Instant, attempt: &UpstreamAttempt) {
		self.quota_budget_mut(QuotaBudget::for_endpoint(attempt.edge.endpoint)).abandon(now, attempt);
		if attempt.edge.half_open && !attempt.completed {
			self.abandon_edge_probe(now, attempt.edge);
		}
	}

	fn active_cooldown(&self, now: Instant) -> Option<(Duration, CooldownReason)> {
		self.active_endpoint_cooldown(now, 0)
	}

	fn active_endpoint_cooldown(&self, now: Instant, endpoint: usize) -> Option<(Duration, CooldownReason)> {
		let mut active = None;
		let mut consider = |deadline: Option<Instant>, reason| {
			if let Some(remaining) = deadline.filter(|deadline| *deadline > now).map(|deadline| deadline.duration_since(now)) {
				if active.map_or(true, |(current, _)| remaining > current) {
					active = Some((remaining, reason));
				}
			}
		};

		consider(self.rate_limit_deadline(QuotaBudget::for_endpoint(endpoint)), CooldownReason::RateLimit);
		consider(self.upstream_failure_blocked_until, CooldownReason::UpstreamFailures);
		match self.edges[endpoint].state {
			EdgeCircuitState::Open { until } => consider(Some(until), CooldownReason::EdgeThrottle),
			EdgeCircuitState::HalfOpen { expires_at, .. } => consider(Some(expires_at), CooldownReason::EdgeThrottle),
			EdgeCircuitState::Closed => {}
		}
		active
	}

	fn redirect_cooldown(&self, now: Instant, attempt: EdgeAttempt) -> Option<(Duration, CooldownReason)> {
		let mut active = None;
		let mut consider = |deadline: Option<Instant>, reason| {
			if let Some(remaining) = deadline.filter(|deadline| *deadline > now).map(|deadline| deadline.duration_since(now)) {
				if active.map_or(true, |(current, _)| remaining > current) {
					active = Some((remaining, reason));
				}
			}
		};
		consider(self.rate_limit_deadline(QuotaBudget::for_endpoint(attempt.endpoint)), CooldownReason::RateLimit);
		consider(self.upstream_failure_blocked_until, CooldownReason::UpstreamFailures);
		match self.edges[attempt.endpoint].state {
			EdgeCircuitState::Closed if attempt.epoch == self.edges[attempt.endpoint].epoch => {}
			EdgeCircuitState::HalfOpen { epoch, .. } if attempt.half_open && attempt.epoch == epoch => {}
			EdgeCircuitState::Open { until } => consider(Some(until.max(now + Duration::from_secs(1))), CooldownReason::EdgeThrottle),
			EdgeCircuitState::HalfOpen { expires_at, .. } => consider(Some(expires_at.max(now + Duration::from_secs(1))), CooldownReason::EdgeThrottle),
			EdgeCircuitState::Closed => consider(Some(now + Duration::from_secs(1)), CooldownReason::EdgeThrottle),
		}
		active
	}

	fn extend_deadline(slot: &mut Option<Instant>, now: Instant, duration: Duration) {
		let deadline = now + duration;
		if deadline > slot.as_ref().copied().unwrap_or(now) {
			*slot = Some(deadline);
		}
	}

	#[cfg(test)]
	fn begin_attempt(&mut self, now: Instant) -> Result<EdgeAttempt, (Duration, CooldownReason)> {
		self.begin_endpoint_attempt(now, 0)
	}

	fn begin_endpoint_attempt(&mut self, now: Instant, endpoint: usize) -> Result<EdgeAttempt, (Duration, CooldownReason)> {
		if matches!(self.edges[endpoint].state, EdgeCircuitState::HalfOpen { expires_at, .. } if expires_at <= now) {
			self.edges[endpoint].epoch = self.edges[endpoint].epoch.wrapping_add(1);
			self.edges[endpoint].state = EdgeCircuitState::HalfOpen {
				epoch: self.edges[endpoint].epoch,
				expires_at: now + self.lane.request_timeout(),
			};
			return Ok(EdgeAttempt {
				endpoint,
				epoch: self.edges[endpoint].epoch,
				half_open: true,
			});
		}
		if let Some(active) = self.active_endpoint_cooldown(now, endpoint) {
			return Err(active);
		}

		match self.edges[endpoint].state {
			EdgeCircuitState::Open { .. } => {
				self.edges[endpoint].state = EdgeCircuitState::HalfOpen {
					epoch: self.edges[endpoint].epoch,
					expires_at: now + self.lane.request_timeout(),
				};
				Ok(EdgeAttempt {
					endpoint,
					epoch: self.edges[endpoint].epoch,
					half_open: true,
				})
			}
			EdgeCircuitState::HalfOpen { expires_at, .. } => Err((expires_at.checked_duration_since(now).unwrap_or(Duration::from_secs(1)), CooldownReason::EdgeThrottle)),
			EdgeCircuitState::Closed => Ok(EdgeAttempt {
				endpoint,
				epoch: self.edges[endpoint].epoch,
				half_open: false,
			}),
		}
	}

	fn record_failure(&mut self, now: Instant) -> bool {
		if self.failure_window_started.map_or(true, |started| now.duration_since(started) > FAILURE_WINDOW) {
			self.failure_window_started = Some(now);
			self.failures_in_window = 0;
		}

		self.failures_in_window = self.failures_in_window.saturating_add(1);
		if self.failures_in_window >= FAILURE_THRESHOLD {
			Self::extend_deadline(&mut self.upstream_failure_blocked_until, now, proportional_positive_jitter(FAILURE_COOLDOWN));
			self.failure_window_started = None;
			self.failures_in_window = 0;
			true
		} else {
			false
		}
	}

	fn reset_failure_window(&mut self) {
		self.failure_window_started = None;
		self.failures_in_window = 0;
	}

	fn record_api_success(&mut self, now: Instant, attempt: EdgeAttempt) -> Option<EdgeRecovery> {
		self.reset_failure_window();
		let closes_probe = matches!(self.edges[attempt.endpoint].state, EdgeCircuitState::HalfOpen { epoch, .. } if epoch == attempt.epoch);
		// A success cannot end a cooldown extended by another in-flight failure.
		if self.upstream_failure_blocked_until.is_some_and(|until| now >= until) {
			self.upstream_failure_blocked_until = None;
		}
		if self.rate_limit_blocked_until.is_some_and(|until| now >= until) {
			self.rate_limit_blocked_until = None;
		}
		if self.graphql_rate_limit_blocked_until.is_some_and(|until| now >= until) {
			self.graphql_rate_limit_blocked_until = None;
		}
		let current_closed_attempt = matches!(self.edges[attempt.endpoint].state, EdgeCircuitState::Closed) && attempt.epoch == self.edges[attempt.endpoint].epoch;
		let recovery = closes_probe.then(|| EdgeRecovery {
			consecutive_failures: self.edges[attempt.endpoint].failures,
			episode_seconds: self.edges[attempt.endpoint]
				.episode_started_at
				.map_or(0, |started| now.saturating_duration_since(started).as_secs()),
			current_generation: self.quota.generation,
			identity_age_seconds: now.saturating_duration_since(self.identity_installed_at).as_secs(),
		});
		if closes_probe {
			self.edges[attempt.endpoint].failures = 0;
			self.edges[attempt.endpoint].epoch = self.edges[attempt.endpoint].epoch.wrapping_add(1);
			self.edges[attempt.endpoint].state = EdgeCircuitState::Closed;
			self.edges[attempt.endpoint].episode_started_at = None;
		} else if current_closed_attempt {
			self.edges[attempt.endpoint].failures = 0;
			self.edges[attempt.endpoint].episode_started_at = None;
		}
		recovery
	}

	fn record_generation_success(&mut self, now: Instant, generation: u64, attempt: EdgeAttempt) -> Option<EdgeRecovery> {
		if self.quota.generation == generation {
			self.record_api_success(now, attempt)
		} else {
			self.abandon_edge_probe(now, attempt);
			None
		}
	}

	fn record_edge_throttle(&mut self, now: Instant, attempt: EdgeAttempt, retry_after: Option<Duration>) -> EdgeThrottleDecision {
		let episode_seconds = self.edges[attempt.endpoint]
			.episode_started_at
			.map_or(0, |started| now.saturating_duration_since(started).as_secs());
		let identity_age_seconds = now.saturating_duration_since(self.identity_installed_at).as_secs();
		if attempt.epoch != self.edges[attempt.endpoint].epoch {
			let delay = match self.edges[attempt.endpoint].state {
				EdgeCircuitState::Open { until } => until.checked_duration_since(now).unwrap_or_default(),
				EdgeCircuitState::HalfOpen { .. } => Duration::from_secs(1),
				EdgeCircuitState::Closed => edge_throttle_base_delay(self.edges[attempt.endpoint].failures.max(1), retry_after).0,
			};
			return EdgeThrottleDecision {
				delay,
				consecutive_failures: self.edges[attempt.endpoint].failures,
				started_cooldown: false,
				episode_seconds,
				current_generation: self.quota.generation,
				identity_age_seconds,
			};
		}
		let episode_started_at = *self.edges[attempt.endpoint].episode_started_at.get_or_insert(now);
		let episode_seconds = now.saturating_duration_since(episode_started_at).as_secs();

		self.edges[attempt.endpoint].failures = self.edges[attempt.endpoint].failures.saturating_add(1);
		let delay = edge_throttle_delay(self.edges[attempt.endpoint].failures, retry_after);
		self.edges[attempt.endpoint].epoch = self.edges[attempt.endpoint].epoch.wrapping_add(1);
		self.edges[attempt.endpoint].state = EdgeCircuitState::Open { until: now + delay };
		EdgeThrottleDecision {
			delay,
			consecutive_failures: self.edges[attempt.endpoint].failures,
			started_cooldown: true,
			episode_seconds,
			current_generation: self.quota.generation,
			identity_age_seconds,
		}
	}

	fn abandon_edge_probe(&mut self, now: Instant, attempt: EdgeAttempt) {
		if attempt.half_open && matches!(self.edges[attempt.endpoint].state, EdgeCircuitState::HalfOpen { epoch, .. } if epoch == attempt.epoch) {
			let delay = edge_throttle_delay(self.edges[attempt.endpoint].failures.max(1), None);
			self.edges[attempt.endpoint].state = EdgeCircuitState::Open { until: now + delay };
		}
	}

	fn block_for_rate_limit(&mut self, now: Instant, duration: Duration) {
		Self::extend_deadline(&mut self.rate_limit_blocked_until, now, duration);
	}
}

fn max_concurrent_api_requests() -> usize {
	parse_max_concurrency(env::var("REDLIB_REDDIT_MAX_CONCURRENCY").ok().as_deref())
}

fn parse_max_concurrency(value: Option<&str>) -> usize {
	value
		.and_then(|value| value.parse::<usize>().ok())
		.unwrap_or(DEFAULT_MAX_CONCURRENT_API_REQUESTS)
		.clamp(1, MAX_CONFIGURED_API_REQUESTS)
}

fn parse_delay_seconds(value: Option<&str>) -> Option<Duration> {
	let seconds = value?.parse::<f64>().ok()?;
	if !seconds.is_finite() || seconds < 0.0 {
		return None;
	}
	Some(Duration::from_secs_f64(seconds.min(MAX_RATE_LIMIT_COOLDOWN.as_secs_f64())))
}

fn parse_retry_after(value: Option<&str>, now: SystemTime) -> Option<Duration> {
	let value = value?;
	parse_delay_seconds(Some(value)).or_else(|| {
		httpdate::parse_http_date(value)
			.ok()
			.and_then(|deadline| deadline.duration_since(now).ok())
			.map(|delay| delay.min(MAX_RATE_LIMIT_COOLDOWN))
	})
}

fn parse_rate_limit_count(value: Option<&str>) -> Option<u16> {
	value?
		.parse::<f64>()
		.ok()
		.filter(|count| count.is_finite() && *count >= 0.0)
		.map(|count| count.floor().min(f64::from(u16::MAX)) as u16)
}

fn rate_limit_delay(retry_after: Option<&str>, reset: Option<&str>) -> Duration {
	let (base, server_is_floor) = rate_limit_base_delay(retry_after, reset);
	if server_is_floor {
		positive_jitter(base, Duration::from_secs(2))
	} else {
		proportional_positive_jitter(base)
	}
}

fn rate_limit_base_delay(retry_after: Option<&str>, reset: Option<&str>) -> (Duration, bool) {
	let server_delay = match (
		parse_retry_after(retry_after, SystemTime::now()).filter(|delay| !delay.is_zero()),
		parse_delay_seconds(reset).filter(|delay| !delay.is_zero()),
	) {
		(Some(retry), Some(reset)) => Some(retry.max(reset)),
		(Some(delay), None) | (None, Some(delay)) => Some(delay),
		(None, None) => None,
	};
	(
		server_delay
			.unwrap_or(DEFAULT_RATE_LIMIT_COOLDOWN)
			.saturating_add(RATE_LIMIT_COOLDOWN_MARGIN)
			.min(MAX_RATE_LIMIT_COOLDOWN),
		server_delay.is_some(),
	)
}

fn edge_throttle_delay(consecutive_failures: u8, retry_after: Option<Duration>) -> Duration {
	let (base, server_is_floor) = edge_throttle_base_delay(consecutive_failures, retry_after);
	if server_is_floor {
		positive_jitter(base, Duration::from_secs(2))
	} else {
		proportional_positive_jitter(base)
	}
}

fn edge_throttle_base_delay(consecutive_failures: u8, retry_after: Option<Duration>) -> (Duration, bool) {
	let exponent = u32::from(consecutive_failures.saturating_sub(1).min(7));
	let multiplier = 1_u64.checked_shl(exponent).unwrap_or(u64::MAX);
	let exponential = Duration::from_secs(EDGE_THROTTLE_INITIAL_COOLDOWN.as_secs().saturating_mul(multiplier)).min(EDGE_THROTTLE_MAX_COOLDOWN);
	let server_delay = retry_after.unwrap_or_default().saturating_add(RATE_LIMIT_COOLDOWN_MARGIN).min(MAX_RATE_LIMIT_COOLDOWN);
	(exponential.max(server_delay), retry_after.is_some() && server_delay >= exponential)
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
enum ThrottleKind {
	Quota,
	Edge,
}

#[derive(Debug)]
enum ApiRequestError {
	Deferred {
		message: String,
		edge_rejected: bool,
		recovery: LaneRecoveryReason,
	},
	TorQuotaRefreshWait {
		message: String,
		ticket: QuotaRefreshWaitTicket,
	},
	Terminal {
		message: String,
		session_unavailable: bool,
	},
	Upstream(String),
}

impl ApiRequestError {
	fn deferred(message: String, reason: CooldownReason, delay: Duration) -> Self {
		Self::Deferred {
			message,
			edge_rejected: reason == CooldownReason::EdgeThrottle,
			recovery: match reason {
				CooldownReason::UpstreamFailures => LaneRecoveryReason::UpstreamCooldown(delay),
				CooldownReason::RateLimit => LaneRecoveryReason::QuotaCooldown(delay),
				CooldownReason::EdgeThrottle => LaneRecoveryReason::None,
			},
		}
	}
}

const MAX_REJECTION_BODY_BYTES: usize = 16 * 1024;

#[derive(Debug, Eq, PartialEq)]
struct RejectionDetails {
	body_kind: &'static str,
	resource_reason: Option<&'static str>,
}

fn rejection_details(body: &[u8]) -> RejectionDetails {
	let Ok(json) = serde_json::from_slice::<Value>(body) else {
		return RejectionDetails {
			body_kind: "non_json",
			resource_reason: None,
		};
	};
	let resource_reason = if json["error"].as_u64() == Some(403) {
		match json["reason"].as_str() {
			Some("private") => Some("private"),
			Some("banned") => Some("banned"),
			Some("quarantined") => Some("quarantined"),
			Some("gated") => Some("gated"),
			_ => None,
		}
	} else {
		None
	};
	RejectionDetails {
		body_kind: "json",
		resource_reason,
	}
}

async fn read_rejection_details(body: &mut Body) -> RejectionDetails {
	use hyper::body::HttpBody;
	let mut bytes = Vec::new();
	while let Some(chunk) = body.data().await {
		let Ok(chunk) = chunk else {
			return RejectionDetails {
				body_kind: "body_error",
				resource_reason: None,
			};
		};
		if bytes.len().saturating_add(chunk.len()) > MAX_REJECTION_BODY_BYTES {
			return RejectionDetails {
				body_kind: "oversized",
				resource_reason: None,
			};
		}
		bytes.extend_from_slice(&chunk);
	}
	rejection_details(&bytes)
}

fn credential_rejected(status: u16, challenge: Option<&str>) -> bool {
	status == 401
		|| status == 403
			&& challenge.is_some_and(|challenge| {
				let challenge = challenge.to_ascii_lowercase();
				challenge.starts_with("bearer ") && challenge.contains("error=\"invalid_token\"")
			})
}

fn classify_throttle_response(status: u16, retry_after_present: bool, quota_headers_present: bool) -> Option<ThrottleKind> {
	match status {
		429 => Some(ThrottleKind::Quota),
		403 if retry_after_present && quota_headers_present => Some(ThrottleKind::Quota),
		403 if retry_after_present => Some(ThrottleKind::Edge),
		_ => None,
	}
}

pub(crate) fn oauth_client(lane: RedditLane) -> Option<Arc<Oauth>> {
	match lane {
		RedditLane::Direct => OAUTH_CLIENT.load_full(),
		RedditLane::Tor => TOR_OAUTH_CLIENT.load_full(),
	}
}

fn is_current_oauth_generation(lane: RedditLane, generation: u64) -> bool {
	oauth_client(lane).is_some_and(|client| client.generation == generation)
}

pub(crate) fn install_oauth_client(oauth: Oauth, fresh_identity: bool, expected_rotation: Option<QuotaRotationTicket>) -> bool {
	let lane = oauth.lane;
	let generation = oauth.generation;
	let mut guard = upstream_guard(lane);
	if let Some(ticket) = expected_rotation {
		if ticket.lane != lane || !is_current_oauth_generation(lane, ticket.generation) || !guard.completed_quota_rotation_still_valid(Instant::now(), ticket) {
			return false;
		}
	}
	match lane {
		RedditLane::Direct => {
			OAUTH_CLIENT.swap(Some(oauth.into()));
		}
		RedditLane::Tor => {
			TOR_OAUTH_CLIENT.swap(Some(oauth.into()));
		}
	}
	guard.install_oauth_generation(generation, fresh_identity);
	drop(guard);
	quota_notify(lane).notify_waiters();
	true
}

pub(crate) fn claim_quota_rotation(ticket: QuotaRotationTicket) -> bool {
	upstream_guard(ticket.lane).claim_quota_rotation(Instant::now(), ticket)
}

pub(crate) fn quota_rotation_still_needed(ticket: QuotaRotationTicket) -> bool {
	upstream_guard(ticket.lane).quota_rotation_still_needed(Instant::now(), ticket)
}

fn maybe_rotate_low_budget(lane: RedditLane, generation: u64, remaining: Option<u16>, used: Option<u16>, reset: Option<Duration>, path: &str) {
	if QuotaBudget::for_endpoint(endpoint_class_index(path)) == QuotaBudget::Graphql {
		return;
	}
	let now = Instant::now();
	let mut guard = upstream_guard(lane);
	let rotation_ticket = guard.quota_rotation_candidate(now, generation, QuotaRotationMode::Proactive);
	let effective_quota = guard.known_quota_state(now, generation);
	let short_reset = rotation_ticket.is_none().then(|| guard.take_short_reset_notice(now, generation)).flatten();
	drop(guard);

	if let Some((available, reset_remaining)) = short_reset {
		info!(
			"Reddit request budget is low but resets soon: lane={} remaining={} effective_available={} used={} reset_seconds={} endpoint={}; preserving the current anonymous OAuth identity",
			lane.label(),
			remaining.map_or(0, u16::from),
			available,
			used.map_or(0, u16::from),
			reset_remaining.as_secs(),
			endpoint_class(path),
		);
	}

	let Some(rotation_ticket) = rotation_ticket else {
		return;
	};
	if !is_current_oauth_generation(lane, generation) || !spawn_rate_limit_refresh(rotation_ticket) {
		return;
	}

	warn!(
		"Reddit request budget is low: lane={} remaining={} effective_available={} used={} reset_seconds={} effective_reset_seconds={} endpoint={}; rotating to a fresh anonymous OAuth identity",
		lane.label(),
		remaining.map_or(0, u16::from),
		effective_quota.map_or(0, |(available, _)| available),
		used.map_or(0, u16::from),
		reset.map_or(0, |duration| duration.as_secs()),
		effective_quota.map_or(0, |(_, duration)| duration.as_secs()),
		endpoint_class(path),
	);
}

fn endpoint_class_index(path: &str) -> usize {
	let path = path.split('?').next().unwrap_or_default();
	if path == "/subreddits/search.json" {
		return 6;
	}
	if path.contains("/comments/") || path.starts_with("/comments/") {
		return 4;
	}
	if path == "/search.json" || path.ends_with("/search.json") {
		return 3;
	}
	match path.split('/').nth(1) {
		Some("r") => 0,
		Some("user") => 1,
		Some("api") => 2,
		_ => 5,
	}
}

fn endpoint_class(path: &str) -> &'static str {
	["subreddit", "user", "api", "search", "comments", "other", "community_search"][endpoint_class_index(path)]
}

fn record_logical_json(path: &str) {
	let endpoint = endpoint_class_index(path);
	LOGICAL_JSON_COUNTS[endpoint].fetch_add(1, Ordering::Relaxed);
	if endpoint == endpoint_class_index("/comments/example.json") {
		let mut hasher = DefaultHasher::new();
		path.hash(&mut hasher);
		COMMENT_JSON_KEYS.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).insert(hasher.finish());
	}
	maybe_log_traffic_summary();
}

fn record_admitted_json(path: &str) {
	ADMITTED_JSON_COUNTS[endpoint_class_index(path)].fetch_add(1, Ordering::Relaxed);
	maybe_log_traffic_summary();
}

fn take_counter_summary<const N: usize>(labels: [&str; N], counters: &[AtomicU64; N]) -> String {
	labels
		.into_iter()
		.zip(counters.iter())
		.map(|(label, counter)| format!("{label}={}", counter.swap(0, Ordering::Relaxed)))
		.collect::<Vec<_>>()
		.join(",")
}

fn lane_index(lane: RedditLane) -> usize {
	match lane {
		RedditLane::Direct => 0,
		RedditLane::Tor => 1,
	}
}

fn record_api_send(path: &str, redirect: bool, lane: RedditLane) {
	API_SEND_COUNTS[endpoint_class_index(path)].fetch_add(1, Ordering::Relaxed);
	API_LANE_SENDS[lane_index(lane)].fetch_add(1, Ordering::Relaxed);
	if redirect {
		REDIRECT_HOPS.fetch_add(1, Ordering::Relaxed);
	}
}

fn record_local_denial(reason: CooldownReason) {
	let index = match reason {
		CooldownReason::RateLimit => 0,
		CooldownReason::EdgeThrottle => 1,
		CooldownReason::UpstreamFailures => 2,
	};
	LOCAL_DENIAL_COUNTS[index].fetch_add(1, Ordering::Relaxed);
	maybe_log_traffic_summary();
}

fn media_destination_index(format: &str) -> usize {
	if format.contains("v.redd.it") || format.contains("packaged-media.redd.it") {
		0
	} else if format.contains("i.redd.it") {
		1
	} else if format.contains("view.redd.it") {
		2
	} else if format.contains("redditmedia.com") || format.contains("redditstatic.com") || format.contains("reddit-econ-prod-assets") {
		3
	} else if format.contains("giphy.com") {
		4
	} else {
		5
	}
}

fn media_result_index(status: Option<u16>) -> usize {
	match status {
		Some(200..=299) => 0,
		Some(300..=399) => 1,
		Some(400..=499) => 2,
		Some(500..=599) => 3,
		Some(_) => 4,
		None => 5,
	}
}

fn record_media_send(destination: usize) {
	MEDIA_SENDS.fetch_add(1, Ordering::Relaxed);
	MEDIA_DESTINATION_SENDS[destination].fetch_add(1, Ordering::Relaxed);
}

fn record_media_result(status: Option<u16>) {
	MEDIA_RESULT_COUNTS[media_result_index(status)].fetch_add(1, Ordering::Relaxed);
	maybe_log_traffic_summary();
}

fn inbound_route_index(path: &str) -> usize {
	if path == "/" {
		return 0;
	}
	if path == "/health/live" {
		return 7;
	}
	if ["/img/", "/preview/", "/thumb/", "/vid/", "/hls/", "/emoji/", "/userpic/", "/giphy/"]
		.iter()
		.any(|prefix| path.starts_with(prefix))
	{
		return 6;
	}
	if path.ends_with(".rss") {
		return 5;
	}
	if path.contains("/comments/") {
		return 2;
	}
	if path == "/search" || path.ends_with("/search") {
		return 4;
	}
	if path.starts_with("/user/") || path.starts_with("/u/") {
		return 3;
	}
	if path.starts_with("/r/") {
		return 1;
	}
	8
}

pub(crate) fn record_inbound_request(method: &str, path: &str, status: u16) {
	INBOUND_ROUTE_COUNTS[inbound_route_index(path)].fetch_add(1, Ordering::Relaxed);
	let method_index = match method {
		"GET" => 0,
		"HEAD" => 1,
		_ => 2,
	};
	INBOUND_METHOD_COUNTS[method_index].fetch_add(1, Ordering::Relaxed);
	let status_index = match status {
		200..=299 => 0,
		300..=399 => 1,
		400..=499 => 2,
		500..=599 => 3,
		_ => 4,
	};
	INBOUND_STATUS_COUNTS[status_index].fetch_add(1, Ordering::Relaxed);
	maybe_log_traffic_summary();
}

pub(crate) fn record_oauth_send(lane: RedditLane) {
	OAUTH_LANE_SENDS[lane_index(lane)].fetch_add(1, Ordering::Relaxed);
	maybe_log_traffic_summary();
}

fn maybe_log_traffic_summary() {
	let now = Instant::now();
	let mut last = LAST_TRAFFIC_SUMMARY.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
	let elapsed = now.duration_since(*last);
	if elapsed < TRAFFIC_SUMMARY_INTERVAL {
		return;
	}
	*last = now;
	drop(last);

	info!(
		"Reddit traffic summary (elapsed_seconds={}): inbound_routes={} inbound_methods={} inbound_status={} logical_json={} comment_keys={} admitted_json={} api_sends={} api_lanes={} tor_fallbacks={} direct_standby_attempts={} tor_quota_spillover_attempts={} tor_quota_spillover_successes={} redirect_hops={} canonical_heads={} media_sends={} media_destinations={} media_results={} oauth_lanes={} local_denials={}",
		elapsed.as_secs().max(1),
		take_counter_summary(
			["home", "subreddit", "comments", "user", "search", "rss", "media", "health", "other"],
			&INBOUND_ROUTE_COUNTS,
		),
		take_counter_summary(["get", "head", "other"], &INBOUND_METHOD_COUNTS),
		take_counter_summary(["2xx", "3xx", "4xx", "5xx", "other"], &INBOUND_STATUS_COUNTS),
		take_counter_summary(
			["subreddit", "user", "api", "search", "comments", "other", "community_search"],
			&LOGICAL_JSON_COUNTS,
		),
		{
			let mut keys = COMMENT_JSON_KEYS.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
			let count = keys.len();
			keys.clear();
			count
		},
		take_counter_summary(
			["subreddit", "user", "api", "search", "comments", "other", "community_search"],
			&ADMITTED_JSON_COUNTS,
		),
		take_counter_summary(
			["subreddit", "user", "api", "search", "comments", "other", "community_search"],
			&API_SEND_COUNTS,
		),
		take_counter_summary(["direct", "tor"], &API_LANE_SENDS),
		TOR_FALLBACKS.swap(0, Ordering::Relaxed),
		DIRECT_STANDBY_ATTEMPTS.swap(0, Ordering::Relaxed),
		TOR_QUOTA_SPILLOVER_ATTEMPTS.swap(0, Ordering::Relaxed),
		TOR_QUOTA_SPILLOVER_SUCCESSES.swap(0, Ordering::Relaxed),
		REDIRECT_HOPS.swap(0, Ordering::Relaxed),
		CANONICAL_HEAD_SENDS.swap(0, Ordering::Relaxed),
		MEDIA_SENDS.swap(0, Ordering::Relaxed),
		take_counter_summary(["video", "image", "preview", "reddit_assets", "third_party", "other"], &MEDIA_DESTINATION_SENDS),
		take_counter_summary(["2xx", "3xx", "4xx", "5xx", "other", "transport"], &MEDIA_RESULT_COUNTS),
		take_counter_summary(["direct", "tor"], &OAUTH_LANE_SENDS),
		take_counter_summary(["quota", "edge", "failures"], &LOCAL_DENIAL_COUNTS),
	);
}

fn upstream_guard(lane: RedditLane) -> std::sync::MutexGuard<'static, UpstreamGuard> {
	match lane {
		RedditLane::Direct => DIRECT_UPSTREAM_GUARD.lock(),
		RedditLane::Tor => TOR_UPSTREAM_GUARD.lock(),
	}
	.unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn quota_notify(lane: RedditLane) -> &'static Notify {
	match lane {
		RedditLane::Direct => &DIRECT_QUOTA_NOTIFY,
		RedditLane::Tor => &TOR_QUOTA_NOTIFY,
	}
}

fn api_concurrency(lane: RedditLane) -> &'static Semaphore {
	match lane {
		RedditLane::Direct => &DIRECT_REDDIT_API_CONCURRENCY,
		RedditLane::Tor => &TOR_REDDIT_API_CONCURRENCY,
	}
}

fn retry_after_seconds(duration: Duration) -> u64 {
	duration.as_secs().saturating_add(u64::from(duration.subsec_nanos() > 0)).max(1)
}

fn oauth_startup_error() -> String {
	format!(
		"Redlib is starting its anonymous Reddit session. Retry in {} seconds",
		retry_after_seconds(OAUTH_STARTUP_RETRY)
	)
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
enum LaneAvailability {
	Ready,
	Unavailable,
	TransportCooldown,
	LocalQuotaPause,
	UpstreamQuotaCooldown,
}

fn configured_primary_lane() -> RedditLane {
	if *PRIMARY_REDDIT_LANE == RedditLane::Tor && matches!(TOR_FALLBACK_CONFIG.as_ref(), Ok(Some(_))) {
		RedditLane::Tor
	} else {
		RedditLane::Direct
	}
}

fn lane_availability(lane: RedditLane, now: Instant, path: &str) -> LaneAvailability {
	let Some(oauth) = oauth_client(lane) else { return LaneAvailability::Unavailable };
	let guard = upstream_guard(lane);
	guard.routing_availability(now, oauth.generation, endpoint_class_index(path))
}

fn tor_fallback_ready(path: &str) -> bool {
	lane_availability(RedditLane::Tor, Instant::now(), path) == LaneAvailability::Ready
}

fn edge_fallback_active(guard: &UpstreamGuard, now: Instant, endpoint: usize) -> bool {
	if guard.rate_limit_deadline(QuotaBudget::for_endpoint(endpoint)).is_some_and(|deadline| deadline > now)
		|| guard.upstream_failure_blocked_until.is_some_and(|deadline| deadline > now)
	{
		return false;
	}
	match guard.edges[endpoint].state {
		EdgeCircuitState::Open { until } => until > now,
		EdgeCircuitState::HalfOpen { expires_at, .. } => expires_at > now,
		EdgeCircuitState::Closed => false,
	}
}

fn direct_edge_fallback_active(now: Instant, path: &str) -> bool {
	edge_fallback_active(&upstream_guard(RedditLane::Direct), now, endpoint_class_index(path))
}

fn select_api_lane(primary: RedditLane, direct: LaneAvailability, tor: LaneAvailability) -> RedditLane {
	let (preferred, standby) = match primary {
		RedditLane::Direct => (direct, tor),
		RedditLane::Tor => (tor, direct),
	};
	// Explicit upstream quota cooldowns stay on their original lane. Direct's
	// local reserve keeps the existing bounded spillover policy.
	if preferred == LaneAvailability::Ready
		|| preferred == LaneAvailability::UpstreamQuotaCooldown
		|| primary == RedditLane::Direct && preferred == LaneAvailability::LocalQuotaPause
	{
		return primary;
	}
	if standby == LaneAvailability::Ready {
		match primary {
			RedditLane::Direct => RedditLane::Tor,
			RedditLane::Tor => RedditLane::Direct,
		}
	} else {
		primary
	}
}

fn preferred_api_lane(now: Instant, path: &str) -> RedditLane {
	select_api_lane(
		configured_primary_lane(),
		lane_availability(RedditLane::Direct, now, path),
		lane_availability(RedditLane::Tor, now, path),
	)
}

fn direct_quota_spillover_valid(ticket: QuotaSpilloverTicket, now: Instant) -> bool {
	!OAUTH_IS_ROLLING_OVER.load(Ordering::SeqCst) && upstream_guard(RedditLane::Direct).quota_spillover_still_needed(now, ticket)
}

#[derive(Debug)]
struct BeginAttemptDenied {
	admission: Option<AdmissionDenied>,
	message: String,
	edge_deferred: bool,
	quota_spillover: Option<QuotaSpilloverTicket>,
	tor_quota_refresh: Option<QuotaRefreshWaitTicket>,
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
struct QuotaRefreshWaitTicket {
	generation: u64,
	quota_epoch: u64,
}

// Shared by admission and the one permitted redirect replay. A transport retry
// must not silently start another ten-second refresh wait or refresh identity.
#[derive(Debug, Default)]
struct TorQuotaRefreshRetryState {
	owner: Option<QuotaRefreshWaitTicket>,
	started: Option<Instant>,
	timed_retries: u8,
	redirect_replayed: bool,
}

impl TorQuotaRefreshRetryState {
	fn claim_redirect_replay(&mut self, lane: RedditLane, ticket: QuotaRefreshWaitTicket, now: Instant) -> bool {
		if lane != RedditLane::Tor
			|| self.redirect_replayed
			|| self.owner.is_some_and(|owner| owner != ticket)
			|| self.started.is_some_and(|started| now.saturating_duration_since(started) >= TOR_QUOTA_REFRESH_RETRY_BUDGET)
			|| self.timed_retries >= MAX_TOR_QUOTA_REFRESH_RETRIES
		{
			return false;
		}
		self.owner.get_or_insert(ticket);
		self.started.get_or_insert(now);
		self.redirect_replayed = true;
		true
	}
}

fn quota_spillover_ticket(lane: RedditLane, denial: AdmissionDenied, short_refresh_retry: bool, generation: u64, quota_epoch: Option<u64>) -> Option<QuotaSpilloverTicket> {
	quota_epoch
		.filter(|_| lane == RedditLane::Direct && !short_refresh_retry && denial.reserve_exhausted && denial.source == "quota_reserve")
		.map(|quota_epoch| QuotaSpilloverTicket {
			budget: QuotaBudget::Rest,
			generation,
			quota_epoch,
		})
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
enum AdmissionRetryMode {
	Bounded,
	Immediate,
}

fn local_quota_retry_delay(denied: &BeginAttemptDenied, retry_count: u8, elapsed: Duration, mode: AdmissionRetryMode, tor_refresh_wait: bool) -> Option<Duration> {
	if mode == AdmissionRetryMode::Immediate {
		return None;
	}
	let admission = denied.admission.as_ref()?;
	let (budget, max_retries) = if tor_refresh_wait {
		(TOR_QUOTA_REFRESH_RETRY_BUDGET, MAX_TOR_QUOTA_REFRESH_RETRIES)
	} else {
		(LOCAL_QUOTA_RETRY_BUDGET, MAX_LOCAL_QUOTA_RETRIES)
	};
	if !admission.local_quota_retry || retry_count >= max_retries || elapsed >= budget {
		return None;
	}
	Some(admission.delay.min(LOCAL_QUOTA_RETRY_INTERVAL).min(budget.saturating_sub(elapsed)))
}

fn matching_tor_quota_refresh_wait(lane: RedditLane, owner: &mut Option<QuotaRefreshWaitTicket>, denied: &BeginAttemptDenied) -> bool {
	if lane != RedditLane::Tor || !denied.admission.is_some_and(|admission| admission.source == "quota_reserve" && admission.reserve_exhausted) {
		return false;
	}
	let Some(ticket) = denied.tor_quota_refresh else {
		return false;
	};
	// Never restart the budget or adopt another identity's refresh mid-wait.
	*owner.get_or_insert(ticket) == ticket
}

fn local_quota_retry_count_after_wait(retry_count: u8, timer_elapsed: bool) -> u8 {
	if timer_elapsed {
		retry_count.saturating_add(1)
	} else {
		retry_count
	}
}

async fn wait_for_local_quota_retry(quota_changed: impl std::future::Future<Output = ()>, delay: Duration) -> bool {
	tokio::select! {
		_ = quota_changed => false,
		_ = tokio::time::sleep(delay) => true,
	}
}

fn local_admission_message(denial: AdmissionDenied, short_refresh_retry: bool) -> String {
	if short_refresh_retry {
		return format!(
			"Refreshing the anonymous Reddit session. Retry in {} seconds",
			retry_after_seconds(EMERGENCY_QUOTA_REFRESH_RETRY)
		);
	}
	format!("{}. Retry in {} seconds", denial.reason.message(), retry_after_seconds(denial.delay))
}

fn begin_upstream_attempt_once(lane: RedditLane, allow_quota_refresh: bool, endpoint: usize) -> Result<(Arc<Oauth>, UpstreamAttempt), BeginAttemptDenied> {
	let mut guard = upstream_guard(lane);
	let budget = QuotaBudget::for_endpoint(endpoint);
	let allow_quota_refresh = allow_quota_refresh && budget == QuotaBudget::Rest;
	let oauth_client = oauth_client(lane).ok_or_else(|| BeginAttemptDenied {
		admission: None,
		message: oauth_startup_error(),
		edge_deferred: false,
		quota_spillover: None,
		tor_quota_refresh: None,
	})?;
	let generation = oauth_client.generation;
	if guard.rejected_oauth_generation == Some(generation) {
		return Err(BeginAttemptDenied {
			admission: None,
			message: session_refresh_wait_message(refresh_backoff_remaining(lane)),
			edge_deferred: false,
			quota_spillover: None,
			tor_quota_refresh: None,
		});
	}
	let now = Instant::now();
	let result = guard.try_admit_endpoint(now, generation, endpoint);
	let denied_quota_epoch = result.as_ref().err().filter(|denial| denial.reserve_exhausted).map(|_| guard.quota_budget(budget).epoch);
	// Capture ownership with the denied generation still locked. A refresh can
	// install its replacement between dropping this guard and formatting denial.
	let refresh_was_in_progress =
		budget == QuotaBudget::Rest && lane == RedditLane::Tor && denied_quota_epoch.is_some_and(|quota_epoch| quota_rotation_in_progress(lane, generation, quota_epoch));
	let emergency_ticket = allow_quota_refresh
		.then(|| {
			result
				.as_ref()
				.err()
				.filter(|denial| denial.reserve_exhausted)
				.and_then(|_| guard.quota_rotation_candidate(now, generation, QuotaRotationMode::Emergency))
		})
		.flatten();
	drop(guard);
	result.map(|attempt| (oauth_client, attempt)).map_err(|denial| {
		let emergency_started = emergency_ticket
			.filter(|_| is_current_oauth_generation(lane, generation))
			.is_some_and(spawn_rate_limit_refresh);
		let matching_refresh_in_progress = budget == QuotaBudget::Rest && denied_quota_epoch.is_some_and(|quota_epoch| quota_rotation_in_progress(lane, generation, quota_epoch));
		let short_refresh_retry = emergency_started || refresh_was_in_progress || matching_refresh_in_progress;
		if emergency_started {
			let reset_remaining = denial.delay.saturating_sub(RATE_LIMIT_COOLDOWN_MARGIN);
			warn!(
				"Local Reddit quota reserve reached with {} seconds left in the current window; rotating once to avoid a prolonged pause",
				reset_remaining.as_secs()
			);
		}
		let message = local_admission_message(denial, short_refresh_retry);
		let quota_spillover = quota_spillover_ticket(lane, denial, short_refresh_retry, generation, denied_quota_epoch).map(|mut ticket| {
			ticket.budget = budget;
			ticket
		});
		BeginAttemptDenied {
			edge_deferred: denial.reason == CooldownReason::EdgeThrottle,
			message,
			admission: Some(denial),
			quota_spillover,
			tor_quota_refresh: denied_quota_epoch
				.filter(|_| lane == RedditLane::Tor && short_refresh_retry)
				.map(|quota_epoch| QuotaRefreshWaitTicket { generation, quota_epoch }),
		}
	})
}

async fn begin_upstream_attempt(
	lane: RedditLane,
	endpoint: usize,
	mode: AdmissionRetryMode,
	tor_refresh: &mut TorQuotaRefreshRetryState,
) -> Result<(Arc<Oauth>, UpstreamAttempt, SemaphorePermit<'static>, Option<SemaphorePermit<'static>>), BeginAttemptDenied> {
	let started = Instant::now();
	let capacity_deadline = tokio::time::Instant::now() + CAPACITY_WAIT_BUDGET;
	let mut retry_count = 0;
	let mut retry_source = "none";
	loop {
		let quota_changed = quota_notify(lane).notified();
		let (permit, probe) = acquire_attempt_capacity(lane, endpoint, mode, capacity_deadline).await?;
		match begin_upstream_attempt_once(lane, mode == AdmissionRetryMode::Bounded && tor_refresh.owner.is_none(), endpoint) {
			Ok(attempt) => {
				if retry_count > 0 {
					info!(
						"Recovered transient local Reddit admission pause: lane={} source={} retries={} wait_milliseconds={}",
						lane.label(),
						retry_source,
						retry_count,
						Instant::now().saturating_duration_since(started).as_millis(),
					);
				}
				return Ok((attempt.0, attempt.1, permit, probe));
			}
			Err(denied) => {
				// Admission sleeps never occupy transport slots or the recovery probe.
				drop(permit);
				drop(probe);
				let now = Instant::now();
				let elapsed = now.saturating_duration_since(started);
				let tor_refresh_wait = matching_tor_quota_refresh_wait(lane, &mut tor_refresh.owner, &denied);
				let (budget_retries, budget_elapsed) = if tor_refresh_wait {
					if tor_refresh.started.is_none() {
						tor_refresh.started = Some(started);
						tor_refresh.timed_retries = retry_count;
					}
					let wait_started = tor_refresh.started.unwrap_or(started);
					(tor_refresh.timed_retries, now.saturating_duration_since(wait_started))
				} else {
					(retry_count, elapsed)
				};
				let Some(delay) = local_quota_retry_delay(&denied, budget_retries, budget_elapsed, mode, tor_refresh_wait) else {
					if let Some(admission) = denied.admission {
						record_local_denial(admission.reason);
						if retry_count > 0 {
							info!(
								"Local Reddit admission pause remained after bounded retries: lane={} source={} retries={} wait_milliseconds={}",
								lane.label(),
								admission.source,
								retry_count,
								elapsed.as_millis(),
							);
						}
					}
					return Err(denied);
				};
				retry_source = denied.admission.as_ref().map_or("unknown", |admission| admission.source);
				let timer_elapsed = wait_for_local_quota_retry(quota_changed, delay).await;
				retry_count = local_quota_retry_count_after_wait(retry_count, timer_elapsed);
				if tor_refresh_wait {
					tor_refresh.timed_retries = local_quota_retry_count_after_wait(tor_refresh.timed_retries, timer_elapsed);
				}
			}
		}
	}
}

async fn acquire_attempt_capacity(
	lane: RedditLane,
	endpoint: usize,
	mode: AdmissionRetryMode,
	deadline: tokio::time::Instant,
) -> Result<(SemaphorePermit<'static>, Option<SemaphorePermit<'static>>), BeginAttemptDenied> {
	let denied = || BeginAttemptDenied {
		message: "Reddit API request timed out while waiting for transport capacity".to_string(),
		edge_deferred: false,
		quota_spillover: None,
		tor_quota_refresh: None,
		admission: Some(AdmissionDenied {
			delay: TRANSPORT_RETRY_BASE_DELAY,
			reason: CooldownReason::UpstreamFailures,
			reserve_exhausted: false,
			local_quota_retry: false,
			source: "transport_capacity",
		}),
	};
	// Serialize recovery probes, including requests that arrived during cooldown.
	// RAII releases the probe on cancellation. Recheck after acquiring it because
	// a preceding probe may already have restored normal service.
	let recovery = || {
		let guard = upstream_guard(lane);
		guard.upstream_failure_blocked_until.is_some() || guard.rate_limit_deadline(QuotaBudget::for_endpoint(endpoint)).is_some()
	};
	let gate = match lane {
		RedditLane::Direct => &DIRECT_RECOVERY_PROBE,
		RedditLane::Tor => &TOR_RECOVERY_PROBE,
	};
	if mode == AdmissionRetryMode::Immediate {
		return acquire_immediate_recovery_capacity(api_concurrency(lane), gate, recovery).map_err(|_| denied());
	}
	acquire_recovery_capacity(api_concurrency(lane), gate, deadline, recovery).await.map_err(|_| denied())
}

fn acquire_immediate_recovery_capacity<'a>(
	capacity: &'a Semaphore,
	gate: &'a Semaphore,
	recovery: impl Fn() -> bool,
) -> Result<(SemaphorePermit<'a>, Option<SemaphorePermit<'a>>), ()> {
	let probe = if recovery() { Some(gate.try_acquire().map_err(|_| ())?) } else { None };
	let permit = capacity.try_acquire().map_err(|_| ())?;
	if probe.is_none() && recovery() {
		return Err(());
	}
	Ok((permit, probe))
}

async fn acquire_recovery_capacity<'a>(
	capacity: &'a Semaphore,
	gate: &'a Semaphore,
	deadline: tokio::time::Instant,
	recovery: impl Fn() -> bool,
) -> Result<(SemaphorePermit<'a>, Option<SemaphorePermit<'a>>), ()> {
	let mut probe = None;
	if recovery() {
		let permit = tokio::time::timeout_at(deadline, gate.acquire()).await.map_err(|_| ())?.map_err(|_| ())?;
		if recovery() {
			probe = Some(permit);
		}
	}
	let permit = tokio::time::timeout_at(deadline, capacity.acquire()).await.map_err(|_| ())?.map_err(|_| ())?;
	if probe.is_none() && recovery() {
		// Recovery may have started while we queued for normal capacity. Return
		// to admission without sending; the next attempt must join the probe gate.
		drop(permit);
		return Err(());
	}
	Ok((permit, probe))
}

fn block_for_rate_limit(lane: RedditLane, generation: u64, retry_after: Option<&str>, reset: Option<&str>, endpoint: usize) -> (Duration, bool) {
	let mut guard = upstream_guard(lane);
	if guard.quota.generation != generation {
		return (rate_limit_base_delay(retry_after, reset).0, false);
	}
	let duration = rate_limit_delay(retry_after, reset);
	match QuotaBudget::for_endpoint(endpoint) {
		QuotaBudget::Rest => guard.block_for_rate_limit(Instant::now(), duration),
		QuotaBudget::Graphql => UpstreamGuard::extend_deadline(&mut guard.graphql_rate_limit_blocked_until, Instant::now(), duration),
	}
	(duration, true)
}

fn reconcile_rate_limit(attempt: &mut UpstreamAttempt, remaining: Option<u16>, reset: Option<Duration>, quota_exhausted: bool) {
	let lane = attempt.lane;
	upstream_guard(lane).reconcile_quota(Instant::now(), attempt, remaining, reset, quota_exhausted);
	quota_notify(lane).notify_waiters();
}

fn confirm_headerless_quota(attempt: &UpstreamAttempt) {
	let lane = attempt.lane;
	upstream_guard(lane)
		.quota_budget_mut(QuotaBudget::for_endpoint(attempt.edge.endpoint))
		.confirm_headerless_success(attempt);
	quota_notify(lane).notify_waiters();
}

fn reserve_redirect_hop(
	attempt: &mut UpstreamAttempt,
	generation: u64,
	remaining: Option<u16>,
	reset: Option<Duration>,
	allow_quota_refresh: bool,
) -> Result<(), ApiRequestError> {
	let now = Instant::now();
	let mut guard = upstream_guard(attempt.lane);
	let budget = QuotaBudget::for_endpoint(attempt.edge.endpoint);
	let allow_quota_refresh = allow_quota_refresh && budget == QuotaBudget::Rest;
	let continue_headerless_discovery = attempt.discovery_probe && remaining.is_none() && reset.is_none();
	guard.reconcile_quota(now, attempt, remaining, reset, false);
	if continue_headerless_discovery {
		guard.quota_budget_mut(budget).continue_headerless_discovery_after_redirect(now, attempt);
	}
	if let Some((delay, reason)) = guard.redirect_cooldown(now, attempt.edge) {
		drop(guard);
		record_local_denial(reason);
		return Err(ApiRequestError::deferred(
			format!("{}. Retry in {} seconds", reason.message(), retry_after_seconds(delay)),
			reason,
			delay,
		));
	}
	let (quota_epoch, request_id, discovery_probe) = match guard.quota_budget_mut(budget).reserve(now, generation) {
		Ok(reservation) => reservation,
		Err(error) => {
			let (denial, denied_quota_epoch, emergency_ticket) = match error {
				QuotaReserveError::ReserveExhausted(delay) => (
					AdmissionDenied {
						delay,
						reason: CooldownReason::RateLimit,
						reserve_exhausted: true,
						local_quota_retry: true,
						source: "quota_reserve",
					},
					Some(guard.quota_budget(budget).epoch),
					allow_quota_refresh
						.then(|| guard.quota_rotation_candidate(now, generation, QuotaRotationMode::Emergency))
						.flatten(),
				),
				QuotaReserveError::StaleGeneration => (
					AdmissionDenied {
						delay: Duration::from_secs(1),
						reason: CooldownReason::RateLimit,
						reserve_exhausted: false,
						local_quota_retry: true,
						source: "stale_generation",
					},
					None,
					None,
				),
			};
			let refresh_was_in_progress = budget == QuotaBudget::Rest
				&& attempt.lane == RedditLane::Tor
				&& denied_quota_epoch.is_some_and(|quota_epoch| quota_rotation_in_progress(attempt.lane, generation, quota_epoch));
			drop(guard);
			let emergency_started = emergency_ticket
				.filter(|_| is_current_oauth_generation(attempt.lane, generation))
				.is_some_and(spawn_rate_limit_refresh);
			let matching_refresh_in_progress =
				budget == QuotaBudget::Rest && denied_quota_epoch.is_some_and(|quota_epoch| quota_rotation_in_progress(attempt.lane, generation, quota_epoch));
			let short_refresh_retry = emergency_started || refresh_was_in_progress || matching_refresh_in_progress;
			if emergency_started {
				let reset_remaining = denial.delay.saturating_sub(RATE_LIMIT_COOLDOWN_MARGIN);
				warn!(
					"Local Reddit quota reserve reached during redirect with {} seconds left in the current window; rotating once to avoid a prolonged pause",
					reset_remaining.as_secs()
				);
			}
			record_local_denial(CooldownReason::RateLimit);
			let message = local_admission_message(denial, short_refresh_retry);
			if let Some(quota_epoch) = denied_quota_epoch.filter(|_| attempt.lane == RedditLane::Tor && short_refresh_retry) {
				return Err(ApiRequestError::TorQuotaRefreshWait {
					message,
					ticket: QuotaRefreshWaitTicket { generation, quota_epoch },
				});
			}
			return Err(ApiRequestError::deferred(message, CooldownReason::RateLimit, denial.delay));
		}
	};
	let quota_consumption_watermark = guard.quota_budget(budget).headerless_consumption;
	attempt.generation = generation;
	attempt.quota_epoch = quota_epoch;
	attempt.request_id = request_id;
	attempt.quota_consumption_watermark = quota_consumption_watermark;
	attempt.discovery_probe = discovery_probe;
	attempt.sent = false;
	attempt.quota_reconciled = false;
	Ok(())
}

fn block_for_edge_throttle(attempt: &mut UpstreamAttempt, retry_after: Option<Duration>) -> EdgeThrottleDecision {
	let now = Instant::now();
	let mut guard = upstream_guard(attempt.lane);
	let decision = guard.record_edge_throttle(now, attempt.edge, retry_after);
	guard.quota_budget_mut(QuotaBudget::for_endpoint(attempt.edge.endpoint)).abandon(now, attempt);
	drop(guard);
	attempt.complete();
	decision
}

fn record_upstream_failure(lane: RedditLane, kind: &str, status: Option<u16>, path: &str, generation: u64) {
	let mut guard = upstream_guard(lane);
	if guard.quota.generation != generation {
		trace!("Ignoring stale Reddit upstream failure: kind={kind} endpoint={}", endpoint_class(path));
		return;
	}
	let opened = guard.record_failure(Instant::now());
	warn!(
		"Reddit upstream failure: lane={} kind={kind} status={} endpoint={} circuit_opened={opened}",
		lane.label(),
		status.map_or_else(|| "transport".to_string(), |status| status.to_string()),
		endpoint_class(path),
	);
}

fn record_upstream_success(attempt: &mut UpstreamAttempt, path: &str) {
	let recovery = upstream_guard(attempt.lane).record_generation_success(Instant::now(), attempt.generation, attempt.edge);
	if let Some(recovery) = recovery {
		info!(
			"Reddit edge circuit recovered: lane={} endpoint={} consecutive_failures={} episode_seconds={} request_generation={} current_generation={} current_identity_age_seconds={}",
			attempt.lane.label(),
			endpoint_class(path),
			recovery.consecutive_failures,
			recovery.episode_seconds,
			attempt.generation,
			recovery.current_generation,
			recovery.identity_age_seconds,
		);
	}
	attempt.complete();
}

const URL_PAIRS: [(&str, &str); 2] = [
	(ALTERNATIVE_REDDIT_URL_BASE, ALTERNATIVE_REDDIT_URL_BASE_HOST),
	(REDDIT_SHORT_URL_BASE, REDDIT_SHORT_URL_BASE_HOST),
];

#[derive(Debug)]
enum CanonicalPathError {
	RetryOnTor(String),
	Terminal(String),
}

impl CanonicalPathError {
	fn into_message(self) -> String {
		match self {
			Self::RetryOnTor(message) | Self::Terminal(message) => message,
		}
	}
}

fn canonical_head_origins(lane: RedditLane) -> [Option<(&'static str, &'static str)>; 2] {
	match lane {
		RedditLane::Direct => [Some(URL_PAIRS[0]), Some(URL_PAIRS[1])],
		RedditLane::Tor => {
			let origin = lane.auth_origin();
			[Some((origin.base, origin.host)), None]
		}
	}
}

fn canonical_head_is_edge_rejected(status: u16, retry_after_present: bool, quota_headers_present: bool) -> bool {
	matches!(classify_throttle_response(status, retry_after_present, quota_headers_present), Some(ThrottleKind::Edge))
}

fn should_retry_canonical_on_tor(lane: RedditLane, tor_ready: bool) -> bool {
	lane == RedditLane::Direct && tor_ready
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub(crate) enum OauthTransportProfile {
	Chrome140Android,
	Chrome141Android,
	Chrome142Android,
	Chrome143Android,
	Chrome144Android,
	Chrome145Android,
	Firefox142Android,
	Firefox143Android,
	Firefox144Android,
	Firefox145Android,
	Firefox146Android,
	Firefox147Android,
}

pub(crate) const OAUTH_BROWSER_PROFILES: [OauthTransportProfile; 12] = [
	OauthTransportProfile::Chrome140Android,
	OauthTransportProfile::Chrome141Android,
	OauthTransportProfile::Chrome142Android,
	OauthTransportProfile::Chrome143Android,
	OauthTransportProfile::Chrome144Android,
	OauthTransportProfile::Chrome145Android,
	OauthTransportProfile::Firefox142Android,
	OauthTransportProfile::Firefox143Android,
	OauthTransportProfile::Firefox144Android,
	OauthTransportProfile::Firefox145Android,
	OauthTransportProfile::Firefox146Android,
	OauthTransportProfile::Firefox147Android,
];

impl OauthTransportProfile {
	pub(crate) fn label(self) -> &'static str {
		match self {
			Self::Chrome140Android => "chrome_140_android",
			Self::Chrome141Android => "chrome_141_android",
			Self::Chrome142Android => "chrome_142_android",
			Self::Chrome143Android => "chrome_143_android",
			Self::Chrome144Android => "chrome_144_android",
			Self::Chrome145Android => "chrome_145_android",
			Self::Firefox142Android => "firefox_142_android",
			Self::Firefox143Android => "firefox_143_android",
			Self::Firefox144Android => "firefox_144_android",
			Self::Firefox145Android => "firefox_145_android",
			Self::Firefox146Android => "firefox_146_android",
			Self::Firefox147Android => "firefox_147_android",
		}
	}

	pub(crate) fn emulation(self) -> Profile {
		match self {
			Self::Chrome140Android => Profile::Chrome140,
			Self::Chrome141Android => Profile::Chrome141,
			Self::Chrome142Android => Profile::Chrome142,
			Self::Chrome143Android => Profile::Chrome143,
			Self::Chrome144Android => Profile::Chrome144,
			Self::Chrome145Android => Profile::Chrome145,
			Self::Firefox142Android => Profile::Firefox142,
			Self::Firefox143Android => Profile::Firefox143,
			Self::Firefox144Android => Profile::Firefox144,
			Self::Firefox145Android => Profile::Firefox145,
			Self::Firefox146Android => Profile::Firefox146,
			Self::Firefox147Android => Profile::Firefox147,
		}
	}
}

#[derive(Debug)]
struct RedditDnsResolver;

// Keep Reddit's TLS/SNI and HTTP host while resolving direct www requests
// through the upstream Fastly alias. Tor keeps remote SOCKS DNS resolution.
fn reddit_dns_name(name: &str) -> &str {
	if name.eq_ignore_ascii_case(ALTERNATIVE_REDDIT_URL_BASE_HOST) {
		ALTERNATIVE_REDDIT_DNS_HOST
	} else {
		name
	}
}

impl Resolve for RedditDnsResolver {
	fn resolve(&self, name: Name) -> Resolving {
		let dns_name = reddit_dns_name(name.as_str()).to_owned();
		Box::pin(async move {
			tokio::net::lookup_host((dns_name, 0))
				.await
				.map(|addresses| Box::new(addresses) as Addrs)
				.map_err(Into::into)
		})
	}
}

pub fn build_client() -> WreqClient {
	build_emulated_client(RedditLane::Direct, None).expect("Should always be able to build the direct Reddit client")
}

fn build_tor_client(profile: OauthTransportProfile) -> Result<WreqClient, String> {
	let config = TOR_FALLBACK_CONFIG.as_ref().map_err(|error| error.clone())?;
	let config = config.as_ref().ok_or_else(|| "Tor fallback is disabled".to_string())?;
	let isolation_id = format!("redlib-{:016x}", fastrand::u64(..));
	let proxy_url = tor_isolation_proxy_url(&config.proxy_url, &isolation_id)?;
	let proxy = Proxy::all(proxy_url.as_str()).map_err(|error| format!("invalid REDLIB_TOR_PROXY: {error}"))?;
	info!("Configured an isolated Tor SOCKS transport for a Reddit identity");
	build_oauth_client(RedditLane::Tor, Some(proxy), profile)
}

fn tor_isolation_proxy_url(proxy_url: &str, isolation_id: &str) -> Result<String, String> {
	let mut proxy_url = url::Url::parse(proxy_url).map_err(|error| format!("invalid REDLIB_TOR_PROXY: {error}"))?;
	proxy_url
		.set_username(&isolation_id)
		.map_err(|_| "could not add a Tor SOCKS isolation username".to_string())?;
	proxy_url
		.set_password(Some(&isolation_id))
		.map_err(|_| "could not add a Tor SOCKS isolation password".to_string())?;
	Ok(proxy_url.to_string())
}

pub(crate) fn client_for_oauth_profile(lane: RedditLane, profile: OauthTransportProfile) -> Result<Arc<WreqClient>, String> {
	match lane {
		RedditLane::Direct => build_oauth_client(RedditLane::Direct, None, profile).map(Arc::new),
		RedditLane::Tor => build_tor_client(profile).map(Arc::new),
	}
}

pub(crate) fn random_oauth_profile_except(current: OauthTransportProfile) -> OauthTransportProfile {
	let current_index = OAUTH_BROWSER_PROFILES.iter().position(|profile| *profile == current).unwrap_or(0);
	let offset = fastrand::usize(1..OAUTH_BROWSER_PROFILES.len());
	OAUTH_BROWSER_PROFILES[(current_index + offset) % OAUTH_BROWSER_PROFILES.len()]
}

pub fn start_oauth() {
	if DIRECT_OAUTH_WARMUP_STARTED.compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst).is_err() {
		return;
	}

	info!("Starting direct Reddit OAuth in the background");
	tokio::spawn(async {
		let oauth = Oauth::new(RedditLane::Direct).await;
		if install_oauth_client(oauth, true, None) {
			info!("Direct Reddit OAuth is ready");
			tokio::spawn(token_daemon(RedditLane::Direct));
			match rate_limit_check().await {
				Ok(()) => info!("[✅] Rate limit check passed"),
				Err(error) => {
					let mut message = format!("Rate limit check failed after OAuth startup: {error}");
					message += "\nThis may cause issues with the rate limit.";
					message += "\nPlease report this error with the above information.";
					message += "\nhttps://github.com/redlib-org/redlib/issues/new?assignees=sigaloid&labels=bug&title=%F0%9F%90%9B+Bug+Report%3A+Rate+limit+mismatch";
					warn!("{message}");
					eprintln!("{message}");
				}
			}
		}
	});
}

pub fn start_tor_fallback() {
	let config = match TOR_FALLBACK_CONFIG.as_ref() {
		Ok(Some(config)) => config,
		Ok(None) => {
			if *PRIMARY_REDDIT_LANE == RedditLane::Tor {
				warn!("Tor-first routing requires REDLIB_TOR_FALLBACK=on; using direct-first routing");
			}
			return;
		}
		Err(error) => {
			warn!("Tor fallback is disabled because its configuration is invalid: {error}");
			return;
		}
	};
	if TOR_WARMUP_STARTED.compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst).is_err() {
		return;
	}
	info!("Warming Tor fallback through {}: primary_lane={}", config.proxy_url, configured_primary_lane().label());
	tokio::spawn(async {
		let oauth = Oauth::new(RedditLane::Tor).await;
		if install_oauth_client(oauth, true, None) {
			info!("Tor fallback is ready");
			tokio::spawn(token_daemon(RedditLane::Tor));
		}
	});
}

fn build_emulated_client(lane: RedditLane, proxy: Option<Proxy>) -> Result<WreqClient, String> {
	let (selected_emulation, selected_operating_system) = random_emulation_profile();

	build_emulated_client_with_profile(lane, proxy, selected_emulation, selected_operating_system, false, "general")
}

fn build_oauth_client(lane: RedditLane, proxy: Option<Proxy>, profile: OauthTransportProfile) -> Result<WreqClient, String> {
	// Mobile OAuth overrides the browser User-Agent and content type with its
	// stable Reddit Android identity while retaining the selected browser's
	// transport fingerprint and ordinary Accept headers.
	build_emulated_client_with_profile(lane, proxy, profile.emulation(), Platform::Android, false, profile.label())
}

fn random_emulation_profile() -> (Profile, Platform) {
	// Keeping this list short to aid in privacy.
	// The more emulations, the more unique a fingerprint each instance has.
	// But some emulations should increase evasiveness.
	let emulations = [Profile::Chrome145, Profile::Firefox147];
	let emulation_operating_systems = [Platform::Android, Platform::Windows];

	let rand = fastrand::usize(..);
	let selected_emulation = emulations[rand % emulations.len()];
	let selected_operating_system = emulation_operating_systems[rand % emulation_operating_systems.len()];
	(selected_emulation, selected_operating_system)
}

fn build_emulated_client_with_profile(
	lane: RedditLane,
	proxy: Option<Proxy>,
	selected_emulation: Profile,
	selected_operating_system: Platform,
	skip_emulation_headers: bool,
	profile_label: &'static str,
) -> Result<WreqClient, String> {
	let emulation = Emulation::builder()
		.profile(selected_emulation)
		.platform(selected_operating_system)
		.headers(!skip_emulation_headers)
		.build();

	info!(
		"Building Wreq client: lane={} profile={profile_label} emulation={selected_emulation:?} os={selected_operating_system:?} emulation_headers={}",
		lane.label(),
		if skip_emulation_headers { "disabled" } else { "enabled" }
	);
	let mut builder = WreqClient::builder().emulation(emulation).redirect(Policy::none());
	if lane == RedditLane::Direct {
		builder = builder.dns_resolver(RedditDnsResolver);
	}
	if let Some(proxy) = proxy {
		builder = builder.proxy(proxy);
	}
	builder.build().map_err(|error| format!("failed to build {} Reddit client: {error}", lane.label()))
}

/// Gets the canonical path for a resource on Reddit. This is accomplished by
/// making a `HEAD` request to Reddit at the path given in `path`.
///
/// This function returns `Ok(Some(path))`, where `path`'s value is identical
/// to that of the value of the argument `path`, if Reddit responds to our
/// `HEAD` request with a 2xx-family HTTP code. It will also return an
/// `Ok(Some(String))` if Reddit responds to our `HEAD` request with a
/// `Location` header in the response, and the HTTP code is in the 3xx-family;
/// the `String` will contain the path as reported in `Location`. The return
/// value is `Ok(None)` if Reddit responded with a 3xx, but did not provide a
/// `Location` header. An `Err(String)` is returned if Reddit responds with a
/// 429, or if we were unable to decode the value in the `Location` header.
#[cached(size = 1024, time = 600, result = true)]
pub async fn canonical_path(path: String, tries: i8) -> Result<Option<String>, String> {
	with_tor_first_page_budget(&path, canonical_path_routed(path.clone(), tries)).await
}

async fn canonical_path_routed(path: String, tries: i8) -> Result<Option<String>, String> {
	let lane = preferred_api_lane(Instant::now(), &path);
	if configured_primary_lane() == RedditLane::Tor && lane == RedditLane::Direct {
		return canonical_direct_then_tor(path, tries).await;
	}
	match canonical_path_on_lane(path.clone(), tries, lane).await {
		Err(CanonicalPathError::RetryOnTor(_))
			if configured_primary_lane() == RedditLane::Tor
				&& lane == RedditLane::Tor
				&& lane_availability(RedditLane::Direct, Instant::now(), &path) == LaneAvailability::Ready =>
		{
			canonical_direct_then_tor(path, tries).await
		}
		Err(CanonicalPathError::RetryOnTor(_)) if should_retry_canonical_on_tor(lane, tor_fallback_ready(&path)) => {
			info!("Retrying Reddit share-link resolution on the Tor lane");
			canonical_path_on_lane(path, tries, RedditLane::Tor).await.map_err(CanonicalPathError::into_message)
		}
		Ok(path) => Ok(path),
		Err(error) => Err(error.into_message()),
	}
}

async fn canonical_direct_then_tor(path: String, tries: i8) -> Result<Option<String>, String> {
	let direct = async {
		let _standby = DIRECT_STANDBY_CAPACITY
			.try_acquire()
			.map_err(|_| "Reddit direct standby is busy. Retry in 2 seconds".to_string())?;
		match tokio::time::timeout(DIRECT_STANDBY_TIMEOUT, canonical_path_on_lane(path.clone(), tries, RedditLane::Direct)).await {
			Ok(result) => result.map_err(CanonicalPathError::into_message),
			Err(_) => Err("Reddit direct standby timed out. Retry in 2 seconds".to_string()),
		}
	};
	recover_after_direct_standby(direct, || async {
		let recovery = async {
			wait_for_tor_recovery(&path).await?;
			canonical_path_on_lane(path.clone(), tries, RedditLane::Tor).await.map_err(CanonicalPathError::into_message)
		};
		match tokio::time::timeout(TOR_STANDBY_RECOVERY_TIMEOUT, recovery).await {
			Ok(result) => result,
			Err(_) => Err(tor_recovery_wait_message(&path)),
		}
	})
	.await
}

#[async_recursion::async_recursion]
async fn canonical_path_on_lane(path: String, tries: i8, lane: RedditLane) -> Result<Option<String>, CanonicalPathError> {
	if tries == 0 {
		return Ok(None);
	}

	let mut res = None;
	let mut request_failed = false;
	let mut edge_rejected = false;
	let mut non_client_response = false;
	for (url_base, url_base_host) in canonical_head_origins(lane).into_iter().flatten() {
		match reddit_short_head(path.clone(), true, url_base, url_base_host, lane).await {
			Ok(response) => {
				let status = response.status().as_u16();
				let retry_after_present = response.headers().get(wreq_header::RETRY_AFTER).is_some();
				let quota_headers_present = response.headers().get("x-ratelimit-remaining").is_some()
					|| response.headers().get("x-ratelimit-reset").is_some()
					|| response.headers().get("x-ratelimit-used").is_some();
				edge_rejected |= canonical_head_is_edge_rejected(status, retry_after_present, quota_headers_present);
				let client_error = response.status().is_client_error();
				res = Some(response);
				if !client_error {
					non_client_response = true;
					break;
				}
			}
			Err(_) => request_failed = true,
		}
	}

	if !non_client_response && (edge_rejected || res.is_none() && request_failed) {
		return Err(CanonicalPathError::RetryOnTor("Unable to resolve Reddit share link on the current lane.".to_string()));
	}

	let res = res.ok_or_else(|| CanonicalPathError::Terminal("Unable to make HEAD request to Reddit.".to_string()))?;
	let status = res.status().as_u16();
	let policy_error = res.headers().get(wreq_header::RETRY_AFTER).is_some();
	let quota_error = || {
		let delay = rate_limit_delay(
			res.headers().get(wreq_header::RETRY_AFTER).and_then(|value| value.to_str().ok()),
			res.headers().get("x-ratelimit-reset").and_then(|value| value.to_str().ok()),
		);
		CanonicalPathError::Terminal(format!("Reddit rate limit exceeded. Retry in {} seconds", retry_after_seconds(delay)))
	};

	match status {
		// If Reddit responds with a 2xx, then the path is already canonical.
		200..=299 => Ok(Some(path)),

		// If Reddit responds with a 301, then the path is redirected.
		301 => match res.headers().get(wreq_header::LOCATION) {
			Some(val) => {
				let Ok(original) = val.to_str() else {
					return Err(CanonicalPathError::Terminal("Unable to decode Location header.".to_string()));
				};

				// We need to strip the .json suffix from the original path.
				// In addition, we want to remove share parameters.
				// Cut it off here instead of letting it propagate all the way
				// to main.rs
				let stripped_uri = original.strip_suffix(".json").unwrap_or(original).split('?').next().unwrap_or_default();

				// The reason why we now have to format_url, is because the new OAuth
				// endpoints seem to return full paths, instead of relative paths.
				// So we need to strip the .json suffix from the original path, and
				// also remove all Reddit domain parts with format_url.
				// Otherwise, it will literally redirect to Reddit.com.
				let uri = comments_logical_path(&format_url(stripped_uri));

				// Decrement tries and try again
				canonical_path_on_lane(uri, tries - 1, lane).await
			}
			None => Ok(None),
		},

		// If Reddit responds with anything other than 3xx (except for the 2xx and 301
		// as above), return a None.
		300..=399 => Ok(None),

		// Rate limiting
		429 => Err(quota_error()),

		// Special condition rate limiting - https://github.com/redlib-org/redlib/issues/229
		403 if policy_error => Err(quota_error()),

		_ => Ok(
			res
				.headers()
				.get(wreq_header::LOCATION)
				.map(|val| percent_encode(val.as_bytes(), CONTROLS).to_string().trim_start_matches(REDDIT_URL_BASE).to_string()),
		),
	}
}

pub async fn proxy(req: HyperRequest<Body>, format: &str) -> Result<HyperResponse<Body>, String> {
	let media_destination = media_destination_index(format);
	let mut url = format!("{format}?{}", req.uri().query().unwrap_or_default());

	// For each parameter in request
	for (name, value) in &req.params() {
		// Fill the parameter value in the url
		url = url.replace(&format!("{{{name}}}"), value);
	}

	// First parameter is target URL (mandatory).
	let wreq_uri = wreq::Uri::try_from(url).map_err(|_| "Couldn't parse URL".to_string())?;

	let mut builder = CLIENT.get(wreq_uri);

	// Copy useful headers from original request
	for &key in &["Range", "If-Modified-Since", "Cache-Control"] {
		if let Some(value) = req.headers().get(key) {
			builder = builder.header(key, value.as_bytes());
		}
	}

	// Add the current Reddit identity's User-Agent when OAuth is ready. The
	// general media client keeps its own coherent default while OAuth starts.
	if let Some(client) = oauth_client(RedditLane::Direct) {
		builder = builder.header("User-Agent", client.user_agent());
	}

	// This is needed or Reddit will redirect us to a /media landing page that just renders the image.
	builder = builder.header(wreq_header::ACCEPT, "*/*");

	record_media_send(media_destination);
	match builder.send().await {
		Ok(mut res) => {
			record_media_result(Some(res.status().as_u16()));
			let headers = res.headers_mut();

			let mut rm = |key: &str| headers.remove(key);

			rm("access-control-expose-headers");
			rm("server");
			rm("vary");
			rm("etag");
			rm("x-cdn");
			rm("x-cdn-client-region");
			rm("x-cdn-name");
			rm("x-cdn-server-region");
			rm("x-reddit-cdn");
			rm("x-reddit-video-features");
			rm("Nel");
			rm("Report-To");

			Ok(res.into_hyper_response())
		}
		Err(error) => {
			record_media_result(None);
			Err(error.to_string())
		}
	}
}

/// A REST listing or the final response of an anonymous comments read.
enum RedditReadResponse {
	Rest(WreqResponse),
	Comments {
		response: WreqResponse,
		post: Option<Value>,
		request: CommentsRequest,
	},
}

fn graphql_api_error(error: GraphqlError, lane: RedditLane, generation: u64, headers: &wreq::header::HeaderMap) -> ApiRequestError {
	if error.kind == GraphqlErrorKind::Invalid {
		return ApiRequestError::Terminal {
			message: error.message,
			session_unavailable: false,
		};
	}
	if error.kind == GraphqlErrorKind::RateLimit {
		let retry = headers.get("retry-after").and_then(|v| v.to_str().ok()).or(Some("30"));
		let reset = headers.get("x-ratelimit-reset").and_then(|v| v.to_str().ok());
		let (delay, _) = block_for_rate_limit(lane, generation, retry, reset, 4);
		return ApiRequestError::deferred(error.message, CooldownReason::RateLimit, delay);
	}
	ApiRequestError::Terminal {
		message: error.message,
		session_unavailable: error.kind == GraphqlErrorKind::Session,
	}
}

async fn reddit_comments(
	path: &str,
	quarantine: bool,
	oauth: Arc<Oauth>,
	attempt: &mut UpstreamAttempt,
	allow_quota_refresh: bool,
	request: CommentsRequest,
	origin: RedditOrigin,
) -> Result<RedditReadResponse, ApiRequestError> {
	let lane = attempt.lane;
	let mut post = None;
	for (index, payload) in [request.post_payload(), request.comments_payload()].into_iter().enumerate() {
		attempt.mark_sent();
		record_api_send(path, false, lane);
		let response = request_once(&Method::POST, "/".into(), quarantine, origin, oauth.clone(), lane, Some(payload))
			.await
			.map_err(ApiRequestError::Upstream)?;
		if index == 1 || !response.status().is_success() {
			return Ok(RedditReadResponse::Comments { response, post, request });
		}
		let headers = response.headers().clone();
		let remaining = headers
			.get("x-ratelimit-remaining")
			.and_then(|v| v.to_str().ok())
			.and_then(|v| parse_rate_limit_count(Some(v)));
		let reset = headers.get("x-ratelimit-reset").and_then(|v| v.to_str().ok()).and_then(|v| parse_delay_seconds(Some(v)));
		let value = response.json::<Value>().await.map_err(|error| ApiRequestError::Upstream(error.to_string()))?;
		post = Some(match request.post(&value, quarantine) {
			Ok(post) => post,
			Err(error) => {
				warn!("Reddit comments response rejected: lane={} stage=post reason={}", lane.label(), error.reason);
				reconcile_rate_limit(attempt, remaining, reset, remaining == Some(0));
				return Err(graphql_api_error(error, lane, oauth.generation, &headers));
			}
		});
		// The second read is a continuation of the same admitted request. Keep its
		// token, transport, permit and half-open probe, and reserve every wire send.
		// No success is recorded until both responses have been validated.
		reserve_redirect_hop(attempt, oauth.generation, remaining, reset, allow_quota_refresh)?;
	}
	unreachable!("comments reads always have two operations")
}

async fn reddit_get(
	path: String,
	quarantine: bool,
	oauth_client: Arc<Oauth>,
	attempt: &mut UpstreamAttempt,
	allow_quota_refresh: bool,
) -> Result<RedditReadResponse, ApiRequestError> {
	if let Some(request) = CommentsRequest::from_path(&path) {
		let origin = attempt.lane.graphql_origin();
		return reddit_comments(&path, quarantine, oauth_client, attempt, allow_quota_refresh, request, origin).await;
	}
	let lane = attempt.lane;
	let origin = lane.api_origin();
	let generation = oauth_client.generation;
	let mut path = path;
	let mut visited = HashSet::new();

	for redirect_count in 0..=MAX_API_REDIRECTS {
		if !visited.insert(path.clone()) {
			return Err(ApiRequestError::Upstream("Reddit returned a redirect loop".to_string()));
		}

		attempt.mark_sent();
		record_api_send(&path, redirect_count > 0, lane);
		let transport_path = api_transport_path(&path, lane);
		let response = request_once(&Method::GET, transport_path.into_owned(), quarantine, origin, oauth_client.clone(), lane, None)
			.await
			.map_err(ApiRequestError::Upstream)?;
		if !response.status().is_redirection() {
			return Ok(RedditReadResponse::Rest(response));
		}

		if redirect_count == MAX_API_REDIRECTS {
			return Err(ApiRequestError::Upstream(format!("Reddit exceeded the {MAX_API_REDIRECTS}-redirect limit")));
		}

		let location = response
			.headers()
			.get(wreq::header::LOCATION)
			.and_then(|value| value.to_str().ok())
			.ok_or_else(|| ApiRequestError::Upstream("Reddit returned a redirect without a valid Location header".to_string()))?;
		let next_path = validated_reddit_redirect_path(location, lane).map_err(ApiRequestError::Upstream)?;

		let remaining = response
			.headers()
			.get("x-ratelimit-remaining")
			.and_then(|value| value.to_str().ok())
			.and_then(|value| parse_rate_limit_count(Some(value)));
		let reset = response
			.headers()
			.get("x-ratelimit-reset")
			.and_then(|value| value.to_str().ok())
			.and_then(|value| parse_delay_seconds(Some(value)));
		reserve_redirect_hop(attempt, generation, remaining, reset, allow_quota_refresh)?;
		path = next_path;
	}

	Err(ApiRequestError::Upstream("Reddit redirect handling terminated unexpectedly".to_string()))
}

/// Makes a HEAD request to Reddit at `path, using the short URL base. This will not follow redirects.
fn reddit_short_head(path: String, quarantine: bool, base_path: &'static str, host: &'static str, lane: RedditLane) -> Boxed<Result<WreqResponse, String>> {
	CANONICAL_HEAD_SENDS.fetch_add(1, Ordering::Relaxed);
	maybe_log_traffic_summary();
	let Some(oauth_client) = oauth_client(lane) else {
		return async { Err(oauth_startup_error()) }.boxed();
	};
	request_once(&Method::HEAD, path, quarantine, RedditOrigin { base: base_path, host }, oauth_client, lane, None)
}

// /// Makes a HEAD request to Reddit at `path`. This will not follow redirects.
// fn reddit_head(path: String, quarantine: bool) -> Boxed<Result<Response<Body>, String>> {
// 	request(&Method::HEAD, path, false, quarantine, false)
// }
// Unused - reddit_head is only ever called in the context of a short URL

fn validated_reddit_redirect_path(location: &str, lane: RedditLane) -> Result<String, String> {
	if location.starts_with("//") {
		return Err("Reddit returned a scheme-relative redirect".to_string());
	}
	if location.starts_with('/') && location.contains('#') {
		return Err("Reddit returned a redirect with a fragment".to_string());
	}
	let path = if location.starts_with('/') {
		location.to_string()
	} else {
		let url = url::Url::parse(location).map_err(|_| "Reddit returned an invalid redirect URL".to_string())?;
		if url.scheme() != "https"
			|| !url.username().is_empty()
			|| url.password().is_some()
			|| url.port().is_some()
			|| url.fragment().is_some()
			|| !url.host_str().is_some_and(|host| lane.accepts_redirect_host(host))
		{
			return Err("Reddit returned an off-origin redirect".to_string());
		}
		let mut path = url.path().to_string();
		if let Some(query) = url.query() {
			path.push('?');
			path.push_str(query);
		}
		path
	};

	if path.is_empty() || !path.starts_with('/') {
		return Err("Reddit returned an invalid redirect path".to_string());
	}
	Ok(normalize_reddit_api_path(&comments_logical_path(&percent_encode(path.as_bytes(), CONTROLS).to_string())))
}

/// Makes exactly one request to an already-approved Reddit origin. Redirects
/// are deliberately handled by the API wrapper so every wire send receives an
/// admission ticket and a bounded, validated destination.
fn request_once(
	method: &'static Method,
	path: String,
	quarantine: bool,
	origin: RedditOrigin,
	oauth_client: Arc<Oauth>,
	lane: RedditLane,
	payload: Option<Value>,
) -> Boxed<Result<WreqResponse, String>> {
	if oauth_client.lane != lane {
		return async move { Err("Reddit request lane does not match its OAuth identity".to_string()) }.boxed();
	}
	// Keep the logical path for admission, caches and diagnostics; apply Reddit's
	// comments route compatibility encoding only to the outgoing URL.
	let transport_path = comments_transport_path(&path);
	let url = format!("{}{transport_path}", origin.base);

	let mut headers: Vec<(String, String)> = vec![
		("Host".into(), origin.host.into()),
		(
			"Cookie".into(),
			if quarantine {
				"_options=%7B%22pref_quarantine_optin%22%3A%20true%2C%20%22pref_gated_sr_optin%22%3A%20true%7D".into()
			} else {
				"".into()
			},
		),
	];

	for (key, value) in oauth_client.headers_map.clone() {
		headers.push((key, value));
	}

	// shuffle headers: https://github.com/redlib-org/redlib/issues/324
	fastrand::shuffle(&mut headers);

	let client = oauth_client.http_client.clone();
	let mut builder = client.request(method.clone(), &url);

	for (key, value) in headers {
		builder = builder.header(key, value);
	}
	if let Some(payload) = payload {
		builder = builder.json(&payload);
	}

	async move {
		match builder.send().await {
			Ok(response) => Ok(response),
			Err(e) => {
				dbg_msg!("{method} {REDDIT_URL_BASE}{path}: {}", e);

				Err(e.to_string())
			}
		}
	}
	.boxed()
}

/// The Tor OAuth API returns JSON without a `.json` suffix, while its edge
/// rejects the suffixed route. Keep logical paths intact for caches, admission,
/// redirects and endpoint classification; change only the outgoing API GET.
fn api_transport_path(path: &str, lane: RedditLane) -> Cow<'_, str> {
	if lane != RedditLane::Tor {
		return Cow::Borrowed(path);
	}
	let (base, query) = path.split_once('?').map_or((path, None), |(base, query)| (base, Some(query)));
	let Some(base) = base.strip_suffix(".json") else {
		return Cow::Borrowed(path);
	};
	Cow::Owned(match query {
		Some(query) => format!("{base}?{query}"),
		None => base.to_string(),
	})
}

/// Reddit currently rejects literal anonymous comments routes while accepting
/// the same route with one percent-encoded separator. Both forms reach its JSON
/// handler; see https://github.com/redlib-org/redlib/issues/591.
fn comments_transport_path(path: &str) -> Cow<'_, str> {
	let base = path.split('?').next().unwrap_or(path);
	if canonical_comments_base(base).is_none() {
		return Cow::Borrowed(path);
	}
	let separator = if base.starts_with("/comments/") {
		"/comments".len()
	} else if let Some(scoped) = base.strip_prefix("/r/") {
		let Some((subreddit, rest)) = scoped.split_once('/') else {
			return Cow::Borrowed(path);
		};
		if subreddit.is_empty() || !rest.starts_with("comments/") {
			return Cow::Borrowed(path);
		}
		"/r/".len() + subreddit.len()
	} else {
		return Cow::Borrowed(path);
	};
	Cow::Owned(format!("{}%2F{}", &path[..separator], &path[separator + 1..]))
}

/// Remove the comments-route separator encoding if Reddit reflects our
/// transport-only rewrite in a redirect.
fn comments_logical_path(path: &str) -> String {
	let (base, query) = path.split_once('?').map_or((path, None), |(base, query)| (base, Some(query)));
	let logical = if let Some(tail) = base.strip_prefix("/comments%2F").or_else(|| base.strip_prefix("/comments%2f")) {
		format!("/comments/{tail}")
	} else if let Some(scoped) = base.strip_prefix("/r/") {
		let Some((subreddit, tail)) = scoped.split_once("%2Fcomments/").or_else(|| scoped.split_once("%2fcomments/")) else {
			return path.to_string();
		};
		if subreddit.is_empty() || subreddit.contains('/') {
			return path.to_string();
		}
		format!("/r/{subreddit}/comments/{tail}")
	} else {
		return path.to_string();
	};
	if canonical_comments_base(&logical).is_none() {
		return path.to_string();
	}
	match query {
		Some(query) => format!("{logical}?{query}"),
		None => logical,
	}
}

/// Make a request to a Reddit API and parse the JSON response.
///
/// Identical in-flight requests share one result, including errors, but a later
/// request can retry immediately after that flight completes. Successful
/// metadata responses are kept longer than dynamic listings, and either cache
/// can serve its most recent success if a refresh fails.
pub async fn json(path: String, quarantine: bool) -> Result<Value, String> {
	let path = normalize_reddit_api_path(&path);
	record_logical_json(&path);
	json_coalesced(path, quarantine).await
}

async fn json_coalesced(path: String, quarantine: bool) -> Result<Value, String> {
	let key = (path.clone(), quarantine);
	coalesce_json_request(&JSON_FLIGHTS, key, move || async move {
		match json_cache_policy(&path) {
			JsonCachePolicy::Metadata => json_metadata_cached(path, quarantine).await,
			JsonCachePolicy::Comments => json_comments_cached(path, quarantine).await,
			JsonCachePolicy::Dynamic => json_dynamic_cached(path, quarantine).await,
		}
	})
	.await
}

async fn coalesce_json_request<F, Fut>(flights: &'static JsonFlightMap, key: JsonRequestKey, fetch: F) -> JsonRequestResult
where
	F: FnOnce() -> Fut + Send + 'static,
	Fut: std::future::Future<Output = JsonRequestResult> + Send + 'static,
{
	let flight = {
		let mut current = flights.lock().await;
		if let Some(flight) = current.get(&key) {
			flight.clone()
		} else {
			let id = NEXT_JSON_FLIGHT_ID.fetch_add(1, Ordering::Relaxed);
			let (sender, receiver) = watch::channel(None);
			let task_key = key.clone();
			tokio::spawn(async move {
				let result = match std::panic::AssertUnwindSafe(async move { fetch().await }).catch_unwind().await {
					Ok(result) => result,
					Err(_) => {
						error!("Coalesced Reddit JSON request ended unexpectedly");
						Err(JSON_FLIGHT_ABORTED_ERROR.to_string())
					}
				};
				let mut current = flights.lock().await;
				if current.get(&task_key).is_some_and(|flight| flight.id == id) {
					current.remove(&task_key);
				}
				drop(current);
				sender.send_replace(Some(result));
			});
			let flight = JsonFlight { id, receiver };
			current.insert(key.clone(), flight.clone());
			flight
		}
	};

	let mut receiver = flight.receiver;
	loop {
		if let Some(result) = receiver.borrow_and_update().clone() {
			return result;
		}
		if receiver.changed().await.is_err() {
			let mut current = flights.lock().await;
			if current.get(&key).is_some_and(|candidate| candidate.id == flight.id) {
				current.remove(&key);
			}
			return Err(JSON_FLIGHT_ABORTED_ERROR.to_string());
		}
	}
}

#[cached(size = 512, time = 60, result = true, result_fallback = true)]
async fn json_dynamic_cached(path: String, quarantine: bool) -> Result<Value, String> {
	json_uncached(path, quarantine).await
}

#[cached(size = 512, time = 180, result = true, result_fallback = true)]
async fn json_comments_cached(path: String, quarantine: bool) -> Result<Value, String> {
	json_uncached(path, quarantine).await
}

#[cached(size = 512, time = 900, result = true, result_fallback = true)]
async fn json_metadata_cached(path: String, quarantine: bool) -> Result<Value, String> {
	json_uncached(path, quarantine).await
}

fn normalize_reddit_api_path(path: &str) -> String {
	let (base, query) = path.split_once('?').unwrap_or((path, ""));
	let canonical_comments_base = canonical_comments_base(base);
	let mut serializer = url::form_urlencoded::Serializer::new(String::new());
	if canonical_comments_base.is_some() {
		serializer.extend_pairs(normalize_comments_query(query));
	} else {
		let mut pairs = url::form_urlencoded::parse(query.as_bytes())
			.filter(|(key, _)| {
				let key = key.as_ref();
				key != "raw_json" && key != "share_id" && !key.starts_with("utm_")
			})
			.map(|(key, value)| (key.into_owned(), value.into_owned()))
			.collect::<Vec<_>>();
		pairs.push(("raw_json".to_string(), "1".to_string()));
		pairs.sort_by(|(left, _), (right, _)| left.cmp(right));
		serializer.extend_pairs(pairs);
	}
	// Title slugs are cosmetic. Within each route scope, Reddit only needs the
	// post ID and optional highlighted comment ID, so use those as the comments
	// cache identity.
	format!("{}?{}", canonical_comments_base.as_deref().unwrap_or(base), serializer.finish())
}

fn normalize_comments_query(query: &str) -> BTreeMap<String, String> {
	let mut raw = BTreeMap::new();
	for (key, value) in url::form_urlencoded::parse(query.as_bytes()) {
		if matches!(
			key.as_ref(),
			"after" | "comment" | "context" | "depth" | "limit" | "sort" | "showedits" | "showmedia" | "showmore" | "showtitle" | "sr_detail" | "theme" | "threaded" | "truncate"
		) {
			raw.insert(key.into_owned(), value.into_owned());
		}
	}

	let mut normalized = BTreeMap::new();
	for (key, value) in raw {
		let value = match key.as_str() {
			"after" if !value.is_empty() && value.len() <= 8192 && !value.chars().any(char::is_control) => value,
			"sort" if matches!(value.as_str(), "confidence" | "top" | "new" | "controversial" | "old" | "random" | "qa" | "live") => value,
			"context" => match value.parse::<u16>() {
				Ok(context) if context <= 9999 => context.to_string(),
				_ => continue,
			},
			"depth" => match value.parse::<u8>() {
				Ok(depth) if depth <= 10 => depth.to_string(),
				_ => continue,
			},
			"limit" => match value.parse::<u16>() {
				Ok(limit) if limit <= 500 => limit.to_string(),
				_ => continue,
			},
			"comment" if !value.is_empty() && value.len() <= 20 && value.bytes().all(|byte| byte.is_ascii_alphanumeric()) => value,
			"showedits" | "showmedia" | "showmore" | "showtitle" | "sr_detail" | "threaded" if matches!(value.as_str(), "true" | "false") => value,
			"theme" if matches!(value.as_str(), "default" | "dark") => value,
			"truncate" => match value.parse::<u16>() {
				Ok(truncate) if truncate <= 50 => truncate.to_string(),
				_ => continue,
			},
			_ => continue,
		};
		normalized.insert(key, value);
	}
	normalized.insert("raw_json".to_string(), "1".to_string());
	normalized
}

fn canonical_comments_base(path: &str) -> Option<String> {
	let base = path.strip_suffix(".json").unwrap_or(path).trim_end_matches('/');
	let segments = base.trim_start_matches('/').split('/').collect::<Vec<_>>();
	let (prefix, post_id, comment_id) = match segments.as_slice() {
		["comments", post_id] | ["comments", post_id, _] => (String::new(), *post_id, None),
		["comments", post_id, _, comment_id] => (String::new(), *post_id, Some(*comment_id)),
		[prefix @ ("r" | "u" | "user"), scope, "comments", post_id] | [prefix @ ("r" | "u" | "user"), scope, "comments", post_id, _] => {
			(format!("/{prefix}/{scope}"), *post_id, None)
		}
		[prefix @ ("r" | "u" | "user"), scope, "comments", post_id, _, comment_id] => (format!("/{prefix}/{scope}"), *post_id, Some(*comment_id)),
		_ => return None,
	};
	if post_id.is_empty() || comment_id.is_some_and(str::is_empty) {
		return None;
	}
	Some(match comment_id {
		Some(comment_id) => format!("{prefix}/comments/{post_id}/_/{comment_id}.json"),
		None => format!("{prefix}/comments/{post_id}.json"),
	})
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
enum JsonCachePolicy {
	Dynamic,
	Comments,
	Metadata,
}

fn json_cache_policy(path: &str) -> JsonCachePolicy {
	if is_metadata_path(path) {
		JsonCachePolicy::Metadata
	} else if is_comments_path(path) {
		JsonCachePolicy::Comments
	} else {
		JsonCachePolicy::Dynamic
	}
}

fn is_comments_path(path: &str) -> bool {
	let base = path.split('?').next().unwrap_or_default();
	let base = base.strip_suffix(".json").unwrap_or(base).trim_end_matches('/');
	let segments = base.trim_start_matches('/').split('/').collect::<Vec<_>>();
	matches!(
		segments.as_slice(),
		["comments", _]
			| ["comments", _, _]
			| ["comments", _, _, _]
			| ["r", _, "comments", _]
			| ["r", _, "comments", _, _]
			| ["r", _, "comments", _, _, _]
			| ["u", _, "comments", _]
			| ["u", _, "comments", _, _]
			| ["u", _, "comments", _, _, _]
			| ["user", _, "comments", _]
			| ["user", _, "comments", _, _]
			| ["user", _, "comments", _, _, _]
	)
}

fn is_metadata_path(path: &str) -> bool {
	let base = path.split('?').next().unwrap_or_default();
	let segments = base.trim_start_matches('/').split('/').collect::<Vec<_>>();
	match segments.as_slice() {
		["r", sub, "about.json"] => !sub.eq_ignore_ascii_case("random") && !sub.eq_ignore_ascii_case("randnsfw"),
		["user", _, "about.json"] | ["r", _, "wiki.json"] | ["r", _, "wiki", ..] | ["subreddits", "search.json"] => true,
		_ => false,
	}
}

async fn json_uncached(path: String, quarantine: bool) -> Result<Value, String> {
	with_tor_first_page_budget(&path, json_uncached_routed(path.clone(), quarantine)).await
}

async fn json_uncached_routed(path: String, quarantine: bool) -> Result<Value, String> {
	let lane = preferred_api_lane(Instant::now(), &path);
	if configured_primary_lane() == RedditLane::Tor && lane == RedditLane::Direct {
		return direct_then_tor_request(path, quarantine).await;
	}
	let (result, retry_reason) = json_uncached_on_lane(path.clone(), quarantine, lane).await;
	if should_use_direct_standby(configured_primary_lane(), lane, retry_reason, lane_availability(RedditLane::Direct, Instant::now(), &path)) {
		info!("Trying bounded direct standby: endpoint={} reason={retry_reason:?}", endpoint_class(&path));
		return direct_then_tor_request(path, quarantine).await;
	}

	let now = Instant::now();
	let tor_ready = tor_fallback_ready(&path);
	let direct_edge_active = direct_edge_fallback_active(now, &path);
	let direct = lane_availability(RedditLane::Direct, now, &path);
	let direct_refreshing = OAUTH_IS_ROLLING_OVER.load(Ordering::SeqCst);
	let quota_spillover_valid = match retry_reason {
		TorRetryReason::QuotaReserve(ticket) if !direct_refreshing => direct_quota_spillover_valid(ticket, now),
		_ => false,
	};
	if !should_retry_on_tor(lane, retry_reason, tor_ready, direct_edge_active, direct, direct_refreshing, quota_spillover_valid) {
		return result;
	}

	match retry_reason {
		TorRetryReason::EdgeRejected => {
			TOR_FALLBACKS.fetch_add(1, Ordering::Relaxed);
			info!("Retrying edge-rejected Reddit API request on the Tor lane: endpoint={}", endpoint_class(&path));
			json_uncached_on_lane(path, quarantine, RedditLane::Tor).await.0
		}
		TorRetryReason::TransportUnavailable => {
			TOR_FALLBACKS.fetch_add(1, Ordering::Relaxed);
			info!("Using ready Tor after direct entered a connection cooldown: endpoint={}", endpoint_class(&path));
			json_uncached_on_lane(path, quarantine, RedditLane::Tor).await.0
		}
		TorRetryReason::QuotaReserve(ticket) => {
			TOR_QUOTA_SPILLOVER_ATTEMPTS.fetch_add(1, Ordering::Relaxed);
			info!("Trying one bounded Tor request after direct quota reserve exhaustion: endpoint={}", endpoint_class(&path));
			let mut tor_refresh = TorQuotaRefreshRetryState::default();
			let (tor_result, _, _) = json_uncached_on_lane_with_options(
				path.clone(),
				quarantine,
				RedditLane::Tor,
				AdmissionRetryMode::Immediate,
				Some(TOR_QUOTA_SPILLOVER_TIMEOUT),
				Some(ticket),
				&mut tor_refresh,
			)
			.await;
			match tor_result {
				Ok(value) => {
					TOR_QUOTA_SPILLOVER_SUCCESSES.fetch_add(1, Ordering::Relaxed);
					info!("Tor quota spillover recovered the Reddit API request: endpoint={}", endpoint_class(&path));
					Ok(value)
				}
				Err(_) => {
					warn!(
						"Tor quota spillover did not recover the Reddit API request; preserving the direct-lane error: endpoint={}",
						endpoint_class(&path)
					);
					result
				}
			}
		}
		TorRetryReason::None | TorRetryReason::SessionUnavailable => result,
	}
}

async fn with_tor_first_page_budget<T>(path: &str, request: impl std::future::Future<Output = Result<T, String>>) -> Result<T, String> {
	if configured_primary_lane() != RedditLane::Tor {
		return request.await;
	}
	// Return an application loading page before a proxy can replace a long
	// recovery with its own error response. Cancellation releases admission.
	match tokio::time::timeout(TOR_FIRST_PAGE_BUDGET, request).await {
		Ok(result) => result,
		Err(_) => Err(tor_recovery_wait_message(path)),
	}
}

async fn recover_after_direct_standby<T, D, R>(direct: D, recover: impl FnOnce() -> R) -> Result<T, String>
where
	D: std::future::Future<Output = Result<T, String>>,
	R: std::future::Future<Output = Result<T, String>>,
{
	match direct.await {
		Ok(value) => Ok(value),
		Err(_) => recover().await,
	}
}

fn session_refresh_wait_message(backoff: Option<Duration>) -> String {
	match backoff {
		Some(delay) => format!("OAuth token refresh is temporarily unavailable. Retry in {} seconds", retry_after_seconds(delay)),
		None => "Refreshing the anonymous Reddit session. Retry in 2 seconds".to_string(),
	}
}

async fn wait_for_tor_recovery(path: &str) -> Result<(), String> {
	loop {
		// No transport permits are held while waiting for cooldown or OAuth.
		let changed = quota_notify(RedditLane::Tor).notified();
		let refresh_changed = token_refresh_notify(RedditLane::Tor).notified();
		let (delay, rejected) = {
			let guard = upstream_guard(RedditLane::Tor);
			let delay = guard
				.active_endpoint_cooldown(Instant::now(), endpoint_class_index(path))
				.map(|(delay, _)| delay)
				.or_else(|| guard.rejected_oauth_generation.map(|_| LOCAL_QUOTA_RETRY_INTERVAL));
			(delay, guard.rejected_oauth_generation.is_some())
		};
		if rejected {
			if let Some(backoff) = refresh_backoff_remaining(RedditLane::Tor) {
				return Err(session_refresh_wait_message(Some(backoff)));
			}
		}
		let Some(delay) = delay else { return Ok(()) };
		tokio::select! { _ = tokio::time::sleep(delay) => {}, _ = changed => {}, _ = refresh_changed => {} }
	}
}

fn tor_recovery_wait_message(path: &str) -> String {
	let guard = upstream_guard(RedditLane::Tor);
	if guard.rejected_oauth_generation.is_some() {
		return session_refresh_wait_message(refresh_backoff_remaining(RedditLane::Tor));
	}
	let delay = guard
		.active_endpoint_cooldown(Instant::now(), endpoint_class_index(path))
		.map_or(Duration::from_secs(5), |(delay, _)| delay);
	format!(
		"Reddit API request timed out while recovering the Tor lane. Retry in {} seconds",
		retry_after_seconds(delay)
	)
}

async fn direct_then_tor_request(path: String, quarantine: bool) -> Result<Value, String> {
	recover_after_direct_standby(direct_standby_request(path.clone(), quarantine), || async move {
		info!("Returning to Tor after direct standby did not recover the page: endpoint={}", endpoint_class(&path));
		let recovery = async {
			wait_for_tor_recovery(&path).await?;
			json_uncached_on_lane(path.clone(), quarantine, RedditLane::Tor).await.0
		};
		match tokio::time::timeout(TOR_STANDBY_RECOVERY_TIMEOUT, recovery).await {
			Ok(result) => result,
			Err(_) => Err(tor_recovery_wait_message(&path)),
		}
	})
	.await
}

async fn direct_standby_request(path: String, quarantine: bool) -> Result<Value, String> {
	let _standby = DIRECT_STANDBY_CAPACITY
		.try_acquire()
		.map_err(|_| "Reddit direct standby is busy. Retry in 2 seconds".to_string())?;
	DIRECT_STANDBY_ATTEMPTS.fetch_add(1, Ordering::Relaxed);
	match tokio::time::timeout(DIRECT_STANDBY_TIMEOUT, json_uncached_on_lane(path, quarantine, RedditLane::Direct)).await {
		Ok((result, _)) => result,
		Err(_) => Err("Reddit direct standby timed out. Retry in 2 seconds".to_string()),
	}
}

fn should_use_direct_standby(primary: RedditLane, lane: RedditLane, reason: TorRetryReason, direct: LaneAvailability) -> bool {
	primary == RedditLane::Tor
		&& lane == RedditLane::Tor
		&& direct == LaneAvailability::Ready
		&& matches!(
			reason,
			TorRetryReason::EdgeRejected | TorRetryReason::TransportUnavailable | TorRetryReason::SessionUnavailable
		)
}

fn should_retry_on_tor(
	lane: RedditLane,
	reason: TorRetryReason,
	tor_ready: bool,
	direct_edge_active: bool,
	direct: LaneAvailability,
	direct_refreshing: bool,
	quota_spillover_valid: bool,
) -> bool {
	if lane != RedditLane::Direct || !tor_ready {
		return false;
	}
	match reason {
		TorRetryReason::EdgeRejected => direct_edge_active,
		TorRetryReason::TransportUnavailable => direct == LaneAvailability::TransportCooldown,
		TorRetryReason::QuotaReserve(_) => !direct_refreshing && quota_spillover_valid,
		TorRetryReason::None | TorRetryReason::SessionUnavailable => false,
	}
}

fn transport_retry_delay(retry_count: u8) -> Option<Duration> {
	(retry_count < MAX_TRANSPORT_RETRIES).then(|| TRANSPORT_RETRY_BASE_DELAY.saturating_mul(1_u32 << retry_count))
}

fn upstream_cooldown_retry_delay(delay: Duration, wait_count: u8, elapsed: Duration) -> Option<Duration> {
	if wait_count >= MAX_UPSTREAM_COOLDOWN_WAITS || elapsed >= UPSTREAM_RECOVERY_BUDGET {
		return None;
	}
	let remaining = UPSTREAM_RECOVERY_BUDGET.saturating_sub(elapsed);
	(delay <= remaining).then_some(delay)
}

struct RecoveryBudget {
	initial_deadline: tokio::time::Instant,
	started: Option<tokio::time::Instant>,
}

impl RecoveryBudget {
	fn new(now: tokio::time::Instant, initial_budget: Duration) -> Self {
		Self {
			initial_deadline: now + initial_budget,
			started: None,
		}
	}

	fn begin(&mut self, now: tokio::time::Instant) -> Duration {
		now.saturating_duration_since(*self.started.get_or_insert(now))
	}

	fn remaining(&self, now: tokio::time::Instant) -> Duration {
		self.started.map_or(self.initial_deadline.saturating_duration_since(now), |started| {
			UPSTREAM_RECOVERY_BUDGET.saturating_sub(now.saturating_duration_since(started))
		})
	}
}

fn request_timeout_is_transport_failure(quota_spillover: bool) -> bool {
	!quota_spillover
}

fn quota_refresh_launch_allowed(mode: AdmissionRetryMode, quota_spillover: bool, refresh_owned: bool) -> bool {
	mode == AdmissionRetryMode::Bounded && !quota_spillover && !refresh_owned
}

fn tor_redirect_refresh_recovery(lane: RedditLane, mode: AdmissionRetryMode, quota_spillover: bool, ticket: QuotaRefreshWaitTicket) -> LaneRecoveryReason {
	if lane == RedditLane::Tor && mode == AdmissionRetryMode::Bounded && !quota_spillover {
		LaneRecoveryReason::TorQuotaRefreshWait(ticket)
	} else {
		LaneRecoveryReason::None
	}
}

async fn json_uncached_on_lane(path: String, quarantine: bool, lane: RedditLane) -> (Result<Value, String>, TorRetryReason) {
	let initial_timeout = initial_lane_request_timeout(lane, configured_primary_lane(), lane_availability(RedditLane::Direct, Instant::now(), &path));
	let mut budget = RecoveryBudget::new(tokio::time::Instant::now(), CAPACITY_WAIT_BUDGET + TOR_QUOTA_REFRESH_RETRY_BUDGET + initial_timeout);
	let mut transport_retries = 0;
	let mut cooldown_waits = 0;
	let mut tor_refresh = TorQuotaRefreshRetryState::default();
	loop {
		let timeout_override = Some(if budget.started.is_some() { TRANSPORT_RETRY_TIMEOUT } else { initial_timeout });
		let attempt_budget = budget.remaining(tokio::time::Instant::now());
		let attempt = json_uncached_on_lane_with_options(path.clone(), quarantine, lane, AdmissionRetryMode::Bounded, timeout_override, None, &mut tor_refresh);
		let (result, tor_retry_reason, recovery_reason) = match tokio::time::timeout(attempt_budget, attempt).await {
			Ok(result) => result,
			Err(_) => return (Err("Reddit API request timed out after bounded server-side recovery".to_string()), TorRetryReason::None),
		};
		// A request can open direct's failure cooldown after initial routing.
		// Yield before sleeping so the caller can hand it to an already-ready Tor
		// lane. The caller rechecks availability and never routes recursively.
		let now = Instant::now();
		if lane == RedditLane::Direct
			&& (matches!(recovery_reason, LaneRecoveryReason::TransportFailure | LaneRecoveryReason::UpstreamCooldown(_))
				|| tor_retry_reason == TorRetryReason::TransportUnavailable)
			&& lane_availability(RedditLane::Direct, now, &path) == LaneAvailability::TransportCooldown
			&& lane_availability(RedditLane::Tor, now, &path) == LaneAvailability::Ready
		{
			return (result, TorRetryReason::TransportUnavailable);
		}
		if lane == RedditLane::Tor
			&& configured_primary_lane() == RedditLane::Tor
			&& (matches!(recovery_reason, LaneRecoveryReason::TransportFailure) || tor_retry_reason == TorRetryReason::TransportUnavailable)
			&& lane_availability(RedditLane::Direct, Instant::now(), &path) == LaneAvailability::Ready
		{
			return (result, TorRetryReason::TransportUnavailable);
		}
		let delay = match recovery_reason {
			LaneRecoveryReason::TorQuotaRefreshWait(ticket) => {
				if !tor_refresh.claim_redirect_replay(lane, ticket, Instant::now()) {
					return (result, tor_retry_reason);
				}
				info!("Joining active Tor quota refresh before one full GET replay: endpoint={}", endpoint_class(&path));
				// The previous attempt and permit are already dropped. Re-enter
				// admission so token, transport, quota and edge guards are selected
				// atomically; never swap identities halfway through a redirect chain.
				continue;
			}
			LaneRecoveryReason::TransportFailure => {
				budget.begin(tokio::time::Instant::now());
				let Some(delay) = transport_retry_delay(transport_retries) else {
					return (result, tor_retry_reason);
				};
				transport_retries = transport_retries.saturating_add(1);
				warn!(
					"Retrying transient Reddit transport failure on the same lane: lane={} endpoint={} retry={}/{} delay_milliseconds={}",
					lane.label(),
					endpoint_class(&path),
					transport_retries,
					MAX_TRANSPORT_RETRIES,
					delay.as_millis(),
				);
				delay
			}
			LaneRecoveryReason::UpstreamCooldown(delay) | LaneRecoveryReason::QuotaCooldown(delay) => {
				let elapsed = budget.begin(tokio::time::Instant::now());
				let Some(delay) = upstream_cooldown_retry_delay(delay, cooldown_waits, elapsed) else {
					return (result, tor_retry_reason);
				};
				cooldown_waits = cooldown_waits.saturating_add(1);
				info!(
					"Waiting through transient Reddit upstream cooldown before retrying server-side: lane={} endpoint={} wait={}/{} delay_milliseconds={}",
					lane.label(),
					endpoint_class(&path),
					cooldown_waits,
					MAX_UPSTREAM_COOLDOWN_WAITS,
					delay.as_millis(),
				);
				delay
			}
			LaneRecoveryReason::None => {
				if result.is_ok() && (transport_retries > 0 || cooldown_waits > 0) {
					info!(
						"Server-side Reddit transport recovery succeeded: lane={} endpoint={} transport_retries={} cooldown_waits={}",
						lane.label(),
						endpoint_class(&path),
						transport_retries,
						cooldown_waits,
					);
				}
				return (result, tor_retry_reason);
			}
		};
		if delay >= budget.remaining(tokio::time::Instant::now()) {
			return (result, tor_retry_reason);
		}
		tokio::time::sleep(delay).await;
	}
}

fn initial_lane_request_timeout(lane: RedditLane, primary: RedditLane, direct: LaneAvailability) -> Duration {
	// A healthy standby can recover a stalled Tor send before the old 30-second
	// network deadline. Tor retains the longer budget when direct is unavailable;
	// a failed standby still returns to the separately bounded Tor recovery.
	if lane == RedditLane::Tor && primary == RedditLane::Tor && direct == LaneAvailability::Ready {
		TOR_PRIMARY_STANDBY_DELAY
	} else {
		lane.request_timeout().min(Duration::from_secs(30))
	}
}

fn start_rejected_session_refresh(lane: RedditLane, generation: u64) {
	let mut guard = upstream_guard(lane);
	if !guard.reject_oauth_generation(generation) {
		return;
	}
	drop(guard);
	tokio::spawn(async move {
		// One worker per rejected generation. OAuth's existing backoff and
		// single-refresh guard also apply while the direct lane serves traffic.
		loop {
			if !oauth_client(lane).is_some_and(|client| client.generation == generation) {
				return;
			}
			let delay = upstream_guard(lane).rejected_session_refresh_delay(Instant::now());
			if !delay.is_zero() {
				tokio::time::sleep(delay).await;
				continue;
			}
			let outcome = force_refresh_token(lane, RefreshReason::Unauthorized).await;
			if matches!(outcome, crate::oauth::RefreshOutcome::Refreshed | crate::oauth::RefreshOutcome::Superseded) {
				return;
			}
			tokio::time::sleep(outcome.retry_after().unwrap_or(Duration::from_secs(2))).await;
		}
	});
}

async fn json_uncached_on_lane_with_options(
	path: String,
	quarantine: bool,
	lane: RedditLane,
	admission_mode: AdmissionRetryMode,
	timeout_override: Option<Duration>,
	quota_spillover_ticket: Option<QuotaSpilloverTicket>,
	tor_refresh: &mut TorQuotaRefreshRetryState,
) -> (Result<Value, String>, TorRetryReason, LaneRecoveryReason) {
	// Closure to quickly build errors
	let err = |msg: &str, e: String, path: String| -> Result<Value, String> {
		// eprintln!("{} - {}: {}", url, msg, e);
		Err(format!("{msg}: {e} | {path}"))
	};

	let request_timeout = timeout_override.unwrap_or_else(|| lane.request_timeout());
	if let Some(ticket) = quota_spillover_ticket {
		debug_assert_eq!(lane, RedditLane::Tor);
		debug_assert_eq!(admission_mode, AdmissionRetryMode::Immediate);
		if !direct_quota_spillover_valid(ticket, Instant::now()) {
			return (
				Err("Tor quota spillover skipped because direct-lane state changed".to_string()),
				TorRetryReason::None,
				LaneRecoveryReason::None,
			);
		}
	}

	// Keep this exact OAuth client throughout redirects and attach its generation
	// to the response. A late response from an old identity must not overwrite a
	// newly rotated identity's request budget.
	let (oauth_client, mut upstream_attempt, _permit, _probe) = match begin_upstream_attempt(lane, endpoint_class_index(&path), admission_mode, tor_refresh).await {
		Ok(attempt) => attempt,
		Err(denied) => {
			let retry_reason = if upstream_guard(lane).rejected_oauth_generation.is_some() {
				TorRetryReason::SessionUnavailable
			} else if denied.edge_deferred {
				TorRetryReason::EdgeRejected
			} else if denied.admission.is_some_and(|admission| admission.reason == CooldownReason::UpstreamFailures) {
				TorRetryReason::TransportUnavailable
			} else {
				denied.quota_spillover.map_or(TorRetryReason::None, TorRetryReason::QuotaReserve)
			};
			let recovery_reason = denied.admission.as_ref().map_or(LaneRecoveryReason::None, |admission| {
				if admission.reason == CooldownReason::UpstreamFailures {
					LaneRecoveryReason::UpstreamCooldown(admission.delay)
				} else if admission.reason == CooldownReason::RateLimit && (lane == RedditLane::Tor || admission.source == "active_cooldown") && denied.tor_quota_refresh.is_none() {
					LaneRecoveryReason::QuotaCooldown(admission.delay)
				} else {
					LaneRecoveryReason::None
				}
			});
			return (Err(denied.message), retry_reason, recovery_reason);
		}
	};
	// Queueing and admission have separate bounds; neither consumes network time.
	let request_deadline = tokio::time::Instant::now() + request_timeout;
	let request_generation = oauth_client.generation;
	// Admission atomically selects the OAuth client and owns its quota
	// reservation and edge half-open probe.
	record_admitted_json(&path);
	let timeout_path = path.clone();
	let mut retry_reason = TorRetryReason::None;
	let mut recovery_reason = LaneRecoveryReason::None;
	let allow_quota_refresh = quota_refresh_launch_allowed(admission_mode, quota_spillover_ticket.is_some(), tor_refresh.owner.is_some());

	// Fetch the url...
	let result = tokio::time::timeout_at(request_deadline, async {
		match reddit_get(path.clone(), quarantine, oauth_client, &mut upstream_attempt, allow_quota_refresh).await {
			Ok(response) => {
				let (response, graphql) = match response {
					RedditReadResponse::Rest(response) => (response, None),
					RedditReadResponse::Comments { response, post, request } => (response, Some((post, request))),
				};
				let status = response.status();
				let status_code = status.as_u16();

				let response_headers = response.headers().clone();
				let remaining = response_headers.get("x-ratelimit-remaining").and_then(|value| value.to_str().ok());
				let reset = response_headers.get("x-ratelimit-reset").and_then(|value| value.to_str().ok());
				let used = response_headers.get("x-ratelimit-used").and_then(|value| value.to_str().ok());
				let retry_after = response_headers.get(wreq_header::RETRY_AFTER).and_then(|value| value.to_str().ok());
				let parsed_remaining = parse_rate_limit_count(remaining);
				let parsed_used = parse_rate_limit_count(used);
				let reset_duration = parse_delay_seconds(reset);
				let retry_after_duration = parse_retry_after(retry_after, SystemTime::now());
				let quota_headers_present = remaining.is_some() || reset.is_some() || used.is_some();

				let challenge = response_headers.get(wreq_header::WWW_AUTHENTICATE).and_then(|value| value.to_str().ok());
				let auth_rejected = credential_rejected(status_code, challenge);
				let content_kind = response_headers.get(wreq_header::CONTENT_TYPE).and_then(|value| value.to_str().ok()).map_or("missing", |value| {
					if value.starts_with("application/json") { "json" } else if value.starts_with("text/html") { "html" } else { "other" }
				});
				let mut response = response.into_hyper_response();
				let rejection = if status_code == 403 { Some(read_rejection_details(response.body_mut()).await) } else { None };
				let resource_reason = rejection.as_ref().and_then(|details| details.resource_reason);
				let throttle_kind = if resource_reason.is_some() || auth_rejected { None } else if graphql.is_some() && status_code == 403 && content_kind == "html" && !quota_headers_present {
					// The GraphQL API reports resource errors as JSON. An HTML 403
					// is an edge rejection even when that edge omits Retry-After.
					Some(ThrottleKind::Edge)
				} else {
					classify_throttle_response(status_code, retry_after.is_some(), quota_headers_present)
				};
				if status_code == 403 {
					warn!("Reddit rejection: lane={} endpoint={} request_id={} generation={} content_kind={content_kind} body_kind={} resource_reason={} credential_rejected={auth_rejected}",
						lane.label(), endpoint_class(&path), upstream_attempt.request_id, request_generation,
						rejection.as_ref().map_or("missing", |details| details.body_kind), resource_reason.unwrap_or("none"));
				}
				reconcile_rate_limit(
					&mut upstream_attempt,
					parsed_remaining,
					reset_duration,
					matches!(throttle_kind, Some(ThrottleKind::Quota)) || parsed_remaining == Some(0),
				);
				if throttle_kind.is_none() && !auth_rejected && resource_reason.is_none() {
					maybe_rotate_low_budget(lane, request_generation, parsed_remaining, parsed_used, reset_duration, &path);
				}
				trace!(
					"Reddit rate-limit observation: remaining={} reset_seconds={} used={} endpoint={} current_generation={} request_id={} discovery_probe={} rollover={}",
					parsed_remaining.map_or(0, u16::from),
					reset_duration.map_or(0, |duration| duration.as_secs()),
					parsed_used.map_or(0, u16::from),
					endpoint_class(&path),
					is_current_oauth_generation(lane, request_generation),
					upstream_attempt.request_id,
					upstream_attempt.discovery_probe,
					match lane {
						RedditLane::Direct => OAUTH_IS_ROLLING_OVER.load(Ordering::SeqCst),
						RedditLane::Tor => TOR_OAUTH_IS_ROLLING_OVER.load(Ordering::SeqCst),
					},
				);

				if let Some(reason) = resource_reason { return Err(reason.to_string()); }

				match throttle_kind {
					Some(ThrottleKind::Quota) => {
						let (delay, response_is_current) = block_for_rate_limit(lane, request_generation, retry_after, reset, endpoint_class_index(&path));
						if response_is_current {
							recovery_reason = LaneRecoveryReason::QuotaCooldown(delay);
						}
						warn!(
							"Reddit quota response: status={} endpoint={} retry_after_seconds={} remaining_present={} reset_seconds={} used_present={} current_generation={response_is_current}",
							status,
							endpoint_class(&path),
							retry_after_duration.map_or(0, |duration| duration.as_secs()),
							remaining.is_some(),
							reset_duration.map_or(0, |duration| duration.as_secs()),
							used.is_some(),
						);
						return Err(format!("Reddit rate limit exceeded. Retry in {} seconds", retry_after_seconds(delay)));
					}
					Some(ThrottleKind::Edge) => {
						retry_reason = TorRetryReason::EdgeRejected;
						let decision = block_for_edge_throttle(&mut upstream_attempt, retry_after_duration);
						match decision {
							decision if decision.started_cooldown => warn!(
								"Reddit edge throttle: lane={} status={} endpoint={} retry_after_present={} retry_after_valid={} retry_after_seconds={} quota_headers_present={} consecutive_failures={} episode_seconds={} cooldown_seconds={} half_open_probe={} request_generation={} current_generation={} current_identity_age_seconds={}",
								lane.label(), status,
								endpoint_class(&path),
								retry_after.is_some(),
								retry_after_duration.is_some(),
								retry_after_duration.map_or(0, |duration| duration.as_secs()),
								quota_headers_present,
								decision.consecutive_failures,
								decision.episode_seconds,
								decision.delay.as_secs(),
								upstream_attempt.edge.half_open,
								request_generation,
								decision.current_generation,
								decision.identity_age_seconds,
							),
							decision => trace!(
								"Reddit edge throttle joined existing cooldown: endpoint={} cooldown_seconds={}",
								endpoint_class(&path),
								decision.delay.as_secs(),
							),
						}
						let delay = decision.delay;
						return Err(format!("Reddit is temporarily rejecting this instance. Retry in {} seconds", retry_after_seconds(delay)));
					}
					None => {}
				}

				if auth_rejected {
					if !is_current_oauth_generation(lane, request_generation) {
						return Err("OAuth token changed while this request was in flight. Please retry.".to_string());
					}
					if configured_primary_lane() == RedditLane::Tor && lane == RedditLane::Tor {
						start_rejected_session_refresh(lane, request_generation);
						retry_reason = TorRetryReason::SessionUnavailable;
						return Err(session_refresh_wait_message(refresh_backoff_remaining(lane)));
					}
					error!("Reddit rejected the OAuth token; forcing a refresh");
					let outcome = force_refresh_token(lane, RefreshReason::Unauthorized).await;
					if let Some(delay) = outcome.retry_after() {
						return Err(format!("OAuth token refresh is temporarily unavailable. Retry in {} seconds", retry_after_seconds(delay)));
					}
					return Err("OAuth token has expired. Please refresh the page!".to_string());
				}

				if status_code == 403 {
					return Err("Reddit error 403: Forbidden".to_string());
				}

				if status.is_server_error() {
					record_upstream_failure(lane, "http_status", Some(status_code), &path, request_generation);
					return Err("Reddit is having issues, check if there's an outage".to_string());
				}

				// asynchronously aggregate the chunks of the body
				match hyper::body::aggregate(response).await {
					Ok(body) => {
						let has_remaining = body.has_remaining();

						if !has_remaining {
							record_upstream_failure(lane, "empty_body", Some(status.as_u16()), &path, request_generation);
							return Err(format!("Reddit returned an empty response (status {status})"));
						}

						// Parse the response from Reddit as JSON
						match serde_json::from_reader(body.reader()) {
							Ok(value) => {
								let json: Value = value;
								let json = if let Some((post, request)) = graphql {
									match request.finish(post.unwrap_or(Value::Null), &json) {
										Ok(json) => json,
										Err(error) => {
											warn!("Reddit comments response rejected: lane={} stage=comments reason={}",lane.label(),error.reason);
											if error.kind == GraphqlErrorKind::Session {
												start_rejected_session_refresh(lane, request_generation);
												retry_reason = TorRetryReason::SessionUnavailable;
											}
											if error.kind == GraphqlErrorKind::RateLimit {
												let (delay, current) = block_for_rate_limit(lane, request_generation, retry_after.or(Some("30")), reset, endpoint_class_index(&path));
												if current {
													recovery_reason = LaneRecoveryReason::QuotaCooldown(delay);
												}
											}
											return Err(error.message);
										}
									}
								} else {
									json
								};

								// If user is suspended
								if let Some(data) = json.get("data") {
									if let Some(is_suspended) = data.get("is_suspended").and_then(Value::as_bool) {
										if is_suspended {
											return Err("suspended".into());
										}
									}
								}

								// If Reddit returned an error
								if json["error"].is_i64() {
									// OAuth token has expired; http status 401
									if json["message"] == "Unauthorized" {
										if !is_current_oauth_generation(lane, request_generation) {
											return Err("OAuth token changed while this request was in flight. Please retry.".to_string());
										}
										error!("Forcing a token refresh");
										let outcome = force_refresh_token(lane, RefreshReason::Unauthorized).await;
										if let Some(delay) = outcome.retry_after() {
											return Err(format!("OAuth token refresh is temporarily unavailable. Retry in {} seconds", retry_after_seconds(delay)));
										}
										return Err("OAuth token has expired. Please refresh the page!".to_string());
									}

									// Handle quarantined
									if json["reason"] == "quarantined" {
										return Err("quarantined".into());
									}
									// Handle gated
									if json["reason"] == "gated" {
										return Err("gated".into());
									}
									// Handle private subs
									if json["reason"] == "private" {
										return Err("private".into());
									}
									// Handle banned subs
									if json["reason"] == "banned" {
										return Err("banned".into());
									}

									Err(format!("Reddit error {} \"{}\": {} | {path}", json["error"], json["reason"], json["message"]))
								} else if !status.is_success() {
									Err(format!("Reddit returned an unexpected response status: {status}"))
								} else {
									if !quota_headers_present {
										confirm_headerless_quota(&upstream_attempt);
									}
									record_upstream_success(&mut upstream_attempt, &path);
									Ok(json)
								}
							}
							Err(e) => {
								error!("Got an invalid response from reddit {e}. Status code: {status}");
								record_upstream_failure(lane, "invalid_json", Some(status.as_u16()), &path, request_generation);
								err("Failed to parse page JSON data", e.to_string(), path)
							}
						}
					}
					Err(e) => {
						record_upstream_failure(lane, "body_transport", Some(status.as_u16()), &path, request_generation);
						if quota_spillover_ticket.is_none() {
							recovery_reason = LaneRecoveryReason::TransportFailure;
						}
						err("Failed receiving body from Reddit", e.to_string(), path)
					}
				}
			}
			Err(ApiRequestError::Deferred {
				message,
				edge_rejected: deferred_edge_rejected,
				recovery,
			}) => {
				if deferred_edge_rejected {
					retry_reason = TorRetryReason::EdgeRejected;
				} else if admission_mode == AdmissionRetryMode::Bounded && quota_spillover_ticket.is_none() {
					// Preserve the denial even if its cooldown expires before handling.
					// Replayed GET admission rechecks any intervening extension.
					recovery_reason = recovery;
				}
				Err(message)
			}
			Err(ApiRequestError::TorQuotaRefreshWait { message, ticket }) => {
				recovery_reason = tor_redirect_refresh_recovery(lane, admission_mode, quota_spillover_ticket.is_some(), ticket);
				Err(message)
			}
			Err(ApiRequestError::Terminal { message, session_unavailable }) => {
				if session_unavailable {
					start_rejected_session_refresh(lane, request_generation);
					retry_reason = TorRetryReason::SessionUnavailable;
				}
				Err(message)
			}
			Err(ApiRequestError::Upstream(error)) => {
				record_upstream_failure(lane, "request_transport", None, &path, request_generation);
				if quota_spillover_ticket.is_none() {
					recovery_reason = LaneRecoveryReason::TransportFailure;
				}
				err("Couldn't send request to Reddit", error, path)
			}
		}
	})
	.await;

	let result = match result {
		Ok(result) => result,
		Err(_) => {
			if request_timeout_is_transport_failure(quota_spillover_ticket.is_some()) {
				record_upstream_failure(lane, "request_timeout", None, &timeout_path, request_generation);
				recovery_reason = LaneRecoveryReason::TransportFailure;
			} else {
				info!("Bounded Tor quota spillover reached its latency budget: endpoint={}", endpoint_class(&timeout_path));
			}
			Err(format!("Reddit API request timed out after {} seconds", request_timeout.as_secs()))
		}
	};
	(result, retry_reason, recovery_reason)
}

async fn self_check_on_lane(sub: &str, lane: RedditLane) -> Result<(), String> {
	let query = normalize_reddit_api_path(&format!("/r/{sub}/hot.json?raw_json=1"));
	record_logical_json(&query);
	let (response, _) = json_uncached_on_lane(query, true, lane).await;
	let response = response?;
	if response["data"]["children"].as_array().is_some() {
		Ok(())
	} else {
		Err("No posts found".to_string())
	}
}

fn oauth_startup_validation_lane() -> RedditLane {
	RedditLane::Direct
}

pub async fn rate_limit_check() -> Result<(), String> {
	oauth_client(RedditLane::Direct).ok_or_else(oauth_startup_error)?;

	// Make one uncached request. Quota-driven identity rotation is handled only
	// after Reddit reports a low budget, rather than creating extra authentication
	// traffic during every startup.
	// This check specifically validates the newly installed direct identity.
	// Do not allow the normal edge-triggered Tor failover to hide a direct-lane
	// rejection here.
	self_check_on_lane("reddit", oauth_startup_validation_lane()).await?;
	Ok(())
}

trait IntoHyperResponse {
	fn into_hyper_response(self) -> HyperResponse<Body>;
}

impl IntoHyperResponse for WreqResponse {
	fn into_hyper_response(self) -> HyperResponse<Body> {
		let status = self.status();
		let version = self.version();

		let mut builder = HyperResponse::builder().status(status.as_u16()).version(match version {
			wreq::Version::HTTP_09 => hyper::Version::HTTP_09,
			wreq::Version::HTTP_10 => hyper::Version::HTTP_10,
			wreq::Version::HTTP_11 => hyper::Version::HTTP_11,
			wreq::Version::HTTP_2 => hyper::Version::HTTP_2,
			wreq::Version::HTTP_3 => hyper::Version::HTTP_3,
			_ => hyper::Version::HTTP_11,
		});

		for (name, value) in self.headers() {
			builder = builder.header(
				header::HeaderName::from_bytes(name.as_str().as_bytes()).unwrap(),
				header::HeaderValue::from_bytes(value.as_bytes()).unwrap(),
			);
		}

		builder.body(Body::wrap_stream(self.bytes_stream())).unwrap()
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use std::sync::atomic::AtomicUsize;
	use {crate::config::get_setting, sealed_test::prelude::*};

	#[test]
	fn test_graphql_comments_budget_is_independent_of_listing_responses() {
		for lane in [RedditLane::Direct, RedditLane::Tor] {
			let now = Instant::now();
			let mut guard = UpstreamGuard::new(lane);
			guard.install_oauth_generation(7, true);
			let mut listing = guard.try_admit_endpoint(now, 7, 0).unwrap();
			let mut comments = guard.try_admit_endpoint(now, 7, 4).unwrap();
			guard.reconcile_quota(now, &mut comments, Some(599), Some(Duration::from_secs(300)), false);
			comments.complete();
			guard.reconcile_quota(now, &mut listing, Some(99), Some(Duration::from_secs(300)), false);
			listing.complete();
			assert!(matches!(guard.graphql_quota.window, QuotaWindow::Known { available: 599, .. }));
			assert!(matches!(guard.quota.window, QuotaWindow::Known { available: 99, .. }));
			// A later REST observation must not lower the comments allowance.
			let mut listing = guard.try_admit_endpoint(now, 7, 0).unwrap();
			guard.reconcile_quota(now, &mut listing, Some(0), Some(Duration::from_secs(300)), true);
			listing.complete();
			assert_eq!(guard.routing_availability(now, 7, 0), LaneAvailability::LocalQuotaPause);
			assert_eq!(guard.routing_availability(now, 7, 4), LaneAvailability::Ready);
			let mut comments = guard.try_admit_endpoint(now, 7, 4).unwrap();
			guard.reconcile_quota(now, &mut comments, Some(590), Some(Duration::from_secs(300)), false);
			comments.complete();
			assert!(matches!(guard.graphql_quota.window, QuotaWindow::Known { available: 590, .. }));
			assert!(matches!(guard.quota.window, QuotaWindow::Known { available: 0, .. }));
		}
	}

	#[test]
	fn test_graphql_comments_cooldowns_refresh_and_spillover_are_pool_scoped() {
		let now = Instant::now();
		let mut guard = UpstreamGuard::new(RedditLane::Direct);
		guard.install_oauth_generation(7, true);
		guard.quota.window = QuotaWindow::Known {
			available: 0,
			reset_at: now + Duration::from_secs(300),
		};
		guard.graphql_quota.window = QuotaWindow::Known {
			available: 599,
			reset_at: now + Duration::from_secs(300),
		};
		let ticket = QuotaSpilloverTicket {
			budget: QuotaBudget::Rest,
			generation: 7,
			quota_epoch: guard.quota.epoch,
		};
		assert!(guard.quota_spillover_still_needed(now, ticket));
		assert!(!guard.quota_spillover_still_needed(
			now,
			QuotaSpilloverTicket {
				budget: QuotaBudget::Graphql,
				..ticket
			}
		));
		guard.rate_limit_blocked_until = Some(now + Duration::from_secs(60));
		assert_eq!(guard.routing_availability(now, 7, 0), LaneAvailability::UpstreamQuotaCooldown);
		assert_eq!(guard.routing_availability(now, 7, 4), LaneAvailability::Ready);
		guard.graphql_rate_limit_blocked_until = Some(now + Duration::from_secs(30));
		guard.install_oauth_generation(8, false);
		assert_eq!(guard.routing_availability(now, 8, 4), LaneAvailability::UpstreamQuotaCooldown);
		assert!(matches!(guard.graphql_quota.window, QuotaWindow::Known { available: 599, .. }));
		guard.install_oauth_generation(9, true);
		assert!(matches!(guard.quota.window, QuotaWindow::Unknown { .. }));
		assert!(matches!(guard.graphql_quota.window, QuotaWindow::Unknown { .. }));
		assert_eq!(guard.routing_availability(now, 9, 0), LaneAvailability::Ready);
		assert_eq!(guard.routing_availability(now, 9, 4), LaneAvailability::Ready);
		// Late responses from the replaced identity cannot refill either pool.
		let mut old = guard.try_admit_endpoint(now, 9, 4).unwrap();
		guard.install_oauth_generation(10, true);
		guard.reconcile_quota(now, &mut old, Some(1), Some(Duration::from_secs(300)), false);
		old.complete();
		assert!(matches!(guard.graphql_quota.window, QuotaWindow::Unknown { .. }));
	}

	#[test]
	fn test_graphql_comments_schema_error_is_terminal_for_transport_recovery() {
		let request = CommentsRequest::from_path("/comments/abc.json").unwrap();
		let error = request
			.finish(Value::Null, &serde_json::json!({"data":{"postInfoById":{"id":"t3_abc","commentForest":null}}}))
			.unwrap_err();
		assert_eq!(error.reason, "missing_comment_forest");
		assert!(matches!(
			graphql_api_error(error, RedditLane::Direct, 0, &wreq_header::HeaderMap::new()),
			ApiRequestError::Terminal { session_unavailable: false, .. }
		));
	}

	#[test]
	#[sealed_test(env = [("REDLIB_TOR_FALLBACK", "off"), ("REDLIB_REDDIT_PRIMARY_LANE", "direct")])]
	fn test_graphql_comments_pair_keeps_probe_and_reserves_each_send() {
		tokio::runtime::Runtime::new().unwrap().block_on(async {
			use hyper::service::{make_service_fn, service_fn};
			use std::convert::Infallible;
			let mode = Arc::new(AtomicUsize::new(0));
			let calls = Arc::new(Mutex::new(Vec::<String>::new()));
			let test_mode = mode.clone();
			let test_calls = calls.clone();
			let endpoint = endpoint_class_index("/comments/abc.json");
			let service = make_service_fn(move |_| {
				let mode = test_mode.clone();
				let calls = test_calls.clone();
				async move {
					Ok::<_, Infallible>(service_fn(move |request: HyperRequest<Body>| {
						let mode = mode.clone();
						let calls = calls.clone();
						async move {
							assert_eq!(request.method(), hyper::Method::POST);
							assert_eq!(request.headers()["authorization"], "Bearer local-test-fixture");
							let body = hyper::body::to_bytes(request.into_body()).await.unwrap();
							let payload: Value = serde_json::from_slice(&body).unwrap();
							let name = payload["operationName"].as_str().unwrap().to_string();
							calls.lock().unwrap().push(name.clone());
							assert!(
								matches!(upstream_guard(RedditLane::Direct).edges[endpoint].state, EdgeCircuitState::HalfOpen { .. }),
								"metadata must not prematurely recover the comments probe"
							);
							let (remaining, body) = if name == "PostsByIds" {
								(
									if mode.load(Ordering::SeqCst) == 0 { "100" } else { "0" },
									serde_json::json!({"data":{"postsInfoByIds":[{"__typename":"SubredditPost","id":"t3_abc","postTitle":"Fixture","subreddit":{"type":"PUBLIC"}}]}}),
								)
							} else {
								assert_eq!(name, "PostComments");
								(
									"99",
									serde_json::json!({"data":{"postInfoById":{"id":"t3_abc","commentForest":{"trees":[{"parentId":null,"node":{"id":"t1_reply","content":{"markdown":"hello"}}}]}}}}),
								)
							};
							Ok::<_, Infallible>(
								HyperResponse::builder()
									.header("content-type", "application/json")
									.header("x-ratelimit-remaining", remaining)
									.header("x-ratelimit-reset", "60")
									.body(Body::from(body.to_string()))
									.unwrap(),
							)
						}
					}))
				}
			});
			let server = hyper::Server::bind(&([127, 0, 0, 1], 0).into()).serve(service);
			let address = server.local_addr();
			let origin = RedditOrigin {
				base: Box::leak(format!("http://{address}").into_boxed_str()),
				host: Box::leak(address.to_string().into_boxed_str()),
			};
			let server = tokio::spawn(server);
			let http = Arc::new(WreqClient::builder().no_proxy().build().unwrap());
			for scenario in 0..2 {
				mode.store(scenario, Ordering::SeqCst);
				calls.lock().unwrap().clear();
				*upstream_guard(RedditLane::Direct) = UpstreamGuard::new(RedditLane::Direct);
				assert!(install_oauth_client(Oauth::request_fixture(RedditLane::Direct, http.clone()), true, None));
				upstream_guard(RedditLane::Direct).edges[endpoint].state = EdgeCircuitState::Open {
					until: Instant::now() - Duration::from_millis(1),
				};
				let mut refresh = TorQuotaRefreshRetryState::default();
				let (oauth, mut attempt, _permit, _probe) = begin_upstream_attempt(RedditLane::Direct, endpoint, AdmissionRetryMode::Bounded, &mut refresh)
					.await
					.unwrap();
				let result = reddit_comments(
					"/comments/abc.json",
					false,
					oauth,
					&mut attempt,
					false,
					CommentsRequest::from_path("/comments/abc.json").unwrap(),
					origin,
				)
				.await;
				if scenario == 0 {
					let RedditReadResponse::Comments {
						response,
						post: Some(post),
						request,
					} = result.unwrap()
					else {
						panic!("comments response")
					};
					let response = response.json::<Value>().await.unwrap();
					assert_eq!(request.finish(post, &response).unwrap()[1]["data"]["children"][0]["data"]["body"], "hello");
					assert_eq!(*calls.lock().unwrap(), ["PostsByIds", "PostComments"]);
					assert_eq!(upstream_guard(RedditLane::Direct).graphql_quota.next_request_id, 2);
					assert_eq!(upstream_guard(RedditLane::Direct).quota.next_request_id, 0);
					reconcile_rate_limit(&mut attempt, Some(99), Some(Duration::from_secs(60)), false);
					record_upstream_success(&mut attempt, "/comments/abc.json");
					assert!(matches!(upstream_guard(RedditLane::Direct).edges[endpoint].state, EdgeCircuitState::Closed));
				} else {
					assert!(matches!(
						result,
						Err(ApiRequestError::Deferred {
							recovery: LaneRecoveryReason::QuotaCooldown(_),
							..
						})
					));
					assert_eq!(*calls.lock().unwrap(), ["PostsByIds"], "do not send the second read after its admission is denied");
				}
			}
			server.abort();
		});
	}

	#[test]
	fn test_tor_api_transport_path_preserves_logical_routes_and_queries() {
		for (logical, wire) in [
			("/r/all/hot.json?sort=hot&t=&after=t3_1xay0pv", "/r/all/hot?sort=hot&t=&after=t3_1xay0pv"),
			("/search.json?q=example.json&sort=new", "/search?q=example.json&sort=new"),
			("/subreddits/search.json?q=rust", "/subreddits/search?q=rust"),
			("/user/example/about.json", "/user/example/about"),
			("/r/example/wiki/index.json?", "/r/example/wiki/index?"),
			("/r/example.json/hot.json", "/r/example.json/hot"),
			("/.json?raw_json=1", "/?raw_json=1"),
		] {
			assert_eq!(api_transport_path(logical, RedditLane::Tor), wire);
			assert_eq!(api_transport_path(logical, RedditLane::Direct), logical);
			assert_eq!(api_transport_path(wire, RedditLane::Tor), wire);
		}
		for path in [
			"/api/morechildren?children=a,b",
			"/r/all/hot?after=t3_abc.json",
			"/r/example.json/hot",
			"/r/example/about.json/",
			"/r/example/.rss",
		] {
			assert!(matches!(api_transport_path(path, RedditLane::Tor), Cow::Borrowed(_)), "{path}");
		}
	}

	#[test]
	fn test_tor_api_transport_path_keeps_comments_encoding_and_context() {
		for (logical, wire) in [
			("/r/codex/comments/1xaomu1.json?sort=new&limit=500", "/r/codex%2Fcomments/1xaomu1?sort=new&limit=500"),
			("/comments/1xaomu1/_/phvobrm.json?context=3", "/comments%2F1xaomu1/_/phvobrm?context=3"),
		] {
			assert_eq!(comments_transport_path(&api_transport_path(logical, RedditLane::Tor)), wire);
			assert_eq!(comments_transport_path(&api_transport_path(logical, RedditLane::Direct)), comments_transport_path(logical));
		}
	}

	#[test]
	fn test_comments_transport_path_scoped_and_short_threads() {
		for (path, expected) in [
			(
				"/r/codex/comments/1xaomu1.json?raw_json=1&limit=500",
				"/r/codex%2Fcomments/1xaomu1.json?raw_json=1&limit=500",
			),
			(
				"/r/codex/comments/1xaomu1/why_is_my_dot_mad_at_me/.json?sort=new",
				"/r/codex%2Fcomments/1xaomu1/why_is_my_dot_mad_at_me/.json?sort=new",
			),
			("/r/codex/comments/1xaomu1/_/phvobrm.json?context=3", "/r/codex%2Fcomments/1xaomu1/_/phvobrm.json?context=3"),
			("/comments/1xaomu1.json?raw_json=1", "/comments%2F1xaomu1.json?raw_json=1"),
			("/comments/1xaomu1/_/phvobrm.json?context=3", "/comments%2F1xaomu1/_/phvobrm.json?context=3"),
			("/r/comments/comments/abc.json", "/r/comments%2Fcomments/abc.json"),
		] {
			assert_eq!(comments_transport_path(path), expected);
			assert_eq!(comments_transport_path(expected), expected, "encoding must be idempotent");
		}
	}

	#[test]
	fn test_comments_transport_path_preserves_other_routes_and_queries() {
		for path in [
			"/r/all/hot.json?after=t3_1xay0pv",
			"/r/codex/comments.json",
			"/r/codex/commentsXYZ/abc.json",
			"/r/codex/wiki/comments/abc.json",
			"/r//comments/abc.json",
			"/comments/.json",
			"/comments.json",
			"/user/example/comments.json",
			"/user/example/comments/abc.json",
			"/search.json?q=/r/codex/comments/abc",
			"https://example.com/r/codex/comments/abc.json",
			"//example.com/r/codex/comments/abc.json",
		] {
			assert!(matches!(comments_transport_path(path), Cow::Borrowed(_)), "{path}");
		}
		let path = "/r/codex/comments/abc.json?sort=new&context=3&q=a%2Bb&empty=&sort=top";
		assert_eq!(comments_transport_path(path), "/r/codex%2Fcomments/abc.json?sort=new&context=3&q=a%2Bb&empty=&sort=top");
	}

	#[test]
	fn test_upstream_comments_redirect_preserves_query_and_endpoint_controls() {
		let encoded = "/r/rust%2Fcomments/18t5968/title/kfbqlbc.json?context=3";
		for lane in [RedditLane::Direct, RedditLane::Tor] {
			let logical = validated_reddit_redirect_path(encoded, lane).unwrap();
			assert_eq!(logical, "/r/rust/comments/18t5968/_/kfbqlbc.json?context=3&raw_json=1");
			assert_eq!(json_cache_policy(&logical), JsonCachePolicy::Comments);
			assert_eq!(endpoint_class(&logical), "comments");
		}
		let query = "sort=new&literal=/comments%2Fabc&other=/r/rust%2fcomments/id";
		assert_eq!(comments_logical_path(&format!("/comments%2Fabc.json?{query}")), format!("/comments/abc.json?{query}"));
		for unchanged in [
			"/search.json?q=/comments%2Fabc",
			"/r/rust%2Fwiki/page",
			"https://example.com/comments%2Fabc.json",
			"/r//rust%2Fcomments/abc.json",
		] {
			assert_eq!(comments_logical_path(unchanged), unchanged);
		}
	}

	#[test]
	fn test_reddit_dns_name_uses_fastly_dualstack_alias() {
		assert_eq!(reddit_dns_name("www.reddit.com"), ALTERNATIVE_REDDIT_DNS_HOST);
		assert_eq!(reddit_dns_name("WWW.REDDIT.COM"), ALTERNATIVE_REDDIT_DNS_HOST);
		assert_eq!(reddit_dns_name("oauth.reddit.com"), "oauth.reddit.com");
	}

	#[test]
	fn test_comments_logical_path_removes_transport_encoding() {
		for (path, expected) in [
			("/r/rust%2Fcomments/18t5968/title/kfbqlbc/", "/r/rust/comments/18t5968/title/kfbqlbc/"),
			("/r/rust%2fcomments/18t5968/title/kfbqlbc/", "/r/rust/comments/18t5968/title/kfbqlbc/"),
			("/comments%2F18t5968/title/", "/comments/18t5968/title/"),
			("/r/rust%2Fwiki/page", "/r/rust%2Fwiki/page"),
		] {
			assert_eq!(comments_logical_path(path), expected);
		}
	}

	const POPULAR_URL: &str = "/r/popular/hot.json?&raw_json=1&geo_filter=GLOBAL";
	static COALESCED_TEST_CALLS: AtomicUsize = AtomicUsize::new(0);
	static COALESCED_RESULT_TEST_CALLS: AtomicUsize = AtomicUsize::new(0);
	static COALESCED_PANIC_TEST_CALLS: AtomicUsize = AtomicUsize::new(0);
	static COALESCED_RESULT_TEST_FLIGHTS: LazyLock<JsonFlightMap> = LazyLock::new(|| AsyncMutex::new(HashMap::new()));
	static COALESCED_PANIC_TEST_FLIGHTS: LazyLock<JsonFlightMap> = LazyLock::new(|| AsyncMutex::new(HashMap::new()));

	#[cached(size = 8, time = 30, sync_writes = "by_key")]
	async fn coalesced_test_fetch(key: u8) -> u8 {
		COALESCED_TEST_CALLS.fetch_add(1, Ordering::SeqCst);
		tokio::time::sleep(Duration::from_millis(50)).await;
		key
	}

	async fn coalesced_result_test_fetch(key: u8) -> JsonRequestResult {
		coalesce_json_request(&COALESCED_RESULT_TEST_FLIGHTS, (format!("test-{key}"), false), move || async move {
			let call = COALESCED_RESULT_TEST_CALLS.fetch_add(1, Ordering::SeqCst);
			tokio::time::sleep(Duration::from_millis(50)).await;
			if call == 0 {
				Err("transient".to_string())
			} else {
				Ok(Value::from(key))
			}
		})
		.await
	}

	async fn coalesced_panic_test_fetch(key: u8, panic_on_first: bool) -> JsonRequestResult {
		coalesce_json_request(&COALESCED_PANIC_TEST_FLIGHTS, (format!("panic-test-{key}"), false), move || async move {
			let call = COALESCED_PANIC_TEST_CALLS.fetch_add(1, Ordering::SeqCst);
			tokio::time::sleep(Duration::from_millis(50)).await;
			assert!(!panic_on_first || call != 0, "simulated fetch panic");
			Ok(Value::from(key))
		})
		.await
	}

	#[tokio::test]
	async fn test_identical_cache_misses_are_coalesced() {
		COALESCED_TEST_CALLS.store(0, Ordering::SeqCst);
		let (first, second, third) = tokio::join!(coalesced_test_fetch(42), coalesced_test_fetch(42), coalesced_test_fetch(42));
		assert_eq!((first, second, third), (42, 42, 42));
		assert_eq!(COALESCED_TEST_CALLS.load(Ordering::SeqCst), 1);
	}

	#[tokio::test]
	async fn test_transient_coalesced_errors_are_not_cached() {
		COALESCED_RESULT_TEST_CALLS.store(0, Ordering::SeqCst);
		let (first, second, third) = tokio::join!(coalesced_result_test_fetch(43), coalesced_result_test_fetch(43), coalesced_result_test_fetch(43));
		let expected_error = Err("transient".to_string());
		assert_eq!(first, expected_error);
		assert_eq!(second, expected_error);
		assert_eq!(third, expected_error);
		assert_eq!(COALESCED_RESULT_TEST_CALLS.load(Ordering::SeqCst), 1);

		assert_eq!(coalesced_result_test_fetch(43).await, Ok(Value::from(43)));
		assert_eq!(COALESCED_RESULT_TEST_CALLS.load(Ordering::SeqCst), 2);
	}

	#[tokio::test]
	async fn test_panicked_coalesced_request_is_removed_and_retryable() {
		COALESCED_PANIC_TEST_CALLS.store(0, Ordering::SeqCst);
		let (first, second) = tokio::join!(coalesced_panic_test_fetch(44, true), coalesced_panic_test_fetch(44, true));
		let expected_error = Err(JSON_FLIGHT_ABORTED_ERROR.to_string());
		assert_eq!(first, expected_error);
		assert_eq!(second, expected_error);
		assert_eq!(COALESCED_PANIC_TEST_CALLS.load(Ordering::SeqCst), 1);

		assert_eq!(coalesced_panic_test_fetch(44, false).await, Ok(Value::from(44)));
		assert_eq!(COALESCED_PANIC_TEST_CALLS.load(Ordering::SeqCst), 2);
	}

	#[tokio::test]
	async fn test_quota_notifications_do_not_consume_timed_retries() {
		let notify = Notify::new();
		let mut retry_count = 0;
		for _ in 0..5 {
			notify.notify_one();
			let timer_elapsed = wait_for_local_quota_retry(notify.notified(), Duration::from_secs(1)).await;
			assert!(!timer_elapsed);
			retry_count = local_quota_retry_count_after_wait(retry_count, timer_elapsed);
		}
		assert_eq!(retry_count, 0);

		let timer_elapsed = wait_for_local_quota_retry(notify.notified(), Duration::from_millis(1)).await;
		assert!(timer_elapsed);
		assert_eq!(local_quota_retry_count_after_wait(retry_count, timer_elapsed), 1);
	}

	#[test]
	fn tor_identity_uses_socks_auth_isolation() {
		let isolated = tor_isolation_proxy_url("socks5h://tor:9050", "identity-17").unwrap();
		let parsed = url::Url::parse(&isolated).unwrap();
		assert_eq!(parsed.scheme(), "socks5h");
		assert_eq!(parsed.host_str(), Some("tor"));
		assert_eq!(parsed.port(), Some(9050));
		assert_eq!(parsed.username(), "identity-17");
		assert_eq!(parsed.password(), Some("identity-17"));
	}

	#[test]
	#[sealed_test(env = [("REDLIB_TOR_FALLBACK", "off")])]
	fn test_tor_first_refresh_backoff_returns_without_another_recovery_wait() {
		tokio::runtime::Runtime::new().unwrap().block_on(async {
			let lane = RedditLane::Tor;
			let client = Arc::new(WreqClient::builder().no_proxy().build().unwrap());
			install_oauth_client(Oauth::request_fixture(lane, client), true, None);
			assert!(upstream_guard(lane).reject_oauth_generation(0));
			crate::oauth::set_refresh_backoff_for_test(lane, Some(Duration::from_secs(120)));
			let denied = begin_upstream_attempt_once(lane, false, 0).err().expect("rejected credentials must not be admitted");
			assert_eq!(denied.message, "OAuth token refresh is temporarily unavailable. Retry in 120 seconds");
			let result = tokio::time::timeout(Duration::from_millis(100), wait_for_tor_recovery("/r/rust/hot.json"))
				.await
				.expect("backoff must not wait thirty seconds");
			assert_eq!(result, Err(denied.message.clone()));
			assert_eq!(tor_recovery_wait_message("/r/rust/hot.json"), denied.message);

			// A failure wakes existing waiters; it must not leave them polling
			// a two-second message throughout the six-minute retry pause.
			crate::oauth::set_refresh_backoff_for_test(lane, None);
			let waiter = tokio::spawn(wait_for_tor_recovery("/r/rust/hot.json"));
			tokio::task::yield_now().await;
			crate::oauth::set_refresh_backoff_for_test(lane, Some(Duration::from_secs(300)));
			token_refresh_notify(lane).notify_waiters();
			let result = tokio::time::timeout(Duration::from_millis(100), waiter).await.expect("failure must wake recovery").unwrap();
			assert_eq!(result, Err("OAuth token refresh is temporarily unavailable. Retry in 300 seconds".into()));

			crate::oauth::set_refresh_backoff_for_test(lane, None);
			let waiter = tokio::spawn(wait_for_tor_recovery("/r/rust/hot.json"));
			tokio::task::yield_now().await;
			let mut replacement = Oauth::request_fixture(lane, Arc::new(WreqClient::builder().no_proxy().build().unwrap()));
			replacement.generation = 1;
			install_oauth_client(replacement, false, None);
			assert_eq!(tokio::time::timeout(Duration::from_millis(100), waiter).await.unwrap().unwrap(), Ok(()));
			assert_eq!(session_refresh_wait_message(None), "Refreshing the anonymous Reddit session. Retry in 2 seconds");
		});
	}

	#[tokio::test]
	async fn test_tor_first_failed_direct_standby_always_returns_to_tor() {
		let calls = std::sync::Mutex::new(Vec::new());
		let direct = async {
			calls.lock().unwrap().push("direct");
			Err("direct failed".to_string())
		};
		let result = recover_after_direct_standby(direct, || async {
			calls.lock().unwrap().push("tor");
			Ok::<_, String>("recovered")
		})
		.await;
		assert_eq!(result, Ok("recovered"));
		assert_eq!(*calls.lock().unwrap(), vec!["direct", "tor"]);
		let result = recover_after_direct_standby(async { Ok::<_, String>("direct success") }, || async { panic!("successful standby must not retry Tor") }).await;
		assert_eq!(result, Ok("direct success"));
	}

	#[test]
	fn test_tor_first_stalled_send_uses_ready_standby_before_thirty_seconds() {
		assert_eq!(
			initial_lane_request_timeout(RedditLane::Tor, RedditLane::Tor, LaneAvailability::Ready),
			Duration::from_secs(8)
		);
		for direct in [
			LaneAvailability::Unavailable,
			LaneAvailability::TransportCooldown,
			LaneAvailability::LocalQuotaPause,
			LaneAvailability::UpstreamQuotaCooldown,
		] {
			assert_eq!(initial_lane_request_timeout(RedditLane::Tor, RedditLane::Tor, direct), Duration::from_secs(30));
		}
		assert_eq!(
			initial_lane_request_timeout(RedditLane::Direct, RedditLane::Tor, LaneAvailability::Ready),
			Duration::from_secs(30)
		);
		assert_eq!(
			initial_lane_request_timeout(RedditLane::Tor, RedditLane::Direct, LaneAvailability::Ready),
			Duration::from_secs(30)
		);
		assert_eq!(TRANSPORT_RETRY_TIMEOUT, Duration::from_secs(15));
		assert_eq!(TOR_STANDBY_RECOVERY_TIMEOUT, Duration::from_secs(30));
	}

	#[tokio::test]
	async fn test_tor_first_final_failure_uses_the_latest_tor_result() {
		let result = recover_after_direct_standby(async { Err::<(), _>("direct unavailable".to_string()) }, || async {
			Err("Reddit is temporarily rejecting this instance. Retry in 42 seconds".to_string())
		})
		.await;
		assert_eq!(result, Err("Reddit is temporarily rejecting this instance. Retry in 42 seconds".to_string()));
	}

	#[test]
	fn test_direct_transport_cooldown_uses_ready_tor() {
		let now = Instant::now();
		let mut direct = UpstreamGuard::new(RedditLane::Direct);
		let mut tor = UpstreamGuard::new(RedditLane::Tor);
		direct.install_oauth_generation(7, true);
		tor.install_oauth_generation(7, true);
		for _ in 0..FAILURE_THRESHOLD {
			direct.record_failure(now);
		}
		let direct_state = direct.routing_availability(now, 7, 0);
		let tor_state = tor.routing_availability(now, 7, 0);
		assert_eq!(direct_state, LaneAvailability::TransportCooldown);
		assert_eq!(tor_state, LaneAvailability::Ready);
		assert_eq!(select_api_lane(RedditLane::Direct, direct_state, tor_state), RedditLane::Tor);
		assert_eq!(select_api_lane(RedditLane::Direct, LaneAvailability::Ready, tor_state), RedditLane::Direct);
		for tor_state in [
			LaneAvailability::Unavailable,
			LaneAvailability::TransportCooldown,
			LaneAvailability::LocalQuotaPause,
			LaneAvailability::UpstreamQuotaCooldown,
		] {
			assert_eq!(select_api_lane(RedditLane::Direct, direct_state, tor_state), RedditLane::Direct);
		}
		let recovered_at = now + Duration::from_secs(60);
		assert_eq!(direct.routing_availability(recovered_at, 7, 0), LaneAvailability::Ready);
		assert_eq!(
			select_api_lane(RedditLane::Direct, direct.routing_availability(recovered_at, 7, 0), tor_state),
			RedditLane::Direct
		);
		direct.block_for_rate_limit(now, Duration::from_secs(30));
		assert_eq!(
			select_api_lane(RedditLane::Direct, direct.routing_availability(now, 7, 0), LaneAvailability::Ready),
			RedditLane::Direct
		);
	}

	#[test]
	fn test_direct_transport_cooldown_handoff_requires_ready_tor_and_active_cooldown() {
		for direct in [
			LaneAvailability::Ready,
			LaneAvailability::Unavailable,
			LaneAvailability::TransportCooldown,
			LaneAvailability::LocalQuotaPause,
			LaneAvailability::UpstreamQuotaCooldown,
		] {
			for tor_ready in [false, true] {
				for lane in [RedditLane::Direct, RedditLane::Tor] {
					assert_eq!(
						should_retry_on_tor(lane, TorRetryReason::TransportUnavailable, tor_ready, false, direct, false, false),
						lane == RedditLane::Direct && tor_ready && direct == LaneAvailability::TransportCooldown
					);
				}
			}
		}
		for reason in [TorRetryReason::None, TorRetryReason::SessionUnavailable] {
			assert!(!should_retry_on_tor(
				RedditLane::Direct,
				reason,
				true,
				false,
				LaneAvailability::TransportCooldown,
				false,
				false
			));
		}
	}

	#[test]
	fn test_tor_first_routing_respects_availability_and_upstream_quota() {
		use LaneAvailability::*;
		for (direct, tor, expected) in [
			(Ready, Ready, RedditLane::Tor),
			(Ready, Unavailable, RedditLane::Direct),
			(Ready, TransportCooldown, RedditLane::Direct),
			(Ready, LocalQuotaPause, RedditLane::Direct),
			(Ready, UpstreamQuotaCooldown, RedditLane::Tor),
			(Unavailable, Unavailable, RedditLane::Tor),
			(UpstreamQuotaCooldown, Unavailable, RedditLane::Tor),
			(LocalQuotaPause, Unavailable, RedditLane::Tor),
			(Unavailable, Ready, RedditLane::Tor),
		] {
			assert_eq!(select_api_lane(RedditLane::Tor, direct, tor), expected);
		}
		assert_eq!(select_api_lane(RedditLane::Direct, LocalQuotaPause, Ready), RedditLane::Direct);
		assert_eq!(select_api_lane(RedditLane::Direct, UpstreamQuotaCooldown, Ready), RedditLane::Direct);
		assert_eq!(select_api_lane(RedditLane::Direct, TransportCooldown, Ready), RedditLane::Tor);
	}

	#[test]
	fn test_credential_rejection_refresh_spacing_survives_token_handoff() {
		let mut guard = UpstreamGuard::default();
		let now = Instant::now();
		assert_eq!(guard.rejected_session_refresh_delay(now), Duration::ZERO);
		guard.install_oauth_generation(1, false);
		assert_eq!(guard.rejected_session_refresh_delay(now), REJECTED_SESSION_REFRESH_INTERVAL);
		assert_eq!(guard.rejected_session_refresh_delay(now + REJECTED_SESSION_REFRESH_INTERVAL), Duration::ZERO);
	}

	#[test]
	fn test_tor_first_standby_is_narrow_and_never_recursive() {
		for reason in [TorRetryReason::EdgeRejected, TorRetryReason::TransportUnavailable, TorRetryReason::SessionUnavailable] {
			assert!(should_use_direct_standby(RedditLane::Tor, RedditLane::Tor, reason, LaneAvailability::Ready));
			assert!(!should_use_direct_standby(RedditLane::Direct, RedditLane::Tor, reason, LaneAvailability::Ready));
			assert!(!should_use_direct_standby(RedditLane::Tor, RedditLane::Direct, reason, LaneAvailability::Ready));
			assert!(!should_use_direct_standby(
				RedditLane::Tor,
				RedditLane::Tor,
				reason,
				LaneAvailability::UpstreamQuotaCooldown
			));
		}
		assert!(!should_use_direct_standby(RedditLane::Tor, RedditLane::Tor, TorRetryReason::None, LaneAvailability::Ready));
	}

	#[test]
	fn test_endpoint_edge_recovery_requires_the_failed_class() {
		let now = Instant::now();
		let comments = endpoint_class_index("/r/example/comments/abc.json");
		let listing = endpoint_class_index("/r/example/hot.json");
		let mut guard = UpstreamGuard::new(RedditLane::Tor);
		let rejected = guard.begin_endpoint_attempt(now, comments).unwrap();
		let denial = guard.record_edge_throttle(now, rejected, Some(Duration::ZERO));
		assert_eq!(guard.routing_availability(now, 0, comments), LaneAvailability::Unavailable);
		assert_eq!(guard.routing_availability(now, 0, listing), LaneAvailability::Ready);
		let probe_at = now + denial.delay + Duration::from_millis(1);
		let listing_attempt = guard.begin_endpoint_attempt(probe_at, listing).unwrap();
		assert!(!listing_attempt.half_open);
		assert!(guard.record_api_success(probe_at, listing_attempt).is_none());
		assert_eq!(guard.edges[comments].failures, 1);
		let comment_probe = guard.begin_endpoint_attempt(probe_at, comments).unwrap();
		assert!(comment_probe.half_open);
		assert!(guard.begin_endpoint_attempt(probe_at, comments).is_err());
		assert!(guard.record_api_success(probe_at, comment_probe).is_some());
		assert_eq!(guard.edges[comments].failures, 0);
	}

	#[test]
	fn test_endpoint_edge_episodes_and_shared_quota_remain_independent() {
		let now = Instant::now();
		let mut guard = UpstreamGuard::default();
		let comments = guard.begin_endpoint_attempt(now, 4).unwrap();
		let listing = guard.begin_endpoint_attempt(now, 0).unwrap();
		let comment_denial = guard.record_edge_throttle(now, comments, None);
		let listing_denial = guard.record_edge_throttle(now, listing, None);
		assert_eq!(guard.edges[4].failures, 1);
		assert_eq!(guard.edges[0].failures, 1);
		let probe_at = now + comment_denial.delay.max(listing_denial.delay) + Duration::from_millis(1);
		let listing_probe = guard.begin_endpoint_attempt(probe_at, 0).unwrap();
		assert!(guard.record_api_success(probe_at, listing_probe).is_some());
		assert_eq!(guard.edges[4].failures, 1);
		assert!(matches!(guard.edges[4].state, EdgeCircuitState::Open { .. }));
		guard.block_for_rate_limit(now, Duration::from_secs(60));
		for endpoint in (0..7).filter(|endpoint| *endpoint != 4) {
			assert_eq!(guard.routing_availability(now, 0, endpoint), LaneAvailability::UpstreamQuotaCooldown);
			assert_eq!(guard.begin_endpoint_attempt(now, endpoint).unwrap_err().1, CooldownReason::RateLimit);
		}
		assert_eq!(guard.routing_availability(now, 0, 4), LaneAvailability::Unavailable);
		assert_eq!(guard.begin_endpoint_attempt(now, 4).unwrap_err().1, CooldownReason::EdgeThrottle);
	}

	#[test]
	fn test_credential_rejection_is_explicit_and_generation_scoped() {
		assert!(credential_rejected(401, None));
		assert!(credential_rejected(403, Some("Bearer error=\"invalid_token\"")));
		assert!(!credential_rejected(403, None));
		assert!(!credential_rejected(403, Some("Bearer error=\"insufficient_scope\"")));
		assert!(!credential_rejected(429, Some("Bearer error=\"invalid_token\"")));
		let now = Instant::now();
		let mut guard = UpstreamGuard::default();
		guard.install_oauth_generation(7, true);
		assert!(!guard.reject_oauth_generation(6));
		assert!(guard.reject_oauth_generation(7));
		assert!(!guard.reject_oauth_generation(7));
		assert_eq!(guard.routing_availability(now, 7, 4), LaneAvailability::Unavailable);
		guard.install_oauth_generation(8, false);
		assert_eq!(guard.routing_availability(now, 8, 4), LaneAvailability::Ready);
		assert_eq!(guard.routing_availability(now, 7, 4), LaneAvailability::Unavailable);
		guard.upstream_failure_blocked_until = Some(now + Duration::from_secs(10));
		assert_eq!(guard.routing_availability(now, 8, 4), LaneAvailability::TransportCooldown);
	}

	#[test]
	fn test_rejection_details_preserve_resource_errors_without_logging_payloads() {
		for reason in ["private", "banned", "quarantined", "gated"] {
			let body = serde_json::json!({"error":403, "reason":reason, "message":"Forbidden"}).to_string();
			assert_eq!(rejection_details(body.as_bytes()).resource_reason, Some(reason));
		}
		for body in [
			br#"{"error":403,"reason":null,"message":"Forbidden"}"#.as_slice(),
			br#"{"error":403,"reason":"arbitrary private payload"}"#.as_slice(),
			b"<html>blocked</html>",
		] {
			assert_eq!(rejection_details(body).resource_reason, None);
			assert!(!format!("{:?}", rejection_details(body)).contains("payload"));
		}
	}

	#[tokio::test]
	async fn test_rejection_body_read_is_bounded() {
		let mut body = Body::from(vec![b'x'; MAX_REJECTION_BODY_BYTES + 1]);
		assert_eq!(read_rejection_details(&mut body).await.body_kind, "oversized");
		let mut body = Body::from(r#"{"error":403,"reason":"private"}"#);
		assert_eq!(read_rejection_details(&mut body).await.resource_reason, Some("private"));
	}

	#[test]
	fn test_parse_max_concurrency() {
		assert_eq!(parse_max_concurrency(None), DEFAULT_MAX_CONCURRENT_API_REQUESTS);
		assert_eq!(parse_max_concurrency(Some("invalid")), DEFAULT_MAX_CONCURRENT_API_REQUESTS);
		assert_eq!(parse_max_concurrency(Some("0")), 1);
		assert_eq!(parse_max_concurrency(Some("12")), 12);
		assert_eq!(parse_max_concurrency(Some("1000")), MAX_CONFIGURED_API_REQUESTS);
	}

	#[test]
	fn test_parse_delay_seconds() {
		assert_eq!(parse_delay_seconds(Some("1.5")), Some(Duration::from_millis(1500)));
		assert_eq!(parse_delay_seconds(Some("9999")), Some(MAX_RATE_LIMIT_COOLDOWN));
		assert_eq!(parse_delay_seconds(Some("-1")), None);
		assert_eq!(parse_delay_seconds(Some("not-a-number")), None);
		assert_eq!(parse_delay_seconds(None), None);
	}

	#[test]
	fn test_parse_rate_limit_count_rejects_invalid_values() {
		assert_eq!(parse_rate_limit_count(Some("9.4")), Some(9));
		assert_eq!(parse_rate_limit_count(Some("9.6")), Some(9));
		assert_eq!(parse_rate_limit_count(Some("-1")), None);
		assert_eq!(parse_rate_limit_count(Some("NaN")), None);
		assert_eq!(parse_rate_limit_count(Some("not-a-number")), None);
	}

	#[test]
	fn test_rate_limit_delay_adds_margin_and_respects_cap() {
		assert_eq!(rate_limit_base_delay(Some("0"), None), (Duration::from_secs(12), false));
		assert_eq!(rate_limit_base_delay(Some("1"), None), (Duration::from_secs(3), true));
		assert_eq!(rate_limit_base_delay(None, Some("20")), (Duration::from_secs(22), true));
		assert_eq!(rate_limit_base_delay(Some("0"), Some("120")), (Duration::from_secs(122), true));
		assert_eq!(rate_limit_base_delay(Some("9999"), None), (MAX_RATE_LIMIT_COOLDOWN, true));
		assert_eq!(parse_delay_seconds(Some("1e300")), Some(MAX_RATE_LIMIT_COOLDOWN));
		assert_eq!(rate_limit_base_delay(None, None), (Duration::from_secs(12), false));
		let delay = rate_limit_delay(Some("1"), None);
		assert!((Duration::from_secs(3)..=Duration::from_secs(5)).contains(&delay));
	}

	#[test]
	fn test_classify_throttle_response_separates_edge_denials() {
		assert_eq!(classify_throttle_response(403, true, false), Some(ThrottleKind::Edge));
		assert_eq!(classify_throttle_response(403, true, true), Some(ThrottleKind::Quota));
		assert_eq!(classify_throttle_response(429, false, false), Some(ThrottleKind::Quota));
		assert_eq!(classify_throttle_response(403, false, false), None);
	}

	#[test]
	fn test_edge_throttle_delay_escalates_and_caps() {
		assert_eq!(edge_throttle_base_delay(1, None), (Duration::from_secs(5), false));
		assert_eq!(edge_throttle_base_delay(2, None), (Duration::from_secs(10), false));
		assert_eq!(edge_throttle_base_delay(3, None), (Duration::from_secs(20), false));
		assert_eq!(edge_throttle_base_delay(4, None), (Duration::from_secs(40), false));
		assert_eq!(edge_throttle_base_delay(5, None), (Duration::from_secs(80), false));
		assert_eq!(edge_throttle_base_delay(6, None), (Duration::from_secs(160), false));
		assert_eq!(edge_throttle_base_delay(8, None), (EDGE_THROTTLE_MAX_COOLDOWN, false));
		assert_eq!(edge_throttle_base_delay(1, Some(Duration::from_secs(90))), (Duration::from_secs(92), true));
		assert_eq!(edge_throttle_base_delay(8, Some(Duration::from_secs(500))), (Duration::from_secs(502), true));
		let delay = edge_throttle_delay(1, None);
		assert!((Duration::from_secs(5)..=Duration::from_millis(6250)).contains(&delay));
		let saturated = edge_throttle_delay(8, None);
		assert!((EDGE_THROTTLE_MAX_COOLDOWN..=Duration::from_secs(375)).contains(&saturated));
	}

	#[test]
	fn test_retry_after_seconds_rounds_up() {
		assert_eq!(retry_after_seconds(Duration::from_millis(1)), 1);
		assert_eq!(retry_after_seconds(Duration::from_secs(2)), 2);
		assert_eq!(retry_after_seconds(Duration::from_millis(2001)), 3);
	}

	#[test]
	fn test_media_diagnostics_group_destinations_and_results() {
		assert_eq!(media_destination_index("https://v.redd.it/{id}/DASH_{size}"), 0);
		assert_eq!(media_destination_index("https://i.redd.it/{path}"), 1);
		assert_eq!(media_destination_index("https://preview.redd.it/{id}"), 2);
		assert_eq!(media_destination_index("https://emoji.redditmedia.com/{id}/{name}"), 3);
		assert_eq!(media_destination_index("https://media.giphy.com/media/{id}/giphy.gif"), 4);
		assert_eq!(media_destination_index("https://example.com/{path}"), 5);

		assert_eq!(media_result_index(Some(200)), 0);
		assert_eq!(media_result_index(Some(304)), 1);
		assert_eq!(media_result_index(Some(403)), 2);
		assert_eq!(media_result_index(Some(503)), 3);
		assert_eq!(media_result_index(Some(101)), 4);
		assert_eq!(media_result_index(None), 5);
	}

	#[test]
	fn test_quota_admission_never_uses_safety_reserve() {
		let now = Instant::now();
		let mut quota = QuotaGovernor {
			generation: 7,
			epoch: 2,
			next_request_id: 0,
			outstanding: 0,
			rollover_reserve: 0,
			headerless_consumption: 0,
			window: QuotaWindow::Known {
				available: QUOTA_SAFETY_RESERVE + 3,
				reset_at: now + Duration::from_secs(120),
			},
		};
		assert!(quota.reserve(now, 7).is_ok());
		assert!(quota.reserve(now, 7).is_ok());
		assert!(quota.reserve(now, 7).is_ok());
		assert!(matches!(quota.reserve(now, 7), Err(QuotaReserveError::ReserveExhausted(_))));
		assert_eq!(quota.outstanding, 3);
	}

	#[test]
	fn test_quota_admission_uses_remaining_allowance_near_reset() {
		let now = Instant::now();
		let mut quota = QuotaGovernor {
			generation: 7,
			epoch: 2,
			next_request_id: 0,
			outstanding: 0,
			rollover_reserve: 0,
			headerless_consumption: 0,
			window: QuotaWindow::Known {
				available: QUOTA_SAFETY_RESERVE,
				reset_at: now + EMERGENCY_QUOTA_ROTATION_MIN_RESET_REMAINING,
			},
		};
		for _ in 0..QUOTA_SAFETY_RESERVE {
			assert!(quota.reserve(now, 7).is_ok());
		}
		assert!(matches!(quota.reserve(now, 7), Err(QuotaReserveError::ReserveExhausted(_))));
		assert_eq!(quota.outstanding, QUOTA_SAFETY_RESERVE);
	}

	#[test]
	fn test_local_quota_retries_are_attempt_and_time_bounded() {
		let retryable = BeginAttemptDenied {
			admission: Some(AdmissionDenied {
				delay: DEFAULT_RATE_LIMIT_COOLDOWN,
				reason: CooldownReason::RateLimit,
				reserve_exhausted: true,
				local_quota_retry: true,
				source: "quota_reserve",
			}),
			message: "retry".to_string(),
			edge_deferred: false,
			quota_spillover: None,
			tor_quota_refresh: None,
		};
		assert_eq!(
			local_quota_retry_delay(&retryable, 0, Duration::ZERO, AdmissionRetryMode::Bounded, false),
			Some(LOCAL_QUOTA_RETRY_INTERVAL)
		);
		assert_eq!(
			local_quota_retry_delay(&retryable, 2, LOCAL_QUOTA_RETRY_BUDGET - Duration::from_millis(50), AdmissionRetryMode::Bounded, false),
			Some(Duration::from_millis(50))
		);
		assert_eq!(
			local_quota_retry_delay(&retryable, MAX_LOCAL_QUOTA_RETRIES, Duration::ZERO, AdmissionRetryMode::Bounded, false),
			None
		);
		assert_eq!(local_quota_retry_delay(&retryable, 0, LOCAL_QUOTA_RETRY_BUDGET, AdmissionRetryMode::Bounded, false), None);
		assert_eq!(local_quota_retry_delay(&retryable, 0, Duration::ZERO, AdmissionRetryMode::Immediate, false), None);
		assert_eq!(local_quota_retry_count_after_wait(2, false), 2);
		assert_eq!(local_quota_retry_count_after_wait(2, true), 3);

		let not_retryable = BeginAttemptDenied {
			admission: Some(AdmissionDenied {
				local_quota_retry: false,
				..retryable.admission.unwrap()
			}),
			message: "stop".to_string(),
			edge_deferred: true,
			quota_spillover: None,
			tor_quota_refresh: None,
		};
		assert_eq!(local_quota_retry_delay(&not_retryable, 0, Duration::ZERO, AdmissionRetryMode::Bounded, false), None);
		assert_eq!(
			local_admission_message(retryable.admission.unwrap(), true),
			"Refreshing the anonymous Reddit session. Retry in 2 seconds"
		);
	}

	#[test]
	fn test_tor_quota_refresh_wait_is_scoped_and_bounded() {
		let ticket = QuotaRefreshWaitTicket { generation: 7, quota_epoch: 4 };
		let mut denied = BeginAttemptDenied {
			admission: Some(AdmissionDenied {
				delay: Duration::from_secs(60),
				reason: CooldownReason::RateLimit,
				reserve_exhausted: true,
				local_quota_retry: true,
				source: "quota_reserve",
			}),
			message: "refreshing".to_string(),
			edge_deferred: false,
			quota_spillover: None,
			tor_quota_refresh: Some(ticket),
		};
		let mut owner = None;
		assert!(!matching_tor_quota_refresh_wait(RedditLane::Direct, &mut owner, &denied));
		assert_eq!(owner, None);
		assert!(matching_tor_quota_refresh_wait(RedditLane::Tor, &mut owner, &denied));
		assert_eq!(owner, Some(ticket));
		assert_eq!(
			local_quota_retry_delay(&denied, 3, Duration::from_secs(2), AdmissionRetryMode::Bounded, true),
			Some(LOCAL_QUOTA_RETRY_INTERVAL)
		);
		assert_eq!(local_quota_retry_delay(&denied, 3, Duration::from_secs(2), AdmissionRetryMode::Bounded, false), None);
		assert_eq!(
			local_quota_retry_delay(&denied, 15, TOR_QUOTA_REFRESH_RETRY_BUDGET - Duration::from_millis(50), AdmissionRetryMode::Bounded, true),
			Some(Duration::from_millis(50))
		);
		assert_eq!(local_quota_retry_delay(&denied, 0, TOR_QUOTA_REFRESH_RETRY_BUDGET, AdmissionRetryMode::Bounded, true), None);
		assert_eq!(
			local_quota_retry_delay(&denied, MAX_TOR_QUOTA_REFRESH_RETRIES, Duration::ZERO, AdmissionRetryMode::Bounded, true),
			None
		);
		assert_eq!(local_quota_retry_delay(&denied, 0, Duration::ZERO, AdmissionRetryMode::Immediate, true), None);

		// A failed/finished refresh, another generation, or another quota window
		// cannot renew the extended wait. The original owner is never replaced.
		denied.tor_quota_refresh = None;
		assert!(!matching_tor_quota_refresh_wait(RedditLane::Tor, &mut owner, &denied));
		denied.tor_quota_refresh = Some(QuotaRefreshWaitTicket { generation: 8, ..ticket });
		assert!(!matching_tor_quota_refresh_wait(RedditLane::Tor, &mut owner, &denied));
		denied.tor_quota_refresh = Some(QuotaRefreshWaitTicket { quota_epoch: 5, ..ticket });
		assert!(!matching_tor_quota_refresh_wait(RedditLane::Tor, &mut owner, &denied));
		assert_eq!(owner, Some(ticket));

		denied.tor_quota_refresh = Some(ticket);
		denied.admission.as_mut().unwrap().source = "active_cooldown";
		assert!(!matching_tor_quota_refresh_wait(RedditLane::Tor, &mut owner, &denied));
		denied.admission.as_mut().unwrap().source = "stale_generation";
		assert!(!matching_tor_quota_refresh_wait(RedditLane::Tor, &mut owner, &denied));
	}

	#[tokio::test]
	async fn test_tor_quota_refresh_wait_wakes_after_short_budget() {
		let notify = Arc::new(Notify::new());
		let ready = Arc::new(AtomicBool::new(false));
		let ready_after_refresh = ready.clone();
		let notify_after_refresh = notify.clone();
		let completed = tokio::spawn(async move {
			tokio::time::sleep(LOCAL_QUOTA_RETRY_BUDGET + Duration::from_millis(200)).await;
			ready_after_refresh.store(true, Ordering::SeqCst);
			notify_after_refresh.notify_waiters();
		});
		let denied = BeginAttemptDenied {
			admission: Some(AdmissionDenied {
				delay: Duration::from_secs(60),
				reason: CooldownReason::RateLimit,
				reserve_exhausted: true,
				local_quota_retry: true,
				source: "quota_reserve",
			}),
			message: "refreshing".to_string(),
			edge_deferred: false,
			quota_spillover: None,
			tor_quota_refresh: Some(QuotaRefreshWaitTicket { generation: 7, quota_epoch: 4 }),
		};
		let started = Instant::now();
		let mut owner = None;
		let mut timed_retries = 0;
		loop {
			let changed = notify.notified();
			if ready.load(Ordering::SeqCst) {
				break;
			}
			let extended = matching_tor_quota_refresh_wait(RedditLane::Tor, &mut owner, &denied);
			let delay = local_quota_retry_delay(&denied, timed_retries, started.elapsed(), AdmissionRetryMode::Bounded, extended)
				.expect("an active Tor refresh must survive the ordinary two-second budget");
			let timer_elapsed = wait_for_local_quota_retry(changed, delay).await;
			timed_retries = local_quota_retry_count_after_wait(timed_retries, timer_elapsed);
		}
		completed.await.unwrap();
		assert!(started.elapsed() >= LOCAL_QUOTA_RETRY_BUDGET);
		assert_eq!(owner, denied.tor_quota_refresh);
	}

	#[test]
	fn test_tor_quota_refresh_redirect_replay_is_once_and_lane_scoped() {
		let now = Instant::now();
		let ticket = QuotaRefreshWaitTicket { generation: 7, quota_epoch: 4 };
		let mut state = TorQuotaRefreshRetryState::default();
		assert!(!state.claim_redirect_replay(RedditLane::Direct, ticket, now));
		assert_eq!(state.owner, None);
		assert_eq!(state.started, None);
		assert!(state.claim_redirect_replay(RedditLane::Tor, ticket, now));
		assert_eq!(state.owner, Some(ticket));
		assert_eq!(state.started, Some(now));
		assert!(state.redirect_replayed);
		assert!(!state.claim_redirect_replay(RedditLane::Tor, ticket, now + Duration::from_secs(1)));
		assert!(!state.claim_redirect_replay(RedditLane::Tor, QuotaRefreshWaitTicket { generation: 8, ..ticket }, now));

		assert_eq!(
			tor_redirect_refresh_recovery(RedditLane::Tor, AdmissionRetryMode::Bounded, false, ticket),
			LaneRecoveryReason::TorQuotaRefreshWait(ticket)
		);
		assert_eq!(
			tor_redirect_refresh_recovery(RedditLane::Direct, AdmissionRetryMode::Bounded, false, ticket),
			LaneRecoveryReason::None
		);
		assert_eq!(
			tor_redirect_refresh_recovery(RedditLane::Tor, AdmissionRetryMode::Immediate, false, ticket),
			LaneRecoveryReason::None
		);
		assert_eq!(
			tor_redirect_refresh_recovery(RedditLane::Tor, AdmissionRetryMode::Bounded, true, ticket),
			LaneRecoveryReason::None
		);
	}

	#[test]
	fn test_tor_quota_refresh_redirect_launch_requires_unowned_bounded_request() {
		assert!(quota_refresh_launch_allowed(AdmissionRetryMode::Bounded, false, false));
		assert!(!quota_refresh_launch_allowed(AdmissionRetryMode::Immediate, false, false));
		assert!(!quota_refresh_launch_allowed(AdmissionRetryMode::Immediate, true, false));
		assert!(!quota_refresh_launch_allowed(AdmissionRetryMode::Bounded, true, false));
		assert!(!quota_refresh_launch_allowed(AdmissionRetryMode::Bounded, false, true));
	}

	#[test]
	fn test_tor_quota_refresh_redirect_replay_preserves_owner_and_budget() {
		let started = Instant::now();
		let ticket = QuotaRefreshWaitTicket { generation: 7, quota_epoch: 4 };
		let mut state = TorQuotaRefreshRetryState {
			owner: Some(ticket),
			started: Some(started),
			timed_retries: 4,
			redirect_replayed: false,
		};
		let now = started + Duration::from_secs(3);
		assert!(!state.claim_redirect_replay(RedditLane::Tor, QuotaRefreshWaitTicket { generation: 8, ..ticket }, now));
		assert!(!state.claim_redirect_replay(RedditLane::Tor, QuotaRefreshWaitTicket { quota_epoch: 5, ..ticket }, now));
		assert_eq!(state.owner, Some(ticket));
		assert!(!state.redirect_replayed);
		assert!(state.claim_redirect_replay(RedditLane::Tor, ticket, now));
		assert_eq!(state.started, Some(started));
		assert_eq!(state.timed_retries, 4);

		let mut expired = TorQuotaRefreshRetryState {
			owner: Some(ticket),
			started: Some(started),
			..TorQuotaRefreshRetryState::default()
		};
		assert!(!expired.claim_redirect_replay(RedditLane::Tor, ticket, started + TOR_QUOTA_REFRESH_RETRY_BUDGET));
		assert_eq!(expired.started, Some(started));
		let mut retries_used = TorQuotaRefreshRetryState {
			timed_retries: MAX_TOR_QUOTA_REFRESH_RETRIES,
			..TorQuotaRefreshRetryState::default()
		};
		assert!(!retries_used.claim_redirect_replay(RedditLane::Tor, ticket, started));
		assert_eq!(retries_used.owner, None);
	}

	#[test]
	fn test_quota_spillover_requires_current_direct_reserve_without_cooldown() {
		let now = Instant::now();
		let mut direct = UpstreamGuard::new(RedditLane::Direct);
		direct.install_oauth_generation(7, true);
		direct.quota.window = QuotaWindow::Known {
			available: QUOTA_SAFETY_RESERVE,
			reset_at: now + Duration::from_secs(120),
		};
		let ticket = QuotaSpilloverTicket {
			budget: QuotaBudget::Rest,
			generation: 7,
			quota_epoch: direct.quota.epoch,
		};
		assert!(direct.quota_spillover_still_needed(now, ticket));

		direct.quota.window = QuotaWindow::Known {
			available: QUOTA_SAFETY_RESERVE + 1,
			reset_at: now + Duration::from_secs(120),
		};
		assert!(!direct.quota_spillover_still_needed(now, ticket));

		direct.quota.window = QuotaWindow::Known {
			available: QUOTA_SAFETY_RESERVE,
			reset_at: now + Duration::from_secs(120),
		};
		assert!(!direct.quota_spillover_still_needed(now, QuotaSpilloverTicket { generation: 8, ..ticket }));
		assert!(!direct.quota_spillover_still_needed(
			now,
			QuotaSpilloverTicket {
				quota_epoch: ticket.quota_epoch.wrapping_add(1),
				..ticket
			}
		));

		direct.quota.window = QuotaWindow::Unknown { probe_in_flight: false };
		assert!(!direct.quota_spillover_still_needed(now, ticket));
		direct.quota.window = QuotaWindow::Unreported;
		assert!(!direct.quota_spillover_still_needed(now, ticket));
		direct.quota.window = QuotaWindow::Known {
			available: 0,
			reset_at: now - RATE_LIMIT_COOLDOWN_MARGIN,
		};
		assert!(!direct.quota_spillover_still_needed(now, ticket));

		direct.quota.window = QuotaWindow::Known {
			available: 1,
			reset_at: now + EMERGENCY_QUOTA_ROTATION_MIN_RESET_REMAINING,
		};
		assert!(!direct.quota_spillover_still_needed(now, ticket));
		direct.quota.window = QuotaWindow::Known {
			available: 0,
			reset_at: now + EMERGENCY_QUOTA_ROTATION_MIN_RESET_REMAINING,
		};
		assert!(direct.quota_spillover_still_needed(now, ticket));

		direct.block_for_rate_limit(now, Duration::from_secs(30));
		assert!(!direct.quota_spillover_still_needed(now, ticket));
		direct.rate_limit_blocked_until = None;
		direct.upstream_failure_blocked_until = Some(now + Duration::from_secs(30));
		assert!(!direct.quota_spillover_still_needed(now, ticket));
		direct.upstream_failure_blocked_until = None;
		let edge_attempt = direct.begin_attempt(now).unwrap();
		direct.record_edge_throttle(now, edge_attempt, None);
		assert!(!direct.quota_spillover_still_needed(now, ticket));

		let mut tor = UpstreamGuard::new(RedditLane::Tor);
		tor.install_oauth_generation(7, true);
		tor.quota = direct.quota;
		assert!(!tor.quota_spillover_still_needed(now, ticket));
	}

	#[test]
	fn test_only_direct_local_reserve_denial_creates_spillover_ticket() {
		let reserve_denial = AdmissionDenied {
			delay: Duration::from_secs(30),
			reason: CooldownReason::RateLimit,
			reserve_exhausted: true,
			local_quota_retry: true,
			source: "quota_reserve",
		};
		let expected = Some(QuotaSpilloverTicket {
			budget: QuotaBudget::Rest,
			generation: 7,
			quota_epoch: 3,
		});
		assert_eq!(quota_spillover_ticket(RedditLane::Direct, reserve_denial, false, 7, Some(3)), expected);
		assert_eq!(quota_spillover_ticket(RedditLane::Tor, reserve_denial, false, 7, Some(3)), None);
		assert_eq!(quota_spillover_ticket(RedditLane::Direct, reserve_denial, true, 7, Some(3)), None);
		assert_eq!(quota_spillover_ticket(RedditLane::Direct, reserve_denial, false, 7, None), None);
		assert_eq!(
			quota_spillover_ticket(
				RedditLane::Direct,
				AdmissionDenied {
					source: "active_cooldown",
					..reserve_denial
				},
				false,
				7,
				Some(3),
			),
			None
		);
	}

	#[test]
	fn test_quota_spillover_keeps_total_local_wait_bounded() {
		assert_eq!(LOCAL_QUOTA_RETRY_BUDGET + TOR_QUOTA_SPILLOVER_TIMEOUT, Duration::from_secs(5));
		assert!(request_timeout_is_transport_failure(false));
		assert!(!request_timeout_is_transport_failure(true));
	}

	#[test]
	fn test_transport_retries_are_short_and_bounded() {
		assert_eq!(transport_retry_delay(0), Some(Duration::from_millis(250)));
		assert_eq!(transport_retry_delay(1), Some(Duration::from_millis(500)));
		assert_eq!(transport_retry_delay(MAX_TRANSPORT_RETRIES), None);
		assert!(TRANSPORT_RETRY_TIMEOUT < RedditLane::Tor.request_timeout());
	}

	#[test]
	fn test_upstream_cooldown_wait_is_server_side_and_bounded() {
		assert_eq!(
			upstream_cooldown_retry_delay(Duration::from_secs(12), 0, Duration::from_secs(1)),
			Some(Duration::from_secs(12))
		);
		assert_eq!(
			upstream_cooldown_retry_delay(Duration::from_secs(1), 1, Duration::from_secs(12)),
			Some(Duration::from_secs(1))
		);
		assert_eq!(upstream_cooldown_retry_delay(Duration::from_secs(15), 0, Duration::from_secs(31)), None);
		assert_eq!(upstream_cooldown_retry_delay(Duration::from_secs(1), MAX_UPSTREAM_COOLDOWN_WAITS, Duration::ZERO), None);
		assert_eq!(upstream_cooldown_retry_delay(Duration::from_secs(1), 0, UPSTREAM_RECOVERY_BUDGET), None);
	}

	#[tokio::test]
	async fn test_recovery_capacity_serializes_probes_and_releases_on_cancel() {
		let capacity = Semaphore::new(4);
		let gate = Semaphore::new(1);
		let deadline = tokio::time::Instant::now() + Duration::from_secs(1);
		let first = acquire_recovery_capacity(&capacity, &gate, deadline, || true).await.unwrap();
		let mut follower = Box::pin(acquire_recovery_capacity(&capacity, &gate, deadline, || true));
		assert!(futures_lite::future::poll_once(follower.as_mut()).await.is_none());
		assert_eq!(capacity.available_permits(), 3, "waiting followers must not occupy transport capacity");
		drop(follower);
		drop(first);
		assert_eq!(capacity.available_permits(), 4);
		assert_eq!(gate.available_permits(), 1);
		let next = acquire_recovery_capacity(&capacity, &gate, deadline, || true).await.unwrap();
		assert!(next.1.is_some());
	}

	#[test]
	fn test_recovery_capacity_immediate_spillover_cannot_bypass_probe() {
		let capacity = Semaphore::new(4);
		let gate = Semaphore::new(1);
		let probe = acquire_immediate_recovery_capacity(&capacity, &gate, || true).unwrap();
		assert!(acquire_immediate_recovery_capacity(&capacity, &gate, || true).is_err());
		assert_eq!(capacity.available_permits(), 3);
		drop(probe);
		assert_eq!(gate.available_permits(), 1);
		assert!(acquire_immediate_recovery_capacity(&capacity, &gate, || true).is_ok());
		let checked = AtomicBool::new(false);
		assert!(acquire_immediate_recovery_capacity(&capacity, &gate, || checked.swap(true, Ordering::SeqCst)).is_err());
		assert_eq!(capacity.available_permits(), 4);
	}

	#[test]
	fn test_recovery_budget_starts_at_failure_and_cannot_be_rearmed() {
		let now = tokio::time::Instant::now();
		let mut budget = RecoveryBudget::new(now, Duration::from_secs(100));
		let failure = now + Duration::from_secs(90);
		assert_eq!(budget.begin(failure), Duration::ZERO);
		assert_eq!(budget.remaining(failure), UPSTREAM_RECOVERY_BUDGET);
		let extended = failure + Duration::from_secs(12);
		assert_eq!(budget.begin(extended), Duration::from_secs(12));
		assert_eq!(budget.remaining(extended), Duration::from_secs(33));
		assert_eq!(budget.begin(extended + Duration::from_secs(20)), Duration::from_secs(32));
		assert_eq!(budget.remaining(failure + UPSTREAM_RECOVERY_BUDGET), Duration::ZERO);
		assert_eq!(budget.remaining(failure + UPSTREAM_RECOVERY_BUDGET + Duration::from_secs(1)), Duration::ZERO);
	}

	#[tokio::test]
	async fn test_recovery_budget_cancels_capacity_wait_without_leaking_probe() {
		let capacity = Semaphore::new(0);
		let gate = Semaphore::new(1);
		let now = tokio::time::Instant::now();
		let mut budget = RecoveryBudget::new(now, Duration::from_secs(100));
		budget.begin(now - UPSTREAM_RECOVERY_BUDGET);
		let future = acquire_recovery_capacity(&capacity, &gate, now + CAPACITY_WAIT_BUDGET, || true);
		assert!(tokio::time::timeout(budget.remaining(now), future).await.is_err());
		assert_eq!(gate.available_permits(), 1);
		assert_eq!(capacity.available_permits(), 0);
	}

	#[tokio::test]
	async fn test_recovery_capacity_rechecks_after_queue_and_success() {
		let capacity = Semaphore::new(1);
		let gate = Semaphore::new(1);
		let recovering = AtomicBool::new(false);
		let deadline = tokio::time::Instant::now() + Duration::from_secs(1);
		let busy = capacity.acquire().await.unwrap();
		let mut queued = Box::pin(acquire_recovery_capacity(&capacity, &gate, deadline, || recovering.load(Ordering::SeqCst)));
		assert!(futures_lite::future::poll_once(queued.as_mut()).await.is_none());
		recovering.store(true, Ordering::SeqCst);
		drop(busy);
		assert!(queued.await.is_err(), "a pre-cooldown waiter must not bypass recovery serialization");
		assert_eq!(capacity.available_permits(), 1);
		let probe = gate.acquire().await.unwrap();
		let mut follower = Box::pin(acquire_recovery_capacity(&capacity, &gate, deadline, || recovering.load(Ordering::SeqCst)));
		assert!(futures_lite::future::poll_once(follower.as_mut()).await.is_none());
		recovering.store(false, Ordering::SeqCst);
		drop(probe);
		let resumed = follower.await.unwrap();
		assert!(resumed.1.is_none(), "normal concurrency resumes after a successful probe");
	}

	#[test]
	fn test_headerless_quota_response_preserves_known_reset() {
		let now = Instant::now();
		let mut guard = UpstreamGuard::new(RedditLane::Tor);
		guard.install_oauth_generation(7, true);
		guard.quota.window = QuotaWindow::Known {
			available: 50,
			reset_at: now + Duration::from_secs(120),
		};
		let mut attempt = guard.try_admit(now, 7).unwrap();
		guard.reconcile_quota(now + Duration::from_secs(1), &mut attempt, None, None, true);
		assert!(matches!(guard.quota.window, QuotaWindow::Known { available: 0, reset_at } if reset_at == now + Duration::from_secs(120)));
	}

	#[test]
	fn test_recovery_success_does_not_clear_extended_cooldown() {
		let now = Instant::now();
		let mut guard = UpstreamGuard::default();
		let attempt = guard.begin_attempt(now).unwrap();
		guard.upstream_failure_blocked_until = Some(now + Duration::from_secs(12));
		guard.record_api_success(now + Duration::from_secs(11), attempt);
		assert!(guard.upstream_failure_blocked_until.is_some());
		guard.record_api_success(now + Duration::from_secs(13), attempt);
		assert!(guard.upstream_failure_blocked_until.is_none());
	}

	#[test]
	fn test_recovery_stale_success_releases_probe_without_clearing_cooldown() {
		let now = Instant::now();
		let mut guard = UpstreamGuard::default();
		guard.quota.generation = 2;
		guard.edges[0].state = EdgeCircuitState::HalfOpen {
			epoch: 1,
			expires_at: now + Duration::from_secs(90),
		};
		guard.upstream_failure_blocked_until = Some(now - Duration::from_secs(1));
		assert!(guard
			.record_generation_success(
				now,
				1,
				EdgeAttempt {
					endpoint: 0,
					epoch: 1,
					half_open: true
				}
			)
			.is_none());
		assert!(matches!(guard.edges[0].state, EdgeCircuitState::Open { .. }));
		assert!(guard.upstream_failure_blocked_until.is_some());
	}

	#[test]
	fn test_tor_spillover_admission_respects_tor_guard() {
		let now = Instant::now();
		let mut tor = UpstreamGuard::new(RedditLane::Tor);
		tor.install_oauth_generation(9, true);
		tor.quota.window = QuotaWindow::Known {
			available: QUOTA_SAFETY_RESERVE,
			reset_at: now + Duration::from_secs(120),
		};
		let denial = tor.try_admit(now, 9).unwrap_err();
		assert_eq!(denial.source, "quota_reserve");
		assert!(denial.reserve_exhausted);

		tor.quota.window = QuotaWindow::Known {
			available: QUOTA_SAFETY_RESERVE + 1,
			reset_at: now + Duration::from_secs(120),
		};
		tor.block_for_rate_limit(now, Duration::from_secs(30));
		let denial = tor.try_admit(now, 9).unwrap_err();
		assert_eq!(denial.source, "active_cooldown");
		assert_eq!(denial.reason, CooldownReason::RateLimit);
	}

	#[test]
	fn test_tor_early_rotation_keeps_current_allowance_admitted() {
		let now = Instant::now();
		let mut guard = UpstreamGuard::new(RedditLane::Tor);
		guard.install_oauth_generation(7, true);
		guard.quota_rotation_armed = true;
		guard.quota.window = QuotaWindow::Known {
			available: 30,
			reset_at: now + Duration::from_secs(600),
		};
		assert_eq!(guard.quota_rotation_candidate(now, 7, QuotaRotationMode::Proactive), None);
		let mut attempt = guard.try_admit(now, 7).unwrap();
		let ticket = guard.quota_rotation_candidate(now, 7, QuotaRotationMode::Proactive).unwrap();
		assert!(guard.claim_quota_rotation(now, ticket));
		assert_eq!(ticket.lane, RedditLane::Tor);
		assert_eq!(guard.quota_rotation_candidate(now, 6, QuotaRotationMode::Proactive), None);
		// Preparing the replacement must not consume or disable the old session.
		assert_eq!(guard.quota.generation, 7);
		guard.reconcile_quota(now, &mut attempt, Some(29), Some(Duration::from_secs(600)), false);
		let mut follower = guard.try_admit(now, 7).unwrap();
		assert!(guard.quota_rotation_still_needed(now, ticket));
		guard.reconcile_quota(now, &mut follower, Some(28), Some(Duration::from_secs(600)), false);
		follower.completed = true;
		attempt.completed = true;

		guard.lane = RedditLane::Direct;
		assert_eq!(guard.quota_rotation_candidate(now, 7, QuotaRotationMode::Proactive), None);
	}

	#[test]
	fn test_tor_early_rotation_avoids_long_near_reset_pause() {
		let now = Instant::now();
		let mut guard = UpstreamGuard::new(RedditLane::Tor);
		guard.install_oauth_generation(7, true);
		guard.quota_rotation_armed = true;
		for seconds in [600, 120, 30, 29, 6] {
			guard.quota.window = QuotaWindow::Known {
				available: 29,
				reset_at: now + Duration::from_secs(seconds),
			};
			assert!(guard.quota_rotation_candidate(now, 7, QuotaRotationMode::Proactive).is_some());
			assert_eq!(guard.take_short_reset_notice(now, 7), None);
		}
		guard.quota.window = QuotaWindow::Known {
			available: 29,
			reset_at: now + Duration::from_secs(5),
		};
		assert_eq!(guard.quota_rotation_candidate(now, 7, QuotaRotationMode::Proactive), None);
		assert_eq!(guard.take_short_reset_notice(now, 7), Some((29, Duration::from_secs(5))));
		assert_eq!(guard.take_short_reset_notice(now, 7), None);

		guard.quota.window = QuotaWindow::Known {
			available: 0,
			reset_at: now + Duration::from_secs(29),
		};
		let ticket = guard.quota_rotation_candidate(now, 7, QuotaRotationMode::Emergency).unwrap();
		assert!(guard.try_admit(now, 7).unwrap_err().reserve_exhausted);
		assert!(guard.claim_quota_rotation(now, ticket));
		assert_eq!(guard.quota_rotation_candidate(now, 7, QuotaRotationMode::Emergency), None);
		guard.emergency_rotation_claimed_epoch = None;
		guard.quota.window = QuotaWindow::Known {
			available: 0,
			reset_at: now + Duration::from_secs(5),
		};
		assert_eq!(guard.quota_rotation_candidate(now, 7, QuotaRotationMode::Emergency), None);
		assert_eq!(guard.quota_rotation_candidate(now + Duration::from_secs(5), 7, QuotaRotationMode::Proactive), None);
	}

	#[test]
	fn test_tor_early_rotation_requires_healthy_discovery() {
		let now = Instant::now();
		for remaining in [0, 9, 29, 30, 99] {
			let mut guard = UpstreamGuard::new(RedditLane::Tor);
			guard.install_oauth_generation(7, true);
			let mut attempt = guard.try_admit(now, 7).unwrap();
			guard.reconcile_quota(now, &mut attempt, Some(remaining), Some(Duration::from_secs(600)), false);
			assert_eq!(guard.quota_rotation_armed, remaining >= 30);
			if remaining < 30 {
				assert_eq!(guard.quota_rotation_candidate(now, 7, QuotaRotationMode::Proactive), None);
				assert_eq!(guard.quota_rotation_candidate(now, 7, QuotaRotationMode::Emergency), None);
			}
			attempt.completed = true;
		}
	}

	#[test]
	fn test_tor_early_rotation_completion_respects_reset_and_cooldowns() {
		let now = Instant::now();
		let reset_at = now + Duration::from_secs(6);
		let mut guard = UpstreamGuard::new(RedditLane::Tor);
		guard.install_oauth_generation(7, true);
		guard.quota_rotation_armed = true;
		guard.quota.window = QuotaWindow::Known { available: 29, reset_at };
		let ticket = guard.quota_rotation_candidate(now, 7, QuotaRotationMode::Proactive).unwrap();
		assert!(guard.claim_quota_rotation(now, ticket));
		let completed_at = now + Duration::from_secs(2);
		assert!(!guard.quota_rotation_still_needed(completed_at, ticket));
		assert!(guard.completed_quota_rotation_still_valid(completed_at, ticket));
		assert!(!guard.completed_quota_rotation_still_valid(reset_at, ticket));
		assert!(!guard.completed_quota_rotation_still_valid(now, QuotaRotationTicket { generation: 6, ..ticket }));
		assert!(!guard.completed_quota_rotation_still_valid(
			now,
			QuotaRotationTicket {
				quota_epoch: ticket.quota_epoch + 1,
				..ticket
			}
		));

		guard.rate_limit_blocked_until = Some(now + Duration::from_secs(60));
		assert_eq!(guard.quota_rotation_candidate(now, 7, QuotaRotationMode::Proactive), None);
		assert!(!guard.completed_quota_rotation_still_valid(completed_at, ticket));
		guard.rate_limit_blocked_until = None;
		guard.upstream_failure_blocked_until = Some(now + Duration::from_secs(60));
		assert_eq!(guard.quota_rotation_candidate(now, 7, QuotaRotationMode::Proactive), None);
		assert!(!guard.completed_quota_rotation_still_valid(completed_at, ticket));
		guard.upstream_failure_blocked_until = None;
		guard.edges[0].state = EdgeCircuitState::Open {
			until: now + Duration::from_secs(60),
		};
		assert_eq!(guard.quota_rotation_candidate(now, 7, QuotaRotationMode::Proactive), None);
		assert!(!guard.completed_quota_rotation_still_valid(completed_at, ticket));
	}

	#[test]
	fn test_quota_rotation_requires_known_exhausted_current_generation() {
		let now = Instant::now();
		let mut guard = UpstreamGuard {
			quota: QuotaGovernor {
				generation: 7,
				epoch: 1,
				next_request_id: 0,
				outstanding: 0,
				rollover_reserve: 0,
				headerless_consumption: 0,
				window: QuotaWindow::Known {
					available: LOW_RATE_LIMIT_THRESHOLD - 1,
					reset_at: now + QUOTA_ROTATION_MIN_RESET_REMAINING + Duration::from_nanos(1),
				},
			},
			quota_rotation_armed: true,
			..UpstreamGuard::default()
		};
		assert_eq!(
			guard.quota_rotation_candidate(now, 7, QuotaRotationMode::Proactive),
			Some(QuotaRotationTicket {
				lane: RedditLane::Direct,
				generation: 7,
				quota_epoch: 1,
				mode: QuotaRotationMode::Proactive,
			})
		);
		assert_eq!(guard.quota_rotation_candidate(now, 6, QuotaRotationMode::Proactive), None);

		guard.quota.window = QuotaWindow::Known {
			available: LOW_RATE_LIMIT_THRESHOLD - 1,
			reset_at: now + QUOTA_ROTATION_MIN_RESET_REMAINING,
		};
		assert_eq!(guard.quota_rotation_candidate(now, 7, QuotaRotationMode::Proactive), None);
		assert_eq!(
			guard.take_short_reset_notice(now, 7),
			Some((LOW_RATE_LIMIT_THRESHOLD - 1, QUOTA_ROTATION_MIN_RESET_REMAINING))
		);
		assert_eq!(guard.take_short_reset_notice(now, 7), None);
		guard.quota.epoch = guard.quota.epoch.wrapping_add(1);
		assert_eq!(
			guard.take_short_reset_notice(now, 7),
			Some((LOW_RATE_LIMIT_THRESHOLD - 1, QUOTA_ROTATION_MIN_RESET_REMAINING))
		);

		guard.quota.window = QuotaWindow::Known {
			available: LOW_RATE_LIMIT_THRESHOLD,
			reset_at: now + Duration::from_secs(60),
		};
		assert_eq!(guard.quota_rotation_candidate(now, 7, QuotaRotationMode::Proactive), None);

		guard.quota.window = QuotaWindow::Unknown { probe_in_flight: true };
		assert_eq!(guard.quota_rotation_candidate(now, 7, QuotaRotationMode::Proactive), None);
		guard.quota.window = QuotaWindow::Unreported;
		assert_eq!(guard.quota_rotation_candidate(now, 7, QuotaRotationMode::Proactive), None);

		guard.quota.window = QuotaWindow::Known {
			available: LOW_RATE_LIMIT_THRESHOLD - 1,
			reset_at: now + QUOTA_ROTATION_MIN_RESET_REMAINING + Duration::from_secs(1),
		};
		guard.edges[0].state = EdgeCircuitState::Open {
			until: now + Duration::from_secs(10),
		};
		assert_eq!(guard.quota_rotation_candidate(now, 7, QuotaRotationMode::Proactive), None);
		guard.edges[0].state = EdgeCircuitState::Closed;
		guard.upstream_failure_blocked_until = Some(now + Duration::from_secs(10));
		assert_eq!(guard.quota_rotation_candidate(now, 7, QuotaRotationMode::Proactive), None);
	}

	#[test]
	fn test_emergency_quota_rotation_requires_reserve_exhaustion_and_long_wait() {
		let now = Instant::now();
		let mut guard = UpstreamGuard {
			quota: QuotaGovernor {
				generation: 7,
				epoch: 4,
				next_request_id: 0,
				outstanding: 0,
				rollover_reserve: 0,
				headerless_consumption: 0,
				window: QuotaWindow::Known {
					available: QUOTA_SAFETY_RESERVE + 1,
					reset_at: now + EMERGENCY_QUOTA_ROTATION_MIN_RESET_REMAINING + Duration::from_secs(1),
				},
			},
			quota_rotation_armed: true,
			..UpstreamGuard::default()
		};
		assert_eq!(guard.quota_rotation_candidate(now, 7, QuotaRotationMode::Emergency), None);

		guard.quota.window = QuotaWindow::Known {
			available: QUOTA_SAFETY_RESERVE,
			reset_at: now + EMERGENCY_QUOTA_ROTATION_MIN_RESET_REMAINING,
		};
		assert_eq!(guard.quota_rotation_candidate(now, 7, QuotaRotationMode::Emergency), None);

		guard.quota.window = QuotaWindow::Known {
			available: QUOTA_SAFETY_RESERVE,
			reset_at: now + EMERGENCY_QUOTA_ROTATION_MIN_RESET_REMAINING + Duration::from_nanos(1),
		};
		let ticket = guard.quota_rotation_candidate(now, 7, QuotaRotationMode::Emergency).unwrap();
		assert_eq!(
			ticket,
			QuotaRotationTicket {
				lane: RedditLane::Direct,
				generation: 7,
				quota_epoch: 4,
				mode: QuotaRotationMode::Emergency,
			}
		);
		let denial = guard.try_admit(now, 7).unwrap_err();
		assert!(denial.reserve_exhausted);
		assert_eq!(denial.reason, CooldownReason::RateLimit);
		assert!(guard.claim_quota_rotation(now, ticket));
		assert!(guard.quota_rotation_still_needed(now, ticket));
		assert_eq!(guard.quota_rotation_candidate(now, 7, QuotaRotationMode::Emergency), None);
		assert!(!guard.claim_quota_rotation(now, ticket));
		guard.quota.window = QuotaWindow::Known {
			available: QUOTA_SAFETY_RESERVE + 1,
			reset_at: now + Duration::from_secs(90),
		};
		assert!(!guard.quota_rotation_still_needed(now, ticket));

		guard.quota.window = QuotaWindow::Known {
			available: QUOTA_SAFETY_RESERVE,
			reset_at: now + EMERGENCY_QUOTA_ROTATION_MIN_RESET_REMAINING,
		};
		assert!(!guard.quota_rotation_still_needed(now, ticket));
		assert!(guard.completed_quota_rotation_still_valid(now, ticket));
		assert!(!guard.completed_quota_rotation_still_valid(now + EMERGENCY_QUOTA_ROTATION_MIN_RESET_REMAINING, ticket));

		guard.emergency_rotation_claimed_epoch = None;
		guard.quota.window = QuotaWindow::Unknown { probe_in_flight: false };
		assert_eq!(guard.quota_rotation_candidate(now, 7, QuotaRotationMode::Emergency), None);
		guard.quota.window = QuotaWindow::Unreported;
		assert_eq!(guard.quota_rotation_candidate(now, 7, QuotaRotationMode::Emergency), None);
		guard.quota_rotation_armed = false;
		guard.quota.window = QuotaWindow::Known {
			available: QUOTA_SAFETY_RESERVE,
			reset_at: now + Duration::from_secs(90),
		};
		assert_eq!(guard.quota_rotation_candidate(now, 7, QuotaRotationMode::Emergency), None);
	}

	#[test]
	fn test_completed_quota_rotation_can_cross_launch_threshold() {
		let now = Instant::now();
		let emergency_reset = now + EMERGENCY_QUOTA_ROTATION_MIN_RESET_REMAINING + Duration::from_secs(1);
		let mut guard = UpstreamGuard {
			quota: QuotaGovernor {
				generation: 7,
				epoch: 4,
				next_request_id: 0,
				outstanding: 0,
				rollover_reserve: 0,
				headerless_consumption: 0,
				window: QuotaWindow::Known {
					available: QUOTA_SAFETY_RESERVE,
					reset_at: emergency_reset,
				},
			},
			quota_rotation_armed: true,
			..UpstreamGuard::default()
		};
		let emergency = guard.quota_rotation_candidate(now, 7, QuotaRotationMode::Emergency).unwrap();
		assert!(guard.claim_quota_rotation(now, emergency));
		let completed_at = now + Duration::from_secs(4);
		assert!(!guard.quota_rotation_still_needed(completed_at, emergency));
		for available in 0..=QUOTA_SAFETY_RESERVE {
			guard.quota.window = QuotaWindow::Known {
				available,
				reset_at: emergency_reset,
			};
			assert!(guard.completed_quota_rotation_still_valid(completed_at, emergency));
		}
		guard.quota.window = QuotaWindow::Known {
			available: QUOTA_SAFETY_RESERVE + 1,
			reset_at: emergency_reset,
		};
		assert!(!guard.completed_quota_rotation_still_valid(completed_at, emergency));

		guard.emergency_rotation_claimed_epoch = None;
		guard.quota.window = QuotaWindow::Known {
			available: LOW_RATE_LIMIT_THRESHOLD - 1,
			reset_at: now + QUOTA_ROTATION_MIN_RESET_REMAINING + Duration::from_secs(1),
		};
		let proactive = guard.quota_rotation_candidate(now, 7, QuotaRotationMode::Proactive).unwrap();
		let completed_at = now + Duration::from_secs(4);
		assert!(!guard.quota_rotation_still_needed(completed_at, proactive));
		assert!(guard.completed_quota_rotation_still_valid(completed_at, proactive));
	}

	#[test]
	fn test_emergency_quota_rotation_claim_survives_stable_refresh_but_not_new_epoch() {
		let now = Instant::now();
		let mut guard = UpstreamGuard {
			quota: QuotaGovernor {
				generation: 3,
				epoch: 8,
				next_request_id: 0,
				outstanding: 0,
				rollover_reserve: 0,
				headerless_consumption: 0,
				window: QuotaWindow::Known {
					available: QUOTA_SAFETY_RESERVE,
					reset_at: now + Duration::from_secs(90),
				},
			},
			quota_rotation_armed: true,
			..UpstreamGuard::default()
		};
		let ticket = guard.quota_rotation_candidate(now, 3, QuotaRotationMode::Emergency).unwrap();
		assert!(guard.claim_quota_rotation(now, ticket));

		guard.install_oauth_generation(4, false);
		assert_eq!(guard.emergency_rotation_claimed_epoch, Some(8));
		assert_eq!(guard.quota_rotation_candidate(now, 4, QuotaRotationMode::Emergency), None);

		guard.quota.epoch = 9;
		let next_ticket = guard.quota_rotation_candidate(now, 4, QuotaRotationMode::Emergency).unwrap();
		assert!(!guard.quota_rotation_still_needed(now, ticket));
		assert!(!guard.completed_quota_rotation_still_valid(now, ticket));
		assert!(guard.claim_quota_rotation(now, next_ticket));
		guard.rate_limit_blocked_until = Some(now + Duration::from_secs(10));
		assert!(!guard.quota_rotation_still_needed(now, next_ticket));
		assert!(!guard.completed_quota_rotation_still_valid(now, next_ticket));
		guard.rate_limit_blocked_until = None;
		guard.upstream_failure_blocked_until = Some(now + Duration::from_secs(10));
		assert!(!guard.quota_rotation_still_needed(now, next_ticket));
		assert!(!guard.completed_quota_rotation_still_valid(now, next_ticket));
		guard.upstream_failure_blocked_until = None;
		guard.edges[0].state = EdgeCircuitState::Open {
			until: now + Duration::from_secs(10),
		};
		assert!(!guard.quota_rotation_still_needed(now, next_ticket));
		assert!(!guard.completed_quota_rotation_still_valid(now, next_ticket));
	}

	#[test]
	fn test_fresh_low_discovery_does_not_rearm_rotation() {
		let mut guard = UpstreamGuard::default();
		guard.install_oauth_generation(1, true);
		let now = Instant::now();
		let mut attempt = guard.try_admit(now + Duration::from_millis(1), 1).unwrap();
		guard.reconcile_quota(now + Duration::from_millis(2), &mut attempt, Some(9), Some(Duration::from_secs(120)), false);
		assert!(!guard.quota_rotation_armed);
		assert_eq!(guard.quota_rotation_candidate(now + Duration::from_millis(3), 1, QuotaRotationMode::Proactive), None);
	}

	#[test]
	fn test_healthy_discovery_arms_quota_rotation() {
		let mut guard = UpstreamGuard::default();
		guard.install_oauth_generation(1, true);
		let now = Instant::now();
		let mut attempt = guard.try_admit(now + Duration::from_millis(1), 1).unwrap();
		guard.reconcile_quota(now + Duration::from_millis(2), &mut attempt, Some(99), Some(Duration::from_secs(120)), false);
		assert!(guard.quota_rotation_armed);

		guard.quota.window = QuotaWindow::Known {
			available: LOW_RATE_LIMIT_THRESHOLD - 1,
			reset_at: now + QUOTA_ROTATION_MIN_RESET_REMAINING + Duration::from_secs(1),
		};
		let candidate = guard.quota_rotation_candidate(now + Duration::from_millis(3), 1, QuotaRotationMode::Proactive);
		assert_eq!(
			candidate,
			Some(QuotaRotationTicket {
				lane: RedditLane::Direct,
				generation: 1,
				quota_epoch: guard.quota.epoch,
				mode: QuotaRotationMode::Proactive,
			})
		);
		guard.quota.epoch = guard.quota.epoch.wrapping_add(1);
		assert_ne!(candidate, guard.quota_rotation_candidate(now + Duration::from_millis(4), 1, QuotaRotationMode::Proactive));
	}

	#[test]
	fn test_out_of_order_quota_responses_cannot_replenish_budget() {
		let now = Instant::now();
		let mut quota = QuotaGovernor {
			generation: 3,
			epoch: 4,
			next_request_id: 2,
			outstanding: 2,
			rollover_reserve: 0,
			headerless_consumption: 0,
			window: QuotaWindow::Known {
				available: 6,
				reset_at: now + Duration::from_secs(120),
			},
		};
		let attempt = |request_id| UpstreamAttempt {
			lane: RedditLane::Direct,
			edge: EdgeAttempt {
				endpoint: 0,
				epoch: 0,
				half_open: false,
			},
			generation: 3,
			quota_epoch: 4,
			request_id,
			quota_consumption_watermark: 0,
			discovery_probe: false,
			sent: true,
			quota_reconciled: true,
			completed: true,
		};
		quota.reconcile(now, &attempt(1), Some(8), Some(Duration::from_secs(120)), false);
		quota.reconcile(now, &attempt(2), Some(9), Some(Duration::from_secs(119)), false);
		assert!(matches!(quota.window, QuotaWindow::Known { available: 6, .. }));
	}

	#[test]
	fn test_early_new_window_response_cannot_poison_low_budget_deadline() {
		let now = Instant::now();
		let old_reset = now + Duration::from_secs(5);
		let mut quota = QuotaGovernor {
			generation: 3,
			epoch: 4,
			next_request_id: 2,
			outstanding: 2,
			rollover_reserve: 0,
			headerless_consumption: 0,
			window: QuotaWindow::Known {
				available: LOW_RATE_LIMIT_THRESHOLD - 1,
				reset_at: old_reset,
			},
		};
		let attempt = UpstreamAttempt {
			lane: RedditLane::Direct,
			edge: EdgeAttempt {
				endpoint: 0,
				epoch: 0,
				half_open: false,
			},
			generation: 3,
			quota_epoch: 4,
			request_id: 1,
			quota_consumption_watermark: 0,
			discovery_probe: false,
			sent: true,
			quota_reconciled: true,
			completed: true,
		};

		// Reddit has already started its next window, but our latency-adjusted
		// deadline is still a few seconds away. Keep the old boundary instead of
		// moving the exhausted allowance forward by another full window.
		assert!(quota.reconcile(now, &attempt, Some(81), Some(Duration::from_secs(565)), false));
		assert_eq!(quota.outstanding, 1);
		assert!(matches!(
			quota.window,
			QuotaWindow::Known { available, reset_at }
				if available == LOW_RATE_LIMIT_THRESHOLD - 1 && reset_at == old_reset
		));

		let mut guard = UpstreamGuard::default();
		guard.quota = quota;
		guard.quota_rotation_armed = true;
		assert_eq!(guard.quota_rotation_candidate(now, 3, QuotaRotationMode::Proactive), None);
		assert_eq!(guard.take_short_reset_notice(now, 3), Some((LOW_RATE_LIMIT_THRESHOLD - 1, Duration::from_secs(5))));

		let after_old_boundary = old_reset + RATE_LIMIT_COOLDOWN_MARGIN;
		let (quota_epoch, request_id, discovery_probe) = guard.quota.reserve(after_old_boundary, 3).unwrap();
		assert_eq!(quota_epoch, 5);
		assert!(discovery_probe);
		let discovery_attempt = UpstreamAttempt {
			lane: RedditLane::Direct,
			edge: EdgeAttempt {
				endpoint: 0,
				epoch: 0,
				half_open: false,
			},
			generation: 3,
			quota_epoch,
			request_id,
			quota_consumption_watermark: 0,
			discovery_probe,
			sent: true,
			quota_reconciled: true,
			completed: true,
		};
		assert!(guard
			.quota
			.reconcile(after_old_boundary, &discovery_attempt, Some(81), Some(Duration::from_secs(565)), false,));
		assert!(matches!(guard.quota.window, QuotaWindow::Known { available: 80, .. }));

		let late_old_attempt = UpstreamAttempt {
			lane: RedditLane::Direct,
			edge: EdgeAttempt {
				endpoint: 0,
				epoch: 0,
				half_open: false,
			},
			generation: 3,
			quota_epoch: 4,
			request_id: 2,
			quota_consumption_watermark: 0,
			discovery_probe: false,
			sent: true,
			quota_reconciled: true,
			completed: true,
		};
		assert!(!guard.quota.reconcile(
			after_old_boundary + Duration::from_millis(1),
			&late_old_attempt,
			Some(82),
			Some(Duration::from_secs(564)),
			false,
		));
		guard.quota.abandon(after_old_boundary + Duration::from_millis(1), &late_old_attempt);
		assert!(matches!(guard.quota.window, QuotaWindow::Known { available: 80, .. }));
	}

	#[test]
	fn test_reset_only_response_cannot_poison_known_deadline() {
		let now = Instant::now();
		let old_reset = now + Duration::from_secs(5);
		let mut quota = QuotaGovernor {
			generation: 2,
			epoch: 3,
			next_request_id: 1,
			outstanding: 1,
			rollover_reserve: 0,
			headerless_consumption: 0,
			window: QuotaWindow::Known {
				available: LOW_RATE_LIMIT_THRESHOLD - 1,
				reset_at: old_reset,
			},
		};
		let attempt = UpstreamAttempt {
			lane: RedditLane::Direct,
			edge: EdgeAttempt {
				endpoint: 0,
				epoch: 0,
				half_open: false,
			},
			generation: 2,
			quota_epoch: 3,
			request_id: 1,
			quota_consumption_watermark: 0,
			discovery_probe: false,
			sent: true,
			quota_reconciled: true,
			completed: true,
		};

		assert!(quota.reconcile(now, &attempt, None, Some(Duration::from_secs(565)), false));
		assert!(matches!(quota.window, QuotaWindow::Known { reset_at, .. } if reset_at == old_reset));
	}

	#[test]
	fn test_small_same_window_reset_jitter_is_preserved() {
		let now = Instant::now();
		let old_reset = now + Duration::from_secs(60);
		let mut quota = QuotaGovernor {
			generation: 2,
			epoch: 3,
			next_request_id: 1,
			outstanding: 1,
			rollover_reserve: 0,
			headerless_consumption: 0,
			window: QuotaWindow::Known {
				available: 40,
				reset_at: old_reset,
			},
		};
		let attempt = UpstreamAttempt {
			lane: RedditLane::Direct,
			edge: EdgeAttempt {
				endpoint: 0,
				epoch: 0,
				half_open: false,
			},
			generation: 2,
			quota_epoch: 3,
			request_id: 1,
			quota_consumption_watermark: 0,
			discovery_probe: false,
			sent: true,
			quota_reconciled: true,
			completed: true,
		};

		assert!(quota.reconcile(now, &attempt, Some(39), Some(Duration::from_secs(61)), false));
		assert!(matches!(quota.window, QuotaWindow::Known { reset_at, .. } if reset_at == old_reset + Duration::from_secs(1)));
	}

	#[test]
	fn test_token_refresh_preserves_quota_window() {
		let now = Instant::now();
		let mut quota = QuotaGovernor {
			generation: 4,
			epoch: 2,
			next_request_id: 8,
			outstanding: 0,
			rollover_reserve: 0,
			headerless_consumption: 0,
			window: QuotaWindow::Known {
				available: 7,
				reset_at: now + Duration::from_secs(90),
			},
		};
		quota.install_generation(5, false);
		assert_eq!(quota.generation, 5);
		assert!(matches!(quota.window, QuotaWindow::Known { available: 7, .. }));
	}

	#[test]
	fn test_fresh_identity_starts_unknown_quota_window_and_ignores_late_responses() {
		let now = Instant::now();
		let mut quota = QuotaGovernor {
			generation: 4,
			epoch: 2,
			next_request_id: 8,
			outstanding: 2,
			rollover_reserve: 1,
			headerless_consumption: 0,
			window: QuotaWindow::Known {
				available: 5,
				reset_at: now + Duration::from_secs(90),
			},
		};
		let stale_attempt = UpstreamAttempt {
			lane: RedditLane::Direct,
			edge: EdgeAttempt {
				endpoint: 0,
				epoch: 0,
				half_open: false,
			},
			generation: 4,
			quota_epoch: 2,
			request_id: 8,
			quota_consumption_watermark: 0,
			discovery_probe: false,
			sent: true,
			quota_reconciled: true,
			completed: true,
		};
		let mut stale_unsent_attempt = UpstreamAttempt {
			lane: RedditLane::Direct,
			edge: EdgeAttempt {
				endpoint: 0,
				epoch: 0,
				half_open: false,
			},
			generation: 4,
			quota_epoch: 2,
			request_id: 9,
			quota_consumption_watermark: 0,
			discovery_probe: false,
			sent: false,
			quota_reconciled: false,
			completed: false,
		};

		quota.install_generation(5, true);
		let fresh_now = Instant::now();
		assert_eq!(quota.generation, 5);
		assert_eq!(quota.epoch, 3);
		assert_eq!(quota.outstanding, 0);
		assert_eq!(quota.rollover_reserve, 0);
		assert!(matches!(quota.window, QuotaWindow::Unknown { probe_in_flight: false, .. }));

		quota.reconcile(now, &stale_attempt, Some(99), Some(Duration::from_secs(300)), false);
		assert!(matches!(quota.window, QuotaWindow::Unknown { probe_in_flight: false, .. }));
		assert!(quota.reserve(fresh_now + Duration::from_secs(1), 5).is_ok());
		quota.abandon(fresh_now + Duration::from_secs(1), &stale_unsent_attempt);
		stale_unsent_attempt.quota_reconciled = true;
		stale_unsent_attempt.completed = true;
		assert_eq!(quota.outstanding, 1);
		assert!(matches!(quota.window, QuotaWindow::Unknown { probe_in_flight: true, .. }));
		let (_, _, follower_is_probe) = quota.reserve(fresh_now + Duration::from_secs(1), 5).unwrap();
		assert!(!follower_is_probe);
		assert_eq!(quota.outstanding, 2);
	}

	#[test]
	fn test_stale_generation_cannot_reserve_or_replace_budget() {
		let now = Instant::now();
		let mut quota = QuotaGovernor {
			generation: 5,
			epoch: 3,
			next_request_id: 1,
			outstanding: 1,
			rollover_reserve: 0,
			headerless_consumption: 0,
			window: QuotaWindow::Known {
				available: 7,
				reset_at: now + Duration::from_secs(90),
			},
		};
		assert_eq!(quota.reserve(now, 4), Err(QuotaReserveError::StaleGeneration));
		assert_eq!(quota.generation, 5);

		let stale_attempt = UpstreamAttempt {
			lane: RedditLane::Direct,
			edge: EdgeAttempt {
				endpoint: 0,
				epoch: 0,
				half_open: false,
			},
			generation: 4,
			quota_epoch: 3,
			request_id: 1,
			quota_consumption_watermark: 0,
			discovery_probe: false,
			sent: true,
			quota_reconciled: true,
			completed: true,
		};
		quota.reconcile(now, &stale_attempt, Some(99), Some(Duration::from_secs(500)), false);
		assert_eq!(quota.outstanding, 0);
		assert!(matches!(quota.window, QuotaWindow::Known { available: 7, .. }));
	}

	#[test]
	fn test_headerless_backend_leaves_discovery_mode() {
		let now = Instant::now();
		let mut quota = QuotaGovernor {
			generation: 2,
			epoch: 0,
			next_request_id: 0,
			outstanding: 0,
			rollover_reserve: 0,
			headerless_consumption: 0,
			window: QuotaWindow::Unknown { probe_in_flight: false },
		};
		let (quota_epoch, request_id, discovery_probe) = quota.reserve(now, 2).unwrap();
		assert!(discovery_probe);
		let attempt = UpstreamAttempt {
			lane: RedditLane::Direct,
			edge: EdgeAttempt {
				endpoint: 0,
				epoch: 0,
				half_open: false,
			},
			generation: 2,
			quota_epoch,
			request_id,
			quota_consumption_watermark: 0,
			discovery_probe,
			sent: true,
			quota_reconciled: true,
			completed: true,
		};
		quota.reconcile(now, &attempt, None, None, false);
		assert!(matches!(quota.window, QuotaWindow::Unknown { probe_in_flight: true, .. }));
		assert!(!quota.reserve(now, 2).unwrap().2);
		quota.confirm_headerless_success(&attempt);
		assert!(matches!(quota.window, QuotaWindow::Unreported));
		assert!(quota.reserve(now, 2).is_ok());
	}

	#[test]
	fn test_follower_redirect_during_headerless_discovery_remains_admitted() {
		let now = Instant::now();
		let mut quota = QuotaGovernor {
			generation: 2,
			epoch: 4,
			next_request_id: 0,
			outstanding: 0,
			rollover_reserve: 0,
			headerless_consumption: 0,
			window: QuotaWindow::Unknown { probe_in_flight: false },
		};
		let (quota_epoch, probe_request_id, discovery_probe) = quota.reserve(now, 2).unwrap();
		assert!(discovery_probe);
		let (_, follower_request_id, follower_is_probe) = quota.reserve(now, 2).unwrap();
		assert!(!follower_is_probe);

		let probe = UpstreamAttempt {
			lane: RedditLane::Direct,
			edge: EdgeAttempt {
				endpoint: 0,
				epoch: 0,
				half_open: false,
			},
			generation: 2,
			quota_epoch,
			request_id: probe_request_id,
			quota_consumption_watermark: 0,
			discovery_probe: true,
			sent: true,
			quota_reconciled: true,
			completed: true,
		};
		let follower = UpstreamAttempt {
			request_id: follower_request_id,
			quota_consumption_watermark: 0,
			discovery_probe: false,
			..probe
		};
		quota.reconcile(now, &probe, None, None, false);
		quota.reconcile(now, &follower, None, None, false);
		quota.continue_headerless_discovery_after_redirect(now, &follower);
		assert!(matches!(quota.window, QuotaWindow::Unknown { probe_in_flight: true, .. }));
		let (_, _, redirect_is_probe) = quota.reserve(now, 2).unwrap();
		assert!(!redirect_is_probe);

		quota.confirm_headerless_success(&probe);
		assert!(matches!(quota.window, QuotaWindow::Unreported));
	}

	#[test]
	fn test_headerless_follower_consumption_is_debited_from_first_quota_snapshot() {
		let now = Instant::now();
		let mut quota = QuotaGovernor {
			generation: 2,
			epoch: 4,
			next_request_id: 0,
			outstanding: 0,
			rollover_reserve: 0,
			headerless_consumption: 0,
			window: QuotaWindow::Unknown { probe_in_flight: false },
		};
		let (quota_epoch, probe_request_id, discovery_probe) = quota.reserve(now, 2).unwrap();
		assert!(discovery_probe);
		let (_, follower_request_id, follower_is_probe) = quota.reserve(now, 2).unwrap();
		assert!(!follower_is_probe);

		let follower = UpstreamAttempt {
			lane: RedditLane::Direct,
			edge: EdgeAttempt {
				endpoint: 0,
				epoch: 0,
				half_open: false,
			},
			generation: 2,
			quota_epoch,
			request_id: follower_request_id,
			quota_consumption_watermark: 0,
			discovery_probe: false,
			sent: true,
			quota_reconciled: true,
			completed: true,
		};
		quota.reconcile(now, &follower, None, None, false);
		assert_eq!(quota.headerless_consumption, 1);
		let (_, _, redirect_is_probe) = quota.reserve(now, 2).unwrap();
		assert!(!redirect_is_probe);

		let probe = UpstreamAttempt {
			request_id: probe_request_id,
			quota_consumption_watermark: 0,
			discovery_probe: true,
			..follower
		};
		quota.reconcile(now, &probe, Some(99), Some(Duration::from_secs(300)), false);
		assert!(matches!(quota.window, QuotaWindow::Known { available: 97, .. }));
		assert_eq!(quota.headerless_consumption, 0);
	}

	#[test]
	fn test_successful_headerless_follower_keeps_debt_until_first_quota_snapshot() {
		let now = Instant::now();
		let mut quota = QuotaGovernor {
			generation: 2,
			epoch: 4,
			next_request_id: 0,
			outstanding: 0,
			rollover_reserve: 0,
			headerless_consumption: 0,
			window: QuotaWindow::Unknown { probe_in_flight: false },
		};
		let (quota_epoch, probe_request_id, discovery_probe) = quota.reserve(now, 2).unwrap();
		assert!(discovery_probe);
		let (_, follower_request_id, follower_is_probe) = quota.reserve(now, 2).unwrap();
		assert!(!follower_is_probe);

		let follower = UpstreamAttempt {
			lane: RedditLane::Direct,
			edge: EdgeAttempt {
				endpoint: 0,
				epoch: 0,
				half_open: false,
			},
			generation: 2,
			quota_epoch,
			request_id: follower_request_id,
			quota_consumption_watermark: 0,
			discovery_probe: false,
			sent: true,
			quota_reconciled: true,
			completed: true,
		};
		quota.reconcile(now, &follower, None, None, false);
		quota.confirm_headerless_success(&follower);
		assert!(matches!(quota.window, QuotaWindow::Unreported));
		assert_eq!(quota.headerless_consumption, 1);

		let probe = UpstreamAttempt {
			request_id: probe_request_id,
			quota_consumption_watermark: 0,
			discovery_probe: true,
			..follower
		};
		quota.reconcile(now, &probe, Some(99), Some(Duration::from_secs(300)), false);
		assert!(matches!(quota.window, QuotaWindow::Known { available: 98, .. }));
		assert_eq!(quota.headerless_consumption, 0);
	}

	#[test]
	fn test_sequential_headerless_history_is_not_charged_to_later_quota_snapshot() {
		let now = Instant::now();
		let mut quota = QuotaGovernor {
			generation: 2,
			epoch: 4,
			next_request_id: 0,
			outstanding: 0,
			rollover_reserve: 0,
			headerless_consumption: 0,
			window: QuotaWindow::Unknown { probe_in_flight: false },
		};

		for index in 0..100 {
			let (quota_epoch, request_id, discovery_probe) = quota.reserve(now, 2).unwrap();
			let watermark = quota.headerless_consumption;
			let attempt = UpstreamAttempt {
				lane: RedditLane::Direct,
				edge: EdgeAttempt {
					endpoint: 0,
					epoch: 0,
					half_open: false,
				},
				generation: 2,
				quota_epoch,
				request_id,
				quota_consumption_watermark: watermark,
				discovery_probe,
				sent: true,
				quota_reconciled: true,
				completed: true,
			};
			quota.reconcile(now, &attempt, None, None, false);
			if index == 0 {
				quota.confirm_headerless_success(&attempt);
			}
		}
		assert!(matches!(quota.window, QuotaWindow::Unreported));
		assert_eq!(quota.headerless_consumption, 100);

		let (quota_epoch, request_id, discovery_probe) = quota.reserve(now, 2).unwrap();
		let snapshot = UpstreamAttempt {
			lane: RedditLane::Direct,
			edge: EdgeAttempt {
				endpoint: 0,
				epoch: 0,
				half_open: false,
			},
			generation: 2,
			quota_epoch,
			request_id,
			quota_consumption_watermark: quota.headerless_consumption,
			discovery_probe,
			sent: true,
			quota_reconciled: true,
			completed: true,
		};
		quota.reconcile(now, &snapshot, Some(99), Some(Duration::from_secs(300)), false);
		assert!(matches!(quota.window, QuotaWindow::Known { available: 99, .. }));
		assert_eq!(quota.headerless_consumption, 0);
	}

	#[test]
	fn test_reconciled_discovery_probe_failure_releases_probe() {
		let now = Instant::now();
		let mut quota = QuotaGovernor {
			generation: 2,
			epoch: 4,
			next_request_id: 0,
			outstanding: 0,
			rollover_reserve: 0,
			headerless_consumption: 0,
			window: QuotaWindow::Unknown { probe_in_flight: false },
		};
		let (quota_epoch, request_id, discovery_probe) = quota.reserve(now, 2).unwrap();
		let mut probe = UpstreamAttempt {
			lane: RedditLane::Direct,
			edge: EdgeAttempt {
				endpoint: 0,
				epoch: 0,
				half_open: false,
			},
			generation: 2,
			quota_epoch,
			request_id,
			quota_consumption_watermark: 0,
			discovery_probe,
			sent: true,
			quota_reconciled: true,
			completed: false,
		};
		quota.reconcile(now, &probe, None, None, false);
		assert!(matches!(quota.window, QuotaWindow::Unknown { probe_in_flight: true, .. }));
		quota.abandon(now, &probe);
		probe.completed = true;
		assert!(matches!(quota.window, QuotaWindow::Unknown { probe_in_flight: false, .. }));
		assert!(quota.reserve(now, 2).unwrap().2);
	}

	#[test]
	fn test_sent_abandoned_discovery_request_remains_uncertain_quota_debt() {
		let now = Instant::now();
		let mut quota = QuotaGovernor {
			generation: 2,
			epoch: 4,
			next_request_id: 0,
			outstanding: 0,
			rollover_reserve: 0,
			headerless_consumption: 0,
			window: QuotaWindow::Unknown { probe_in_flight: false },
		};
		let (quota_epoch, request_id, discovery_probe) = quota.reserve(now, 2).unwrap();
		let abandoned = UpstreamAttempt {
			lane: RedditLane::Direct,
			edge: EdgeAttempt {
				endpoint: 0,
				epoch: 0,
				half_open: false,
			},
			generation: 2,
			quota_epoch,
			request_id,
			quota_consumption_watermark: 0,
			discovery_probe,
			sent: true,
			quota_reconciled: false,
			completed: true,
		};
		quota.abandon(now, &abandoned);
		assert_eq!(quota.rollover_reserve, 1);
		assert_eq!(quota.headerless_consumption, 0);

		let (quota_epoch, request_id, discovery_probe) = quota.reserve(now, 2).unwrap();
		let snapshot = UpstreamAttempt {
			quota_epoch,
			request_id,
			quota_consumption_watermark: quota.headerless_consumption,
			discovery_probe,
			quota_reconciled: true,
			..abandoned
		};
		quota.reconcile(now, &snapshot, Some(99), Some(Duration::from_secs(300)), false);
		assert!(matches!(quota.window, QuotaWindow::Known { available: 98, .. }));
		assert_eq!(quota.rollover_reserve, 0);
	}

	#[test]
	fn test_discovery_follower_failure_does_not_release_probe() {
		let now = Instant::now();
		let mut quota = QuotaGovernor {
			generation: 2,
			epoch: 4,
			next_request_id: 0,
			outstanding: 0,
			rollover_reserve: 0,
			headerless_consumption: 0,
			window: QuotaWindow::Unknown { probe_in_flight: false },
		};
		let (quota_epoch, probe_request_id, discovery_probe) = quota.reserve(now, 2).unwrap();
		assert!(discovery_probe);
		let (_, follower_request_id, follower_is_probe) = quota.reserve(now, 2).unwrap();
		assert!(!follower_is_probe);

		let follower = UpstreamAttempt {
			lane: RedditLane::Direct,
			edge: EdgeAttempt {
				endpoint: 0,
				epoch: 0,
				half_open: false,
			},
			generation: 2,
			quota_epoch,
			request_id: follower_request_id,
			quota_consumption_watermark: 0,
			discovery_probe: false,
			sent: true,
			quota_reconciled: true,
			completed: true,
		};
		quota.reconcile(now, &follower, None, None, false);
		assert_eq!(quota.outstanding, 1);
		assert!(matches!(quota.window, QuotaWindow::Unknown { probe_in_flight: true, .. }));

		let (_, abandoned_request_id, abandoned_is_probe) = quota.reserve(now, 2).unwrap();
		assert!(!abandoned_is_probe);
		let mut abandoned_follower = UpstreamAttempt {
			request_id: abandoned_request_id,
			quota_consumption_watermark: 0,
			sent: false,
			quota_reconciled: false,
			completed: false,
			..follower
		};
		quota.abandon(now, &abandoned_follower);
		abandoned_follower.quota_reconciled = true;
		abandoned_follower.completed = true;
		assert_eq!(quota.outstanding, 1);
		assert!(matches!(quota.window, QuotaWindow::Unknown { probe_in_flight: true, .. }));

		let mut probe = UpstreamAttempt {
			request_id: probe_request_id,
			quota_consumption_watermark: 0,
			discovery_probe: true,
			sent: false,
			quota_reconciled: false,
			completed: false,
			..follower
		};
		quota.abandon(now, &probe);
		probe.quota_reconciled = true;
		probe.completed = true;
		assert_eq!(quota.outstanding, 0);
		assert!(matches!(quota.window, QuotaWindow::Unknown { probe_in_flight: false, .. }));
	}

	#[test]
	fn test_stale_discovery_releases_probe_without_applying_headers() {
		let now = Instant::now();
		let mut quota = QuotaGovernor {
			generation: 1,
			epoch: 4,
			next_request_id: 0,
			outstanding: 0,
			rollover_reserve: 0,
			headerless_consumption: 0,
			window: QuotaWindow::Unknown { probe_in_flight: false },
		};
		let (quota_epoch, request_id, discovery_probe) = quota.reserve(now, 1).unwrap();
		quota.install_generation(2, false);
		let stale_attempt = UpstreamAttempt {
			lane: RedditLane::Direct,
			edge: EdgeAttempt {
				endpoint: 0,
				epoch: 0,
				half_open: false,
			},
			generation: 1,
			quota_epoch,
			request_id,
			quota_consumption_watermark: 0,
			discovery_probe,
			sent: true,
			quota_reconciled: true,
			completed: true,
		};
		quota.reconcile(now, &stale_attempt, Some(99), Some(Duration::from_secs(300)), false);
		assert_eq!(quota.outstanding, 0);
		assert!(matches!(quota.window, QuotaWindow::Unknown { probe_in_flight: false, .. }));
		assert!(quota.reserve(now, 2).unwrap().2);
	}

	#[test]
	fn test_headerless_redirect_preserves_discovery_probe_with_a_new_reservation() {
		let now = Instant::now();
		let mut quota = QuotaGovernor {
			generation: 3,
			epoch: 7,
			next_request_id: 0,
			outstanding: 0,
			rollover_reserve: 0,
			headerless_consumption: 0,
			window: QuotaWindow::Unknown { probe_in_flight: false },
		};
		let (quota_epoch, request_id, discovery_probe) = quota.reserve(now, 3).unwrap();
		let mut attempt = UpstreamAttempt {
			lane: RedditLane::Direct,
			edge: EdgeAttempt {
				endpoint: 0,
				epoch: 0,
				half_open: false,
			},
			generation: 3,
			quota_epoch,
			request_id,
			quota_consumption_watermark: 0,
			discovery_probe,
			sent: true,
			quota_reconciled: false,
			completed: false,
		};
		assert_eq!(quota.outstanding, 1);
		quota.reconcile(now, &attempt, None, None, false);
		attempt.quota_reconciled = true;
		quota.continue_headerless_discovery_after_redirect(now, &attempt);
		let (next_epoch, next_request_id, next_discovery_probe) = quota.reserve(now, 3).unwrap();
		assert_eq!(next_epoch, quota_epoch);
		assert_ne!(next_request_id, request_id);
		assert!(next_discovery_probe);
		let (_, _, follower_is_probe) = quota.reserve(now, 3).unwrap();
		assert!(!follower_is_probe);
		assert_eq!(quota.outstanding, 2);
		attempt.completed = true;
	}

	#[test]
	fn test_response_after_reset_establishes_new_quota_window() {
		let now = Instant::now();
		let old_reset = now - RATE_LIMIT_COOLDOWN_MARGIN;
		let mut quota = QuotaGovernor {
			generation: 8,
			epoch: 12,
			next_request_id: 3,
			outstanding: 3,
			rollover_reserve: 0,
			headerless_consumption: 0,
			window: QuotaWindow::Known {
				available: 6,
				reset_at: old_reset,
			},
		};
		let attempt = UpstreamAttempt {
			lane: RedditLane::Direct,
			edge: EdgeAttempt {
				endpoint: 0,
				epoch: 0,
				half_open: false,
			},
			generation: 8,
			quota_epoch: 12,
			request_id: 3,
			quota_consumption_watermark: 0,
			discovery_probe: false,
			sent: true,
			quota_reconciled: true,
			completed: true,
		};
		quota.reconcile(now, &attempt, Some(90), Some(Duration::from_secs(300)), false);
		assert_eq!(quota.epoch, 13);
		assert_eq!(quota.outstanding, 0);
		assert!(matches!(quota.window, QuotaWindow::Known { available: 88, .. }));
	}

	#[test]
	fn test_reset_allows_exactly_one_discovery_probe() {
		let now = Instant::now();
		let mut quota = QuotaGovernor {
			generation: 1,
			epoch: 9,
			next_request_id: 0,
			outstanding: 0,
			rollover_reserve: 0,
			headerless_consumption: 0,
			window: QuotaWindow::Known {
				available: 0,
				reset_at: now - RATE_LIMIT_COOLDOWN_MARGIN,
			},
		};
		assert!(quota.reserve(now, 1).unwrap().2);
		for _ in 0..16 {
			assert!(!quota.reserve(now, 1).unwrap().2);
		}
		assert_eq!(quota.outstanding, 17);
		assert!(matches!(quota.window, QuotaWindow::Unknown { probe_in_flight: true, .. }));
	}

	#[test]
	fn test_redirect_validation_rejects_off_origin_and_normalizes_reddit() {
		assert!(validated_reddit_redirect_path("https://example.com/r/rust", RedditLane::Direct).is_err());
		assert!(validated_reddit_redirect_path("//oauth.reddit.com/r/rust", RedditLane::Direct).is_err());
		assert!(validated_reddit_redirect_path("https://user@oauth.reddit.com/r/rust", RedditLane::Direct).is_err());
		assert!(validated_reddit_redirect_path("https://oauth.reddit.com:444/r/rust", RedditLane::Direct).is_err());
		assert!(validated_reddit_redirect_path("https://oauth.reddit.com/r/rust#fragment", RedditLane::Direct).is_err());
		assert!(validated_reddit_redirect_path("/r/rust#fragment", RedditLane::Direct).is_err());
		assert_eq!(
			validated_reddit_redirect_path("https://www.reddit.com/r/rust/hot.json?limit=25", RedditLane::Direct).unwrap(),
			"/r/rust/hot.json?limit=25&raw_json=1"
		);
		let tor_location = format!("{}/r/rust/hot.json?limit=25", RedditLane::Tor.auth_origin().base);
		assert_eq!(
			validated_reddit_redirect_path(&tor_location, RedditLane::Tor).unwrap(),
			"/r/rust/hot.json?limit=25&raw_json=1"
		);
		assert!(validated_reddit_redirect_path(&tor_location, RedditLane::Direct).is_err());
		assert!(validated_reddit_redirect_path("https://www.reddit.com/r/rust", RedditLane::Tor).is_err());
	}

	#[test]
	fn test_upstream_guard_opens_after_burst_and_recovers() {
		let now = Instant::now();
		let mut guard = UpstreamGuard::default();
		assert!(!guard.record_failure(now));
		assert!(!guard.record_failure(now + Duration::from_secs(1)));
		assert!(guard.record_failure(now + Duration::from_secs(2)));
		assert_eq!(
			guard.active_cooldown(now + Duration::from_secs(3)).map(|(_, reason)| reason),
			Some(CooldownReason::UpstreamFailures)
		);
		assert!(guard.active_cooldown(now + FAILURE_COOLDOWN + Duration::from_secs(6)).is_none());
		guard.reset_failure_window();
		assert_eq!(guard.failures_in_window, 0);
	}

	#[test]
	fn test_redirect_continuation_observes_new_cooldowns() {
		let now = Instant::now();
		let mut guard = UpstreamGuard::default();
		let redirecting = guard.begin_attempt(now).unwrap();
		let second_redirecting = guard.begin_attempt(now).unwrap();
		let denied = guard.begin_attempt(now).unwrap();
		let denial = guard.record_edge_throttle(now, denied, None);
		let probe_at = now + denial.delay + Duration::from_millis(1);
		assert_eq!(guard.redirect_cooldown(now, redirecting).map(|(_, reason)| reason), Some(CooldownReason::EdgeThrottle));
		assert_eq!(guard.redirect_cooldown(probe_at, redirecting).map(|(_, reason)| reason), Some(CooldownReason::EdgeThrottle));
		assert_eq!(
			guard.redirect_cooldown(probe_at, second_redirecting).map(|(_, reason)| reason),
			Some(CooldownReason::EdgeThrottle)
		);
		let recovery_probe = guard.begin_attempt(probe_at).unwrap();
		assert!(recovery_probe.half_open);
		assert_eq!(guard.redirect_cooldown(probe_at, redirecting).map(|(_, reason)| reason), Some(CooldownReason::EdgeThrottle));

		guard.edges[0].state = EdgeCircuitState::Closed;
		guard.edges[0].epoch = redirecting.epoch;
		guard.block_for_rate_limit(now, Duration::from_secs(20));
		assert_eq!(guard.redirect_cooldown(now, redirecting).map(|(_, reason)| reason), Some(CooldownReason::RateLimit));
	}

	#[test]
	fn test_edge_throttle_uses_one_probe_and_escalates_until_success() {
		let now = Instant::now();
		let mut guard = UpstreamGuard::default();
		let original = guard.begin_attempt(now).unwrap();
		let first = guard.record_edge_throttle(now, original, Some(Duration::from_secs(2)));
		assert!((Duration::from_secs(5)..=Duration::from_millis(6250)).contains(&first.delay));
		assert_eq!(first.consecutive_failures, 1);
		assert!(first.started_cooldown);
		let concurrent = guard.record_edge_throttle(now + Duration::from_secs(1), original, Some(Duration::from_secs(2)));
		assert_eq!(concurrent.consecutive_failures, 1);
		assert!(!concurrent.started_cooldown);
		assert_eq!(concurrent.delay, first.delay - Duration::from_secs(1));

		let first_probe_at = now + first.delay + Duration::from_millis(1);
		let probe = guard.begin_attempt(first_probe_at).unwrap();
		assert!(probe.half_open);
		assert!(guard.begin_attempt(first_probe_at).is_err());
		let second = guard.record_edge_throttle(first_probe_at, probe, Some(Duration::from_secs(2)));
		assert!((Duration::from_secs(10)..=Duration::from_millis(12_500)).contains(&second.delay));
		assert_eq!(second.consecutive_failures, 2);
		assert!(second.started_cooldown);

		let recovery_at = first_probe_at + second.delay + Duration::from_millis(1);
		let recovery_probe = guard.begin_attempt(recovery_at).unwrap();
		let recovery = guard.record_api_success(recovery_at, recovery_probe).unwrap();
		assert_eq!(recovery.consecutive_failures, 2);
		assert!(recovery.episode_seconds >= 15);
		let stale = guard.record_edge_throttle(recovery_at, original, None);
		assert!(!stale.started_cooldown);
		assert!(guard.edges[0].episode_started_at.is_none());
		let recovered_attempt = guard.begin_attempt(recovery_at).unwrap();
		let recovered = guard.record_edge_throttle(recovery_at, recovered_attempt, Some(Duration::from_secs(2)));
		assert!((Duration::from_secs(5)..=Duration::from_millis(6250)).contains(&recovered.delay));
		assert_eq!(recovered.consecutive_failures, 1);
		assert_eq!(recovered.episode_seconds, 0);
	}

	#[test]
	fn test_edge_fallback_is_lane_isolated_and_never_bypasses_quota() {
		let now = Instant::now();
		let mut direct = UpstreamGuard::new(RedditLane::Direct);
		let mut tor = UpstreamGuard::new(RedditLane::Tor);
		direct.install_oauth_generation(7, true);
		tor.install_oauth_generation(7, true);

		let direct_attempt = direct.begin_attempt(now).unwrap();
		direct.record_edge_throttle(now, direct_attempt, Some(Duration::ZERO));
		assert!(edge_fallback_active(&direct, now, 0));
		assert!(!edge_fallback_active(&tor, now, 0));
		assert!(tor.begin_attempt(now).is_ok());

		direct.block_for_rate_limit(now, Duration::from_secs(30));
		assert!(!edge_fallback_active(&direct, now, 0));
		assert_eq!(direct.active_cooldown(now).map(|(_, reason)| reason), Some(CooldownReason::RateLimit));
	}

	#[test]
	fn test_tor_retry_policy_distinguishes_edge_and_quota_spillover() {
		use LaneAvailability::Ready;
		let ticket = QuotaSpilloverTicket {
			budget: QuotaBudget::Rest,
			generation: 7,
			quota_epoch: 2,
		};
		assert!(should_retry_on_tor(RedditLane::Direct, TorRetryReason::EdgeRejected, true, true, Ready, false, false,));
		assert!(!should_retry_on_tor(RedditLane::Direct, TorRetryReason::EdgeRejected, true, false, Ready, false, false,));
		assert!(!should_retry_on_tor(RedditLane::Direct, TorRetryReason::EdgeRejected, false, true, Ready, false, false,));
		assert!(!should_retry_on_tor(RedditLane::Tor, TorRetryReason::EdgeRejected, true, true, Ready, false, false,));
		assert!(should_retry_on_tor(
			RedditLane::Direct,
			TorRetryReason::QuotaReserve(ticket),
			true,
			false,
			Ready,
			false,
			true,
		));
		assert!(!should_retry_on_tor(
			RedditLane::Direct,
			TorRetryReason::QuotaReserve(ticket),
			true,
			false,
			Ready,
			true,
			true,
		));
		assert!(!should_retry_on_tor(
			RedditLane::Direct,
			TorRetryReason::QuotaReserve(ticket),
			false,
			false,
			Ready,
			false,
			true,
		));
		assert!(!should_retry_on_tor(RedditLane::Tor, TorRetryReason::QuotaReserve(ticket), true, false, Ready, false, true,));
		assert!(!should_retry_on_tor(RedditLane::Direct, TorRetryReason::None, true, true, Ready, false, true,));
	}

	#[test]
	fn test_tor_serves_requests_while_direct_oauth_is_starting() {
		use LaneAvailability::{Ready, Unavailable};
		assert_eq!(select_api_lane(RedditLane::Direct, Unavailable, Ready), RedditLane::Tor);
		assert_eq!(select_api_lane(RedditLane::Direct, Unavailable, Unavailable), RedditLane::Direct);
		assert_eq!(select_api_lane(RedditLane::Direct, Ready, Ready), RedditLane::Direct);
		assert_eq!(select_api_lane(RedditLane::Direct, Ready, Unavailable), RedditLane::Direct);
	}

	#[test]
	fn test_share_link_resolution_uses_lane_specific_origins() {
		assert_eq!(
			canonical_head_origins(RedditLane::Direct),
			[
				Some((ALTERNATIVE_REDDIT_URL_BASE, ALTERNATIVE_REDDIT_URL_BASE_HOST)),
				Some((REDDIT_SHORT_URL_BASE, REDDIT_SHORT_URL_BASE_HOST)),
			]
		);
		let tor_origin = RedditLane::Tor.auth_origin();
		assert_eq!(canonical_head_origins(RedditLane::Tor), [Some((tor_origin.base, tor_origin.host)), None]);
	}

	#[test]
	fn test_share_link_resolution_retries_only_direct_edge_failures_on_ready_tor() {
		assert!(canonical_head_is_edge_rejected(403, true, false));
		assert!(!canonical_head_is_edge_rejected(403, true, true));
		assert!(!canonical_head_is_edge_rejected(403, false, false));
		assert!(should_retry_canonical_on_tor(RedditLane::Direct, true));
		assert!(!should_retry_canonical_on_tor(RedditLane::Direct, false));
		assert!(!should_retry_canonical_on_tor(RedditLane::Tor, true));
	}

	#[test]
	fn direct_oauth_validation_never_uses_tor_lane() {
		assert_eq!(oauth_startup_validation_lane(), RedditLane::Direct);
	}

	#[test]
	fn test_only_edge_redirect_deferrals_qualify_for_tor_retry() {
		let deferred = |reason| ApiRequestError::deferred("deferred".to_string(), reason, Duration::from_secs(1));
		assert!(matches!(deferred(CooldownReason::EdgeThrottle), ApiRequestError::Deferred { edge_rejected: true, .. }));
		assert!(matches!(deferred(CooldownReason::RateLimit), ApiRequestError::Deferred { edge_rejected: false, .. }));
		assert!(matches!(deferred(CooldownReason::UpstreamFailures), ApiRequestError::Deferred { edge_rejected: false, .. }));
		assert!(
			matches!(deferred(CooldownReason::RateLimit), ApiRequestError::Deferred { recovery: LaneRecoveryReason::QuotaCooldown(delay), .. } if delay == Duration::from_secs(1))
		);
		assert!(
			matches!(deferred(CooldownReason::UpstreamFailures), ApiRequestError::Deferred { recovery: LaneRecoveryReason::UpstreamCooldown(delay), .. } if delay == Duration::from_secs(1))
		);
	}

	#[test]
	fn test_equal_numbered_quota_tickets_remain_lane_scoped() {
		let now = Instant::now();
		let guard = |lane| UpstreamGuard {
			lane,
			quota: QuotaGovernor {
				generation: 7,
				epoch: 3,
				next_request_id: 0,
				outstanding: 0,
				rollover_reserve: 0,
				headerless_consumption: 0,
				window: QuotaWindow::Known {
					available: LOW_RATE_LIMIT_THRESHOLD - 1,
					reset_at: now + QUOTA_ROTATION_MIN_RESET_REMAINING + Duration::from_secs(1),
				},
			},
			quota_rotation_armed: true,
			..UpstreamGuard::new(lane)
		};
		let direct_ticket = guard(RedditLane::Direct).quota_rotation_candidate(now, 7, QuotaRotationMode::Proactive).unwrap();
		let tor_ticket = guard(RedditLane::Tor).quota_rotation_candidate(now, 7, QuotaRotationMode::Proactive).unwrap();
		assert_eq!(direct_ticket.generation, tor_ticket.generation);
		assert_eq!(direct_ticket.quota_epoch, tor_ticket.quota_epoch);
		assert_ne!(direct_ticket.lane, tor_ticket.lane);
	}

	#[test]
	fn test_oauth_refresh_preserves_all_cooldowns() {
		let now = Instant::now();
		let mut guard = UpstreamGuard::default();
		let attempt = guard.begin_attempt(now).unwrap();
		guard.record_edge_throttle(now, attempt, None);
		guard.block_for_rate_limit(now, Duration::from_secs(20));
		guard.install_oauth_generation(2, false);
		assert_eq!(guard.active_cooldown(now).map(|(_, reason)| reason), Some(CooldownReason::RateLimit));
		assert_eq!(guard.edges[0].failures, 1);
		assert_eq!(guard.quota.generation, 2);
	}

	#[test]
	fn test_fresh_identity_clears_only_quota_cooldown() {
		let now = Instant::now();
		let mut guard = UpstreamGuard::default();
		let attempt = guard.begin_attempt(now).unwrap();
		guard.record_edge_throttle(now, attempt, None);
		guard.block_for_rate_limit(now, Duration::from_secs(20));
		let failure_deadline = now + Duration::from_secs(30);
		guard.upstream_failure_blocked_until = Some(failure_deadline);

		guard.install_oauth_generation(2, true);

		assert!(guard.rate_limit_blocked_until.is_none());
		assert_eq!(guard.active_cooldown(now).map(|(_, reason)| reason), Some(CooldownReason::UpstreamFailures));
		assert_eq!(guard.edges[0].failures, 1);
		assert_eq!(guard.upstream_failure_blocked_until, Some(failure_deadline));
		assert_eq!(guard.quota.generation, 2);
	}

	#[test]
	fn test_response_started_before_edge_denial_cannot_close_circuit() {
		let now = Instant::now();
		let mut guard = UpstreamGuard::default();
		let denied = guard.begin_attempt(now).unwrap();
		let late_success = guard.begin_attempt(now).unwrap();
		guard.record_edge_throttle(now, denied, None);
		assert!(guard.record_api_success(now, late_success).is_none());
		assert!(matches!(guard.edges[0].state, EdgeCircuitState::Open { .. }));
		assert_eq!(guard.edges[0].failures, 1);
	}

	#[test]
	fn test_abandoned_half_open_probe_reopens_edge_circuit() {
		let now = Instant::now();
		let mut guard = UpstreamGuard::default();
		let attempt = guard.begin_attempt(now).unwrap();
		let denial = guard.record_edge_throttle(now, attempt, None);
		let probe_at = now + denial.delay + Duration::from_millis(1);
		let probe = guard.begin_attempt(probe_at).unwrap();
		guard.abandon_edge_probe(probe_at, probe);
		assert!(matches!(guard.edges[0].state, EdgeCircuitState::Open { .. }));
		assert!(guard.begin_attempt(probe_at + Duration::from_secs(1)).is_err());
	}

	#[test]
	fn test_expired_half_open_probe_cannot_block_or_recover_circuit() {
		let now = Instant::now();
		let mut guard = UpstreamGuard::default();
		let attempt = guard.begin_attempt(now).unwrap();
		let denial = guard.record_edge_throttle(now, attempt, None);
		let stale_probe_at = now + denial.delay + Duration::from_millis(1);
		let stale_probe = guard.begin_attempt(stale_probe_at).unwrap();
		let replacement_probe = guard.begin_attempt(stale_probe_at + RedditLane::Direct.request_timeout() + Duration::from_secs(1)).unwrap();
		assert!(replacement_probe.half_open);
		assert!(guard.record_api_success(stale_probe_at, stale_probe).is_none());
		assert!(matches!(guard.edges[0].state, EdgeCircuitState::HalfOpen { .. }));
		assert!(guard
			.record_api_success(stale_probe_at + RedditLane::Direct.request_timeout() + Duration::from_secs(1), replacement_probe)
			.is_some());
		assert!(matches!(guard.edges[0].state, EdgeCircuitState::Closed));
	}

	#[test]
	fn test_api_path_normalization_is_conservative_and_deterministic() {
		assert_eq!(
			normalize_reddit_api_path("/r/rust/hot.json?utm_source=test&after=t3_abc&raw_json=0&sort=new&share_id=secret&raw_json=1"),
			"/r/rust/hot.json?after=t3_abc&raw_json=1&sort=new"
		);
		assert_eq!(
			normalize_reddit_api_path("/r/rust/hot.json?sort=new&after=t3_abc"),
			normalize_reddit_api_path("/r/rust/hot.json?after=t3_abc&sort=new")
		);
		let preserved = normalize_reddit_api_path("/comments/abc.json?context=3&q=a%2Bb&cache_bust=unique");
		assert!(preserved.contains("context=3"));
		assert!(!preserved.contains("q="));
		assert!(!preserved.contains("cache_bust"));
		assert_ne!(
			normalize_reddit_api_path("/comments/abc/title.json?sort=top"),
			normalize_reddit_api_path("/comments/abc/title.json?sort=new")
		);
		assert_eq!(
			normalize_reddit_api_path("/r/rust/comments/abc/a-title.json?sort=top&nonce=one"),
			normalize_reddit_api_path("/r/rust/comments/abc/different-title.json?nonce=two&sort=top")
		);
		assert_eq!(
			normalize_reddit_api_path("/user/example/comments/abc/a-title/def.json?context=03&context=3"),
			"/user/example/comments/abc/_/def.json?context=3&raw_json=1"
		);
		assert_eq!(
			normalize_reddit_api_path("/comments/abc/title.json?sort=invalid&context=10000"),
			"/comments/abc.json?raw_json=1"
		);
		assert_eq!(
			normalize_reddit_api_path("/comments/abc/title.json?limit=025&depth=03&sort=new&sort=top"),
			"/comments/abc.json?depth=3&limit=25&raw_json=1&sort=top"
		);
		assert_eq!(
			normalize_reddit_api_path("/comments/abc/title.json?sort=top&sort=invalid&showmedia=nonce&theme=light&truncate=51"),
			"/comments/abc.json?raw_json=1"
		);
		assert_eq!(
			normalize_reddit_api_path("/r/rust/comments/abc/title/def/.json?context=3"),
			"/r/rust/comments/abc/_/def.json?context=3&raw_json=1"
		);
		assert_ne!(
			normalize_reddit_api_path("/r/rust/comments/abc/title/def.json"),
			normalize_reddit_api_path("/r/rust/comments/abc/title/ghi.json")
		);
		assert_ne!(
			normalize_reddit_api_path("/r/rust/comments/abc/title.json"),
			normalize_reddit_api_path("/user/rust/comments/abc/title.json")
		);
		assert_eq!(
			normalize_reddit_api_path("/comments/abc/title/def/extra.json?nonce=one"),
			"/comments/abc/title/def/extra.json?nonce=one&raw_json=1"
		);
		assert!(normalize_reddit_api_path("/search.json?q=rust&sort=new").contains("q=rust"));
	}

	#[test]
	fn test_json_cache_policy_is_narrow() {
		assert_eq!(json_cache_policy("/r/rust/about.json?raw_json=1"), JsonCachePolicy::Metadata);
		assert_eq!(json_cache_policy("/r/rust/wiki/index.json?raw_json=1"), JsonCachePolicy::Metadata);
		assert_eq!(json_cache_policy("/subreddits/search.json?q=rust&raw_json=1"), JsonCachePolicy::Metadata);
		assert_eq!(json_cache_policy("/comments/abc.json?raw_json=1"), JsonCachePolicy::Comments);
		assert_eq!(json_cache_policy("/r/rust/comments/abc/title.json?raw_json=1"), JsonCachePolicy::Comments);
		assert_eq!(json_cache_policy("/user/example/comments/abc/title/def.json?raw_json=1"), JsonCachePolicy::Comments);
		assert_eq!(json_cache_policy("/r/rust/comments/abc/title/def/.json?raw_json=1"), JsonCachePolicy::Comments);
		assert_eq!(json_cache_policy("/r/rust/hot.json?raw_json=1"), JsonCachePolicy::Dynamic);
		assert_eq!(json_cache_policy("/comments/about.json?raw_json=1"), JsonCachePolicy::Comments);
		assert_eq!(json_cache_policy("/r/random/about.json?raw_json=1"), JsonCachePolicy::Dynamic);
		assert_eq!(json_cache_policy("/user/example/comments.json?raw_json=1"), JsonCachePolicy::Dynamic);
		assert_eq!(json_cache_policy("/comments/abc/title/def/extra.json?raw_json=1"), JsonCachePolicy::Dynamic);
	}

	#[test]
	fn test_endpoint_class_does_not_log_resource_names() {
		assert_eq!(endpoint_class("/r/example/hot.json?raw_json=1"), "subreddit");
		assert_eq!(endpoint_class("/r/example/comments/abc/title.json"), "comments");
		assert_eq!(endpoint_class("/user/example/about.json"), "user");
		assert_eq!(endpoint_class("/search.json?q=private"), "search");
		assert_eq!(endpoint_class("/subreddits/search.json?q=private"), "community_search");
	}

	#[tokio::test(flavor = "multi_thread")]
	#[ignore = "requires application-managed live OAuth startup"]
	async fn test_rate_limit_check() {
		rate_limit_check().await.unwrap();
	}

	#[test]
	#[sealed_test(env = [("REDLIB_DEFAULT_SUBSCRIPTIONS", "rust")])]
	fn test_default_subscriptions() {
		let subscriptions = get_setting("REDLIB_DEFAULT_SUBSCRIPTIONS");
		assert!(subscriptions.is_some());
	}

	#[tokio::test(flavor = "multi_thread")]
	async fn test_localization_popular() {
		let val = json(POPULAR_URL.to_string(), false).await.unwrap();
		assert_eq!("GLOBAL", val["data"]["geo_filter"].as_str().unwrap());
	}

	#[tokio::test(flavor = "multi_thread")]
	async fn test_obfuscated_share_link() {
		let share_link = "/r/rust/s/kPgq8WNHRK".into();
		// Correct link without share parameters
		let canonical_link = "/r/rust/comments/18t5968/why_use_tuple_struct_over_standard_struct/kfbqlbc/".into();
		assert_eq!(canonical_path(share_link, 3).await, Ok(Some(canonical_link)));
	}

	#[tokio::test(flavor = "multi_thread")]
	async fn test_private_sub() {
		let link = json("/r/suicide/about.json?raw_json=1".into(), true).await;
		assert!(link.is_err());
		assert_eq!(link, Err("private".into()));
	}

	#[tokio::test(flavor = "multi_thread")]
	async fn test_banned_sub() {
		let link = json("/r/aaa/about.json?raw_json=1".into(), true).await;
		assert!(link.is_err());
		assert_eq!(link, Err("banned".into()));
	}

	#[tokio::test(flavor = "multi_thread")]
	async fn test_gated_sub() {
		// quarantine to false to specifically catch when we _don't_ catch it
		let link = json("/r/drugs/about.json?raw_json=1".into(), false).await;
		assert!(link.is_err());
		assert_eq!(link, Err("gated".into()));
	}
}
