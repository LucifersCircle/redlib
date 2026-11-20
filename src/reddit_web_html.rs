//! Converts anonymous Reddit HTML into the JSON shapes consumed by Redlib.
//!
//! DOM contracts cross-checked against AGPL-3.0 references:
//! - 0xMassi/webclaw, b8fea1632d715dbc374be35fa339aa586f658cdb,
//!   crates/webclaw-core/src/reddit.rs (full post/comment attributes).
//! - Nachtalb/redlib, 856c6e0, src/backends/scraped.rs (comment body ownership
//!   and same-post shreddit fragment paths). This module never enriches via API.
use super::WebError;
use chrono::DateTime;
use scraper::{ElementRef, Html, Selector};
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::sync::LazyLock;

#[derive(Debug)]
pub(super) struct Document {
	pub posts: Vec<Value>,
	pub comments: Vec<Value>,
	pub next: Option<String>,
	pub fragments: Vec<String>,
	pub more: Vec<More>,
}

#[derive(Debug, Clone)]
pub(super) struct More {
	pub path: String,
	pub cursor: String,
	pub parent: String,
}

static POSTS: LazyLock<Selector> = LazyLock::new(|| selector("shreddit-post, #siteTable .thing[data-fullname^='t3_']"));
static COMMENTS: LazyLock<Selector> = LazyLock::new(|| selector("shreddit-comment[thingid], .comment[data-fullname^='t1_']"));
static BODY: LazyLock<Selector> = LazyLock::new(|| selector("[slot='text-body'], .usertext-body .md"));
static COMMENT_BODY: LazyLock<Selector> = LazyLock::new(|| selector("[slot='comment'], .usertext-body .md"));
static REMOVED_COMMENT: LazyLock<Selector> = LazyLock::new(|| selector("div.md > div.scalable-text > p.text-neutral-content-weak"));
static TITLE: LazyLock<Selector> = LazyLock::new(|| selector("a.title, [slot='title']"));
static IMAGES: LazyLock<Selector> = LazyLock::new(|| selector("img[src], img[data-lazy-src], img[data-src], img[srcset], img[data-lazy-srcset]"));
static GALLERIES: LazyLock<Selector> = LazyLock::new(|| selector("gallery-carousel"));
static SLIDES: LazyLock<Selector> = LazyLock::new(|| selector("li[slot^='page-']"));
static CAPTIONS: LazyLock<Selector> = LazyLock::new(|| selector("figcaption"));
static LINKS: LazyLock<Selector> = LazyLock::new(|| selector("a[href]"));
static PLAYERS: LazyLock<Selector> = LazyLock::new(|| selector("shreddit-player, video"));
static FLAIR: LazyLock<Selector> = LazyLock::new(|| selector("shreddit-post-flair, .linkflairlabel, [slot='post-flair']"));
static FRAGMENTS: LazyLock<Selector> = LazyLock::new(|| selector("faceplate-partial[src], shreddit-async-loader[src]"));
static FORM_CURSOR: LazyLock<Selector> = LazyLock::new(|| selector("input[name='cursor']"));
static MORE_COUNT: LazyLock<Selector> = LazyLock::new(|| selector("faceplate-number[number]"));
static NEXT: LazyLock<Selector> = LazyLock::new(|| selector("a[rel='next'][href], .next-button a[href]"));
static GATES: LazyLock<Selector> = LazyLock::new(|| selector("shreddit-content-gate, shreddit-error-page, [data-testid='content-gate']"));
static HEADINGS: LazyLock<Selector> = LazyLock::new(|| selector("h1, h2, title"));
static APP: LazyLock<Selector> = LazyLock::new(|| selector("shreddit-app"));
static CANONICAL: LazyLock<Selector> = LazyLock::new(|| selector("#canonical-url-updater[value]"));

pub(super) fn canonical_shell_target(html: &str, communities: &[String]) -> Result<Option<String>, WebError> {
	if !html.contains("canonical-url-updater") {
		return Ok(None);
	}
	let document = Html::parse_document(html);
	restrictions(&document, communities)?;
	if document.select(&APP).next().is_none() || document.select(&POSTS).next().is_some() || document.select(&COMMENTS).next().is_some() {
		return Ok(None);
	}
	Ok(
		document
			.select(&CANONICAL)
			.find(|element| !inside_ugc(*element) && !auxiliary(*element))
			.and_then(|element| attr(element, &["value"]))
			.map(str::to_owned),
	)
}
static SCRIPTS: LazyLock<Selector> = LazyLock::new(|| selector("script"));
static FORMS: LazyLock<Selector> = LazyLock::new(|| selector("form"));
static INPUTS: LazyLock<Selector> = LazyLock::new(|| selector("input[name]"));

pub(super) struct ScriptCheck {
	pub action: String,
	pub fields: Vec<(String, String)>,
}

/// The fixed seed-doubling form documented by webclaw 3c2006a13e5f96ea2a6956f578b9afefb8f5d8b7.
/// Interpret only that operation; never evaluate upstream JavaScript or submit other forms.
pub(super) fn script_check(html: &str, communities: &[String]) -> Result<Option<ScriptCheck>, WebError> {
	const MARKER: &str = "async e=>e+e)(\"";
	if !html.contains(MARKER) {
		return Ok(None);
	}
	let document = Html::parse_document(html);
	if let Err(error) = restrictions(&document, communities) {
		if error != WebError::Temporary("web_access_wall") {
			return Err(error);
		}
	}
	if document.select(&POSTS).next().is_some() || document.select(&COMMENTS).next().is_some() {
		return Ok(None);
	}
	let seeds: Vec<_> = document
		.select(&SCRIPTS)
		.filter(|element| !inside_ugc(*element) && !auxiliary(*element))
		.flat_map(|element| {
			let script = text(element);
			script
				.match_indices(MARKER)
				.filter_map(|(at, _)| {
					let rest = &script[at + MARKER.len()..];
					let (seed, tail) = rest.split_once('"')?;
					(!seed.is_empty() && seed.len() <= 256 && seed.bytes().all(|b| b.is_ascii_alphanumeric()) && tail.starts_with(')')).then(|| seed.to_string())
				})
				.collect::<Vec<_>>()
		})
		.collect();
	let [seed] = seeds.as_slice() else { return Ok(None) };
	let forms: Vec<_> = document
		.select(&FORMS)
		.filter(|element| !inside_ugc(*element) && !auxiliary(*element))
		.filter(|form| form.select(&INPUTS).any(|input| input.value().attr("name") == Some("jsc_token")))
		.collect();
	let [form] = forms.as_slice() else { return Ok(None) };
	if !attr(*form, &["method"]).is_some_and(|method| method.eq_ignore_ascii_case("get")) {
		return Ok(None);
	}
	let Some(action) = attr(*form, &["action"]) else { return Ok(None) };
	let mut fields = vec![("solution".into(), format!("{seed}{seed}"))];
	let mut names = HashSet::new();
	for input in form.select(&INPUTS) {
		let name = input.value().attr("name").unwrap();
		let value = input.value().attr("value").unwrap_or_default();
		if !input.value().attr("type").is_some_and(|kind| kind.eq_ignore_ascii_case("hidden")) || !names.insert(name) || value.len() > 4096 || value.chars().any(char::is_control)
		{
			return Ok(None);
		}
		match name {
			"solution" if value.is_empty() => {}
			"js_challenge" if value == "1" => fields.push((name.into(), value.into())),
			"jsc_token" if !value.is_empty() => fields.push((name.into(), value.into())),
			"jsc_orig_r" => fields.push((name.into(), value.into())),
			_ => return Ok(None),
		}
	}
	if names.len() != 4 || fields.len() != 4 {
		return Ok(None);
	}
	Ok(Some(ScriptCheck { action: action.into(), fields }))
}

fn selector(value: &str) -> Selector {
	Selector::parse(value).expect("fixed Reddit HTML selector")
}
fn attr<'a>(element: ElementRef<'a>, names: &[&str]) -> Option<&'a str> {
	names.iter().find_map(|name| element.value().attr(name).map(str::trim).filter(|value| !value.is_empty()))
}
fn flag(element: ElementRef<'_>, names: &[&str]) -> bool {
	names.iter().any(|name| {
		element
			.value()
			.attr(name)
			.is_some_and(|value| !matches!(value.to_ascii_lowercase().as_str(), "false" | "0" | "off" | "no"))
	})
}
fn class(element: ElementRef<'_>, name: &str) -> bool {
	element.value().classes().any(|value| value == name)
}
fn fullname(value: &str, kind: &str) -> bool {
	value
		.strip_prefix(kind)
		.is_some_and(|id| !id.is_empty() && id.len() <= 20 && id.bytes().all(|byte| byte.is_ascii_alphanumeric()))
}
fn community(value: &str) -> bool {
	!value.is_empty() && value.len() <= 64 && value.bytes().all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-')
}
fn allowed_community(value: &str, communities: &[String]) -> bool {
	community(value) && (communities.is_empty() || communities.iter().any(|name| name.eq_ignore_ascii_case(value)))
}
fn post_element(element: ElementRef<'_>) -> bool {
	element.value().name() == "shreddit-post" || (class(element, "thing") && element.value().attr("data-fullname").is_some_and(|id| id.starts_with("t3_")))
}
fn comment_element(element: ElementRef<'_>) -> bool {
	element.value().name() == "shreddit-comment" || (class(element, "comment") && element.value().attr("data-fullname").is_some_and(|id| id.starts_with("t1_")))
}
fn owned_by(element: ElementRef<'_>, owner: ElementRef<'_>, is_owner: fn(ElementRef<'_>) -> bool) -> bool {
	element.ancestors().filter_map(ElementRef::wrap).find(|element| is_owner(*element)) == Some(owner)
}
fn inside_content(element: ElementRef<'_>) -> bool {
	element
		.ancestors()
		.filter_map(ElementRef::wrap)
		.any(|ancestor| matches!(ancestor.value().attr("slot"), Some("comment" | "text-body")) || class(ancestor, "md"))
}
fn inside_ugc(element: ElementRef<'_>) -> bool {
	inside_content(element)
		|| element
			.ancestors()
			.filter_map(ElementRef::wrap)
			.any(|ancestor| post_element(ancestor) || comment_element(ancestor))
}
fn auxiliary(element: ElementRef<'_>) -> bool {
	let unwanted = |ancestor: ElementRef<'_>| {
		matches!(ancestor.value().name(), "aside" | "nav" | "header" | "footer" | "shreddit-sidebar")
			|| matches!(ancestor.value().id(), Some("right-sidebar-container" | "sidebar"))
			|| ["sidebar", "recommendations", "related-posts"].iter().any(|name| class(ancestor, name))
			|| matches!(ancestor.value().attr("data-testid"), Some("sidebar" | "recommendations" | "related-posts"))
	};
	unwanted(element) || element.ancestors().filter_map(ElementRef::wrap).any(unwanted)
}
fn hidden_restriction(element: ElementRef<'_>) -> bool {
	std::iter::once(element).chain(element.ancestors().filter_map(ElementRef::wrap)).any(|ancestor| {
		ancestor.value().attr("hidden").is_some()
			|| ancestor.value().attr("aria-hidden").is_some_and(|value| value.eq_ignore_ascii_case("true"))
			|| (ancestor.value().name() == "dialog" && ancestor.value().attr("open").is_none())
			|| class(ancestor, "hidden")
			|| ancestor.value().attr("style").is_some_and(|style| {
				style.split(';').any(|declaration| {
					let Some((name, value)) = declaration.split_once(':') else { return false };
					let value: String = value.chars().filter(|character| !character.is_whitespace()).flat_map(char::to_lowercase).collect();
					matches!(
						(name.trim().to_ascii_lowercase().as_str(), value.as_str()),
						("display", "none" | "none!important") | ("visibility", "hidden" | "hidden!important")
					)
				})
			})
	})
}
fn active_restriction(element: ElementRef<'_>) -> bool {
	!inside_ugc(element)
		// A document challenge title is evidence even when the rest of its
		// shell starts hidden. Auxiliary or unopened login UI is not a wall.
		&& (element.value().name() == "title" || (!auxiliary(element) && !hidden_restriction(element)))
}
fn primary_post(element: ElementRef<'_>, feed: bool) -> bool {
	if auxiliary(element) {
		return false;
	}
	let mut feed_root = false;
	for ancestor in element.ancestors().filter_map(ElementRef::wrap) {
		if post_element(ancestor) || comment_element(ancestor) {
			return false;
		}
		feed_root |= ancestor.value().name() == "shreddit-feed" || ancestor.value().id() == Some("siteTable");
	}
	!feed || feed_root
}

