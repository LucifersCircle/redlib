use super::*;
use cached::Cached;
use sealed_test::prelude::*;
use std::sync::atomic::AtomicUsize;

#[derive(Clone, Copy)]
enum GqlCase {
	Missing,
	Empty,
	EmptyWithNext,
	ZeroEmpty,
	Edge,
	Quota,
	Zero,
	Resource,
	Session,
}
#[derive(Clone, Copy)]
enum LegacyCase {
	Listing,
	LowRemaining,
	Quarantined,
	PrivatePost,
	Empty,
	Invalid,
	PartialErrors,
	PartialErrorString,
	Html,
	Redirect,
	Unauthorized,
	InvalidToken,
	JsonUnauthorized,
	Private,
	Quota,
	Slow,
}
struct Fixture {
	server: tokio::task::JoinHandle<Result<(), hyper::Error>>,
	origin: RedditOrigin,
	gql: Arc<AtomicUsize>,
	legacy: Arc<AtomicUsize>,
	auth: Arc<AtomicUsize>,
	requests: Arc<Mutex<Vec<(String, String, String)>>>,
}
impl Drop for Fixture {
	fn drop(&mut self) {
		self.server.abort();
	}
}
fn listing() -> Value {
	serde_json::json!({"kind":"Listing","data":{"children":[{"kind":"t3","data":{"id":"abc","name":"t3_abc","title":"Fixture","subreddit":"fixture","permalink":"/r/fixture/comments/abc/fixture/","author":"fixture"}}],"after":"t3_next","before":"t3_previous"}})
}
impl Fixture {
	async fn new(gql_case: GqlCase, legacy_case: LegacyCase) -> Self {
		use hyper::service::{make_service_fn, service_fn};
		use std::convert::Infallible;
		let gql = Arc::new(AtomicUsize::new(0));
		let legacy = Arc::new(AtomicUsize::new(0));
		let auth = Arc::new(AtomicUsize::new(0));
		let requests = Arc::new(Mutex::new(Vec::new()));
		let shared = (gql.clone(), legacy.clone(), auth.clone(), requests.clone());
		let service = make_service_fn(move |_| {
			let shared = shared.clone();
			async move {
				Ok::<_, Infallible>(service_fn(move |request: HyperRequest<Body>| {
					let (gql, legacy, auth, requests) = shared.clone();
					async move {
						requests.lock().unwrap().push((
							request.uri().to_string(),
							request.headers().get("authorization").and_then(|v| v.to_str().ok()).unwrap_or("").into(),
							request.headers().get("cookie").and_then(|v| v.to_str().ok()).unwrap_or("").into(),
						));
						let mut response = HyperResponse::builder().header("content-type", "application/json");
						let value = if request.uri().path().starts_with("/auth/") {
							auth.fetch_add(1, Ordering::SeqCst);
							serde_json::json!({"access_token":"unexpected-refresh","expires_in":3600})
						} else if request.method() == hyper::Method::GET {
							legacy.fetch_add(1, Ordering::SeqCst);
							match legacy_case {
								LegacyCase::Listing => listing(),
								LegacyCase::LowRemaining => {
									response = response.header("x-ratelimit-remaining", "2").header("x-ratelimit-reset", "60");
									listing()
								}
								LegacyCase::Quarantined => {
									let mut value = listing();
									value["data"]["children"][0]["data"]["quarantine"] = Value::Bool(true);
									value
								}
								LegacyCase::PrivatePost => {
									let mut value = listing();
									value["data"]["children"][0]["data"]["subreddit_type"] = Value::String("private".into());
									value
								}
								LegacyCase::Empty => serde_json::json!({"kind":"Listing","data":{"children":[],"after":null,"before":null}}),
								LegacyCase::Invalid => serde_json::json!({"challenge":"not a listing"}),
								LegacyCase::PartialErrors => {
									let mut value = listing();
									value["errors"] = serde_json::json!([{"message":"untrusted challenge"}]);
									value
								}
								LegacyCase::PartialErrorString => {
									let mut value = listing();
									value["error"] = Value::String("untrusted challenge".into());
									value
								}
								LegacyCase::Html => return Ok::<_, Infallible>(HyperResponse::builder().header("content-type", "text/html").body(Body::from("<html>login</html>")).unwrap()),
								LegacyCase::Redirect => return Ok(HyperResponse::builder().status(302).header("location", "/login").body(Body::empty()).unwrap()),
								LegacyCase::Unauthorized => {
									response = response.status(401);
									serde_json::json!({"error":401,"message":"Unauthorized"})
								}
								LegacyCase::InvalidToken => {
									response = response.status(403).header("www-authenticate", "Bearer error=\"invalid_token\"");
									serde_json::json!({"error":403})
								}
								LegacyCase::JsonUnauthorized => serde_json::json!({"error":401,"message":"Unauthorized"}),
								LegacyCase::Private => {
									response = response.status(403);
									serde_json::json!({"error":403,"reason":"private"})
								}
								LegacyCase::Quota => {
									response = response.status(429).header("x-ratelimit-remaining", "0").header("x-ratelimit-reset", "60");
									serde_json::json!({"error":429})
								}
								LegacyCase::Slow => {
									tokio::time::sleep(Duration::from_secs(10)).await;
									listing()
								}
							}
						} else {
							gql.fetch_add(1, Ordering::SeqCst);
							match gql_case {
								GqlCase::Missing => serde_json::json!({"data":{"allV3":{"elements":null}}}),
								GqlCase::Empty => serde_json::json!({"data":{"allV3":{"elements":{"edges":[],"pageInfo":{"hasNextPage":false}}}}}),
								GqlCase::EmptyWithNext => serde_json::json!({"data":{"allV3":{"elements":{"edges":[],"pageInfo":{"hasNextPage":true,"endCursor":"opaque-next"}}}}}),
								GqlCase::ZeroEmpty => {
									response = response.header("x-ratelimit-remaining", "0").header("x-ratelimit-reset", "60");
									serde_json::json!({"data":{"allV3":{"elements":{"edges":[],"pageInfo":{"hasNextPage":false}}}}})
								}
								GqlCase::Edge => {
									return Ok(
										HyperResponse::builder()
											.status(403)
											.header("content-type", "text/html")
											.body(Body::from("<html>edge</html>"))
											.unwrap(),
									)
								}
								GqlCase::Quota => {
									response = response.status(429).header("x-ratelimit-remaining", "0").header("x-ratelimit-reset", "60");
									serde_json::json!({"error":429})
								}
								GqlCase::Zero => {
									response = response.header("x-ratelimit-remaining", "0").header("x-ratelimit-reset", "60");
									serde_json::json!({"data":{}})
								}
								GqlCase::Resource => serde_json::json!({"errors":[{"extensions":{"code":"PRIVATE_SUBREDDIT"}}]}),
								GqlCase::Session => serde_json::json!({"errors":[{"extensions":{"code":"UNAUTHENTICATED"}}]}),
							}
						};
						Ok::<_, Infallible>(response.body(Body::from(value.to_string())).unwrap())
					}
				}))
			}
		});
		let server = hyper::Server::bind(&([127, 0, 0, 1], 0).into()).serve(service);
		let address = server.local_addr();
		Self {
			server: tokio::spawn(server),
			origin: RedditOrigin {
				base: Box::leak(format!("http://{address}").into_boxed_str()),
				host: Box::leak(address.to_string().into_boxed_str()),
			},
			gql,
			legacy,
			auth,
			requests,
		}
	}
	fn install(&self) {
		assert_eq!(configured_primary_lane(), RedditLane::Tor, "fixture must exercise actual Tor-primary routing");
		*upstream_guard(RedditLane::Tor) = UpstreamGuard::new(RedditLane::Tor);
		TOR_OAUTH_CLIENT.swap(None);
		OAUTH_CLIENT.swap(None);
		assert!(install_oauth_client(
			Oauth::request_fixture_with_refresh(RedditLane::Tor, Arc::new(WreqClient::builder().no_proxy().build().unwrap()), self.origin),
			true,
			None
		));
	}
}

