use crate::dbg_msg;
use crate::oauth::{force_refresh_token, quota_rotation_in_progress, spawn_rate_limit_refresh, token_daemon, Oauth, OauthBackendImpl, RefreshReason};
use crate::reddit_lane::{RedditLane, TOR_FALLBACK_CONFIG};
use crate::server::RequestExt;
use crate::timing::{positive_jitter, proportional_positive_jitter};
use crate::utils::{format_url, Post};
use arc_swap::{ArcSwap, ArcSwapOption};
use cached::proc_macro::cached;
use futures_lite::future::block_on;
use futures_lite::{future::Boxed, FutureExt};
use hyper::{body::Buf, header, Body, Request as HyperRequest, Response as HyperResponse};
use log::{error, info, trace, warn};
use percent_encoding::{percent_encode, CONTROLS};
use serde_json::Value;
use std::collections::HashSet;
use std::env;
use std::result::Result;
use std::sync::atomic::Ordering;
use std::sync::atomic::{AtomicBool, AtomicU64};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, Instant, SystemTime};
use tokio::sync::Semaphore;
use wreq::redirect::Policy;
use wreq::{header as wreq_header, Client as WreqClient, EmulationFactory, Method, Proxy, Response as WreqResponse};
use wreq_util::{Emulation, EmulationOS, EmulationOption};

const REDDIT_URL_BASE: &str = "https://oauth.reddit.com";
const REDDIT_URL_BASE_HOST: &str = "oauth.reddit.com";

const REDDIT_SHORT_URL_BASE: &str = "https://redd.it";
const REDDIT_SHORT_URL_BASE_HOST: &str = "redd.it";

const ALTERNATIVE_REDDIT_URL_BASE: &str = "https://www.reddit.com";
const ALTERNATIVE_REDDIT_URL_BASE_HOST: &str = "www.reddit.com";

pub static CLIENT: LazyLock<WreqClient> = LazyLock::new(build_client);

static TOR_CLIENT: LazyLock<Result<WreqClient, String>> = LazyLock::new(build_tor_client);

pub static OAUTH_CLIENT: LazyLock<ArcSwap<Oauth>> = LazyLock::new(|| {
	let client = block_on(Oauth::new(RedditLane::Direct));
	tokio::spawn(token_daemon(RedditLane::Direct));
	ArcSwap::new(client.into())
});

pub(crate) static TOR_OAUTH_CLIENT: LazyLock<ArcSwapOption<Oauth>> = LazyLock::new(ArcSwapOption::empty);

pub static OAUTH_IS_ROLLING_OVER: AtomicBool = AtomicBool::new(false);
pub(crate) static TOR_OAUTH_IS_ROLLING_OVER: AtomicBool = AtomicBool::new(false);
static TOR_WARMUP_STARTED: AtomicBool = AtomicBool::new(false);

const DEFAULT_MAX_CONCURRENT_API_REQUESTS: usize = 8;
const MAX_CONFIGURED_API_REQUESTS: usize = 64;
const FAILURE_WINDOW: Duration = Duration::from_secs(10);
const FAILURE_COOLDOWN: Duration = Duration::from_secs(10);
const FAILURE_THRESHOLD: u8 = 3;
const DEFAULT_RATE_LIMIT_COOLDOWN: Duration = Duration::from_secs(10);
const MAX_RATE_LIMIT_COOLDOWN: Duration = Duration::from_secs(600);
const RATE_LIMIT_COOLDOWN_MARGIN: Duration = Duration::from_secs(2);
const LOW_RATE_LIMIT_THRESHOLD: u16 = 10;
const QUOTA_ROTATION_MIN_RESET_REMAINING: Duration = Duration::from_secs(120);
const EMERGENCY_QUOTA_ROTATION_MIN_RESET_REMAINING: Duration = Duration::from_secs(30);
const QUOTA_SAFETY_RESERVE: u16 = 5;
const QUOTA_UNKNOWN_RETRY: Duration = Duration::from_secs(5);
const EMERGENCY_QUOTA_REFRESH_RETRY: Duration = Duration::from_secs(2);
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
static LAST_TRAFFIC_SUMMARY: LazyLock<Mutex<Instant>> = LazyLock::new(|| Mutex::new(Instant::now()));

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
			upstream_guard(self.lane).abandon_attempt(Instant::now(), self);
		}
	}
}

#[derive(Debug, Clone, Copy)]
enum QuotaWindow {
	Unknown { not_before: Instant, probe_in_flight: bool },
	Unreported,
	Known { available: u16, reset_at: Instant },
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
enum QuotaReserveError {
	Deferred(Duration),
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
			window: QuotaWindow::Unknown {
				not_before: Instant::now(),
				probe_in_flight: false,
			},
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
			self.window = QuotaWindow::Unknown {
				not_before: Instant::now(),
				probe_in_flight: false,
			};
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
			self.window = QuotaWindow::Unknown {
				not_before: now,
				probe_in_flight: false,
			};
		}