/// Only HTTP(S), a local path or an anchor can survive into rendered content.
fn safe_link(value: &str) -> Option<String> {
	let value = value.trim();
	if value.is_empty() || value.len() > 8192 || value.chars().any(char::is_control) || value.contains('\\') || value.starts_with("//") {
		return None;
	}
	if value.starts_with('/') || value.starts_with('#') {
		return Some(value.to_string());
	}
	let url = url::Url::parse(value).ok()?;
	(matches!(url.scheme(), "http" | "https") && url.username().is_empty() && url.password().is_none()).then(|| url.to_string())
}
fn reddit_path(value: &str) -> Option<String> {
	let value = safe_link(value)?;
	let origin = url::Url::parse("https://www.reddit.com/").ok()?;
	let url = origin.join(&value).ok()?;
	if url.scheme() != "https"
		|| !matches!(url.host_str(), Some("www.reddit.com" | "old.reddit.com"))
		|| url.port().is_some()
		|| !url.username().is_empty()
		|| url.password().is_some()
	{
		return None;
	}
	let mut path = url.path().to_string();
	if let Some(query) = url.query() {
		path.push('?');
		path.push_str(query);
	}
	Some(path)
}
fn reddit_source(value: &str) -> Option<String> {
	let mut path = reddit_path(value)?;
	if value.trim().starts_with('/') {
		if let Some((_, fragment)) = value.split_once('#') {
			path.push('#');
			path.push_str(fragment);
		}
		Some(path)
	} else {
		// Keep the explicit frontend for the caller's origin validator. A fetch
		// source cannot be silently rebased like a local UI permalink.
		url::Url::parse(value.trim()).ok().map(|url| url.to_string())
	}
}
fn permalink(value: &str, id: &str) -> Option<(String, String)> {
	let path = reddit_path(value)?;
	let path = path.split('?').next()?;
	let parts: Vec<_> = path.split('/').filter(|part| !part.is_empty()).collect();
	let ["r", sub, "comments", actual, ..] = parts.as_slice() else { return None };
	(*actual == id && community(sub)).then(|| (format!("/r/{sub}/comments/{id}/{}/", parts.get(4).copied().unwrap_or("_")), (*sub).to_string()))
}
fn image_url(value: &str) -> Option<String> {
	let value = safe_link(value)?;
	let url = url::Url::parse(&value).ok()?;
	if url.scheme() != "https" {
		return None;
	}
	let rewritten = crate::utils::format_url(&value);
	["/img/", "/preview/", "/thumb/", "/emote/"]
		.iter()
		.any(|prefix| rewritten.starts_with(prefix))
		.then_some(value)
}

#[derive(Clone)]
struct WebImage {
	url: String,
	width: i64,
	height: i64,
}
impl WebImage {
	fn preview(&self) -> Value {
		json!({"url":self.url,"width":self.width,"height":self.height})
	}
	fn gallery(&self) -> Value {
		json!({"u":self.url,"x":self.width,"y":self.height})
	}
}
fn image_dimension(value: &str) -> Option<i64> {
	value.parse::<i64>().ok().filter(|n| (1..=32768).contains(n))
}
fn image_sources(image: ElementRef<'_>) -> Vec<WebImage> {
	let base = ["src", "data-lazy-src", "data-src"].iter().find_map(|name| attr(image, &[name]).and_then(image_url));
	let base = base.as_deref().and_then(|url| url::Url::parse(url).ok());
	let base_dimension = |key| {
		base
			.as_ref()
			.and_then(|url| url.query_pairs().find(|(name, _)| name == key))
			.and_then(|(_, value)| image_dimension(&value))
	};
	let width = attr(image, &["width"]).and_then(image_dimension).or_else(|| base_dimension("width")).unwrap_or(0);
	let height = attr(image, &["height"]).and_then(image_dimension).or_else(|| base_dimension("height")).unwrap_or(0);
	let candidate = |raw: &str, descriptor: Option<i64>| {
		let url = image_url(raw)?;
		let parsed = url::Url::parse(&url).ok()?;
		let parameter = |key| parsed.query_pairs().find(|(name, _)| name == key).and_then(|(_, value)| image_dimension(&value));
		let w = parameter("width").or(descriptor).unwrap_or(width);
		let h = parameter("height").unwrap_or_else(|| if width > 0 && height > 0 && w > 0 { (height * w / width).max(1) } else { height });
		Some(WebImage { url, width: w, height: h })
	};
	let mut sources = ["src", "data-lazy-src", "data-src"]
		.iter()
		.find_map(|name| attr(image, &[name]).and_then(|raw| candidate(raw, None)))
		.into_iter()
		.collect::<Vec<_>>();
	if let Some(srcset) = attr(image, &["srcset", "data-lazy-srcset"]).filter(|raw| raw.len() <= 16384) {
		for source in srcset.split(',').take(32) {
			let parts = source.split_whitespace().collect::<Vec<_>>();
			let [url, descriptor] = parts.as_slice() else { continue };
			let width = descriptor.strip_suffix('w').and_then(image_dimension).or_else(|| {
				let density = descriptor.strip_suffix('x')?.parse::<f64>().ok().filter(|n| n.is_finite() && (0.1..=8.0).contains(n))?;
				let scaled = (width as f64 * density).round() as i64;
				(1..=32768).contains(&scaled).then_some(scaled)
			});
			let Some(width) = width else {
				continue;
			};
			if let Some(source) = candidate(url, Some(width)) {
				if !sources.iter().any(|old| old.url == source.url) {
					sources.push(source);
				}
			}
		}
	}
	sources
}
fn post_media_image(image: ElementRef<'_>, post: ElementRef<'_>) -> bool {
	owned_by(image, post, post_element)
		&& !inside_content(image)
		&& !auxiliary(image)
		&& image.value().attr("role") != Some("presentation")
		&& !class(image, "post-background-image-filter")
		&& !image.ancestors().filter_map(ElementRef::wrap).any(|ancestor| {
			ancestor.value().attr("avatar").is_some()
				|| (gallery_element(ancestor) && attr(ancestor, &["post-id"]).is_some_and(|owner| attr(post, &["id", "data-fullname"]) != Some(owner)))
				|| matches!(
					ancestor.value().attr("slot"),
					Some("credit-bar" | "authorName" | "authorIcon" | "post-flair" | "commentAvatar")
				)
		})
}
fn gallery_element(element: ElementRef<'_>) -> bool {
	element.value().name() == "gallery-carousel"
}
fn slide_element(element: ElementRef<'_>) -> bool {
	element.value().name() == "li" && element.value().attr("slot").is_some_and(|slot| slot.starts_with("page-"))
}

// Read the gallery's visible pages, not the duplicate background and lightbox
// images. Original URLs and signed preview variants come only from this post's
// own HTML; never invent a media URL or fetch metadata through the API.
fn gallery_data(post: ElementRef<'_>, id: &str) -> Result<Option<(Value, Value)>, WebError> {
	let Some(carousel) = post.select(&GALLERIES).find(|gallery| {
		owned_by(*gallery, post, post_element) && !inside_content(*gallery) && !auxiliary(*gallery) && attr(*gallery, &["post-id"]).is_none_or(|owner| owner == format!("t3_{id}"))
	}) else {
		return Ok(None);
	};
	let mut slides = carousel.select(&SLIDES).filter(|slide| owned_by(*slide, carousel, gallery_element)).collect::<Vec<_>>();
	if slides.len() > 100 {
		return Err(WebError::Temporary("gallery_bound"));
	}
	slides.sort_by_key(|slide| {
		attr(*slide, &["slot"])
			.and_then(|slot| slot.strip_prefix("page-"))
			.and_then(|value| value.parse::<u16>().ok())
			.unwrap_or(u16::MAX)
	});
	let mut pages = HashSet::new();
	if slides.iter().any(|slide| {
		!attr(*slide, &["slot"])
			.and_then(|slot| slot.strip_prefix("page-"))
			.and_then(|value| value.parse::<u16>().ok())
			.is_some_and(|page| page > 0 && pages.insert(page))
	}) {
		return Err(WebError::Temporary("gallery_order"));
	}
	let mut items = Vec::new();
	let mut metadata = serde_json::Map::new();
	for slide in slides {
		let owned = |image: ElementRef<'_>| post_media_image(image, post) && owned_by(image, slide, slide_element);
		let images = slide.select(&IMAGES).filter(|image| owned(*image)).collect::<Vec<_>>();
		let preview = images
			.iter()
			.copied()
			.find(|image| class(*image, "non-lightboxed-content"))
			.or_else(|| images.first().copied());
		let mut previews = preview.map(image_sources).unwrap_or_default();
		previews.sort_by_key(|image| image.width);
		let original = images.iter().copied().filter(|image| !class(*image, "non-lightboxed-content")).find_map(|image| {
			image_sources(image)
				.into_iter()
				.find(|source| url::Url::parse(&source.url).is_ok_and(|url| url.host_str() == Some("i.redd.it")))
		});
		let Some(mut original) = original.or_else(|| previews.last().cloned()) else { continue };
		if original.width == 0 || original.height == 0 {
			if let Some(image) = preview {
				original.width = attr(image, &["width"]).and_then(image_dimension).unwrap_or(original.width);
				original.height = attr(image, &["height"]).and_then(image_dimension).unwrap_or(original.height);
			}
		}
		let media_id = format!("web{}", items.len());
		let caption = slide.select(&CAPTIONS).find(|caption| owned_by(*caption, slide, slide_element));
		let outbound = caption.and_then(|caption| caption.select(&LINKS).find_map(|link| attr(link, &["href"]).and_then(safe_link)));
		items.push(json!({"media_id":media_id,"caption":caption.map(text).unwrap_or_default(),"outbound_url":outbound.unwrap_or_default()}));
		let animated = url::Url::parse(&original.url).is_ok_and(|url| url.path().ends_with(".gif"));
		let mut source = original.gallery();
		if animated {
			source["gif"] = json!(original.url);
		}
		metadata.insert(
			media_id,
			json!({"status":"valid","e":if animated {"AnimatedImage"} else {"Image"},"s":source,"p":previews.iter().map(WebImage::gallery).collect::<Vec<_>>()}),
		);
	}
	Ok((!items.is_empty()).then(|| (json!({"items":items}), Value::Object(metadata))))
}

