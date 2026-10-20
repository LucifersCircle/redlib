//! Read-only adapter for Reddit's anonymous website comment tree. The website
//! still serves this tree when the app-only /comments JSON route rejects it.
use crate::reddit_lane::{RedditLane, TOR_FALLBACK_CONFIG};
use hyper::body::HttpBody;
use scraper::{node::Node, ElementRef, Html, Selector};
use serde_json::{json, Value};
use std::{
	collections::HashMap,
	sync::{
		atomic::{AtomicBool, Ordering},
		LazyLock, Mutex,
	},
	time::{Duration, Instant},
};
use tokio::sync::{OnceCell, Semaphore};
use wreq::EmulationFactory;

const MAX_BODY: usize = 8 * 1024 * 1024;
const MAX_COMMENTS: usize = 1000;
const MAX_DEPTH: usize = 64;
const WAIT: &str = "Reddit is temporarily rejecting this instance. Retry in 5 seconds";

#[derive(Debug, Clone)]
pub(crate) struct ThreadPath {
	pub(crate) id: String,
	pub(crate) website_path: String,
}

impl ThreadPath {
	pub(crate) fn parse(path: &str) -> Option<Self> {
		let (base, query) = path.split_once('?').unwrap_or((path, ""));
		let base = base.trim_end_matches('/').strip_suffix(".json").unwrap_or(base.trim_end_matches('/'));
		let parts: Vec<_> = base.trim_start_matches('/').split('/').collect();
		let (prefix, id, tail) = match parts.as_slice() {
			["r", sub, "comments", id, tail @ ..] if valid_sub(sub) => (format!("/r/{sub}/comments"), *id, tail),
			["comments", id, tail @ ..] => ("/comments".to_string(), *id, tail),
			_ => return None,
		};
		if !valid_id(id) || tail.len() > 2 || !path.starts_with('/') || path.starts_with("//") {
			return None;
		}
		let slug = tail
			.first()
			.copied()
			.filter(|slug| slug.len() <= 512 && slug.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-')))
			.unwrap_or("_");
		let mut website_path = format!("{prefix}/{id}/{slug}/");
		if let [_, comment] = tail {
			if !valid_id(comment) {
				return None;
			}
			website_path = format!("{prefix}/{id}/comment/{comment}/");
		}
		let mut params = url::form_urlencoded::Serializer::new(String::new());
		for (key, value) in url::form_urlencoded::parse(query.as_bytes()) {
			if matches!(key.as_ref(), "context" | "depth" | "limit") && value.parse::<u16>().is_ok() {
				params.append_pair(&key, &value);
			} else if key == "sort" && matches!(value.as_ref(), "confidence" | "best" | "top" | "new" | "old" | "controversial" | "qa") {
				params.append_pair("sort", if value == "confidence" { "best" } else { &value });
			}
		}
		let query = params.finish();
		if !query.is_empty() {
			website_path.push('?');
			website_path.push_str(&query);
		}
		Some(Self { id: id.to_string(), website_path })
	}
}

fn valid_id(id: &str) -> bool {
	!id.is_empty() && id.len() <= 16 && id.bytes().all(|b| b.is_ascii_alphanumeric())
}
fn valid_sub(sub: &str) -> bool {
	!sub.is_empty() && sub.len() <= 32 && sub.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
}

struct Session {
	client: wreq::Client,
	capacity: Semaphore,
	cooldown: Mutex<Option<Instant>>,
	ready_logged: AtomicBool,
	bootstrap: OnceCell<()>,
}

static DIRECT: LazyLock<Result<Session, String>> = LazyLock::new(|| Session::new(RedditLane::Direct));
static TOR: LazyLock<Result<Session, String>> = LazyLock::new(|| Session::new(RedditLane::Tor));

impl Session {
	fn new(lane: RedditLane) -> Result<Self, String> {
		let emulation = wreq_util::EmulationOption::builder()
			.emulation(wreq_util::Emulation::Chrome145)
			.emulation_os(wreq_util::EmulationOS::Windows)
			.build()
			.emulation();
		let mut builder = wreq::Client::builder().emulation(emulation).cookie_store(true).redirect(wreq::redirect::Policy::none());
		if lane == RedditLane::Tor {
			let config = TOR_FALLBACK_CONFIG.as_ref().map_err(Clone::clone)?.as_ref().ok_or_else(|| WAIT.to_string())?;
			let mut proxy = url::Url::parse(&config.proxy_url).map_err(|_| WAIT.to_string())?;
			let identity = format!("redlib-web-{:016x}", fastrand::u64(..));
			proxy.set_username(&identity).map_err(|_| WAIT.to_string())?;
			proxy.set_password(Some(&identity)).map_err(|_| WAIT.to_string())?;
			builder = builder.proxy(wreq::Proxy::all(proxy.as_str()).map_err(|_| WAIT.to_string())?);
		}
		Ok(Self {
			client: builder.build().map_err(|_| WAIT.to_string())?,
			capacity: Semaphore::new(4),
			cooldown: Mutex::new(None),
			ready_logged: AtomicBool::new(false),
			bootstrap: OnceCell::new(),
		})
	}
}

pub(crate) async fn fetch(thread: &ThreadPath, lane: RedditLane) -> Result<Value, String> {
	let session = match lane {
		RedditLane::Direct => &*DIRECT,
		RedditLane::Tor => &*TOR,
	}
	.as_ref()
	.map_err(Clone::clone)?;
	if session.cooldown.lock().unwrap().is_some_and(|until| until > Instant::now()) {
		return Err(WAIT.to_string());
	}
	let permit = tokio::time::timeout(Duration::from_secs(2), session.capacity.acquire())
		.await
		.map_err(|_| WAIT.to_string())?
		.map_err(|_| WAIT.to_string())?;
	let result = tokio::time::timeout(Duration::from_secs(25), fetch_inner(session, thread, lane))
		.await
		.unwrap_or_else(|_| Err(WAIT.to_string()));
	drop(permit);
	if result.is_err() {
		let mut until = session.cooldown.lock().unwrap();
		let next = Instant::now() + Duration::from_secs(5);
		*until = Some(until.map_or(next, |old| old.max(next)));
	} else if !session.ready_logged.swap(true, Ordering::Relaxed) {
		log::info!("Anonymous Reddit website comments are ready: lane={} backend=website_html", lane.label());
	}
	result
}

async fn fetch_inner(session: &Session, thread: &ThreadPath, lane: RedditLane) -> Result<Value, String> {
	let origin = lane.auth_origin();
	// The homepage establishes a fresh logged-out website session. Never copy
	// visitor cookies or OAuth credentials into this separate cookie jar.
	session
		.bootstrap
		.get_or_try_init(|| async {
			let response = session.client.get(format!("{}/", origin.base)).send().await.map_err(|_| WAIT.to_string())?;
			if !response.status().is_success() && !response.status().is_redirection() {
				return Err(WAIT.to_string());
			}
			Ok(())
		})
		.await?;
	let mut url = url::Url::parse(&format!("{}{}", origin.base, thread.website_path)).map_err(|_| WAIT.to_string())?;
	for _ in 0..4 {
		let response = session.client.get(url.as_str()).send().await.map_err(|_| WAIT.to_string())?;
		if response.status().is_redirection() {
			let location = response.headers().get("location").and_then(|v| v.to_str().ok()).ok_or_else(|| WAIT.to_string())?;
			url = checked_redirect(&url, location, lane).ok_or_else(|| WAIT.to_string())?;
			continue;
		}
		if !response.status().is_success() {
			log::warn!(
				"Anonymous Reddit website request rejected: lane={} backend=website_html status={}",
				lane.label(),
				response.status().as_u16()
			);
			if matches!(response.status().as_u16(), 403 | 429 | 503) {
				let delay = response
					.headers()
					.get("retry-after")
					.and_then(|v| v.to_str().ok())
					.and_then(|v| v.parse::<u64>().ok())
					.unwrap_or(30)
					.clamp(5, 900);
				*session.cooldown.lock().unwrap() = Some(Instant::now() + Duration::from_secs(delay));
			}
			return Err(WAIT.to_string());
		}
		let content = response.headers().get("content-type").and_then(|v| v.to_str().ok()).unwrap_or_default();
		if !content.starts_with("text/html") {
			return Err(WAIT.to_string());
		}
		let mut body = hyper::Body::wrap_stream(response.bytes_stream());
		let mut bytes = Vec::new();
		while let Some(chunk) = body.data().await {
			let chunk = chunk.map_err(|_| WAIT.to_string())?;
			if bytes.len().saturating_add(chunk.len()) > MAX_BODY {
				return Err(WAIT.to_string());
			}
			bytes.extend_from_slice(&chunk);
		}
		let html = String::from_utf8(bytes).map_err(|_| WAIT.to_string())?;
		let id = thread.id.clone();
		return tokio::task::spawn_blocking(move || parse_comments(&html, &id))
			.await
			.map_err(|_| WAIT.to_string())?
			.map_err(|reason| {
				log::warn!("Anonymous Reddit website response incomplete: lane={} backend=website_html reason={reason}", lane.label());
				WAIT.to_string()
			});
	}
	Err(WAIT.to_string())
}

fn checked_redirect(base: &url::Url, location: &str, lane: RedditLane) -> Option<url::Url> {
	let next = base.join(location).ok()?;
	(next.scheme() == "https"
		&& next.host_str() == Some(lane.auth_origin().host)
		&& next.port_or_known_default() == Some(443)
		&& next.username().is_empty()
		&& next.password().is_none())
	.then_some(next)
}

fn selector(query: &str) -> Selector {
	Selector::parse(query).expect("fixed website selector")
}

pub(crate) fn parse_comments(html: &str, post_id: &str) -> Result<Value, &'static str> {
	let document = Html::parse_document(html);
	let post_fullname = format!("t3_{post_id}");
	let post = document
		.select(&selector("shreddit-post"))
		.find(|e| e.value().attr("id") == Some(post_fullname.as_str()))
		.ok_or("missing post")?;
	let count = post.value().attr("comment-count").and_then(|v| v.parse::<u64>().ok()).ok_or("missing comment count")?;
	let mut comments = Vec::new();
	let mut ids = HashMap::new();
	for element in document.select(&selector("shreddit-comment")) {
		if element.value().attr("postid") != Some(post_fullname.as_str()) {
			continue;
		}
		if comments.len() >= MAX_COMMENTS {
			return Err("comment limit");
		}
		let fullname = element.value().attr("thingid").ok_or("missing comment id")?;
		let id = fullname.strip_prefix("t1_").filter(|id| valid_id(id)).ok_or("invalid comment id")?;
		if ids.insert(fullname.to_string(), comments.len()).is_some() {
			return Err("duplicate comment");
		}
		let parent = element.value().attr("parentid").unwrap_or(&post_fullname);
		if parent != post_fullname && !parent.strip_prefix("t1_").is_some_and(valid_id) {
			return Err("invalid parent");
		}
		let body = element
			.select(&selector("[slot=comment]"))
			.find(|e| {
				e.ancestors()
					.filter_map(ElementRef::wrap)
					.find(|p| p.value().name() == "shreddit-comment")
					.is_some_and(|p| p.id() == element.id())
			})
			.ok_or("missing comment body")?;
		let timestamp = element.value().attr("created").and_then(parse_timestamp).ok_or("missing timestamp")?;
		let body_html = render_body(body, 0);
		let mut data = json!({"id":id,"name":fullname,"parent_id":parent,"link_id":post_fullname,"author":element.value().attr("author").unwrap_or("[deleted]"),"body":body.text().collect::<String>(),"body_html":body_html,"created_utc":timestamp,"score":element.value().attr("score").and_then(|v|v.parse::<i64>().ok()).unwrap_or(0),"score_hidden":element.value().attr("score").is_none(),"replies":"","all_awardings":[]});
		// Preserve Reddit's permalink to deeper replies using the existing more
		// comment renderer. No tracking or signed continuation URL leaves Redlib.
		if element.select(&selector("[slot=more-comments-permalink]")).any(|e| {
			e.ancestors()
				.filter_map(ElementRef::wrap)
				.find(|p| p.value().name() == "shreddit-comment")
				.is_some_and(|p| p.id() == element.id())
		}) {
			data["replies"] = json!({"kind":"Listing","data":{"children":[{"kind":"more","data":{"id":"_","parent_id":fullname,"count":0}}]}});
		}
		comments.push(json!({"kind":"t1","data":data}));
	}
	if comments.is_empty() && count != 0 {
		return Err("missing comments");
	}
	let mut children: HashMap<String, Vec<usize>> = HashMap::new();
	for (index, comment) in comments.iter().enumerate() {
		let parent = comment["data"]["parent_id"].as_str().ok_or("missing parent")?;
		let parent = if ids.contains_key(parent) { parent } else { &post_fullname };
		children.entry(parent.to_string()).or_default().push(index);
	}
	fn attach(parent: &str, nodes: &[Value], children: &HashMap<String, Vec<usize>>, depth: usize, visited: &mut usize) -> Result<Vec<Value>, &'static str> {
		if depth > MAX_DEPTH {
			return Err("comment nesting limit");
		}
		let mut result = Vec::new();
		for &index in children.get(parent).into_iter().flatten() {
			*visited += 1;
			if *visited > nodes.len() {
				return Err("comment cycle");
			}
			let mut node = nodes[index].clone();
			let mut replies = attach(node["data"]["name"].as_str().unwrap(), nodes, children, depth + 1, visited)?;
			if let Some(more) = node["data"]["replies"]["data"]["children"].as_array() {
				replies.extend(more.iter().cloned());
			}
			if !replies.is_empty() {
				node["data"]["replies"] = json!({"kind":"Listing","data":{"children":replies}});
			}
			result.push(node);
		}
		Ok(result)
	}
	let mut visited = 0;
	let roots = attach(&post_fullname, &comments, &children, 0, &mut visited)?;
	if visited != comments.len() {
		return Err("unreachable comment cycle");
	}
	Ok(json!({"kind":"Listing","data":{"children":roots}}))
}

