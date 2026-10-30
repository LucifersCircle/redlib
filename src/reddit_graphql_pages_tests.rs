use super::*;

// Exercise the real request planner and converter against fourteen independently
// paginated communities. Skyrim deliberately dominates every score/age comparison.
fn fourteen_community_page(path: &str) -> Value {
	fourteen_community_page_with_pins(path, false)
}
fn fourteen_community_page_with_pins(path: &str, old_pins: bool) -> Value {
	let request = PageRequest::from_path(path, false).unwrap();
	let mut values = Vec::new();
	while let Some(payload) = request.next_payload(&mut values).unwrap() {
		assert_eq!(payload["operationName"], "SubredditFeedElements");
		let vars = &payload["variables"];
		let name = vars["subredditName"].as_str().unwrap();
		let start = vars["after"]
			.as_str()
			.map(|s| {
				let id = String::from_utf8(STANDARD.decode(s).unwrap()).unwrap();
				id.strip_prefix(&format!("t3_{name}")).unwrap().parse::<usize>().unwrap() + 1
			})
			.unwrap_or(0);
		let end = (start + vars["pageSize"].as_u64().unwrap() as usize).min(100);
		let posts = (start..end)
			.map(|index| {
				let mut value = post(&format!("{name}{index}"), 1);
				value["subreddit"]["name"] = json!(name);
				value["score"] = json!(if name == "skyrim" { 100_000 } else { 1 });
				value["createdAt"] = json!(if name == "skyrim" { "2026-10-30T00:00:00Z" } else { "2026-01-01T00:00:00Z" });
				if old_pins && index < 2 {
					value["isStickied"] = json!(true);
					value["createdAt"] = json!("2026-01-01T00:00:00Z");
					value["score"] = json!(1_000_000);
				}
				value
			})
			.collect();
		let after = (end < 100).then(|| format!("t3_{name}{}", end - 1));
		values.push(feed_value(posts, after.as_deref()));
	}
	request.finish_at(&values, pin_test_now()).unwrap()
}

#[test]
fn test_graphql_pages_subscription_old_pins_do_not_take_page_slots_or_repeat() {
	let mut cursor = None;
	let mut ids = HashSet::new();
	for _ in 0..3 {
		let page = fourteen_community_page_with_pins(&fourteen_community_path("hot", 25, cursor.as_deref()), true);
		let children = page["data"]["children"].as_array().unwrap();
		assert_eq!(children.len(), 25);
		let mut counts = BTreeMap::<String, usize>::new();
		for item in children {
			assert_ne!(item["data"]["stickied"], true, "old pins must not occupy a home-feed slot");
			*counts.entry(item["data"]["subreddit"].as_str().unwrap().into()).or_default() += 1;
			assert!(ids.insert(item["data"]["name"].as_str().unwrap().to_string()), "post repeated across pages");
		}
		assert_eq!(counts.len(), 14);
		assert!(counts.values().all(|&count| count <= 2));
		cursor = page["data"]["after"].as_str().map(str::to_string);
	}
	assert_eq!(ids.len(), 75);
}
fn fourteen_community_path(sort: &str, limit: usize, after: Option<&str>) -> String {
	let names = (0..13).map(|i| format!("community{i:02}")).chain(std::iter::once("skyrim".into())).collect::<Vec<_>>();
	let mut query = url::form_urlencoded::Serializer::new(String::new());
	query.append_pair("limit", &limit.to_string());
	if let Some(after) = after {
		query.append_pair("after", after);
	}
	format!("/r/{}/{sort}.json?{}", names.join("%2B"), query.finish())
}
#[test]
fn test_graphql_pages_subscription_hot_balances_fourteen_communities_across_three_pages() {
	let mut cursor = None;
	let mut total = BTreeMap::<String, usize>::new();
	let mut ids = HashSet::new();
	for _ in 0..3 {
		let page = fourteen_community_page(&fourteen_community_path("hot", 25, cursor.as_deref()));
		let mut counts = BTreeMap::<String, usize>::new();
		let children = page["data"]["children"].as_array().unwrap();
		assert_eq!(children.len(), 25);
		for item in children {
			let sub = item["data"]["subreddit"].as_str().unwrap().to_string();
			*counts.entry(sub.clone()).or_default() += 1;
			*total.entry(sub).or_default() += 1;
			assert!(ids.insert(item["data"]["name"].as_str().unwrap().to_string()), "post repeated across pages");
		}
		assert_eq!(counts.len(), 14, "one community must not monopolize a home-feed page");
		assert!(counts.values().all(|&count| count <= 2));
		cursor = page["data"]["after"].as_str().map(str::to_string);
	}
	assert_eq!(ids.len(), 75);
	assert!(total.values().max().unwrap() - total.values().min().unwrap() <= 1);
}

