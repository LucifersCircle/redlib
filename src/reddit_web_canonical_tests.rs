use super::*;

#[test]
fn test_reddit_web_canonical_page_accepts_same_post_slug_changes() {
	let spec = Spec::parse("/r/rust/comments/1y0ubfx/_?sort=top&context=7", false).unwrap();
	let current = "/r/rust/comments/1y0ubfx/_/?sort=top&context=7";
	let target = "https://www.reddit.com/r/rust/comments/1y0ubfx/breaking_1000_rust_crates_for_security/";
	assert_eq!(
		spec.canonical_page(current, target, Some("rust")).unwrap().as_deref(),
		Some("/r/rust/comments/1y0ubfx/breaking_1000_rust_crates_for_security/?sort=top&context=7")
	);

	let current = "/r/rust/comments/1y0ubfx/old_title/?sort=top&context=7";
	assert_eq!(
		spec.canonical_page(current, "/r/rust/comments/1y0ubfx/new_title/", Some("rust")).unwrap().as_deref(),
		Some("/r/rust/comments/1y0ubfx/new_title/?sort=top&context=7")
	);
}

#[test]
fn test_reddit_web_canonical_page_preserves_focus_when_target_drops_it() {
	let spec = Spec::parse("/r/rust/comments/1y0ubfx/_/t1focus?sort=top&context=7", false).unwrap();
	let current = "/r/rust/comments/1y0ubfx/_/t1focus/?sort=top&context=7";
	assert_eq!(
		spec.canonical_page(current, "/r/rust/comments/1y0ubfx/canonical_title/", Some("rust")).unwrap().as_deref(),
		Some("/r/rust/comments/1y0ubfx/canonical_title/t1focus/?sort=top&context=7")
	);
}

#[test]
fn test_reddit_web_canonical_page_rejects_identity_focus_origin_and_query_changes() {
	let spec = Spec::parse("/r/rust/comments/1y0ubfx/_/t1focus?sort=top&context=7", false).unwrap();
	let current = "/r/rust/comments/1y0ubfx/_/t1focus/?sort=top&context=7";
	for target in [
		"/r/other/comments/1y0ubfx/canonical_title/",
		"/r/rust/comments/1y0ubfy/canonical_title/",
		"/r/rust/comments/1y0ubfx/canonical_title/t1other/",
		"https://evil.test/r/rust/comments/1y0ubfx/canonical_title/",
		"https://old.reddit.com/r/rust/comments/1y0ubfx/canonical_title/",
		"/r/rust/comments/1y0ubfx/canonical_title/?context=8",
		"/r/rust/comments/1y0ubfx/canonical_title/?next=evil",
	] {
		assert!(spec.canonical_page(current, target, Some("rust")).is_err(), "{target}");
	}
}

#[test]
fn test_reddit_web_canonical_page_keeps_feed_case_and_sort_checks() {
	let spec = Spec::parse("/r/askreddit/hot.json", false).unwrap();
	assert_eq!(
		spec
			.canonical_page("/r/askreddit/hot/?t=all&limit=25", "/r/AskReddit/hot/", Some("askreddit"))
			.unwrap()
			.as_deref(),
		Some("/r/AskReddit/hot/?t=all&limit=25")
	);
	assert!(spec.canonical_page("/r/askreddit/hot/?t=all&limit=25", "/r/AskReddit/new/", Some("askreddit")).is_err());
	assert!(spec.canonical_page("/r/askreddit/hot/?t=all&limit=25", "/r/elsewhere/hot/", Some("askreddit")).is_err());
}
