use super::*;
use sealed_test::prelude::*;
use std::sync::atomic::AtomicUsize;

#[derive(Clone, Copy, PartialEq)]
enum Scenario {
	CompleteLow,
	First429,
	Final429,
	FinalThenFirst429,
	FirstInvalid,
	MalformedFirstFeed,
	FinalInvalid,
	Publication429,
	FirstQuotaBody,
	FinalQuotaBody,
	FirstZero,
	LateContinuation,
	Late401,
	LateSession,
	LateQuota,
	AuthFailure,
}

struct Fixture {
	origin: RedditOrigin,
	server: tokio::task::JoinHandle<Result<(), hyper::Error>>,
	auth_calls: Arc<AtomicUsize>,
	wires: Arc<Mutex<Vec<(String, String)>>>,
	devices: Arc<Mutex<Vec<String>>>,
	auth_seen: Arc<Notify>,
	auth_release: Arc<Notify>,
	data_seen: Arc<Notify>,
	data_release: Arc<Notify>,
}
impl Drop for Fixture {
	fn drop(&mut self) {
		self.server.abort();
	}
}
impl Fixture {
	async fn new(scenario: Scenario, hold_auth: bool) -> Self {
		use hyper::service::{make_service_fn, service_fn};
		use std::convert::Infallible;
		let auth_calls = Arc::new(AtomicUsize::new(0));
		let wires = Arc::new(Mutex::new(Vec::new()));
		let devices = Arc::new(Mutex::new(Vec::new()));
		let auth_seen = Arc::new(Notify::new());
		let auth_release = Arc::new(Notify::new());
		let data_seen = Arc::new(Notify::new());
		let data_release = Arc::new(Notify::new());
		let shared = (
			auth_calls.clone(),
			wires.clone(),
			devices.clone(),
			auth_seen.clone(),
			auth_release.clone(),
			data_seen.clone(),
			data_release.clone(),
		);
		let service = make_service_fn(move |_| {
			let shared = shared.clone();
			async move {
				Ok::<_, Infallible>(service_fn(move |request: HyperRequest<Body>| {
					let (auth_calls, wires, devices, auth_seen, auth_release, data_seen, data_release) = shared.clone();
					async move {
						if request.uri().path().starts_with("/auth/") {
							assert!(request.headers()["authorization"].to_str().unwrap().starts_with("Basic "));
							devices.lock().unwrap().push(request.headers()["x-reddit-device-id"].to_str().unwrap().to_string());
							auth_calls.fetch_add(1, Ordering::SeqCst);
							auth_seen.notify_one();
							if hold_auth {
								auth_release.notified().await;
							}
							let response = if scenario == Scenario::AuthFailure {
								HyperResponse::builder().status(503).body(Body::from("unavailable")).unwrap()
							} else {
								HyperResponse::new(Body::from(r#"{"access_token":"renewed-fixture","expires_in":3600}"#))
							};
							return Ok::<_, Infallible>(response);
						}
						let bearer = request.headers()["authorization"].to_str().unwrap().to_string();
						let payload: Value = serde_json::from_slice(&hyper::body::to_bytes(request.into_body()).await.unwrap()).unwrap();
						let op = payload["operationName"].as_str().unwrap().to_string();
						wires.lock().unwrap().push((bearer.clone(), op.clone()));
						let old = bearer == "Bearer local-test-fixture";
						let first = wires.lock().unwrap().iter().filter(|(b, _)| b == &bearer).count() == 1;
						if old && ((scenario == Scenario::LateContinuation && first) || (matches!(scenario, Scenario::Late401 | Scenario::LateSession | Scenario::LateQuota) && !first)) {
							data_seen.notify_one();
							data_release.notified().await;
						}
						let quota = old
							&& ((matches!(scenario, Scenario::First429 | Scenario::Publication429 | Scenario::FirstQuotaBody) && first)
								|| (matches!(scenario, Scenario::FinalQuotaBody | Scenario::LateQuota | Scenario::Final429) && !first))
							|| (scenario == Scenario::FinalThenFirst429 && ((old && !first) || (!old && first)));
						let remaining = if quota || (old && scenario == Scenario::FirstZero && first) {
							0
						} else if old && scenario == Scenario::CompleteLow {
							if first {
								20
							} else {
								19
							}
						} else {
							if first {
								100
							} else {
								99
							}
						};
						let value = if quota
							&& !matches!(
								scenario,
								Scenario::First429 | Scenario::Publication429 | Scenario::LateQuota | Scenario::Final429 | Scenario::FinalThenFirst429
							) {
							serde_json::json!({"errors":[{"extensions":{"code":"RATE_LIMITED"}}]})
						} else if old && ((scenario == Scenario::FirstInvalid && first) || (scenario == Scenario::FinalInvalid && !first)) {
							serde_json::json!({"errors":[{"extensions":{"code":"UNRECOGNIZED_ERROR"}}]})
						} else if scenario == Scenario::MalformedFirstFeed && op == "AllFeedSdui" {
							serde_json::json!({"data":{"allV3":{"elements":{"pageInfo":{"hasNextPage":false}}}}})
						} else if old && scenario == Scenario::LateSession && !first {
							serde_json::json!({"errors":[{"extensions":{"code":"UNAUTHENTICATED"}}]})
						} else {
							match op.as_str() {
								"AllFeedSdui" => {
									serde_json::json!({"data":{"allV3":{"elements":{"edges":[{"node":{"groupId":"t3_abc"}}],"pageInfo":{"hasNextPage":false,"endCursor":null}}}}})
								}
								"PostsByIds" => {
									serde_json::json!({"data":{"postsInfoByIds":[{"__typename":"SubredditPost","id":"t3_abc","postTitle":"Fixture","subreddit":{"name":"fixture","type":"PUBLIC"}}]}})
								}
								"PostComments" => serde_json::json!({"data":{"postInfoById":{"id":"t3_abc","commentForest":{"trees":[]}}}}),
								other => panic!("unexpected operation {other}"),
							}
						};
						let status = if quota
							&& matches!(
								scenario,
								Scenario::First429 | Scenario::Publication429 | Scenario::LateQuota | Scenario::Final429 | Scenario::FinalThenFirst429
							) {
							429
						} else if old && scenario == Scenario::Late401 && !first {
							401
						} else {
							200
						};
						let mut response = HyperResponse::builder()
							.status(status)
							.header("content-type", "application/json")
							.header("x-ratelimit-remaining", remaining.to_string())
							.header("x-ratelimit-reset", if scenario == Scenario::Publication429 { "25" } else { "60" });
						if status == 401 {
							response = response.header("www-authenticate", "Bearer error=\"invalid_token\"");
						}
						Ok::<_, Infallible>(response.body(Body::from(value.to_string())).unwrap())
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
		Self {
			origin,
			server: tokio::spawn(server),
			auth_calls,
			wires,
			devices,
			auth_seen,
			auth_release,
			data_seen,
			data_release,
		}
	}
	fn install(&self) -> String {
		let lane = RedditLane::Tor;
		*upstream_guard(lane) = UpstreamGuard::new(lane);
		let fixture = Oauth::request_fixture_with_refresh(lane, Arc::new(WreqClient::builder().no_proxy().build().unwrap()), self.origin);
		let device = fixture.headers_map["X-Reddit-Device-Id"].clone();
		assert!(install_oauth_client(fixture, true, None));
		let mut guard = upstream_guard(lane);
		let mut prior = guard.try_admit_endpoint(Instant::now(), 0, 4).unwrap();
		guard.reconcile_quota(Instant::now(), &mut prior, Some(100), Some(Duration::from_secs(60)), false);
		prior.complete();
		device
	}
	async fn refreshed(&self) {
		tokio::time::timeout(Duration::from_secs(3), async {
			loop {
				if oauth_client(RedditLane::Tor).unwrap().generation == 1 && !TOR_OAUTH_IS_ROLLING_OVER.load(Ordering::SeqCst) {
					break;
				}
				tokio::task::yield_now().await;
			}
		})
		.await
		.expect("loopback renewal installed");
	}
	fn renew_old_budget(&self, rest: bool) {
		let mut guard = upstream_guard(RedditLane::Tor);
		let budget = if rest { QuotaBudget::Rest } else { QuotaBudget::Graphql };
		if rest {
			guard.quota_rotation_armed = true;
			guard.quota.epoch = 17;
		}
		guard.quota_budget_mut(budget).window = QuotaWindow::Known {
			available: 0,
			reset_at: Instant::now() + Duration::from_secs(60),
		};
		if !rest {
			guard.graphql_rate_limit_blocked_until = Some(Instant::now() + Duration::from_secs(60));
		}
		let ticket = guard.quota_rotation_candidate_for_budget(Instant::now(), 0, QuotaRotationMode::Emergency, budget).unwrap();
		drop(guard);
		assert!(spawn_rate_limit_refresh(ticket));
	}
}

#[test]
fn test_tor_graphql_quota_rotation_scope_and_safety() {
	let now = Instant::now();
	let mut guard = UpstreamGuard::new(RedditLane::Tor);
	guard.install_oauth_generation(7, true);
	guard.graphql_rotation_armed = true;
	guard.graphql_quota.window = QuotaWindow::Known {
		available: 0,
		reset_at: now + Duration::from_secs(60),
	};
	guard.graphql_rate_limit_blocked_until = Some(now + Duration::from_secs(60));
	guard.onion_transport.blocked_until = Some(now + Duration::from_secs(60));
	let ticket = guard
		.quota_rotation_candidate_for_budget(now, 7, QuotaRotationMode::Emergency, QuotaBudget::Graphql)
		.expect("own quota pause and onion outage do not block public GraphQL renewal");
	assert!(!guard.claim_quota_rotation(
		now,
		QuotaRotationTicket {
			budget: QuotaBudget::Rest,
			..ticket
		}
	));
	assert!(guard.claim_quota_rotation(now, ticket));
	assert!(guard.quota_rotation_still_needed(now, ticket));
	assert!(guard.completed_quota_rotation_still_valid(now, ticket));
	assert!(guard
		.quota_rotation_candidate_for_budget(now, 7, QuotaRotationMode::Emergency, QuotaBudget::Graphql)
		.is_none());
	guard.transport.blocked_until = Some(now + Duration::from_secs(60));
	assert!(!guard.quota_rotation_still_needed(now, ticket));
	guard.transport.blocked_until = None;
	guard.graphql_quota.window = QuotaWindow::Known {
		available: 0,
		reset_at: now + Duration::from_secs(5),
	};
	assert!(!guard.quota_rotation_still_needed(now, ticket));
	guard.install_oauth_generation(8, true);
	assert!(!guard.graphql_rotation_armed);
	assert!(matches!(guard.graphql_quota.window, QuotaWindow::Unknown { .. }));
	let mut low = guard.try_admit_endpoint(now, 8, 4).unwrap();
	guard.reconcile_quota(now, &mut low, Some(29), Some(Duration::from_secs(60)), false);
	low.complete();
	assert!(!guard.graphql_rotation_armed, "fresh low discovery cannot start an authentication loop");
	let mut direct = UpstreamGuard::new(RedditLane::Direct);
	direct.graphql_rotation_armed = true;
	direct.graphql_quota.window = QuotaWindow::Known {
		available: 0,
		reset_at: now + Duration::from_secs(60),
	};
	assert!(direct
		.quota_rotation_candidate_for_budget(now, 0, QuotaRotationMode::Emergency, QuotaBudget::Graphql)
		.is_none());
}

#[test]
#[sealed_test(env=[("REDLIB_TOR_FALLBACK","on"),("REDLIB_REDDIT_PRIMARY_LANE","tor")])]
fn test_tor_graphql_quota_proactive_after_complete_read_changes_real_device_and_bearer() {
	tokio::runtime::Runtime::new().unwrap().block_on(async {
		for path in ["/comments/abc.json", "/r/all/hot.json"] {
			let fixture = Fixture::new(Scenario::CompleteLow, true).await;
			let old_device = fixture.install();
			let (result, _) = tokio::time::timeout(Duration::from_secs(3), json_uncached_on_lane(path.into(), false, RedditLane::Tor))
				.await
				.unwrap();
			result.unwrap();
			fixture.auth_seen.notified().await;
			assert_eq!(fixture.wires.lock().unwrap().len(), 2);
			assert!(fixture.wires.lock().unwrap().iter().all(|(b, _)| b == "Bearer local-test-fixture"));
			assert_eq!(oauth_client(RedditLane::Tor).unwrap().generation, 0, "auth has not completed");
			let auth_device = fixture.devices.lock().unwrap()[0].clone();
			assert_ne!(auth_device, old_device);
			fixture.auth_release.notify_one();
			fixture.refreshed().await;
			let current = oauth_client(RedditLane::Tor).unwrap();
			assert_eq!(current.headers_map["Authorization"], "Bearer renewed-fixture");
			assert_eq!(current.headers_map["X-Reddit-Device-Id"], auth_device);
			let guard = upstream_guard(RedditLane::Tor);
			assert!(!guard.graphql_rotation_armed);
			assert!(matches!(guard.graphql_quota.window, QuotaWindow::Unknown { .. }));
			drop(guard);
			assert_eq!(fixture.auth_calls.load(Ordering::SeqCst), 1);
		}
	});
}

#[test]
#[sealed_test(env=[("REDLIB_TOR_FALLBACK","on"),("REDLIB_REDDIT_PRIMARY_LANE","tor")])]
fn test_tor_graphql_quota_active_pause_joins_one_refresh_without_capacity() {
	tokio::runtime::Runtime::new().unwrap().block_on(async {
		let fixture = Fixture::new(Scenario::FirstZero, true).await;
		fixture.install();
		{
			let mut guard = upstream_guard(RedditLane::Tor);
			guard.graphql_quota.window = QuotaWindow::Known {
				available: 0,
				reset_at: Instant::now() + Duration::from_secs(60),
			};
			guard.graphql_rate_limit_blocked_until = Some(Instant::now() + Duration::from_secs(60));
		}
		let capacity = api_concurrency(RedditLane::Tor).available_permits();
		let read = tokio::spawn(json_uncached_on_lane("/comments/abc.json".into(), false, RedditLane::Tor));
		fixture.auth_seen.notified().await;
		assert!(upstream_guard(RedditLane::Tor).graphql_rate_limit_blocked_until.is_some());
		assert_eq!(api_concurrency(RedditLane::Tor).available_permits(), capacity);
		assert_eq!(TOR_RECOVERY_PROBE.available_permits(), 1);
		assert!(fixture.wires.lock().unwrap().is_empty());
		fixture.auth_release.notify_one();
		let (result, _) = tokio::time::timeout(Duration::from_secs(3), read).await.unwrap().unwrap();
		result.unwrap();
		assert_eq!(fixture.auth_calls.load(Ordering::SeqCst), 1);
		assert_eq!(fixture.wires.lock().unwrap().len(), 2);
		assert!(fixture.wires.lock().unwrap().iter().all(|(b, _)| b == "Bearer renewed-fixture"));
	});
}

#[test]
#[sealed_test(env=[("REDLIB_TOR_FALLBACK","on"),("REDLIB_REDDIT_PRIMARY_LANE","tor")])]
fn test_tor_graphql_quota_exhausted_operations_replay_whole_read_once() {
	tokio::runtime::Runtime::new().unwrap().block_on(async {
		for scenario in [Scenario::First429, Scenario::FirstQuotaBody, Scenario::FinalQuotaBody, Scenario::FirstZero] {
			for path in ["/comments/abc.json", "/r/all/hot.json"] {
				let fixture = Fixture::new(scenario, false).await;
				fixture.install();
				let (result, _) = tokio::time::timeout(Duration::from_secs(4), json_uncached_on_lane(path.into(), false, RedditLane::Tor))
					.await
					.unwrap();
				result.unwrap();
				let wires = fixture.wires.lock().unwrap();
				let old: Vec<_> = wires.iter().filter(|(b, _)| b == "Bearer local-test-fixture").collect();
				let new: Vec<_> = wires.iter().filter(|(b, _)| b == "Bearer renewed-fixture").collect();
				assert_eq!(old.len(), if scenario == Scenario::FinalQuotaBody { 2 } else { 1 });
				assert_eq!(new.len(), 2);
				assert_eq!(old[0].1, new[0].1, "renewal restarts the complete logical read");
				assert_eq!(fixture.auth_calls.load(Ordering::SeqCst), 1);
			}
		}
	});
}

#[test]
#[sealed_test(env=[("REDLIB_TOR_FALLBACK","on"),("REDLIB_REDDIT_PRIMARY_LANE","tor")])]
fn test_tor_graphql_quota_confirmed_renewal_replays_late_continuation_and_auth_errors() {
	tokio::runtime::Runtime::new().unwrap().block_on(async {
		for (scenario, rest) in [
			(Scenario::LateContinuation, false),
			(Scenario::LateContinuation, true),
			(Scenario::Late401, false),
			(Scenario::LateSession, false),
			(Scenario::LateQuota, false),
		] {
			let fixture = Fixture::new(scenario, false).await;
			fixture.install();
			let read = tokio::spawn(json_uncached_on_lane("/comments/abc.json".into(), false, RedditLane::Tor));
			fixture.data_seen.notified().await;
			fixture.renew_old_budget(rest);
			fixture.refreshed().await;
			fixture.data_release.notify_one();
			let (result, _) = tokio::time::timeout(Duration::from_secs(3), read).await.unwrap().unwrap();
			result.unwrap();
			let wires = fixture.wires.lock().unwrap();
			assert_eq!(wires.iter().filter(|(b, _)| b == "Bearer renewed-fixture").count(), 2);
			assert_eq!(fixture.auth_calls.load(Ordering::SeqCst), 1);
			assert_eq!(upstream_guard(RedditLane::Tor).rejected_oauth_generation, None);
		}
	});
}

#[test]
#[sealed_test(env=[("REDLIB_TOR_FALLBACK","on"),("REDLIB_REDDIT_PRIMARY_LANE","tor")])]
fn test_tor_graphql_quota_failed_auth_retains_pause_and_does_not_reclaim_epoch() {
	tokio::runtime::Runtime::new().unwrap().block_on(async {
		let fixture = Fixture::new(Scenario::AuthFailure, false).await;
		fixture.install();
		fixture.renew_old_budget(false);
		tokio::time::timeout(Duration::from_secs(3), async {
			loop {
				if !TOR_OAUTH_IS_ROLLING_OVER.load(Ordering::SeqCst) {
					break;
				}
				tokio::task::yield_now().await;
			}
		})
		.await
		.unwrap();
		let guard = upstream_guard(RedditLane::Tor);
		assert_eq!(guard.graphql_quota.generation, 0);
		assert!(guard.graphql_rate_limit_blocked_until.is_some());
		assert_eq!(guard.completed_quota_rotation, None);
		assert!(guard
			.quota_rotation_candidate_for_budget(Instant::now(), 0, QuotaRotationMode::Emergency, QuotaBudget::Graphql)
			.is_none());
		assert!(crate::oauth::refresh_backoff_remaining(RedditLane::Tor).is_some());
		assert_eq!(fixture.auth_calls.load(Ordering::SeqCst), 1);
	});
}

#[tokio::test]
async fn test_tor_graphql_quota_install_wait_preserves_deadline_on_unrelated_notifications() {
	let changed = Notify::new();
	let deadline = tokio::time::Instant::now() + Duration::from_millis(25);
	let mut waiting = Box::pin(wait_for_confirmed_quota_install(&changed, deadline, || false));
	assert!(futures_lite::future::poll_once(waiting.as_mut()).await.is_none());
	for _ in 0..20 {
		changed.notify_waiters();
		assert!(futures_lite::future::poll_once(waiting.as_mut()).await.is_none());
	}
	tokio::time::sleep_until(deadline + Duration::from_millis(1)).await;
	changed.notify_waiters();
	assert_eq!(futures_lite::future::poll_once(waiting.as_mut()).await, Some(false));
	let completed = std::sync::atomic::AtomicBool::new(false);
	let mut waiting = Box::pin(wait_for_confirmed_quota_install(&changed, tokio::time::Instant::now() + Duration::from_secs(30), || {
		completed.load(Ordering::SeqCst)
	}));
	assert!(futures_lite::future::poll_once(waiting.as_mut()).await.is_none());
	completed.store(true, Ordering::SeqCst);
	changed.notify_waiters();
	assert_eq!(futures_lite::future::poll_once(waiting.as_mut()).await, Some(true));
	let mut state = TorQuotaRefreshRetryState::default();
	assert!(state.claim_generation_replay(RedditLane::Tor, Instant::now()));
	assert!(!state.claim_generation_replay(RedditLane::Tor, Instant::now()));
}

#[test]
#[sealed_test(env=[("REDLIB_TOR_FALLBACK","on"),("REDLIB_REDDIT_PRIMARY_LANE","tor")])]
fn test_tor_graphql_quota_flag_before_ticket_publication_wakes_on_confirmed_install() {
	tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap().block_on(async {
		let fixture = Fixture::new(Scenario::Publication429, false).await;
		fixture.install();
		let publish = crate::oauth::stage_quota_refresh_for_test(RedditLane::Tor).unwrap();
		let epoch = upstream_guard(RedditLane::Tor).graphql_quota.epoch;
		assert!(!quota_rotation_in_progress(RedditLane::Tor, QuotaBudget::Graphql, 0, epoch));
		let read = tokio::spawn(json_uncached_on_lane("/comments/abc.json".into(), false, RedditLane::Tor));
		tokio::time::timeout(Duration::from_secs(2), async {
			loop {
				if upstream_guard(RedditLane::Tor).graphql_rate_limit_blocked_until.is_some() {
					break;
				}
				tokio::task::yield_now().await;
			}
		})
		.await
		.unwrap();
		assert_eq!(fixture.auth_calls.load(Ordering::SeqCst), 0);
		assert!(!read.is_finished());
		assert!(publish(QuotaRotationTicket {
			lane: RedditLane::Tor,
			budget: QuotaBudget::Graphql,
			generation: 0,
			quota_epoch: epoch,
			mode: QuotaRotationMode::Emergency
		}));
		let (result, _) = tokio::time::timeout(Duration::from_secs(3), read)
			.await
			.expect("confirmed installation must wake the old 25-second quota sleep")
			.unwrap();
		result.unwrap();
		assert_eq!(fixture.auth_calls.load(Ordering::SeqCst), 1);
		assert_eq!(fixture.wires.lock().unwrap().len(), 3);
	});
}

#[test]
#[sealed_test(env=[("REDLIB_TOR_FALLBACK","on"),("REDLIB_REDDIT_PRIMARY_LANE","tor")])]
fn test_tor_graphql_quota_exhausted_replay_returns_retryable_recovery_message() {
	tokio::runtime::Runtime::new().unwrap().block_on(async {
		let fixture = Fixture::new(Scenario::LateContinuation, false).await;
		fixture.install();
		let capacity = api_concurrency(RedditLane::Tor).available_permits();
		let read = tokio::spawn(async {
			let mut refresh = TorQuotaRefreshRetryState {
				redirect_replayed: true,
				..TorQuotaRefreshRetryState::default()
			};
			let (result, _, recovery) = json_uncached_on_lane_with_options(
				"/comments/abc.json".into(),
				false,
				RedditLane::Tor,
				AdmissionRetryMode::Bounded,
				Some(Duration::from_secs(3)),
				None,
				&mut refresh,
			)
			.await;
			assert_eq!(recovery, LaneRecoveryReason::TorQuotaGenerationChanged);
			assert!(
				!refresh.claim_generation_replay(RedditLane::Tor, Instant::now()),
				"a used full-read replay must remain exhausted"
			);
			result
		});
		fixture.data_seen.notified().await;
		fixture.renew_old_budget(false);
		fixture.refreshed().await;
		fixture.data_release.notify_one();
		let result = tokio::time::timeout(Duration::from_secs(3), read).await.unwrap().unwrap();
		assert_eq!(result, Err("Refreshing the anonymous Reddit session. Retry in 2 seconds".into()));
		assert_eq!(
			fixture.wires.lock().unwrap().len(),
			1,
			"exhausted recovery must neither continue the old page nor replay it again"
		);
		assert_eq!(fixture.auth_calls.load(Ordering::SeqCst), 1);
		assert_eq!(api_concurrency(RedditLane::Tor).available_permits(), capacity);
	});
}

#[test]
#[sealed_test(env=[("REDLIB_TOR_FALLBACK","on"),("REDLIB_REDDIT_PRIMARY_LANE","tor")])]
fn test_tor_graphql_quota_invalid_cursor_stays_terminal_without_wire_sends() {
	tokio::runtime::Runtime::new().unwrap().block_on(async {
		let fixture = Fixture::new(Scenario::CompleteLow, false).await;
		fixture.install();
		let path = "/r/one+two/hot.json?after=invalid";
		let (result, _) = json_uncached_on_lane(path.into(), false, RedditLane::Tor).await;
		let message = result.unwrap_err();
		assert_eq!(message, "This page link is invalid. Return to the first page and try again.");
		let request = HyperRequest::builder().uri(path).body(Body::empty()).unwrap();
		let response = crate::utils::error(request, &message).await.unwrap();
		assert_eq!(response.status(), hyper::StatusCode::NOT_FOUND);
		assert!(!response.headers().contains_key("retry-after"));
		let body = hyper::body::to_bytes(response.into_body()).await.unwrap();
		let html = std::str::from_utf8(&body).unwrap();
		assert!(html.contains("Return to the first page"));
		assert!(!html.contains("http-equiv=\"refresh\""));
		assert!(fixture.wires.lock().unwrap().is_empty());
		assert_eq!(fixture.auth_calls.load(Ordering::SeqCst), 0);
	});
}

struct OperationLogCapture;
static OPERATION_LOG_ROWS: LazyLock<Mutex<Vec<(log::Level, String)>>> = LazyLock::new(|| Mutex::new(Vec::new()));
static OPERATION_LOG_CAPTURE: OperationLogCapture = OperationLogCapture;
impl log::Log for OperationLogCapture {
	fn enabled(&self, metadata: &log::Metadata<'_>) -> bool {
		metadata.target() == "redlib::client"
	}
	fn log(&self, record: &log::Record<'_>) {
		if !self.enabled(record.metadata()) {
			return;
		}
		let message = record.args().to_string();
		if [
			"Reddit quota response:",
			"Reddit rate-limit observation:",
			"Reddit comments response rejected:",
			"Reddit GraphQL page response rejected:",
			"Reddit request budget is low",
		]
		.iter()
		.any(|prefix| message.starts_with(prefix))
		{
			OPERATION_LOG_ROWS.lock().unwrap().push((record.level(), message));
		}
	}
	fn flush(&self) {}
}

#[test]
#[sealed_test(env=[("REDLIB_TOR_FALLBACK","on"),("REDLIB_REDDIT_PRIMARY_LANE","tor")])]
fn test_quota_diagnostics_operation_labels_follow_each_failed_wire_and_full_replay() {
	tokio::runtime::Runtime::new().unwrap().block_on(async {
		log::set_logger(&OPERATION_LOG_CAPTURE).unwrap();
		log::set_max_level(log::LevelFilter::Trace);
		for (path, first, final_operation) in [("/comments/abc.json", "PostsByIds", "PostComments"), ("/r/all/hot.json", "AllFeedSdui", "PostsByIds")] {
			for (scenario, expected, prefix, valid) in [
				(Scenario::First429, first, "Reddit quota response:", true),
				(Scenario::Final429, final_operation, "Reddit quota response:", true),
				(
					Scenario::FirstQuotaBody,
					first,
					if first == "PostsByIds" {
						"Reddit comments response rejected:"
					} else {
						"Reddit GraphQL page response rejected:"
					},
					true,
				),
				(
					Scenario::FinalQuotaBody,
					final_operation,
					if first == "PostsByIds" {
						"Reddit comments response rejected:"
					} else {
						"Reddit GraphQL page response rejected:"
					},
					true,
				),
				(
					Scenario::FirstInvalid,
					first,
					if first == "PostsByIds" {
						"Reddit comments response rejected:"
					} else {
						"Reddit GraphQL page response rejected:"
					},
					false,
				),
				(
					Scenario::FinalInvalid,
					final_operation,
					if first == "PostsByIds" {
						"Reddit comments response rejected:"
					} else {
						"Reddit GraphQL page response rejected:"
					},
					false,
				),
				(Scenario::CompleteLow, final_operation, "Reddit request budget is low:", true),
			] {
				OPERATION_LOG_ROWS.lock().unwrap().clear();
				let fixture = Fixture::new(scenario, false).await;
				fixture.install();
				let (result, _) = tokio::time::timeout(Duration::from_secs(4), json_uncached_on_lane(path.into(), false, RedditLane::Tor))
					.await
					.unwrap();
				assert_eq!(result.is_ok(), valid);
				if fixture.auth_calls.load(Ordering::SeqCst) > 0 {
					fixture.refreshed().await;
				}
				let rows = OPERATION_LOG_ROWS.lock().unwrap();
				assert!(
					rows
						.iter()
						.any(|(level, row)| *level == log::Level::Warn && row.starts_with(prefix) && row.contains(&format!("operation={expected} "))),
					"the error/low-budget diagnostic must label its actual wire: {rows:?}"
				);
				if valid {
					assert!(rows
						.iter()
						.any(|(level, row)| *level == log::Level::Trace && row.starts_with("Reddit rate-limit observation:") && row.contains(&format!("operation={final_operation} "))));
				}
			}
			OPERATION_LOG_ROWS.lock().unwrap().clear();
			let fixture = Fixture::new(Scenario::FinalThenFirst429, false).await;
			fixture.install();
			let (result, _) = tokio::time::timeout(Duration::from_secs(4), json_uncached_on_lane(path.into(), false, RedditLane::Tor))
				.await
				.unwrap();
			assert!(result.is_err());
			let rows = OPERATION_LOG_ROWS.lock().unwrap();
			let quota_rows: Vec<_> = rows.iter().filter(|(_, row)| row.starts_with("Reddit quota response:")).collect();
			assert_eq!(quota_rows.len(), 2);
			assert!(quota_rows[0].1.contains(&format!("operation={final_operation} ")));
			assert!(
				quota_rows[1].1.contains(&format!("operation={first} ")),
				"replay must reset the diagnostic to the new first wire"
			);
			assert_eq!(fixture.auth_calls.load(Ordering::SeqCst), 1, "a fresh exhausted identity must not renew repeatedly");
		}
	});
}

#[test]
fn test_quota_diagnostics_operation_labels_reject_arbitrary_payload_text() {
	assert_eq!(wire_operation_label(None), "rest");
	for operation in ["PostsByIds", "PostComments", "AllFeedSdui"] {
		assert_eq!(wire_operation_label(Some(&serde_json::json!({"operationName": operation}))), operation);
	}
	for payload in [
		serde_json::json!({"operationName": "PostComments token=untrusted"}),
		serde_json::json!({"operationName": "https://untrusted.example/?secret=value"}),
		serde_json::json!({"operationName": 42}),
		Value::Null,
	] {
		assert_eq!(wire_operation_label(Some(&payload)), "unknown");
	}
}

#[test]
#[sealed_test(env=[("REDLIB_TOR_FALLBACK","on"),("REDLIB_REDDIT_PRIMARY_LANE","tor")])]
fn test_quota_diagnostics_operation_malformed_feed_labels_the_preceding_wire() {
	tokio::runtime::Runtime::new().unwrap().block_on(async {
		log::set_logger(&OPERATION_LOG_CAPTURE).unwrap();
		log::set_max_level(log::LevelFilter::Trace);
		let fixture = Fixture::new(Scenario::MalformedFirstFeed, false).await;
		fixture.install();
		let (result, _) = json_uncached_on_lane("/r/all/hot.json".into(), false, RedditLane::Tor).await;
		assert_eq!(result, Err("Reddit is temporarily unable to load this page. Retry in 30 seconds".into()));
		let rows = OPERATION_LOG_ROWS.lock().unwrap();
		assert!(
			rows.iter().any(|(level, row)| *level == log::Level::Warn
				&& row.starts_with("Reddit GraphQL page response rejected:")
				&& row.contains("operation=AllFeedSdui reason=missing_feed_edges")),
			"the missing shape must identify the preceding actual response: {rows:?}"
		);
		let wires = fixture.wires.lock().unwrap();
		assert_eq!(wires.len(), 1, "a malformed feed must fail before hydration sends another operation");
		assert_eq!(wires[0].1, "AllFeedSdui");
		assert_eq!(fixture.auth_calls.load(Ordering::SeqCst), 0);
	});
}