#[test]
fn test_graphql_pages_subscription_small_pages_do_not_starve_other_communities() {
	for sort in ["hot", "best", "rising"] {
		for limit in [1, 5] {
			let mut cursor = None;
			let mut counts = BTreeMap::<String, usize>::new();
			let mut ids = HashSet::new();
			for _ in 0..14usize.div_ceil(limit) {
				let page = fourteen_community_page(&fourteen_community_path(sort, limit, cursor.as_deref()));
				for item in page["data"]["children"].as_array().unwrap() {
					*counts.entry(item["data"]["subreddit"].as_str().unwrap().to_string()).or_default() += 1;
					assert!(ids.insert(item["data"]["name"].as_str().unwrap().to_string()));
				}
				cursor = page["data"]["after"].as_str().map(str::to_string);
			}
			assert_eq!(counts.len(), 14, "{sort} limit={limit} must visit every community");
			assert!(counts.values().max().unwrap() - counts.values().min().unwrap() <= 1);
		}
	}
}

#[test]
fn test_graphql_pages_subscription_new_and_top_keep_global_order() {
	for sort in ["new", "top"] {
		let page = fourteen_community_page(&fourteen_community_path(sort, 25, None));
		let children = page["data"]["children"].as_array().unwrap();
		assert_eq!(children.len(), 25);
		assert!(children.iter().all(|item| item["data"]["subreddit"] == "skyrim"));
		assert!(decode_cursor(page["data"]["after"].as_str().unwrap(), 14).is_some());
	}
}

#[test]
fn test_graphql_pages_subscription_balancing_accepts_existing_page_links() {
	let names = (0..13).map(|i| format!("community{i:02}")).chain(std::iter::once("skyrim".into()));
	let legacy = encode_cursor(&names.map(|name| format!("t3_{name}2")).collect::<Vec<_>>()).unwrap();
	let page = fourteen_community_page(&fourteen_community_path("hot", 25, Some(&legacy)));
	let mut counts = BTreeMap::<String, usize>::new();
	for item in page["data"]["children"].as_array().unwrap() {
		let name = item["data"]["subreddit"].as_str().unwrap();
		let index = item["data"]["name"]
			.as_str()
			.unwrap()
			.strip_prefix(&format!("t3_{name}"))
			.unwrap()
			.parse::<usize>()
			.unwrap();
		assert!(index >= 3, "legacy cursor must not repeat earlier posts");
		*counts.entry(name.into()).or_default() += 1;
	}
	assert_eq!(counts.len(), 14);
	assert!(counts.values().all(|&count| count <= 2));
}

#[test]
fn test_graphql_pages_subscription_empty_exhausted_and_duplicate_sources_fill_page() {
	let request = PageRequest::from_path("/r/alpha%2Bbeta%2Bgamma/hot.json?limit=5", false).unwrap();
	let page = request
		.finish(&[
			feed_value(vec![], None),
			feed_value(vec![post("shared", 1)], None),
			feed_value(
				std::iter::once(post("shared", 1)).chain((0..8).map(|i| post(&format!("gamma{i}"), 1))).collect(),
				Some("t3_gamma7"),
			),
		])
		.unwrap();
	let children = page["data"]["children"].as_array().unwrap();
	assert_eq!(children.len(), 5);
	let ids: HashSet<_> = children.iter().map(|v| v["data"]["name"].as_str().unwrap()).collect();
	assert_eq!(ids.len(), 5);
	let cursor = page["data"]["after"].as_str().unwrap();
	let (cursors, _) = decode_subscription_cursor(cursor, 3).unwrap();
	assert_eq!(&cursors[..2], ["-", "-"]);
	assert_eq!(cursors[2], children.last().unwrap()["data"]["name"].as_str().unwrap());
	let next = PageRequest::from_path(&format!("/r/alpha%2Bbeta%2Bgamma/hot.json?after={cursor}"), false).unwrap();
	assert_eq!(next.next_payload(&mut vec![]).unwrap().unwrap()["variables"]["subredditName"], "gamma");
	let last = next.finish(&[feed_value(vec![post("last", 1)], None)]).unwrap();
	assert!(last["data"]["after"].is_null());
}

