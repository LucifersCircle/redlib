use super::*;
use hyper::service::{make_service_fn, service_fn};
use sealed_test::prelude::*;
use std::convert::Infallible;

#[tokio::test]
#[ignore = "Manual signed-out public HTTPS verification; honors the environment proxy and CA trust"]
async fn live_anonymous_web_reader_capture() {
	let _ = pretty_env_logger::try_init();
	std::fs::create_dir_all("/tmp/redlib-live-capture").unwrap();
	let feed = env::var("REDLIB_LIVE_FEED").unwrap_or_else(|_| "rust".into());
	let limit = env::var("REDLIB_LIVE_LIMIT").unwrap_or_else(|_| "25".into());
	assert!(limit.parse::<u32>().is_ok_and(|n| (1..=100).contains(&n)));
	assert!(matches!(feed.as_str(), "rust" | "all" | "popular" | "AskReddit" | "news" | "technology"));
	let mut builder = WreqClient::builder()
		.emulation(Emulation::builder().profile(Profile::Chrome145).platform(Platform::Windows).headers(true).build())
		.redirect(Policy::none())
		.cookie_provider(Arc::new(Jar::default()))
		.tls_cert_store(wreq::tls::trust::CertStore::builder().set_default_paths().build().unwrap());
	if let Ok(proxy) = env::var("HTTPS_PROXY").or_else(|_| env::var("https_proxy")) {
		builder = builder.proxy(Proxy::all(proxy).unwrap());
	}
	let transport = Arc::new(Transport {
		client: Arc::new(builder.build().unwrap()),
		health: Mutex::new(Health::default()),
	});
	let ctx = Context {
		lane: RedditLane::Direct,
		quarantine: false,
		budget: Arc::new(Budget::new()),
		session: transport,
		origin: None,
	};
	let spec = Spec::parse(&format!("/r/{feed}/hot.json?limit={limit}"), false).unwrap();
	let response = tokio::time::timeout(Duration::from_secs(40), ctx.fetch(&spec, &spec.initial(Some(&feed)), Some(&feed), None))
		.await
		.unwrap();
	if feed == "all" && matches!(response, Err(WebError::Resource("login_required"))) {
		println!("Real All result: login required; no anonymous feed was loaded");
		return;
	}
	let text = response.unwrap();
	std::fs::write("/tmp/redlib-live-capture/wreq-page.html", &text).unwrap();
	let names = if matches!(feed.as_str(), "all" | "popular") { vec![] } else { vec![feed.clone()] };
	let doc = html::parse(&text, None, &names, false).unwrap();
	println!("Real HTML: posts={} comments={} fragments={}", doc.posts.len(), doc.comments.len(), doc.fragments.len());
	for path in doc.fragments.iter().chain(doc.next.iter()) {
		let url = url::Url::parse(spec.origin()).unwrap().join(path).unwrap();
		println!(
			"Fragment: path={} query_keys={:?} accepted={}",
			url.path(),
			url.query_pairs().map(|(k, _)| k.into_owned()).collect::<Vec<_>>(),
			spec.validate_page(path, Some(&feed)).is_ok()
		);
		spec
			.validate_page(path, Some(&feed))
			.expect("every continuation from the real page must survive validation");
	}
	assert!(!doc.posts.is_empty(), "a shell is not a successful real feed load");
	let first = read_on_lane(ctx.clone(), spec).await.expect("the full real feed pipeline must succeed");
	assert!(!first["data"]["children"].as_array().unwrap().is_empty());
	println!("Real feed pipeline: {} posts", first["data"]["children"].as_array().unwrap().len());
	if let Some(after) = first["data"]["after"].as_str() {
		let next = Spec::parse(&format!("/r/{feed}/hot.json?limit={limit}&after={after}"), false).unwrap();
		let next = read_on_lane(ctx.clone(), next).await.expect("the real Next pipeline must succeed");
		assert!(!next["data"]["children"].as_array().unwrap().is_empty());
		println!("Real Next pipeline: {} posts", next["data"]["children"].as_array().unwrap().len());
	}
	let permalink = first["data"]["children"][0]["data"]["permalink"].as_str().unwrap();
	let post = Spec::parse(&format!("{}.json", permalink.trim_end_matches('/')), false).unwrap();
	let post = read_on_lane(ctx.clone(), post).await.expect("the real post/comment pipeline must succeed");
	assert!(!post[0]["data"]["children"].as_array().unwrap().is_empty());
	println!("Real thread pipeline: {} root comments", post[1]["data"]["children"].as_array().unwrap().len());
	// Exercise the real HTTP router, JSON conversion and Askama rendering too.
	*MOCK.lock().unwrap() = Some((ctx.session.clone(), String::new()));
	let mut app = crate::server::Server::new();
	app.at("/").get(|r| Box::pin(crate::subreddit::community(r)));
	app.at("/r/:sub/:sort").get(|r| Box::pin(crate::subreddit::community(r)));
	app.at("/r/:sub/comments/:id/:title").get(|r| Box::pin(crate::post::item(r)));
	let server = tokio::spawn(app.listen("127.0.0.1:18081"));
	let http = hyper::Client::new();
	for path in ["/".to_string(), format!("/r/{feed}/hot?limit={limit}"), permalink.to_string()] {
		let url = format!("http://127.0.0.1:18081{path}").parse().unwrap();
		let response = http.get(url).await.unwrap();
		let status = response.status();
		let body = hyper::body::to_bytes(response.into_body()).await.unwrap();
		std::fs::write("/tmp/redlib-live-capture/rendered-response.html", &body).unwrap();
		assert_eq!(status, hyper::StatusCode::OK, "rendered HTTP route {path}");
		assert!(body.len() > 1000);
		let page = scraper::Html::parse_document(std::str::from_utf8(&body).unwrap());
		let titles = scraper::Selector::parse(".post_title").unwrap();
		assert!(page.select(&titles).next().is_some(), "HTTP 200 must contain rendered post content");
		println!("Real rendered HTTP route: status=200 bytes={}", body.len());
	}
	server.abort();
}