fn video_source(value: &str, hls: bool) -> Option<String> {
	let value = safe_link(value)?;
	let url = url::Url::parse(&value).ok()?;
	if url.scheme() != "https" || url.port().is_some() || !matches!(url.host_str(), Some("v.redd.it" | "packaged-media.redd.it")) {
		return None;
	}
	let proxy = crate::utils::format_url(&value);
	(if hls {
		url.path().ends_with(".m3u8") && proxy.starts_with("/hls/")
	} else {
		url.path().ends_with(".mp4") && ["/vid/", "/packaged-video/"].iter().any(|prefix| proxy.starts_with(prefix))
	})
	.then_some(value)
}
fn timestamp(value: Option<&str>) -> f64 {
	value
		.and_then(|value| {
			DateTime::parse_from_rfc3339(value)
				.or_else(|_| DateTime::parse_from_str(value, "%Y-%m-%dT%H:%M:%S%.f%z"))
				.ok()
				.map(|value| value.timestamp_millis() as f64 / 1000.0)
				.or_else(|| {
					value
						.parse::<f64>()
						.ok()
						.filter(|value| value.is_finite() && *value >= 0.0)
						.map(|value| if value > 100_000_000_000.0 { value / 1000.0 } else { value })
				})
		})
		.unwrap_or_default()
}
fn number(element: ElementRef<'_>, names: &[&str]) -> i64 {
	attr(element, names).and_then(|value| value.parse().ok()).unwrap_or_default()
}
fn text(element: ElementRef<'_>) -> String {
	element.text().collect::<String>().trim().to_string()
}

// Rebuild a deliberately small HTML vocabulary. No raw upstream HTML or
// attribute is ever copied into a template's |safe body.
fn safe_body(element: ElementRef<'_>, post: Option<&str>) -> String {
	fn children(element: ElementRef<'_>, depth: usize, budget: &mut usize, out: &mut String, post: Option<&str>) {
		if depth > 64 || *budget == 0 {
			return;
		}
		for child in element.children() {
			if *budget == 0 {
				break;
			}
			*budget -= 1;
			if let Some(value) = child.value().as_text() {
				out.push_str(&htmlescape::encode_minimal(value));
				continue;
			}
			let Some(child) = ElementRef::wrap(child) else { continue };
			let name = child.value().name();
			if matches!(
				name,
				"script" | "style" | "iframe" | "object" | "embed" | "svg" | "math" | "form" | "input" | "button" | "template" | "noscript"
			) {
				continue;
			}
			if name == "img" {
				if let Some(source) = attr(child, &["src"]).and_then(image_url) {
					let source = crate::utils::format_url(&source);
					out.push_str(&format!(
						"<img loading=\"lazy\" src=\"{}\" alt=\"{}\">",
						htmlescape::encode_attribute(&source),
						htmlescape::encode_attribute(attr(child, &["alt"]).unwrap_or_default())
					));
				}
				continue;
			}
			if name == "a" {
				if let Some(href) = attr(child, &["href"]).and_then(safe_link) {
					if child.value().attr("slot") == Some("text-body") && post.is_some_and(|id| permalink(&href, id).is_some()) {
						children(child, depth + 1, budget, out, post);
						continue;
					}
					out.push_str(&format!("<a href=\"{}\" rel=\"nofollow\">", htmlescape::encode_attribute(&href)));
					children(child, depth + 1, budget, out, post);
					out.push_str("</a>");
				} else {
					children(child, depth + 1, budget, out, post)
				}
				continue;
			}
			let allowed = matches!(
				name,
				"p"
					| "div"
					| "br"
					| "strong"
					| "b"
					| "em"
					| "i"
					| "u"
					| "s"
					| "del"
					| "blockquote"
					| "pre"
					| "code"
					| "ul"
					| "ol"
					| "li"
					| "table"
					| "thead"
					| "tbody"
					| "tr"
					| "th"
					| "td"
					| "hr"
					| "h1"
					| "h2"
					| "h3"
					| "h4"
					| "h5"
					| "h6"
					| "span"
					| "figure"
					| "figcaption"
			);
			if allowed {
				if name == "span" && (class(child, "md-spoiler-text") || child.value().attr("data-spoiler").is_some()) {
					out.push_str("<span class=\"md-spoiler-text\">")
				} else {
					out.push('<');
					out.push_str(name);
					out.push('>')
				}
			}
			children(child, depth + 1, budget, out, post);
			if allowed && !matches!(name, "br" | "hr") {
				out.push_str("</");
				out.push_str(name);
				out.push('>')
			}
		}
	}
	let mut out = String::new();
	children(element, 0, &mut 20_000, &mut out, post);
	format!("<div class=\"md\">{out}</div>")
}

fn restrictions(document: &Html, communities: &[String]) -> Result<(), WebError> {
	if let Some(element) = document.select(&GATES).find(|element| active_restriction(*element)) {
		let reason = attr(element, &["reason", "type", "content-gate-type", "error-type", "data-reason"])
			.unwrap_or_default()
			.to_ascii_lowercase();
		return Err(match reason.as_str() {
			"private" | "private_community" => WebError::Resource("private"),
			"banned" | "banned_community" => WebError::Resource("banned"),
			"quarantined" | "quarantine" => WebError::Resource("quarantined"),
			"gated" | "nsfw" | "age-verification" | "age_verification" => WebError::Resource("gated"),
			"post_not_found" | "not_found" | "404" => WebError::Resource("post_not_found"),
			_ => WebError::Temporary("content_gate"),
		});
	}
	for heading in document.select(&HEADINGS).filter(|element| active_restriction(*element)) {
		let value = text(heading).split_whitespace().collect::<Vec<_>>().join(" ").to_ascii_lowercase();
		if matches!(
			value.as_str(),
			"reddit - prove your humanity"
				| "prove your humanity"
				| "you've been blocked by network security"
				| "you have been blocked by network security"
				| "log in to continue"
				| "log in to reddit to continue"
		) {
			return Err(WebError::Temporary("web_access_wall"));
		}
		if matches!(value.as_str(), "post not found" | "page not found") {
			return Err(WebError::Resource("post_not_found"));
		}
		for sub in communities {
			if value == format!("r/{} is a private community", sub.to_ascii_lowercase()) {
				return Err(WebError::Resource("private"));
			}
			if value == format!("r/{} has been banned from reddit", sub.to_ascii_lowercase()) {
				return Err(WebError::Resource("banned"));
			}
		}
	}
	Ok(())
}