fn parse_timestamp(value: &str) -> Option<f64> {
	chrono::DateTime::parse_from_rfc3339(value)
		.or_else(|_| chrono::DateTime::parse_from_str(value, "%Y-%m-%dT%H:%M:%S%.f%z"))
		.ok()
		.map(|date| date.timestamp_millis() as f64 / 1000.0)
}

// Reconstruct only inert comment markup. Reddit scripts, forms, custom element
// behavior, event handlers, inline styling and tracking attributes are dropped.
fn render_body(element: ElementRef<'_>, depth: usize) -> String {
	if depth > MAX_DEPTH {
		return String::new();
	}
	let mut output = String::new();
	for child in element.children() {
		match child.value() {
			Node::Text(text) => output.push_str(&htmlescape::encode_minimal(text)),
			Node::Element(_) => {
				let child = ElementRef::wrap(child).unwrap();
				let name = child.value().name();
				if matches!(name, "script" | "style" | "iframe" | "form" | "input" | "button" | "svg" | "template") {
					continue;
				}
				let inner = render_body(child, depth + 1);
				if matches!(
					name,
					"p"
						| "div"
						| "span"
						| "pre"
						| "code"
						| "strong"
						| "em" | "b"
						| "i" | "blockquote"
						| "del"
						| "ul" | "ol"
						| "li" | "table"
						| "thead"
						| "tbody"
						| "tr" | "td"
						| "th" | "sup"
						| "sub"
				) {
					output.push_str(&format!("<{name}>{inner}</{name}>"));
				} else if matches!(name, "br" | "hr") {
					output.push_str(&format!("<{name}>"));
				} else if name == "a" {
					if let Some(href) = child.value().attr("href").filter(|v| safe_link(v)) {
						output.push_str(&format!("<a href=\"{}\" rel=\"nofollow noreferrer\">{inner}</a>", htmlescape::encode_attribute(href)));
					} else {
						output.push_str(&inner);
					}
				} else if name == "img" {
					if let Some(src) = child.value().attr("src").filter(|v| safe_link(v)) {
						let src = crate::utils::format_url(src);
						if !src.is_empty() {
							output.push_str(&format!(
								"<img src=\"{}\" alt=\"{}\" loading=\"lazy\">",
								htmlescape::encode_attribute(&src),
								htmlescape::encode_attribute(child.value().attr("alt").unwrap_or_default())
							));
						}
					}
				} else if name == "shreddit-spoiler" {
					output.push_str(&format!("<span class=\"spoiler\">{inner}</span>"));
				} else {
					output.push_str(&inner);
				}
			}
			_ => {}
		}
	}
	output
}

