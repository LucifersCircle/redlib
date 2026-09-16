use crate::dbg_msg;
use crate::oauth::{force_refresh_token, spawn_rate_limit_refresh, token_daemon, Oauth, OauthBackendImpl, RefreshReason};
use crate::server::RequestExt;
use crate::utils::{format_url, Post};
use arc_swap::ArcSwap;
use cached::proc_macro::cached;
use futures_lite::future::block_on;
use futures_lite::{future::Boxed, FutureExt};
use hyper::{body::Buf, header, Body, Request as HyperRequest, Response as HyperResponse};
use log::{error, info, trace, warn};
use percent_encoding::{percent_encode, CONTROLS};
use serde_json::Value;
use std::env;
use std::result::Result;
use std::sync::atomic::Ordering;
use std::sync::atomic::{AtomicBool, AtomicU64};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::Semaphore;
use wreq::redirect::Policy;
use wreq::{header as wreq_header, Client as WreqClient, EmulationFactory, Method, Response as WreqResponse};
use wreq_util::{Emulation, EmulationOS, EmulationOption};

const REDDIT_URL_BASE: &str = "https://oauth.reddit.com";
const REDDIT_URL_BASE_HOST: &str = "oauth.reddit.com";

const REDDIT_SHORT_URL_BASE: &str = "https://redd.it";
const REDDIT_SHORT_URL_BASE_HOST: &str = "redd.it";

const ALTERNATIVE_REDDIT_URL_BASE: &str = "https://www.reddit.com";
const ALTERNATIVE_REDDIT_URL_BASE_HOST: &str = "www.reddit.com";

pub static CLIENT: LazyLock<WreqClient> = LazyLock::new(build_client);

pub static OAUTH_CLIENT: LazyLock<ArcSwap<Oauth>> = LazyLock::new(|| {
	let client = block_on(Oauth::new());
	tokio::spawn(token_daemon());
	ArcSwap::new(client.into())
});

// The high bits identify the OAuth client generation and the low 16 bits hold
// its estimated remaining request budget. Keeping them in one atomic prevents
// late responses from an old token from overwriting a newly rotated budget.
static OAUTH_RATELIMIT_STATE: AtomicU64 = AtomicU64::new(99);

pub static OAUTH_IS_ROLLING_OVER: AtomicBool = AtomicBool::new(false);

const DEFAULT_MAX_CONCURRENT_API_REQUESTS: usize = 8;
const MAX_CONFIGURED_API_REQUESTS: usize = 64;
const FAILURE_WINDOW: Duration = Duration::from_secs(10);
const FAILURE_COOLDOWN: Duration = Duration::from_secs(10);
const FAILURE_THRESHOLD: u8 = 3;
const DEFAULT_RATE_LIMIT_COOLDOWN: Duration = Duration::from_secs(10);
const MAX_RATE_LIMIT_COOLDOWN: Duration = Duration::from_secs(600);
const RATE_LIMIT_COOLDOWN_MARGIN: Duration = Duration::from_secs(2);
const LOW_RATE_LIMIT_THRESHOLD: u16 = 10;
const RATE_LIMIT_REMAINING_MASK: u64 = u16::MAX as u64;
const EDGE_THROTTLE_INITIAL_COOLDOWN: Duration = Duration::from_secs(5);
const EDGE_THROTTLE_MAX_COOLDOWN: Duration = Duration::from_secs(60);
const REDDIT_API_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

static REDDIT_API_CONCURRENCY: LazyLock<Semaphore> = LazyLock::new(|| {
	let configured = max_concurrent_api_requests();
	info!("Reddit API concurrency limit: {configured}");
	Semaphore::new(configured)
});
static UPSTREAM_GUARD: LazyLock<Mutex<UpstreamGuard>> = LazyLock::new(|| Mutex::new(UpstreamGuard::default()));
static UPSTREAM_REQUEST_COUNTS: LazyLock<[AtomicU64; 6]> = LazyLock::new(|| std::array::from_fn(|_| AtomicU64::new(0)));

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
struct EdgeThrottleDecision {
	delay: Duration,
	consecutive_failures: u8,
	started_cooldown: bool,
}