#[tokio::test]
#[ignore = "Manual real subscription homepage verification"]
async fn live_subscription_homepage_capture() {
	let _ = pretty_env_logger::try_init();
	let names =
		env::var("REDLIB_LIVE_SUBSCRIPTIONS").unwrap_or_else(|_| "rust+programming+technology+AskReddit+news+worldnews+science+aww+pics+funny+linux+apple+Android+Games".into());
	assert!(names.split('+').count() <= 16 && names.split('+').all(valid_name));
	let mut builder = WreqClient::builder()
		.emulation(Emulation::builder().profile(Profile::Chrome145).platform(Platform::Windows).headers(true).build())
		.redirect(Policy::none())
		.cookie_provider(Arc::new(Jar::default()))
		.tls_cert_store(wreq::tls::trust::CertStore::builder().set_default_paths().build().unwrap());
	if let Ok(proxy) = env::var("HTTPS_PROXY").or_else(|_| env::var("https_proxy")) {
		builder = builder.proxy(Proxy::all(proxy).unwrap());
	}
	let transport = Arc::new(Transport {
		client: Arc::new(builder.build().unwrap()),
		health: Mutex::new(Health::default()),
	});
	*MOCK.lock().unwrap() = Some((transport, String::new()));
	let mut app = crate::server::Server::new();
	app.at("/").get(|r| Box::pin(crate::subreddit::community(r)));
	let server = tokio::spawn(app.listen("127.0.0.1:18082"));
	let http = hyper::Client::new();
	let request = hyper::Request::builder()
		.uri("http://127.0.0.1:18082/")
		.header("cookie", format!("subscriptions={names}; post_sort=hot; front_page=default"))
		.body(Body::empty())
		.unwrap();
	let response = http.request(request).await.unwrap();
	let status = response.status();
	let body = hyper::body::to_bytes(response.into_body()).await.unwrap();
	let dir = std::path::Path::new("/tmp/redlib-home-live");
	std::fs::create_dir_all(dir).unwrap();
	std::fs::write(dir.join("homepage.html"), &body).unwrap();
	println!("Subscription homepage: communities={} status={} bytes={}", names.split('+').count(), status, body.len());
	assert_eq!(status, hyper::StatusCode::OK);
	let doc = scraper::Html::parse_document(std::str::from_utf8(&body).unwrap());
	let selector = scraper::Selector::parse(".post_title").unwrap();
	assert!(doc.select(&selector).count() > 0);
	let communities = scraper::Selector::parse(".post_subreddit").unwrap();
	let shown: std::collections::HashSet<_> = doc
		.select(&communities)
		.filter_map(|el| el.value().attr("href"))
		.filter_map(|href| href.strip_prefix("/r/"))
		.map(str::to_ascii_lowercase)
		.collect();
	println!("Subscription homepage: represented_communities={}", shown.len());
	for name in names.split('+') {
		assert!(shown.contains(&name.to_ascii_lowercase()), "the first page must include {name}");
	}
	let next_selector = scraper::Selector::parse("a[accesskey='N']").unwrap();
	let next = doc
		.select(&next_selector)
		.next()
		.and_then(|el| el.value().attr("href"))
		.expect("subscription homepage must issue Next");
	let request = hyper::Request::builder()
		.uri(format!("http://127.0.0.1:18082/{next}"))
		.header("cookie", format!("subscriptions={names}; post_sort=hot; front_page=default"))
		.body(Body::empty())
		.unwrap();
	let response = http.request(request).await.unwrap();
	assert_eq!(response.status(), hyper::StatusCode::OK, "homepage Next must load with the same preferences");
	let body = hyper::body::to_bytes(response.into_body()).await.unwrap();
	let doc = scraper::Html::parse_document(std::str::from_utf8(&body).unwrap());
	assert!(doc.select(&selector).count() > 0);
	println!("Subscription homepage Next: status=200 posts={}", doc.select(&selector).count());
	server.abort();
}

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
						if matches!(challenge_mode, 24 | 25) && path.starts_with("/svc/shreddit/community-more-posts/") {
							assert_eq!(
								query.get("navigationSessionId").map(String::as_str),
								Some(if challenge_mode == 25 {
									"12345678-1234-4234-8234-123456789abc"
								} else {
									"00000000-0000-4000-8000-000000000001"
								})
							);
						}
						let rdt = req.uri().query().unwrap_or_default().contains("rdt=");
						let after = req.uri().query().unwrap_or_default().contains("after=");
						let method = req.method().to_string();
						let headers = req.headers().clone();
						let body = hyper::body::to_bytes(req.into_body()).await.unwrap();
						wires
							.lock()
							.unwrap()
							.push((path.clone(), method.clone(), headers, String::from_utf8(body.to_vec()).unwrap()));
						let status = if mode.load(Ordering::SeqCst) == 1 || (challenge_mode == 10 && answered) || (challenge_mode == 17 && wires.lock().unwrap().len() == 1) {
							429
						} else if challenge_mode == 18 {
							302
						} else if challenge_mode == 13 && !answered {
							403
						} else if mode.load(Ordering::SeqCst) == 6 && !rdt {
							302
						} else {
							200
						};
						let name = path.split('/').nth(2).unwrap_or("rust");
						let canonical_name = if name.eq_ignore_ascii_case("askreddit") {
							"AskReddit"
						} else if name.eq_ignore_ascii_case("android") {
							"Android"
						} else {
							name
						};
						let body = if (19..=23).contains(&challenge_mode) && (name != canonical_name || challenge_mode != 19) {
							let target = if challenge_mode == 20 {
								"elsewhere"
							} else if challenge_mode == 21 && name == canonical_name {
								"askreddit"
							} else {
								canonical_name
							};
							let gate = if challenge_mode == 22 {
								"<shreddit-content-gate reason=\"private\"></shreddit-content-gate>"
							} else {
								""
							};
							format!(r#"{gate}<shreddit-app><div id="canonical-url-updater" value="https://www.reddit.com/r/{target}/hot/"></div></shreddit-app>"#)
						} else if challenge_mode == 19 {
							format!("<shreddit-feed>{}</shreddit-feed>", post(name, &format!("p{}", name.to_ascii_lowercase()), 1, false, false))
						} else if (8..=14).contains(&challenge_mode) && (!answered || matches!(challenge_mode, 9 | 11 | 12)) {
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
							if challenge_mode == 25 {
								assert_eq!(body.as_ref(), b"cursor=fresh%2Bcursor", "renewal must discover a new form token");
							}
							comment("second")
						} else if matches!(challenge_mode, 24 | 25) && path.starts_with("/svc/shreddit/community-more-posts/") {
							format!("{}{}", post("rust", "x0c", 2, false, false), post("rust", "x0d", 1, false, false))
						} else if challenge_mode == 16 && path.starts_with("/svc/shreddit/community-more-posts/") {
							post("rust", "x0c", 2, false, false)
						} else if challenge_mode == 15 && path.starts_with("/svc/shreddit/comments/") {
							comment("fragment")
						} else if path.contains("/comments/") {
							let p = post("rust", "abc", 20, false, false).replace("comment-count=\"0\"", "comment-count=\"3\"");
							let repeated = wires
								.lock()
								.unwrap()
								.iter()
								.filter(|(path, method, _, _)| method == "GET" && path.starts_with("/r/rust/comments/"))
								.count()
								> if matches!(challenge_mode, 8 | 13) { 2 } else { 1 };
							if challenge_mode == 15 {
								format!(r#"<main>{p}<faceplate-partial src="/svc/shreddit/comments/r/rust/t3_abc?sort=BEST"></faceplate-partial></main>"#)
							} else if repeated {
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
							let next = if challenge_mode == 16 {
								r#"<faceplate-partial src="/svc/shreddit/community-more-posts/hot/?t=ALL&amp;limit=1&amp;after=t3_x0b&amp;name=rust&amp;adDistance=1&amp;ad_posts_served=1&amp;navigationSessionId=12345678-1234-4234-8234-123456789abc&amp;feedLength=4"></faceplate-partial>"#
							} else {
								""
							};
							format!("<main><shreddit-feed>{cards}{next}</shreddit-feed></main>")
						};
						let body = if matches!(challenge_mode, 24 | 25) {
							let body = body.replace("opaque+cursor", if challenge_mode == 25 { "fresh+cursor" } else { "opaque+cursor" });
							if !path.contains("/comments/") && !path.starts_with("/svc/") {
								format!(
									"{body}<faceplate-partial src=\"/svc/shreddit/community-more-posts/hot/?after=t3_x0b&amp;name=rust&amp;navigationSessionId={}\"></faceplate-partial>",
									if challenge_mode == 25 {
										"12345678-1234-4234-8234-123456789abc"
									} else {
										"00000000-0000-4000-8000-000000000001"
									}
								)
							} else {
								body
							}
						} else if matches!(challenge_mode, 28 | 29) {
							let fixture = include_str!("../tests/fixtures/reddit_web_media.html");
							if challenge_mode == 29 {
								fixture.replace("?width=140&amp;height=93&amp;auto=webp&amp;s=fixture", "?s=fixture")
							} else {
								fixture.to_string()
							}
						} else if matches!(challenge_mode, 26 | 27) {
							let body = body.replace("<div slot=\"text-body\"><p>Readable body</p></div>", "<shreddit-post-text-body slot=\"text-body\"><a slot=\"text-body\" href=\"/r/rust/comments/x0a/title/\"><p>Readable <strong>body</strong></p></a></shreddit-post-text-body>")
								.replace("</shreddit-post>", "<shreddit-player src=\"https://v.redd.it/clip/HLSPlaylist.m3u8\" preview=\"https://v.redd.it/clip/CMAF_96.mp4\"></shreddit-player></shreddit-post>");
							if challenge_mode == 27 {
								body.replace(" preview=\"https://v.redd.it/clip/CMAF_96.mp4\"", "")
							} else {
								body
							}
						} else {
							body
						};
						let response = HyperResponse::builder()
							.status(status)
							.header("content-type", "text/html")
							.header("set-cookie", "anon=fixture; Path=/; HttpOnly");
						let response = if status == 429 {
							response.header("retry-after", "60")
						} else if matches!(challenge_mode, 14 | 23) {
							response.header("x-ratelimit-remaining", "0").header("x-ratelimit-reset", "60")
						} else if challenge_mode == 18 {
							response.header("location", "https://old.reddit.com/login/?reason=lor2&dest=%2Fr%2Fall%2Fhot%2F")
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
			session: self.transport.clone(),
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
#[sealed_test(env=[("REDLIB_READ_BACKEND","web"),("REDLIB_TOR_FALLBACK","off"),("REDLIB_REDDIT_PRIMARY_LANE","direct")])]
fn test_reddit_web_transport_all_login_redirect_is_terminal_without_retry_or_identity_rotation() {
	tokio::runtime::Runtime::new().unwrap().block_on(async {
		let fixture = Fixture::new().await;
		fixture.mode.store(18, Ordering::SeqCst);
		let spec = Spec::parse("/r/all/hot.json", false).unwrap();
		let error = document(&fixture.context(), &spec, &spec.initial(Some("all")), Some("all"), true).await.unwrap_err();
		assert_eq!(error, WebError::Resource("login_required"));
		assert!(!error.retry_lane());
		assert_eq!(fixture.wires.lock().unwrap().len(), 1);
	});
}

#[test]
#[sealed_test(env=[("REDLIB_READ_BACKEND","web"),("REDLIB_TOR_FALLBACK","off"),("REDLIB_REDDIT_PRIMARY_LANE","direct")])]
fn test_reddit_web_transport_public_json_preserves_full_post_permalink() {
	tokio::runtime::Runtime::new().unwrap().block_on(async {
		let fixture = Fixture::new().await;
		*MOCK.lock().unwrap() = Some((fixture.transport.clone(), fixture.origin.clone()));
		let result = super::super::json("/r/RuSt/comments/abc/Actual_Title/.json".into(), false).await.unwrap();
		assert_eq!(result[0]["data"]["children"][0]["data"]["name"], "t3_abc");
		assert_eq!(fixture.wires.lock().unwrap()[0].0, "/r/RuSt/comments/abc/Actual_Title/");
	});
}

#[test]
#[sealed_test(env=[("REDLIB_READ_BACKEND","web"),("REDLIB_TOR_FALLBACK","off"),("REDLIB_REDDIT_PRIMARY_LANE","direct")])]
fn test_reddit_web_transport_default_comments_follow_server_best_fragment() {
	tokio::runtime::Runtime::new().unwrap().block_on(async {
		let fixture = Fixture::new().await;
		fixture.mode.store(15, Ordering::SeqCst);
		let spec = Spec::parse("/r/rust/comments/abc.json", false).unwrap();
		let doc = document(&fixture.context(), &spec, &spec.initial(Some("rust")), Some("rust"), true).await.unwrap();
		assert_eq!(doc.posts[0]["data"]["name"], "t3_abc");
		assert_eq!(doc.comments[0]["data"]["name"], "t1_fragment");
		assert_eq!(fixture.wires.lock().unwrap().len(), 2);
		for sort in ["new", "top", "controversial", "old"] {
			let spec = Spec::parse(&format!("/r/rust/comments/abc.json?sort={sort}"), false).unwrap();
			assert!(spec.validate_page("/svc/shreddit/comments/r/rust/t3_abc?sort=BEST", Some("rust")).is_err());
		}
	});
}

#[test]
#[sealed_test(env=[("REDLIB_READ_BACKEND","web"),("REDLIB_TOR_FALLBACK","on"),("REDLIB_REDDIT_PRIMARY_LANE","tor")])]
fn test_reddit_web_transport_production_tor_renewal_changes_socks_identity_and_cookie_jar() {
	use tokio::io::{AsyncReadExt, AsyncWriteExt};
	tokio::runtime::Runtime::new().unwrap().block_on(async {
		let fixture = Fixture::new().await;
		fixture.mode.store(17, Ordering::SeqCst);
		let target: std::net::SocketAddr = fixture.origin.strip_prefix("http://").unwrap().parse().unwrap();
		let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
		std::env::set_var("REDLIB_TOR_PROXY", format!("socks5h://{}", listener.local_addr().unwrap()));
		let identities = Arc::new(Mutex::new(Vec::new()));
		let recorded = identities.clone();
		let relay = tokio::spawn(async move {
			loop {
				let (mut socket, _) = listener.accept().await.unwrap();
				let recorded = recorded.clone();
				tokio::spawn(async move {
					assert_eq!(socket.read_u8().await.unwrap(), 5);
					let n = socket.read_u8().await.unwrap();
					let mut methods = vec![0; n as usize];
					socket.read_exact(&mut methods).await.unwrap();
					assert!(methods.contains(&2));
					socket.write_all(&[5, 2]).await.unwrap();
					assert_eq!(socket.read_u8().await.unwrap(), 1);
					let n = socket.read_u8().await.unwrap();
					let mut user = vec![0; n as usize];
					socket.read_exact(&mut user).await.unwrap();
					let n = socket.read_u8().await.unwrap();
					let mut password = vec![0; n as usize];
					socket.read_exact(&mut password).await.unwrap();
					assert_eq!(user, password);
					recorded.lock().unwrap().push(user);
					socket.write_all(&[1, 0]).await.unwrap();
					let mut request = [0; 4];
					socket.read_exact(&mut request).await.unwrap();
					assert_eq!(&request[..3], &[5, 1, 0]);
					match request[3] {
						1 => {
							let mut ip = [0; 4];
							socket.read_exact(&mut ip).await.unwrap();
							assert_eq!(ip, [127, 0, 0, 1]);
						}
						3 => {
							let n = socket.read_u8().await.unwrap();
							let mut host = vec![0; n as usize];
							socket.read_exact(&mut host).await.unwrap();
							assert_eq!(host, b"127.0.0.1");
						}
						other => panic!("unexpected test destination type {other}"),
					}
					assert_eq!(socket.read_u16().await.unwrap(), target.port());
					let mut upstream = tokio::net::TcpStream::connect(target).await.unwrap();
					socket.write_all(&[5, 0, 0, 1, 127, 0, 0, 1, 0, 0]).await.unwrap();
					let _ = tokio::io::copy_bidirectional(&mut socket, &mut upstream).await;
				});
			}
		});
		let spec = Spec::parse("/r/rust/hot.json", false).unwrap();
		let old = active_transport(RedditLane::Tor, false).unwrap();
		let ctx = Context {
			lane: RedditLane::Tor,
			quarantine: false,
			budget: Arc::new(Budget::new()),
			session: old.clone(),
			origin: Some(fixture.origin.clone()),
		};
		assert!(matches!(ctx.fetch(&spec, "/r/rust/hot/", Some("rust"), None).await, Err(WebError::RateLimited(_))));
		let fresh = renew_tor_transport(&old, false).unwrap();
		assert!(!Arc::ptr_eq(&fresh, &old));
		let fresh = Context {
			session: fresh,
			budget: Arc::new(ctx.budget.new_attempt()),
			..ctx
		};
		let doc = document(&fresh, &spec, "/r/rust/hot/", Some("rust"), true).await.unwrap();
		assert!(!doc.posts.is_empty());
		let identities = identities.lock().unwrap();
		assert_eq!(identities.len(), 2);
		assert_ne!(identities[0], identities[1]);
		let wires = fixture.wires.lock().unwrap();
		assert_eq!(wires.len(), 2);
		assert!(wires[1].2.get("cookie").is_none());
		relay.abort();
	});
}

#[test]
#[sealed_test(env=[("REDLIB_READ_BACKEND","web"),("REDLIB_TOR_FALLBACK","on"),("REDLIB_TOR_PROXY","socks5h://127.0.0.1:1"),("REDLIB_REDDIT_PRIMARY_LANE","tor")])]
fn test_reddit_web_transport_tor_quota_renews_one_session_for_concurrent_reads() {
	tokio::runtime::Runtime::new().unwrap().block_on(async {
		let old = Fixture::new().await;
		old.mode.store(1, Ordering::SeqCst);
		let fresh = Fixture::new().await;
		*MOCK.lock().unwrap() = Some((old.transport.clone(), old.origin.clone()));
		*RENEW_MOCK.lock().unwrap() = Some((fresh.transport.clone(), fresh.origin.clone()));
		let mut tasks = tokio::task::JoinSet::new();
		for _ in 0..8 {
			tasks.spawn(async { read("/r/rust/hot.json?limit=1", false).await });
		}
		while let Some(page) = tasks.join_next().await {
			let page = page.unwrap().unwrap();
			let token = page["data"]["after"].as_str().unwrap().strip_prefix(PREFIX).unwrap();
			let saved = cached::Cached::cache_get(&mut *CURSORS.lock().unwrap(), token).unwrap().clone();
			assert!(saved.tor);
			assert!(Arc::ptr_eq(saved.session.as_ref().unwrap(), &fresh.transport));
		}
		assert_eq!(TEST_RENEWALS.load(Ordering::SeqCst), 1, "all failures of the old identity must share one replacement");
		assert!((1..=4).contains(&old.wires.lock().unwrap().len()));
		let sent = fresh.wires.lock().unwrap();
		assert_eq!(sent.len(), 8);
		assert!(
			sent[0].2.get("cookie").is_none(),
			"the fresh session must not reuse the exhausted identity's anonymous cookie"
		);
		assert!(sent.iter().all(|(_, _, headers, _)| !headers.contains_key("authorization")));
		assert!(old.transport.health.lock().unwrap().until.is_some());
		assert!(fresh.transport.health.lock().unwrap().until.is_none());
	});
}

#[test]
#[sealed_test(env=[("REDLIB_READ_BACKEND","web"),("REDLIB_TOR_FALLBACK","on"),("REDLIB_TOR_PROXY","socks5h://127.0.0.1:1"),("REDLIB_REDDIT_PRIMARY_LANE","tor")])]
fn test_reddit_web_transport_cached_root_discards_cursor_of_exhausted_tor_identity() {
	tokio::runtime::Runtime::new().unwrap().block_on(async {
		let old = Fixture::new().await;
		let fresh = Fixture::new().await;
		*MOCK.lock().unwrap() = Some((old.transport.clone(), old.origin.clone()));
		*RENEW_MOCK.lock().unwrap() = Some((fresh.transport.clone(), fresh.origin.clone()));
		let _ = json("/r/rust/hot.json?limit=1".into(), false).await.unwrap();
		old.mode.store(1, Ordering::SeqCst);
		read("/r/rust/new.json?limit=1", false).await.unwrap();
		let page = json("/r/rust/hot.json?limit=1".into(), false).await.unwrap();
		let token = page["data"]["after"].as_str().unwrap().strip_prefix(PREFIX).unwrap();
		let cursor = cached::Cached::cache_get(&mut *CURSORS.lock().unwrap(), token).unwrap().clone();
		assert!(Arc::ptr_eq(cursor.session.as_ref().unwrap(), &fresh.transport));
		assert_eq!(
			fresh.wires.lock().unwrap().len(),
			2,
			"the cached root must reload instead of issuing another exhausted cursor"
		);
	});
}

#[test]
#[sealed_test(env=[("REDLIB_READ_BACKEND","web"),("REDLIB_TOR_FALLBACK","on"),("REDLIB_TOR_PROXY","socks5h://127.0.0.1:1"),("REDLIB_REDDIT_PRIMARY_LANE","tor")])]
fn test_reddit_web_transport_tor_renewal_resumes_without_replaying_old_cursor() {
	tokio::runtime::Runtime::new().unwrap().block_on(async {
		let old = Fixture::new().await;
		let fresh = Fixture::new().await;
		*MOCK.lock().unwrap() = Some((old.transport.clone(), old.origin.clone()));
		*RENEW_MOCK.lock().unwrap() = Some((fresh.transport.clone(), fresh.origin.clone()));
		let page = read("/r/rust/hot.json?limit=1", false).await.unwrap();
		let cursor = page["data"]["after"].as_str().unwrap();
		old.mode.store(1, Ordering::SeqCst);
		let resumed = read(&format!("/r/rust/hot.json?limit=1&after={cursor}"), false).await.unwrap();
		assert_eq!(resumed["data"]["children"][0]["data"]["name"], "t3_x0b");
		assert_eq!(fresh.wires.lock().unwrap().len(), 1, "resume must re-discover the public anchor through the new jar");
		fresh.mode.store(1, Ordering::SeqCst);
		let another = Fixture::new().await;
		another.mode.store(1, Ordering::SeqCst);
		*RENEW_MOCK.lock().unwrap() = Some((another.transport.clone(), another.origin.clone()));
		assert!(matches!(read("/r/rust/hot.json?limit=1", false).await, Err(WebError::RateLimited(_))));
		assert_eq!(another.wires.lock().unwrap().len(), 1, "a logical read may try only one replacement identity");
		let budget = Budget::new();
		budget.wires.store(MAX_WIRES, Ordering::SeqCst);
		let next = budget.new_attempt();
		assert_eq!(next.wires.load(Ordering::SeqCst), MAX_WIRES, "renewal must retain the full read's request and byte bounds");
	});
}

#[test]
#[sealed_test(env=[("REDLIB_READ_BACKEND","web"),("REDLIB_TOR_FALLBACK","on"),("REDLIB_TOR_PROXY","socks5h://127.0.0.1:1"),("REDLIB_REDDIT_PRIMARY_LANE","tor")])]
fn test_reddit_web_transport_renewal_rediscovers_feed_and_comment_continuations() {
	tokio::runtime::Runtime::new().unwrap().block_on(async {
		for thread in [false, true] {
			let old = Fixture::new().await;
			old.mode.store(24, Ordering::SeqCst);
			let fresh = Fixture::new().await;
			fresh.mode.store(25, Ordering::SeqCst);
			*MOCK.lock().unwrap() = Some((old.transport.clone(), old.origin.clone()));
			*RENEW_MOCK.lock().unwrap() = Some((fresh.transport.clone(), fresh.origin.clone()));
			let path = if thread {
				"/r/rust/comments/abc/title.json?sort=new"
			} else {
				"/r/rust/hot.json?limit=2"
			};
			let first = read(path, false).await.unwrap();
			let cursor = if thread { first[1]["data"]["after"].as_str() } else { first["data"]["after"].as_str() }.unwrap();
			old.mode.store(1, Ordering::SeqCst);
			let next = read(&format!("{path}&after={cursor}"), false).await.unwrap();
			if thread {
				assert_eq!(next[1]["data"]["children"][0]["data"]["name"], "t1_second");
				assert!(next[1]["data"]["after"].is_null());
			} else {
				assert_eq!(next["data"]["children"][0]["data"]["name"], "t3_x0c");
				assert_eq!(next["data"]["children"][1]["data"]["name"], "t3_x0d");
			}
			assert_eq!(fresh.wires.lock().unwrap().len(), 2, "fresh root plus freshly discovered continuation");
		}
	});
}

#[test]
#[sealed_test(env=[("REDLIB_READ_BACKEND","web"),("REDLIB_TOR_FALLBACK","off"),("REDLIB_REDDIT_PRIMARY_LANE","direct")])]
fn test_reddit_web_transport_subscription_anchors_survive_repeated_session_recovery() {
	tokio::runtime::Runtime::new().unwrap().block_on(async {
		let fixture = Fixture::new().await;
		let names = (0..14).map(|i| format!("s{i}")).collect::<Vec<_>>().join("%2B");
		let base = format!("/r/{names}/hot.json?limit=1");
		let mut path = base.clone();
		let mut seen = HashSet::new();
		for index in 0..28 {
			let mut spec = Spec::parse(&path, false).unwrap();
			spec.recover = spec.cursor.is_some();
			let page = read_on_lane(fixture.context(), spec).await.unwrap();
			assert!(seen.insert(page["data"]["children"][0]["data"]["name"].as_str().unwrap().to_string()));
			match page["data"]["after"].as_str() {
				Some(cursor) => path = format!("{base}&after={cursor}"),
				None => assert_eq!(index, 27),
			}
		}
	});
}

#[test]
#[sealed_test(env=[("REDLIB_READ_BACKEND","web"),("REDLIB_TOR_FALLBACK","off"),("REDLIB_REDDIT_PRIMARY_LANE","direct")])]
fn test_reddit_web_transport_expired_home_navigation_recovers_and_media_renders() {
	tokio::runtime::Runtime::new().unwrap().block_on(async {
		let fixture = Fixture::new().await;
		fixture.mode.store(26, Ordering::SeqCst);
		*MOCK.lock().unwrap() = Some((fixture.transport.clone(), fixture.origin.clone()));
		let mut app = crate::server::Server::new();
		app.at("/").get(|r| Box::pin(crate::subreddit::community(r)));
		let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
		let address = listener.local_addr().unwrap();
		drop(listener);
		let server = tokio::spawn(app.listen(&address.to_string()));
		let http = hyper::Client::new();
		let cookie = "subscriptions=rust; front_page=default; layout=card; theme=black; post_sort=hot";
		let request = hyper::Request::builder()
			.uri(format!("http://{address}/?after=redlib-web%3A{}&t=all", "0".repeat(32)))
			.header("cookie", cookie)
			.body(Body::empty())
			.unwrap();
		let response = http.request(request).await.unwrap();
		assert_eq!(response.status(), hyper::StatusCode::FOUND);
		assert_eq!(response.headers()["location"], "/?t=all");
		assert!(fixture.wires.lock().unwrap().is_empty(), "expired tokens should recover before any upstream request");
		let request = hyper::Request::builder()
			.uri(format!("http://{address}/?t=all"))
			.header("cookie", cookie)
			.body(Body::empty())
			.unwrap();
		let response = http.request(request).await.unwrap();
		assert_eq!(response.status(), hyper::StatusCode::OK);
		let body = hyper::body::to_bytes(response.into_body()).await.unwrap();
		let page = scraper::Html::parse_document(std::str::from_utf8(&body).unwrap());
		let video = scraper::Selector::parse("video.post_media_video").unwrap();
		assert_eq!(page.select(&video).count(), 2);
		assert!(page
			.select(&video)
			.all(|el| el.value().attr("src").is_some_and(|src| src.starts_with("/vid/clip/cmaf/96.mp4"))));
		let text = scraper::Selector::parse(".md").unwrap();
		assert!(page.select(&text).any(|el| el.text().collect::<String>().contains("Readable body")));
		assert!(
			!page.select(&text).next().unwrap().inner_html().contains("href="),
			"preview text must not inherit link accent styling"
		);
		std::fs::write("/tmp/redlib-browsing-rendered.html", &body).unwrap();
		fixture.mode.store(27, Ordering::SeqCst);
		let request = hyper::Request::builder()
			.uri(format!("http://{address}/?t=week"))
			.header("cookie", cookie)
			.body(Body::empty())
			.unwrap();
		let response = http.request(request).await.unwrap();
		assert_eq!(response.status(), hyper::StatusCode::OK);
		let body = hyper::body::to_bytes(response.into_body()).await.unwrap();
		let page = scraper::Html::parse_document(std::str::from_utf8(&body).unwrap());
		let source = scraper::Selector::parse("video source").unwrap();
		assert_eq!(page.select(&source).count(), 2);
		assert!(page.select(&source).all(|el| el.value().attr("type") == Some("application/vnd.apple.mpegurl")));
		assert!(page
			.select(&source)
			.all(|el| el.value().attr("src").is_some_and(|src| src.starts_with("/hls/clip/HLSPlaylist.m3u8"))));
		std::fs::write("/tmp/redlib-browsing-hls-rendered.html", &body).unwrap();
		server.abort();
	});
}

#[test]
#[sealed_test(env=[("REDLIB_READ_BACKEND","web"),("REDLIB_TOR_FALLBACK","off"),("REDLIB_REDDIT_PRIMARY_LANE","direct")])]
fn test_reddit_web_transport_recorded_feed_continuation_shape_loads_next_without_skips() {
	tokio::runtime::Runtime::new().unwrap().block_on(async {
		let fixture = Fixture::new().await;
		fixture.mode.store(16, Ordering::SeqCst);
		*MOCK.lock().unwrap() = Some((fixture.transport.clone(), fixture.origin.clone()));
		let mut path = "/r/rust/hot.json?limit=1".to_string();
		let mut ids = vec![];
		for _ in 0..3 {
			let page = read(&path, false).await.unwrap();
			ids.push(page["data"]["children"][0]["data"]["name"].as_str().unwrap().to_string());
			if let Some(after) = page["data"]["after"].as_str() {
				path = format!("/r/rust/hot.json?limit=1&after={after}");
			}
		}
		assert_eq!(ids, ["t3_x0a", "t3_x0b", "t3_x0c"]);
		let spec = Spec::parse("/r/rust/hot.json", false).unwrap();
		for query in ["feedLength=-1", "ad_posts_served=1000001", "navigationSessionId=not-a-uuid", "t=DAY", "name=elsewhere"] {
			assert!(spec
				.validate_page(&format!("/svc/shreddit/community-more-posts/hot/?name=rust&{query}"), Some("rust"))
				.is_err());
		}
	});
}

#[test]
#[sealed_test(env=[("REDLIB_READ_BACKEND","web"),("REDLIB_TOR_FALLBACK","off"),("REDLIB_REDDIT_PRIMARY_LANE","direct")])]
fn test_reddit_web_transport_gallery_and_article_render_feed_and_post() {
	tokio::runtime::Runtime::new().unwrap().block_on(async {
		let fixture = Fixture::new().await;
		fixture.mode.store(28, Ordering::SeqCst);
		*MOCK.lock().unwrap() = Some((fixture.transport.clone(), fixture.origin.clone()));
		let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
		let address = listener.local_addr().unwrap();
		drop(listener);
		let mut app = crate::server::Server::new();
		app.at("/").get(|r| Box::pin(crate::subreddit::community(r)));
		app.at("/r/:sub/comments/:id/:title").get(|r| Box::pin(crate::post::item(r)));
		let server = tokio::spawn(app.listen(&address.to_string()));
		let http = hyper::Client::new();
		for (path, mode, file) in [
			("/", "feed", "/tmp/redlib-gallery-feed.html"),
			("/r/rust/comments/abc/title", "detail", "/tmp/redlib-gallery-detail.html"),
			("/?t=week", "feed", "/tmp/redlib-gallery-feed-unknown.html"),
		] {
			fixture.mode.store(if path.contains("week") { 29 } else { 28 }, Ordering::SeqCst);
			let request = hyper::Request::builder()
				.uri(format!("http://{address}{path}"))
				.header("cookie", "subscriptions=rust; front_page=default; layout=card; theme=black; post_sort=hot")
				.body(Body::empty())
				.unwrap();
			let response = http.request(request).await.unwrap();
			assert_eq!(response.status(), hyper::StatusCode::OK);
			let body = hyper::body::to_bytes(response.into_body()).await.unwrap();
			let page = scraper::Html::parse_document(std::str::from_utf8(&body).unwrap());
			let gallery = scraper::Selector::parse(&format!("[data-gallery-mode='{mode}']")).unwrap();
			assert_eq!(page.select(&gallery).count(), 1);
			let slide = scraper::Selector::parse("[data-gallery-slide]").unwrap();
			assert_eq!(page.select(&slide).count(), 2);
			let originals = scraper::Selector::parse("a[data-gallery-original]").unwrap();
			assert_eq!(
				page.select(&originals).map(|a| a.value().attr("href").unwrap()).collect::<Vec<_>>(),
				["/img/one.jpeg", "/img/two.png"]
			);
			let lazy = scraper::Selector::parse("[data-gallery-slide] img[data-src]").unwrap();
			assert_eq!(page.select(&lazy).count(), 1, "later gallery items must stay lazy");
			if mode == "feed" {
				let thumb = scraper::Selector::parse(".external_thumbnail img").unwrap();
				let image = page.select(&thumb).next().unwrap();
				assert!(image.value().attr("src").unwrap().starts_with("/preview/external-pre/"));
				assert!(!std::str::from_utf8(&body).unwrap().contains("max-width:0px"));
			}
			std::fs::write(file, &body).unwrap();
		}
		assert!(fixture
			.wires
			.lock()
			.unwrap()
			.iter()
			.all(|(path, _, headers, _)| path.starts_with("/r/rust/") && !headers.contains_key("authorization")));
		server.abort();
	});
}

#[test]
#[sealed_test(env=[("REDLIB_READ_BACKEND","web"),("REDLIB_TOR_FALLBACK","off"),("REDLIB_REDDIT_PRIMARY_LANE","direct")])]
fn test_reddit_web_subscription_home_preserves_and_recovers_canonical_names() {
	tokio::runtime::Runtime::new().unwrap().block_on(async {
		let fixture = Fixture::new().await;
		fixture.mode.store(19, Ordering::SeqCst);
		*MOCK.lock().unwrap() = Some((fixture.transport.clone(), fixture.origin.clone()));
		let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
		let address = listener.local_addr().unwrap();
		drop(listener);
		let mut app = crate::server::Server::new();
		app.at("/").get(|r| Box::pin(crate::subreddit::community(r)));
		let server = tokio::spawn(app.listen(&address.to_string()));
		let http = hyper::Client::new();
		for names in ["AskReddit+Android+rust", "askreddit+android+rust"] {
			let request = hyper::Request::builder()
				.uri(format!("http://{address}/"))
				.header("cookie", format!("subscriptions={names}; post_sort=hot; front_page=default"))
				.body(Body::empty())
				.unwrap();
			let response = http.request(request).await.unwrap();
			assert_eq!(response.status(), hyper::StatusCode::OK);
			let body = hyper::body::to_bytes(response.into_body()).await.unwrap();
			let body = std::str::from_utf8(&body).unwrap();
			for id in ["paskreddit", "pandroid", "prust"] {
				assert!(body.contains(&format!("id=\"{id}\"")), "homepage must retain every subscribed community");
			}
		}
		let wires = fixture.wires.lock().unwrap();
		assert_eq!(wires.iter().filter(|(path, _, _, _)| path == "/r/AskReddit/hot/").count(), 2);
		assert!(wires.iter().any(|(path, _, _, _)| path == "/r/askreddit/hot/"));
		server.abort();
	});
}

#[test]
#[sealed_test(env=[("REDLIB_READ_BACKEND","web"),("REDLIB_TOR_FALLBACK","off"),("REDLIB_REDDIT_PRIMARY_LANE","direct")])]
fn test_reddit_web_subscription_home_next_empty_range_retains_cursor_scope() {
	tokio::runtime::Runtime::new().unwrap().block_on(async {
		let fixture = Fixture::new().await;
		fixture.mode.store(16, Ordering::SeqCst);
		*MOCK.lock().unwrap() = Some((fixture.transport.clone(), fixture.origin.clone()));
		let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
		let address = listener.local_addr().unwrap();
		drop(listener);
		let mut app = crate::server::Server::new();
		app.at("/").get(|r| Box::pin(crate::subreddit::community(r)));
		let server = tokio::spawn(app.listen(&address.to_string()));
		let http = hyper::Client::new();
		let make = |path: &str| {
			hyper::Request::builder()
				.uri(format!("http://{address}/{path}"))
				.header("cookie", "subscriptions=rust; post_sort=hot; front_page=default")
				.body(Body::empty())
				.unwrap()
		};
		let first = http.request(make("")).await.unwrap();
		assert_eq!(first.status(), hyper::StatusCode::OK);
		let body = hyper::body::to_bytes(first.into_body()).await.unwrap();
		let doc = scraper::Html::parse_document(std::str::from_utf8(&body).unwrap());
		let next = scraper::Selector::parse("a[accesskey='N']").unwrap();
		let next = doc.select(&next).next().unwrap().value().attr("href").unwrap();
		assert!(next.contains("t=&"));
		let response = http.request(make(next)).await.unwrap();
		assert_eq!(response.status(), hyper::StatusCode::OK);
		let body = hyper::body::to_bytes(response.into_body()).await.unwrap();
		assert!(std::str::from_utf8(&body).unwrap().contains("id=\"x0c\""));
		server.abort();
	});
}

#[test]
#[sealed_test(env=[("REDLIB_READ_BACKEND","web"),("REDLIB_TOR_FALLBACK","off"),("REDLIB_REDDIT_PRIMARY_LANE","direct")])]
fn test_reddit_web_canonical_case_recovery_is_scoped_bounded_and_respects_limits() {
	tokio::runtime::Runtime::new().unwrap().block_on(async {
		let spec = Spec::parse("/r/askreddit/hot.json", false).unwrap();
		for (mode, expected, wires) in [
			(20, WebError::Temporary("canonical_scope"), 1),
			(21, WebError::Temporary("canonical_loop"), 2),
			(22, WebError::Resource("private"), 1),
			(23, WebError::RateLimited(Duration::from_secs(60)), 1),
		] {
			let fixture = Fixture::new().await;
			fixture.mode.store(mode, Ordering::SeqCst);
			let error = document(&fixture.context(), &spec, &spec.initial(Some("askreddit")), Some("askreddit"), true)
				.await
				.unwrap_err();
			if mode == 23 {
				assert!(matches!(error, WebError::RateLimited(delay) if delay <= Duration::from_secs(60) && delay > Duration::from_secs(59)));
			} else {
				assert_eq!(error, expected);
			}
			assert_eq!(fixture.wires.lock().unwrap().len(), wires);
		}
		for target in [
			"https://evil.test/r/AskReddit/hot/",
			"https://old.reddit.com/r/AskReddit/hot/",
			"/r/elsewhere/hot/",
			"/r/AskReddit/new/",
			"/r/AskReddit/hot/?after=different",
			"/r/AskReddit/hot/?next=evil",
		] {
			assert!(spec.canonical_case_page("/r/askreddit/hot/?t=all&limit=25", target, Some("askreddit")).is_err(), "{target}");
		}
		assert_eq!(
			spec
				.canonical_case_page("/r/askreddit/hot/?t=all&limit=25", "/r/AskReddit/hot/", Some("askreddit"))
				.unwrap()
				.unwrap(),
			"/r/AskReddit/hot/?t=all&limit=25"
		);
	});
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
		resume: None,
	};
	let cursor = encode_cursor(&spec.scope, vec![source], RedditLane::Direct, None).unwrap().unwrap();
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
			resume: None,
		}],
		RedditLane::Direct,
		None,
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
			session: active_transport(RedditLane::Direct, quarantine).unwrap(),
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
