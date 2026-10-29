use super::*;

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
