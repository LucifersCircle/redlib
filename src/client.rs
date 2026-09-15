use crate::dbg_msg;
use crate::oauth::{force_refresh_token, should_attempt_refresh, token_daemon, Oauth, OauthBackendImpl};
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
use std::sync::atomic::{AtomicBool, AtomicU16};
use std::sync::{LazyLock, Mutex};
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

pub static OAUTH_RATELIMIT_REMAINING: AtomicU16 = AtomicU16::new(99);

pub static OAUTH_IS_ROLLING_OVER: AtomicBool = AtomicBool::new(false);

const DEFAULT_MAX_CONCURRENT_API_REQUESTS: usize = 8;
const MAX_CONFIGURED_API_REQUESTS: usize = 64;
const FAILURE_WINDOW: Duration = Duration::from_secs(10);
const FAILURE_COOLDOWN: Duration = Duration::from_secs(10);
const FAILURE_THRESHOLD: u8 = 3;
const DEFAULT_RATE_LIMIT_COOLDOWN: Duration = Duration::from_secs(10);
const MAX_RATE_LIMIT_COOLDOWN: Duration = Duration::from_secs(600);
const RATE_LIMIT_COOLDOWN_MARGIN: Duration = Duration::from_secs(2);

static REDDIT_API_CONCURRENCY: LazyLock<Semaphore> = LazyLock::new(|| {
	let configured = max_concurrent_api_requests();
	info!("Reddit API concurrency limit: {configured}");
	Semaphore::new(configured)
});
static UPSTREAM_GUARD: LazyLock<Mutex<UpstreamGuard>> = LazyLock::new(|| Mutex::new(UpstreamGuard::default()));

#[derive(Debug, Default)]
struct UpstreamGuard {
	failure_window_started: Option<Instant>,
	failures_in_window: u8,
	blocked_until: Option<Instant>,
}

impl UpstreamGuard {
	fn cooldown_remaining(&self, now: Instant) -> Option<Duration> {
		self.blocked_until.and_then(|deadline| deadline.checked_duration_since(now))
	}

	fn block_for(&mut self, now: Instant, duration: Duration) {
		let deadline = now + duration.min(MAX_RATE_LIMIT_COOLDOWN);
		if self.blocked_until.map_or(true, |current| deadline > current) {
			self.blocked_until = Some(deadline);
		}
	}

	fn record_failure(&mut self, now: Instant) -> bool {
		if self.failure_window_started.map_or(true, |started| now.duration_since(started) > FAILURE_WINDOW) {
			self.failure_window_started = Some(now);
			self.failures_in_window = 0;
		}

		self.failures_in_window = self.failures_in_window.saturating_add(1);
		if self.failures_in_window >= FAILURE_THRESHOLD {
			self.block_for(now, FAILURE_COOLDOWN);
			self.failure_window_started = None;
			self.failures_in_window = 0;
			true
		} else {
			false
		}
	}

