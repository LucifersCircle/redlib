use super::*;
use sealed_test::prelude::*;
use std::sync::atomic::AtomicUsize;

#[test]
#[sealed_test(env = [("REDLIB_TOR_FALLBACK","on"),("REDLIB_TOR_PROXY","socks5h://127.0.0.1:1"),("REDLIB_REDDIT_PRIMARY_LANE","tor")])]
fn test_graphql_pages_transport_retains_session_probe_and_counts_every_send() {
	tokio::runtime::Runtime::new().unwrap().block_on(async {
		use hyper::service::{make_service_fn, service_fn};
		use std::convert::Infallible;
		let mode = Arc::new(AtomicUsize::new(0));
		let calls = Arc::new(Mutex::new(Vec::<String>::new()));
		let server_mode = mode.clone();
		let server_calls = calls.clone();
		let lane = RedditLane::Tor;
		let path = "/r/all/hot.json";
		let endpoint = api_endpoint_index(path);
		assert_eq!(endpoint, 7);
		let service = make_service_fn(move |_| {
			let mode = server_mode.clone();
			let calls = server_calls.clone();
			async move {
				Ok::<_, Infallible>(service_fn(move |request: HyperRequest<Body>| {
					let mode = mode.clone();
					let calls = calls.clone();
					async move {
						assert_eq!(request.method(), hyper::Method::POST);
						assert_eq!(request.headers()["authorization"], "Bearer local-test-fixture");
						assert_eq!(request.headers().get_all("content-type").iter().count(), 1);
						let payload: Value = serde_json::from_slice(&hyper::body::to_bytes(request.into_body()).await.unwrap()).unwrap();
						let operation = payload["operationName"].as_str().unwrap().to_string();
						calls.lock().unwrap().push(operation.clone());
						assert!(matches!(upstream_guard(lane).edges[endpoint].state, EdgeCircuitState::HalfOpen { .. }));
						assert!(upstream_guard(lane).onion_transport.blocked_until.is_some());
						let remaining;
						let body = if operation == "AllFeedSdui" {
							remaining = if mode.load(Ordering::SeqCst) == 1 { "0" } else { "100" };
							serde_json::json!({"data":{"allV3":{"elements":{"edges":[{"node":{"groupId":"t3_fixture"}}],
                            "pageInfo":{"hasNextPage":false,"endCursor":null}}}}})
						} else {
							assert_eq!(operation, "PostsByIds");
							assert_eq!(payload["variables"]["ids"], serde_json::json!(["t3_fixture"]));
							remaining = "99";
							if mode.load(Ordering::SeqCst) == 2 {
								serde_json::json!({"errors":[{"extensions":{"code":"RATE_LIMITED"}}]})
							} else {
								serde_json::json!({"data":{"postsInfoByIds":[{"__typename":"SubredditPost","id":"t3_fixture","postTitle":"Fixture","subreddit":{"name":"fixture","type":"PUBLIC"}}]}})
							}
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
		for scenario in 0..3 {
			mode.store(scenario, Ordering::SeqCst);
			calls.lock().unwrap().clear();
			*upstream_guard(lane) = UpstreamGuard::new(lane);
			assert!(install_oauth_client(Oauth::request_fixture(lane, http.clone()), true, None));
			upstream_guard(lane).record_endpoint_failure(Instant::now(), 0);
			upstream_guard(lane).edges[endpoint].state = EdgeCircuitState::Open {
				until: Instant::now() - Duration::from_millis(1),
			};
			let mut refresh = TorQuotaRefreshRetryState::default();
			let (oauth, mut attempt, permit, probe) = begin_upstream_attempt(lane, endpoint, AdmissionRetryMode::Bounded, &mut refresh).await.unwrap();
			let result = reddit_pages(path, false, oauth, &mut attempt, false, PageRequest::from_path(path, false).unwrap(), origin).await;
			if scenario == 0 {
				let RedditReadResponse::Pages(response) = result.unwrap() else {
					panic!("page response")
				};
				let value: Value = serde_json::from_slice(&hyper::body::to_bytes(response.into_body()).await.unwrap()).unwrap();
				assert_eq!(value["data"]["children"][0]["data"]["title"], "Fixture");
				assert_eq!(*calls.lock().unwrap(), ["AllFeedSdui", "PostsByIds"]);
				assert_eq!(upstream_guard(lane).graphql_quota.next_request_id, 2);
				record_upstream_success(&mut attempt, path);
				assert!(matches!(upstream_guard(lane).edges[endpoint].state, EdgeCircuitState::Closed));
			} else {
				assert!(matches!(result, Err(ApiRequestError::Deferred { .. })));
				assert_eq!(calls.lock().unwrap().len(), if scenario == 1 { 1 } else { 2 });
				if scenario == 2 {
					assert!(upstream_guard(lane).graphql_rate_limit_blocked_until.is_some());
				} else {
					assert!(upstream_guard(lane).graphql_quota.reserve_exhausted(Instant::now()));
				}
			}
			assert_eq!(upstream_guard(lane).quota.next_request_id, 0);
			assert!(upstream_guard(lane).onion_transport.blocked_until.is_some());
			assert!(upstream_guard(lane).transport.blocked_until.is_none());
			drop(attempt);
			drop(permit);
			drop(probe);
		}
		server.abort();
	});
}

#[test]
#[sealed_test(env = [("REDLIB_TOR_FALLBACK","on"),("REDLIB_TOR_PROXY","socks5h://127.0.0.1:1"),("REDLIB_REDDIT_PRIMARY_LANE","tor")])]
fn test_endpoint_edge_tor_fixture_recovers_on_the_next_request() {
	tokio::runtime::Runtime::new().unwrap().block_on(async {
		use hyper::service::{make_service_fn, service_fn};
		use std::convert::Infallible;
		let calls = Arc::new(AtomicUsize::new(0));
		let server_calls = calls.clone();
		let service = make_service_fn(move |_| {
			let calls = server_calls.clone();
			async move {
				Ok::<_, Infallible>(service_fn(move |request: HyperRequest<Body>| {
					let calls = calls.clone();
					async move {
						assert_eq!(request.headers()["authorization"], "Bearer local-test-fixture");
						let payload: Value = serde_json::from_slice(&hyper::body::to_bytes(request.into_body()).await.unwrap()).unwrap();
						assert_eq!(payload["operationName"], "SubredditFeedElements");
						let rejected = calls.fetch_add(1, Ordering::SeqCst) == 0;
						let body = if rejected {
							"<html>Edge rejected this request</html>".to_string()
						} else {
							serde_json::json!({"data":{"postFeed":{"elements":{
								"edges":[{"node":{"__typename":"SubredditPost","id":"t3_recovered","postTitle":"Recovered page",
									"subreddit":{"name":"fixture","type":"PUBLIC"}}}],
								"pageInfo":{"hasNextPage":false,"endCursor":null}
							}}}})
							.to_string()
						};
						Ok::<_, Infallible>(
							HyperResponse::builder()
								.status(if rejected { 403 } else { 200 })
								.header("content-type", if rejected { "text/html" } else { "application/json" })
								.header("retry-after", "0")
								.body(Body::from(body))
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
		let lane = RedditLane::Tor;
		*upstream_guard(lane) = UpstreamGuard::new(lane);
		let http = Arc::new(WreqClient::builder().no_proxy().build().unwrap());
		assert!(install_oauth_client(Oauth::request_fixture(lane, http), true, None));
		let now = Instant::now();
		{
			let mut guard = upstream_guard(lane);
			guard.transport.blocked_until = Some(now - Duration::from_secs(1));
			guard.graphql_rate_limit_blocked_until = Some(now - Duration::from_secs(1));
			guard.onion_transport.blocked_until = Some(now + Duration::from_secs(60));
		}
		let path = "/r/fixture/hot.json";
		for rejected in [true, false] {
			let request = async {
				let mut refresh = TorQuotaRefreshRetryState::default();
				let (oauth, mut attempt, _permit, _probe) = begin_upstream_attempt(lane, api_endpoint_index(path), AdmissionRetryMode::Bounded, &mut refresh)
					.await
					.unwrap();
				let result = reddit_pages(path, false, oauth, &mut attempt, false, PageRequest::from_path(path, false).unwrap(), origin)
					.await
					.unwrap();
				let RedditReadResponse::Pages(response) = result else { panic!("page response") };
				if rejected {
					assert_eq!(response.status(), 403);
					{
						let guard = upstream_guard(lane);
						assert!(guard.transport.blocked_until.is_none(), "a received HTTP response must end an expired transport recovery");
						assert!(
							guard.graphql_rate_limit_blocked_until.is_none(),
							"an expired quota deadline must not keep the recovery gate active"
						);
						assert!(guard.onion_transport.blocked_until.is_some(), "a public HTTPS response must not clear onion recovery");
					}
					let mut followers = Vec::new();
					for _ in 0..3 {
						let capacity = acquire_attempt_capacity(lane, api_endpoint_index(path), AdmissionRetryMode::Immediate, tokio::time::Instant::now())
							.await
							.expect("followers must use normal concurrency even while the original recovery probe is still held");
						assert!(capacity.1.is_none());
						followers.push(capacity);
					}
					let decision = block_for_edge_throttle(&mut attempt, Some(Duration::ZERO));
					assert_eq!(decision.delay, Duration::ZERO);
					assert!(!decision.started_cooldown);
				} else {
					assert_eq!(response.status(), 200);
					let value: Value = serde_json::from_slice(&hyper::body::to_bytes(response.into_body()).await.unwrap()).unwrap();
					assert_eq!(value["data"]["children"][0]["data"]["title"], "Recovered page");
					record_upstream_success(&mut attempt, path);
				}
			};
			tokio::time::timeout(Duration::from_secs(2), request)
				.await
				.expect("the next Tor request must not wait for an edge cooldown");
		}
		assert_eq!(calls.load(Ordering::SeqCst), 2);
		assert_eq!(upstream_guard(lane).edges[api_endpoint_index(path)].failures, 0);
		server.abort();
	});
}