#[test]
#[sealed_test(env=[("REDLIB_TOR_FALLBACK","on"),("REDLIB_TOR_PROXY","socks5h://127.0.0.1:1"),("REDLIB_REDDIT_PRIMARY_LANE","tor")])]
fn test_legacy_all_recovers_schema_failure_with_one_encoded_rest_request_and_original_cache_key() {
	tokio::runtime::Runtime::new().unwrap().block_on(async {
		let fixture = Fixture::new(GqlCase::Missing, LegacyCase::Listing).await;
		fixture.install();
		let path = "/r/all/hot.json?limit=2&t=day";
		let (first, second) = tokio::join!(json(path.into(), true), json(path.into(), true));
		let first = first.expect("one bounded legacy JSON read may recover this fixture");
		assert_eq!(second.unwrap(), first);
		assert_eq!(first["data"]["children"][0]["data"]["id"], "abc");
		assert_eq!(fixture.gql.load(Ordering::SeqCst), 1);
		assert_eq!(fixture.legacy.load(Ordering::SeqCst), 1);
		assert_eq!(fixture.auth.load(Ordering::SeqCst), 0);
		let requests = fixture.requests.lock().unwrap();
		let legacy = &requests[1];
		assert!(legacy.0.starts_with("/r/all%2B/hot?"), "{}", legacy.0);
		assert!(legacy.0.contains("limit=2"));
		assert!(legacy.0.contains("t=day"));
		assert_eq!(legacy.1, "Bearer local-test-fixture");
		assert!(legacy.2.contains("pref_quarantine_optin"));
		drop(requests);
		let key = (normalize_reddit_api_path(path), true);
		assert!(json_pause_cache_value(&*JSON_DYNAMIC_CACHED.lock().await, &key).is_some());
		assert_eq!(json(path.into(), true).await.unwrap(), first);
		assert_eq!(fixture.legacy.load(Ordering::SeqCst), 1);
	});
}

