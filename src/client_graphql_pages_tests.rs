use super::*;
use sealed_test::prelude::*;
use std::sync::atomic::AtomicUsize;

#[test]
#[sealed_test(env = [("REDLIB_TOR_FALLBACK","on"),("REDLIB_REDDIT_MAX_CONCURRENCY","4"),("REDLIB_REDDIT_PRIMARY_LANE","tor")])]
fn test_graphql_pages_queued_followers_resume_while_second_operation_is_held() {
	tokio::runtime::Runtime::new().unwrap().block_on(async {
		use hyper::service::{make_service_fn, service_fn};
		use std::convert::Infallible;
		let first_seen = Arc::new(Notify::new());
		let first_release = Arc::new(Notify::new());
		let second_seen = Arc::new(Notify::new());
		let second_release = Arc::new(Notify::new());
		let signals = (first_seen.clone(), first_release.clone(), second_seen.clone(), second_release.clone());
		let service = make_service_fn(move |_| {
			let signals = signals.clone();
			async move {
				Ok::<_, Infallible>(service_fn(move |request: HyperRequest<Body>| {
					let (first_seen, first_release, second_seen, second_release) = signals.clone();
					async move {
						let payload: Value = serde_json::from_slice(&hyper::body::to_bytes(request.into_body()).await.unwrap()).unwrap();
						let body = if payload["operationName"] == "AllFeedSdui" {
							let (mut sender, body) = Body::channel();
							first_seen.notify_one();
							tokio::spawn(async move {
								first_release.notified().await;
								let value = serde_json::json!({"data":{"allV3":{"elements":{"edges":[{"node":{"groupId":"t3_fixture"}}],
                                    "pageInfo":{"hasNextPage":false,"endCursor":null}}}}});
								sender.send_data(value.to_string().into()).await.unwrap();
							});
							body
						} else {
							assert_eq!(payload["operationName"], "PostsByIds");
							second_seen.notify_one();
							second_release.notified().await;
							Body::from(
								serde_json::json!({"data":{"postsInfoByIds":[{"__typename":"SubredditPost","id":"t3_fixture",
                                    "postTitle":"Fixture","subreddit":{"name":"fixture","type":"PUBLIC"}}]}})
								.to_string(),
							)
						};
						Ok::<_, Infallible>(
							HyperResponse::builder()
								.header("content-type", "application/json")
								.header("x-ratelimit-remaining", "100")
								.header("x-ratelimit-reset", "60")
								.body(body)
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
		let path = "/r/all/hot.json";
		*upstream_guard(lane) = UpstreamGuard::new(lane);
		let http = Arc::new(WreqClient::builder().no_proxy().build().unwrap());
		assert!(install_oauth_client(Oauth::request_fixture(lane, http), true, None));
		{
			let mut guard = upstream_guard(lane);
			guard.transport.blocked_until = Some(Instant::now() - Duration::from_secs(1));
			guard.graphql_rate_limit_blocked_until = Some(Instant::now() - Duration::from_secs(1));
		}
		let owner = tokio::spawn(async move {
			let mut refresh = TorQuotaRefreshRetryState::default();
			let (oauth, mut attempt, _permit, _probe) = begin_upstream_attempt(lane, api_endpoint_index(path), AdmissionRetryMode::Bounded, &mut refresh)
				.await
				.unwrap();
			reddit_pages(path, false, oauth, &mut attempt, false, PageRequest::from_path(path, false).unwrap(), origin)
				.await
				.unwrap()
		});
		tokio::time::timeout(Duration::from_secs(2), first_seen.notified()).await.unwrap();
		let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
		let mut followers = Box::pin(async {
			let (first, second, third) = tokio::join!(
				acquire_attempt_capacity(lane, 7, AdmissionRetryMode::Bounded, deadline),
				acquire_attempt_capacity(lane, 7, AdmissionRetryMode::Bounded, deadline),
				acquire_attempt_capacity(lane, 7, AdmissionRetryMode::Bounded, deadline),
			);
			[first.unwrap(), second.unwrap(), third.unwrap()]
		});
		assert!(tokio::time::timeout(Duration::from_millis(20), &mut followers).await.is_err());
		assert!(
			upstream_guard(lane).transport.blocked_until.is_some(),
			"200 headers alone cannot prove a slow GraphQL body is quota-safe"
		);
		assert!(upstream_guard(lane).graphql_rate_limit_blocked_until.is_some());
		first_release.notify_one();
		tokio::time::timeout(Duration::from_secs(2), second_seen.notified()).await.unwrap();
		let started = Instant::now();
		let resumed = tokio::time::timeout(Duration::from_millis(200), &mut followers).await.unwrap();
		println!("queued followers available after validated/reserved first operation: {} ms", started.elapsed().as_millis());
		assert!(resumed.iter().all(|(_, probe)| probe.is_none()));
		assert!(!owner.is_finished(), "the original logical request is still waiting for its second operation");
		assert_eq!(TOR_RECOVERY_PROBE.available_permits(), 0);
		assert_eq!(
			upstream_guard(lane).graphql_quota.next_request_id,
			2,
			"continuation must reserve before followers are released"
		);
		second_release.notify_one();
		let RedditReadResponse::Pages(response) = tokio::time::timeout(Duration::from_secs(2), owner).await.unwrap().unwrap() else {
			panic!("page response")
		};
		assert_eq!(response.status(), 200);
		drop(resumed);
		server.abort();
	});
}

#[test]
#[sealed_test(env = [("REDLIB_TOR_FALLBACK","on"),("REDLIB_REDDIT_PRIMARY_LANE","tor")])]
fn test_graphql_pages_recovery_waits_for_quota_safe_evidence_and_preserves_resource_errors() {
	tokio::runtime::Runtime::new().unwrap().block_on(async {
		use hyper::service::{make_service_fn, service_fn};
		use std::convert::Infallible;
		let error = |code: &str| serde_json::json!({"errors":[{"extensions":{"code":code}}]}).to_string();
		let empty_feed = serde_json::json!({"data":{"postFeed":{"elements":{"edges":[],"pageInfo":{"hasNextPage":false,"endCursor":null}}}}}).to_string();
		let private_post = serde_json::json!({"data":{"postFeed":{"elements":{"edges":[{"node":{"__typename":"SubredditPost","id":"t3_private",
                "postTitle":"Private","subreddit":{"name":"private","type":"PRIVATE"}}}],"pageInfo":{"hasNextPage":false,"endCursor":null}}}}})
		.to_string();
		let cases = [
			(429, Some("100"), "application/json", false, "{}".to_string(), false),
			(403, Some("100"), "text/html", false, "<html>quota denial</html>".to_string(), false),
			(403, None, "text/html", true, "<html>token denied</html>".to_string(), false),
			(200, Some("0"), "application/json", false, empty_feed, false),
			(200, Some("100"), "application/json", false, error("RATE_LIMITED"), false),
			(200, Some("100"), "application/json", false, error("UNAUTHENTICATED"), false),
			(200, Some("100"), "application/json", false, error("UNKNOWN_ERROR"), false),
			(200, Some("100"), "application/json", false, error("PRIVATE_SUBREDDIT"), true),
			(200, Some("100"), "application/json", false, private_post, true),
			(403, None, "text/html", false, "<html>edge denied</html>".to_string(), true),
		];
		for (status, remaining, content_type, rejected_token, body, may_release) in cases {
			let release = Arc::new(Notify::new());
			let server_release = release.clone();
			let service = make_service_fn(move |_| {
				let body = body.clone();
				let release = server_release.clone();
				async move {
					Ok::<_, Infallible>(service_fn(move |_request: HyperRequest<Body>| {
						let body = body.clone();
						let release = release.clone();
						async move {
							let (mut sender, stream) = Body::channel();
							tokio::spawn(async move {
								release.notified().await;
								let _ = sender.send_data(body.into()).await;
							});
							let mut response = HyperResponse::builder().status(status).header("content-type", content_type);
							if let Some(remaining) = remaining {
								response = response.header("x-ratelimit-remaining", remaining).header("x-ratelimit-reset", "60");
								if status == 403 {
									response = response.header("retry-after", "0");
								}
							}
							if rejected_token {
								response = response.header("www-authenticate", "Bearer error=\"invalid_token\"");
							}
							Ok::<_, Infallible>(response.body(stream).unwrap())
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
			let path = "/r/fixture/hot.json";
			*upstream_guard(lane) = UpstreamGuard::new(lane);
			let http = Arc::new(WreqClient::builder().no_proxy().build().unwrap());
			assert!(install_oauth_client(Oauth::request_fixture(lane, http), true, None));
			{
				let mut guard = upstream_guard(lane);
				guard.transport.blocked_until = Some(Instant::now() - Duration::from_secs(1));
				guard.graphql_rate_limit_blocked_until = Some(Instant::now() - Duration::from_secs(1));
				guard.edges[7].failures = 5;
			}
			let mut refresh = TorQuotaRefreshRetryState::default();
			let (oauth, mut attempt, _permit, _probe) = begin_upstream_attempt(lane, 7, AdmissionRetryMode::Bounded, &mut refresh).await.unwrap();
			let mut read = Box::pin(reddit_pages(path, false, oauth, &mut attempt, false, PageRequest::from_path(path, false).unwrap(), origin));
			let early = tokio::time::timeout(Duration::from_millis(20), &mut read).await;
			if status == 200 {
				assert!(early.is_err(), "the fixture intentionally holds the GraphQL body");
				assert!(upstream_guard(lane).transport.blocked_until.is_some(), "unvalidated 200 headers must not release recovery");
				assert!(upstream_guard(lane).graphql_rate_limit_blocked_until.is_some());
			}
			release.notify_one();
			let result = match early {
				Ok(result) => result,
				Err(_) => tokio::time::timeout(Duration::from_secs(2), read.as_mut()).await.unwrap(),
			};
			drop(read);
			assert_eq!(
				upstream_guard(lane).transport.blocked_until.is_none(),
				may_release,
				"status={status}, remaining={remaining:?}"
			);
			assert_eq!(upstream_guard(lane).graphql_rate_limit_blocked_until.is_none(), may_release);
			assert!(!attempt.completed, "a validated resource error must not become a successful logical page");
			assert_eq!(upstream_guard(lane).edges[7].failures, 5);
			if status == 200 && may_release {
				assert!(
					matches!(result, Err(ApiRequestError::Terminal { session_unavailable: false, .. })),
					"resource error must remain terminal"
				);
			}
			server.abort();
		}
	});
}

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