fn parse_post(element: ElementRef<'_>, communities: &[String], quarantine: bool) -> Result<Option<Value>, WebError> {
	if flag(element, &["promoted", "is-promoted", "is-sponsored", "data-promoted"]) || class(element, "promoted") {
		return Ok(None);
	}
	let Some(id) = attr(element, &["id", "thingid", "data-fullname"]).filter(|id| fullname(id, "t3_")) else {
		return Ok(None);
	};
	let id = &id[3..];
	let Some((permalink, permalink_sub)) = attr(element, &["permalink", "data-permalink"]).and_then(|path| permalink(path, id)) else {
		return Ok(None);
	};
	let sub = attr(element, &["subreddit-name", "data-subreddit"]).unwrap_or(&permalink_sub);
	if !sub.eq_ignore_ascii_case(&permalink_sub) || !allowed_community(sub, communities) {
		return Ok(None);
	}
	if attr(element, &["subreddit-type", "data-subreddit-type"]).is_some_and(|value| value.eq_ignore_ascii_case("private")) {
		return Err(WebError::Resource("private"));
	}
	if !quarantine && flag(element, &["quarantined", "is-quarantined"]) {
		return Err(WebError::Resource("quarantined"));
	}
	let title = attr(element, &["post-title"])
		.map(str::to_string)
		.or_else(|| element.select(&TITLE).find(|title| owned_by(*title, element, post_element)).map(text));
	let Some(title) = title.filter(|title| !title.is_empty()) else { return Ok(None) };
	let body = element.select(&BODY).find(|body| owned_by(*body, element, post_element));
	let external = attr(element, &["content-href", "data-url"]).and_then(safe_link).unwrap_or_else(|| permalink.clone());
	let self_post = attr(element, &["post-type"]).is_some_and(|kind| matches!(kind, "text" | "self"))
		|| reddit_path(&external).is_some_and(|path| path.split('?').next() == Some(permalink.as_str()));
	let flair = element.select(&FLAIR).find(|flair| owned_by(*flair, element, post_element)).map(text).unwrap_or_default();
	let comment_count = attr(element, &["comment-count", "data-comments-count"]).and_then(|value| value.parse::<u64>().ok());
	let mut data = json!({
		"id":id,"name":format!("t3_{id}"),"title":title,"permalink":permalink,"subreddit":sub,
		"author":attr(element,&["author","data-author"]).unwrap_or("[deleted]"),"score":number(element,&["score","data-score"]),
		"num_comments":comment_count,"created_utc":timestamp(attr(element,&["created-timestamp","data-timestamp"])),
		"selftext":body.map(text).unwrap_or_default(),"selftext_html":body.map(|body|safe_body(body,Some(id))).unwrap_or_default(),
		"url":external,"url_overridden_by_dest":external,"is_self":self_post,
		"domain":if self_post {format!("self.{sub}")} else {url::Url::parse(&external).ok().and_then(|url|url.host_str().map(str::to_string)).unwrap_or_default()},
		"over_18":flag(element,&["nsfw","is-nsfw","over18","over-18"])||class(element,"over18"),
		"spoiler":flag(element,&["spoiler","is-spoiler"])||class(element,"spoiler"),
		"stickied":flag(element,&["stickied","is-stickied","pinned","is-pinned"])||class(element,"stickied"),
		"locked":flag(element,&["locked","is-locked"]),"archived":flag(element,&["archived","is-archived"]),
		"link_flair_text":flair,"all_awardings":[],"thumbnail":"","post_hint":"","is_video":false,
		"removed_by_category":if flag(element,&["removed","is-removed"]){"moderator"}else{""}
	});
	let mut images = element.select(&IMAGES).filter(|image| post_media_image(*image, element)).collect::<Vec<_>>();
	images.sort_by_key(|image| {
		!image
			.ancestors()
			.filter_map(ElementRef::wrap)
			.any(|parent| parent.value().attr("slot") == Some("thumbnail") || class(parent, "thumbnail"))
	});
	if let Some(mut sources) = images.into_iter().find_map(|image| {
		let sources = image_sources(image);
		(!sources.is_empty()).then_some(sources)
	}) {
		let thumbnail = &sources[0];
		data["thumbnail"] = json!(thumbnail.url);
		data["thumbnail_width"] = json!(thumbnail.width);
		data["thumbnail_height"] = json!(thumbnail.height);
		sources.sort_by_key(|image| image.width);
		data["preview"] = json!({"images":[{"source":sources.last().unwrap().preview(),"resolutions":sources.iter().map(WebImage::preview).collect::<Vec<_>>(),"variants":{}}]});
	}
	if let Some((gallery, metadata)) = gallery_data(element, id)? {
		data["is_gallery"] = json!(true);
		data["is_self"] = json!(false);
		data["gallery_data"] = gallery;
		data["media_metadata"] = metadata;
	}
	if image_url(&external).is_some() && !self_post {
		data["post_hint"] = json!("image")
	}
	if let Some(player) = element
		.select(&PLAYERS)
		.find(|player| owned_by(*player, element, post_element) && !inside_content(*player) && !player.ancestors().filter_map(ElementRef::wrap).any(gallery_element))
	{
		let packaged = attr(player, &["packaged-media-json"])
			.filter(|raw| raw.len() <= 256 * 1024)
			.and_then(|raw| serde_json::from_str::<Value>(raw).ok());
		let packaged = packaged
			.as_ref()
			.and_then(|value| value.pointer("/playbackMp4s/permutations"))
			.and_then(Value::as_array)
			.into_iter()
			.flatten()
			.take(32)
			.filter_map(|value| {
				let source = value.get("source")?;
				if source["videoCodec"].as_str().is_some_and(|codec| !codec.eq_ignore_ascii_case("H264")) {
					return None;
				}
				let url = source["url"].as_str().and_then(|url| video_source(url, false))?;
				let width = source["dimensions"]["width"].as_u64().unwrap_or(0).min(8192);
				let height = source["dimensions"]["height"].as_u64().unwrap_or(0).min(8192);
				Some((url, width, height))
			})
			.min_by_key(|(_, _, height)| if *height <= 720 { 720 - *height } else { 10000 + *height - 720 });
		let mp4 = packaged.as_ref().map(|(url, _, _)| url.clone()).or_else(|| {
			["src-mp4", "src", "preview"]
				.iter()
				.find_map(|name| attr(player, &[name]).and_then(|url| video_source(url, false)))
		});
		let hls = ["hls-src", "hls-url", "src"]
			.iter()
			.find_map(|name| attr(player, &[name]).and_then(|url| video_source(url, true)));
		if let Some(primary) = mp4.or_else(|| hls.clone()) {
			let (width, height) = packaged
				.as_ref()
				.map(|(_, w, h)| (*w as i64, *h as i64))
				.unwrap_or_else(|| (number(player, &["width"]).max(0), number(player, &["height"]).max(0)));
			data["is_video"] = json!(true);
			data["secure_media"] = json!({"reddit_video":{"fallback_url":primary,"hls_url":hls.unwrap_or_default(),"width":width,"height":height,"is_gif":false}});
		}
	}
	Ok(Some(json!({"kind":"t3","data":data})))
}

struct ParsedComment {
	id: String,
	parent: String,
	data: Value,
}
fn parse_comments(document: &Html, post_id: &str) -> Result<Vec<Value>, WebError> {
	let post = format!("t3_{post_id}");
	let mut entries = Vec::new();
	let mut ids = HashSet::new();
	for element in document.select(&COMMENTS).filter(|element| {
		!inside_content(*element)
			&& !auxiliary(*element)
			&& element
				.ancestors()
				.filter_map(ElementRef::wrap)
				.filter(|ancestor| post_element(*ancestor))
				.all(|ancestor| attr(ancestor, &["id", "thingid", "data-fullname"]) == Some(post.as_str()))
	}) {
		let Some(id) = attr(element, &["thingid", "data-fullname"]).filter(|id| fullname(id, "t1_")) else {
			return Err(WebError::Temporary("invalid_comment_id"));
		};
		if !ids.insert(id.to_string()) {
			return Err(WebError::Temporary("duplicate_comment_id"));
		}
		if attr(element, &["postid", "linkid", "data-link-id"]).is_some_and(|value| value != post) {
			return Err(WebError::Temporary("comment_post_mismatch"));
		}
		let ancestor = element.ancestors().filter_map(ElementRef::wrap).find(|ancestor| comment_element(*ancestor));
		let ancestor = ancestor.and_then(|ancestor| attr(ancestor, &["thingid", "data-fullname"]));
		if ancestor.is_some_and(|parent| !fullname(parent, "t1_")) {
			return Err(WebError::Temporary("invalid_comment_parent"));
		}
		let explicit = attr(element, &["parentid", "data-parent-fullname"]);
		if explicit.is_some_and(|parent| !(fullname(parent, "t1_") || parent == post)) {
			return Err(WebError::Temporary("invalid_comment_parent"));
		}
		if ancestor.zip(explicit).is_some_and(|(dom, actual)| dom != actual) {
			return Err(WebError::Temporary("comment_parent_mismatch"));
		}
		let parent = ancestor.or(explicit).unwrap_or(&post).to_string();
		let comment_permalink = if let Some(path) = attr(element, &["permalink", "data-permalink"]) {
			let Some(raw) = reddit_path(path) else {
				return Err(WebError::Temporary("comment_permalink_mismatch"));
			};
			let parts: Vec<_> = raw.split('?').next().unwrap_or_default().split('/').filter(|part| !part.is_empty()).collect();
			if parts.get(5).is_some_and(|comment| *comment != &id[3..]) {
				return Err(WebError::Temporary("comment_permalink_mismatch"));
			}
			let Some((base, _)) = permalink(path, post_id) else {
				return Err(WebError::Temporary("comment_permalink_mismatch"));
			};
			format!("{base}{}/", &id[3..])
		} else {
			String::new()
		};
		let body = element
			.select(&COMMENT_BODY)
			.find(|body| owned_by(*body, element, comment_element) && body.value().id().is_none_or(|body_id| body_id == format!("{id}-comment-rtjson-content")));
		let deleted = flag(element, &["deleted", "is-deleted"]) || attr(element, &["author", "data-author"]) == Some("[deleted]");
		// Reddit renders its removal notice outside the usual comment slot.
		// Recognize only the native placeholder of this comment, never text
		// inside a user body, child reply or sidebar.
		let removed = flag(element, &["removed", "is-removed"])
			|| (body.is_none()
				&& element
					.select(&REMOVED_COMMENT)
					.any(|notice| !auxiliary(notice) && owned_by(notice, element, comment_element) && text(notice) == "Comment removed by Reddit"));
		if body.is_none() && !deleted && !removed {
			return Err(WebError::Temporary("missing_comment_body"));
		}
		let author = attr(element, &["author", "data-author"]).unwrap_or("[deleted]");
		let plain = if removed {
			"[removed]".to_string()
		} else {
			body.map(text).unwrap_or_else(|| "[deleted]".into())
		};
		let html = if removed {
			"<div class=\"md\"><p>[removed]</p></div>".to_string()
		} else {
			body.map(|body| safe_body(body, None)).unwrap_or_else(|| "<div class=\"md\"><p>[deleted]</p></div>".into())
		};
		let data = json!({"id":&id[3..],"name":id,"parent_id":parent,"link_id":post,"author":author,"body":plain,"body_html":html,
			"score":number(element,&["score","data-score"]),"created_utc":timestamp(attr(element,&["created","created-timestamp","data-timestamp"])),
			"score_hidden":flag(element,&["score-hidden","is-score-hidden"]),"edited":false,"distinguished":if flag(element,&["is-mod-distinguished"]){json!("moderator")}else{Value::Null},
			"stickied":flag(element,&["stickied","is-stickied"]),"collapsed":flag(element,&["collapsed"]),"permalink":comment_permalink,"replies":"","all_awardings":[]});
		entries.push(ParsedComment { id: id.into(), parent, data });
		if entries.len() > 1000 {
			return Err(WebError::Temporary("too_many_comments"));
		}
	}
	let indices: HashMap<_, _> = entries.iter().enumerate().map(|(index, comment)| (comment.id.as_str(), index)).collect();
	let mut children = vec![Vec::new(); entries.len()];
	let mut roots = Vec::new();
	for (index, comment) in entries.iter().enumerate() {
		if let Some(parent) = indices.get(comment.parent.as_str()) {
			children[*parent].push(index)
		} else {
			roots.push(index)
		}
	}
	fn build(index: usize, entries: &[ParsedComment], children: &[Vec<usize>], seen: &mut HashSet<usize>, depth: usize) -> Result<Value, WebError> {
		if depth > 64 || !seen.insert(index) {
			return Err(WebError::Temporary("comment_cycle"));
		}
		let mut data = entries[index].data.clone();
		let replies = children[index]
			.iter()
			.map(|child| build(*child, entries, children, seen, depth + 1))
			.collect::<Result<Vec<_>, _>>()?;
		if !replies.is_empty() {
			data["replies"] = json!({"kind":"Listing","data":{"children":replies,"after":null,"before":null}})
		}
		Ok(json!({"kind":"t1","data":data}))
	}
	let mut seen = HashSet::new();
	let comments = roots
		.into_iter()
		.map(|root| build(root, &entries, &children, &mut seen, 0))
		.collect::<Result<Vec<_>, _>>()?;
	if seen.len() != entries.len() {
		return Err(WebError::Temporary("comment_cycle"));
	}
	Ok(comments)
}