#[test]
fn test_legacy_all_url_cursor_and_response_validation_are_strict() {
	for sort in ["hot", "new", "top", "rising", "controversial", "best"] {
		let path = format!("/r/all/{sort}.json?t=week&limit=10");
		let Some(LegacyAllPlan::FirstPage(rest)) = legacy_all_plan(&path).unwrap() else {
			panic!("first page")
		};
		assert_eq!(rest, format!("/r/all%2B/{sort}.json?t=week&limit=10"));
		assert_eq!(api_endpoint_index(&rest), 0);
		assert!(PageRequest::from_path(&rest, false).is_none());
	}
	for direction in ["after", "before"] {
		let path = format!("/r/all/top.json?{direction}=redlib-legacy-all%3At3_next&t=week");
		assert_eq!(
			legacy_all_plan(&path).unwrap(),
			Some(LegacyAllPlan::Tagged(format!("/r/all%2B/top.json?{direction}=t3_next&t=week")))
		);
	}
	for path in [
		"/r/all/hot.json?after=redlib-legacy-all%3At1_comment",
		"/r/all/hot.json?after=redlib-legacy-all%3Agql%3Aopaque",
		"/r/all/hot.json?after=redlib-legacy-all%3At3_a&before=t3_b",
		"/r/all/hot.json?after=redlib-legacy-all%3At3_a&after=redlib-legacy-all%3At3_b",
		"/r/rust/hot.json?after=redlib-legacy-all%3At3_a",
		"/comments/abc.json?after=redlib-legacy-all%3At3_a",
		"/r/all/hot.json?before=redlib-legacy-all",
	] {
		assert!(legacy_all_plan(path).is_err(), "{path}");
	}
	assert_eq!(legacy_all_plan("/r/all/hot.json?after=gql%3Aexisting").unwrap(), None);
	for path in [
		"/r/all%2B/hot.json?after=gql%3Aopaque",
		"/r/all%2B/hot.json?after=t1_comment",
		"/r/all%2B/hot.json?after=t3_a&before=t3_b",
	] {
		assert!(!legacy_all_rest_cursors_valid(path));
	}
	assert!(legacy_all_rest_cursors_valid("/r/all%2B/hot.json?before=t3_previous"));
	let mut valid = listing();
	validate_legacy_all_listing(&mut valid, false).unwrap();
	assert_eq!(valid["data"]["after"], "redlib-legacy-all:t3_next");
	assert_eq!(valid["data"]["before"], "redlib-legacy-all:t3_previous");
	let mut wrong_kind = listing();
	wrong_kind["data"]["children"][0]["kind"] = Value::String("t1".into());
	let mut wrong_id = listing();
	wrong_id["data"]["children"][0]["data"]["id"] = Value::String("different".into());
	let mut missing_title = listing();
	missing_title["data"]["children"][0]["data"]["title"] = Value::Null;
	let mut wrong_cursor = listing();
	wrong_cursor["data"]["after"] = Value::String("gql:opaque".into());
	for mut invalid in [
		serde_json::json!({}),
		serde_json::json!({"kind":"Listing","data":{"children":[]}}),
		wrong_kind,
		wrong_id,
		missing_title,
		wrong_cursor,
	] {
		assert!(validate_legacy_all_listing(&mut invalid, false).is_err());
	}

	let mut private = listing();
	private["data"]["children"][0]["data"]["subreddit_type"] = Value::String("private".into());
	assert_eq!(validate_legacy_all_listing(&mut private, false), Err("private".into()));
	let mut quarantined = listing();
	quarantined["data"]["children"][0]["data"]["quarantine"] = Value::Bool(true);
	assert_eq!(validate_legacy_all_listing(&mut quarantined, false), Err("quarantined".into()));
	assert!(validate_legacy_all_listing(&mut quarantined, true).is_ok());
}

#[test]
#[sealed_test(env=[("REDLIB_TOR_FALLBACK","on"),("REDLIB_TOR_PROXY","socks5h://127.0.0.1:1"),("REDLIB_REDDIT_PRIMARY_LANE","tor")])]
fn test_legacy_all_tagged_next_previous_and_invalid_cursors_never_use_gql() {
	tokio::runtime::Runtime::new().unwrap().block_on(async {
		let fixture = Fixture::new(GqlCase::Missing, LegacyCase::Listing).await;
		fixture.install();
		for direction in ["after", "before"] {
			let value = json(format!("/r/all/top.json?t=week&{direction}=redlib-legacy-all%3At3_next"), false).await.unwrap();
			assert_eq!(value["data"]["after"], "redlib-legacy-all:t3_next");
			assert_eq!(value["data"]["before"], "redlib-legacy-all:t3_previous");
		}
		assert_eq!(fixture.gql.load(Ordering::SeqCst), 0);
		assert_eq!(fixture.legacy.load(Ordering::SeqCst), 2);
		assert_eq!(fixture.auth.load(Ordering::SeqCst), 0);
		let requests = fixture.requests.lock().unwrap();
		assert!(requests[0].0.contains("after=t3_next"));
		assert!(requests[1].0.contains("before=t3_next"));
		drop(requests);
		for path in [
			"/r/all/hot.json?after=redlib-legacy-all%3At1_bad",
			"/r/all/hot.json?after=redlib-legacy-all%3Agql%3Acursor",
			"/r/all/hot.json?after=redlib-legacy-all%3At3_a&before=t3_b",
		] {
			assert!(json_uncached(path.into(), false).await.is_err());
		}
		assert_eq!(fixture.legacy.load(Ordering::SeqCst), 2);
		assert_eq!(fixture.gql.load(Ordering::SeqCst), 0);
	});
}