#[test]
fn test_graphql_pages_subscription_cursor_validates_counts_and_avoids_overflow() {
	for state in [
		json!({"cursors":vec!["";14],"served":[0]}),
		json!({"cursors":vec!["";14],"served":vec![-1;14]}),
		json!({"cursors":vec!["\n";14],"served":vec![0;14]}),
	] {
		let cursor = format!("gql:{}", URL_SAFE_NO_PAD.encode(state.to_string()));
		let request = PageRequest::from_path(&fourteen_community_path("hot", 25, Some(&cursor)), false).unwrap();
		assert!(request.next_payload(&mut vec![]).is_err());
	}
	let cursor = encode_subscription_cursor(&vec![String::new(); 14], &[u64::MAX; 14]).unwrap();
	let page = fourteen_community_page(&fourteen_community_path("hot", 25, Some(&cursor)));
	let mut counts = BTreeMap::<String, usize>::new();
	for item in page["data"]["children"].as_array().unwrap() {
		*counts.entry(item["data"]["subreddit"].as_str().unwrap().into()).or_default() += 1;
	}
	assert_eq!(counts.len(), 14);
	assert!(counts.values().all(|&count| count <= 2));
}

fn post(id: &str, time: u64) -> Value {
	json!({"__typename":"SubredditPost","id":format!("t3_{id}"),"title":format!("Post {id}"),"authorInfo":{"name":"fixture"},
        "subreddit":{"name":"fixture","type":"PUBLIC"},"createdAt":format!("2026-10-29T00:00:{time:02}Z"),
        "url":"https://example.com/","permalink":format!("/r/fixture/comments/{id}/fixture/"),"score":time,"content":{"markdown":"Safe **text**"}})
}
fn conn(posts: Vec<Value>, after: Option<&str>) -> Value {
	json!({"edges":posts.into_iter().map(|node|json!({"node":node})).collect::<Vec<_>>(),
        "pageInfo":{"hasNextPage":after.is_some(),"endCursor":after.map(|v|STANDARD.encode(v))}})
}
fn feed_value(posts: Vec<Value>, after: Option<&str>) -> Value {
	json!({"data":{"postFeed":{"elements":conn(posts,after)}}})
}

fn pin_test_now() -> i64 {
	timestamp(&json!("2026-10-30T12:00:00Z")) as i64
}
fn pinned_post(id: &str, age: i64) -> Value {
	let mut value = post(id, 1);
	value["isStickied"] = json!(true);
	value["createdAt"] = json!(chrono::DateTime::from_timestamp(pin_test_now() - age, 0).unwrap().to_rfc3339());
	value
}

#[test]
fn test_graphql_pages_feed_pin_age_uses_creation_time_and_24_hour_boundary() {
	let mut old = pinned_post("old", 86_401);
	old["editedAt"] = json!("2026-10-30T12:00:00Z");
	let mut missing_time = pinned_post("missing", 0);
	missing_time.as_object_mut().unwrap().remove("createdAt");
	let mut invalid_time = pinned_post("invalid", 0);
	invalid_time["createdAt"] = json!("invalid");
	let posts = vec![
		old,
		pinned_post("boundary", 86_400),
		pinned_post("recent", 1),
		pinned_post("now", 0),
		missing_time,
		invalid_time,
		pinned_post("future", -1),
		post("ordinary", 1),
	];
	for sort in ["hot", "best", "rising", "new", "top", "controversial"] {
		for name in ["fixture", "popular"] {
			let request = PageRequest::from_path(&format!("/r/{name}/{sort}.json"), false).unwrap();
			let value = if name == "popular" {
				json!({"data":{"popular":{"elements":conn(posts.iter().map(|p|json!({"postInfo":p})).collect(),Some("t3_ordinary"))}}})
			} else {
				feed_value(posts.clone(), Some("t3_ordinary"))
			};
			let page = request.finish_at(&[value], pin_test_now()).unwrap();
			let ids: Vec<_> = page["data"]["children"].as_array().unwrap().iter().map(|p| p["data"]["name"].as_str().unwrap()).collect();
			assert_eq!(ids, ["t3_boundary", "t3_recent", "t3_now", "t3_ordinary"], "{name}/{sort}");
			assert_eq!(page["data"]["after"], "t3_ordinary");
			assert_eq!(page["data"]["children"][0]["data"]["stickied"], true);
		}
	}
}

