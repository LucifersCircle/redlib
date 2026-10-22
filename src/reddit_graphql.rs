//! Anonymous, read-only operations used by Reddit's Android client.
//! Persisted operation names/hashes are protocol identifiers, not account credentials.
use pulldown_cmark::{html, CowStr, Event, Options, Parser, Tag, TagEnd};
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};

const POSTS_HASH: &str = "0e471495bcba33554b6c540b3cf1cae23dd5432a8b741dddc7fd6717dc2520a7";
const COMMENTS_HASH: &str = "314f29c2494fd78c7844ef2c6b09acb4bef5b37ab50b4ddc01691d6d5c8433f8";

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CommentsRequest {
	pub(crate) post_id: String,
	comment_id: Option<String>,
	sort: String,
	limit: u16,
	depth: u8,
	after: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ErrorKind {
	Session,
	RateLimit,
	Resource,
	Invalid,
}

#[derive(Debug)]
pub(crate) struct GraphqlError {
	pub(crate) kind: ErrorKind,
	pub(crate) message: String,
}
impl GraphqlError {
	fn invalid() -> Self {
		Self {
			kind: ErrorKind::Invalid,
			message: "Reddit is temporarily unable to load comments. Retry in 30 seconds".into(),
		}
	}
	fn resource(message: &str) -> Self {
		Self {
			kind: ErrorKind::Resource,
			message: message.into(),
		}
	}
}

pub(crate) fn response_error(value: &Value) -> Option<GraphqlError> {
	let error = value["errors"].as_array()?.first()?;
	let code = error["extensions"]["code"].as_str().unwrap_or_default();
	Some(match code {
		"NOT_AUTHENTICATED" | "UNAUTHENTICATED" | "UNAUTHORIZED" => GraphqlError {
			kind: ErrorKind::Session,
			message: "Reddit's anonymous comments session is unavailable. Retry in 5 seconds".into(),
		},
		"TOO_MANY_REQUESTS" | "RATE_LIMITED" | "RATELIMIT" => GraphqlError {
			kind: ErrorKind::RateLimit,
			message: "Reddit rate limit exceeded. Retry in 30 seconds".into(),
		},
		"NOT_FOUND" => GraphqlError::resource("Post not found"),
		"PRIVATE_SUBREDDIT" => GraphqlError::resource("private"),
		"QUARANTINED" => GraphqlError::resource("quarantined"),
		_ => GraphqlError::invalid(),
	})
}

impl CommentsRequest {
	pub(crate) fn from_path(path: &str) -> Option<Self> {
		let (base, query) = path.split_once('?').unwrap_or((path, ""));
		let base = base.strip_suffix(".json").unwrap_or(base).trim_end_matches('/');
		let segments = base.trim_start_matches('/').split('/').collect::<Vec<_>>();
		let tail = match segments.as_slice() {
			["comments", tail @ ..] => tail,
			["r" | "u" | "user", _, "comments", tail @ ..] => tail,
			_ => return None,
		};
		let (id, focused) = match tail {
			[id] | [id, _] => (*id, None),
			[id, _, comment] => (*id, Some(*comment)),
			_ => return None,
		};
		let valid_id = |id: &str| !id.is_empty() && id.len() <= 20 && id.bytes().all(|b| b.is_ascii_alphanumeric());
		if !valid_id(id) || focused.is_some_and(|id| !valid_id(id)) {
			return None;
		}
		let pairs = url::form_urlencoded::parse(query.as_bytes()).into_owned().collect::<HashMap<_, _>>();
		let sort = match pairs.get("sort").map(String::as_str).unwrap_or("confidence") {
			"top" => "TOP",
			"new" => "NEW",
			"old" => "OLD",
			"controversial" => "CONTROVERSIAL",
			"qa" => "QA",
			"random" => "RANDOM",
			"live" => "LIVE",
			_ => "CONFIDENCE",
		}
		.to_string();
		let comment_id = focused.map(str::to_string).or_else(|| pairs.get("comment").filter(|id| valid_id(id)).cloned());
		Some(Self {
			post_id: format!("t3_{id}"),
			comment_id,
			sort,
			limit: pairs.get("limit").and_then(|v| v.parse().ok()).unwrap_or(200).min(500),
			depth: pairs.get("depth").and_then(|v| v.parse().ok()).unwrap_or(10).min(10),
			after: pairs.get("after").filter(|v| !v.is_empty() && v.len() <= 8192 && !v.chars().any(char::is_control)).cloned(),
		})
	}
	pub(crate) fn post_payload(&self) -> Value {
		operation(
			"PostsByIds",
			POSTS_HASH,
			json!({"ids":[self.post_id], "includePostContentPostHint":true, "includePostContentThumbnailEnabled":true}),
		)
	}
	pub(crate) fn comments_payload(&self) -> Value {
		let mut variables = json!({"id":self.post_id, "sortType":self.sort, "count":self.limit.max(1), "maxDepth":self.depth, "includePremiumAvatarTreatment":false,"includeConsistencyToken":false});
		if let Some(id) = &self.comment_id {
			variables["focusedCommentContext"] = json!({"commentId":format!("t1_{id}"),"isFocusedThreadOnly":true});
		}
		if let Some(after) = &self.after {
			variables["after"] = json!(after);
		}
		operation("PostComments", COMMENTS_HASH, variables)
	}
	pub(crate) fn post(&self, value: &Value, quarantine: bool) -> Result<Value, GraphqlError> {
		if let Some(error) = response_error(value) {
			return Err(error);
		}
		let post = value["data"]["postsInfoByIds"]
			.as_array()
			.and_then(|posts| posts.iter().find(|p| p["id"] == self.post_id))
			.ok_or_else(|| GraphqlError::resource("Post not found"))?;
		if post["subreddit"]["isQuarantined"] == true && !quarantine {
			return Err(GraphqlError::resource("quarantined"));
		}
		if post["subreddit"]["type"] == "PRIVATE" {
			return Err(GraphqlError::resource("private"));
		}
		if !matches!(post["__typename"].as_str(), Some("SubredditPost" | "ProfilePost")) {
			return Err(GraphqlError::resource("gated"));
		}
		Ok(json!({"kind":"t3", "data":legacy_post(post, 0)}))
	}
	pub(crate) fn finish(&self, post: Value, response: &Value) -> Result<Value, GraphqlError> {
		if let Some(error) = response_error(response) {
			return Err(error);
		}
		let info = &response["data"]["postInfoById"];
		if info["id"] != self.post_id {
			return Err(GraphqlError::invalid());
		}
		let trees = info["commentForest"]["trees"].as_array().ok_or_else(GraphqlError::invalid)?;
		let mut entries = Vec::new();
		let mut ids = HashSet::new();
		let mut after = None;
		for tree in trees {
			let parent = tree["parentId"].as_str().unwrap_or(&self.post_id).to_string();
			let node = if tree["node"].is_object() {
				let id = tree["node"]["id"].as_str().ok_or_else(GraphqlError::invalid)?.to_string();
				if !ids.insert(id.clone()) {
					return Err(GraphqlError::invalid());
				}
				(
					id,
					json!({"kind":"t1", "data":legacy_comment(&tree["node"], &parent, &self.post_id, &post["data"]["permalink"])}),
				)
			} else if tree["more"].is_object() {
				if tree["parentId"].is_null() {
					after = tree["more"]["cursor"].as_str().map(str::to_string);
					continue;
				}
				(
					format!("more-{}", entries.len()),
					json!({"kind":"more","data":{"id":"_","parent_id":parent,"count":tree["more"]["count"],"children":[]}}),
				)
			} else {
				continue;
			};
			entries.push((node.0, parent, node.1));
		}
		let mut children: HashMap<String, Vec<usize>> = HashMap::new();
		let mut roots = Vec::new();
		for (index, (_, parent, _)) in entries.iter().enumerate() {
			if ids.contains(parent) {
				children.entry(parent.clone()).or_default().push(index);
			} else {
				roots.push(index);
			}
		}
		fn assemble(
			index: usize,
			entries: &[(String, String, Value)],
			children: &HashMap<String, Vec<usize>>,
			visiting: &mut HashSet<usize>,
			depth: usize,
		) -> Result<Value, GraphqlError> {
			if depth > 64 || !visiting.insert(index) {
				return Err(GraphqlError::invalid());
			}
			let (id, _, entry) = &entries[index];
			let mut result = entry.clone();
			if result["kind"] == "t1" {
				let replies = children
					.get(id)
					.into_iter()
					.flatten()
					.map(|&i| assemble(i, entries, children, visiting, depth + 1))
					.collect::<Result<Vec<_>, _>>()?;
				result["data"]["replies"] = if replies.is_empty() { json!("") } else { listing(replies, None) };
			}
			Ok(result)
		}
		let mut visited = HashSet::new();
		let comments = if self.limit == 0 {
			Vec::new()
		} else {
			roots
				.into_iter()
				.map(|i| assemble(i, &entries, &children, &mut visited, 0))
				.collect::<Result<Vec<_>, _>>()?
		};
		if self.limit > 0 && visited.len() != entries.len() {
			return Err(GraphqlError::invalid());
		}
		Ok(json!([listing(vec![post], None), listing(comments, after)]))
	}
}

fn operation(name: &str, hash: &str, variables: Value) -> Value {
	json!({"operationName":name,"variables":variables,"extensions":{"persistedQuery":{"version":1,"sha256Hash":hash}}})
}
fn listing(children: Vec<Value>, after: Option<String>) -> Value {
	json!({"kind":"Listing","data":{"children":children,"after":after,"before":null}})
}
fn short_id(id: &Value) -> &str {
	let id = id.as_str().unwrap_or_default();
	id.strip_prefix("t3_").or_else(|| id.strip_prefix("t1_")).unwrap_or(id)
}
fn timestamp(value: &Value) -> f64 {
	value
		.as_str()
		.and_then(|v| {
			chrono::DateTime::parse_from_rfc3339(v)
				.or_else(|_| chrono::DateTime::parse_from_str(v, "%Y-%m-%dT%H:%M:%S%.f%z"))
				.ok()
		})
		.map_or(0.0, |v| v.timestamp_millis() as f64 / 1000.0)
}
fn author(value: &Value) -> &str {
	value["name"].as_str().filter(|name| !name.is_empty()).unwrap_or("[deleted]")
}
fn distinguished(value: &Value) -> String {
	value.as_str().unwrap_or_default().to_ascii_lowercase()
}
fn awardings(value: &Value) -> Value {
	json!(value
		.as_array()
		.into_iter()
		.flatten()
		.map(|v| json!({"name":v["award"]["name"],"count":v["total"],"icon_url":v["award"]["static_icon_64"]["url"]}))
		.collect::<Vec<_>>())
}
fn add_flair(data: &mut Value, prefix: &str, flair: &Value) {
	data[format!("{prefix}_flair_type")] = json!("text");
	data[format!("{prefix}_flair_text")] = flair["text"].clone();
	data[format!("{prefix}_flair_background_color")] = flair["template"]["backgroundColor"].clone();
	data[format!("{prefix}_flair_text_color")] = json!(flair["textColor"].as_str().unwrap_or_default().to_ascii_lowercase());
	if let Some(parts) = flair["richtext"].as_array() {
		data[format!("{prefix}_flair_type")] = json!("richtext");
		data[format!("{prefix}_flair_richtext")] = json!(parts
			.iter()
			.map(|part| if part["url"].is_string() {
				json!({"e":"emoji","u":part["url"],"a":part["text"]})
			} else {
				json!({"e":"text","t":part["text"]})
			})
			.collect::<Vec<_>>());
	}
}
fn legacy_comment(node: &Value, parent: &str, post_id: &str, permalink: &Value) -> Value {
	let removed = node["isRemoved"] == true || node["isAdminTakedown"] == true || node["__typename"] == "DeletedComment";
	let text = if removed {
		if matches!(node["removedByCategory"].as_str(), Some("AUTHOR" | "DELETED")) {
			"[deleted]"
		} else {
			"[removed]"
		}
	} else {
		node["content"]["markdown"].as_str().or_else(|| node["content"]["preview"].as_str()).unwrap_or_default()
	};
	let mut data = json!({"id":short_id(&node["id"]),"name":node["id"],"parent_id":parent,"link_id":post_id,
        "author":if removed { "[deleted]" } else { author(&node["authorInfo"]) },"body":text,"body_html":markdown(text,&node["content"]),
        "created_utc":timestamp(&node["createdAt"]),"edited":if node["editedAt"].is_string(){json!(timestamp(&node["editedAt"]))}else{json!(false)},
        "score":node["score"].as_i64().unwrap_or_default(),"score_hidden":node["isScoreHidden"] == true,"stickied":node["isStickied"] == true,
        "distinguished":distinguished(&node["distinguishedAs"]),"is_submitter":node["isOP"] == true,"permalink":node["permalink"].as_str().unwrap_or(permalink.as_str().unwrap_or_default()),
        "all_awardings":awardings(&node["awardings"]),"replies":""});
	add_flair(&mut data, "author", &node["authorFlair"]);
	data
}
fn source(value: &Value) -> Value {
	json!({"url":value["url"],"width":value["dimensions"]["width"],"height":value["dimensions"]["height"]})
}
fn asset_metadata(asset: &Value) -> Value {
	let url = asset["url"].as_str().or_else(|| asset["xxxlarge"]["url"].as_str()).unwrap_or_default();
	let animated = asset["mimetype"] == "image/gif" || asset["__typename"] == "AnimatedImageAsset";
	let mut source = json!({"u":url,"x":asset["width"],"y":asset["height"]});
	if animated {
		source["gif"] = json!(url);
	}
	let previews = ["small", "medium", "large", "xlarge", "xxlarge", "xxxlarge"]
		.iter()
		.filter_map(|key| {
			let item = &asset[*key];
			item["url"]
				.as_str()
				.map(|url| json!({"u":url,"x":item["dimensions"]["width"],"y":item["dimensions"]["height"]}))
		})
		.collect::<Vec<_>>();
	json!({"status":"valid","e":if animated {"AnimatedImage"} else {"Image"},"m":asset["mimetype"],"s":source,"p":previews})
}
fn legacy_post(post: &Value, depth: usize) -> Value {
	let content = &post["content"];
	let text = content["markdown"].as_str().unwrap_or_default();
	let mut data = json!({"id":short_id(&post["id"]),"name":post["id"],"title":post["postTitle"].as_str().or_else(||post["title"].as_str()).unwrap_or_default(),
        "author":author(&post["authorInfo"]),"subreddit":post["subreddit"]["name"],"subreddit_id":post["subreddit"]["id"],"permalink":post["permalink"],
        "created_utc":timestamp(&post["createdAt"]),"score":post["score"],"upvote_ratio":post["upvoteRatio"],"num_comments":post["commentCount"],
        "domain":post["domain"],"url":post["url"],"url_overridden_by_dest":post["url"],"selftext":text,"selftext_html":markdown(text,content),
        "is_self":post["isSelfPost"] == true,"over_18":post["isNsfw"] == true,"spoiler":post["isSpoiler"] == true,"stickied":post["isStickied"] == true,
        "archived":post["isArchived"] == true,"locked":post["isLocked"] == true,"distinguished":distinguished(&post["distinguishedAs"]),
        "post_hint":post["postHint"].as_str().unwrap_or_default().to_ascii_lowercase(),"thumbnail":post["thumbnail"]["url"],
        "thumbnail_width":post["thumbnail"]["dimensions"]["width"],"thumbnail_height":post["thumbnail"]["dimensions"]["height"],
        "all_awardings":awardings(&post["awardings"]),"removed_by_category":distinguished(&post["removedByCategory"]),
        "suggested_sort":post["suggestedCommentSort"].as_str().unwrap_or_default().to_ascii_lowercase()});
	add_flair(&mut data, "author", &post["authorFlair"]);
	add_flair(&mut data, "link", &post["flair"]);
	let media = &post["media"];
	let still = &media["still"];
	let resolutions = ["small", "medium", "large", "xlarge", "xxlarge", "xxxlarge"]
		.iter()
		.filter_map(|key| still[*key].is_object().then(|| source(&still[*key])))
		.collect::<Vec<_>>();
	let mut preview = json!({"source":source(&still["source"]),"resolutions":resolutions,"variants":{}});
	let mp4 = if media["animated"]["mp4_source"].is_object() {
		&media["animated"]["mp4_source"]
	} else {
		&media["animated"]["mp4"]
	};
	if mp4["url"].is_string() {
		preview["variants"]["mp4"] = json!({"source":source(mp4)});
	}
	data["preview"] = json!({"images":[preview]});
	let streaming = &media["streaming"];
	if let Some(hls) = streaming["hlsUrl"].as_str() {
		let mut url = url::Url::parse(hls).ok();
		if let Some(url) = url.as_mut() {
			let pairs = url
				.query_pairs()
				.filter(|(key, _)| key != "a")
				.map(|(k, v)| (k.into_owned(), v.into_owned()))
				.collect::<Vec<_>>();
			url.query_pairs_mut().clear().extend_pairs(pairs);
		}
		let hls = url.map(String::from).unwrap_or_else(|| hls.to_string());
		let fallback = media["download"]["url"].as_str().or_else(|| streaming["scrubberMediaUrl"].as_str()).unwrap_or_default();
		let proxied = crate::utils::format_url(fallback);
		if (proxied.starts_with("/packaged-video/") || proxied.starts_with("/vid/")) && crate::utils::format_url(&hls).starts_with("/hls/") {
			data["secure_media"] = json!({"reddit_video":{"fallback_url":fallback,"hls_url":hls,"width":streaming["dimensions"]["width"],"height":streaming["dimensions"]["height"],"is_gif":streaming["isGif"] == true}});
			data["is_video"] = json!(true);
		}
	}
	if let Some(items) = post["gallery"]["items"].as_array() {
		let mut metadata = serde_json::Map::new();
		let mut gallery = Vec::new();
		for (index, item) in items.iter().enumerate() {
			let asset = &item["media"];
			let id = asset["id"].as_str().map(str::to_string).unwrap_or_else(|| index.to_string());
			metadata.insert(id.clone(), asset_metadata(asset));
			gallery.push(json!({"media_id":id,"id":index,"caption":item["caption"],"outbound_url":item["outboundUrl"]}));
		}
		data["is_gallery"] = json!(!gallery.is_empty());
		data["gallery_data"] = json!({"items":gallery});
		data["media_metadata"] = json!(metadata);
	}
	if let Some(options) = post["poll"]["options"].as_array() {
		data["poll_data"] = json!({"options":options.iter().map(|option|json!({"id":option["id"],"text":option["text"],"vote_count":option["voteCount"]})).collect::<Vec<_>>(),"total_vote_count":post["poll"]["totalVoteCount"],"voting_end_timestamp":timestamp(&post["poll"]["votingEndsAt"])*1000.0});
	}
	if depth < 4 && post["crosspostRoot"]["postInfo"].is_object() {
		data["crosspost_parent_list"] = json!([legacy_post(&post["crosspostRoot"]["postInfo"], depth + 1)]);
	}
	data
}

fn safe_destination(value: &str) -> bool {
	let trimmed = value.trim();
	if trimmed.chars().any(char::is_control) || trimmed.starts_with('\\') {
		return false;
	}
	if trimmed.starts_with('/') || trimmed.starts_with('#') || trimmed.is_empty() {
		return true;
	}
	match url::Url::parse(trimmed) {
		Ok(url) => matches!(url.scheme(), "http" | "https" | "mailto"),
		Err(_) => !trimmed.contains(':'),
	}
}
pub(crate) fn markdown(text: &str, content: &Value) -> String {
	let nonce = uuid::Uuid::new_v4().simple().to_string();
	let open = format!("redlibspoileropen{nonce}");
	let close = format!("redlibspoilerclose{nonce}");
	static SPOILERS: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| regex::Regex::new(r"(?s)>!(.+?)!<").unwrap());
	let prepared = SPOILERS.replace_all(text, |caps: &regex::Captures<'_>| format!("{open}{}{close}", &caps[1])).into_owned();
	let media = content["richtextMedia"]
		.as_array()
		.into_iter()
		.flatten()
		.filter_map(|asset| {
			let id = asset["id"].as_str()?;
			let url = asset["url"].as_str().or_else(|| asset["xxxlarge"]["url"].as_str())?;
			Some((id, url))
		})
		.collect::<HashMap<_, _>>();
	let mut events = Vec::new();
	let mut code_block = false;
	let mut image_links = Vec::new();
	for event in Parser::new_ext(
		&prepared,
		Options::ENABLE_TABLES | Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TASKLISTS | Options::ENABLE_SUPERSCRIPT,
	) {
		match event {
			Event::Start(Tag::CodeBlock(kind)) => {
				code_block = true;
				events.push(Event::Start(Tag::CodeBlock(kind)));
			}
			Event::End(TagEnd::CodeBlock) => {
				code_block = false;
				events.push(Event::End(TagEnd::CodeBlock));
			}
			Event::Html(value) | Event::InlineHtml(value) => events.push(Event::Text(CowStr::from(value.replace(&open, ">!").replace(&close, "!<")))),
			Event::Code(value) => events.push(Event::Code(CowStr::from(value.replace(&open, ">!").replace(&close, "!<")))),
			Event::Text(value) if code_block => events.push(Event::Text(CowStr::from(value.replace(&open, ">!").replace(&close, "!<")))),
			Event::Text(value) => {
				let mut rest = value.as_ref();
				while let Some((index, is_open)) = [rest.find(&open).map(|i| (i, true)), rest.find(&close).map(|i| (i, false))]
					.into_iter()
					.flatten()
					.min_by_key(|(i, _)| *i)
				{
					events.push(Event::Text(CowStr::from(rest[..index].to_string())));
					events.push(Event::InlineHtml(CowStr::Borrowed(if is_open { "<span class=\"md-spoiler-text\">" } else { "</span>" })));
					rest = &rest[index + if is_open { open.len() } else { close.len() }..];
				}
				events.push(Event::Text(CowStr::from(rest.to_string())));
			}
			Event::Start(Tag::Link { link_type, dest_url, title, id }) => events.push(Event::Start(Tag::Link {
				link_type,
				dest_url: if safe_destination(&dest_url) { dest_url } else { CowStr::Borrowed("#") },
				title,
				id,
			})),
			Event::Start(Tag::Image { link_type, dest_url, title, id }) => {
				let destination = media.get(dest_url.as_ref()).map(|url| CowStr::from((*url).to_string())).unwrap_or(dest_url);
				let proxied = crate::utils::format_url(&destination);
				let inline = ["/img/", "/preview/", "/thumb/", "/emote/", "/static/", "/style/"]
					.iter()
					.any(|prefix| proxied.starts_with(prefix));
				image_links.push(!inline);
				if inline {
					events.push(Event::Start(Tag::Image {
						link_type,
						dest_url: CowStr::from(proxied),
						title,
						id,
					}));
				} else {
					// External Markdown images remain explicit links. Never make
					// the browser contact an arbitrary image host automatically.
					events.push(Event::Start(Tag::Link {
						link_type,
						dest_url: if safe_destination(&destination) { destination } else { CowStr::Borrowed("#") },
						title,
						id,
					}));
				}
			}
			Event::End(TagEnd::Image) => events.push(Event::End(if image_links.pop().unwrap_or(false) { TagEnd::Link } else { TagEnd::Image })),
			event => events.push(event),
		}
	}
	let mut result = String::from("<div class=\"md\">");
	html::push_html(&mut result, events.into_iter());
	result.push_str("</div>");
	result
}

#[cfg(test)]
mod tests {
	use super::*;
	#[test]
	fn test_graphql_comments_request_preserves_scope_sort_and_focus() {
		let request = CommentsRequest::from_path("/r/gaming/comments/abc/_/reply.json?sort=new&limit=300&depth=7&after=a%2Bb").unwrap();
		let payload = request.comments_payload();
		assert_eq!(payload["variables"]["id"], "t3_abc");
		assert_eq!(payload["variables"]["focusedCommentContext"]["commentId"], "t1_reply");
		assert_eq!(payload["variables"]["sortType"], "NEW");
		assert_eq!(payload["variables"]["after"], "a+b");
		assert_eq!(payload["variables"]["count"], 300);
		assert!(CommentsRequest::from_path("/r/gaming/hot.json").is_none());
		assert!(CommentsRequest::from_path("/comments/invalid-id.json").is_none());
	}
	#[test]
	fn test_graphql_comments_markdown_escapes_html_and_unsafe_links() {
		let html = markdown(
			"<script>alert(1)</script>\n\n[bad](javascript:alert%281%29) **bold** >!hidden!< `>!literal!<`",
			&Value::Null,
		);
		assert!(!html.contains("<script>"));
		assert!(!html.contains("href=\"javascript:"));
		assert!(html.contains("<strong>bold</strong>"));
		assert!(html.contains("md-spoiler-text\">hidden</span>"));
		assert!(html.contains("<code>&gt;!literal!&lt;</code>"));
	}
	#[test]
	fn test_graphql_comments_images_stay_proxied_and_external_images_are_links() {
		let html = markdown("![reddit](https://i.redd.it/example.png) ![remote](https://tracking.example/pixel)", &Value::Null);
		assert!(html.contains("src=\"/img/example.png\""));
		assert!(!html.contains("src=\"https://"));
		assert!(html.contains("href=\"https://tracking.example/pixel\""));
	}

	#[test]
	fn test_graphql_comments_nested_tree_deleted_nodes_and_cursor() {
		let request = CommentsRequest::from_path("/comments/abc.json").unwrap();
		let post = json!({"kind":"t3","data":{"id":"abc","permalink":"/r/test/comments/abc/_/"}});
		let response = json!({"data":{"postInfoById":{"id":"t3_abc","commentForest":{"trees":[
				{"parentId":"t1_parent","node":{"id":"t1_child","content":{"markdown":"reply"},"authorInfo":{"name":"Bob"}}},
				{"parentId":null,"node":{"id":"t1_parent","content":{"markdown":"parent"},"authorInfo":{"name":"Alice"}}},
				{"parentId":"t1_child","node":{"id":"t1_deleted","__typename":"DeletedComment","content":{"markdown":"must not appear"},"removedByCategory":"AUTHOR"}},
				{"parentId":"t1_child","more":{"count":5,"cursor":"nested"}},
				{"parentId":null,"more":{"count":20,"cursor":"opaque+cursor&next"}}
		]}}}});
		let value = request.finish(post, &response).unwrap();
		let parent = &value[1]["data"]["children"][0];
		assert_eq!(parent["data"]["id"], "parent");
		let child = &parent["data"]["replies"]["data"]["children"][0];
		assert_eq!(child["data"]["id"], "child");
		let replies = &child["data"]["replies"]["data"]["children"];
		assert_eq!(replies[0]["data"]["body"], "[deleted]");
		assert_eq!(replies[1]["kind"], "more");
		assert_eq!(value[1]["data"]["after"], "opaque+cursor&next");
	}

	#[tokio::test]
	async fn test_graphql_comments_post_media_poll_and_markdown_remain_renderable() {
		let request = CommentsRequest::from_path("/comments/abc.json").unwrap();
		let base = json!({"__typename":"SubredditPost","id":"t3_abc","postTitle":"Fixture","subreddit":{"name":"test","type":"PUBLIC"},"createdAt":"2026-10-22T11:09:01.966000+0000","permalink":"/r/test/comments/abc/_/","url":"https://v.redd.it/example"});
		let convert = |post: Value| request.post(&json!({"data":{"postsInfoByIds":[post]}}), false).unwrap();
		let mut video = base.clone();
		video["media"] = json!({"streaming":{"hlsUrl":"https://v.redd.it/example/HLSPlaylist.m3u8?a=tracking&v=1","dimensions":{"width":720,"height":1280}},"download":{"url":"https://packaged-media.redd.it/example/dl/video.mp4?m=DASHPlaylist.mpd&v=1"}});
		let video = crate::utils::parse_post(&convert(video)).await;
		assert_eq!(video.post_type, "video");
		assert!(video.media.url.starts_with("/packaged-video/"));
		assert!(video.media.alt_url.starts_with("/hls/"));
		assert!(!video.media.alt_url.contains("a=tracking"));
		let mut gif = base.clone();
		gif["postHint"] = json!("IMAGE");
		gif["media"] = json!({"animated":{"mp4_source":{"url":"https://preview.redd.it/example.gif?format=mp4","dimensions":{"width":640,"height":400}}}});
		let gif = crate::utils::parse_post(&convert(gif)).await;
		assert_eq!(gif.post_type, "gif");
		assert!(gif.media.url.starts_with("/preview/"));
		let mut gallery = base.clone();
		gallery["gallery"] = json!({"items":[{"media":{"id":"image","mimetype":"image/jpeg","url":"https://i.redd.it/example.jpeg","width":1000,"height":800,"xlarge":{"url":"https://preview.redd.it/example.jpeg?width=640","dimensions":{"width":640,"height":512}}}}]});
		let gallery = crate::utils::parse_post(&convert(gallery)).await;
		assert_eq!(gallery.post_type, "gallery");
		assert_eq!(gallery.gallery.len(), 1);
		assert!(gallery.gallery[0].preview_url.starts_with("/preview/"));
		let mut poll = base.clone();
		poll["isSelfPost"] = json!(true);
		poll["poll"] = json!({"options":[{"id":"1","text":"Yes","voteCount":null}],"totalVoteCount":7,"votingEndsAt":"2026-10-29T20:02:09.527000+0000"});
		let poll = crate::utils::parse_post(&convert(poll)).await;
		assert_eq!(poll.poll.as_ref().unwrap().total_vote_count, 7);
		assert_eq!(poll.poll.as_ref().unwrap().poll_options[0].text, "Yes");
		let mut text = base;
		text["isSelfPost"] = json!(true);
		text["content"] = json!({"markdown":"<img src=x onerror=alert(1)>\n\n```\ncode\n```"});
		let text = crate::utils::parse_post(&convert(text)).await;
		assert!(!text.body.contains("<img src=x"));
		assert!(text.body.contains("<pre><code>code"));
	}

	#[test]
	fn test_graphql_comments_fail_closed_for_wrong_post_and_errors() {
		let request = CommentsRequest::from_path("/comments/abc.json").unwrap();
		assert!(request.post(&json!({"data":{"postsInfoByIds":[{"id":"t3_other"}]}}), false).is_err());
		let error = response_error(&json!({"errors":[{"extensions":{"code":"NOT_AUTHENTICATED"},"message":"untrusted"}]})).unwrap();
		assert_eq!(error.kind, ErrorKind::Session);
		assert!(!error.message.contains("untrusted"));
		let private = json!({"data":{"postsInfoByIds":[{"id":"t3_abc","subreddit":{"isQuarantined":true}}]}});
		assert_eq!(request.post(&private, false).unwrap_err().message, "quarantined");
		let cycle = json!({"data":{"postInfoById":{"id":"t3_abc","commentForest":{"trees":[{"parentId":"t1_b","node":{"id":"t1_a"}},{"parentId":"t1_a","node":{"id":"t1_b"}}]}}}});
		assert!(request.finish(json!({"data":{}}), &cycle).is_err());
	}
}