fn safe_link(value: &str) -> bool {
	(value.starts_with('/') && !value.starts_with("//") && !value.contains('\\'))
		|| url::Url::parse(value).is_ok_and(|u| matches!(u.scheme(), "http" | "https") && u.username().is_empty() && u.password().is_none())
}

#[cfg(test)]
mod tests {
	use super::*;

	#[tokio::test(flavor = "multi_thread")]
	#[ignore = "requires explicit isolated Tor access to a public Reddit thread"]
	async fn live_anonymous_comments_html() {
		let _ = pretty_env_logger::try_init();
		let path = ThreadPath::parse("/r/codex/comments/1xaomu1/why_is_my_dot_mad_at_me/.json?sort=new").unwrap();
		let result = fetch(&path, RedditLane::Tor).await.expect("anonymous website comments must load");
		let roots = result["data"]["children"].as_array().expect("comment listing");
		assert!(!roots.is_empty(), "the supplied public thread has comments");
		assert!(roots.iter().all(|c| c["data"]["body_html"].as_str().is_some_and(|body| !body.is_empty())));
		println!("ANONYMOUS_HTML_ADAPTER root_comments={} tree_valid=true", roots.len());
	}

	#[tokio::test(flavor = "multi_thread")]
	#[ignore = "requires explicit isolated Tor access and an anonymous OAuth session"]
	async fn live_anonymous_comments_page() {
		let _ = pretty_env_logger::try_init();
		assert_eq!(std::env::var("REDLIB_REDDIT_PRIMARY_LANE").as_deref(), Ok("tor"));
		let oauth = tokio::time::timeout(Duration::from_secs(60), crate::oauth::Oauth::new(RedditLane::Tor))
			.await
			.expect("anonymous Tor OAuth startup");
		assert!(crate::client::install_oauth_client(oauth, true, None));
		for query in ["", "?sort=new"] {
			let path = format!("/r/codex/comments/1xaomu1/why_is_my_dot_mad_at_me/{query}");
			let request = hyper::Request::builder().uri(&path).body(hyper::Body::empty()).unwrap();
			let response = tokio::time::timeout(Duration::from_secs(75), crate::post::item(request))
				.await
				.expect("page budget")
				.expect("rendered page");
			assert_eq!(response.status(), 200, "page must load without the recovery screen");
			let body = hyper::body::to_bytes(response.into_body()).await.unwrap();
			let body = String::from_utf8(body.to_vec()).unwrap();
			assert!(body.contains("Why is my Dot mad at me?"));
			assert!(body.contains("class=\"comment\""), "page must contain real comments");
			assert!(!body.contains("Still trying to load your page"));
			println!("ANONYMOUS_COMMENTS_PAGE sorted={} status=200 post_visible=true comments_visible=true", !query.is_empty());
		}
	}