#[test]
fn test_graphql_pages_all_and_filtered_only_feeds_keep_pagination() {
	let old = pinned_post("old", 86_401);
	let request = PageRequest::from_path("/r/all/hot.json", false).unwrap();
	let page = request
		.finish_at(
			&[
				json!({"data":{"allV3":{"elements":conn(vec![json!({"groupId":"t3_old"}),json!({"groupId":"t3_recent"})],Some("all-next"))}}}),
				json!({"data":{"postsInfoByIds":[old.clone(),pinned_post("recent",1)]}}),
			],
			pin_test_now(),
		)
		.unwrap();
	assert_eq!(page["data"]["children"].as_array().unwrap().len(), 1);
	assert_eq!(page["data"]["children"][0]["data"]["name"], "t3_recent");
	assert_eq!(page["data"]["after"], "all-next");
	let request = PageRequest::from_path("/r/fixture/hot.json", false).unwrap();
	for after in [Some("t3_old"), None] {
		let page = request.finish_at(&[feed_value(vec![old.clone()], after)], pin_test_now()).unwrap();
		assert_eq!(page["data"]["children"], json!([]), "filtered pins are a valid empty page");
		assert_eq!(page["data"]["after"], json!(after));
	}
}

#[test]
fn test_graphql_pages_filtered_pin_sources_advance_without_spending_a_turn() {
	for sort in ["hot", "best", "rising", "new", "top", "controversial"] {
		let request = PageRequest::from_path(&format!("/r/alpha%2Bbeta/{sort}.json?limit=1"), false).unwrap();
		let page = request
			.finish_at(
				&[feed_value(vec![pinned_post("old", 86_401)], Some("t3_old")), feed_value(vec![post("normal", 1)], None)],
				pin_test_now(),
			)
			.unwrap();
		assert_eq!(page["data"]["children"][0]["data"]["name"], "t3_normal");
		let after = page["data"]["after"].as_str().unwrap();
		let balanced = matches!(sort, "hot" | "best" | "rising");
		if balanced {
			let (cursors, served) = decode_subscription_cursor(after, 2).unwrap();
			assert_eq!(cursors, ["t3_old", "-"]);
			assert_eq!(served, [0, 1]);
		} else {
			assert_eq!(decode_cursor(after, 2).unwrap(), ["t3_old", "-"]);
		}
		let next = PageRequest::from_path(&format!("/r/alpha%2Bbeta/{sort}.json?limit=1&after={after}"), false).unwrap();
		let mut values = Vec::new();
		let payload = next.next_payload(&mut values).unwrap().unwrap();
		assert_eq!(payload["variables"]["subredditName"], "alpha");
		assert_eq!(payload["variables"]["after"], STANDARD.encode("t3_old"));
		values.push(feed_value(vec![post("next", 1)], None));
		assert!(next.next_payload(&mut values).unwrap().is_none());
		let page = next.finish_at(&values, pin_test_now()).unwrap();
		assert_eq!(page["data"]["children"][0]["data"]["name"], "t3_next");
		assert!(page["data"]["after"].is_null());
	}
}