		let discovery_probe = match &mut self.window {
			QuotaWindow::Unknown { not_before, probe_in_flight } => {
				if *probe_in_flight {
					return Err(QuotaReserveError::Deferred(QUOTA_UNKNOWN_RETRY));
				}
				if now < *not_before {
					let delay = (*not_before).duration_since(now);
					return Err(QuotaReserveError::Deferred(delay.max(Duration::from_secs(1))));
				}
				*probe_in_flight = true;
				true
			}
			QuotaWindow::Unreported => false,
			QuotaWindow::Known { available, reset_at } => {
				if *available <= QUOTA_SAFETY_RESERVE {
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

	fn reconcile(&mut self, now: Instant, attempt: &UpstreamAttempt, remaining: Option<u16>, reset: Option<Duration>, quota_exhausted: bool) -> bool {
		if attempt.quota_epoch != self.epoch {
			return false;
		}
		self.outstanding = self.outstanding.saturating_sub(1);
		if attempt.generation != self.generation {
			if attempt.discovery_probe && matches!(self.window, QuotaWindow::Unknown { .. }) {
				self.window = QuotaWindow::Unknown {
					not_before: now + proportional_positive_jitter(QUOTA_UNKNOWN_RETRY),
					probe_in_flight: false,
				};
			}
			return false;
		}

		let reset_at = reset.map(|delay| now + delay.min(MAX_RATE_LIMIT_COOLDOWN));
		if quota_exhausted {
			self.window = QuotaWindow::Known {
				available: 0,
				reset_at: reset_at.unwrap_or(now + DEFAULT_RATE_LIMIT_COOLDOWN),
			};
			return true;
		}

		if let (QuotaWindow::Known { reset_at: known_reset, .. }, Some(remaining), Some(observed_reset)) = (&self.window, remaining, reset_at) {
			if now >= *known_reset && observed_reset > *known_reset + RATE_LIMIT_COOLDOWN_MARGIN {
				let unresolved = self.outstanding.saturating_add(self.rollover_reserve);
				self.epoch = self.epoch.wrapping_add(1);
				self.outstanding = 0;
				self.rollover_reserve = 0;
				self.window = QuotaWindow::Known {
					available: remaining.saturating_sub(unresolved),
					reset_at: observed_reset,
				};
				return true;
			}
		}

		match (&mut self.window, remaining) {
			(QuotaWindow::Unknown { .. }, Some(remaining)) => {
				self.window = QuotaWindow::Known {
					available: remaining.saturating_sub(self.outstanding).saturating_sub(self.rollover_reserve),
					reset_at: reset_at.unwrap_or(now + MAX_RATE_LIMIT_COOLDOWN),
				};
				self.rollover_reserve = 0;
			}
			(QuotaWindow::Unknown { .. }, None) => {
				self.window = QuotaWindow::Unknown {
					not_before: now + proportional_positive_jitter(QUOTA_UNKNOWN_RETRY),
					probe_in_flight: false,
				};
			}
			(QuotaWindow::Unreported, Some(remaining)) => {
				self.window = QuotaWindow::Known {
					available: remaining.saturating_sub(self.outstanding).saturating_sub(self.rollover_reserve),
					reset_at: reset_at.unwrap_or(now + MAX_RATE_LIMIT_COOLDOWN),
				};
				self.rollover_reserve = 0;
			}
			(QuotaWindow::Unreported, None) => {}
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
			self.rollover_reserve = 0;
			self.window = QuotaWindow::Unreported;
		}
	}

	fn continue_headerless_discovery_after_redirect(&mut self, now: Instant, attempt: &UpstreamAttempt) {
		if attempt.discovery_probe && attempt.generation == self.generation && attempt.quota_epoch == self.epoch && matches!(self.window, QuotaWindow::Unknown { .. }) {
			// The redirect response is accounted for, but it is not evidence that
			// the final JSON endpoint omits quota headers. Keep the discovery chain
			// exclusive and let its next hop inherit probe ownership immediately.
			self.window = QuotaWindow::Unknown {
				not_before: now,
				probe_in_flight: false,
			};
		}
	}

	fn abandon(&mut self, now: Instant, attempt: &UpstreamAttempt) {
		if attempt.quota_reconciled || attempt.quota_epoch != self.epoch {
			return;
		}
		self.outstanding = self.outstanding.saturating_sub(1);
		match &mut self.window {
			QuotaWindow::Known { available, .. } if !attempt.sent => {
				*available = available.saturating_add(1);
			}
			QuotaWindow::Unknown { .. } => {
				self.window = QuotaWindow::Unknown {
					not_before: now + proportional_positive_jitter(QUOTA_UNKNOWN_RETRY),
					probe_in_flight: false,
				};
			}
			QuotaWindow::Unreported => {}
			QuotaWindow::Known { .. } => {}
		}
	}
}

#[derive(Debug)]
struct UpstreamGuard {
	lane: RedditLane,
	quota: QuotaGovernor,
	quota_rotation_armed: bool,
	quota_wait_logged_epoch: Option<u64>,
	emergency_rotation_claimed_epoch: Option<u64>,
	failure_window_started: Option<Instant>,
	failures_in_window: u8,
	upstream_failure_blocked_until: Option<Instant>,
	rate_limit_blocked_until: Option<Instant>,
	edge_throttle_failures: u8,
	edge_epoch: u64,
	edge_state: EdgeCircuitState,
	edge_episode_started_at: Option<Instant>,
	identity_installed_at: Instant,
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
			quota_rotation_armed: false,
			quota_wait_logged_epoch: None,
			emergency_rotation_claimed_epoch: None,
			failure_window_started: None,
			failures_in_window: 0,
			upstream_failure_blocked_until: None,
			rate_limit_blocked_until: None,
			edge_throttle_failures: 0,
			edge_epoch: 0,
			edge_state: EdgeCircuitState::Closed,
			edge_episode_started_at: None,
			identity_installed_at: Instant::now(),
		}
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
		if fresh_identity {
			self.quota_rotation_armed = false;
			self.quota_wait_logged_epoch = None;
			self.emergency_rotation_claimed_epoch = None;
			self.rate_limit_blocked_until = None;
			self.identity_installed_at = Instant::now();
		}
	}

	fn quota_rotation_allowed(&self, now: Instant, generation: u64) -> bool {
		if !self.quota_rotation_armed || generation != self.quota.generation {
			return false;
		}
		if self.rate_limit_blocked_until.is_some_and(|deadline| deadline > now)
			|| self.upstream_failure_blocked_until.is_some_and(|deadline| deadline > now)
			|| !matches!(self.edge_state, EdgeCircuitState::Closed)
		{
			return false;
		}
		true
	}

	fn quota_rotation_window_matches(&self, now: Instant, mode: QuotaRotationMode) -> bool {
		let QuotaWindow::Known { available, reset_at } = self.quota.window else {
			return false;
		};
		let Some(remaining) = reset_at.checked_duration_since(now) else {
			return false;
		};
		match mode {
			QuotaRotationMode::Proactive => available < LOW_RATE_LIMIT_THRESHOLD && remaining > QUOTA_ROTATION_MIN_RESET_REMAINING,
			QuotaRotationMode::Emergency => available <= QUOTA_SAFETY_RESERVE && remaining > EMERGENCY_QUOTA_ROTATION_MIN_RESET_REMAINING,
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

	fn take_short_reset_notice(&mut self, now: Instant, generation: u64) -> Option<(u16, Duration)> {
		if !self.quota_rotation_armed || generation != self.quota.generation || self.quota_wait_logged_epoch == Some(self.quota.epoch) {
			return None;
		}
		let QuotaWindow::Known { available, reset_at } = self.quota.window else {
			return None;
		};
		let reset_remaining = reset_at.saturating_duration_since(now);
		if available >= LOW_RATE_LIMIT_THRESHOLD || reset_remaining > QUOTA_ROTATION_MIN_RESET_REMAINING {
			return None;
		}
		self.quota_wait_logged_epoch = Some(self.quota.epoch);
		Some((available, reset_remaining))
	}

	fn try_admit(&mut self, now: Instant, generation: u64) -> Result<UpstreamAttempt, AdmissionDenied> {
		if let Some((delay, reason)) = self.active_cooldown(now) {
			return Err(AdmissionDenied {
				delay,
				reason,
				reserve_exhausted: false,
			});
		}

		let (quota_epoch, request_id, discovery_probe) = self.quota.reserve(now, generation).map_err(|error| match error {
			QuotaReserveError::Deferred(delay) => AdmissionDenied {
				delay,
				reason: CooldownReason::RateLimit,
				reserve_exhausted: false,
			},
			QuotaReserveError::ReserveExhausted(delay) => AdmissionDenied {
				delay,
				reason: CooldownReason::RateLimit,
				reserve_exhausted: true,
			},
			QuotaReserveError::StaleGeneration => AdmissionDenied {
				delay: Duration::from_secs(1),
				reason: CooldownReason::RateLimit,
				reserve_exhausted: false,
			},
		})?;
		let edge = match self.begin_attempt(now) {
			Ok(edge) => edge,
			Err(error) => {
				let mut placeholder = UpstreamAttempt {
					lane: self.lane,
					edge: EdgeAttempt {
						epoch: self.edge_epoch,
						half_open: false,
					},
					generation,
					quota_epoch,
					request_id,
					discovery_probe,
					sent: false,
					quota_reconciled: false,
					completed: true,
				};
				self.quota.abandon(now, &placeholder);
				placeholder.quota_reconciled = true;
				return Err(AdmissionDenied {
					delay: error.0,
					reason: error.1,
					reserve_exhausted: false,
				});
			}
		};

		Ok(UpstreamAttempt {
			lane: self.lane,
			edge,
			generation,
			quota_epoch,
			request_id,
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
		let applied = self.quota.reconcile(now, attempt, remaining, reset, quota_exhausted);
		if applied && !quota_exhausted && remaining.is_some_and(|remaining| remaining >= LOW_RATE_LIMIT_THRESHOLD) {
			self.quota_rotation_armed = true;
		}
		attempt.quota_reconciled = true;
	}

	fn abandon_attempt(&mut self, now: Instant, attempt: &UpstreamAttempt) {
		self.quota.abandon(now, attempt);
		if attempt.edge.half_open && !attempt.completed {
			self.abandon_edge_probe(now, attempt.edge);
		}
	}

	fn active_cooldown(&self, now: Instant) -> Option<(Duration, CooldownReason)> {
		let mut active = None;
		let mut consider = |deadline: Option<Instant>, reason| {
			if let Some(remaining) = deadline.filter(|deadline| *deadline > now).map(|deadline| deadline.duration_since(now)) {
				if active.map_or(true, |(current, _)| remaining > current) {
					active = Some((remaining, reason));
				}
			}
		};

		consider(self.rate_limit_blocked_until, CooldownReason::RateLimit);
		consider(self.upstream_failure_blocked_until, CooldownReason::UpstreamFailures);
		match self.edge_state {
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
		consider(self.rate_limit_blocked_until, CooldownReason::RateLimit);
		consider(self.upstream_failure_blocked_until, CooldownReason::UpstreamFailures);
		match self.edge_state {
			EdgeCircuitState::Closed if attempt.epoch == self.edge_epoch => {}
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

	fn begin_attempt(&mut self, now: Instant) -> Result<EdgeAttempt, (Duration, CooldownReason)> {
		if matches!(self.edge_state, EdgeCircuitState::HalfOpen { expires_at, .. } if expires_at <= now) {
			self.edge_epoch = self.edge_epoch.wrapping_add(1);
			self.edge_state = EdgeCircuitState::HalfOpen {
				epoch: self.edge_epoch,
				expires_at: now + self.lane.request_timeout(),
			};
			return Ok(EdgeAttempt {
				epoch: self.edge_epoch,
				half_open: true,
			});
		}
		if let Some(active) = self.active_cooldown(now) {
			return Err(active);
		}

		match self.edge_state {
			EdgeCircuitState::Open { .. } => {
				self.edge_state = EdgeCircuitState::HalfOpen {
					epoch: self.edge_epoch,
					expires_at: now + self.lane.request_timeout(),
				};
				Ok(EdgeAttempt {
					epoch: self.edge_epoch,
					half_open: true,
				})
			}
			EdgeCircuitState::HalfOpen { expires_at, .. } => Err((expires_at.checked_duration_since(now).unwrap_or(Duration::from_secs(1)), CooldownReason::EdgeThrottle)),
			EdgeCircuitState::Closed => Ok(EdgeAttempt {
				epoch: self.edge_epoch,
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
		let closes_probe = matches!(self.edge_state, EdgeCircuitState::HalfOpen { epoch, .. } if epoch == attempt.epoch);
		let current_closed_attempt = matches!(self.edge_state, EdgeCircuitState::Closed) && attempt.epoch == self.edge_epoch;
		let recovery = closes_probe.then(|| EdgeRecovery {
			consecutive_failures: self.edge_throttle_failures,
			episode_seconds: self.edge_episode_started_at.map_or(0, |started| now.saturating_duration_since(started).as_secs()),
			current_generation: self.quota.generation,
			identity_age_seconds: now.saturating_duration_since(self.identity_installed_at).as_secs(),
		});
		if closes_probe {
			self.edge_throttle_failures = 0;
			self.edge_epoch = self.edge_epoch.wrapping_add(1);
			self.edge_state = EdgeCircuitState::Closed;
			self.edge_episode_started_at = None;
		} else if current_closed_attempt {
			self.edge_throttle_failures = 0;
			self.edge_episode_started_at = None;
		}
		recovery
	}

	fn record_edge_throttle(&mut self, now: Instant, attempt: EdgeAttempt, retry_after: Option<Duration>) -> EdgeThrottleDecision {
		let episode_seconds = self.edge_episode_started_at.map_or(0, |started| now.saturating_duration_since(started).as_secs());
		let identity_age_seconds = now.saturating_duration_since(self.identity_installed_at).as_secs();
		if attempt.epoch != self.edge_epoch {
			let delay = match self.edge_state {
				EdgeCircuitState::Open { until } => until.checked_duration_since(now).unwrap_or_default(),
				EdgeCircuitState::HalfOpen { .. } => Duration::from_secs(1),
				EdgeCircuitState::Closed => edge_throttle_base_delay(self.edge_throttle_failures.max(1), retry_after).0,
			};
			return EdgeThrottleDecision {
				delay,
				consecutive_failures: self.edge_throttle_failures,
				started_cooldown: false,
				episode_seconds,
				current_generation: self.quota.generation,
				identity_age_seconds,
			};
		}
		let episode_started_at = *self.edge_episode_started_at.get_or_insert(now);
		let episode_seconds = now.saturating_duration_since(episode_started_at).as_secs();

		self.edge_throttle_failures = self.edge_throttle_failures.saturating_add(1);
		let delay = edge_throttle_delay(self.edge_throttle_failures, retry_after);
		self.edge_epoch = self.edge_epoch.wrapping_add(1);
		self.edge_state = EdgeCircuitState::Open { until: now + delay };
		EdgeThrottleDecision {
			delay,
			consecutive_failures: self.edge_throttle_failures,
			started_cooldown: true,
			episode_seconds,
			current_generation: self.quota.generation,
			identity_age_seconds,
		}
	}

	fn abandon_edge_probe(&mut self, now: Instant, attempt: EdgeAttempt) {
		if attempt.half_open && matches!(self.edge_state, EdgeCircuitState::HalfOpen { epoch, .. } if epoch == attempt.epoch) {
			let delay = edge_throttle_delay(self.edge_throttle_failures.max(1), None);
			self.edge_state = EdgeCircuitState::Open { until: now + delay };
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
	let server_delay = match (parse_retry_after(retry_after, SystemTime::now()), parse_delay_seconds(reset)) {
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
	Deferred { message: String, edge_rejected: bool },
	Upstream(String),
}

impl ApiRequestError {
	fn deferred(message: String, reason: CooldownReason) -> Self {
		Self::Deferred {
			message,
			edge_rejected: reason == CooldownReason::EdgeThrottle,
		}
	}
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
		RedditLane::Direct => Some(OAUTH_CLIENT.load_full()),
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
		if ticket.lane != lane || !is_current_oauth_generation(lane, ticket.generation) || !guard.quota_rotation_still_needed(Instant::now(), ticket) {
			return false;
		}
	}
	match lane {
		RedditLane::Direct => {
			OAUTH_CLIENT.swap(oauth.into());
		}
		RedditLane::Tor => {
			TOR_OAUTH_CLIENT.swap(Some(oauth.into()));
		}
	}
	guard.install_oauth_generation(generation, fresh_identity);
	true
}

pub(crate) fn claim_quota_rotation(ticket: QuotaRotationTicket) -> bool {
	upstream_guard(ticket.lane).claim_quota_rotation(Instant::now(), ticket)
}

pub(crate) fn quota_rotation_still_needed(ticket: QuotaRotationTicket) -> bool {
	upstream_guard(ticket.lane).quota_rotation_still_needed(Instant::now(), ticket)
}

fn maybe_rotate_low_budget(lane: RedditLane, generation: u64, remaining: Option<u16>, used: Option<u16>, reset: Option<Duration>, path: &str) {
	let now = Instant::now();
	let mut guard = upstream_guard(lane);
	let rotation_ticket = guard.quota_rotation_candidate(now, generation, QuotaRotationMode::Proactive);
	let effective_quota = guard.known_quota_state(now, generation);
	let short_reset = rotation_ticket.is_none().then(|| guard.take_short_reset_notice(now, generation)).flatten();
	drop(guard);

	if let Some((available, reset_remaining)) = short_reset {
		info!(
			"Reddit request budget is low but resets soon: remaining={} effective_available={} used={} reset_seconds={} endpoint={}; preserving the current anonymous OAuth identity",
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
	LOGICAL_JSON_COUNTS[endpoint_class_index(path)].fetch_add(1, Ordering::Relaxed);
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
	if format.contains("v.redd.it") {
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
		"Reddit traffic summary (elapsed_seconds={}): inbound_routes={} inbound_methods={} inbound_status={} logical_json={} admitted_json={} api_sends={} api_lanes={} tor_fallbacks={} redirect_hops={} canonical_heads={} media_sends={} media_destinations={} media_results={} oauth_lanes={} local_denials={}",
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

fn api_concurrency(lane: RedditLane) -> &'static Semaphore {
	match lane {
		RedditLane::Direct => &DIRECT_REDDIT_API_CONCURRENCY,
		RedditLane::Tor => &TOR_REDDIT_API_CONCURRENCY,
	}
}

fn retry_after_seconds(duration: Duration) -> u64 {
	duration.as_secs().saturating_add(u64::from(duration.subsec_nanos() > 0)).max(1)
}

fn cooldown_error(lane: RedditLane) -> Option<(String, bool)> {
	let guard = upstream_guard(lane);
	let active = guard.active_cooldown(Instant::now());
	drop(guard);
	active.map(|(remaining, reason)| {
		record_local_denial(reason);
		let message = reason.message();
		(
			format!("{message}. Retry in {} seconds", retry_after_seconds(remaining)),
			reason == CooldownReason::EdgeThrottle,
		)
	})
}

fn tor_fallback_ready() -> bool {
	TOR_OAUTH_CLIENT.load().is_some() && client_for_lane(RedditLane::Tor).is_ok()
}

fn edge_fallback_active(guard: &UpstreamGuard, now: Instant) -> bool {
	if guard.rate_limit_blocked_until.is_some_and(|deadline| deadline > now) || guard.upstream_failure_blocked_until.is_some_and(|deadline| deadline > now) {
		return false;
	}
	match guard.edge_state {
		EdgeCircuitState::Open { until } => until > now,
		EdgeCircuitState::HalfOpen { expires_at, .. } => expires_at > now,
		EdgeCircuitState::Closed => false,
	}
}

fn direct_edge_fallback_active(now: Instant) -> bool {
	edge_fallback_active(&upstream_guard(RedditLane::Direct), now)
}

fn preferred_api_lane(now: Instant) -> RedditLane {
	if tor_fallback_ready() && direct_edge_fallback_active(now) {
		RedditLane::Tor
	} else {
		RedditLane::Direct
	}
}

fn begin_upstream_attempt(lane: RedditLane) -> Result<(Arc<Oauth>, UpstreamAttempt), (String, bool)> {
	let mut guard = upstream_guard(lane);
	let oauth_client = oauth_client(lane).ok_or_else(|| (format!("{} Reddit OAuth is not ready", lane.label()), false))?;
	let generation = oauth_client.generation;
	let now = Instant::now();
	let result = guard.try_admit(now, generation);
	let denied_quota_epoch = result.as_ref().err().filter(|denial| denial.reserve_exhausted).map(|_| guard.quota.epoch);
	let emergency_ticket = result
		.as_ref()
		.err()
		.filter(|denial| denial.reserve_exhausted)
		.and_then(|_| guard.quota_rotation_candidate(now, generation, QuotaRotationMode::Emergency));
	drop(guard);
	result.map(|attempt| (oauth_client, attempt)).map_err(|denial| {
		let emergency_started = emergency_ticket
			.filter(|_| is_current_oauth_generation(lane, generation))
			.is_some_and(spawn_rate_limit_refresh);
		let matching_refresh_in_progress = denied_quota_epoch.is_some_and(|quota_epoch| quota_rotation_in_progress(lane, generation, quota_epoch));
		let short_refresh_retry = emergency_started || matching_refresh_in_progress;
		if emergency_started {
			let reset_remaining = denial.delay.saturating_sub(RATE_LIMIT_COOLDOWN_MARGIN);
			warn!(
				"Local Reddit quota reserve reached with {} seconds left in the current window; rotating once to avoid a prolonged pause",
				reset_remaining.as_secs()
			);
		}
		record_local_denial(denial.reason);
		let message = if short_refresh_retry {
			format!(
				"Refreshing the anonymous Reddit session. Retry in {} seconds",
				retry_after_seconds(EMERGENCY_QUOTA_REFRESH_RETRY)
			)
		} else {
			format!("{}. Retry in {} seconds", denial.reason.message(), retry_after_seconds(denial.delay))
		};
		(message, denial.reason == CooldownReason::EdgeThrottle)
	})
}

fn block_for_rate_limit(lane: RedditLane, generation: u64, retry_after: Option<&str>, reset: Option<&str>) -> (Duration, bool) {
	let mut guard = upstream_guard(lane);
	if guard.quota.generation != generation {
		return (rate_limit_base_delay(retry_after, reset).0, false);
	}
	let duration = rate_limit_delay(retry_after, reset);
	guard.block_for_rate_limit(Instant::now(), duration);
	(duration, true)
}

fn reconcile_rate_limit(attempt: &mut UpstreamAttempt, remaining: Option<u16>, reset: Option<Duration>, quota_exhausted: bool) {
	upstream_guard(attempt.lane).reconcile_quota(Instant::now(), attempt, remaining, reset, quota_exhausted);
}

fn confirm_headerless_quota(attempt: &UpstreamAttempt) {
	upstream_guard(attempt.lane).quota.confirm_headerless_success(attempt);
}

fn reserve_redirect_hop(attempt: &mut UpstreamAttempt, generation: u64, remaining: Option<u16>, reset: Option<Duration>) -> Result<(), ApiRequestError> {
	let now = Instant::now();
	let mut guard = upstream_guard(attempt.lane);
	let continue_headerless_discovery = attempt.discovery_probe && remaining.is_none() && reset.is_none();
	guard.reconcile_quota(now, attempt, remaining, reset, false);
	if continue_headerless_discovery {
		guard.quota.continue_headerless_discovery_after_redirect(now, attempt);
	}
	if let Some((delay, reason)) = guard.redirect_cooldown(now, attempt.edge) {
		drop(guard);
		record_local_denial(reason);
		return Err(ApiRequestError::deferred(
			format!("{}. Retry in {} seconds", reason.message(), retry_after_seconds(delay)),
			reason,
		));
	}
	let (quota_epoch, request_id, discovery_probe) = match guard.quota.reserve(now, generation) {
		Ok(reservation) => reservation,
		Err(error) => {
			let (delay, denied_quota_epoch, emergency_ticket) = match error {
				QuotaReserveError::Deferred(delay) => (delay, None, None),
				QuotaReserveError::ReserveExhausted(delay) => (
					delay,
					Some(guard.quota.epoch),
					guard.quota_rotation_candidate(now, generation, QuotaRotationMode::Emergency),
				),
				QuotaReserveError::StaleGeneration => (Duration::from_secs(1), None, None),
			};
			drop(guard);
			let emergency_started = emergency_ticket
				.filter(|_| is_current_oauth_generation(attempt.lane, generation))
				.is_some_and(spawn_rate_limit_refresh);
			let matching_refresh_in_progress = denied_quota_epoch.is_some_and(|quota_epoch| quota_rotation_in_progress(attempt.lane, generation, quota_epoch));
			let short_refresh_retry = emergency_started || matching_refresh_in_progress;
			if emergency_started {
				let reset_remaining = delay.saturating_sub(RATE_LIMIT_COOLDOWN_MARGIN);
				warn!(
					"Local Reddit quota reserve reached during redirect with {} seconds left in the current window; rotating once to avoid a prolonged pause",
					reset_remaining.as_secs()
				);
			}
			record_local_denial(CooldownReason::RateLimit);
			if short_refresh_retry {
				return Err(ApiRequestError::deferred(
					format!(
						"Refreshing the anonymous Reddit session. Retry in {} seconds",
						retry_after_seconds(EMERGENCY_QUOTA_REFRESH_RETRY)
					),
					CooldownReason::RateLimit,
				));
			}
			return Err(ApiRequestError::deferred(
				format!("{}. Retry in {} seconds", CooldownReason::RateLimit.message(), retry_after_seconds(delay)),
				CooldownReason::RateLimit,
			));
		}
	};
	attempt.generation = generation;
	attempt.quota_epoch = quota_epoch;
	attempt.request_id = request_id;
	attempt.discovery_probe = discovery_probe;
	attempt.sent = false;
	attempt.quota_reconciled = false;
	Ok(())
}

fn block_for_edge_throttle(attempt: &mut UpstreamAttempt, retry_after: Option<Duration>) -> EdgeThrottleDecision {
	let decision = upstream_guard(attempt.lane).record_edge_throttle(Instant::now(), attempt.edge, retry_after);
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
	if let Some(recovery) = upstream_guard(attempt.lane).record_api_success(Instant::now(), attempt.edge) {
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

pub fn build_client() -> WreqClient {
	build_emulated_client(RedditLane::Direct, None).expect("Should always be able to build the direct Reddit client")
}

fn build_tor_client() -> Result<WreqClient, String> {
	let config = TOR_FALLBACK_CONFIG.as_ref().map_err(|error| error.clone())?;
	let config = config.as_ref().ok_or_else(|| "Tor fallback is disabled".to_string())?;
	let proxy = Proxy::all(config.proxy_url.as_str()).map_err(|error| format!("invalid REDLIB_TOR_PROXY: {error}"))?;
	build_emulated_client(RedditLane::Tor, Some(proxy))
}

pub(crate) fn client_for_lane(lane: RedditLane) -> Result<&'static WreqClient, String> {
	match lane {
		RedditLane::Direct => Ok(&CLIENT),
		RedditLane::Tor => TOR_CLIENT.as_ref().map_err(|error| error.clone()),
	}
}

pub fn start_tor_fallback() {
	let config = match TOR_FALLBACK_CONFIG.as_ref() {
		Ok(Some(config)) => config,
		Ok(None) => return,
		Err(error) => {
			warn!("Tor fallback is disabled because its configuration is invalid: {error}");
			return;
		}
	};
	if TOR_WARMUP_STARTED.compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst).is_err() {
		return;
	}
	if let Err(error) = client_for_lane(RedditLane::Tor) {
		warn!("Tor fallback is disabled because its HTTP client could not be built: {error}");
		return;
	}
	info!("Warming Tor fallback through {}", config.proxy_url);
	tokio::spawn(async {
		let oauth = Oauth::new(RedditLane::Tor).await;
		if install_oauth_client(oauth, true, None) {
			info!("Tor fallback is ready");
			tokio::spawn(token_daemon(RedditLane::Tor));
		}
	});
}

fn build_emulated_client(lane: RedditLane, proxy: Option<Proxy>) -> Result<WreqClient, String> {
	// Keeping this list short to aid in privacy.
	// The more emulations, the more unique a fingerprint each instance has.
	// But some emulations should increase evasiveness.
	let emulations = [Emulation::Chrome145, Emulation::Firefox147];
	let emulation_operating_systems = [EmulationOS::Android, EmulationOS::Windows];

	let rand = fastrand::usize(..);
	let selected_emulation = emulations[rand % emulations.len()];
	let selected_operating_system = emulation_operating_systems[rand % emulation_operating_systems.len()];
	let emulation = EmulationOption::builder()
		.emulation(selected_emulation)
		.emulation_os(selected_operating_system)
		.build()
		.emulation();

	info!(
		"Building Wreq client: lane={} browser={selected_emulation:?} os={selected_operating_system:?}",
		lane.label()
	);
	let mut builder = WreqClient::builder().emulation(emulation).redirect(Policy::none());
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
#[async_recursion::async_recursion]
pub async fn canonical_path(path: String, tries: i8) -> Result<Option<String>, String> {
	if tries == 0 {
		return Ok(None);
	}

	// for each URL pair, try the HEAD request
	let res = {
		// for url base and host in URL_PAIRS, try reddit_short_head(path.clone(), true, url_base, url_base_host) and if it succeeds, set res. else, res = None
		let mut res = None;
		for (url_base, url_base_host) in URL_PAIRS {
			res = reddit_short_head(path.clone(), true, url_base, url_base_host).await.ok();
			if let Some(res) = &res {
				if !res.status().is_client_error() {
					break;
				}
			}
		}
		res
	};

	let res = res.ok_or_else(|| "Unable to make HEAD request to Reddit.".to_string())?;
	let status = res.status().as_u16();
	let policy_error = res.headers().get(wreq_header::RETRY_AFTER).is_some();

	match status {
		// If Reddit responds with a 2xx, then the path is already canonical.
		200..=299 => Ok(Some(path)),

		// If Reddit responds with a 301, then the path is redirected.
		301 => match res.headers().get(wreq_header::LOCATION) {
			Some(val) => {
				let Ok(original) = val.to_str() else {
					return Err("Unable to decode Location header.".to_string());
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
				let uri = format_url(stripped_uri);

				// Decrement tries and try again
				canonical_path(uri, tries - 1).await
			}
			None => Ok(None),
		},

		// If Reddit responds with anything other than 3xx (except for the 2xx and 301
		// as above), return a None.
		300..=399 => Ok(None),

		// Rate limiting
		429 => Err("Too many requests.".to_string()),

		// Special condition rate limiting - https://github.com/redlib-org/redlib/issues/229
		403 if policy_error => Err("Too many requests.".to_string()),

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

	// Add User-Agent header of the currently spoofed device
	{
		let client = OAUTH_CLIENT.load_full();
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

/// Makes a GET request to Reddit at `path`. By default, this will honor HTTP
/// 3xx codes Reddit returns and will automatically redirect.
async fn reddit_get(path: String, quarantine: bool, oauth_client: Arc<Oauth>, attempt: &mut UpstreamAttempt) -> Result<WreqResponse, ApiRequestError> {
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
		let response = request_once(&Method::GET, path.clone(), quarantine, origin.base, origin.host, oauth_client.clone(), lane)
			.await
			.map_err(ApiRequestError::Upstream)?;
		if !response.status().is_redirection() {
			return Ok(response);
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
		reserve_redirect_hop(attempt, generation, remaining, reset)?;
		path = next_path;
	}

	Err(ApiRequestError::Upstream("Reddit redirect handling terminated unexpectedly".to_string()))
}

/// Makes a HEAD request to Reddit at `path, using the short URL base. This will not follow redirects.
fn reddit_short_head(path: String, quarantine: bool, base_path: &'static str, host: &'static str) -> Boxed<Result<WreqResponse, String>> {
	CANONICAL_HEAD_SENDS.fetch_add(1, Ordering::Relaxed);
	maybe_log_traffic_summary();
	request_once(&Method::HEAD, path, quarantine, base_path, host, OAUTH_CLIENT.load_full(), RedditLane::Direct)
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
	Ok(normalize_reddit_api_path(&percent_encode(path.as_bytes(), CONTROLS).to_string()))
}

/// Makes exactly one request to an already-approved Reddit origin. Redirects
/// are deliberately handled by the API wrapper so every wire send receives an
/// admission ticket and a bounded, validated destination.
fn request_once(
	method: &'static Method,
	path: String,
	quarantine: bool,
	base_path: &'static str,
	host: &'static str,
	oauth_client: Arc<Oauth>,
	lane: RedditLane,
) -> Boxed<Result<WreqResponse, String>> {
	if oauth_client.lane != lane {
		return async move { Err("Reddit request lane does not match its OAuth identity".to_string()) }.boxed();
	}
	// Build Reddit URL from path.
	let url = format!("{base_path}{path}");

	let mut headers: Vec<(String, String)> = vec![
		("Host".into(), host.into()),
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

	let client = match client_for_lane(lane) {
		Ok(client) => client,
		Err(error) => return async move { Err(error) }.boxed(),
	};
	let mut builder = client.request(method.clone(), &url);

	for (key, value) in headers {
		builder = builder.header(key, value);
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

/// Make a request to a Reddit API and parse the JSON response.
///
/// The short outer cache coalesces identical concurrent misses and briefly
/// caches errors. Successful metadata responses are kept longer than dynamic
/// listings, and either cache can serve its most recent success if a refresh
/// fails.
pub async fn json(path: String, quarantine: bool) -> Result<Value, String> {
	let path = normalize_reddit_api_path(&path);
	record_logical_json(&path);
	json_coalesced(path, quarantine).await
}

#[cached(size = 1024, time = 2, sync_writes = "by_key")]
async fn json_coalesced(path: String, quarantine: bool) -> Result<Value, String> {
	match json_cache_policy(&path) {
		JsonCachePolicy::Metadata => json_metadata_cached(path, quarantine).await,
		JsonCachePolicy::Comments => json_comments_cached(path, quarantine).await,
		JsonCachePolicy::Dynamic => json_dynamic_cached(path, quarantine).await,
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
	let mut pairs = url::form_urlencoded::parse(query.as_bytes())
		.filter(|(key, _)| {
			let key = key.as_ref();
			key != "raw_json" && key != "share_id" && !key.starts_with("utm_")
		})
		.map(|(key, value)| (key.into_owned(), value.into_owned()))
		.collect::<Vec<_>>();
	pairs.push(("raw_json".to_string(), "1".to_string()));
	pairs.sort_by(|(left, _), (right, _)| left.cmp(right));

	let mut serializer = url::form_urlencoded::Serializer::new(String::new());
	serializer.extend_pairs(pairs);
	format!("{base}?{}", serializer.finish())
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
	let lane = preferred_api_lane(Instant::now());
	let (result, edge_rejected) = json_uncached_on_lane(path.clone(), quarantine, lane).await;
	if should_retry_on_tor(lane, edge_rejected, tor_fallback_ready(), direct_edge_fallback_active(Instant::now())) {
		TOR_FALLBACKS.fetch_add(1, Ordering::Relaxed);
		info!("Retrying edge-rejected Reddit API request on the Tor lane: endpoint={}", endpoint_class(&path));
		return json_uncached_on_lane(path, quarantine, RedditLane::Tor).await.0;
	}
	result
}

fn should_retry_on_tor(lane: RedditLane, edge_rejected: bool, tor_ready: bool, direct_edge_active: bool) -> bool {
	lane == RedditLane::Direct && edge_rejected && tor_ready && direct_edge_active
}

async fn json_uncached_on_lane(path: String, quarantine: bool, lane: RedditLane) -> (Result<Value, String>, bool) {
	// Closure to quickly build errors
	let err = |msg: &str, e: String, path: String| -> Result<Value, String> {
		// eprintln!("{} - {}: {}", url, msg, e);
		Err(format!("{msg}: {e} | {path}"))
	};

	if let Some((error, edge_deferred)) = cooldown_error(lane) {
		return (Err(error), edge_deferred);
	}

	let request_timeout = lane.request_timeout();
	let request_deadline = tokio::time::Instant::now() + request_timeout;
	let _permit = match tokio::time::timeout_at(request_deadline, api_concurrency(lane).acquire()).await {
		Ok(Ok(permit)) => permit,
		Ok(Err(_)) => return (Err("Reddit request limiter is unavailable".to_string()), false),
		Err(_) => return (Err("Reddit API request timed out while waiting for transport capacity".to_string()), false),
	};

	// Keep this exact OAuth client throughout redirects and attach its generation
	// to the response. A late response from an old identity must not overwrite a
	// newly rotated identity's request budget.
	let (oauth_client, mut upstream_attempt) = match begin_upstream_attempt(lane) {
		Ok(attempt) => attempt,
		Err((error, edge_deferred)) => return (Err(error), edge_deferred),
	};
	let request_generation = oauth_client.generation;
	// Admission atomically selects the OAuth client and owns its quota
	// reservation and edge half-open probe.
	record_admitted_json(&path);
	let timeout_path = path.clone();
	let mut edge_rejected = false;

	// Fetch the url...
	let result = tokio::time::timeout_at(request_deadline, async {
		match reddit_get(path.clone(), quarantine, oauth_client, &mut upstream_attempt).await {
			Ok(response) => {
				let status = response.status();
				let status_code = status.as_u16();

				let remaining = response.headers().get("x-ratelimit-remaining").and_then(|value| value.to_str().ok());
				let reset = response.headers().get("x-ratelimit-reset").and_then(|value| value.to_str().ok());
				let used = response.headers().get("x-ratelimit-used").and_then(|value| value.to_str().ok());
				let retry_after = response.headers().get(wreq_header::RETRY_AFTER).and_then(|value| value.to_str().ok());
				let parsed_remaining = parse_rate_limit_count(remaining);
				let parsed_used = parse_rate_limit_count(used);
				let reset_duration = parse_delay_seconds(reset);
				let retry_after_duration = parse_retry_after(retry_after, SystemTime::now());
				let quota_headers_present = remaining.is_some() || reset.is_some() || used.is_some();

				let throttle_kind = classify_throttle_response(status_code, retry_after.is_some(), quota_headers_present);
				reconcile_rate_limit(
					&mut upstream_attempt,
					parsed_remaining,
					reset_duration,
					matches!(throttle_kind, Some(ThrottleKind::Quota)) || parsed_remaining == Some(0),
				);
				if !matches!(throttle_kind, Some(ThrottleKind::Edge)) {
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

				match throttle_kind {
					Some(ThrottleKind::Quota) => {
						let (delay, response_is_current) = block_for_rate_limit(lane, request_generation, retry_after, reset);
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
						edge_rejected = true;
						let decision = block_for_edge_throttle(&mut upstream_attempt, retry_after_duration);
						match decision {
							decision if decision.started_cooldown => warn!(
								"Reddit edge throttle: status={} endpoint={} retry_after_present={} retry_after_valid={} retry_after_seconds={} quota_headers_present={} consecutive_failures={} episode_seconds={} cooldown_seconds={} half_open_probe={} request_generation={} current_generation={} current_identity_age_seconds={}",
								status,
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

				if status_code == 401 {
					if !is_current_oauth_generation(lane, request_generation) {
						return Err("OAuth token changed while this request was in flight. Please retry.".to_string());
					}
					error!("Reddit rejected the OAuth token; forcing a refresh");
					let outcome = force_refresh_token(lane, RefreshReason::Unauthorized).await;
					if let Some(delay) = outcome.retry_after() {
						return Err(format!("OAuth token refresh is temporarily unavailable. Retry in {} seconds", retry_after_seconds(delay)));
					}
					return Err("OAuth token has expired. Please refresh the page!".to_string());
				}

				if status.is_server_error() {
					record_upstream_failure(lane, "http_status", Some(status_code), &path, request_generation);
					return Err("Reddit is having issues, check if there's an outage".to_string());
				}

				// asynchronously aggregate the chunks of the body
				match hyper::body::aggregate(response.into_hyper_response()).await {
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
						err("Failed receiving body from Reddit", e.to_string(), path)
					}
				}
			}
			Err(ApiRequestError::Deferred {
				message,
				edge_rejected: deferred_edge_rejected,
			}) => {
				edge_rejected |= deferred_edge_rejected;
				Err(message)
			}
			Err(ApiRequestError::Upstream(error)) => {
			record_upstream_failure(lane, "request_transport", None, &path, request_generation);
				err("Couldn't send request to Reddit", error, path)
			}
		}
	})
	.await;

	let result = match result {
		Ok(result) => result,
		Err(_) => {
			record_upstream_failure(lane, "request_timeout", None, &timeout_path, request_generation);
			Err(format!("Reddit API request timed out after {} seconds", request_timeout.as_secs()))
		}
	};
	(result, edge_rejected)
}

async fn self_check(sub: &str) -> Result<(), String> {
	let query = format!("/r/{sub}/hot.json?&raw_json=1");

	match Post::fetch(&query, true).await {
		Ok(_) => Ok(()),
		Err(e) => Err(e),
	}
}

pub async fn rate_limit_check() -> Result<(), String> {
	// We can perform a startup reachability check if the OAuth backend is
	// MobileSpoof; GenericWeb does not expose the same rate-limit behavior.
	if matches!(OAUTH_CLIENT.load().backend, OauthBackendImpl::GenericWeb(_)) {
		warn!("[⚠️] Cannot perform rate limit check, running as GenericWeb. Skipping check.");
		return Ok(());
	}

	// Make one uncached request. Quota-driven identity rotation is handled only
	// after Reddit reports a low budget, rather than creating extra authentication
	// traffic during every startup.
	self_check("reddit").await?;
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

	const POPULAR_URL: &str = "/r/popular/hot.json?&raw_json=1&geo_filter=GLOBAL";
	static COALESCED_TEST_CALLS: AtomicUsize = AtomicUsize::new(0);

	#[cached(size = 8, time = 30, sync_writes = "by_key")]
	async fn coalesced_test_fetch(key: u8) -> u8 {
		COALESCED_TEST_CALLS.fetch_add(1, Ordering::SeqCst);
		tokio::time::sleep(Duration::from_millis(50)).await;
		key
	}

	#[tokio::test]
	async fn test_identical_cache_misses_are_coalesced() {
		COALESCED_TEST_CALLS.store(0, Ordering::SeqCst);
		let (first, second, third) = tokio::join!(coalesced_test_fetch(42), coalesced_test_fetch(42), coalesced_test_fetch(42));
		assert_eq!((first, second, third), (42, 42, 42));
		assert_eq!(COALESCED_TEST_CALLS.load(Ordering::SeqCst), 1);
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
	fn test_quota_rotation_requires_known_exhausted_current_generation() {
		let now = Instant::now();
		let mut guard = UpstreamGuard {
			quota: QuotaGovernor {
				generation: 7,
				epoch: 1,
				next_request_id: 0,
				outstanding: 0,
				rollover_reserve: 0,
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

		guard.quota.window = QuotaWindow::Unknown {
			not_before: now,
			probe_in_flight: true,
		};
		assert_eq!(guard.quota_rotation_candidate(now, 7, QuotaRotationMode::Proactive), None);
		guard.quota.window = QuotaWindow::Unreported;
		assert_eq!(guard.quota_rotation_candidate(now, 7, QuotaRotationMode::Proactive), None);

		guard.quota.window = QuotaWindow::Known {
			available: LOW_RATE_LIMIT_THRESHOLD - 1,
			reset_at: now + QUOTA_ROTATION_MIN_RESET_REMAINING + Duration::from_secs(1),
		};
		guard.edge_state = EdgeCircuitState::Open {
			until: now + Duration::from_secs(10),
		};
		assert_eq!(guard.quota_rotation_candidate(now, 7, QuotaRotationMode::Proactive), None);
		guard.edge_state = EdgeCircuitState::Closed;
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

		guard.emergency_rotation_claimed_epoch = None;
		guard.quota.window = QuotaWindow::Unknown {
			not_before: now,
			probe_in_flight: false,
		};
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
	fn test_emergency_quota_rotation_claim_survives_stable_refresh_but_not_new_epoch() {
		let now = Instant::now();
		let mut guard = UpstreamGuard {
			quota: QuotaGovernor {
				generation: 3,
				epoch: 8,
				next_request_id: 0,
				outstanding: 0,
				rollover_reserve: 0,
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
		assert!(guard.claim_quota_rotation(now, next_ticket));
		guard.rate_limit_blocked_until = Some(now + Duration::from_secs(10));
		assert!(!guard.quota_rotation_still_needed(now, next_ticket));
		guard.rate_limit_blocked_until = None;
		guard.upstream_failure_blocked_until = Some(now + Duration::from_secs(10));
		assert!(!guard.quota_rotation_still_needed(now, next_ticket));
		guard.upstream_failure_blocked_until = None;
		guard.edge_state = EdgeCircuitState::Open {
			until: now + Duration::from_secs(10),
		};
		assert!(!guard.quota_rotation_still_needed(now, next_ticket));
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
			window: QuotaWindow::Known {
				available: 6,
				reset_at: now + Duration::from_secs(120),
			},
		};
		let attempt = |request_id| UpstreamAttempt {
			lane: RedditLane::Direct,
			edge: EdgeAttempt { epoch: 0, half_open: false },
			generation: 3,
			quota_epoch: 4,
			request_id,
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
			window: QuotaWindow::Known {
				available: LOW_RATE_LIMIT_THRESHOLD - 1,
				reset_at: old_reset,
			},
		};
		let attempt = UpstreamAttempt {
			lane: RedditLane::Direct,
			edge: EdgeAttempt { epoch: 0, half_open: false },
			generation: 3,
			quota_epoch: 4,
			request_id: 1,
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
			edge: EdgeAttempt { epoch: 0, half_open: false },
			generation: 3,
			quota_epoch,
			request_id,
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
			edge: EdgeAttempt { epoch: 0, half_open: false },
			generation: 3,
			quota_epoch: 4,
			request_id: 2,
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
			window: QuotaWindow::Known {
				available: LOW_RATE_LIMIT_THRESHOLD - 1,
				reset_at: old_reset,
			},
		};
		let attempt = UpstreamAttempt {
			lane: RedditLane::Direct,
			edge: EdgeAttempt { epoch: 0, half_open: false },
			generation: 2,
			quota_epoch: 3,
			request_id: 1,
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
			window: QuotaWindow::Known {
				available: 40,
				reset_at: old_reset,
			},
		};
		let attempt = UpstreamAttempt {
			lane: RedditLane::Direct,
			edge: EdgeAttempt { epoch: 0, half_open: false },
			generation: 2,
			quota_epoch: 3,
			request_id: 1,
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
			window: QuotaWindow::Known {
				available: 5,
				reset_at: now + Duration::from_secs(90),
			},
		};
		let stale_attempt = UpstreamAttempt {
			lane: RedditLane::Direct,
			edge: EdgeAttempt { epoch: 0, half_open: false },
			generation: 4,
			quota_epoch: 2,
			request_id: 8,
			discovery_probe: false,
			sent: true,
			quota_reconciled: true,
			completed: true,
		};
		let mut stale_unsent_attempt = UpstreamAttempt {
			lane: RedditLane::Direct,
			edge: EdgeAttempt { epoch: 0, half_open: false },
			generation: 4,
			quota_epoch: 2,
			request_id: 9,
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
		assert!(quota.reserve(fresh_now + Duration::from_secs(1), 5).is_err());
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
			window: QuotaWindow::Known {
				available: 7,
				reset_at: now + Duration::from_secs(90),
			},
		};
		assert_eq!(quota.reserve(now, 4), Err(QuotaReserveError::StaleGeneration));
		assert_eq!(quota.generation, 5);

		let stale_attempt = UpstreamAttempt {
			lane: RedditLane::Direct,
			edge: EdgeAttempt { epoch: 0, half_open: false },
			generation: 4,
			quota_epoch: 3,
			request_id: 1,
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
			window: QuotaWindow::Unknown {
				not_before: now,
				probe_in_flight: false,
			},
		};
		let (quota_epoch, request_id, discovery_probe) = quota.reserve(now, 2).unwrap();
		assert!(discovery_probe);
		let attempt = UpstreamAttempt {
			lane: RedditLane::Direct,
			edge: EdgeAttempt { epoch: 0, half_open: false },
			generation: 2,
			quota_epoch,
			request_id,
			discovery_probe,
			sent: true,
			quota_reconciled: true,
			completed: true,
		};
		quota.reconcile(now, &attempt, None, None, false);
		assert!(matches!(quota.window, QuotaWindow::Unknown { probe_in_flight: false, .. }));
		assert!(quota.reserve(now, 2).is_err());
		quota.confirm_headerless_success(&attempt);
		assert!(matches!(quota.window, QuotaWindow::Unreported));
		assert!(quota.reserve(now, 2).is_ok());
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
			window: QuotaWindow::Unknown {
				not_before: now,
				probe_in_flight: false,
			},
		};
		let (quota_epoch, request_id, discovery_probe) = quota.reserve(now, 1).unwrap();
		quota.install_generation(2, false);
		let stale_attempt = UpstreamAttempt {
			lane: RedditLane::Direct,
			edge: EdgeAttempt { epoch: 0, half_open: false },
			generation: 1,
			quota_epoch,
			request_id,
			discovery_probe,
			sent: true,
			quota_reconciled: true,
			completed: true,
		};
		quota.reconcile(now, &stale_attempt, Some(99), Some(Duration::from_secs(300)), false);
		assert_eq!(quota.outstanding, 0);
		assert!(matches!(quota.window, QuotaWindow::Unknown { probe_in_flight: false, .. }));
		assert_eq!(quota.reserve(now + QUOTA_UNKNOWN_RETRY + Duration::from_secs(2), 2).unwrap().2, true);
	}

	#[test]
	fn test_headerless_redirect_preserves_exclusive_discovery_with_a_new_reservation() {
		let now = Instant::now();
		let mut quota = QuotaGovernor {
			generation: 3,
			epoch: 7,
			next_request_id: 0,
			outstanding: 0,
			rollover_reserve: 0,
			window: QuotaWindow::Unknown {
				not_before: now,
				probe_in_flight: false,
			},
		};
		let (quota_epoch, request_id, discovery_probe) = quota.reserve(now, 3).unwrap();
		let mut attempt = UpstreamAttempt {
			lane: RedditLane::Direct,
			edge: EdgeAttempt { epoch: 0, half_open: false },
			generation: 3,
			quota_epoch,
			request_id,
			discovery_probe,
			sent: true,
			quota_reconciled: false,
			completed: false,
		};
		assert_eq!(quota.outstanding, 1);
		quota.reconcile(now, &attempt, None, None, false);
		attempt.quota_reconciled = true;
		assert!(quota.reserve(now, 3).is_err());
		quota.continue_headerless_discovery_after_redirect(now, &attempt);
		let (next_epoch, next_request_id, next_discovery_probe) = quota.reserve(now, 3).unwrap();
		assert_eq!(next_epoch, quota_epoch);
		assert_ne!(next_request_id, request_id);
		assert!(next_discovery_probe);
		assert!(quota.reserve(now, 3).is_err());
		assert_eq!(quota.outstanding, 1);
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
			window: QuotaWindow::Known {
				available: 6,
				reset_at: old_reset,
			},
		};
		let attempt = UpstreamAttempt {
			lane: RedditLane::Direct,
			edge: EdgeAttempt { epoch: 0, half_open: false },
			generation: 8,
			quota_epoch: 12,
			request_id: 3,
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
			window: QuotaWindow::Known {
				available: 0,
				reset_at: now - RATE_LIMIT_COOLDOWN_MARGIN,
			},
		};
		assert!(quota.reserve(now, 1).is_ok());
		assert!(quota.reserve(now, 1).is_err());
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

		guard.edge_state = EdgeCircuitState::Closed;
		guard.edge_epoch = redirecting.epoch;
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
		assert!(guard.edge_episode_started_at.is_none());
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
		assert!(edge_fallback_active(&direct, now));
		assert!(!edge_fallback_active(&tor, now));
		assert!(tor.begin_attempt(now).is_ok());

		direct.block_for_rate_limit(now, Duration::from_secs(30));
		assert!(!edge_fallback_active(&direct, now));
		assert_eq!(direct.active_cooldown(now).map(|(_, reason)| reason), Some(CooldownReason::RateLimit));
	}

	#[test]
	fn test_tor_retry_requires_this_request_to_be_edge_rejected() {
		assert!(should_retry_on_tor(RedditLane::Direct, true, true, true));
		assert!(!should_retry_on_tor(RedditLane::Direct, false, true, true));
		assert!(!should_retry_on_tor(RedditLane::Direct, true, false, true));
		assert!(!should_retry_on_tor(RedditLane::Direct, true, true, false));
		assert!(!should_retry_on_tor(RedditLane::Tor, true, true, true));
	}

	#[test]
	fn test_only_edge_redirect_deferrals_qualify_for_tor_retry() {
		let deferred = |reason| ApiRequestError::deferred("deferred".to_string(), reason);
		assert!(matches!(deferred(CooldownReason::EdgeThrottle), ApiRequestError::Deferred { edge_rejected: true, .. }));
		assert!(matches!(deferred(CooldownReason::RateLimit), ApiRequestError::Deferred { edge_rejected: false, .. }));
		assert!(matches!(deferred(CooldownReason::UpstreamFailures), ApiRequestError::Deferred { edge_rejected: false, .. }));
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
		assert_eq!(guard.edge_throttle_failures, 1);
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
		assert_eq!(guard.edge_throttle_failures, 1);
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
		assert!(matches!(guard.edge_state, EdgeCircuitState::Open { .. }));
		assert_eq!(guard.edge_throttle_failures, 1);
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
		assert!(matches!(guard.edge_state, EdgeCircuitState::Open { .. }));
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
		assert!(matches!(guard.edge_state, EdgeCircuitState::HalfOpen { .. }));
		assert!(guard
			.record_api_success(stale_probe_at + RedditLane::Direct.request_timeout() + Duration::from_secs(1), replacement_probe)
			.is_some());
		assert!(matches!(guard.edge_state, EdgeCircuitState::Closed));
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
		let preserved = normalize_reddit_api_path("/comments/abc.json?context=3&q=a%2Bb");
		assert!(preserved.contains("context=3"));
		assert!(preserved.contains("q=a%2Bb"));
		assert_ne!(
			normalize_reddit_api_path("/comments/abc/title.json?sort=top"),
			normalize_reddit_api_path("/comments/abc/title.json?sort=new")
		);
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
	async fn test_rate_limit_check() {
		rate_limit_check().await.unwrap();
	}

	#[test]
	#[sealed_test(env = [("REDLIB_DEFAULT_SUBSCRIPTIONS", "rust")])]
	fn test_default_subscriptions() {
		tokio::runtime::Builder::new_multi_thread().enable_all().build().unwrap().block_on(async {
			let subscriptions = get_setting("REDLIB_DEFAULT_SUBSCRIPTIONS");
			assert!(subscriptions.is_some());

			// check rate limit
			rate_limit_check().await.unwrap();
		});
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