#[test]
#[sealed_test(env=[("REDLIB_TOR_FALLBACK","on"),("REDLIB_TOR_PROXY","socks5h://127.0.0.1:1"),("REDLIB_REDDIT_PRIMARY_LANE","tor")])]
fn test_legacy_all_failed_fallback_preserves_errors_and_terminal_empty_is_temporary_without_auth() {
	tokio::runtime::Runtime::new().unwrap().block_on(async {
		for gql_case in [GqlCase::Missing, GqlCase::Empty] {
			for legacy_case in [
				LegacyCase::Empty,
				LegacyCase::Invalid,
				LegacyCase::PartialErrors,
				LegacyCase::PartialErrorString,
				LegacyCase::Html,
				LegacyCase::Redirect,
				LegacyCase::Unauthorized,
				LegacyCase::InvalidToken,
				LegacyCase::JsonUnauthorized,
				LegacyCase::Private,
				LegacyCase::Quarantined,
				LegacyCase::PrivatePost,
				LegacyCase::Quota,
			] {
				let fixture = Fixture::new(gql_case, legacy_case).await;
				fixture.install();
				let result = json_uncached("/r/all/hot.json".into(), false).await;
				match gql_case {
					GqlCase::Missing => assert_eq!(result, Err("Reddit is temporarily unable to load this page. Retry in 30 seconds".into())),
					GqlCase::Empty => assert_eq!(result, Err(legacy_all_unavailable())),
					_ => unreachable!(),
				}
				for _ in 0..5 {
					tokio::task::yield_now().await;
				}
				assert_eq!(fixture.gql.load(Ordering::SeqCst), 1);
				assert_eq!(fixture.legacy.load(Ordering::SeqCst), 1);
				assert_eq!(fixture.auth.load(Ordering::SeqCst), 0);
				assert_eq!(oauth_client(RedditLane::Tor).unwrap().generation, 0);
				assert_eq!(upstream_guard(RedditLane::Tor).rejected_oauth_generation, None);
				assert!(!TOR_OAUTH_IS_ROLLING_OVER.load(Ordering::SeqCst));
			}
		}
	});
}

#[test]
#[sealed_test(env=[("REDLIB_TOR_FALLBACK","on"),("REDLIB_TOR_PROXY","socks5h://127.0.0.1:1"),("REDLIB_REDDIT_PRIMARY_LANE","tor")])]
fn test_legacy_all_tagged_failures_do_not_claim_success_retire_recovery_or_refresh() {
	tokio::runtime::Runtime::new().unwrap().block_on(async {
		for legacy_case in [
			LegacyCase::Invalid,
			LegacyCase::PartialErrors,
			LegacyCase::PartialErrorString,
			LegacyCase::Html,
			LegacyCase::Redirect,
			LegacyCase::Unauthorized,
			LegacyCase::InvalidToken,
			LegacyCase::JsonUnauthorized,
			LegacyCase::Private,
			LegacyCase::Quarantined,
			LegacyCase::PrivatePost,
			LegacyCase::Quota,
		] {
			let fixture = Fixture::new(GqlCase::Missing, legacy_case).await;
			fixture.install();
			let expired = Instant::now() - Duration::from_secs(1);
			upstream_guard(RedditLane::Tor).rate_limit_blocked_until = Some(expired);
			let result = json_uncached("/r/all/hot.json?after=redlib-legacy-all%3At3_a".into(), false).await;
			assert!(result.is_err());
			for _ in 0..5 {
				tokio::task::yield_now().await;
			}
			let guard = upstream_guard(RedditLane::Tor);
			assert!(guard.rate_limit_blocked_until.is_some());
			assert!(!matches!(guard.quota.window, QuotaWindow::Unreported));
			assert_eq!(guard.rejected_oauth_generation, None);
			drop(guard);
			assert_eq!(fixture.gql.load(Ordering::SeqCst), 0);
			assert_eq!(fixture.legacy.load(Ordering::SeqCst), 1);
			assert_eq!(fixture.auth.load(Ordering::SeqCst), 0);
			assert!(!TOR_OAUTH_IS_ROLLING_OVER.load(Ordering::SeqCst));
			let message = result.unwrap_err();
			if matches!(legacy_case, LegacyCase::Private | LegacyCase::PrivatePost) {
				assert_eq!(message, "private");
			}
			if matches!(legacy_case, LegacyCase::Quarantined) {
				assert_eq!(message, "quarantined");
			}
		}
	});
}