#[test]
fn test_graphql_pages_old_pins_remain_in_search_user_history_and_post_data() {
	let old = pinned_post("old", 86_401);
	assert_eq!(legacy_post(&old, 0)["stickied"], true);
	let request = PageRequest::from_path("/user/fixture/submitted.json", false).unwrap();
	let page = request
		.finish_at(&[json!({"data":{"postFeed":{"posts":conn(vec![old.clone()],None)}}})], pin_test_now())
		.unwrap();
	assert_eq!(page["data"]["children"][0]["data"]["name"], "t3_old");
	let request = PageRequest::from_path("/search.json?q=old", false).unwrap();
	let page = request
		.finish_at(
			&[json!({"data":{"search":{"dynamic":{"components":{"main":conn(vec![json!({"children":[{"__typename":"SearchPost","post":old}]})],None)}}}}})],
			pin_test_now(),
		)
		.unwrap();
	assert_eq!(page["data"]["children"][0]["data"]["name"], "t3_old");
}

#[tokio::test]
async fn test_graphql_pages_cached_feed_pins_expire_when_rendered() {
	let mut posts = Vec::new();
	for value in [pinned_post("boundary", 86_400), pinned_post("recent", 1), post("ordinary", 1)] {
		posts.push(crate::utils::parse_post(&json!({"data":legacy_post(&value,0)})).await);
	}
	let mut alias = legacy_post(&pinned_post("alias", 86_401), 0);
	alias["stickied"] = json!(false);
	alias["pinned"] = json!(true);
	posts.push(crate::utils::parse_post(&json!({"data":alias})).await);
	crate::utils::filter_feed_pins(&mut posts, pin_test_now());
	assert_eq!(posts.iter().map(|p| p.id.as_str()).collect::<Vec<_>>(), ["boundary", "recent", "ordinary"]);
	crate::utils::filter_feed_pins(&mut posts, pin_test_now() + 1);
	assert_eq!(posts.iter().map(|p| p.id.as_str()).collect::<Vec<_>>(), ["recent", "ordinary"]);
	crate::utils::filter_feed_pins(&mut posts, pin_test_now() + 86_400);
	assert_eq!(posts.iter().map(|p| p.id.as_str()).collect::<Vec<_>>(), ["ordinary"]);
}

