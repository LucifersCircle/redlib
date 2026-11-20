use super::*;
use hyper::service::{make_service_fn, service_fn};
use sealed_test::prelude::*;
use std::convert::Infallible;

struct Fixture {
	origin: String,
	transport: Arc<Transport>,
	server: tokio::task::JoinHandle<Result<(), hyper::Error>>,
	wires: Arc<Mutex<Vec<(String, String, hyper::HeaderMap, String)>>>,
	mode: Arc<AtomicUsize>,
}
impl Drop for Fixture {
	fn drop(&mut self) {
		self.server.abort();
	}
}
fn post(name: &str, id: &str, score: usize, pinned: bool, old: bool) -> String {
	let created = if old {
		1
	} else {
		SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs()
	};
	format!(
		r#"<shreddit-post id="t3_{id}" permalink="/r/{name}/comments/{id}/title/" content-href="https://www.reddit.com/r/{name}/comments/{id}/title/" post-title="Post {id}" author="alice" subreddit-name="{name}" score="{score}" comment-count="0" created-timestamp="{created}" {}><div slot="text-body"><p>Readable body</p></div></shreddit-post>"#,
		if pinned { "is-stickied" } else { "" }
	)
}
fn comment(id: &str) -> String {
	format!(r#"<shreddit-comment thingid="t1_{id}" author="bob" score="2"><div id="t1_{id}-comment-rtjson-content" slot="comment"><p>Comment {id}</p></div></shreddit-comment>"#)
}
// Recorded script/form contract from 0xMassi/webclaw 3c2006a13e5f96ea2a6956f578b9afefb8f5d8b7.
fn script_check(action: &str) -> String {
	format!(
		r#"<html><head><title>Reddit - Prove your humanity</title><script>document.addEventListener("DOMContentLoaded",async function(){{var e=document.forms[0],n=(e.onsubmit=function(t){{return!0}},await(async e=>e+e)("3ca3d93c"));e.elements.namedItem("solution").value=n,e.requestSubmit()}},{{once:!0}});</script></head><body><form hidden method="GET" action="{action}"><input type="hidden" name="solution" /><input type="hidden" name="js_challenge" value="1"/><input type="hidden" name="jsc_token" value="2824be"/><input type="hidden" name="jsc_orig_r" value=""/></form></body></html>"#
	)
}
impl Fixture {
	async fn new() -> Self {
		let wires = Arc::new(Mutex::new(Vec::new()));
		let mode = Arc::new(AtomicUsize::new(0));
		let shared = (wires.clone(), mode.clone());
		let service = make_service_fn(move |_| {
			let shared = shared.clone();
			async move {
				Ok::<_, Infallible>(service_fn(move |req: HyperRequest<Body>| {
					let (wires, mode) = shared.clone();
					async move {
						let path = req.uri().path().to_string();
						let query: HashMap<_, _> = url::form_urlencoded::parse(req.uri().query().unwrap_or_default().as_bytes()).into_owned().collect();
						let answered = query.get("solution").is_some_and(|s| s == "3ca3d93c3ca3d93c")
							&& query.get("jsc_token").is_some_and(|s| s == "2824be")
							&& query.get("js_challenge").is_some_and(|s| s == "1")
							&& query.get("jsc_orig_r").is_some_and(String::is_empty)
							&& req.headers().get("cookie").is_some_and(|v| v.to_str().unwrap().contains("anon=fixture"))
							&& !req.headers().contains_key("authorization");
						if query.contains_key("solution") {
							assert!(answered, "only the recognized solution and the anonymous session cookie may be submitted");
							if path.contains("/comments/") {
								assert_eq!(query.get("sort").map(String::as_str), Some("new"));
								assert_eq!(query.get("context").map(String::as_str), Some("2"));
							} else {
								assert_eq!(query.get("t").map(String::as_str), Some("all"));
								assert_eq!(query.get("limit").map(String::as_str), Some("1"));
							}
						}
						let challenge_mode = mode.load(Ordering::SeqCst);
						let rdt = req.uri().query().unwrap_or_default().contains("rdt=");
						let after = req.uri().query().unwrap_or_default().contains("after=");
						let method = req.method().to_string();
						let headers = req.headers().clone();
						let body = hyper::body::to_bytes(req.into_body()).await.unwrap();
						wires
							.lock()
							.unwrap()
							.push((path.clone(), method.clone(), headers, String::from_utf8(body.to_vec()).unwrap()));
						let status = if mode.load(Ordering::SeqCst) == 1 || (challenge_mode == 10 && answered) {
							429
						} else if challenge_mode == 13 && !answered {
							403
						} else if mode.load(Ordering::SeqCst) == 6 && !rdt {
							302
						} else {
							200
						};
						let body = if (8..=14).contains(&challenge_mode) && (!answered || matches!(challenge_mode, 9 | 11 | 12)) {
							let action = if challenge_mode == 12 { "https://evil.test/r/rust/hot/" } else { &path };
							let check = script_check(action);
							if challenge_mode == 11 {
								format!("<shreddit-content-gate reason=\"private\"></shreddit-content-gate>{check}")
							} else {
								check
							}
						} else if mode.load(Ordering::SeqCst) == 7 {
							let id = if after { "oldtwo" } else { "oldone" };
							let next = if after {
								String::new()
							} else {
								r#"<span class="next-button"><a href="https://old.reddit.com/r/all/new/?after=t3_oldone">Next</a></span>"#.into()
							};
							format!(
								r#"<div id="siteTable"><div class="thing link" data-fullname="t3_{id}" data-author="alice" data-subreddit="rust" data-permalink="https://old.reddit.com/r/rust/comments/{id}/title/" data-url="https://example.com/story" data-score="4" data-timestamp="1794991382000"><a class="title">Old feed</a></div></div>{next}"#
							)
						} else if mode.load(Ordering::SeqCst) == 4 {
							if path.starts_with("/svc/shreddit/community-more-posts/") {
								post("rust", "fragment", 5, false, false)
							} else {
								r#"<main><faceplate-partial src="/svc/shreddit/community-more-posts/hot/?name=t5_abc&amp;t=all"></faceplate-partial></main>"#.into()
							}
						} else if mode.load(Ordering::SeqCst) == 2 {
							"x".repeat(MAX_HTML + 1)
						} else if path.starts_with("/svc/shreddit/more-comments/") {
							comment("second")
						} else if path.contains("/comments/") {
							let p = post("rust", "abc", 20, false, false).replace("comment-count=\"0\"", "comment-count=\"3\"");
							let repeated = wires
								.lock()
								.unwrap()
								.iter()
								.filter(|(path, method, _, _)| method == "GET" && path.starts_with("/r/rust/comments/"))
								.count()
								> if matches!(challenge_mode, 8 | 13) { 2 } else { 1 };
							if repeated {
								format!("<main>{p}</main>")
							} else {
								format!(
									r#"<main>{p}{}<faceplate-partial src="/svc/shreddit/more-comments/rust/t3_abc?top-level=true&amp;comments-remaining=2&amp;startingDepth=0&amp;commentPositionOffset=1&amp;commentParentPositions=[]" method="post"><input name="cursor" value="opaque+cursor"></faceplate-partial></main>"#,
									comment("first")
								)
							}
						} else {
							let name = path.split('/').nth(2).unwrap_or("popular");
							let number = name.strip_prefix('s').and_then(|n| n.parse::<usize>().ok()).unwrap_or(0);
							let cards = if mode.load(Ordering::SeqCst) == 3 {
								format!(
									"{}{}{}",
									post(name, "oldpin", 500, true, true),
									post(name, "newpin", 200, true, false),
									post(name, "normal", 10, false, false)
								)
							} else {
								format!(
									"{}{}",
									post(name, &format!("x{number}a"), number + 1, false, false),
									post(name, &format!("x{number}b"), number, false, false)
								)
							};
							format!("<main><shreddit-feed>{cards}</shreddit-feed></main>")
						};
						let response = HyperResponse::builder()
							.status(status)
							.header("content-type", "text/html")
							.header("set-cookie", "anon=fixture; Path=/; HttpOnly");
						let response = if status == 429 {
							response.header("retry-after", "60")
						} else if challenge_mode == 14 {
							response.header("x-ratelimit-remaining", "0").header("x-ratelimit-reset", "60")
						} else if status == 302 {
							response.header("location", "/r/rust/hot/?rdt=12345")
						} else {
							response
						};
						Ok::<_, Infallible>(response.body(Body::from(body)).unwrap())
					}
				}))
			}
		});
		let server = hyper::Server::bind(&([127, 0, 0, 1], 0).into()).serve(service);
		let origin = format!("http://{}", server.local_addr());
		let transport = Arc::new(Transport {
			client: Arc::new(
				WreqClient::builder()
					.no_proxy()
					.redirect(Policy::none())
					.cookie_provider(Arc::new(Jar::default()))
					.build()
					.unwrap(),
			),
			health: Mutex::new(Health::default()),
		});
		Self {
			origin,
			transport,
			server: tokio::spawn(server),
			wires,
			mode,
		}
	}
	fn context(&self) -> Context {
		Context {
			lane: RedditLane::Direct,
			quarantine: false,
			budget: Arc::new(Budget::new()),
			fixture: Some(self.transport.clone()),
			origin: Some(self.origin.clone()),
		}
	}
}

#[test]
#[sealed_test(env=[("REDLIB_READ_BACKEND","api"),("REDLIB_TOR_FALLBACK","on"),("REDLIB_TOR_PROXY","socks5h://127.0.0.1:1"),("REDLIB_REDDIT_PRIMARY_LANE","tor")])]
fn test_reddit_web_default_keeps_tor_primary_without_backend_flag() {
	std::env::remove_var("REDLIB_READ_BACKEND");
	assert!(enabled(), "the web reader must be selected without another operator flag");
	tokio::runtime::Runtime::new().unwrap().block_on(async {
		let fixture = Fixture::new().await;
		*MOCK.lock().unwrap() = Some((fixture.transport.clone(), fixture.origin.clone()));
		assert_eq!(configured_primary_lane(), RedditLane::Tor);
		start_oauth();
		start_tor_fallback();
		assert!(oauth_client(RedditLane::Direct).is_none());
		assert!(oauth_client(RedditLane::Tor).is_none());
		let page = super::super::json("/r/rust/hot.json?limit=1".into(), false).await.unwrap();
		let cursor = page["data"]["after"].as_str().unwrap().strip_prefix(PREFIX).unwrap();
		assert!(
			cached::Cached::cache_get(&mut *CURSORS.lock().unwrap(), cursor).unwrap().tor,
			"navigation must retain the existing Tor primary"
		);
		let calls = fixture.wires.lock().unwrap().len();
		assert_eq!(calls, 1);
		std::env::set_var("REDLIB_READ_BACKEND", "api");
		assert!(!enabled());
		assert!(super::super::json("/r/rust/new.json?limit=1".into(), false).await.is_err());
		assert_eq!(fixture.wires.lock().unwrap().len(), calls, "the explicit API override must not call the web backend");
	});
}

#[test]
#[sealed_test(env=[("REDLIB_READ_BACKEND","web"),("REDLIB_TOR_FALLBACK","off"),("REDLIB_REDDIT_PRIMARY_LANE","direct")])]
fn test_reddit_web_transport_uncached_reads_are_independent_of_api_and_keep_anonymous_session() {
	tokio::runtime::Runtime::new().unwrap().block_on(async {
		let fixture = Fixture::new().await;
		*MOCK.lock().unwrap() = Some((fixture.transport.clone(), fixture.origin.clone()));
		assert!(oauth_client(RedditLane::Direct).is_none());
		start_oauth();
		start_tor_fallback();
		assert!(oauth_client(RedditLane::Direct).is_none());
		upstream_guard(RedditLane::Direct).graphql_rate_limit_blocked_until = Some(Instant::now() + Duration::from_secs(60));
		let feed = super::super::json("/r/rust/hot.json?limit=1".into(), false).await.unwrap();
		assert_eq!(feed["data"]["children"].as_array().unwrap().len(), 1);
		let old = feed["data"]["after"].as_str().unwrap();
		cached::Cached::cache_remove(&mut *CURSORS.lock().unwrap(), old.strip_prefix(PREFIX).unwrap());
		let renewed = super::super::json("/r/rust/hot.json?limit=1".into(), false).await.unwrap();
		let fresh = renewed["data"]["after"].as_str().unwrap();
		assert_ne!(old, fresh, "a cached first page must not retain an evicted NEXT token");
		assert!(Spec::parse(&format!("/r/rust/hot.json?limit=1&after={fresh}"), false).is_ok());
		let first = super::super::json("/r/rust/comments/abc/title.json".into(), false).await.unwrap();
		assert_eq!(first[1]["data"]["children"][0]["data"]["name"], "t1_first");
		let cursor = first[1]["data"]["after"].as_str().unwrap();
		let mut serializer = url::form_urlencoded::Serializer::new(String::new());
		serializer.append_pair("after", cursor);
		let next = super::super::json(format!("/r/rust/comments/abc/title.json?{}", serializer.finish()), false).await.unwrap();
		assert_eq!(next[1]["data"]["children"][0]["data"]["name"], "t1_second");
		assert!(next[1]["data"]["after"].is_null());
		fixture.mode.store(4, Ordering::SeqCst);
		let fragment = super::super::json("/r/rust/hot.json?limit=2".into(), false).await.unwrap();
		assert_eq!(fragment["data"]["children"][0]["data"]["name"], "t3_fragment");
		assert!(fragment["data"]["after"].is_null(), "bootstrap fragment must not be advertised again as NEXT");
		let before = fixture.wires.lock().unwrap().len();
		assert!(super::super::json("/user/alice/about.json".into(), false).await.is_err());
		assert_eq!(fixture.wires.lock().unwrap().len(), before);
		let wires = fixture.wires.lock().unwrap();
		assert!(wires.iter().all(|(path, _, _, _)| !path.contains("by_id") && !path.contains("auth/") && path != "/"));
		assert!(wires.iter().all(|(_, _, headers, _)| !headers.contains_key("authorization")
			&& !headers.contains_key("x-reddit-device-id")
			&& !headers.contains_key("x-reddit-loid")
			&& !headers.contains_key("client-vendor-id")));
		assert!(wires[0].2.get("cookie").is_none());
		assert!(wires.iter().skip(1).any(|(_, _, headers, _)| headers.get("cookie").is_some_and(|v| v == "anon=fixture")));
		let form = wires.iter().find(|(_, method, _, _)| method == "POST").unwrap();
		assert!(form.0.starts_with("/svc/shreddit/more-comments/rust/t3_abc"));
		assert_eq!(form.3, "cursor=opaque%2Bcursor");
		assert_eq!(form.2["content-type"], "application/x-www-form-urlencoded");
		assert_eq!(
			wires.iter().filter(|(_, method, _, _)| method == "POST").count(),
			1,
			"NEXT fetches post metadata without replaying the previous comment expansion"
		);
	});
}

#[test]
#[sealed_test(env=[("REDLIB_READ_BACKEND","web"),("REDLIB_TOR_FALLBACK","off"),("REDLIB_REDDIT_PRIMARY_LANE","direct")])]
fn test_reddit_web_transport_fourteen_sources_resume_without_skips_and_preserve_pins_and_order() {
	tokio::runtime::Runtime::new().unwrap().block_on(async {
		let fixture = Fixture::new().await;
		let names = (0..14).map(|i| format!("s{i}")).collect::<Vec<_>>().join("%2B");
		let base = format!("/r/{names}/hot.json?limit=1");
		let mut path = base.clone();
		let mut seen = HashSet::new();
		let mut communities = HashSet::new();
		for index in 0..28 {
			let spec = Spec::parse(&path, false).unwrap();
			let page = read_on_lane(fixture.context(), spec).await.unwrap();
			let items = page["data"]["children"].as_array().unwrap();
			assert_eq!(items.len(), 1);
			let data = &items[0]["data"];
			assert!(
				seen.insert(data["name"].as_str().unwrap().to_string()),
				"no skipped or duplicate cards across a partial source page"
			);
			communities.insert(data["subreddit"].as_str().unwrap().to_string());
			if index == 13 {
				assert_eq!(communities.len(), 14, "every available subscription gets one turn before a second turn");
			}
			match page["data"]["after"].as_str() {
				Some(cursor) => {
					assert!(index < 27);
					path = format!("{base}&after={cursor}");
				}
				None => assert_eq!(index, 27),
			}
		}
		fixture.mode.store(3, Ordering::SeqCst);
		for (name, expected) in [("rust", 3), ("all", 2), ("popular", 2)] {
			let spec = Spec::parse(&format!("/r/{name}/hot.json"), false).unwrap();
			let page = read_on_lane(fixture.context(), spec).await.unwrap();
			assert_eq!(page["data"]["children"].as_array().unwrap().len(), expected);
		}
		fixture.mode.store(7, Ordering::SeqCst);
		let first = read_on_lane(fixture.context(), Spec::parse("/r/all/new.json?limit=1", false).unwrap()).await.unwrap();
		assert_eq!(first["data"]["children"][0]["data"]["name"], "t3_oldone");
		let after = first["data"]["after"].as_str().unwrap();
		let next = read_on_lane(fixture.context(), Spec::parse(&format!("/r/all/new.json?limit=1&after={after}"), false).unwrap())
			.await
			.unwrap();
		assert_eq!(next["data"]["children"][0]["data"]["name"], "t3_oldtwo");
		assert!(next["data"]["after"].is_null());
		fixture.mode.store(0, Ordering::SeqCst);
		let page = read_on_lane(fixture.context(), Spec::parse("/r/s1%2Bs9/top.json?limit=2", false).unwrap()).await.unwrap();
		assert_eq!(page["data"]["children"][0]["data"]["subreddit"], "s9");
		assert_eq!(
			page["data"]["children"][1]["data"]["subreddit"], "s9",
			"top globally ranks rather than applying hot-feed fairness"
		);
	});
}

#[test]
#[sealed_test(env=[("REDLIB_READ_BACKEND","web"),("REDLIB_TOR_FALLBACK","off"),("REDLIB_REDDIT_PRIMARY_LANE","direct")])]
fn test_reddit_web_transport_quota_stops_queued_sends_and_streaming_is_bounded() {
	tokio::runtime::Runtime::new().unwrap().block_on(async {
		let fixture = Fixture::new().await;
		fixture.mode.store(1, Ordering::SeqCst);
		let ctx = fixture.context();
		let spec = Spec::parse("/r/rust/hot.json", false).unwrap();
		let mut tasks = tokio::task::JoinSet::new();
		for _ in 0..12 {
			let ctx = ctx.clone();
			let spec = spec.clone();
			tasks.spawn(async move { ctx.fetch(&spec, "/r/rust/hot/", Some("rust"), None).await });
		}
		while let Some(result) = tasks.join_next().await {
			assert!(matches!(result.unwrap(), Err(WebError::RateLimited(_))));
		}
		let calls = fixture.wires.lock().unwrap().len();
		assert!((1..=4).contains(&calls));
		assert!(matches!(ctx.fetch(&spec, "/r/rust/hot/", Some("rust"), None).await, Err(WebError::RateLimited(_))));
		assert_eq!(fixture.wires.lock().unwrap().len(), calls);
		let fixture = Fixture::new().await;
		fixture.mode.store(2, Ordering::SeqCst);
		assert!(matches!(
			fixture.context().fetch(&spec, "/r/rust/hot/", Some("rust"), None).await,
			Err(WebError::Temporary("body_bound"))
		));
		let redirect = Fixture::new().await;
		redirect.mode.store(6, Ordering::SeqCst);
		redirect.context().fetch(&spec, "/r/rust/hot/", Some("rust"), None).await.unwrap();
		assert_eq!(
			redirect.wires.lock().unwrap().len(),
			2,
			"same-scope anonymous rdt redirect remains bounded and follows the new cookie"
		);
		let budget = Budget::new();
		*budget.quota.lock().unwrap() = Some(Duration::from_secs(60));
		assert!(budget.finish(Ok(json!({"ok":true}))).is_ok());
		assert!(matches!(budget.finish(Err(WebError::Temporary("transport"))), Err(WebError::RateLimited(_))));
	});
}

#[test]
#[sealed_test(env=[("REDLIB_READ_BACKEND","web"),("REDLIB_TOR_FALLBACK","on"),("REDLIB_TOR_PROXY","socks5h://127.0.0.1:1"),("REDLIB_REDDIT_PRIMARY_LANE","tor")])]
fn test_reddit_web_transport_script_check_retains_tor_cookie_and_request_scope() {
	tokio::runtime::Runtime::new().unwrap().block_on(async {
		for mode in [8, 13] {
			let fixture = Fixture::new().await;
			fixture.mode.store(mode, Ordering::SeqCst);
			*MOCK.lock().unwrap() = Some((fixture.transport.clone(), fixture.origin.clone()));
			let page = read("/r/rust/hot.json?limit=1", false).await.unwrap();
			let token = page["data"]["after"].as_str().unwrap().strip_prefix(PREFIX).unwrap();
			let saved = cached::Cached::cache_get(&mut *CURSORS.lock().unwrap(), token).unwrap().clone();
			assert!(saved.tor);
			assert!(saved.sources.iter().all(|s| !s.page.as_deref().unwrap_or_default().contains("jsc_token")));
			assert_eq!(
				fixture.wires.lock().unwrap().len(),
				2,
				"the initial Tor read must complete its script check without a Direct replay"
			);
			assert!(oauth_client(RedditLane::Direct).is_none());
			assert!(oauth_client(RedditLane::Tor).is_none());
		}
		let fixture = Fixture::new().await;
		fixture.mode.store(8, Ordering::SeqCst);
		let spec = Spec::parse("/r/rust/comments/abc.json?sort=new&context=2", false).unwrap();
		let doc = document(&fixture.context(), &spec, &spec.initial(Some("rust")), Some("rust"), true).await.unwrap();
		assert_eq!(doc.posts[0]["data"]["name"], "t3_abc");
		assert!(!doc.comments.is_empty());
		assert_eq!(fixture.wires.lock().unwrap().len(), 2);
	});
}

#[test]
#[sealed_test(env=[("REDLIB_READ_BACKEND","web"),("REDLIB_TOR_FALLBACK","on"),("REDLIB_TOR_PROXY","socks5h://127.0.0.1:1"),("REDLIB_REDDIT_PRIMARY_LANE","tor")])]
fn test_reddit_web_transport_script_check_is_bounded_and_respects_resources_and_quota() {
	tokio::runtime::Runtime::new().unwrap().block_on(async {
		let spec = Spec::parse("/r/rust/hot.json?limit=1", false).unwrap();
		for (mode, wires, resource, quota) in [
			(9, 2, false, false),
			(10, 2, false, true),
			(11, 1, true, false),
			(12, 1, false, false),
			(14, 1, false, true),
		] {
			let fixture = Fixture::new().await;
			fixture.mode.store(mode, Ordering::SeqCst);
			let ctx = fixture.context();
			let error = document(&ctx, &spec, &spec.initial(Some("rust")), Some("rust"), true).await.unwrap_err();
			assert_eq!(matches!(error, WebError::Resource(_)), resource, "mode {mode}: {error:?}");
			assert_eq!(matches!(error, WebError::RateLimited(_)), quota, "mode {mode}: {error:?}");
			assert_eq!(fixture.wires.lock().unwrap().len(), wires, "mode {mode}: {error:?}");
		}
		let fixture = Fixture::new().await;
		fixture.mode.store(9, Ordering::SeqCst);
		*MOCK.lock().unwrap() = Some((fixture.transport.clone(), fixture.origin.clone()));
		let _busy_standby = STANDBY.acquire_many(2).await.unwrap();
		assert_eq!(
			read("/r/rust/hot.json?limit=1", false).await,
			Err(WebError::Temporary("script_check_repeated")),
			"busy standby must preserve the upstream failure instead of replacing it with capacity"
		);
		assert_eq!(fixture.wires.lock().unwrap().len(), 2);
	});
}

#[test]
fn test_reddit_web_script_check_rejects_other_forms_scripts_and_destinations() {
	let page = script_check("/r/rust/hot/");
	let check = html::script_check(&page, &["rust".into()]).unwrap().unwrap();
	assert_eq!(check.fields[0], ("solution".into(), "3ca3d93c3ca3d93c".into()));
	for altered in [
		page.replace("async e=>e+e", "async e=>fetch(e)"),
		page.replace("3ca3d93c", "bad;code"),
		page.replace("method=\"GET\"", "method=\"POST\""),
		page.replace("name=\"js_challenge\" value=\"1\"", "name=\"js_challenge\" value=\"2\""),
		page.replace("name=\"jsc_token\"", "name=\"password\""),
		page.replace("</form>", "<input type=\"hidden\" name=\"jsc_token\" value=\"duplicate\"></form>"),
		page.replace("</form>", "<input type=\"hidden\" name=\"next\" value=\"https://evil.test\"></form>"),
		page.replace("3ca3d93c", &"a".repeat(257)),
		format!("<shreddit-post><div slot=\"text-body\">{page}</div></shreddit-post>"),
	] {
		assert!(html::script_check(&altered, &["rust".into()]).unwrap().is_none());
	}
	let spec = Spec::parse("/r/rust/hot.json?limit=1&t=day", false).unwrap();
	assert_eq!(
		spec.script_action("/r/rust/hot/?limit=1&t=day", &check.action, Some("rust")).unwrap(),
		"/r/rust/hot/?limit=1&t=day"
	);
	for action in [
		"https://evil.test/r/rust/hot/",
		"https://old.reddit.com/r/rust/hot/",
		"//evil.test/r/rust/hot/",
		"/r/linux/hot/",
		"/r/rust/new/",
		"/r/rust/hot/?t=all",
		"/r/rust/hot/?jsc_token=untrusted",
		"/r/rust/hot/#fragment",
	] {
		assert!(spec.script_action("/r/rust/hot/?limit=1&t=day", action, Some("rust")).is_err(), "{action}");
	}
	assert_eq!(
		html::script_check(&format!("<shreddit-content-gate reason=\"private\"></shreddit-content-gate>{page}"), &["rust".into()]).err(),
		Some(WebError::Resource("private"))
	);
}

#[test]
fn test_reddit_web_cursor_scope_terminal_priority_and_destination_allowlist() {
	assert_eq!(Spec::parse("/r/all/hot.json", false).unwrap().origin(), "https://old.reddit.com");
	let all = Spec::parse("/r/all/hot.json", false).unwrap();
	assert!(all.validate_page("https://old.reddit.com/r/all/hot/?after=t3_abc", Some("all")).is_ok());
	assert!(all.validate_page("https://www.reddit.com/r/all/hot/", Some("all")).is_err());
	assert!(all.validate_page("https://old.reddit.com/r/popular/hot/", Some("all")).is_err());
	assert_eq!(Spec::parse("/r/rust/hot.json", false).unwrap().origin(), "https://www.reddit.com");
	assert!(!contains_cursor("/search.json?q=redlib-web:example"));
	assert!(contains_cursor("/r/rust/hot.json?after=redlib-web:example"));
	let spec = Spec::parse("/r/rust/hot.json?limit=1", false).unwrap();
	let source = Source {
		page: Some("/r/rust/hot/?after=opaque".into()),
		last: Some("t3_abc".into()),
		done: false,
		served: 1,
		form: None,
	};
	let cursor = encode_cursor(&spec.scope, vec![source], RedditLane::Direct).unwrap().unwrap();
	let path = format!("/r/rust/hot.json?limit=1&after={cursor}");
	assert!(Spec::parse(&path, false).is_ok());
	assert!(matches!(Spec::parse(&path, true), Err(WebError::Cursor)));
	assert!(Spec::parse(&path.replace("rust", "linux"), false).is_err());
	for page in [
		"https://evil.test/r/rust/hot/",
		"//evil.test/r/rust/hot/",
		"https://www.reddit.com:444/r/rust/hot/",
		"/r/linux/hot/",
		"/svc/shreddit/unknown/rust",
		"/r/rust/hot/?next=https://evil.test",
	] {
		assert!(spec.validate_page(page, Some("rust")).is_err(), "reject destination {page}");
	}
	assert!(spec.validate_page("/svc/shreddit/community-more-posts/hot/?name=rust&after=opaque", Some("rust")).is_ok());
	assert!(spec.validate_page("/svc/shreddit/community-more-posts/hot/?name=linux", Some("rust")).is_err());
	assert!(spec.validate_page("/svc/shreddit/community-more-posts/hot/?name=rust&name=linux", Some("rust")).is_err());
	assert!(spec.validate_page("/svc/shreddit/community-more-posts/hot/?name=t5_abc", Some("all")).is_err());
	let thread = Spec::parse("/r/rust/comments/abc/_/one.json?context=2", false).unwrap();
	assert!(thread.initial(Some("rust")).contains("context=2"));
	let thread_cursor = encode_cursor(
		&thread.scope,
		vec![Source {
			page: Some(thread.initial(Some("rust"))),
			last: None,
			done: false,
			served: 0,
			form: None,
		}],
		RedditLane::Direct,
	)
	.unwrap()
	.unwrap();
	assert!(Spec::parse(&format!("/r/rust/comments/abc/_/one.json?context=3&after={thread_cursor}"), false).is_err());
	let budget = Budget::new();
	*budget.terminal.lock().unwrap() = Some(WebError::Resource("gated"));
	assert_eq!(budget.finish(Err(WebError::Temporary("timeout"))), Err(WebError::Resource("gated")));
	assert_eq!(WebError::Resource("quarantined").message(), "quarantined");
	let mut health = Health::default();
	health.pause(Duration::from_secs(60));
	let prior = health.until.unwrap();
	health.pause(Duration::from_secs(1));
	assert_eq!(health.until, Some(prior), "a later short response must not shorten an observed pause");
}

#[test]
#[sealed_test(env=[("REDLIB_READ_BACKEND","web"),("REDLIB_TOR_FALLBACK","off"),("REDLIB_REDDIT_PRIMARY_LANE","direct")])]
fn test_reddit_web_production_browser_jars_are_isolated_without_oauth_identity() {
	tokio::runtime::Runtime::new().unwrap().block_on(async {
		let fixture = Fixture::new().await;
		let spec = Spec::parse("/r/rust/hot.json", false).unwrap();
		let context = |quarantine| Context {
			lane: RedditLane::Direct,
			quarantine,
			budget: Arc::new(Budget::new()),
			fixture: None,
			origin: Some(fixture.origin.clone()),
		};
		for quarantine in [false, false, true] {
			context(quarantine).fetch(&spec, "/r/rust/hot/", Some("rust"), None).await.unwrap();
		}
		let wires = fixture.wires.lock().unwrap();
		assert_eq!(wires.len(), 3);
		assert!(wires[0].2.get("cookie").is_none());
		assert_eq!(wires[1].2["cookie"], "anon=fixture");
		assert!(wires[2].2.get("cookie").is_none(), "an opted-in read must have a distinct anonymous jar");
		for (_, _, headers, _) in wires.iter() {
			let ua = headers["user-agent"].to_str().unwrap();
			assert!(ua.contains("Chrome/"));
			assert!(!ua.starts_with("Reddit/"));
			assert!(!headers.contains_key("authorization"));
			assert!(!headers.contains_key("x-reddit-device-id"));
		}
	});
}