fn same_scope_partial(path: &str, post: Option<&str>, communities: &[String]) -> bool {
	let Some(path) = reddit_path(path) else { return false };
	let base = path.split('?').next().unwrap_or_default();
	let parts: Vec<_> = base.split('/').filter(|part| !part.is_empty()).collect();
	if let Some(post) = post {
		return matches!(parts.as_slice(),["svc","shreddit","comments","r",sub,id,..] if allowed_community(sub,communities) && *id==format!("t3_{post}"));
	}
	match parts.as_slice() {
		["svc", "shreddit", "community-more-posts", sort] if matches!(*sort, "hot" | "new" | "top" | "rising" | "controversial" | "best") => {
			let names = url::form_urlencoded::parse(path.split_once('?').map_or("", |(_, query)| query).as_bytes())
				.filter(|(key, _)| key == "name")
				.map(|(_, value)| value.into_owned())
				.collect::<Vec<_>>();
			names.len() == 1
				&& if communities.is_empty() {
					matches!(names[0].as_str(), "all" | "popular")
				} else {
					allowed_community(&names[0], communities)
						// Reddit also spells this server-provided continuation with
						// a t5 ID. It is only a candidate for one expected community:
						// the returned posts must still pass parse_post's independent
						// subreddit/permalink checks before the caller can use them.
						|| (communities.len() == 1 && fullname(&names[0], "t5_"))
				}
		}
		["svc", "shreddit", "feeds", "popular-feed"] => communities.is_empty(),
		_ => false,
	}
}

pub(super) fn parse(html: &str, expected_post: Option<&str>, communities: &[String], quarantine: bool) -> Result<Document, WebError> {
	parse_mode(html, expected_post, communities, quarantine, true)
}

/// A saved continuation may need to refetch only its post metadata. Its own
/// validated GET/POST response supplies the comments; do not require the base
/// page to expand or parse that initial tree again.
pub(super) fn parse_metadata(html: &str, expected_post: &str, communities: &[String], quarantine: bool) -> Result<Document, WebError> {
	parse_mode(html, Some(expected_post), communities, quarantine, false)
}

fn parse_mode(html: &str, expected_post: Option<&str>, communities: &[String], quarantine: bool, require_comments: bool) -> Result<Document, WebError> {
	if expected_post.is_some_and(|id| !fullname(&format!("t3_{id}"), "t3_")) {
		return Err(WebError::Temporary("invalid_post_id"));
	}
	let document = Html::parse_document(html);
	restrictions(&document, communities)?;
	let mut posts = Vec::new();
	let mut seen = HashSet::new();
	for element in document
		.select(&POSTS)
		.filter(|element| !inside_content(*element) && primary_post(*element, expected_post.is_none()))
	{
		if let Some(expected) = expected_post {
			if attr(element, &["id", "thingid", "data-fullname"]) != Some(format!("t3_{expected}").as_str()) {
				continue;
			}
		}
		if let Some(post) = parse_post(element, communities, quarantine)? {
			let id = post["data"]["name"].as_str().unwrap().to_string();
			if seen.insert(id) {
				posts.push(post)
			}
		}
		if posts.len() > 200 {
			return Err(WebError::Temporary("too_many_posts"));
		}
	}
	if !require_comments && posts.is_empty() {
		return Err(WebError::Temporary("missing_post_metadata"));
	}
	let mut comments = if require_comments {
		expected_post.map(|post| parse_comments(&document, post)).transpose()?.unwrap_or_default()
	} else {
		Vec::new()
	};
	let mut fragments = Vec::new();
	let mut more = Vec::new();
	for element in document.select(&FRAGMENTS).filter(|element| !inside_content(*element) && !auxiliary(*element)) {
		if fragments.len() + more.len() >= 8 {
			break;
		}
		if let Some(post) = expected_post {
			if let Some(path) = attr(element, &["src"]).and_then(reddit_source).filter(|path| same_scope_more(path, post, communities)) {
				if let Some(cursor) = element
					.select(&FORM_CURSOR)
					.find(|input| {
						input
							.ancestors()
							.filter_map(ElementRef::wrap)
							.find(|ancestor| matches!(ancestor.value().name(), "faceplate-partial" | "shreddit-async-loader"))
							== Some(element)
					})
					.and_then(|input| input.value().attr("value"))
					.filter(|cursor| !cursor.is_empty() && cursor.len() <= 4096 && !cursor.chars().any(char::is_control))
				{
					let parent = element
						.ancestors()
						.filter_map(ElementRef::wrap)
						.find(|ancestor| comment_element(*ancestor))
						.and_then(|parent| attr(parent, &["thingid", "data-fullname"]))
						.map(str::to_string)
						.unwrap_or_else(|| format!("t3_{post}"));
					if fullname(&parent, "t1_") || parent == format!("t3_{post}") {
						if more.len() < 8 && !more.iter().any(|item: &More| item.path == path && item.cursor == cursor) {
							if fullname(&parent, "t1_") {
								let count = element.select(&MORE_COUNT).next().map(|count| number(count, &["number"])).unwrap_or(0).max(1);
								let stub = json!({"kind":"more","data":{"id":"_","name":"more","parent_id":parent,"link_id":format!("t3_{post}"),"count":count,"children":[]}});
								append_more_stub(&mut comments, &parent, stub, 0);
							}
							more.push(More {
								path,
								cursor: cursor.into(),
								parent,
							});
						}
					}
				}
				continue;
			}
		}
		if attr(element, &["method"]).is_some_and(|method| !method.eq_ignore_ascii_case("get")) || element.select(&FORM_CURSOR).next().is_some() {
			continue;
		}
		if let Some(path) = attr(element, &["src"])
			.and_then(reddit_source)
			.filter(|path| same_scope_partial(path, expected_post, communities))
		{
			if !fragments.contains(&path) {
				fragments.push(path)
			}
			if fragments.len() == 8 {
				break;
			}
		}
	}
	let next = document
		.select(&NEXT)
		.filter(|element| !inside_content(*element) && !auxiliary(*element))
		.filter_map(|element| attr(element, &["href"]).and_then(reddit_source))
		.find(|path| {
			let Some(path) = reddit_path(path) else { return false };
			let base = path.split('?').next().unwrap_or_default();
			let parts: Vec<_> = base.split('/').filter(|part| !part.is_empty()).collect();
			expected_post.is_none() && matches!(parts.as_slice(),["r",sub,..] if allowed_community(sub,communities))
		})
		.or_else(|| expected_post.is_none().then(|| fragments.first().cloned()).flatten());
	if posts.is_empty() && comments.is_empty() && fragments.is_empty() && more.is_empty() {
		let raw_posts = document.select(&POSTS).count();
		let raw_comments = document.select(&COMMENTS).count();
		let raw_partials = document.select(&FRAGMENTS).count();
		let app_shell = raw_posts == 0 && raw_comments == 0 && document.select(&APP).next().is_some();
		log::info!(
			"Anonymous Reddit HTML has no usable content: page_kind={} post_candidates={} comment_candidates={} partial_candidates={} app_shell={}",
			if expected_post.is_some() { "thread" } else { "feed" },
			raw_posts,
			raw_comments,
			raw_partials,
			app_shell
		);
		if app_shell {
			return Err(WebError::Temporary("empty_web_app_shell"));
		}
		return Err(WebError::Temporary("missing_web_content"));
	}
	if require_comments
		&& expected_post.is_some()
		&& !posts.is_empty()
		&& comments.is_empty()
		&& fragments.is_empty()
		&& more.is_empty()
		&& posts[0]["data"]["num_comments"].as_u64() != Some(0)
	{
		return Err(WebError::Temporary("missing_comment_tree"));
	}
	Ok(Document {
		posts,
		comments,
		next,
		fragments,
		more,
	})
}

fn same_scope_more(path: &str, post: &str, communities: &[String]) -> bool {
	let Some(path) = reddit_path(path) else { return false };
	let parts: Vec<_> = path.split('?').next().unwrap_or_default().split('/').filter(|part| !part.is_empty()).collect();
	matches!(parts.as_slice(),["svc","shreddit","more-comments",sub,id] if allowed_community(sub,communities)&&*id==format!("t3_{post}"))
}
fn append_more_stub(comments: &mut [Value], parent: &str, stub: Value, depth: usize) -> bool {
	if depth > 64 {
		return false;
	}
	for comment in comments.iter_mut() {
		if comment["data"]["name"] == parent {
			if !comment["data"]["replies"]["data"]["children"].is_array() {
				comment["data"]["replies"] = json!({"kind":"Listing","data":{"children":[],"after":null,"before":null}})
			}
			comment["data"]["replies"]["data"]["children"].as_array_mut().unwrap().push(stub);
			return true;
		}
		if let Some(children) = comment.pointer_mut("/data/replies/data/children").and_then(Value::as_array_mut) {
			if append_more_stub(children, parent, stub.clone(), depth + 1) {
				return true;
			}
		}
	}
	false
}

#[cfg(test)]
mod tests {
	use super::*;

	#[tokio::test]
	async fn test_reddit_web_html_gallery_and_article_metadata_reaches_existing_views() {
		let doc = parse(include_str!("../tests/fixtures/reddit_web_media.html"), None, &names(), false).unwrap();
		let gallery = crate::utils::parse_post(&doc.posts[0]).await;
		assert_eq!(gallery.post_type, "gallery");
		assert_eq!(gallery.gallery.len(), 2, "one item per page, without backgrounds, avatars or lightbox duplicates");
		assert_eq!(gallery.gallery[0].url, "/img/one.jpeg");
		assert_eq!(gallery.gallery[1].url, "/img/two.png");
		assert!(gallery.gallery[0].preview_url.contains("width=640"));
		assert_eq!(gallery.gallery[0].caption, "First & best Source");
		assert_eq!(gallery.gallery[0].outbound_url, "https://example.com/source");
		assert_eq!((gallery.gallery[1].width, gallery.gallery[1].height), (800, 1200));
		let article = crate::utils::parse_post(&doc.posts[1]).await;
		assert_eq!(article.post_type, "link");
		assert_eq!(article.media.url, "https://example.com/news");
		assert!(article.thumbnail.url.starts_with("/preview/external-pre/"));
		assert_eq!((article.thumbnail.width, article.thumbnail.height), (140, 93));
	}

	const POST: &str = r#"<shreddit-post id="t3_abc" permalink="/r/rust/comments/abc/title/" content-href="https://www.reddit.com/r/rust/comments/abc/title/" post-title="A &amp; B" author="op" subreddit-name="rust" score="42" comment-count="2" created-timestamp="2026-11-18T07:43:02+0000" is-stickied nsfw="false" spoiler><div slot="text-body"><p>Own <strong>body</strong>.</p></div></shreddit-post>"#;
	const COMMENTS_HTML: &str = r#"<shreddit-comment thingid="t1_one" author="alice" score="5" created="2026-11-18T08:00:00+0000"><div id="t1_one-comment-rtjson-content" slot="comment"><p>Parent</p></div><shreddit-comment thingid="t1_two" author="bob" score="3" depth="1"><div id="t1_two-comment-rtjson-content" slot="comment"><p>Child</p></div></shreddit-comment></shreddit-comment>"#;
	fn names() -> Vec<String> {
		vec!["rust".into()]
	}
	fn page() -> String {
		format!("<html><body>{POST}{COMMENTS_HTML}</body></html>")
	}

