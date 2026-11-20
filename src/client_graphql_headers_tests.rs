use super::*;
use sealed_test::prelude::*;
use std::sync::atomic::AtomicUsize;

#[derive(Clone)]
struct Wire {
	path: String,
	headers: hyper::HeaderMap,
	payload: Option<Value>,
}
struct Fixture {
	origin: RedditOrigin,
	server: tokio::task::JoinHandle<Result<(), hyper::Error>>,
	wires: Arc<Mutex<Vec<Wire>>>,
	auth: Arc<AtomicUsize>,
}
impl Drop for Fixture {
	fn drop(&mut self) {
		self.server.abort();
	}
}
impl Fixture {
	async fn new() -> Self {
		use hyper::service::{make_service_fn, service_fn};
		use std::convert::Infallible;
		let wires = Arc::new(Mutex::new(Vec::new()));
		let auth = Arc::new(AtomicUsize::new(0));
		let shared = (wires.clone(), auth.clone());
		let service = make_service_fn(move |_| {
			let shared = shared.clone();
			async move {
				Ok::<_, Infallible>(service_fn(move |request: HyperRequest<Body>| {
					let (wires, auth) = shared.clone();
					async move {
						if request.uri().path().starts_with("/auth/") {
							auth.fetch_add(1, Ordering::SeqCst);
							return Ok::<_, Infallible>(HyperResponse::new(Body::from(r#"{"access_token":"unexpected-refresh","expires_in":3600}"#)));
						}
						let path = request.uri().path().to_string();
						let headers = request.headers().clone();
						let payload = if request.method() == hyper::Method::POST {
							Some(serde_json::from_slice::<Value>(&hyper::body::to_bytes(request.into_body()).await.unwrap()).unwrap())
						} else {
							None
						};
						wires.lock().unwrap().push(Wire {
							path,
							headers,
							payload: payload.clone(),
						});
						let value = match payload.as_ref().and_then(|payload| payload["operationName"].as_str()) {
							Some("PostsByIds") => {
								serde_json::json!({"data":{"postsInfoByIds":[{"__typename":"SubredditPost","id":"t3_abc","postTitle":"Fixture","subreddit":{"name":"fixture","type":"PUBLIC"}}]}})
							}
							Some("PostComments") => serde_json::json!({"data":{"postInfoById":{"id":"t3_abc","commentForest":{"trees":[]}}}}),
							Some("AllFeedSdui") => serde_json::json!({"data":{"allV3":{"elements":{"edges":[{"node":{"groupId":"t3_abc"}}],"pageInfo":{"hasNextPage":false}}}}}),
							None => serde_json::json!({"kind":"Listing","data":{"children":[]}}),
							operation => panic!("unexpected operation {operation:?}"),
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
			wires,
			auth,
		}
	}
}

fn one_header<'a>(wire: &'a Wire, name: &str) -> &'a str {
	let values: Vec<_> = wire.headers.get_all(name).iter().collect();
	assert_eq!(values.len(), 1, "expected exactly one {name} on this wire");
	values[0].to_str().unwrap()
}

#[test]
#[sealed_test(env=[("REDLIB_TOR_FALLBACK","off"),("REDLIB_REDDIT_PRIMARY_LANE","direct")])]
fn test_graphql_comments_request_headers_match_each_payload_with_android_browser_transports() {
	tokio::runtime::Runtime::new().unwrap().block_on(async {
		for profile in [OauthTransportProfile::Chrome145Android, OauthTransportProfile::Firefox147Android] {
			let fixture = Fixture::new().await;
			let lane = RedditLane::Direct;
			*upstream_guard(lane) = UpstreamGuard::new(lane);
			let client = Arc::new(build_oauth_client(lane, None, profile).unwrap());
			let oauth = Oauth::request_fixture_with_refresh(lane, client.clone(), fixture.origin);
			let identity_headers = oauth.headers_map.clone();
			assert!(identity_headers["User-Agent"].starts_with("Reddit/"));
			assert_eq!(identity_headers["Content-Type"], "application/json; charset=UTF-8");
			assert!(install_oauth_client(oauth, true, None));
			for path in ["/comments/abc.json", "/r/all/hot.json"] {
				assert!(json_uncached_on_lane(path.into(), false, lane).await.0.is_ok());
			}
			let oauth = oauth_client(lane).unwrap();
			request_once(&Method::GET, "/rest".into(), false, fixture.origin, oauth, lane, None).await.unwrap();
			let mut control = client.get(format!("{}/control", fixture.origin.base));
			for (key, value) in &identity_headers {
				control = control.header(key, value);
			}
			control.send().await.unwrap();
			let wires = fixture.wires.lock().unwrap();
			assert_eq!(wires.len(), 6, "both logical reads require two operations, followed by REST and control");
			let operations = ["PostsByIds", "PostComments", "AllFeedSdui", "PostsByIds"];
			for (wire, expected_operation) in wires[..4].iter().zip(operations) {
				let payload = wire.payload.as_ref().unwrap();
				assert_eq!(payload["operationName"], expected_operation);
				assert_eq!(one_header(wire, "x-apollo-operation-name"), expected_operation);
				assert_eq!(
					one_header(wire, "x-apollo-operation-id"),
					payload["extensions"]["persistedQuery"]["sha256Hash"].as_str().unwrap()
				);
				assert_eq!(one_header(wire, "accept"), "application/graphql-response+json, application/json");
				assert_eq!(one_header(wire, "content-type"), identity_headers["Content-Type"]);
				for name in ["Authorization", "User-Agent", "client-vendor-id", "X-Reddit-Device-Id"] {
					assert_eq!(one_header(wire, name), identity_headers[name]);
				}
			}
			let rest = &wires[4];
			let control = &wires[5];
			assert_eq!(rest.path, "/rest");
			assert_eq!(control.path, "/control");
			assert_eq!(
				one_header(rest, "accept"),
				one_header(control, "accept"),
				"REST must retain the ordinary emulated-browser Accept"
			);
			assert!(
				one_header(control, "accept").contains("text/html"),
				"the fixture must expose browser defaults rather than a bare client"
			);
			assert_eq!(one_header(rest, "content-type"), identity_headers["Content-Type"]);
			assert_eq!(one_header(rest, "authorization"), identity_headers["Authorization"]);
			for wire in [rest, control] {
				assert!(wire.payload.is_none());
				assert!(!wire.headers.contains_key("x-apollo-operation-name"));
				assert!(!wire.headers.contains_key("x-apollo-operation-id"));
			}
			assert_eq!(fixture.auth.load(Ordering::SeqCst), 0);
			assert!(!OAUTH_IS_ROLLING_OVER.load(Ordering::SeqCst));
		}
	});
}