	fn record_success(&mut self) {
		self.failure_window_started = None;
		self.failures_in_window = 0;
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

fn rate_limit_delay(retry_after: Option<&str>, reset: Option<&str>) -> Duration {
	parse_delay_seconds(retry_after)
		.or_else(|| parse_delay_seconds(reset))
		.unwrap_or(DEFAULT_RATE_LIMIT_COOLDOWN)
		.saturating_add(RATE_LIMIT_COOLDOWN_MARGIN)
		.min(MAX_RATE_LIMIT_COOLDOWN)
}

fn reserve_rate_limit_slot(counter: &AtomicU16) -> u16 {
	counter
		.fetch_update(Ordering::SeqCst, Ordering::SeqCst, |value| Some(value.saturating_sub(1)))
		.unwrap_or_else(|value| value)
}

fn endpoint_class(path: &str) -> &'static str {
	match path.split('?').next().unwrap_or_default().split('/').nth(1) {
		Some("r") => "subreddit",
		Some("user") => "user",
		Some("api") => "api",
		Some("search.json") => "search",
		Some("comments") => "comments",
		_ => "other",
	}
}

fn upstream_guard() -> std::sync::MutexGuard<'static, UpstreamGuard> {
	UPSTREAM_GUARD.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn cooldown_error() -> Option<String> {
	upstream_guard().cooldown_remaining(Instant::now()).map(|remaining| {
		format!(
			"Reddit requests are temporarily paused after upstream failures. Retry in {} seconds",
			remaining.as_secs().max(1)
		)
	})
}

fn block_for_rate_limit(duration: Duration) {
	upstream_guard().block_for(Instant::now(), duration);
}

fn record_upstream_failure(kind: &str, status: Option<u16>, path: &str) {
	let opened = upstream_guard().record_failure(Instant::now());
	warn!(
		"Reddit upstream failure: kind={kind} status={} endpoint={} circuit_opened={opened}",
		status.map_or_else(|| "transport".to_string(), |status| status.to_string()),
		endpoint_class(path),
	);
}

fn record_upstream_success() {
	upstream_guard().record_success();
}

const URL_PAIRS: [(&str, &str); 2] = [
	(ALTERNATIVE_REDDIT_URL_BASE, ALTERNATIVE_REDDIT_URL_BASE_HOST),
	(REDDIT_SHORT_URL_BASE, REDDIT_SHORT_URL_BASE_HOST),
];

pub fn build_client() -> WreqClient {
	// Keeping this list short to aid in privacy.
	// The more emulations, the more unique a fingerprint each instance has.
	// But some emulations should increase evasiveness.
	let emulation = [Emulation::Chrome145, Emulation::Firefox147];
	let emulation_os = [EmulationOS::Android, EmulationOS::Windows];

	let rand = fastrand::usize(..);
	let emulation = EmulationOption::builder()
		.emulation(emulation[rand % emulation.len()])
		.emulation_os(emulation_os[rand % emulation_os.len()])
		.build()
		.emulation();

	info!("Building Wreq client with random emulation {:?}", emulation);
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
fn reddit_get(path: String, quarantine: bool) -> Boxed<Result<WreqResponse, String>> {
	request(&Method::GET, path, true, quarantine, REDDIT_URL_BASE, REDDIT_URL_BASE_HOST)
}

/// Makes a HEAD request to Reddit at `path, using the short URL base. This will not follow redirects.
fn reddit_short_head(path: String, quarantine: bool, base_path: &'static str, host: &'static str) -> Boxed<Result<WreqResponse, String>> {
	request(&Method::HEAD, path, false, quarantine, base_path, host)
}

// /// Makes a HEAD request to Reddit at `path`. This will not follow redirects.
// fn reddit_head(path: String, quarantine: bool) -> Boxed<Result<Response<Body>, String>> {
// 	request(&Method::HEAD, path, false, quarantine, false)
// }
// Unused - reddit_head is only ever called in the context of a short URL

/// Makes a request to Reddit. If `redirect` is `true`, `request_with_redirect`
/// will recurse on the URL that Reddit provides in the Location HTTP header
/// in its response.
fn request(method: &'static Method, path: String, redirect: bool, quarantine: bool, base_path: &'static str, host: &'static str) -> Boxed<Result<WreqResponse, String>> {
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

	{
		let client = OAUTH_CLIENT.load_full();
		for (key, value) in client.headers_map.clone() {
			headers.push((key, value));
		}
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
/// caches errors. The inner cache keeps successful responses longer and can
/// serve its most recent success if a refresh fails.
#[cached(size = 1024, time = 2, sync_writes = "by_key")]
pub async fn json(path: String, quarantine: bool) -> Result<Value, String> {
	json_cached(path, quarantine).await
}

#[cached(size = 1024, time = 60, result = true, result_fallback = true)]
async fn json_cached(path: String, quarantine: bool) -> Result<Value, String> {
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
	if let Some(error) = cooldown_error() {
		return Err(error);
	}

	// Reserve estimated OAuth budget only when an upstream call will be made.
	let current_rate_limit = OAUTH_RATELIMIT_REMAINING.load(Ordering::SeqCst);
	let is_rolling_over = OAUTH_IS_ROLLING_OVER.load(Ordering::SeqCst);
	if current_rate_limit < 10 && should_attempt_refresh() {
		warn!("Rate limit {current_rate_limit} is low. Spawning force_refresh_token()");
		tokio::spawn(force_refresh_token());
	}
	reserve_rate_limit_slot(&OAUTH_RATELIMIT_REMAINING);

	// Fetch the url...
	match reddit_get(path.clone(), quarantine).await {
		Ok(response) => {
			let status = response.status();

			let remaining = response.headers().get("x-ratelimit-remaining").and_then(|value| value.to_str().ok());
			let reset = response.headers().get("x-ratelimit-reset").and_then(|value| value.to_str().ok());
			let used = response.headers().get("x-ratelimit-used").and_then(|value| value.to_str().ok());
			let retry_after = response.headers().get(wreq_header::RETRY_AFTER).and_then(|value| value.to_str().ok());

			if let (Some(remaining), Some(reset), Some(used)) = (remaining, reset, used) {
				trace!(
					"Ratelimit remaining: Header says {remaining}, we have {current_rate_limit}. Resets in {reset}. Rollover: {}. Ratelimit used: {used}",
					if is_rolling_over { "yes" } else { "no" },
				);

				// If can parse remaining as a float, round to a u16 and save
				if let Ok(val) = remaining.parse::<f32>() {
					OAUTH_RATELIMIT_REMAINING.store(val.round() as u16, Ordering::SeqCst);
					if val <= 0.0 {
						block_for_rate_limit(rate_limit_delay(None, Some(reset)));
					}
				}
			}

			if status.as_u16() == 429 || (status.as_u16() == 403 && retry_after.is_some()) {
				let delay = rate_limit_delay(retry_after, reset);
				block_for_rate_limit(delay);
				warn!(
					"Reddit rate limit response: status={} endpoint={} retry_after_present={} reset_present={}",
					status,
					endpoint_class(&path),
					retry_after.is_some(),
					reset.is_some(),
				);
				return Err(format!("Reddit rate limit exceeded. Retry in {} seconds", delay.as_secs().max(1)));
			}

			if status.as_u16() == 401 {
				error!("Reddit rejected the OAuth token; forcing a refresh");
				let outcome = force_refresh_token().await;
				if let Some(delay) = outcome.retry_after() {
					return Err(format!("OAuth token refresh is temporarily unavailable. Retry in {} seconds", delay.as_secs().max(1)));
				}
				return Err("OAuth token has expired. Please refresh the page!".to_string());
			}

			// asynchronously aggregate the chunks of the body
			match hyper::body::aggregate(response.into_hyper_response()).await {
				Ok(body) => {
					let has_remaining = body.has_remaining();

					if !has_remaining {
						record_upstream_failure("empty_body", Some(status.as_u16()), &path);
						return Err(format!("Reddit returned an empty response (status {status})"));
					}

					// Parse the response from Reddit as JSON
					match serde_json::from_reader(body.reader()) {
						Ok(value) => {
							let json: Value = value;
							record_upstream_success();

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
									error!("Forcing a token refresh");
									let outcome = force_refresh_token().await;
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
							} else {
								Ok(json)
							}
						}
						Err(e) => {
							error!("Got an invalid response from reddit {e}. Status code: {status}");
							record_upstream_failure("invalid_json", Some(status.as_u16()), &path);
							if status.is_server_error() {
								Err("Reddit is having issues, check if there's an outage".to_string())
							} else {
								err("Failed to parse page JSON data", e.to_string(), path)
							}
						}
					}
				}
				Err(e) => {
					record_upstream_failure("body_transport", Some(status.as_u16()), &path);
					err("Failed receiving body from Reddit", e.to_string(), path)
				}
			}
		}
		Err(e) => {
			record_upstream_failure("request_transport", None, &path);
			err("Couldn't send request to Reddit", e, path)
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
	// First, test the Oauth client: we can perform a rate limit check if the OAuth backend is MobileSpoof; if GenericWeb, we skip the check.
	if matches!(OAUTH_CLIENT.load().backend, OauthBackendImpl::GenericWeb(_)) {
		warn!("[⚠️] Cannot perform rate limit check, running as GenericWeb. Skipping check.");
		return Ok(());
	}

	// First, check a subreddit.
	self_check("reddit").await?;
	// This will reduce the rate limit to 99. Assert this check.
	if OAUTH_RATELIMIT_REMAINING.load(Ordering::SeqCst) != 99 {
		return Err(format!("Rate limit check 1 failed: expected 99, got {}", OAUTH_RATELIMIT_REMAINING.load(Ordering::SeqCst)));
	}
	// Now, we switch out the OAuth client.
	// This checks for the IP rate limit association.
	let outcome = force_refresh_token().await;
	if !outcome.was_refreshed() {
		return Err(match outcome.retry_after() {
			Some(delay) => format!("OAuth rollover check could not refresh the token; retry available in {} seconds", delay.as_secs().max(1)),
			None => "OAuth rollover check found another refresh already in progress".to_string(),
		});
	}
	// Now, check a new sub to break cache.
	self_check("rust").await?;
	// Again, assert the rate limit check.
	if OAUTH_RATELIMIT_REMAINING.load(Ordering::SeqCst) != 99 {
		return Err(format!("Rate limit check 2 failed: expected 99, got {}", OAUTH_RATELIMIT_REMAINING.load(Ordering::SeqCst)));
	}

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
	fn test_rate_limit_delay_adds_margin_and_respects_cap() {
		assert_eq!(rate_limit_delay(Some("1"), None), Duration::from_secs(3));
		assert_eq!(rate_limit_delay(None, Some("20")), Duration::from_secs(22));
		assert_eq!(rate_limit_delay(Some("9999"), None), MAX_RATE_LIMIT_COOLDOWN);
		assert_eq!(rate_limit_delay(None, None), Duration::from_secs(12));
	}

	#[test]
	fn test_rate_limit_counter_does_not_underflow() {
		let counter = AtomicU16::new(1);
		assert_eq!(reserve_rate_limit_slot(&counter), 1);
		assert_eq!(counter.load(Ordering::SeqCst), 0);
		assert_eq!(reserve_rate_limit_slot(&counter), 0);
		assert_eq!(counter.load(Ordering::SeqCst), 0);
	}

	#[test]
	fn test_upstream_guard_opens_after_burst_and_recovers() {
		let now = Instant::now();
		let mut guard = UpstreamGuard::default();
		assert!(!guard.record_failure(now));
		assert!(!guard.record_failure(now + Duration::from_secs(1)));
		assert!(guard.record_failure(now + Duration::from_secs(2)));
		assert!(guard.cooldown_remaining(now + Duration::from_secs(3)).is_some());
		assert!(guard.cooldown_remaining(now + FAILURE_COOLDOWN + Duration::from_secs(3)).is_none());
		guard.record_success();
		assert_eq!(guard.failures_in_window, 0);
	}

	#[test]
	fn test_endpoint_class_does_not_log_resource_names() {
		assert_eq!(endpoint_class("/r/example/hot.json?raw_json=1"), "subreddit");
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