#[test]
#[sealed_test(env=[("REDLIB_TOR_FALLBACK","on"),("REDLIB_TOR_PROXY","socks5h://127.0.0.1:1"),("REDLIB_REDDIT_PRIMARY_LANE","tor")])]
fn test_legacy_all_low_allowance_and_three_second_deadline_do_not_launch_auth() {
	tokio::runtime::Runtime::new().unwrap().block_on(async {
		let fixture = Fixture::new(GqlCase::Missing, LegacyCase::LowRemaining).await;
		fixture.install();
		{
			let mut guard = upstream_guard(RedditLane::Tor);
			guard.quota.window = QuotaWindow::Known {
				available: 100,
				reset_at: Instant::now() + Duration::from_secs(60),
			};
			guard.quota_rotation_armed = true;
		}
		json_uncached("/r/all/hot.json?after=redlib-legacy-all%3At3_a".into(), false).await.unwrap();
		for _ in 0..5 {
			tokio::task::yield_now().await;
		}
		assert_eq!(fixture.auth.load(Ordering::SeqCst), 0);
		assert!(!TOR_OAUTH_IS_ROLLING_OVER.load(Ordering::SeqCst));
		let fixture = Fixture::new(GqlCase::Missing, LegacyCase::Slow).await;
		fixture.install();
		let capacity = api_concurrency(RedditLane::Tor).available_permits();
		let result = tokio::time::timeout(Duration::from_secs(5), json_uncached("/r/all/hot.json".into(), false))
			.await
			.expect("one legacy try must finish near3s");
		assert!(result.is_err());
		assert_eq!(fixture.legacy.load(Ordering::SeqCst), 1);
		assert_eq!(fixture.auth.load(Ordering::SeqCst), 0);
		assert_eq!(api_concurrency(RedditLane::Tor).available_permits(), capacity);
	});
}

#[test]
#[sealed_test(env=[("REDLIB_TOR_FALLBACK","on"),("REDLIB_TOR_PROXY","socks5h://127.0.0.1:1"),("REDLIB_REDDIT_PRIMARY_LANE","tor")])]
fn test_legacy_all_quota_resource_and_session_classifications_never_trigger_fallback() {
	let runtime = tokio::runtime::Runtime::new().unwrap();
	runtime.block_on(async {
		for gql_case in [GqlCase::Quota, GqlCase::Zero, GqlCase::Resource, GqlCase::Session] {
			let fixture = Fixture::new(gql_case, LegacyCase::Listing).await;
			fixture.install();
			crate::oauth::set_refresh_backoff_for_test(RedditLane::Tor, Some(Duration::from_secs(60)));
			assert!(json_uncached("/r/all/hot.json".into(), false).await.is_err());
			assert_eq!(fixture.legacy.load(Ordering::SeqCst), 0);
		}
		let fixture = Fixture::new(GqlCase::Edge, LegacyCase::Listing).await;
		fixture.install();
		assert!(json_uncached("/r/all/hot.json".into(), false).await.is_ok());
		assert_eq!(fixture.legacy.load(Ordering::SeqCst), 1);
		assert_eq!(fixture.auth.load(Ordering::SeqCst), 0);
		let current = oauth_client(RedditLane::Tor).unwrap();
		assert_eq!(current.generation, 0);
		assert_eq!(current.graphql_origin(), fixture.origin);
	});
	// A Session error starts a background worker. Keep its backoff until the
	// runtime has cancelled it, so it cannot overwrite a replacement fixture.
	drop(runtime);
	crate::oauth::set_refresh_backoff_for_test(RedditLane::Tor, None);
}

#[tokio::test]
async fn test_legacy_all_latch_is_monotonic_request_local_and_not_inherited_by_auth_tasks() {
	let changed = Arc::new(Notify::new());
	let seen = Arc::new(Notify::new());
	let sibling_changed = changed.clone();
	let sibling_seen = seen.clone();
	let a = LEGACY_ALL_DISQUALIFIED.scope(std::cell::Cell::new(false), async {
		disqualify_legacy_all_recovery();
		seen.notify_one();
		changed.notified().await;
		assert!(LEGACY_ALL_DISQUALIFIED.with(std::cell::Cell::get));
		let child = tokio::spawn(async {
			disqualify_legacy_all_recovery();
			LEGACY_ALL_DISQUALIFIED.try_with(std::cell::Cell::get).is_err()
		});
		assert!(child.await.unwrap());
	});
	let b = LEGACY_ALL_DISQUALIFIED.scope(std::cell::Cell::new(false), async {
		sibling_seen.notified().await;
		assert!(!LEGACY_ALL_DISQUALIFIED.with(std::cell::Cell::get));
		sibling_changed.notify_one();
	});
	tokio::join!(a, b);
	disqualify_legacy_all_recovery();
}