	fn page(comments: &str) -> String {
		format!(
			r#"<shreddit-post id="t3_abc123" comment-count="2"></shreddit-post><template><shreddit-comment-tree post-id="t3_abc123">{comments}</shreddit-comment-tree></template>"#
		)
	}
	fn comment(id: &str, parent: &str, body: &str) -> String {
		format!(
			r#"<shreddit-comment thingid="t1_{id}" postid="t3_abc123" parentid="{parent}" author="example" score="4" created="2026-10-20T10:36:30.813000+0000"><details><div slot="comment"><div><p>{body}</p></div></div></details></shreddit-comment>"#
		)
	}

	#[test]
	fn comments_html_reads_template_comments_and_rebuilds_reply_tree() {
		let html = page(&(comment("first", "t3_abc123", "Parent") + &comment("reply", "t1_first", "Reply")));
		let parsed = parse_comments(&html, "abc123").unwrap();
		assert_eq!(parsed["data"]["children"].as_array().unwrap().len(), 1);
		let first = &parsed["data"]["children"][0]["data"];
		assert_eq!(first["id"], "first");
		assert_eq!(first["body_html"], "<div><p>Parent</p></div>");
		assert_eq!(first["created_utc"], 1792492590.813);
		assert_eq!(first["replies"]["data"]["children"][0]["data"]["id"], "reply");
	}