#[test]
fn test_graphql_pages_routes_keep_rest_resources_and_comments_separate() {
	for path in [
		"/r/popular/.json",
		"/r/all/hot.json",
		"/r/rust/top.json?t=week",
		"/r/rust/about.json",
		"/search.json?q=rust",
		"/r/rust/search.json?q=test&restrict_sr=on",
		"/subreddits/search.json?q=rust",
		"/user/reddit/about.json",
		"/user/reddit/overview.json",
		"/user/reddit/submitted.json",
		"/user/reddit/comments.json",
		"/r/rust%2Bgaming/new.json",
	] {
		assert!(PageRequest::from_path(path, false).is_some(), "{path}");
	}
	for path in [
		"/r/rust/comments/abc/test.json",
		"/comments/abc.json",
		"/r/rust/wiki/index.json",
		"/duplicates/abc.json",
		"/api/test.json",
	] {
		assert!(PageRequest::from_path(path, false).is_none(), "{path}");
	}
}
#[test]
fn test_graphql_pages_preserve_sort_time_cursors_and_search_scope() {
	let request = PageRequest::from_path("/r/rust/top.json?t=week&limit=7&after=t3_before", false).unwrap();
	let payload = request.next_payload(&mut vec![]).unwrap().unwrap();
	assert_eq!(payload["variables"]["range"], "WEEK");
	assert_eq!(payload["variables"]["pageSize"], 7);
	assert_eq!(payload["variables"]["after"], STANDARD.encode("t3_before"));
	let path = "/r/rust/search.json?q=hello+world&restrict_sr=on&sort=top&t=year";
	let a = PageRequest::from_path(path, false).unwrap().next_payload(&mut vec![]).unwrap().unwrap();
	let b = PageRequest::from_path(&format!("{path}&after=opaque"), false)
		.unwrap()
		.next_payload(&mut vec![])
		.unwrap()
		.unwrap();
	assert_eq!(a["operationName"], "DynamicSearch");
	assert_eq!(a["variables"]["query"], "hello world subreddit:rust");
	assert_eq!(a["variables"]["queryId"], b["variables"]["queryId"]);
	assert!(a["variables"]["filters"].as_array().unwrap().contains(&json!({"key":"time_range","value":"year"})));
}
#[test]
fn test_graphql_pages_convert_feed_and_reject_changed_schema() {
	let request = PageRequest::from_path("/r/rust/new.json", false).unwrap();
	let page = request.finish(&[feed_value(vec![post("a", 1), post("b", 2)], Some("t3_b"))]).unwrap();
	assert_eq!(page["data"]["after"], "t3_b");
	assert_eq!(page["data"]["children"][0]["data"]["title"], "Post a");
	assert!(page["data"]["children"][0]["data"]["selftext_html"].as_str().unwrap().contains("<strong>"));
	assert!(request.finish(&[json!({"data":{"postFeed":{"elements":{"unexpected":[]}}}})]).is_err());
	assert!(request
		.finish(&[json!({"errors":[{"extensions":{"code":"DEPENDENCY_FAILURE"},"message":"private upstream details"}]})])
		.is_err());
	let mut gated = post("g", 1);
	gated["subreddit"]["isQuarantined"] = json!(true);
	assert_eq!(request.finish(&[feed_value(vec![gated.clone()], None)]).unwrap_err().message, "quarantined");
	assert!(PageRequest::from_path("/r/rust/new.json", true).unwrap().finish(&[feed_value(vec![gated], None)]).is_ok());
}
#[test]
fn test_graphql_pages_empty_all_and_user_comments_skip_followup_sends() {
	for (path, value) in [
		("/r/all/hot.json", json!({"data":{"allV3":{"elements":conn(vec![],None)}}})),
		("/user/fixture/comments.json", json!({"data":{"redditorInfoByName":{"comments":conn(vec![],None)}}})),
	] {
		let request = PageRequest::from_path(path, false).unwrap();
		let mut values = vec![value];
		assert!(request.next_payload(&mut values).unwrap().is_none());
		assert_eq!(values.len(), 2);
		assert_eq!(request.finish(&values).unwrap()["data"]["children"], json!([]));
	}
}
#[test]
fn test_graphql_pages_user_comments_fill_body_and_post_links() {
	let request = PageRequest::from_path("/user/fixture/comments.json", false).unwrap();
	let preview = json!({"id":"t1_reply","postInfo":{"id":"t3_post","title":"Post title"}});
	let values = vec![
		json!({"data":{"redditorInfoByName":{"comments":conn(vec![preview],Some("t1_reply"))}}}),
		json!({"data":{"commentsByIds":[{"id":"t1_reply","content":{"markdown":"Comment **body**"},"permalink":"/r/rust/comments/post/title/reply/","authorInfo":{"name":"fixture"}}]}}),
	];
	let page = request.finish(&values).unwrap();
	let item = &page["data"]["children"][0]["data"];
	assert_eq!(item["body"], "Comment **body**");
	assert_eq!(item["link_title"], "Post title");
	assert_eq!(item["subreddit"], "rust");
	assert_eq!(page["data"]["after"], "t1_reply");
	assert!(request.finish(&[values[0].clone(), json!({"data":{"commentsByIds":[]}})]).is_err());
}
#[test]
fn test_graphql_pages_subscription_merge_keeps_unconsumed_entries_for_next_page() {
	let request = PageRequest::from_path("/r/alpha%2Bbeta/new.json?limit=2", false).unwrap();
	let page = request
		.finish(&[
			feed_value(vec![post("a", 30), post("b", 10)], Some("t3_b")),
			feed_value(vec![post("c", 20), post("d", 5)], None),
		])
		.unwrap();
	let names: Vec<_> = page["data"]["children"].as_array().unwrap().iter().map(|p| p["data"]["name"].as_str().unwrap()).collect();
	assert_eq!(names, ["t3_a", "t3_c"]);
	let cursor = page["data"]["after"].as_str().unwrap();
	assert_eq!(decode_cursor(cursor, 2).unwrap(), ["t3_a", "t3_c"]);
	let next = PageRequest::from_path(&format!("/r/alpha%2Bbeta/new.json?limit=2&after={cursor}"), false).unwrap();
	let payload = next.next_payload(&mut vec![]).unwrap().unwrap();
	assert_eq!(payload["variables"]["after"], STANDARD.encode("t3_a"));
	let page = next.finish(&[feed_value(vec![post("b", 10)], None), feed_value(vec![post("d", 5)], None)]).unwrap();
	assert!(page["data"]["after"].is_null());
	let bad = PageRequest::from_path("/r/alpha%2Bbeta/new.json?after=gql:garbage", false).unwrap();
	assert!(bad.next_payload(&mut vec![]).is_err());
}
#[test]
fn test_graphql_pages_overview_preserves_independent_posts_and_comments_cursors() {
	let prior = encode_cursor(&["-".into(), "t1_before".into()]).unwrap();
	let request = PageRequest::from_path(&format!("/user/fixture/overview.json?after={prior}"), false).unwrap();
	let payload = request.next_payload(&mut vec![]).unwrap().unwrap();
	assert_eq!(payload["operationName"], "SubmittedComments");
	assert_eq!(payload["variables"]["after"], STANDARD.encode("t1_before"));
	let empty = json!({"data":{"redditorInfoByName":{"comments":conn(vec![],None)}}});
	let mut values = vec![empty];
	assert!(request.next_payload(&mut values).unwrap().is_none());
	let page = request.finish(&values).unwrap();
	assert!(page["data"]["after"].is_null());
}
#[test]
fn test_graphql_pages_search_and_metadata_keep_resource_restrictions() {
	let request = PageRequest::from_path("/r/private/about.json", false).unwrap();
	assert_eq!(
		request
			.finish(&[json!({"data":{"subredditInfoByName":{"__typename":"UnavailableSubreddit","forbiddenReason":"PRIVATE"}}})])
			.unwrap_err()
			.message,
		"private"
	);
	let search = PageRequest::from_path("/search.json?q=hello", false).unwrap();
	let page = search
		.finish(&[json!({"data":{"search":{"dynamic":{"components":{"main":{
				"edges":[{"node":{"children":[{"__typename":"SearchAdPost"},{"__typename":"SearchPost","post":post("x",1)}]}}],
				"pageInfo":{"hasNextPage":true,"endCursor":"opaque-search-cursor"}
		}}}}}})])
		.unwrap();
	assert_eq!(page["data"]["children"].as_array().unwrap().len(), 1);
	assert_eq!(page["data"]["after"], "opaque-search-cursor");
}

