use super::*;
use sealed_test::prelude::*;
use std::sync::atomic::AtomicUsize;

struct Fixture {
	origin: RedditOrigin,
	server: tokio::task::JoinHandle<Result<(), hyper::Error>>,
	calls: Arc<AtomicUsize>,
	auth: Arc<AtomicUsize>,
	reject: Arc<AtomicBool>,
	seen: Arc<Notify>,
	release: Arc<Notify>,
}
impl Drop for Fixture {
	fn drop(&mut self) {
		self.server.abort();
	}
}
impl Fixture {
	async fn new(hold_first: bool) -> Self {
		use hyper::service::{make_service_fn, service_fn};
		use std::convert::Infallible;
		let calls = Arc::new(AtomicUsize::new(0));
		let auth = Arc::new(AtomicUsize::new(0));
		let reject = Arc::new(AtomicBool::new(false));
		let seen = Arc::new(Notify::new());
		let release = Arc::new(Notify::new());
		let shared = (calls.clone(), auth.clone(), reject.clone(), seen.clone(), release.clone());
		let service = make_service_fn(move |_| {
			let shared = shared.clone();
			async move {
				Ok::<_, Infallible>(service_fn(move |request: HyperRequest<Body>| {
					let (calls, auth, reject, seen, release) = shared.clone();
					async move {
						if request.uri().path().starts_with("/auth/") {
							auth.fetch_add(1, Ordering::SeqCst);
							return Ok::<_, Infallible>(HyperResponse::new(Body::from(r#"{"access_token":"unexpected-refresh","expires_in":3600}"#)));
						}
						let number = calls.fetch_add(1, Ordering::SeqCst);
						if reject.load(Ordering::SeqCst) {
							return Ok(
								HyperResponse::builder()
									.status(404)
									.header("content-type", "application/json")
									.body(Body::from(r#"{"error":404}"#))
									.unwrap(),
							);
						}
						let payload: Value = serde_json::from_slice(&hyper::body::to_bytes(request.into_body()).await.unwrap()).unwrap();
						if hold_first && number == 0 {
							seen.notify_one();
							release.notified().await;
						}
						let value = match payload["operationName"].as_str().unwrap() {
							"PostsByIds" => {
								serde_json::json!({"data":{"postsInfoByIds":[{"__typename":"SubredditPost","id":"t3_abc","postTitle":"Fixture","subreddit":{"name":"fixture","type":"PUBLIC"}}]}})
							}
							"PostComments" => serde_json::json!({"data":{"postInfoById":{"id":"t3_abc","commentForest":{"trees":[]}}}}),
							operation => panic!("unexpected operation {operation}"),
						};
						Ok(
							HyperResponse::builder()
								.header("content-type", "application/json")
								.header("x-ratelimit-remaining", "100")
								.header("x-ratelimit-reset", "60")
								.body(Body::from(value.to_string()))
								.unwrap(),
						)
					}
				}))
			}
		});
		let server = hyper::Server::bind(&([127, 0, 0, 1], 0).into()).serve(service);
		let address = server.local_addr();
		Self {
			origin: RedditOrigin {
				base: Box::leak(format!("http://{address}").into_boxed_str()),
				host: Box::leak(address.to_string().into_boxed_str()),
			},
			server: tokio::spawn(server),
			calls,
			auth,
			reject,
			seen,
			release,
		}
	}
	fn install(&self, lane: RedditLane) {
		*upstream_guard(lane) = UpstreamGuard::new(lane);
		let oauth = Oauth::request_fixture_with_refresh(lane, Arc::new(WreqClient::builder().no_proxy().build().unwrap()), self.origin);
		assert!(install_oauth_client(oauth, true, None));
	}
}

#[test]
#[sealed_test(env=[("REDLIB_TOR_FALLBACK","on"),("REDLIB_TOR_PROXY","socks5h://127.0.0.1:1"),("REDLIB_REDDIT_PRIMARY_LANE","tor")])]
fn test_quota_discovery_standby_held_owner_follower_uses_direct_before_local_retry_budget() {
	tokio::runtime::Runtime::new().unwrap().block_on(async {
		assert_eq!(configured_primary_lane(), RedditLane::Tor);
		for cancel in [false, true] {
			let tor = Fixture::new(true).await;
			let direct = Fixture::new(false).await;
			tor.install(RedditLane::Tor);
			direct.install(RedditLane::Direct);
			let path = "/comments/abc.json";
			let owner = tokio::spawn(json_uncached_on_lane(path.into(), false, RedditLane::Tor));
			tokio::time::timeout(Duration::from_secs(2), tor.seen.notified()).await.unwrap();
			let owner_id = upstream_guard(RedditLane::Tor).graphql_quota.discovery_owner.unwrap();
			let follower = tokio::time::timeout(Duration::from_millis(1500), json_uncached_routed(path.into(), false)).await;
			assert!(follower.is_ok(), "ready Direct must finish while the Tor discovery owner is still held");
			assert!(follower.unwrap().is_ok());
			assert_eq!(tor.calls.load(Ordering::SeqCst), 1);
			assert_eq!(direct.calls.load(Ordering::SeqCst), 2);
			{
				let guard = upstream_guard(RedditLane::Tor);
				assert_eq!(guard.graphql_quota.discovery_owner, Some(owner_id));
				assert_eq!(guard.graphql_quota.outstanding, 1);
			}
			if cancel {
				owner.abort();
				assert!(owner.await.unwrap_err().is_cancelled());
				assert!(matches!(
					upstream_guard(RedditLane::Tor).graphql_quota.window,
					QuotaWindow::Unknown { probe_in_flight: false }
				));
			} else {
				tor.release.notify_one();
				assert!(owner.await.unwrap().0.is_ok());
				assert_eq!(tor.calls.load(Ordering::SeqCst), 2);
			}
			assert_eq!(upstream_guard(RedditLane::Tor).graphql_quota.discovery_owner, None);
			assert_eq!(upstream_guard(RedditLane::Tor).graphql_quota.outstanding, 0);
			assert_eq!(api_concurrency(RedditLane::Tor).available_permits(), max_concurrent_api_requests());
			assert_eq!(DIRECT_STANDBY_CAPACITY.available_permits(), 2);
			assert_eq!(tor.auth.load(Ordering::SeqCst), 0);
			assert_eq!(direct.auth.load(Ordering::SeqCst), 0);
		}
	});
}

fn set_owned_discovery(lane: RedditLane, budget: QuotaBudget) {
	let mut guard = upstream_guard(lane);
	let quota = guard.quota_budget_mut(budget);
	quota.window = QuotaWindow::Unknown { probe_in_flight: true };
	quota.discovery_owner = Some(1);
}

#[test]
#[sealed_test(env=[("REDLIB_TOR_FALLBACK","on"),("REDLIB_TOR_PROXY","socks5h://127.0.0.1:1"),("REDLIB_REDDIT_PRIMARY_LANE","tor")])]
fn test_quota_discovery_standby_scope_guards_cache_and_existing_ready_contracts() {
	tokio::runtime::Runtime::new().unwrap().block_on(async {
		let tor = Fixture::new(false).await;
		let direct = Fixture::new(false).await;
		let gql = "/comments/abc.json";
		let rest = "/api/site_admin.json";
		for (path, other_path, budget) in [(gql, rest, QuotaBudget::Graphql), (rest, gql, QuotaBudget::Rest)] {
			tor.install(RedditLane::Tor);
			direct.install(RedditLane::Direct);
			set_owned_discovery(RedditLane::Tor, budget);
			let state = lane_routing_state(RedditLane::Tor, Instant::now(), path);
			assert!(state.discovery_pending);
			assert_eq!(state.availability, LaneAvailability::Ready);
			assert_eq!(lane_availability(RedditLane::Tor, Instant::now(), path), LaneAvailability::Ready);
			assert!(tor_fallback_ready(path), "global Ready callers retain their existing meaning");
			assert_eq!(preferred_api_lane(Instant::now(), path), RedditLane::Direct);
			assert!(!lane_routing_state(RedditLane::Tor, Instant::now(), other_path).discovery_pending);
			assert_eq!(preferred_api_lane(Instant::now(), other_path), RedditLane::Tor);
			assert!(!json_pause_cache_eligible(Instant::now(), path), "ready Direct preserves the normal fresh-read/cache path");
			{
				let mut guard = upstream_guard(RedditLane::Direct);
				guard.rejected_oauth_generation = Some(0);
			}
			assert_eq!(preferred_api_lane(Instant::now(), path), RedditLane::Tor);
			assert!(
				json_pause_cache_eligible(Instant::now(), path),
				"an unavailable alternate must preserve warm-cache recovery"
			);
			direct.install(RedditLane::Direct);
			set_owned_discovery(RedditLane::Direct, budget);
			assert_eq!(
				preferred_api_lane(Instant::now(), path),
				RedditLane::Tor,
				"pending Direct is not a ready standby for Tor's discovery follower"
			);
			assert_eq!(
				initial_lane_request_timeout(RedditLane::Tor, RedditLane::Tor, lane_availability(RedditLane::Direct, Instant::now(), path)),
				Duration::from_secs(8)
			);
			// Both pending lanes keep the old cache/global Ready contract as well.
			assert!(!json_pause_cache_eligible(Instant::now(), path));
		}
		for missing_owner in [false, true] {
			tor.install(RedditLane::Tor);
			direct.install(RedditLane::Direct);
			set_owned_discovery(RedditLane::Tor, QuotaBudget::Graphql);
			{
				let mut guard = upstream_guard(RedditLane::Tor);
				if missing_owner {
					guard.graphql_quota.discovery_owner = None;
				} else {
					guard.graphql_quota.generation = 99;
				}
			}
			assert!(!lane_routing_state(RedditLane::Tor, Instant::now(), gql).discovery_pending);
			assert_eq!(preferred_api_lane(Instant::now(), gql), RedditLane::Tor);
		}
		for source in 0..4 {
			tor.install(RedditLane::Tor);
			direct.install(RedditLane::Direct);
			set_owned_discovery(RedditLane::Tor, QuotaBudget::Graphql);
			let now = Instant::now();
			{
				let mut guard = upstream_guard(RedditLane::Tor);
				match source {
					0 => guard.graphql_rate_limit_blocked_until = Some(now + Duration::from_secs(60)),
					1 => guard.rejected_oauth_generation = Some(0),
					2 => guard.transport_health_mut(api_endpoint_index(gql)).blocked_until = Some(now + Duration::from_secs(60)),
					_ => {
						guard.graphql_quota.window = QuotaWindow::Known {
							available: 0,
							reset_at: now + Duration::from_secs(60),
						}
					}
				}
			}
			let state = lane_routing_state(RedditLane::Tor, now, gql);
			assert!(!state.discovery_pending, "real quota/session/transport state must outrank discovery");
			let expected = select_api_lane(RedditLane::Tor, lane_availability(RedditLane::Direct, now, gql), state.availability);
			assert_eq!(preferred_api_lane(now, gql), expected);
			if source == 0 {
				assert_eq!(state.availability, LaneAvailability::UpstreamQuotaCooldown);
				assert_eq!(expected, RedditLane::Tor);
			}
		}
		assert_eq!(tor.calls.load(Ordering::SeqCst), 0);
		assert_eq!(direct.calls.load(Ordering::SeqCst), 0);
		assert_eq!(tor.auth.load(Ordering::SeqCst), 0);
		assert_eq!(direct.auth.load(Ordering::SeqCst), 0);
	});
}

#[test]
#[sealed_test(env=[("REDLIB_TOR_FALLBACK","on"),("REDLIB_TOR_PROXY","socks5h://127.0.0.1:1"),("REDLIB_REDDIT_PRIMARY_LANE","direct")])]
fn test_quota_discovery_standby_direct_primary_routing_is_unchanged() {
	tokio::runtime::Runtime::new().unwrap().block_on(async {
		assert_eq!(configured_primary_lane(), RedditLane::Direct);
		let tor = Fixture::new(false).await;
		let direct = Fixture::new(false).await;
		let path = "/comments/abc.json";
		for direct_state in 0..5 {
			tor.install(RedditLane::Tor);
			direct.install(RedditLane::Direct);
			set_owned_discovery(RedditLane::Tor, QuotaBudget::Graphql);
			let now = Instant::now();
			{
				let mut guard = upstream_guard(RedditLane::Direct);
				match direct_state {
					0 => {}
					1 => {
						guard.graphql_quota.window = QuotaWindow::Unknown { probe_in_flight: true };
						guard.graphql_quota.discovery_owner = Some(1);
					}
					2 => guard.rejected_oauth_generation = Some(0),
					3 => guard.transport_health_mut(api_endpoint_index(path)).blocked_until = Some(now + Duration::from_secs(60)),
					_ => guard.graphql_rate_limit_blocked_until = Some(now + Duration::from_secs(60)),
				}
			}
			let expected = select_api_lane(
				RedditLane::Direct,
				lane_availability(RedditLane::Direct, now, path),
				lane_availability(RedditLane::Tor, now, path),
			);
			assert_eq!(preferred_api_lane(now, path), expected);
			if direct_state <= 1 || direct_state == 4 {
				assert_eq!(expected, RedditLane::Direct);
			} else {
				assert_eq!(expected, RedditLane::Tor, "unavailable Direct still selects the prior Tor waiting path");
			}
		}
	});
}

#[test]
#[sealed_test(env=[("REDLIB_TOR_FALLBACK","on"),("REDLIB_TOR_PROXY","socks5h://127.0.0.1:1"),("REDLIB_REDDIT_PRIMARY_LANE","tor")])]
fn test_quota_discovery_standby_pending_or_unavailable_direct_keeps_tor_owner_and_waiter() {
	tokio::runtime::Runtime::new().unwrap().block_on(async {
		for pending in [false, true] {
			let tor = Fixture::new(true).await;
			let direct = Fixture::new(false).await;
			tor.install(RedditLane::Tor);
			direct.install(RedditLane::Direct);
			if pending {
				set_owned_discovery(RedditLane::Direct, QuotaBudget::Graphql);
			} else {
				upstream_guard(RedditLane::Direct).rejected_oauth_generation = Some(0);
			}
			let path = "/comments/abc.json";
			let owner = tokio::spawn(json_uncached_on_lane(path.into(), false, RedditLane::Tor));
			tokio::time::timeout(Duration::from_secs(2), tor.seen.notified()).await.unwrap();
			assert_eq!(preferred_api_lane(Instant::now(), path), RedditLane::Tor);
			let mut follower = tokio::spawn(json_uncached_routed(path.into(), false));
			assert!(tokio::time::timeout(Duration::from_millis(50), &mut follower).await.is_err());
			assert_eq!(tor.calls.load(Ordering::SeqCst), 1);
			assert_eq!(direct.calls.load(Ordering::SeqCst), 0);
			tor.release.notify_one();
			assert!(owner.await.unwrap().0.is_ok());
			assert!(tokio::time::timeout(Duration::from_secs(2), follower).await.unwrap().unwrap().is_ok());
			assert_eq!(tor.calls.load(Ordering::SeqCst), 4);
			assert_eq!(direct.calls.load(Ordering::SeqCst), 0);
			assert_eq!(tor.auth.load(Ordering::SeqCst), 0);
			assert_eq!(direct.auth.load(Ordering::SeqCst), 0);
		}
	});
}

#[test]
#[sealed_test(env=[("REDLIB_TOR_FALLBACK","on"),("REDLIB_TOR_PROXY","socks5h://127.0.0.1:1"),("REDLIB_REDDIT_PRIMARY_LANE","tor")])]
fn test_quota_discovery_standby_busy_slots_recover_on_tor_without_recursive_standby() {
	tokio::runtime::Runtime::new().unwrap().block_on(async {
		let tor = Fixture::new(true).await;
		let direct = Fixture::new(false).await;
		tor.install(RedditLane::Tor);
		direct.install(RedditLane::Direct);
		let path = "/comments/abc.json";
		let owner = tokio::spawn(json_uncached_on_lane(path.into(), false, RedditLane::Tor));
		tokio::time::timeout(Duration::from_secs(2), tor.seen.notified()).await.unwrap();
		let slots = DIRECT_STANDBY_CAPACITY.acquire_many(2).await.unwrap();
		let mut follower = tokio::spawn(json_uncached_routed(path.into(), false));
		assert!(tokio::time::timeout(Duration::from_millis(50), &mut follower).await.is_err());
		assert_eq!(direct.calls.load(Ordering::SeqCst), 0);
		tor.release.notify_one();
		assert!(owner.await.unwrap().0.is_ok());
		assert!(tokio::time::timeout(Duration::from_secs(2), follower).await.unwrap().unwrap().is_ok());
		assert_eq!(tor.calls.load(Ordering::SeqCst), 4);
		assert_eq!(direct.calls.load(Ordering::SeqCst), 0);
		drop(slots);
		assert_eq!(DIRECT_STANDBY_CAPACITY.available_permits(), 2);
		assert_eq!(tor.auth.load(Ordering::SeqCst), 0);
		assert_eq!(direct.auth.load(Ordering::SeqCst), 0);
	});
}

#[test]
#[sealed_test(env=[("REDLIB_TOR_FALLBACK","on"),("REDLIB_TOR_PROXY","socks5h://127.0.0.1:1"),("REDLIB_REDDIT_PRIMARY_LANE","tor")])]
fn test_quota_discovery_standby_late_ready_direct_recovers_already_selected_tor_follower() {
	use std::future::Future;
	tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap().block_on(async {
		let tor = Fixture::new(true).await;
		let direct = Fixture::new(false).await;
		tor.install(RedditLane::Tor);
		direct.install(RedditLane::Direct);
		let path = "/comments/abc.json";
		let endpoint = api_endpoint_index(path);
		upstream_guard(RedditLane::Direct).transport_health_mut(endpoint).blocked_until = Some(Instant::now() + Duration::from_secs(60));
		let owner = tokio::spawn(json_uncached_on_lane(path.into(), false, RedditLane::Tor));
		tokio::time::timeout(Duration::from_secs(2), tor.seen.notified()).await.unwrap();
		let owner_id = upstream_guard(RedditLane::Tor).graphql_quota.discovery_owner.unwrap();
		assert_eq!(preferred_api_lane(Instant::now(), path), RedditLane::Tor);
		let mut follower = Box::pin(json_uncached_routed(path.into(), false));
		// Poll into Tor's admission wait before making Direct ready. This proves
		// recovery after routing, rather than the initial ready-standby shortcut.
		assert!(std::future::poll_fn(|cx| std::task::Poll::Ready(follower.as_mut().poll(cx))).await.is_pending());
		assert_eq!(direct.calls.load(Ordering::SeqCst), 0);
		upstream_guard(RedditLane::Direct).transport_health_mut(endpoint).blocked_until = None;
		let result = tokio::time::timeout(Duration::from_millis(3250), &mut follower).await;
		assert!(result.is_ok(), "an already-selected Tor discovery follower must recheck the newly ready Direct lane");
		assert!(result.unwrap().is_ok());
		assert_eq!(tor.calls.load(Ordering::SeqCst), 1);
		assert_eq!(direct.calls.load(Ordering::SeqCst), 2);
		assert_eq!(upstream_guard(RedditLane::Tor).graphql_quota.discovery_owner, Some(owner_id));
		assert_eq!(upstream_guard(RedditLane::Tor).graphql_quota.outstanding, 1);
		tor.release.notify_one();
		assert!(owner.await.unwrap().0.is_ok());
		assert_eq!(upstream_guard(RedditLane::Tor).graphql_quota.discovery_owner, None);
		assert_eq!(api_concurrency(RedditLane::Tor).available_permits(), max_concurrent_api_requests());
		assert_eq!(DIRECT_STANDBY_CAPACITY.available_permits(), 2);
		assert_eq!(tor.auth.load(Ordering::SeqCst), 0);
		assert_eq!(direct.auth.load(Ordering::SeqCst), 0);
	});
}

#[test]
#[sealed_test(env=[("REDLIB_TOR_FALLBACK","on"),("REDLIB_TOR_PROXY","socks5h://127.0.0.1:1"),("REDLIB_REDDIT_PRIMARY_LANE","tor")])]
fn test_quota_discovery_standby_recheck_requires_exact_denial_probe_and_current_guards() {
	tokio::runtime::Runtime::new().unwrap().block_on(async {
		let tor = Fixture::new(false).await;
		let direct = Fixture::new(false).await;
		let path = "/comments/abc.json";
		let endpoint = api_endpoint_index(path);
		for change in 0..14 {
			tor.install(RedditLane::Tor);
			direct.install(RedditLane::Direct);
			let owner = upstream_guard(RedditLane::Tor).try_admit_endpoint(Instant::now(), 0, endpoint).unwrap();
			let denied = begin_upstream_attempt_once(RedditLane::Tor, false, endpoint)
				.err()
				.expect("held discovery must deny a follower");
			assert_eq!(denied.admission.unwrap().source, "quota_discovery");
			let ticket = denied.discovery_wait.unwrap();
			assert!(matches!(denied.recovery_reason(RedditLane::Tor), LaneRecoveryReason::QuotaDiscoveryPending { ticket: actual, .. } if actual == ticket));
			assert_eq!(ticket.owner, owner.request_id);
			assert!(discovery_standby_ready(path, ticket));
			assert!(!discovery_standby_ready("/api/site_admin.json", ticket), "another quota budget cannot reuse this denial");
			let mut busy_slots = None;
			match change {
				0 => upstream_guard(RedditLane::Tor).graphql_quota.discovery_owner = None,
				1 => upstream_guard(RedditLane::Tor).graphql_quota.discovery_owner = Some(ticket.owner + 1),
				2 => upstream_guard(RedditLane::Tor).graphql_quota.epoch += 1,
				3 => upstream_guard(RedditLane::Tor).install_oauth_generation(1, true),
				4 => upstream_guard(RedditLane::Tor).graphql_rate_limit_blocked_until = Some(Instant::now() + Duration::from_secs(60)),
				5 => {
					upstream_guard(RedditLane::Tor).graphql_quota.window = QuotaWindow::Known {
						available: 0,
						reset_at: Instant::now() + Duration::from_secs(60),
					}
				}
				6 => upstream_guard(RedditLane::Tor).rejected_oauth_generation = Some(0),
				7 => upstream_guard(RedditLane::Tor).transport_health_mut(endpoint).blocked_until = Some(Instant::now() + Duration::from_secs(60)),
				8 => set_owned_discovery(RedditLane::Direct, QuotaBudget::Graphql),
				9 => upstream_guard(RedditLane::Direct).graphql_rate_limit_blocked_until = Some(Instant::now() + Duration::from_secs(60)),
				10 => upstream_guard(RedditLane::Direct).transport_health_mut(endpoint).blocked_until = Some(Instant::now() + Duration::from_secs(60)),
				11 => upstream_guard(RedditLane::Direct).rejected_oauth_generation = Some(0),
				12 => busy_slots = Some(DIRECT_STANDBY_CAPACITY.acquire_many(2).await.unwrap()),
				_ => upstream_guard(RedditLane::Tor).graphql_quota.window = QuotaWindow::Unreported,
			}
			assert!(
				!discovery_standby_ready(path, ticket),
				"changed owner, identity, epoch, quota, session or capacity must keep the original Tor engine"
			);
			drop(busy_slots);
			drop(owner);
		}
		assert_eq!(tor.calls.load(Ordering::SeqCst), 0);
		assert_eq!(direct.calls.load(Ordering::SeqCst), 0);
		assert_eq!(tor.auth.load(Ordering::SeqCst), 0);
		assert_eq!(direct.auth.load(Ordering::SeqCst), 0);
	});
}

#[test]
#[sealed_test(env=[("REDLIB_TOR_FALLBACK","on"),("REDLIB_TOR_PROXY","socks5h://127.0.0.1:1"),("REDLIB_REDDIT_PRIMARY_LANE","tor")])]
fn test_quota_discovery_standby_recheck_defaults_and_used_standby_continue_tor() {
	tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap().block_on(async {
		// Each follower remains blocked beyond the local two-second retry budget.
		// A disabled/noneligible handoff must continue recovering on Tor, not
		// expose the intermediate discovery error or try standby recursively.
		for scenario in 0..5 {
			let tor = Fixture::new(true).await;
			let direct = Fixture::new(false).await;
			tor.install(RedditLane::Tor);
			direct.install(RedditLane::Direct);
			let path = "/comments/abc.json";
			let endpoint = api_endpoint_index(path);
			let owner = tokio::spawn(json_uncached_on_lane(path.into(), false, RedditLane::Tor));
			tokio::time::timeout(Duration::from_secs(2), tor.seen.notified()).await.unwrap();
			let mut busy_slots = None;
			match scenario {
				1 => direct.reject.store(true, Ordering::SeqCst),
				2 => set_owned_discovery(RedditLane::Direct, QuotaBudget::Graphql),
				3 => upstream_guard(RedditLane::Direct).transport_health_mut(endpoint).blocked_until = Some(Instant::now() + Duration::from_secs(60)),
				4 => busy_slots = Some(DIRECT_STANDBY_CAPACITY.acquire_many(2).await.unwrap()),
				_ => {}
			}
			let mut follower = tokio::spawn(async move {
				match scenario {
					0 => json_uncached_on_lane(path.into(), false, RedditLane::Tor).await.0,
					1 => direct_then_tor_request(path.into(), false).await,
					_ => json_uncached_routed(path.into(), false).await,
				}
			});
			assert!(
				tokio::time::timeout(Duration::from_millis(2100), &mut follower).await.is_err(),
				"noneligible/default/final Tor recovery must remain pending instead of returning the local discovery error"
			);
			let expected_direct = usize::from(scenario == 1);
			assert_eq!(direct.calls.load(Ordering::SeqCst), expected_direct);
			if scenario == 1 {
				let direct_state = lane_routing_state(RedditLane::Direct, Instant::now(), path);
				assert_eq!(direct_state.availability, LaneAvailability::Ready);
				assert!(!direct_state.discovery_pending, "only the disabled final-Tor handoff prevents another ready-Direct attempt");
			}
			assert_eq!(tor.calls.load(Ordering::SeqCst), 1);
			tor.release.notify_one();
			assert!(owner.await.unwrap().0.is_ok());
			assert!(tokio::time::timeout(Duration::from_secs(2), follower).await.unwrap().unwrap().is_ok());
			assert_eq!(tor.calls.load(Ordering::SeqCst), 4);
			assert_eq!(
				direct.calls.load(Ordering::SeqCst),
				expected_direct,
				"no new standby call after fallback has already been used"
			);
			drop(busy_slots);
			assert_eq!(api_concurrency(RedditLane::Tor).available_permits(), max_concurrent_api_requests());
			assert_eq!(DIRECT_STANDBY_CAPACITY.available_permits(), 2);
			assert_eq!(tor.auth.load(Ordering::SeqCst), 0);
			assert_eq!(direct.auth.load(Ordering::SeqCst), 0);
		}
	});
}