	#[tokio::test]
	async fn test_reddit_web_html_gallery_scope_variants_and_preview_sizing() {
		let fixture = include_str!("../tests/fixtures/reddit_web_media.html");
		for page in [
			fixture
				.replace("<gallery-carousel", "<div slot=\"text-body\"><gallery-carousel")
				.replace("</gallery-carousel>", "</gallery-carousel></div>"),
			fixture.replace("post-id=\"t3_abc\"", "post-id=\"t3_wrong\""),
		] {
			let doc = parse(&page, None, &names(), false).unwrap();
			assert!(doc.posts[0]["data"]["is_gallery"].is_null(), "a gallery must belong to its own post media");
		}
		let gif = fixture.replace("https://i.redd.it/two.png", "https://i.redd.it/two.gif");
		let doc = parse(&gif, None, &names(), false).unwrap();
		let post = crate::utils::parse_post(&doc.posts[0]).await;
		assert!(post.gallery[1].is_animated);
		assert_eq!(post.gallery[1].gif_url, "/img/two.gif");
		let invalid = fixture.replace("https://i.redd.it/one.jpeg", "https://evil.test/one.jpeg");
		let doc = parse(&invalid, None, &names(), false).unwrap();
		let post = crate::utils::parse_post(&doc.posts[0]).await;
		assert!(
			post.gallery[0].url.starts_with("/preview/"),
			"use a supplied proxyable preview if the original is unavailable"
		);
		assert!(!post.gallery.iter().any(|image| image.url.contains("evil.test")));
		assert!(parse(&fixture.replace("slot=\"page-2\"", "slot=\"page-1\""), None, &names(), false).is_err());
		let richer = fixture.replace("alt=\"An article\"", "width=\"1200\" height=\"800\" srcset=\"https://external-preview.redd.it/article.jpeg?width=640&amp;s=fixture 640w, https://external-preview.redd.it/article.jpeg?width=1080&amp;s=fixture 1080w\"");
		let doc = parse(&richer, None, &names(), false).unwrap();
		let article = crate::utils::parse_post(&doc.posts[1]).await;
		assert_eq!((article.thumbnail.width, article.thumbnail.height), (1080, 720));
		assert!(article.thumbnail.url.contains("width=1080"));
		let bad = fixture.replace("https://external-preview.redd.it/article.jpeg", "https://external-preview.redd.it.evil.test/article.jpeg");
		let doc = parse(&bad, None, &names(), false).unwrap();
		assert!(crate::utils::parse_post(&doc.posts[1]).await.thumbnail.url.is_empty());
	}

	#[test]
	fn test_reddit_web_html_post_comment_tree_and_flags_are_independent_of_api() {
		let doc = parse(&page(), Some("abc"), &names(), false).unwrap();
		assert_eq!(doc.posts.len(), 1);
		let post = &doc.posts[0]["data"];
		assert_eq!(post["name"], "t3_abc");
		assert_eq!(post["title"], "A & B");
		assert_eq!(post["subreddit"], "rust");
		assert_eq!(post["author"], "op");
		assert_eq!(post["score"], 42);
		assert!(post["created_utc"].as_f64().unwrap() > 1_700_000_000.0);
		assert_eq!(post["stickied"], true);
		assert_eq!(post["over_18"], false);
		assert_eq!(post["spoiler"], true);
		assert!(post["selftext_html"].as_str().unwrap().contains("<strong>body</strong>"));
		assert_eq!(doc.comments.len(), 1);
		assert_eq!(doc.comments[0]["data"]["parent_id"], "t3_abc");
		assert_eq!(doc.comments[0]["data"]["body"], "Parent");
		let child = &doc.comments[0]["data"]["replies"]["data"]["children"][0];
		assert_eq!(child["data"]["parent_id"], "t1_one");
		assert_eq!(child["data"]["body"], "Child");
		assert_eq!(child["kind"], "t1");
	}

	#[test]
	fn test_reddit_web_html_scope_wrong_ids_shells_and_comment_identity_fail_closed() {
		for html in [
			"<html><h1>Reddit</h1></html>".to_string(),
			POST.replace("t3_abc", "t3_other"),
			POST.replace("/comments/abc/", "/comments/wrong/"),
			POST.replace("subreddit-name=\"rust\"", "subreddit-name=\"elsewhere\""),
		] {
			assert!(matches!(parse(&html, Some("abc"), &names(), false), Err(WebError::Temporary(_))));
		}
		let doc = parse(&page(), None, &["elsewhere".into()], false);
		assert!(matches!(doc, Err(WebError::Temporary("missing_web_content"))));
		for html in [
			COMMENTS_HTML.replace("t1_two", "t1_one"),
			COMMENTS_HTML.replace("author=\"bob\"", "author=\"bob\" parentid=\"t3_wrong\""),
			COMMENTS_HTML.replace("author=\"bob\"", "author=\"bob\" parentid=\"t1_other\""),
		] {
			assert!(matches!(parse(&html, Some("abc"), &names(), false), Err(WebError::Temporary(_))));
		}
		let absent_parent_body = COMMENTS_HTML.replace("<div id=\"t1_one-comment-rtjson-content\" slot=\"comment\"><p>Parent</p></div>", "");
		assert!(matches!(
			parse(&absent_parent_body, Some("abc"), &names(), false),
			Err(WebError::Temporary("missing_comment_body"))
		));
		let deleted = absent_parent_body.replace("author=\"alice\"", "author=\"[deleted]\"");
		let doc = parse(&deleted, Some("abc"), &names(), false).unwrap();
		assert_eq!(doc.comments[0]["data"]["body"], "[deleted]");
		assert_eq!(doc.comments[0]["data"]["replies"]["data"]["children"][0]["data"]["body"], "Child");
	}

	#[test]
	fn test_reddit_web_html_gates_and_challenges_are_not_empty_success_or_user_text_matches() {
		for (reason, expected) in [
			("private", "private"),
			("banned", "banned"),
			("quarantine", "quarantined"),
			("age-verification", "gated"),
			("not_found", "post_not_found"),
		] {
			let html = format!("<shreddit-content-gate reason=\"{reason}\"></shreddit-content-gate>{POST}");
			assert!(matches!(parse(&html, Some("abc"), &names(), true), Err(WebError::Resource(actual)) if actual==expected));
		}
		for heading in ["Reddit - Prove your humanity", "Log in to continue", "You've been blocked by network security"] {
			assert!(matches!(
				parse(&format!("<title>{heading}</title>{POST}"), Some("abc"), &names(), false),
				Err(WebError::Temporary("web_access_wall"))
			));
		}
		let quoted = page().replace(
			"Own <strong>body</strong>.",
			"<h1>Post not found</h1><shreddit-content-gate reason=\"private\"></shreddit-content-gate>Own content",
		);
		assert!(parse(&quoted, Some("abc"), &names(), false).is_ok());
		let quarantined = page().replace("is-stickied", "is-stickied is-quarantined");
		assert!(matches!(parse(&quarantined, Some("abc"), &names(), false), Err(WebError::Resource("quarantined"))));
		assert!(parse(&quarantined, Some("abc"), &names(), true).is_ok());
	}

	#[test]
	fn test_reddit_web_html_body_rebuild_strips_active_html_and_external_inline_images() {
		let poison = r#"<script>alert(1)</script><svg onload="alert(1)"><script>x</script></svg><iframe src="https://evil.test"></iframe><a href="javascript:alert(1)" onclick="bad()">Bad link</a><a href="https://example.com/a?x=1&amp;y=2" style="bad">Good link</a><img src="https://evil.test/tracker" onerror="bad()"><img src="https://i.redd.it/picture.png" onerror="bad()"><span class="md-spoiler-text" onclick="bad()">secret</span>"#;
		let html = page().replace("Own <strong>body</strong>.", poison).replace("<p>Parent</p>", poison);
		let doc = parse(&html, Some("abc"), &names(), false).unwrap();
		for body in [
			doc.posts[0]["data"]["selftext_html"].as_str().unwrap(),
			doc.comments[0]["data"]["body_html"].as_str().unwrap(),
		] {
			let decoded = htmlescape::decode_html(body).unwrap();
			for bad in ["<script", "<svg", "<iframe", "javascript:", "onclick", "onerror", "style=", "evil.test", "alert(1)"] {
				assert!(!decoded.contains(bad), "unsafe HTML survived: {bad}");
			}
			assert!(decoded.contains("https://example.com"));
			assert!(decoded.contains("/img/picture.png"));
			assert!(body.contains("md-spoiler-text"));
		}
	}

	#[test]
	fn test_reddit_web_html_ads_images_outbound_media_and_old_listing_shape() {
		let image = POST
			.replace(
				"content-href=\"https://www.reddit.com/r/rust/comments/abc/title/\"",
				"content-href=\"https://i.redd.it/picture.png\"",
			)
			.replace(
				"<div slot=\"text-body\"><p>Own <strong>body</strong>.</p></div>",
				"<img src=\"https://preview.redd.it/picture.png?width=640\" width=\"640\" height=\"480\">",
			);
		let ad = image
			.replace("t3_abc", "t3_ad")
			.replace("/comments/abc/", "/comments/ad/")
			.replace("is-stickied", "is-stickied promoted");
		let doc = parse(&format!("<shreddit-feed>{ad}{image}</shreddit-feed>"), None, &names(), false).unwrap();
		assert_eq!(doc.posts.len(), 1);
		assert_eq!(doc.posts[0]["data"]["post_hint"], "image");
		assert_eq!(doc.posts[0]["data"]["preview"]["images"][0]["source"]["width"], 640);
		let old = r#"<div id="siteTable"><div class="thing link stickied over18" data-fullname="t3_old" data-author="alice" data-subreddit="rust" data-permalink="/r/rust/comments/old/title/" data-url="https://example.com/story" data-score="7" data-timestamp="1794991382000"><a class="title">Old listing</a></div></div>"#;
		let doc = parse(old, None, &names(), false).unwrap();
		assert_eq!(doc.posts[0]["data"]["url"], "https://example.com/story");
		assert_eq!(doc.posts[0]["data"]["stickied"], true);
		assert_eq!(doc.posts[0]["data"]["over_18"], true);
		assert!(doc.posts[0]["data"]["created_utc"].as_f64().unwrap() < 10_000_000_000.0);
	}