#[test]
fn test_graphql_pages_popular_feed_unwraps_post_info_envelopes() {
	let request = PageRequest::from_path("/r/popular/hot.json", false).unwrap();
	let page = request
		.finish(&[json!({"data":{"popular":{"elements":conn(vec![json!({"__typename":"PostFeedElement","postInfo":post("wrapped",1)})],Some("t3_wrapped"))}}})])
		.unwrap();
	assert_eq!(page["data"]["children"][0]["data"]["name"], "t3_wrapped");
	assert_eq!(page["data"]["after"], "t3_wrapped");
}

#[test]
fn test_graphql_pages_changed_post_envelope_is_an_error_not_an_empty_page() {
	let request = PageRequest::from_path("/r/rust/new.json", false).unwrap();
	assert!(request.finish(&[feed_value(vec![json!({"unexpectedPostEnvelope":post("a",1)})], None)]).is_err());
	let cursor = STANDARD.encode("opaque cursor containing rank metadata");
	assert_eq!(
		super::cursor(&json!({"edges":[{}],"pageInfo":{"hasNextPage":true,"endCursor":cursor}})).unwrap(),
		Some("opaque cursor containing rank metadata".into())
	);
}

#[test]
fn test_graphql_pages_community_summary_respects_limit() {
	let request = PageRequest::from_path("/subreddits/search.json?q=rust&limit=1", false).unwrap();
	let results = ["alpha", "beta"].map(|name| json!({"__typename":"SearchCommunity","community":{"name":name}}));
	let page = request
		.finish(&[json!({"data":{"search":{"dynamic":{"components":{"main":{
				"edges":[{"node":{"children":results}}],"pageInfo":{"hasNextPage":false}
		}}}}}})])
		.unwrap();
	assert_eq!(page["data"]["children"].as_array().unwrap().len(), 1);
}