	#[test]
	fn comments_html_keeps_the_requested_sort_order() {
		let html = page(&(comment("second", "t3_abc123", "Second") + &comment("first", "t3_abc123", "First")));
		let parsed = parse_comments(&html, "abc123").unwrap();
		assert_eq!(parsed["data"]["children"][0]["data"]["id"], "second");
		assert_eq!(parsed["data"]["children"][1]["data"]["id"], "first");
	}

	#[test]
	fn comments_html_drops_active_markup_and_escapes_text_and_attributes() {
		let html = page(&comment(
			"first",
			"t3_abc123",
			r#"&lt;script&gt; <strong onclick="alert(1)">bold</strong><script>secret()</script><iframe src="https://bad.example"></iframe><a href="javascript:alert(1)">unsafe</a><a href="https://example.com/?x=&quot;a&quot;" onclick="bad()">safe</a><img src="data:image/svg+xml,bad"><shreddit-spoiler>spoiler</shreddit-spoiler>"#,
		));
		let parsed = parse_comments(&html, "abc123").unwrap();
		let body = parsed["data"]["children"][0]["data"]["body_html"].as_str().unwrap();
		assert!(body.contains("&lt;script&gt;"));
		assert!(body.contains("<strong>bold</strong>"));
		assert!(body.contains("unsafe<a href="));
		assert!(body.contains("&quot;a&quot;"));
		assert!(body.contains("class=\"spoiler\""));
		for forbidden in ["onclick", "javascript:", "data:", "<script>", "iframe", "secret()"] {
			assert!(!body.contains(forbidden), "{forbidden}");
		}
	}