	#[test]
	fn test_reddit_web_html_fragment_and_form_more_sources_are_scoped_and_bounded() {
		let fragment = r#"<faceplate-partial src="/svc/shreddit/comments/r/rust/t3_abc?sort=TOP"></faceplate-partial>"#;
		let doc = parse(fragment, Some("abc"), &names(), false).unwrap();
		assert!(doc.posts.is_empty() && doc.comments.is_empty());
		assert_eq!(doc.fragments, ["/svc/shreddit/comments/r/rust/t3_abc?sort=TOP"]);
		for path in [
			"https://evil.test/svc/shreddit/comments/r/rust/t3_abc",
			"/svc/shreddit/comments/r/elsewhere/t3_abc",
			"/svc/shreddit/comments/r/rust/t3_wrong",
			"//evil.test/svc/shreddit/comments/r/rust/t3_abc",
		] {
			assert!(matches!(
				parse(&fragment.replace("/svc/shreddit/comments/r/rust/t3_abc?sort=TOP", path), Some("abc"), &names(), false),
				Err(WebError::Temporary(_))
			));
		}
		let more = r#"<faceplate-partial src="/svc/shreddit/more-comments/rust/t3_abc?commentParentPositions=1" method="post"><input name="cursor" value="opaque+token"><faceplate-number number="7"></faceplate-number></faceplate-partial>"#;
		let nested = COMMENTS_HTML.replace("<p>Parent</p></div>", &format!("<p>Parent</p></div>{more}"));
		let doc = parse(&format!("{nested}{more}"), Some("abc"), &names(), false).unwrap();
		assert_eq!(doc.more.len(), 1, "duplicate form cursors are deduplicated");
		assert_eq!(doc.more[0].parent, "t1_one");
		assert_eq!(doc.more[0].cursor, "opaque+token");
		assert!(doc.fragments.is_empty(), "form POST sources must never be retried as GET");
		assert_eq!(doc.comments[0]["data"]["replies"]["data"]["children"][1]["kind"], "more");
		let root = parse(more, Some("abc"), &names(), false).unwrap();
		assert_eq!(root.more[0].parent, "t3_abc");
		for bad in [more.replace("t3_abc", "t3_wrong"), more.replace("opaque+token", &"x".repeat(4097))] {
			assert!(parse(&bad, Some("abc"), &names(), false).is_err());
		}
	}

	#[test]
	fn test_reddit_web_html_more_comment_after_leaf_sibling_does_not_panic() {
		let comments = r#"<shreddit-comment thingid="t1_leaf" author="alice"><div slot="comment"><p>A leaf</p></div></shreddit-comment><shreddit-comment thingid="t1_target" author="bob"><div slot="comment"><p>A parent</p></div><faceplate-partial src="/svc/shreddit/more-comments/rust/t3_abc?commentParentPositions=1" method="post"><input name="cursor" value="fixture"><faceplate-number number="3"></faceplate-number></faceplate-partial></shreddit-comment>"#;
		let doc = parse(&format!("{POST}{comments}"), Some("abc"), &names(), false).unwrap();
		assert_eq!(doc.comments[0]["data"]["replies"], "");
		assert_eq!(doc.comments[1]["data"]["replies"]["data"]["children"][0]["kind"], "more");
		assert_eq!(doc.more[0].parent, "t1_target");
	}

	#[test]
	fn test_reddit_web_html_native_removed_comment_preserves_thread_and_replies() {
		let notice = r#"<div class="md text-14-scalable"><div class="scalable-text"><p class="text-neutral-content-weak">Comment removed by Reddit</p></div></div>"#;
		let removed = format!(
			r#"<shreddit-comment thingid="t1_removed" author="alice">{notice}<shreddit-comment thingid="t1_reply" author="bob"><div slot="comment"><p>A reply</p></div></shreddit-comment></shreddit-comment>"#
		);
		let doc = parse(&format!("{POST}{removed}"), Some("abc"), &names(), false).unwrap();
		assert_eq!(doc.comments[0]["data"]["body"], "[removed]");
		assert_eq!(doc.comments[0]["data"]["replies"]["data"]["children"][0]["data"]["body"], "A reply");
		let ordinary = removed.replace(notice, "<div slot=\"comment\"><p>Comment removed by Reddit</p></div>");
		assert_eq!(
			parse(&format!("{POST}{ordinary}"), Some("abc"), &names(), false).unwrap().comments[0]["data"]["body"],
			"Comment removed by Reddit"
		);
		for bad in [
			removed.replace(notice, ""),
			removed.replace(notice, "<p>Comment removed by Reddit</p>"),
			removed.replace(notice, &format!("<aside>{notice}</aside>")),
		] {
			assert!(
				parse(&format!("{POST}{bad}"), Some("abc"), &names(), false).is_err(),
				"missing bodies must not turn into fabricated removals"
			);
		}
	}

	#[test]
	fn test_reddit_web_html_canonical_shell_target_requires_empty_app_and_primary_metadata() {
		let canonical = r#"<div id="canonical-url-updater" value="https://www.reddit.com/r/Rust/hot/"></div>"#;
		let shell = format!("<shreddit-app>{canonical}</shreddit-app>");
		assert_eq!(canonical_shell_target(&shell, &names()).unwrap().as_deref(), Some("https://www.reddit.com/r/Rust/hot/"));
		assert!(matches!(parse(&shell, None, &names(), false), Err(WebError::Temporary("empty_web_app_shell"))));
		for page in [
			format!("{shell}<shreddit-feed>{POST}</shreddit-feed>"),
			format!("<shreddit-app><div slot=\"comment\">{canonical}</div></shreddit-app>"),
			format!("<shreddit-app><aside>{canonical}</aside></shreddit-app>"),
		] {
			assert!(canonical_shell_target(&page, &names()).unwrap().is_none());
		}
		assert_eq!(
			canonical_shell_target(&format!("{shell}<shreddit-content-gate reason=\"private\"></shreddit-content-gate>"), &names()).err(),
			Some(WebError::Resource("private"))
		);
	}

	#[test]
	fn test_reddit_web_html_modern_video_sources_and_preview_wrapper_text() {
		let player = r#"<shreddit-player src="https://v.redd.it/clip/HLSPlaylist.m3u8?a=fixture" preview="https://v.redd.it/clip/CMAF_96.mp4" packaged-media-json='{"playbackMp4s":{"permutations":[{"source":{"url":"https://packaged-media.redd.it/clip/pb/720.mp4","videoCodec":"H264","dimensions":{"width":1280,"height":720}}}]}}'></shreddit-player>"#;
		let video = POST.replace("</shreddit-post>", &format!("{player}</shreddit-post>"));
		let data = &parse(&format!("<shreddit-feed>{video}</shreddit-feed>"), None, &names(), false).unwrap().posts[0]["data"];
		assert_eq!(data["is_video"], true);
		assert_eq!(data["secure_media"]["reddit_video"]["fallback_url"], "https://packaged-media.redd.it/clip/pb/720.mp4");
		assert_eq!(data["secure_media"]["reddit_video"]["hls_url"], "https://v.redd.it/clip/HLSPlaylist.m3u8?a=fixture");
		for player in [
			player.replace("H264", "HEVC"),
			player.replace("https://packaged-media.redd.it", "https://evil.test"),
			player.replace("playbackMp4s", "unsupported"),
		] {
			let video = POST.replace("</shreddit-post>", &format!("{player}</shreddit-post>"));
			let data = &parse(&format!("<shreddit-feed>{video}</shreddit-feed>"), None, &names(), false).unwrap().posts[0]["data"];
			assert_eq!(
				data["secure_media"]["reddit_video"]["fallback_url"], "https://v.redd.it/clip/CMAF_96.mp4",
				"unsupported packaged media must fall back to the supplied MP4"
			);
		}
		let wrapped = POST
			.replace(
				"<div slot=\"text-body\">",
				"<shreddit-post-text-body slot=\"text-body\"><a slot=\"text-body\" href=\"/r/rust/comments/abc/title/\"><div>",
			)
			.replace("</div></shreddit-post>", "</div></a></shreddit-post-text-body></shreddit-post>");
		let data = &parse(&format!("<shreddit-feed>{wrapped}</shreddit-feed>"), None, &names(), false).unwrap().posts[0]["data"];
		let body = data["selftext_html"].as_str().unwrap();
		assert!(!body.contains("<a "), "the preview navigation wrapper must not color all post text as a link");
		assert!(body.contains("<strong>body</strong>"));
		// Detail pages contain genuine user links without the feed's navigation wrapper.
		let linked = POST.replace(
			"Own <strong>body</strong>.",
			"Own <strong>body</strong>. <a href=\"https://example.com/user-link\">A real link</a>",
		);
		let data = &parse(&format!("<shreddit-feed>{linked}</shreddit-feed>"), None, &names(), false).unwrap().posts[0]["data"];
		assert!(htmlescape::decode_html(data["selftext_html"].as_str().unwrap())
			.unwrap()
			.contains("https://example.com/user-link"));
	}

	#[test]
	fn test_reddit_web_html_empty_thread_and_next_link_contracts() {
		assert!(matches!(parse(POST, Some("abc"), &names(), false), Err(WebError::Temporary("missing_comment_tree"))));
		for count in ["", "comment-count=\"unknown\"", "comment-count=\"-1\""] {
			let html = POST.replace("comment-count=\"2\"", count);
			assert!(matches!(parse(&html, Some("abc"), &names(), false), Err(WebError::Temporary("missing_comment_tree"))));
			let feed = parse(&format!("<shreddit-feed>{html}</shreddit-feed>"), None, &names(), false).unwrap();
			assert!(feed.posts[0]["data"]["num_comments"].is_null(), "unknown counts must not be invented as zero");
		}
		assert!(parse(&POST.replace("comment-count=\"2\"", "comment-count=\"0\""), Some("abc"), &names(), false).is_ok());
		let html =
			format!("<shreddit-feed>{POST}</shreddit-feed><a rel=\"next\" href=\"/r/rust/new/?after=t3_abc\">next</a><a rel=\"next\" href=\"https://evil.test/r/rust/\">bad</a>");
		assert_eq!(parse(&html, None, &names(), false).unwrap().next.as_deref(), Some("/r/rust/new/?after=t3_abc"));
		assert!(parse(&html.replace("/r/rust/new/?after=t3_abc", "/r/other/new/?after=t3_abc"), None, &names(), false)
			.unwrap()
			.next
			.is_none());
	}