#[test]
#[sealed_test(env=[("REDLIB_TOR_FALLBACK","on"),("REDLIB_TOR_PROXY","socks5h://127.0.0.1:1"),("REDLIB_REDDIT_PRIMARY_LANE","tor")])]
fn test_legacy_all_latch_retains_earlier_real_classification_after_guards_reset_and_later_schema_or_empty() {
	let runtime = tokio::runtime::Runtime::new().unwrap();
	runtime.block_on(async {
		for initial in [GqlCase::Quota, GqlCase::Zero, GqlCase::Resource, GqlCase::Session] {
			for later in [GqlCase::Missing, GqlCase::Empty] {
				let first = Fixture::new(initial, LegacyCase::Listing).await;
				first.install();
				crate::oauth::set_refresh_backoff_for_test(RedditLane::Tor, Some(Duration::from_secs(60)));
				LEGACY_ALL_DISQUALIFIED
					.scope(std::cell::Cell::new(false), async {
						let mut refresh = TorQuotaRefreshRetryState::default();
						let first_result = json_uncached_on_lane_with_options(
							"/r/all/hot.json".into(),
							false,
							RedditLane::Tor,
							AdmissionRetryMode::Immediate,
							Some(Duration::from_secs(2)),
							None,
							&mut refresh,
						)
						.await;
						assert!(first_result.0.is_err());
						assert!(LEGACY_ALL_DISQUALIFIED.with(std::cell::Cell::get));
						let second = Fixture::new(later, LegacyCase::Listing).await;
						second.install();
						let result = json_uncached_routed("/r/all/hot.json".into(), false).await;
						assert!(legacy_all_candidate(&result));
						assert!(LEGACY_ALL_DISQUALIFIED.with(std::cell::Cell::get));
						assert_eq!(second.legacy.load(Ordering::SeqCst), 0);
					})
					.await;
			}
		}
	});
	// A Session error starts a background worker. Keep its backoff until the
	// runtime has cancelled it, so it cannot overwrite a replacement fixture.
	drop(runtime);
	crate::oauth::set_refresh_backoff_for_test(RedditLane::Tor, None);
}

#[test]
#[sealed_test(env=[("REDLIB_TOR_FALLBACK","on"),("REDLIB_TOR_PROXY","socks5h://127.0.0.1:1"),("REDLIB_REDDIT_PRIMARY_LANE","tor")])]
fn test_legacy_all_current_quota_session_capacity_and_ready_direct_guards_are_preserved() {
	tokio::runtime::Runtime::new().unwrap().block_on(async {
		let fixture = Fixture::new(GqlCase::Missing, LegacyCase::Listing).await;
		fixture.install();
		let logical = "/r/all/hot.json";
		let rest = "/r/all%2B/hot.json";
		upstream_guard(RedditLane::Tor).graphql_rate_limit_blocked_until = Some(Instant::now() + Duration::from_secs(60));
		assert!(legacy_all_read(logical, rest.into(), false, true).await.is_err());
		{
			let mut guard = upstream_guard(RedditLane::Tor);
			guard.graphql_rate_limit_blocked_until = None;
			guard.graphql_quota.window = QuotaWindow::Known {
				available: 0,
				reset_at: Instant::now() + Duration::from_secs(60),
			};
		}
		assert!(legacy_all_read(logical, rest.into(), false, true).await.is_err());
		fixture.install();
		upstream_guard(RedditLane::Tor).rejected_oauth_generation = Some(0);
		assert!(legacy_all_read(logical, rest.into(), false, true).await.is_err());
		fixture.install();
		upstream_guard(RedditLane::Tor).rate_limit_blocked_until = Some(Instant::now() + Duration::from_secs(60));
		assert!(legacy_all_read(logical, rest.into(), false, true).await.is_err());
		fixture.install();
		let count = api_concurrency(RedditLane::Tor).available_permits() as u32;
		let capacity = api_concurrency(RedditLane::Tor).acquire_many(count).await.unwrap();
		assert!(legacy_all_read(logical, rest.into(), false, true).await.is_err());
		drop(capacity);
		let onion_count = TOR_ONION_CONCURRENCY.available_permits() as u32;
		let onion = TOR_ONION_CONCURRENCY.acquire_many(onion_count).await.unwrap();
		assert!(legacy_all_read(logical, rest.into(), false, true).await.is_err());
		drop(onion);
		assert_eq!(fixture.legacy.load(Ordering::SeqCst), 0);
		*upstream_guard(RedditLane::Direct) = UpstreamGuard::new(RedditLane::Direct);
		assert!(install_oauth_client(
			Oauth::request_fixture_with_refresh(RedditLane::Direct, Arc::new(WreqClient::builder().no_proxy().build().unwrap()), fixture.origin),
			true,
			None
		));
		upstream_guard(RedditLane::Tor).onion_transport.blocked_until = Some(Instant::now() + Duration::from_secs(60));
		let standby = DIRECT_STANDBY_CAPACITY.acquire_many(2).await.unwrap();
		assert!(legacy_all_read(logical, rest.into(), false, true).await.is_err());
		assert_eq!(fixture.legacy.load(Ordering::SeqCst), 0);
		drop(standby);
		assert!(legacy_all_read(logical, rest.into(), false, true).await.is_ok());
		assert_eq!(fixture.legacy.load(Ordering::SeqCst), 1);
		assert!(fixture.requests.lock().unwrap()[0].0.starts_with("/r/all%2B/hot.json"));
		assert_eq!(fixture.auth.load(Ordering::SeqCst), 0);
		let fixture = Fixture::new(GqlCase::Missing, LegacyCase::Unauthorized).await;
		fixture.install();
		*upstream_guard(RedditLane::Direct) = UpstreamGuard::new(RedditLane::Direct);
		assert!(install_oauth_client(
			Oauth::request_fixture_with_refresh(RedditLane::Direct, Arc::new(WreqClient::builder().no_proxy().build().unwrap()), fixture.origin),
			true,
			None
		));
		upstream_guard(RedditLane::Tor).onion_transport.blocked_until = Some(Instant::now() + Duration::from_secs(60));
		assert!(legacy_all_read(logical, rest.into(), false, true).await.is_err());
		for _ in 0..5 {
			tokio::task::yield_now().await;
		}
		assert_eq!(upstream_guard(RedditLane::Direct).rejected_oauth_generation, None);
		assert_eq!(oauth_client(RedditLane::Direct).unwrap().generation, 0);
		assert_eq!(fixture.auth.load(Ordering::SeqCst), 0);
		assert!(!OAUTH_IS_ROLLING_OVER.load(Ordering::SeqCst));
	});
}

