//! Anonymous Android GraphQL operations for pages whose REST origin is unavailable.
//! Protocol names/hashes and response shapes cross-checked against Amqx/reddit-gql
//! and Nachtalb/redlib (AGPL-3.0), commit b6130fac7cb923ddccb9b8f11cf0a0ac63e1f0df.
use crate::reddit_graphql::{legacy_comment, legacy_post, listing, markdown, operation, response_error, timestamp, GraphqlError};
use base64::{
	engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD},
	Engine as _,
};
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashSet};
use std::hash::{Hash, Hasher};

const POSTS: &str = "0e471495bcba33554b6c540b3cf1cae23dd5432a8b741dddc7fd6717dc2520a7";
const SUBREDDIT: &str = "97046e2a48050f10a507db354a8da60955ffccb586dcf63163bdfc6610d5cf7a";
const POPULAR: &str = "728c0f9a4d12c17b8cffe07669bb1a8770b0f2ec0c84e040485f8fad2d9c417f";
const ALL: &str = "0e9c0045dd244e934efd0ea4fefc00cb0b4d91523e2f09c8af075856cb59b937";
const ABOUT: &str = "6b9c1679e69097e1c6df364adc11183afe6e2b6545dd1c0c1cc8f7490448c3e5";
const SEARCH: &str = "4acac653e5aac2ae2f08114d07ba7582e852464a57e324547c259ea3ef2ebd40";
const PROFILE: &str = "096ccf5d943485822cb6d3e0dc13f9977da1984514e1d539e1ed38a51158e689";
const PEOPLE: &str = "d9211ac33f72079f25cb4c8a8e00a2b4889e984f70278ec91283965c7856d33d";
const USER_POSTS: &str = "78a213c02a340db4a707d4bcd375ce34f733b75893379603acb4ec20388fafe6";
const USER_COMMENTS: &str = "e1b37f521fb40af2120946b70e7fbf6a3d924db2abaf28ff2164d4cfacca28db";
const COMMENTS: &str = "51ab9f2e0eb87b6dac9aac02012ba7046049e516ec55ec442abfed0f19c6f311";