	#[test]
	fn test_reddit_web_html_feed_root_excludes_sidebar_recommendation_shell_and_nested_cards() {
		for html in [
			POST.to_string(),
			format!("<aside><shreddit-feed>{POST}</shreddit-feed></aside>"),
			format!("<div id=\"right-sidebar-container\"><shreddit-feed>{POST}</shreddit-feed></div>"),
		] {
			assert!(matches!(parse(&html, None, &[], false), Err(WebError::Temporary("missing_web_content"))));
		}
		let nested = POST.replace("Own <strong>body</strong>.", "Own content").replace(
			"</shreddit-post>",
			&format!("{} </shreddit-post>", POST.replace("t3_abc", "t3_other").replace("/comments/abc/", "/comments/other/")),
		);
		let doc = parse(&format!("<shreddit-feed>{nested}</shreddit-feed>"), None, &[], false).unwrap();
		assert_eq!(doc.posts.len(), 1);
		assert_eq!(doc.posts[0]["data"]["name"], "t3_abc");
		for source in [
			"/svc/shreddit/community-more-posts/new/?name=elsewhere",
			"/svc/shreddit/community-more-posts/new/",
			"/svc/shreddit/feeds/anything",
		] {
			assert!(parse(&format!("<faceplate-partial src=\"{source}\"></faceplate-partial>"), None, &names(), false).is_err());
		}
		let scoped = parse(
			"<faceplate-partial src=\"/svc/shreddit/community-more-posts/new/?name=rust\"></faceplate-partial>",
			None,
			&names(),
			false,
		)
		.unwrap();
		assert_eq!(scoped.fragments.len(), 1);
		assert!(scoped.posts.is_empty());
	}
	#[test]
	fn test_reddit_web_html_permalink_normalization_and_sidebar_comments() {
		for path in ["/r/rust/comments/abc", "/r/rust/comments/abc/", "/r/rust/comments/abc/title/one/extra"] {
			let html = page().replace("permalink=\"/r/rust/comments/abc/title/\"", &format!("permalink=\"{path}\""));
			let doc = parse(&html, Some("abc"), &names(), false).unwrap();
			let expected = if path.contains("title") {
				"/r/rust/comments/abc/title/"
			} else {
				"/r/rust/comments/abc/_/"
			};
			assert_eq!(doc.posts[0]["data"]["permalink"], expected);
		}
		for wrapper in ["aside", "shreddit-sidebar"] {
			let html = format!("{POST}<{wrapper}>{COMMENTS_HTML}</{wrapper}>");
			assert!(matches!(parse(&html, Some("abc"), &names(), false), Err(WebError::Temporary("missing_comment_tree"))));
		}
		let wrong_post = format!(
			"{}{}",
			POST,
			POST
				.replace("t3_abc", "t3_wrong")
				.replace("/comments/abc/", "/comments/wrong/")
				.replace("</shreddit-post>", &format!("{COMMENTS_HTML}</shreddit-post>"))
		);
		assert!(matches!(parse(&wrong_post, Some("abc"), &names(), false), Err(WebError::Temporary("missing_comment_tree"))));
	}

	#[test]
	fn test_reddit_web_html_video_sources_require_known_proxy_destinations() {
		let video = POST.replace("</shreddit-post>", "<shreddit-player src=\"https://v.redd.it/clip/DASH_720.mp4\" hls-src=\"https://v.redd.it/clip/HLSPlaylist.m3u8\" width=\"720\" height=\"480\"></shreddit-player></shreddit-post>");
		let doc = parse(&format!("<shreddit-feed>{video}</shreddit-feed>"), None, &names(), false).unwrap();
		assert_eq!(doc.posts[0]["data"]["is_video"], true);
		assert!(doc.posts[0]["data"]["secure_media"]["reddit_video"]["fallback_url"]
			.as_str()
			.unwrap()
			.contains("DASH_720.mp4"));
		let unsupported = video
			.replace("https://v.redd.it/clip/DASH_720.mp4", "https://evil.test/video.mp4")
			.replace("https://v.redd.it/clip/HLSPlaylist.m3u8", "https://evil.test/playlist.m3u8");
		let doc = parse(&format!("<shreddit-feed>{unsupported}</shreddit-feed>"), None, &names(), false).unwrap();
		assert_eq!(doc.posts[0]["data"]["is_video"], false);
		assert!(doc.posts[0]["data"]["secure_media"].is_null());
		for player in [
			r#"<shreddit-player src="https://v.redd.it/clip/HLSPlaylist.m3u8" preview="https://v.redd.it/clip/CMAF_96.mp4"></shreddit-player>"#,
			r#"<shreddit-player src="https://v.redd.it/clip/HLSPlaylist.m3u8"></shreddit-player>"#,
			r#"<shreddit-player src="https://v.redd.it/clip/DASH_720.mp4"></shreddit-player>"#,
		] {
			let html = POST.replace("</shreddit-post>", &format!("{player}</shreddit-post>"));
			assert_eq!(
				parse(&format!("<shreddit-feed>{html}</shreddit-feed>"), None, &names(), false).unwrap().posts[0]["data"]["is_video"],
				true
			);
		}
		for url in [
			"http://v.redd.it/clip/DASH_720.mp4",
			"https://evil.test/clip/DASH_720.mp4",
			"https://v.redd.it:444/clip/DASH_720.mp4",
			"https://v.redd.it/clip/not-video.txt",
			"https://alice@v.redd.it/clip/DASH_720.mp4",
		] {
			assert!(video_source(url, false).is_none());
		}
	}
	#[test]
	fn test_reddit_web_html_metadata_mode_preserves_scope_without_requiring_initial_comments() {
		for html in [
			POST.to_string(),
			POST.replace("comment-count=\"2\"", ""),
			POST.replace("comment-count=\"2\"", "comment-count=\"unknown\""),
		] {
			let metadata = parse_metadata(&html, "abc", &names(), false).unwrap();
			assert_eq!(metadata.posts.len(), 1);
			assert_eq!(metadata.posts[0]["data"]["name"], "t3_abc");
			assert!(metadata.comments.is_empty());
			assert!(matches!(parse(&html, Some("abc"), &names(), false), Err(WebError::Temporary("missing_comment_tree"))));
		}
		let broken_unused_tree = format!("{POST}<shreddit-comment thingid=\"t1_bad\" author=\"alice\"></shreddit-comment>");
		assert!(parse_metadata(&broken_unused_tree, "abc", &names(), false).unwrap().comments.is_empty());
		assert!(matches!(
			parse(&broken_unused_tree, Some("abc"), &names(), false),
			Err(WebError::Temporary("missing_comment_body"))
		));
		for html in [
			COMMENTS_HTML.to_string(),
			POST.replace("t3_abc", "t3_other"),
			POST.replace("subreddit-name=\"rust\"", "subreddit-name=\"wrong\""),
		] {
			assert!(matches!(parse_metadata(&html, "abc", &names(), false), Err(WebError::Temporary(_))));
		}
		assert!(matches!(
			parse_metadata(&format!("<shreddit-content-gate reason=\"private\"></shreddit-content-gate>{POST}"), "abc", &names(), false),
			Err(WebError::Resource("private"))
		));
	}

	#[test]
	fn test_reddit_web_html_opaque_community_id_candidate_requires_independent_post_scope() {
		let shell = "<shreddit-feed><faceplate-partial src=\"/svc/shreddit/community-more-posts/new/?name=t5_abc\"></faceplate-partial></shreddit-feed>";
		let source = parse(shell, None, &names(), false).unwrap();
		assert_eq!(source.fragments, ["/svc/shreddit/community-more-posts/new/?name=t5_abc"]);
		assert!(source.posts.is_empty());
		let returned = format!("<shreddit-feed>{POST}</shreddit-feed>");
		assert_eq!(parse(&returned, None, &names(), false).unwrap().posts[0]["data"]["subreddit"], "rust");
		let wrong = returned
			.replace("subreddit-name=\"rust\"", "subreddit-name=\"elsewhere\"")
			.replace("/r/rust/", "/r/elsewhere/");
		assert!(matches!(parse(&wrong, None, &names(), false), Err(WebError::Temporary("missing_web_content"))));
		assert!(parse(shell, None, &[], false).is_err(), "an opaque single-community ID cannot stand in for All/Popular");
		for name in ["elsewhere", "t5_", "t5_bad/id", "t5_abc&name=rust", "rust&name=elsewhere"] {
			assert!(parse(&shell.replace("t5_abc", name), None, &names(), false).is_err());
		}
	}
	#[test]
	fn test_reddit_web_html_restrictions_belong_to_visible_main_content_not_auxiliary_ui() {
		let private = "<shreddit-content-gate reason=\"private\"></shreddit-content-gate>";
		let login = "<h2>Log in to continue</h2>";
		let ui = format!("<aside>{private}</aside><div id=\"right-sidebar-container\">{private}</div><dialog>{login}{private}</dialog><div role=\"dialog\" hidden>{login}{private}</div><div aria-hidden=\"true\">{login}{private}</div><div style=\"display: none !important\">{login}{private}</div><div class=\"hidden\">{login}{private}</div>");
		let thread = format!("<main>{POST}{COMMENTS_HTML}</main>{ui}");
		assert_eq!(parse(&thread, Some("abc"), &names(), false).unwrap().posts.len(), 1);
		assert_eq!(parse_metadata(&thread, "abc", &names(), false).unwrap().posts.len(), 1);
		let feed = format!("<main><shreddit-feed>{POST}</shreddit-feed></main>{ui}");
		assert_eq!(parse(&feed, None, &names(), false).unwrap().posts.len(), 1);
		for wall in [format!("<main>{private}</main>"), format!("<dialog open>{private}</dialog>")] {
			assert!(matches!(
				parse(&format!("{wall}{POST}{COMMENTS_HTML}"), Some("abc"), &names(), false),
				Err(WebError::Resource("private"))
			));
		}
		assert!(matches!(
			parse(
				&format!("<html hidden><head><title>Reddit - Prove your humanity</title></head><body>{POST}{COMMENTS_HTML}</body></html>"),
				Some("abc"),
				&names(),
				false
			),
			Err(WebError::Temporary("web_access_wall"))
		));
	}
	#[test]
	fn test_reddit_web_html_fixed_old_frontend_permalink_and_next_preserve_fetch_origin() {
		let old = r#"<div id="siteTable"><div class="thing link" data-fullname="t3_old" data-author="alice" data-subreddit="rust" data-permalink="https://old.reddit.com/r/rust/comments/old/title/" data-url="https://example.com/story"><a class="title">Old All post</a></div></div><span class="next-button"><a href="https://old.reddit.com/r/all/?after=t3_old">next</a></span>"#;
		let doc = parse(old, None, &[], false).unwrap();
		assert_eq!(doc.posts[0]["data"]["permalink"], "/r/rust/comments/old/title/");
		assert_eq!(doc.next.as_deref(), Some("https://old.reddit.com/r/all/?after=t3_old"));
		let cross_frontend = old.replace("href=\"https://old.reddit.com/r/all/", "href=\"https://www.reddit.com/r/all/");
		assert_eq!(
			parse(&cross_frontend, None, &[], false).unwrap().next.as_deref(),
			Some("https://www.reddit.com/r/all/?after=t3_old"),
			"caller must see and reject a frontend change rather than silently rebasing it"
		);
		for host in ["old.reddit.com.evil.test", "evil.test", "user@old.reddit.com", "old.reddit.com:444"] {
			assert!(parse(&old.replace("old.reddit.com", host), None, &[], false).is_err());
		}
	}
}