#[test]
#[sealed_test(env=[("REDLIB_TOR_FALLBACK","on"),("REDLIB_TOR_PROXY","socks5h://127.0.0.1:1"),("REDLIB_REDDIT_PRIMARY_LANE","tor")])]
fn test_legacy_all_raw_alias_follows_its_returned_next_and_previous_tags() {
	tokio::runtime::Runtime::new().unwrap().block_on(async {
		let fixture = Fixture::new(GqlCase::Missing, LegacyCase::Listing).await;
		fixture.install();
		let first = json("/r/all%2B/hot.json".into(), false).await.unwrap();
		let next = first["data"]["after"].as_str().unwrap();
		let mut query = url::form_urlencoded::Serializer::new(String::new());
		query.append_pair("after", next);
		let second = json(format!("/r/all%2B/hot.json?{}", query.finish()), false)
			.await
			.expect("the raw alias must accept its own returned legacy tag");
		let previous = second["data"]["before"].as_str().unwrap();
		let mut query = url::form_urlencoded::Serializer::new(String::new());
		query.append_pair("before", previous);
		json(format!("/r/all%2B/hot.json?{}", query.finish()), false).await.unwrap();
		assert_eq!(fixture.gql.load(Ordering::SeqCst), 0);
		assert_eq!(fixture.legacy.load(Ordering::SeqCst), 3);
		assert_eq!(fixture.auth.load(Ordering::SeqCst), 0);
		let requests = fixture.requests.lock().unwrap();
		assert!(requests.iter().all(|request| request.0.starts_with("/r/all%2B/hot")));
		assert!(requests[1].0.contains("after=t3_next"));
		assert!(requests[2].0.contains("before=t3_previous"));
		assert!(
			!requests.iter().any(|request| request.0.contains("redlib-legacy-all")),
			"provenance tags must be removed before the actual REST request"
		);
		drop(requests);
		for invalid in [
			"/r/all%2B/hot.json?after=redlib-legacy-all%3A",
			"/r/all%2B/hot.json?after=redlib-legacy-all%3A&after=redlib-legacy-all%3A",
			"/r/all%2B/hot.json?after=redlib-legacy-all%3Agql%3Aopaque",
			"/r/all%2B/hot.json?after=redlib-legacy-all%3At1_comment",
			"/r/all%2B/hot.json?after=redlib-legacy-all%3At3_a&before=t3_b",
		] {
			assert!(json_uncached(invalid.into(), false).await.is_err());
		}
		assert_eq!(fixture.legacy.load(Ordering::SeqCst), 3);
	});
}

fn terminal_empty_listing() -> Value {
	serde_json::json!({"kind":"Listing","data":{"children":[],"after":null,"before":null}})
}

#[test]
#[sealed_test(env=[("REDLIB_TOR_FALLBACK","on"),("REDLIB_TOR_PROXY","socks5h://127.0.0.1:1"),("REDLIB_REDDIT_PRIMARY_LANE","tor")])]
fn test_legacy_all_terminal_empty_cached_value_cannot_bypass_a_current_quota_pause() {
	tokio::runtime::Runtime::new().unwrap().block_on(async {
		for expired in [false, true] {
			let fixture = Fixture::new(GqlCase::Missing, LegacyCase::Listing).await;
			fixture.install();
			let path = format!("/r/all/hot.json?empty_cache_expired={expired}");
			let key = (normalize_reddit_api_path(&path), false);
			{
				let mut cache = JSON_DYNAMIC_CACHED.lock().await;
				cache.cache_set_lifespan(if expired { Duration::ZERO } else { Duration::from_secs(60) });
				cache.cache_set(key.clone(), terminal_empty_listing());
			}
			{
				let mut guard = upstream_guard(RedditLane::Tor);
				guard.graphql_rate_limit_blocked_until = Some(Instant::now() + Duration::from_secs(60));
				guard.graphql_quota.window = QuotaWindow::Known {
					available: 0,
					reset_at: Instant::now() + Duration::from_secs(60),
				};
			}
			assert!(
				tokio::time::timeout(Duration::from_secs(5), json(path, false)).await.unwrap().is_err(),
				"an old terminal blank must not become fresh success during a quota pause"
			);
			assert_eq!(json_pause_cache_value(&*JSON_DYNAMIC_CACHED.lock().await, &key), None);
			assert_eq!(fixture.gql.load(Ordering::SeqCst), 0);
			assert_eq!(fixture.legacy.load(Ordering::SeqCst), 0);
			assert_eq!(fixture.auth.load(Ordering::SeqCst), 0);
		}
	});
}