	#[test]
	fn comments_html_rejects_blocks_incomplete_trees_and_wrong_posts() {
		assert!(parse_comments("<html>blocked by network security</html>", "abc123").is_err());
		assert!(parse_comments(&page(""), "abc123").is_err());
		assert!(parse_comments(&page(&comment("first", "t3_abc123", "Body")), "different").is_err());
		let missing_body = page(&comment("first", "t3_abc123", "Body")).replace("slot=\"comment\"", "slot=\"actionRow\"");
		assert!(parse_comments(&missing_body, "abc123").is_err());
		assert!(parse_comments("<shreddit-post id=t3_abc123 comment-count=0></shreddit-post>", "abc123").is_ok());
	}

	#[test]
	fn comments_html_rejects_duplicate_ids_and_cycles() {
		assert!(parse_comments(&page(&(comment("first", "t3_abc123", "Body") + &comment("first", "t3_abc123", "Duplicate"))), "abc123").is_err());
		assert!(parse_comments(&page(&(comment("first", "t1_reply", "Body") + &comment("reply", "t1_first", "Cycle"))), "abc123").is_err());
	}

	#[test]
	fn comments_html_does_not_use_a_childs_body_for_its_parent() {
		let html = page(&format!(
			r#"<shreddit-comment thingid=t1_parent postid=t3_abc123>{}</shreddit-comment>"#,
			comment("child", "t1_parent", "Child")
		));
		assert!(parse_comments(&html, "abc123").is_err());
	}

	#[test]
	fn comments_html_preserves_deeper_reply_links_without_tracking_urls() {
		let html = page(&comment("first", "t3_abc123", "Body").replace(
			"</details>",
			r#"<a slot="more-comments-permalink" href="/r/example/comments/abc123/comment/first/?tracking=secret">More</a></details>"#,
		));
		let parsed = parse_comments(&html, "abc123").unwrap();
		let more = &parsed["data"]["children"][0]["data"]["replies"]["data"]["children"][0];
		assert_eq!(more["kind"], "more");
		assert_eq!(more["data"]["parent_id"], "t1_first");
		assert!(!parsed.to_string().contains("tracking"));
	}

	#[test]
	fn comments_html_paths_keep_thread_context_and_allowlisted_queries() {
		let path = ThreadPath::parse("/r/codex/comments/abc123/_/reply.json?sort=confidence&context=3&raw_json=1&secret=discard").unwrap();
		assert_eq!(path.id, "abc123");
		assert_eq!(path.website_path, "/r/codex/comments/abc123/comment/reply/?sort=best&context=3");
		for path in [
			"//outside.example/comments/abc123.json",
			"https://outside.example/comments/abc123.json",
			"/r/all.json",
			"/user/example/comments.json",
			"/r/example/comments/../_/.json",
		] {
			assert!(ThreadPath::parse(path).is_none(), "{path}");
		}
	}

	#[test]
	fn comments_html_redirects_stay_on_the_same_lane_and_website_origin() {
		let base = url::Url::parse(RedditLane::Tor.auth_origin().base).unwrap();
		assert!(checked_redirect(&base, "/r/codex/comments/abc123/", RedditLane::Tor).is_some());
		for location in [
			"https://www.reddit.com/",
			"https://evil.example/",
			"http://www.reddit.com/",
			"https://www.reddit.com:444/",
			"https://user:secret@www.reddit.com/",
		] {
			assert!(checked_redirect(&base, location, RedditLane::Tor).is_none());
		}
	}
}