#[derive(Debug, Clone)]
enum Step {
	Query(Value),
	PostIds(usize),
	CommentIds(usize),
}
#[derive(Debug, Clone)]
enum Page {
	Feed(&'static str),
	All,
	About,
	Search,
	Profile(String),
	UserPosts,
	UserComments,
	Overview {
		posts: Option<usize>,
		comments: Option<usize>,
		cursors: Vec<String>,
	},
	Multi {
		names: Vec<String>,
		indices: Vec<Option<usize>>,
		cursors: Vec<String>,
	},
}
#[derive(Debug, Clone)]
pub(crate) struct PageRequest {
	page: Page,
	steps: Vec<Step>,
	sort: String,
	limit: usize,
	quarantine: bool,
	invalid_cursor: bool,
	query: BTreeMap<String, String>,
}
fn invalid(reason: &'static str) -> GraphqlError {
	let mut error = GraphqlError::invalid(reason);
	error.message = "Reddit is temporarily unable to load this page. Retry in 30 seconds".into();
	error
}
fn name_ok(name: &str) -> bool {
	!name.is_empty() && name.len() <= 64 && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}
fn decode_cursor(cursor: &str, count: usize) -> Option<Vec<String>> {
	if cursor.is_empty() {
		return Some(vec![String::new(); count]);
	}
	if cursor.len() > 8192 {
		return None;
	}
	let bytes = URL_SAFE_NO_PAD.decode(cursor.strip_prefix("gql:")?).ok()?;
	let cursors: Vec<String> = serde_json::from_slice(&bytes).ok()?;
	(cursors.len() == count && cursors.iter().all(|s| s.len() <= 1024 && !s.chars().any(char::is_control))).then_some(cursors)
}
fn encode_cursor(cursors: &[String]) -> Option<String> {
	if cursors.iter().all(|s| s == "-") {
		return None;
	}
	let cursor = format!("gql:{}", URL_SAFE_NO_PAD.encode(serde_json::to_vec(cursors).ok()?));
	(cursor.len() <= 8192).then_some(cursor)
}
impl PageRequest {
	pub(crate) fn from_path(path: &str, quarantine: bool) -> Option<Self> {
		let (base, raw) = path.split_once('?').unwrap_or((path, ""));
		let base = percent_encoding::percent_decode_str(base).decode_utf8().ok()?;
		let parts: Vec<_> = base
			.trim_end_matches('/')
			.trim_end_matches(".json")
			.trim_end_matches('/')
			.split('/')
			.filter(|s| !s.is_empty())
			.collect();
		let query: BTreeMap<String, String> = url::form_urlencoded::parse(raw.as_bytes()).into_owned().collect();
		let mut request = Self {
			page: Page::About,
			steps: vec![],
			sort: query.get("sort").filter(|s| !s.is_empty()).cloned().unwrap_or_else(|| "new".into()),
			limit: query.get("limit").and_then(|s| s.parse::<usize>().ok()).unwrap_or(25).clamp(1, 100),
			quarantine,
			invalid_cursor: false,
			query,
		};
		let after = request.query.get("after").cloned().unwrap_or_default();
		match parts.as_slice() {
			["r", name, "about"] if name_ok(name) => {
				request.page = Page::About;
				request.push(
					"SubredditInfoByName",
					ABOUT,
					json!({"subredditName":name,"filterGated":!quarantine,"includeDevvitData":false,"includeRecapFields":false,"includeWelcomePage":false}),
				);
			}
			["r", name, rest @ ..] if rest.len() <= 1 && rest.first() != Some(&"search") && name.split('+').all(name_ok) => {
				request.sort = rest.first().filter(|s| !s.is_empty()).copied().unwrap_or("hot").into();
				if !matches!(request.sort.as_str(), "hot" | "new" | "top" | "rising" | "controversial" | "best") {
					return None;
				}
				if name.contains('+') {
					let names: Vec<_> = name
						.split('+')
						.map(str::to_ascii_lowercase)
						.collect::<std::collections::BTreeSet<_>>()
						.into_iter()
						.collect();
					// Keep the cursor bound independent of how many subscriptions a visitor stores.
					if names.len() > 64 {
						return None;
					}
					let cursors = decode_cursor(&after, names.len());
					request.invalid_cursor = cursors.is_none();
					let cursors = cursors.unwrap_or_else(|| vec![String::new(); names.len()]);
					let mut indices = Vec::new();
					for (name, cursor) in names.iter().zip(&cursors) {
						indices.push(if cursor == "-" {
							None
						} else {
							let index = request.steps.len();
							request.subreddit_feed(name, cursor);
							Some(index)
						});
					}
					request.page = Page::Multi { names, indices, cursors };
				} else if name.eq_ignore_ascii_case("popular") {
					request.page = Page::Feed("/data/popular/elements");
					request.push("PopularFeedElements", POPULAR, request.feed_variables(&after));
				} else if name.eq_ignore_ascii_case("all") {
					request.page = Page::All;
					let mut vars = request.feed_variables(&after);
					vars["time"] = vars["range"].take();
					vars.as_object_mut()?.remove("range");
					vars.as_object_mut()?.remove("pageSize");
					request.push("AllFeedSdui", ALL, vars);
					request.steps.push(Step::PostIds(0));
				} else {
					request.page = Page::Feed("/data/postFeed/elements");
					request.subreddit_feed(name, &after);
				}
			}
			["search"] | ["r", _, "search"] | ["subreddits", "search"] => {
				let pane = if parts[0] == "subreddits" { "communities" } else { "posts" };
				let mut text = request.query.get("q").cloned().unwrap_or_default();
				if let ["r", sub, "search"] = parts.as_slice() {
					if request.query.get("restrict_sr").is_some_and(|v| v != "off" && v != "0" && !v.is_empty()) {
						text = format!("{text} subreddit:{sub}");
					}
				}
				let nsfw = request.query.get("include_over_18").is_some_and(|v| v == "on" || v == "1" || v == "true");
				let mut filters = vec![json!({"key":"safe_search","value":if nsfw {"off"}else{"on"}})];
				if let Some(sort) = request.query.get("sort").filter(|s| !s.is_empty()) {
					filters.push(json!({"key":"sort","value":sort.to_uppercase()}));
				}
				if let Some(range) = request.query.get("t").filter(|s| !s.is_empty() && !matches!(request.sort.as_str(), "hot" | "new")) {
					filters.push(json!({"key":"time_range","value":range}));
				}
				let options = [
					("page_type", "search_results"),
					("pane", pane),
					("pref_allow_nsfw", if nsfw { "1" } else { "0" }),
					("is_eligible_for_kwf", "0"),
					("after", &after),
					("treatment", "default"),
				];
				let mut hasher = std::hash::DefaultHasher::new();
				text.hash(&mut hasher);
				let hash = hasher.finish();
				let query_id = uuid::Uuid::from_bytes(((u128::from(hash) << 64) | u128::from(hash.rotate_left(32))).to_le_bytes());
				request.page = Page::Search;
				request.push(
					"DynamicSearch",
					SEARCH,
					json!({"query":text,"queryId":query_id.to_string(),"filters":filters,
                    "options":options.iter().map(|(key,value)|json!({"key":key,"value":value})).collect::<Vec<_>>(),"includeSubredditUnit":true}),
				);
			}
			["user" | "u", name, "about"] if name_ok(name) => {
				request.page = Page::Profile((*name).into());
				request.push("ProfileDetailsByName", PROFILE, json!({"profileName":name}));
				request.push("SearchPeople", PEOPLE, json!({"query":name,"filters":[],"pageSize":5,"productSurface":""}));
			}
			["user" | "u", name, rest @ ..] if name_ok(name) && rest.len() <= 1 => match rest.first().copied().unwrap_or("overview") {
				"submitted" => {
					request.page = Page::UserPosts;
					request.user_posts(name, &after);
				}
				"comments" => {
					request.page = Page::UserComments;
					request.user_comments(name, &after);
				}
				"overview" | "" => {
					let cursors = decode_cursor(&after, 2);
					request.invalid_cursor = cursors.is_none();
					let cursors = cursors.unwrap_or_else(|| vec![String::new(); 2]);
					let posts = if cursors[0] == "-" {
						None
					} else {
						request.user_posts(name, &cursors[0]);
						Some(0)
					};
					let comments = if cursors[1] == "-" {
						None
					} else {
						let i = request.steps.len();
						request.user_comments(name, &cursors[1]);
						Some(i)
					};
					request.page = Page::Overview { posts, comments, cursors };
				}
				_ => return None,
			},
			_ => return None,
		}
		Some(request)
	}
	fn push(&mut self, name: &str, hash: &str, vars: Value) {
		self.steps.push(Step::Query(operation(name, hash, vars)));
	}
	fn feed_variables(&self, after: &str) -> Value {
		let mut vars = json!({"sort":self.sort.to_uppercase(),"pageSize":self.limit});
		if !after.is_empty() {
			vars["after"] = json!(STANDARD.encode(after));
		}
		if let Some(range) = self.query.get("t").filter(|s| !s.is_empty() && matches!(self.sort.as_str(), "top" | "controversial")) {
			vars["range"] = json!(range.to_uppercase());
		}
		vars
	}
	fn subreddit_feed(&mut self, name: &str, after: &str) {
		let mut vars = self.feed_variables(after);
		vars["subredditName"] = json!(name);
		vars["includeSubredditInPosts"] = json!(true);
		self.push("SubredditFeedElements", SUBREDDIT, vars);
	}
	fn user_posts(&mut self, name: &str, after: &str) {
		let mut vars = self.feed_variables(after);
		vars["username"] = json!(name);
		vars["includeSubredditInPosts"] = json!(true);
		if after.is_empty() {
			vars["after"] = Value::Null;
		}
		self.push("UserSubmittedPosts", USER_POSTS, vars);
	}
	fn user_comments(&mut self, name: &str, after: &str) {
		let mut vars = self.feed_variables(after);
		vars["name"] = json!(name);
		if after.is_empty() {
			vars["after"] = Value::Null;
		}
		let index = self.steps.len();
		self.push("SubmittedComments", USER_COMMENTS, vars);
		self.steps.push(Step::CommentIds(index));
	}
	/// Empty ID batches are resolved locally, without another wire request.
	pub(crate) fn next_payload(&self, values: &mut Vec<Value>) -> Result<Option<Value>, GraphqlError> {
		if self.invalid_cursor {
			return Err(invalid("invalid_page_cursor"));
		}
		loop {
			let Some(step) = self.steps.get(values.len()) else { return Ok(None) };
			let (name, hash, ids, key) = match step {
				Step::Query(payload) => return Ok(Some(payload.clone())),
				Step::PostIds(index) => {
					let edges = connection(&values[*index], "/data/allV3/elements")?["edges"]
						.as_array()
						.ok_or_else(|| invalid("missing_feed_edges"))?;
					let ids: Vec<_> = edges
						.iter()
						.filter(|e| e["node"]["adPayload"].is_null())
						.filter_map(|e| e["node"]["groupId"].as_str())
						.filter(|id| id.starts_with("t3_"))
						.map(str::to_string)
						.collect();
					("PostsByIds", POSTS, ids, "postsInfoByIds")
				}
				Step::CommentIds(index) => {
					let edges = connection(&values[*index], "/data/redditorInfoByName/comments")?["edges"]
						.as_array()
						.ok_or_else(|| invalid("missing_user_comments"))?;
					let ids: Vec<_> = edges.iter().filter_map(|e| e["node"]["id"].as_str()).map(str::to_string).collect();
					("LiveCommentsByIds", COMMENTS, ids, "commentsByIds")
				}
			};
			if ids.is_empty() {
				values.push(json!({"data":{key:[]}}));
				continue;
			}
			return Ok(Some(operation(
				name,
				hash,
				json!({"ids":ids,"includePremiumAvatarTreatment":false,"includePostContentPostHint":true,"includePostContentThumbnailEnabled":true}),
			)));
		}
	}
	pub(crate) fn finish(&self, values: &[Value]) -> Result<Value, GraphqlError> {
		let result = match &self.page {
			Page::Feed(pointer) => feed(connection(&values[0], pointer)?, self.quarantine)?,
			Page::All => {
				let posts = values[1]["data"]["postsInfoByIds"].as_array().ok_or_else(|| invalid("missing_posts"))?;
				listing(post_items(posts, self.quarantine)?, cursor(connection(&values[0], "/data/allV3/elements")?)?)
			}
			Page::About => subreddit(&values[0]["data"]["subredditInfoByName"], self.quarantine)?,
			Page::Search => {
				let main = connection(&values[0], "/data/search/dynamic/components/main")?;
				let edges = main["edges"].as_array().ok_or_else(|| invalid("missing_search_results"))?;
				let mut children = Vec::new();
				for item in edges.iter().flat_map(|e| e["node"]["children"].as_array().into_iter().flatten()) {
					match item["__typename"].as_str() {
						Some("SearchPost" | "SearchMediaPost") => children.extend(post_items(&[item["post"].clone()], self.quarantine)?),
						Some("SearchCommunity") => children.push(subreddit(&item["community"], self.quarantine)?),
						_ => {}
					}
				}
				listing(children, cursor(main)?)
			}
			Page::Profile(name) => {
				let profile = &values[0]["data"]["profileByName"];
				let actual = profile["name"].as_str().ok_or_else(|| GraphqlError::resource("User not found"))?;
				let karma = values[1]["data"]["search"]["general"]["authors"]["edges"]
					.as_array()
					.into_iter()
					.flatten()
					.find(|a| a["node"]["name"].as_str().is_some_and(|n| n.eq_ignore_ascii_case(name)))
					.map(|a| a["node"]["karma"]["total"].clone())
					.unwrap_or(Value::Null);
				json!({"kind":"t2","data":{"name":actual.strip_prefix("u_").unwrap_or(actual),"created":timestamp(&profile["createdAt"]),"total_karma":karma,
                    "subreddit":{"title":profile["title"],"icon_img":profile["styles"]["icon"],"banner_img":profile["styles"]["legacyBannerBackgroundImage"],
                        "public_description":profile["publicDescriptionText"],"over_18":profile["isNsfw"]==true}}})
			}
			Page::UserPosts => feed(connection(&values[0], "/data/postFeed/posts")?, self.quarantine)?,
			Page::UserComments => user_comments(&values[0], &values[1])?,
			Page::Overview { posts, comments, cursors } => {
				let mut sides = Vec::new();
				sides.push(
					posts
						.map(|i| feed(connection(&values[i], "/data/postFeed/posts")?, self.quarantine))
						.transpose()?
						.unwrap_or_else(|| listing(vec![], None)),
				);
				sides.push(
					comments
						.map(|i| user_comments(&values[i], &values[i + 1]))
						.transpose()?
						.unwrap_or_else(|| listing(vec![], None)),
				);
				merge(&sides, cursors, &self.sort, self.limit)?
			}
			Page::Multi { names, indices, cursors } => {
				debug_assert_eq!(names.len(), indices.len());
				let sides = indices
					.iter()
					.map(|index| {
						index
							.map(|i| feed(connection(&values[i], "/data/postFeed/elements")?, self.quarantine))
							.transpose()
							.map(|v| v.unwrap_or_else(|| listing(vec![], None)))
					})
					.collect::<Result<Vec<_>, _>>()?;
				merge(&sides, cursors, &self.sort, self.limit)?
			}
		};
		Ok(result)
	}
}
fn connection<'a>(value: &'a Value, pointer: &str) -> Result<&'a Value, GraphqlError> {
	if let Some(mut error) = response_error(value) {
		error.message = error.message.replace("comments", "page");
		return Err(error);
	}
	value.pointer(pointer).filter(|v| v.is_object()).ok_or_else(|| invalid("missing_page_connection"))
}
fn cursor(connection: &Value) -> Result<Option<String>, GraphqlError> {
	if connection["pageInfo"]["hasNextPage"] == false {
		return Ok(None);
	}
	let Some(end) = connection["pageInfo"]["endCursor"].as_str() else {
		return if connection["edges"].as_array().is_some_and(Vec::is_empty) {
			Ok(None)
		} else {
			Err(invalid("missing_page_cursor"))
		};
	};
	// Feed endpoints use base64 fullnames; search cursors are already opaque.
	Ok(Some(
		STANDARD
			.decode(end)
			.ok()
			.and_then(|v| String::from_utf8(v).ok())
			.filter(|s| s.starts_with("t3_") || s.starts_with("t1_"))
			.unwrap_or_else(|| end.into()),
	))
}
fn post_items(posts: &[Value], quarantine: bool) -> Result<Vec<Value>, GraphqlError> {
	let mut result = Vec::new();
	let mut seen = HashSet::new();
	for post in posts {
		if !matches!(post["__typename"].as_str(), Some("SubredditPost" | "ProfilePost")) {
			continue;
		}
		let Some(id) = post["id"].as_str().filter(|id| id.starts_with("t3_")) else {
			return Err(invalid("missing_post_id"));
		};
		if !seen.insert(id) {
			continue;
		}
		if post["subreddit"]["isQuarantined"] == true && !quarantine {
			return Err(GraphqlError::resource("quarantined"));
		}
		if post["subreddit"]["type"] == "PRIVATE" {
			return Err(GraphqlError::resource("private"));
		}
		result.push(json!({"kind":"t3","data":legacy_post(post,0)}));
	}
	Ok(result)
}
fn feed(connection: &Value, quarantine: bool) -> Result<Value, GraphqlError> {
	let edges = connection["edges"].as_array().ok_or_else(|| invalid("missing_feed_edges"))?;
	let posts: Vec<_> = edges.iter().filter(|e| e["node"]["adPayload"].is_null()).map(|e| e["node"].clone()).collect();
	Ok(listing(post_items(&posts, quarantine)?, cursor(connection)?))
}
fn subreddit(node: &Value, quarantine: bool) -> Result<Value, GraphqlError> {
	if node["__typename"] == "UnavailableSubreddit" {
		return Err(GraphqlError::resource(match node["forbiddenReason"].as_str() {
			Some("PRIVATE") => "private",
			Some("BANNED") => "banned",
			Some("QUARANTINED") => "quarantined",
			_ => "gated",
		}));
	}
	if node["isQuarantined"] == true && !quarantine {
		return Err(GraphqlError::resource("quarantined"));
	}
	let name = node["name"].as_str().ok_or_else(|| invalid("missing_subreddit"))?;
	Ok(
		json!({"kind":"t5","data":{"name":node["id"],"display_name":name,"display_name_prefixed":node["prefixedName"],
        "url":format!("/r/{name}/"),"title":node["title"],"public_description":node["publicDescriptionText"],
        "description":node["description"]["markdown"],"description_html":markdown(node["description"]["markdown"].as_str().unwrap_or_default(),&node["description"]),
        "community_icon":node["styles"]["icon"],"icon_img":node["styles"]["legacyIcon"]["url"],"subscribers":node["subscribersCount"],"accounts_active":node["activeCount"],
        "wiki_enabled":node["wikiEditMode"].as_str().is_some_and(|v|v!="DISABLED"),"over18":node["isNsfw"]==true}}),
	)
}
fn user_comments(previews: &Value, full: &Value) -> Result<Value, GraphqlError> {
	let conn = connection(previews, "/data/redditorInfoByName/comments")?;
	let edges = conn["edges"].as_array().ok_or_else(|| invalid("missing_user_comments"))?;
	let comments = full["data"]["commentsByIds"].as_array().ok_or_else(|| invalid("missing_comment_content"))?;
	let mut children = Vec::new();
	for edge in edges {
		let preview = &edge["node"];
		let node = comments.iter().find(|c| c["id"] == preview["id"]).ok_or_else(|| invalid("missing_comment_content"))?;
		let post_id = preview["postInfo"]["id"].as_str().ok_or_else(|| invalid("missing_comment_post"))?;
		let permalink = node["permalink"].as_str().ok_or_else(|| invalid("missing_comment_permalink"))?;
		let mut data = legacy_comment(node, post_id, post_id, &json!(permalink));
		data["link_title"] = preview["postInfo"]["title"].clone();
		let parts: Vec<_> = permalink.split('/').collect();
		data["subreddit"] = json!(if parts.get(1) == Some(&"r") {
			parts.get(2).copied().unwrap_or_default().to_string()
		} else {
			format!("u_{}", parts.get(2).copied().unwrap_or_default())
		});
		children.push(json!({"kind":"t1","data":data}));
	}
	Ok(listing(children, cursor(conn)?))
}
/// Consume only the next item of each ordered source. Unconsumed entries stay
/// behind that source's cursor, so interleaved pages neither skip nor repeat them.
fn merge(sides: &[Value], prior: &[String], sort: &str, limit: usize) -> Result<Value, GraphqlError> {
	let lists: Vec<_> = sides.iter().map(|s| s["data"]["children"].as_array().cloned().unwrap_or_default()).collect();
	let mut positions = vec![0; lists.len()];
	let mut cursors = prior.to_vec();
	let mut result = Vec::new();
	let mut seen = HashSet::new();
	let rank = |post: &Value| {
		let created = post["data"]["created_utc"].as_f64().unwrap_or_default();
		let score = post["data"]["score"].as_f64().unwrap_or_default();
		match sort {
			"top" | "controversial" => score,
			"hot" | "best" | "rising" => score.signum() * score.abs().max(1.0).log10() + created / 45000.0,
			_ => created,
		}
	};
	while result.len() < limit {
		let next = (0..lists.len())
			.filter(|&i| positions[i] < lists[i].len())
			.max_by(|&a, &b| rank(&lists[a][positions[a]]).total_cmp(&rank(&lists[b][positions[b]])));
		let Some(i) = next else { break };
		let post = lists[i][positions[i]].clone();
		positions[i] += 1;
		let id = post["data"]["name"].as_str().ok_or_else(|| invalid("missing_merge_cursor"))?;
		cursors[i] = id.into();
		if seen.insert(id.to_string()) {
			result.push(post);
		}
	}
	for i in 0..lists.len() {
		if positions[i] == lists[i].len() {
			cursors[i] = sides[i]["data"]["after"].as_str().unwrap_or("-").into();
		}
	}
	Ok(listing(result, encode_cursor(&cursors)))
}

#[cfg(test)]
#[path = "reddit_graphql_pages_tests.rs"]
mod tests;