#[test]
#[sealed_test(env=[("REDLIB_TOR_FALLBACK","on"),("REDLIB_TOR_PROXY","socks5h://127.0.0.1:1"),("REDLIB_REDDIT_PRIMARY_LANE","tor")])]
fn test_legacy_all_terminal_empty_refresh_preserves_expired_nonempty_last_good() {
	tokio::runtime::Runtime::new().unwrap().block_on(async {
		for (index, gql_case) in [GqlCase::Empty, GqlCase::Missing].into_iter().enumerate() {
			let fixture = Fixture::new(gql_case, LegacyCase::Invalid).await;
			fixture.install();
			let path = format!("/r/all/hot.json?last_good={index}");
			let key = (normalize_reddit_api_path(&path), false);
			let mut last_good = listing();
			last_good["data"]["children"][0]["data"]["title"] = Value::String("Last good All page".into());
			{
				let mut cache = JSON_DYNAMIC_CACHED.lock().await;
				cache.cache_set_lifespan(Duration::ZERO);
				cache.cache_set(key.clone(), last_good.clone());
			}
			assert_eq!(
				json(path.clone(), false).await.unwrap(),
				last_good,
				"new terminal-empty or failed refresh must preserve the expired nonempty fallback"
			);
			assert_eq!(json_pause_cache_value(&*JSON_DYNAMIC_CACHED.lock().await, &key), Some(last_good.clone()));
			JSON_DYNAMIC_CACHED.lock().await.cache_set_lifespan(Duration::from_secs(60));
			assert_eq!(json(path, false).await.unwrap(), last_good);
			assert_eq!(fixture.gql.load(Ordering::SeqCst), 1);
			assert_eq!(fixture.legacy.load(Ordering::SeqCst), 1);
			assert_eq!(fixture.auth.load(Ordering::SeqCst), 0);
		}
	});
}

#[test]
#[sealed_test(env=[("REDLIB_TOR_FALLBACK","on"),("REDLIB_TOR_PROXY","socks5h://127.0.0.1:1"),("REDLIB_REDDIT_PRIMARY_LANE","tor")])]
fn test_legacy_all_terminal_empty_is_not_recached_even_when_history_skips_recovery() {
	tokio::runtime::Runtime::new().unwrap().block_on(async {
		for (index, gql_case) in [GqlCase::Empty, GqlCase::ZeroEmpty].into_iter().enumerate() {
			let fixture = Fixture::new(gql_case, LegacyCase::Invalid).await;
			fixture.install();
			let path = format!("/r/all/hot.json?terminal_empty={index}");
			let key = (normalize_reddit_api_path(&path), false);
			assert!(json(path, false).await.is_err());
			assert_eq!(json_pause_cache_value(&*JSON_DYNAMIC_CACHED.lock().await, &key), None);
			assert_eq!(fixture.gql.load(Ordering::SeqCst), 1);
			assert_eq!(fixture.legacy.load(Ordering::SeqCst), if index == 0 { 1 } else { 0 });
			assert_eq!(fixture.auth.load(Ordering::SeqCst), 0);
		}
	});
}

#[test]
#[sealed_test(env=[("REDLIB_TOR_FALLBACK","on"),("REDLIB_TOR_PROXY","socks5h://127.0.0.1:1"),("REDLIB_REDDIT_PRIMARY_LANE","tor")])]
fn test_legacy_all_terminal_empty_policy_preserves_cursor_progress_other_feeds_later_pages_and_quarantine_keys() {
	tokio::runtime::Runtime::new().unwrap().block_on(async {
		let fixture = Fixture::new(GqlCase::EmptyWithNext, LegacyCase::Listing).await;
		fixture.install();
		let path = "/r/all/hot.json?empty_with_next=1";
		let first = json(path.into(), false).await.unwrap();
		assert_eq!(first["data"]["children"], serde_json::json!([]));
		assert_eq!(first["data"]["after"], "opaque-next");
		assert_eq!(json(path.into(), false).await.unwrap(), first);
		assert_eq!(fixture.gql.load(Ordering::SeqCst), 1);
		assert_eq!(fixture.legacy.load(Ordering::SeqCst), 0);
		for path in ["/r/fixture/hot.json?other_empty=1", "/r/all/hot.json?after=opaque-existing"] {
			let key = (normalize_reddit_api_path(path), false);
			JSON_DYNAMIC_CACHED.lock().await.cache_set(key, terminal_empty_listing());
			assert_eq!(json(path.into(), false).await.unwrap(), terminal_empty_listing());
		}
		let path = "/r/all/hot.json?separate_quarantine=1";
		let key = (normalize_reddit_api_path(path), true);
		let retained = listing();
		JSON_DYNAMIC_CACHED.lock().await.cache_set(key.clone(), retained.clone());
		assert_eq!(json(path.into(), true).await.unwrap(), retained);
		assert_eq!(json_pause_cache_value(&*JSON_DYNAMIC_CACHED.lock().await, &key), Some(retained));
		assert_eq!(fixture.gql.load(Ordering::SeqCst), 1);
	});
}