#[derive(Debug, Clone, Copy, Default, Eq, PartialEq)]
enum EdgeCircuitState {
	#[default]
	Closed,
	Open { until: Instant },
	HalfOpen { epoch: u64, expires_at: Instant },
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
struct EdgeAttempt {
	epoch: u64,
	half_open: bool,
}

#[derive(Debug)]
struct UpstreamAttempt {
	edge: EdgeAttempt,
	completed: bool,
}

impl UpstreamAttempt {
	fn complete(&mut self) {
		self.completed = true;
	}
}

impl Drop for UpstreamAttempt {
	fn drop(&mut self) {
		if self.edge.half_open && !self.completed {
			upstream_guard().abandon_edge_probe(Instant::now(), self.edge);
		}
	}
}

#[derive(Debug, Default)]
struct UpstreamGuard {
	failure_window_started: Option<Instant>,
	failures_in_window: u8,
	upstream_failure_blocked_until: Option<Instant>,
	rate_limit_blocked_until: Option<Instant>,
	edge_throttle_failures: u8,
	edge_epoch: u64,
	edge_state: EdgeCircuitState,
}

impl UpstreamGuard {
	fn active_cooldown(&self, now: Instant) -> Option<(Duration, CooldownReason)> {
		let mut active = None;
		let mut consider = |deadline: Option<Instant>, reason| {
			if let Some(remaining) = deadline.and_then(|deadline| deadline.checked_duration_since(now)) {
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

	fn extend_deadline(slot: &mut Option<Instant>, now: Instant, duration: Duration) {
		let deadline = now + duration.min(MAX_RATE_LIMIT_COOLDOWN);
		if deadline > slot.as_ref().copied().unwrap_or(now) {
			*slot = Some(deadline);
		}
	}

	fn begin_attempt(&mut self, now: Instant) -> Result<EdgeAttempt, (Duration, CooldownReason)> {
		if let Some(active) = self.active_cooldown(now) {
			return Err(active);
		}

		match self.edge_state {
			EdgeCircuitState::Open { .. } => {
				self.edge_state = EdgeCircuitState::HalfOpen {
					epoch: self.edge_epoch,
					expires_at: now + REDDIT_API_REQUEST_TIMEOUT,
				};
				Ok(EdgeAttempt {
					epoch: self.edge_epoch,
					half_open: true,
				})
			}
			EdgeCircuitState::HalfOpen { .. } => {
				self.edge_epoch = self.edge_epoch.wrapping_add(1);
				let delay = edge_throttle_delay(self.edge_throttle_failures.max(1), None);
				self.edge_state = EdgeCircuitState::Open { until: now + delay };
				Err((delay, CooldownReason::EdgeThrottle))
			}
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
			Self::extend_deadline(&mut self.upstream_failure_blocked_until, now, FAILURE_COOLDOWN);
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

	fn record_api_success(&mut self, attempt: EdgeAttempt) {
		self.reset_failure_window();
		let closes_probe = matches!(self.edge_state, EdgeCircuitState::HalfOpen { epoch, .. } if epoch == attempt.epoch);
		let current_closed_attempt = matches!(self.edge_state, EdgeCircuitState::Closed) && attempt.epoch == self.edge_epoch;
		if closes_probe {
			self.edge_throttle_failures = 0;
			self.edge_epoch = self.edge_epoch.wrapping_add(1);
			self.edge_state = EdgeCircuitState::Closed;
		} else if current_closed_attempt {
			self.edge_throttle_failures = 0;
		}
	}

	fn record_edge_throttle(&mut self, now: Instant, attempt: EdgeAttempt, retry_after: Option<Duration>) -> EdgeThrottleDecision {
		if attempt.epoch != self.edge_epoch {
			let delay = match self.edge_state {
				EdgeCircuitState::Open { until } => until.checked_duration_since(now).unwrap_or_default(),
				EdgeCircuitState::HalfOpen { .. } => Duration::from_secs(1),
				EdgeCircuitState::Closed => edge_throttle_delay(self.edge_throttle_failures.max(1), retry_after),
			};
			return EdgeThrottleDecision {
				delay,
				consecutive_failures: self.edge_throttle_failures,
				started_cooldown: false,
			};
		}

		self.edge_throttle_failures = self.edge_throttle_failures.saturating_add(1);
		let delay = edge_throttle_delay(self.edge_throttle_failures, retry_after);
		self.edge_epoch = self.edge_epoch.wrapping_add(1);
		self.edge_state = EdgeCircuitState::Open { until: now + delay };
		EdgeThrottleDecision {
			delay,
			consecutive_failures: self.edge_throttle_failures,
			started_cooldown: true,
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

	fn clear_rate_limit_block(&mut self) {
		self.rate_limit_blocked_until = None;
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
	value?
		.parse::<f64>()
		.ok()
		.filter(|seconds| seconds.is_finite() && *seconds >= 0.0)
		.map(|seconds| Duration::from_secs_f64(seconds).min(MAX_RATE_LIMIT_COOLDOWN))
}

fn parse_rate_limit_count(value: Option<&str>) -> Option<u16> {
	value?
		.parse::<f64>()
		.ok()
		.filter(|count| count.is_finite() && *count >= 0.0)
		.map(|count| count.round().min(f64::from(u16::MAX)) as u16)
}

fn rate_limit_delay(retry_after: Option<&str>, reset: Option<&str>) -> Duration {
	parse_delay_seconds(retry_after)
		.or_else(|| parse_delay_seconds(reset))
		.unwrap_or(DEFAULT_RATE_LIMIT_COOLDOWN)
		.saturating_add(RATE_LIMIT_COOLDOWN_MARGIN)
		.min(MAX_RATE_LIMIT_COOLDOWN)
}

fn edge_throttle_delay(consecutive_failures: u8, retry_after: Option<Duration>) -> Duration {
	let exponent = u32::from(consecutive_failures.saturating_sub(1).min(7));
	let multiplier = 1_u64.checked_shl(exponent).unwrap_or(u64::MAX);
	let exponential = Duration::from_secs(EDGE_THROTTLE_INITIAL_COOLDOWN.as_secs().saturating_mul(multiplier)).min(EDGE_THROTTLE_MAX_COOLDOWN);
	let server_delay = retry_after.unwrap_or_default().saturating_add(RATE_LIMIT_COOLDOWN_MARGIN).min(MAX_RATE_LIMIT_COOLDOWN);
	exponential.max(server_delay)
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
enum ThrottleKind {
	Quota,
	Edge,
}

fn classify_throttle_response(status: u16, retry_after_present: bool, quota_headers_present: bool) -> Option<ThrottleKind> {
	match status {
		429 => Some(ThrottleKind::Quota),
		403 if retry_after_present && quota_headers_present => Some(ThrottleKind::Quota),
		403 if retry_after_present => Some(ThrottleKind::Edge),
		_ => None,
	}
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
struct RateLimitReservation {
	generation: u64,
	previous_remaining: u16,
}

fn rate_limit_state(generation: u64, remaining: u16) -> u64 {
	(generation << 16) | u64::from(remaining)
}

fn rate_limit_generation(state: u64) -> u64 {
	state >> 16
}

fn rate_limit_remaining(state: u64) -> u16 {
	(state & RATE_LIMIT_REMAINING_MASK) as u16
}

fn is_current_oauth_generation(generation: u64) -> bool {
	rate_limit_generation(OAUTH_RATELIMIT_STATE.load(Ordering::SeqCst)) == generation
}

fn reserve_rate_limit_slot(counter: &AtomicU64, generation: u64) -> RateLimitReservation {
	let previous = counter
		.fetch_update(Ordering::SeqCst, Ordering::SeqCst, |state| {
			(rate_limit_generation(state) == generation).then(|| rate_limit_state(generation, rate_limit_remaining(state).saturating_sub(1)))
		})
		.unwrap_or_else(|state| state);
	RateLimitReservation {
		generation,
		previous_remaining: rate_limit_remaining(previous),
	}
}

fn update_rate_limit_remaining(counter: &AtomicU64, generation: u64, remaining: u16) -> bool {
	let mut current = counter.load(Ordering::SeqCst);
	loop {
		if rate_limit_generation(current) != generation {
			return false;
		}
		let updated = rate_limit_state(generation, remaining);
		match counter.compare_exchange_weak(current, updated, Ordering::SeqCst, Ordering::SeqCst) {
			Ok(_) => return true,
			Err(actual) => current = actual,
		}
	}
}

pub(crate) fn current_rate_limit_remaining() -> u16 {
	rate_limit_remaining(OAUTH_RATELIMIT_STATE.load(Ordering::SeqCst))
}

fn state_for_oauth_install(state: u64, generation: u64, fresh_identity: bool) -> u64 {
	let remaining = if fresh_identity { 99 } else { rate_limit_remaining(state) };
	rate_limit_state(generation, remaining)
}

pub(crate) fn install_oauth_generation(generation: u64, fresh_identity: bool) {
	OAUTH_RATELIMIT_STATE
		.fetch_update(Ordering::SeqCst, Ordering::SeqCst, |state| Some(state_for_oauth_install(state, generation, fresh_identity)))
		.unwrap_or_else(|state| state);

	let mut guard = upstream_guard();
	guard.reset_failure_window();
	if fresh_identity {
		guard.clear_rate_limit_block();
		reset_upstream_request_counts();
	}
}

fn endpoint_class_index(path: &str) -> usize {
	let path = path.split('?').next().unwrap_or_default();
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
	["subreddit", "user", "api", "search", "comments", "other"][endpoint_class_index(path)]
}

fn record_upstream_request(path: &str) {
	UPSTREAM_REQUEST_COUNTS[endpoint_class_index(path)].fetch_add(1, Ordering::Relaxed);
}

fn reset_upstream_request_counts() {
	for counter in UPSTREAM_REQUEST_COUNTS.iter() {
		counter.store(0, Ordering::Relaxed);
	}
}

fn upstream_request_summary() -> String {
	["subreddit", "user", "api", "search", "comments", "other"]
		.into_iter()
		.zip(UPSTREAM_REQUEST_COUNTS.iter())
		.map(|(label, counter)| format!("{label}={}", counter.load(Ordering::Relaxed)))
		.collect::<Vec<_>>()
		.join(",")
}

fn maybe_rotate_low_budget(generation: u64, remaining: u16, used: Option<u16>, reset: Option<Duration>, path: &str) {
	if remaining >= LOW_RATE_LIMIT_THRESHOLD || !is_current_oauth_generation(generation) || !edge_circuit_allows_oauth_rollover() || !spawn_rate_limit_refresh(reset) {
		return;
	}

	warn!(
		"Reddit request budget is low: remaining={remaining} used={} reset_seconds={} endpoint={} api_requests_since_identity_approx={}; rotating anonymous OAuth identity once",
		used.map_or(0, u16::from),
		reset.map_or(0, |duration| duration.as_secs()),
		endpoint_class(path),
		upstream_request_summary(),
	);
}

fn upstream_guard() -> std::sync::MutexGuard<'static, UpstreamGuard> {
	UPSTREAM_GUARD.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn cooldown_error() -> Option<String> {
	let guard = upstream_guard();
	guard.active_cooldown(Instant::now()).map(|(remaining, reason)| {
		let message = reason.message();
		format!("{message}. Retry in {} seconds", remaining.as_secs().max(1))
	})
}

fn edge_circuit_allows_oauth_rollover() -> bool {
	matches!(upstream_guard().edge_state, EdgeCircuitState::Closed)
}

fn begin_upstream_attempt() -> Result<UpstreamAttempt, String> {
	let edge = upstream_guard()
		.begin_attempt(Instant::now())
		.map_err(|(remaining, reason)| format!("{}. Retry in {} seconds", reason.message(), remaining.as_secs().max(1)))?;
	Ok(UpstreamAttempt { edge, completed: false })
}

fn block_for_rate_limit(generation: u64, duration: Duration) -> bool {
	let mut guard = upstream_guard();
	if rate_limit_generation(OAUTH_RATELIMIT_STATE.load(Ordering::SeqCst)) != generation {
		return false;
	}
	guard.block_for_rate_limit(Instant::now(), duration);
	true
}

fn block_for_edge_throttle(attempt: &mut UpstreamAttempt, retry_after: Option<Duration>) -> EdgeThrottleDecision {
	let decision = upstream_guard().record_edge_throttle(Instant::now(), attempt.edge, retry_after);
	attempt.complete();
	decision
}

fn record_upstream_failure(kind: &str, status: Option<u16>, path: &str, generation: u64) {
	let mut guard = upstream_guard();
	if rate_limit_generation(OAUTH_RATELIMIT_STATE.load(Ordering::SeqCst)) != generation {
		trace!("Ignoring stale Reddit upstream failure: kind={kind} endpoint={}", endpoint_class(path));
		return;
	}
	let opened = guard.record_failure(Instant::now());
	warn!(
		"Reddit upstream failure: kind={kind} status={} endpoint={} circuit_opened={opened}",
		status.map_or_else(|| "transport".to_string(), |status| status.to_string()),
		endpoint_class(path),
	);
}

fn record_upstream_success(attempt: &mut UpstreamAttempt) {
	upstream_guard().record_api_success(attempt.edge);
	attempt.complete();
}

const URL_PAIRS: [(&str, &str); 2] = [
	(ALTERNATIVE_REDDIT_URL_BASE, ALTERNATIVE_REDDIT_URL_BASE_HOST),
	(REDDIT_SHORT_URL_BASE, REDDIT_SHORT_URL_BASE_HOST),
];

pub fn build_client() -> WreqClient {
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

	info!("Building Wreq client: browser={selected_emulation:?} os={selected_operating_system:?}");
	WreqClient::builder()
		.emulation(emulation)
		.redirect(Policy::none())
		.build()
		.expect("Should always be able to build a client")
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

	builder
		.send()
		.await
		.map(|mut res| {
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

			res.into_hyper_response()
		})
		.map_err(|e| e.to_string())
}

/// Makes a GET request to Reddit at `path`. By default, this will honor HTTP
/// 3xx codes Reddit returns and will automatically redirect.
fn reddit_get(path: String, quarantine: bool, oauth_client: Arc<Oauth>) -> Boxed<Result<WreqResponse, String>> {
	request(&Method::GET, path, true, quarantine, REDDIT_URL_BASE, REDDIT_URL_BASE_HOST, oauth_client)
}

/// Makes a HEAD request to Reddit at `path, using the short URL base. This will not follow redirects.
fn reddit_short_head(path: String, quarantine: bool, base_path: &'static str, host: &'static str) -> Boxed<Result<WreqResponse, String>> {
	request(&Method::HEAD, path, false, quarantine, base_path, host, OAUTH_CLIENT.load_full())
}

// /// Makes a HEAD request to Reddit at `path`. This will not follow redirects.
// fn reddit_head(path: String, quarantine: bool) -> Boxed<Result<Response<Body>, String>> {
// 	request(&Method::HEAD, path, false, quarantine, false)
// }
// Unused - reddit_head is only ever called in the context of a short URL

/// Makes a request to Reddit. If `redirect` is `true`, `request_with_redirect`
/// will recurse on the URL that Reddit provides in the Location HTTP header
/// in its response.
fn request(
	method: &'static Method,
	path: String,
	redirect: bool,
	quarantine: bool,
	base_path: &'static str,
	host: &'static str,
	oauth_client: Arc<Oauth>,
) -> Boxed<Result<WreqResponse, String>> {
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

	let mut builder = CLIENT.request(method.clone(), &url);

	for (key, value) in headers {
		builder = builder.header(key, value);
	}

	async move {
		match builder.send().await {
			Ok(response) => {
				// Reddit may respond with a 3xx. Decide whether or not to
				// redirect based on caller params.
				if response.status().is_redirection() {
					if !redirect {
						return Ok(response);
					};
					let location_header = response.headers().get(wreq::header::LOCATION);
					if location_header.and_then(|h| h.to_str().ok()) == Some(ALTERNATIVE_REDDIT_URL_BASE) {
						return Err("Reddit response was invalid".to_string());
					}
					return request(
						method,
						location_header
							.map(|val| {
								// We need to make adjustments to the URI
								// we get back from Reddit. Namely, we
								// must:
								//
								//     1. Remove the authority (e.g.
								//     https://www.reddit.com) that may be
								//     present, so that we recurse on the
								//     path (and query parameters) as
								//     required.
								//
								//     2. Percent-encode the path.
								let new_path = percent_encode(val.as_bytes(), CONTROLS)
									.to_string()
									.trim_start_matches(REDDIT_URL_BASE)
									.trim_start_matches(ALTERNATIVE_REDDIT_URL_BASE)
									.to_string();
								format!("{new_path}{}raw_json=1", if new_path.contains('?') { "&" } else { "?" })
							})
							.unwrap_or_default()
							.to_string(),
						true,
						quarantine,
						base_path,
						host,
						oauth_client,
					)
					.await;
				};

				Ok(response)
			}
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
	json_coalesced(normalize_reddit_api_path(&path), quarantine).await
}

#[cached(size = 1024, time = 2, sync_writes = "by_key")]
async fn json_coalesced(path: String, quarantine: bool) -> Result<Value, String> {
	if is_metadata_path(&path) {
		json_metadata_cached(path, quarantine).await
	} else {
		json_dynamic_cached(path, quarantine).await
	}
}

#[cached(size = 1024, time = 60, result = true, result_fallback = true)]
async fn json_dynamic_cached(path: String, quarantine: bool) -> Result<Value, String> {
	json_uncached(path, quarantine).await
}

#[cached(size = 512, time = 300, result = true, result_fallback = true)]
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
	// Closure to quickly build errors
	let err = |msg: &str, e: String, path: String| -> Result<Value, String> {
		// eprintln!("{} - {}: {}", url, msg, e);
		Err(format!("{msg}: {e} | {path}"))
	};

	if let Some(error) = cooldown_error() {
		return Err(error);
	}

	let _permit = REDDIT_API_CONCURRENCY.acquire().await.map_err(|_| "Reddit request limiter is unavailable".to_string())?;

	// A cooldown may have started while this request was waiting for a permit.
	let mut upstream_attempt = begin_upstream_attempt()?;

	// Keep this exact OAuth client throughout redirects and attach its generation
	// to the response. A late response from an old identity must not overwrite a
	// newly rotated identity's request budget.
	let oauth_client = OAUTH_CLIENT.load_full();
	let reservation = reserve_rate_limit_slot(&OAUTH_RATELIMIT_STATE, oauth_client.generation);
	record_upstream_request(&path);
	let timeout_path = path.clone();

	// Fetch the url...
	let result = tokio::time::timeout(REDDIT_API_REQUEST_TIMEOUT, async {
		match reddit_get(path.clone(), quarantine, oauth_client).await {
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
				let retry_after_duration = parse_delay_seconds(retry_after);
				let quota_headers_present = remaining.is_some() || reset.is_some() || used.is_some();

				if let Some(remaining) = parsed_remaining {
					let response_is_current = update_rate_limit_remaining(&OAUTH_RATELIMIT_STATE, reservation.generation, remaining);
					trace!(
						"Reddit rate-limit state: remaining={remaining} estimated_before={} reset_seconds={} used={} endpoint={} current_generation={response_is_current} rollover={}",
						reservation.previous_remaining,
						reset_duration.map_or(0, |duration| duration.as_secs()),
						parsed_used.map_or(0, u16::from),
						endpoint_class(&path),
						OAUTH_IS_ROLLING_OVER.load(Ordering::SeqCst),
					);

					if response_is_current && remaining == 0 {
						let _ = block_for_rate_limit(reservation.generation, rate_limit_delay(None, reset));
					}
				}

				match classify_throttle_response(status_code, retry_after.is_some(), quota_headers_present) {
					Some(ThrottleKind::Quota) => {
						let delay = rate_limit_delay(retry_after, reset);
						let response_is_current = block_for_rate_limit(reservation.generation, delay);
						warn!(
							"Reddit quota response: status={} endpoint={} retry_after_seconds={} remaining_present={} reset_seconds={} used_present={} current_generation={response_is_current}",
							status,
							endpoint_class(&path),
							retry_after_duration.map_or(0, |duration| duration.as_secs()),
							remaining.is_some(),
							reset_duration.map_or(0, |duration| duration.as_secs()),
							used.is_some(),
						);
						if let Some(remaining) = parsed_remaining {
							maybe_rotate_low_budget(reservation.generation, remaining, parsed_used, reset_duration, &path);
						}
						return Err(format!("Reddit rate limit exceeded. Retry in {} seconds", delay.as_secs().max(1)));
					}
					Some(ThrottleKind::Edge) => {
						let decision = block_for_edge_throttle(&mut upstream_attempt, retry_after_duration);
						match decision {
							decision if decision.started_cooldown => warn!(
								"Reddit edge throttle: status={} endpoint={} retry_after_seconds={} consecutive_failures={} cooldown_seconds={} half_open_probe={}",
								status,
								endpoint_class(&path),
								retry_after_duration.map_or(0, |duration| duration.as_secs()),
								decision.consecutive_failures,
								decision.delay.as_secs(),
								upstream_attempt.edge.half_open,
							),
							decision => trace!(
								"Reddit edge throttle joined existing cooldown: endpoint={} cooldown_seconds={}",
								endpoint_class(&path),
								decision.delay.as_secs(),
							),
						}
						let delay = decision.delay;
						return Err(format!("Reddit is temporarily rejecting this instance. Retry in {} seconds", delay.as_secs().max(1)));
					}
					None => {}
				}

				if status_code == 401 {
					if !is_current_oauth_generation(reservation.generation) {
						return Err("OAuth token changed while this request was in flight. Please retry.".to_string());
					}
					error!("Reddit rejected the OAuth token; forcing a refresh");
					let outcome = force_refresh_token(RefreshReason::Unauthorized).await;
					if let Some(delay) = outcome.retry_after() {
						return Err(format!("OAuth token refresh is temporarily unavailable. Retry in {} seconds", delay.as_secs().max(1)));
					}
					return Err("OAuth token has expired. Please refresh the page!".to_string());
				}

				if status.is_server_error() {
					record_upstream_failure("http_status", Some(status_code), &path, reservation.generation);
					return Err("Reddit is having issues, check if there's an outage".to_string());
				}

				// asynchronously aggregate the chunks of the body
				match hyper::body::aggregate(response.into_hyper_response()).await {
					Ok(body) => {
						let has_remaining = body.has_remaining();

						if !has_remaining {
							record_upstream_failure("empty_body", Some(status.as_u16()), &path, reservation.generation);
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
										if !is_current_oauth_generation(reservation.generation) {
											return Err("OAuth token changed while this request was in flight. Please retry.".to_string());
										}
										error!("Forcing a token refresh");
										let outcome = force_refresh_token(RefreshReason::Unauthorized).await;
										if let Some(delay) = outcome.retry_after() {
											return Err(format!("OAuth token refresh is temporarily unavailable. Retry in {} seconds", delay.as_secs().max(1)));
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
									record_upstream_success(&mut upstream_attempt);
									if let Some(remaining) = parsed_remaining {
										maybe_rotate_low_budget(reservation.generation, remaining, parsed_used, reset_duration, &path);
									}
									Ok(json)
								}
							}
							Err(e) => {
								error!("Got an invalid response from reddit {e}. Status code: {status}");
								record_upstream_failure("invalid_json", Some(status.as_u16()), &path, reservation.generation);
								err("Failed to parse page JSON data", e.to_string(), path)
							}
						}
					}
					Err(e) => {
						record_upstream_failure("body_transport", Some(status.as_u16()), &path, reservation.generation);
						err("Failed receiving body from Reddit", e.to_string(), path)
					}
				}
			}
			Err(e) => {
				record_upstream_failure("request_transport", None, &path, reservation.generation);
				err("Couldn't send request to Reddit", e, path)
			}
		}
	})
	.await;

	match result {
		Ok(result) => result,
		Err(_) => {
			record_upstream_failure("request_timeout", None, &timeout_path, reservation.generation);
			Err(format!("Reddit API request timed out after {} seconds", REDDIT_API_REQUEST_TIMEOUT.as_secs()))
		}
	}
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
		assert_eq!(parse_rate_limit_count(Some("9.6")), Some(10));
		assert_eq!(parse_rate_limit_count(Some("-1")), None);
		assert_eq!(parse_rate_limit_count(Some("NaN")), None);
		assert_eq!(parse_rate_limit_count(Some("not-a-number")), None);
	}

	#[test]
	fn test_rate_limit_delay_adds_margin_and_respects_cap() {
		assert_eq!(rate_limit_delay(Some("1"), None), Duration::from_secs(3));
		assert_eq!(rate_limit_delay(None, Some("20")), Duration::from_secs(22));
		assert_eq!(rate_limit_delay(Some("9999"), None), MAX_RATE_LIMIT_COOLDOWN);
		assert_eq!(rate_limit_delay(None, None), Duration::from_secs(12));
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
		assert_eq!(edge_throttle_delay(1, None), Duration::from_secs(5));
		assert_eq!(edge_throttle_delay(2, None), Duration::from_secs(10));
		assert_eq!(edge_throttle_delay(3, None), Duration::from_secs(20));
		assert_eq!(edge_throttle_delay(4, None), Duration::from_secs(40));
		assert_eq!(edge_throttle_delay(5, None), EDGE_THROTTLE_MAX_COOLDOWN);
		assert_eq!(edge_throttle_delay(8, None), EDGE_THROTTLE_MAX_COOLDOWN);
		assert_eq!(edge_throttle_delay(1, Some(Duration::from_secs(90))), Duration::from_secs(92));
	}

	#[test]
	fn test_rate_limit_counter_does_not_underflow() {
		let counter = AtomicU64::new(rate_limit_state(7, 1));
		assert_eq!(
			reserve_rate_limit_slot(&counter, 7),
			RateLimitReservation {
				generation: 7,
				previous_remaining: 1,
			}
		);
		assert_eq!(counter.load(Ordering::SeqCst), rate_limit_state(7, 0));
		assert_eq!(reserve_rate_limit_slot(&counter, 7).previous_remaining, 0);
		assert_eq!(counter.load(Ordering::SeqCst), rate_limit_state(7, 0));
	}

	#[test]
	fn test_stale_rate_limit_responses_cannot_replace_new_budget() {
		let counter = AtomicU64::new(rate_limit_state(3, 99));
		assert!(!update_rate_limit_remaining(&counter, 2, 0));
		assert_eq!(counter.load(Ordering::SeqCst), rate_limit_state(3, 99));
		assert!(update_rate_limit_remaining(&counter, 3, 88));
		assert_eq!(counter.load(Ordering::SeqCst), rate_limit_state(3, 88));
	}

	#[test]
	fn test_oauth_install_only_resets_budget_for_fresh_identity() {
		let current = rate_limit_state(4, 7);
		assert_eq!(state_for_oauth_install(current, 5, false), rate_limit_state(5, 7));
		assert_eq!(state_for_oauth_install(current, 5, true), rate_limit_state(5, 99));
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
		assert!(guard.active_cooldown(now + FAILURE_COOLDOWN + Duration::from_secs(3)).is_none());
		guard.reset_failure_window();
		assert_eq!(guard.failures_in_window, 0);
	}

	#[test]
	fn test_edge_throttle_uses_one_probe_and_escalates_until_success() {
		let now = Instant::now();
		let mut guard = UpstreamGuard::default();
		let original = guard.begin_attempt(now).unwrap();
		let first = guard.record_edge_throttle(now, original, Some(Duration::from_secs(2)));
		assert_eq!(
			first,
			EdgeThrottleDecision {
				delay: Duration::from_secs(5),
				consecutive_failures: 1,
				started_cooldown: true,
			}
		);
		let concurrent = guard.record_edge_throttle(now + Duration::from_secs(1), original, Some(Duration::from_secs(2)));
		assert_eq!(concurrent.consecutive_failures, 1);
		assert!(!concurrent.started_cooldown);

		let probe = guard.begin_attempt(now + Duration::from_secs(6)).unwrap();
		assert!(probe.half_open);
		assert!(guard.begin_attempt(now + Duration::from_secs(6)).is_err());
		let second = guard.record_edge_throttle(now + Duration::from_secs(6), probe, Some(Duration::from_secs(2)));
		assert_eq!(second.delay, Duration::from_secs(10));
		assert_eq!(second.consecutive_failures, 2);
		assert!(second.started_cooldown);

		let recovery_probe = guard.begin_attempt(now + Duration::from_secs(17)).unwrap();
		guard.record_api_success(recovery_probe);
		let recovered_attempt = guard.begin_attempt(now + Duration::from_secs(17)).unwrap();
		let recovered = guard.record_edge_throttle(now + Duration::from_secs(17), recovered_attempt, Some(Duration::from_secs(2)));
		assert_eq!(recovered.delay, Duration::from_secs(5));
		assert_eq!(recovered.consecutive_failures, 1);
	}

	#[test]
	fn test_oauth_refresh_only_clears_rate_limit_cooldown() {
		let now = Instant::now();
		let mut guard = UpstreamGuard::default();
		let attempt = guard.begin_attempt(now).unwrap();
		guard.record_edge_throttle(now, attempt, None);
		guard.block_for_rate_limit(now, Duration::from_secs(20));
		guard.reset_failure_window();
		guard.clear_rate_limit_block();
		assert_eq!(guard.active_cooldown(now).map(|(_, reason)| reason), Some(CooldownReason::EdgeThrottle));
		assert_eq!(guard.edge_throttle_failures, 1);
	}

	#[test]
	fn test_response_started_before_edge_denial_cannot_close_circuit() {
		let now = Instant::now();
		let mut guard = UpstreamGuard::default();
		let denied = guard.begin_attempt(now).unwrap();
		let late_success = guard.begin_attempt(now).unwrap();
		guard.record_edge_throttle(now, denied, None);
		guard.record_api_success(late_success);
		assert!(matches!(guard.edge_state, EdgeCircuitState::Open { .. }));
		assert_eq!(guard.edge_throttle_failures, 1);
	}

	#[test]
	fn test_abandoned_half_open_probe_reopens_edge_circuit() {
		let now = Instant::now();
		let mut guard = UpstreamGuard::default();
		let attempt = guard.begin_attempt(now).unwrap();
		guard.record_edge_throttle(now, attempt, None);
		let probe = guard.begin_attempt(now + Duration::from_secs(6)).unwrap();
		guard.abandon_edge_probe(now + Duration::from_secs(6), probe);
		assert!(matches!(guard.edge_state, EdgeCircuitState::Open { .. }));
		assert!(guard.begin_attempt(now + Duration::from_secs(7)).is_err());
	}

	#[test]
	fn test_expired_half_open_probe_cannot_block_or_recover_circuit() {
		let now = Instant::now();
		let mut guard = UpstreamGuard::default();
		let attempt = guard.begin_attempt(now).unwrap();
		guard.record_edge_throttle(now, attempt, None);
		let stale_probe = guard.begin_attempt(now + Duration::from_secs(6)).unwrap();
		assert!(guard.begin_attempt(now + Duration::from_secs(37)).is_err());
		guard.record_api_success(stale_probe);
		assert!(matches!(guard.edge_state, EdgeCircuitState::Open { .. }));
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
	}

	#[test]
	fn test_metadata_cache_policy_is_narrow() {
		assert!(is_metadata_path("/r/rust/about.json?raw_json=1"));
		assert!(is_metadata_path("/r/rust/wiki/index.json?raw_json=1"));
		assert!(is_metadata_path("/subreddits/search.json?q=rust&raw_json=1"));
		assert!(!is_metadata_path("/r/rust/hot.json?raw_json=1"));
		assert!(!is_metadata_path("/comments/abc.json?raw_json=1"));
		assert!(!is_metadata_path("/comments/about.json?raw_json=1"));
		assert!(!is_metadata_path("/r/rust/comments/abc/wiki/def.json?raw_json=1"));
		assert!(!is_metadata_path("/r/random/about.json?raw_json=1"));
	}

	#[test]
	fn test_endpoint_class_does_not_log_resource_names() {
		assert_eq!(endpoint_class("/r/example/hot.json?raw_json=1"), "subreddit");
		assert_eq!(endpoint_class("/r/example/comments/abc/title.json"), "comments");
		assert_eq!(endpoint_class("/user/example/about.json"), "user");
		assert_eq!(endpoint_class("/search.json?q=private"), "search");
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
