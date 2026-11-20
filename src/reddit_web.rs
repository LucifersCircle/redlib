//! Default anonymous HTML reader. No API tokens or API admission state enter this module.
use super::*;
use futures_lite::StreamExt;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::sync::atomic::AtomicUsize;
use wreq::cookie::Jar;

#[path = "reddit_web_html.rs"]
mod html;

const ORIGIN: &str = "https://www.reddit.com";
const PREFIX: &str = "redlib-web:";
const MAX_HTML: usize = 8 * 1024 * 1024;
const MAX_TOTAL: usize = 64 * 1024 * 1024;
const MAX_WIRES: usize = 64;
static CAPACITY: Semaphore = Semaphore::const_new(4);
static STANDBY: Semaphore = Semaphore::const_new(2);
static FLIGHTS: LazyLock<JsonFlightMap> = LazyLock::new(|| AsyncMutex::new(HashMap::new()));

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum WebError {
	Temporary(&'static str),
	Resource(&'static str),
	Unsupported,
	Cursor,
	RateLimited(Duration),
}
impl WebError {
	fn message(&self) -> String {
		match self {
			Self::Temporary(_) => "Reddit is temporarily unable to load this page. Retry in 30 seconds".into(),
			Self::Resource("private") => "This subreddit is private".into(),
			Self::Resource("banned") => "This subreddit is banned".into(),
			Self::Resource("quarantined") => "quarantined".into(),
			Self::Resource("gated") => "gated".into(),
			Self::Resource("login_required") => "Reddit requires a signed-in account to view this page".into(),
			Self::Resource(_) => "Post not found".into(),
			Self::Unsupported => "The anonymous web reader currently supports public feeds and post pages".into(),
			Self::Cursor => "Page navigation expired. Open the first page again.".into(),
			Self::RateLimited(delay) => format!("Reddit rate limit exceeded. Retry in {} seconds", retry_after_seconds(*delay)),
		}
	}
	fn retry_lane(&self) -> bool {
		matches!(self, Self::Temporary(_))
	}
	fn label(&self) -> &'static str {
		match self {
			Self::Temporary(reason) | Self::Resource(reason) => reason,
			Self::Unsupported => "unsupported",
			Self::Cursor => "cursor",
			Self::RateLimited(_) => "quota",
		}
	}
}

pub(super) fn enabled() -> bool {
	env::var("REDLIB_READ_BACKEND").map_or(true, |value| value == "web")
}
pub(super) fn contains_cursor(path: &str) -> bool {
	url::form_urlencoded::parse(path.split_once('?').map_or("", |(_, query)| query).as_bytes())
		.any(|(key, value)| matches!(key.as_ref(), "after" | "before") && value.starts_with(PREFIX))
}

#[derive(Clone, Debug)]
struct Spec {
	communities: Vec<String>,
	sort: String,
	range: String,
	post: Option<String>,
	post_slug: String,
	post_community: Option<String>,
	focus: Option<String>,
	limit: usize,
	scope: String,
	context: u16,
	cursor: Option<Cursor>,
	recover: bool,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Source {
	page: Option<String>,
	last: Option<String>,
	done: bool,
	served: u64,
	#[serde(default)]
	form: Option<Form>,
	#[serde(default)]
	resume: Option<String>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Form {
	cursor: String,
	parent: String,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Cursor {
	scope: String,
	sources: Vec<Source>,
	tor: bool,
	#[serde(skip)]
	session: Option<Arc<Transport>>,
}
static CURSORS: LazyLock<Mutex<cached::TimedSizedCache<String, Cursor>>> =
	LazyLock::new(|| Mutex::new(cached::TimedSizedCache::with_size_and_lifespan(512, Duration::from_secs(3600))));
fn valid_name(value: &str) -> bool {
	!value.is_empty() && value.len() <= 64 && value.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
}
fn valid_id(value: &str) -> bool {
	!value.is_empty() && value.len() <= 20 && value.bytes().all(|b| b.is_ascii_alphanumeric())
}
impl Spec {
	fn origin(&self) -> &'static str {
		if self.post.is_none() && self.communities.as_slice() == ["all"] {
			"https://old.reddit.com"
		} else {
			ORIGIN
		}
	}
	fn parse(path: &str, quarantine: bool) -> Result<Self, WebError> {
		let (base, query) = path.split_once('?').unwrap_or((path, ""));
		let base = base.strip_suffix(".json").unwrap_or(base).trim_end_matches('/');
		let pairs: Vec<_> = url::form_urlencoded::parse(query.as_bytes()).collect();
		let get = |key: &str| pairs.iter().find(|(k, _)| k == key).map(|(_, v)| v.as_ref());
		if get("geo_filter").is_some_and(|value| !value.is_empty() && value != "GLOBAL") {
			return Err(WebError::Unsupported);
		}
		if pairs.iter().filter(|(k, v)| matches!(k.as_ref(), "after" | "before") && !v.is_empty()).count() > 1 || get("before").is_some_and(|v| !v.is_empty()) {
			return Err(WebError::Cursor);
		}
		let mut spec = Self {
			communities: vec![],
			sort: "hot".into(),
			range: get("t").filter(|value| !value.is_empty()).unwrap_or("all").into(),
			post: None,
			post_slug: "_".into(),
			post_community: None,
			focus: None,
			limit: get("limit").and_then(|v| v.parse().ok()).unwrap_or(25).clamp(1, 100),
			scope: String::new(),
			context: get("context").and_then(|v| v.parse::<u16>().ok()).filter(|v| *v <= 9999).unwrap_or(0),
			cursor: None,
			recover: false,
		};
		let segments: Vec<_> = base.trim_start_matches('/').split('/').collect();
		if segments.contains(&"comments") {
			let at = segments.iter().position(|s| *s == "comments").unwrap();
			if !(at == 0 || (at == 2 && segments[0] == "r")) || segments.len() < at + 2 || segments.len() > at + 4 {
				return Err(WebError::Unsupported);
			}
			let id = segments[at + 1];
			if !valid_id(id) {
				return Err(WebError::Unsupported);
			}
			spec.post = Some(id.into());
			if let Some(slug) = segments.get(at + 2).filter(|slug| !slug.is_empty()) {
				if slug.len() > 1800 || matches!(*slug, "." | "..") || !slug.bytes().all(|b| b.is_ascii_alphanumeric() || b"_.~-%".contains(&b)) {
					return Err(WebError::Unsupported);
				}
				spec.post_slug = (*slug).into();
			}
			if at == 2 {
				if !valid_name(segments[1]) {
					return Err(WebError::Unsupported);
				}
				spec.communities.push(segments[1].to_ascii_lowercase());
				spec.post_community = Some(segments[1].into());
			}
			if let Some(focus) = segments.get(at + 3).copied().or_else(|| get("comment")) {
				if !valid_id(focus) {
					return Err(WebError::Cursor);
				}
				spec.focus = Some(focus.into());
			}
			spec.sort = get("sort").filter(|s| !s.is_empty()).unwrap_or("confidence").into();
			if !matches!(spec.sort.as_str(), "confidence" | "top" | "new" | "controversial" | "old" | "random" | "qa" | "live") {
				return Err(WebError::Unsupported);
			}
		} else {
			let (names, sort) = match segments.as_slice() {
				["r", name] => (*name, "hot"),
				["r", name, sort] => (*name, *sort),
				[""] => ("popular", "hot"),
				_ => return Err(WebError::Unsupported),
			};
			if !matches!(sort, "hot" | "new" | "top" | "rising" | "controversial" | "best") {
				return Err(WebError::Unsupported);
			}
			spec.communities = names
				.replace("%2B", "+")
				.replace("%2b", "+")
				.split('+')
				.map(|name| {
					if name.eq_ignore_ascii_case("all") || name.eq_ignore_ascii_case("popular") {
						name.to_ascii_lowercase()
					} else {
						name.to_string()
					}
				})
				.collect();
			if spec.communities.is_empty() || spec.communities.len() > 32 || !spec.communities.iter().all(|s| valid_name(s)) {
				return Err(WebError::Unsupported);
			}
			spec.communities.sort_by_key(|name| name.to_ascii_lowercase());
			spec.communities.dedup_by(|a, b| a.eq_ignore_ascii_case(b));
			spec.sort = sort.into();
		}
		if !matches!(spec.range.as_str(), "hour" | "day" | "week" | "month" | "year" | "all") {
			return Err(WebError::Unsupported);
		}
		spec.scope = format!(
			"{}|{}|{}|{}|{}|{}|{}|{}",
			spec.communities.iter().map(|name| name.to_ascii_lowercase()).collect::<Vec<_>>().join("+"),
			spec.post.as_deref().unwrap_or(""),
			spec.focus.as_deref().unwrap_or(""),
			spec.sort,
			spec.range,
			spec.limit,
			spec.context,
			quarantine
		);
		if let Some(raw) = get("after").filter(|v| !v.is_empty()) {
			if raw.len() > 8192 {
				return Err(WebError::Cursor);
			}
			let token = raw.strip_prefix(PREFIX).ok_or(WebError::Cursor)?;
			if token.len() != 32 || !token.bytes().all(|b| b.is_ascii_hexdigit()) {
				return Err(WebError::Cursor);
			}
			let cursor = cached::Cached::cache_get(&mut *CURSORS.lock().unwrap(), token).cloned().ok_or(WebError::Cursor)?;
			let count = if spec.post.is_some() { 1 } else { spec.communities.len() };
			if cursor.scope != spec.scope
				|| cursor.sources.len() != count
				|| cursor
					.sources
					.iter()
					.any(|s| s.last.as_ref().is_some_and(|id| !id.strip_prefix("t3_").is_some_and(valid_id)))
			{
				return Err(WebError::Cursor);
			}
			for (i, source) in cursor.sources.iter().enumerate() {
				if source
					.resume
					.as_ref()
					.is_some_and(|value| !value.strip_prefix(if spec.post.is_some() { "t1_" } else { "t3_" }).is_some_and(valid_id))
				{
					return Err(WebError::Cursor);
				}
				if let Some(page) = &source.page {
					spec.validate_page(page, spec.communities.get(i).map(String::as_str))?;
				}
				if let Some(form) = &source.form {
					if spec.post.is_none()
						|| !source.page.as_ref().is_some_and(|p| p.starts_with("/svc/shreddit/more-comments/"))
						|| form.parent != format!("t3_{}", spec.post.as_deref().unwrap())
						|| form.cursor.is_empty()
						|| form.cursor.len() > 4096
						|| form.cursor.chars().any(char::is_control)
					{
						return Err(WebError::Cursor);
					}
				} else if source.page.as_ref().is_some_and(|p| p.starts_with("/svc/shreddit/more-comments/")) {
					return Err(WebError::Cursor);
				}
			}
			spec.cursor = Some(cursor);
		}
		Ok(spec)
	}
	fn initial(&self, community: Option<&str>) -> String {
		if let Some(post) = &self.post {
			let prefix = self.post_community.as_deref().or(community).map_or(String::new(), |s| format!("/r/{s}"));
			let focus = self.focus.as_ref().map_or_else(|| format!("{}/", self.post_slug), |s| format!("{}/{s}/", self.post_slug));
			let sort = if self.sort == "confidence" { "best" } else { &self.sort };
			format!("{prefix}/comments/{post}/{focus}?sort={sort}&context={}", self.context)
		} else {
			format!("/r/{}/{}/?t={}&limit={}", community.unwrap_or("popular"), self.sort, self.range, self.limit)
		}
	}
	fn validate_page(&self, path: &str, community: Option<&str>) -> Result<String, WebError> {
		if path.len() > 2048 || path.chars().any(char::is_control) || path.contains('\\') {
			return Err(WebError::Cursor);
		}
		let base = url::Url::parse(self.origin()).unwrap();
		let url = base.join(path).map_err(|_| WebError::Cursor)?;
		if url.scheme() != "https"
			|| url.host_str() != base.host_str()
			|| url.port().is_some()
			|| !url.username().is_empty()
			|| url.password().is_some()
			|| url.fragment().is_some()
		{
			return Err(WebError::Cursor);
		}
		let p = url.path();
		let parts: Vec<_> = p.trim_matches('/').split('/').collect();
		let valid = if let Some(post) = &self.post {
			let regular = parts.iter().position(|s| *s == "comments").is_some_and(|at| {
				(at == 0 || (at == 2 && parts[0] == "r" && community.is_none_or(|s| parts[1].eq_ignore_ascii_case(s))))
					&& parts.get(at + 1) == Some(&post.as_str())
					&& parts.len() <= at + 4
			});
			let fragment = parts.starts_with(&["svc", "shreddit", "comments", "r"])
				&& parts.get(5).is_some_and(|id| *id == format!("t3_{post}"))
				&& community.is_none_or(|s| parts.get(4).is_some_and(|v| v.eq_ignore_ascii_case(s)))
				&& parts.len() <= 7;
			let more = parts.starts_with(&["svc", "shreddit", "more-comments"])
				&& parts.len() == 5
				&& parts[4] == format!("t3_{post}")
				&& community.is_none_or(|s| parts[3].eq_ignore_ascii_case(s));
			regular || fragment || more
		} else {
			let name = community.ok_or(WebError::Cursor)?;
			let names = url.query_pairs().filter(|(key, _)| key == "name").map(|(_, value)| value.into_owned()).collect::<Vec<_>>();
			let community_fragment = parts.starts_with(&["svc", "shreddit", "community-more-posts"])
				&& parts.len() == 4
				&& parts[3].eq_ignore_ascii_case(&self.sort)
				&& names.len() == 1
				&& (names[0].eq_ignore_ascii_case(name) || (!matches!(name, "all" | "popular") && names[0].strip_prefix("t5_").is_some_and(valid_id)));
			let popular_fragment = name == "popular" && parts.as_slice() == ["svc", "shreddit", "feeds", "popular-feed"];
			(parts.len() >= 2 && parts.len() <= 3 && parts[0] == "r" && parts[1].eq_ignore_ascii_case(name) && parts.get(2).is_none_or(|s| *s == self.sort))
				|| community_fragment
				|| popular_fragment
		};
		if !valid {
			return Err(WebError::Cursor);
		}
		for (key, value) in url.query_pairs() {
			if value.len() > 1024
				|| !matches!(
					key.as_ref(),
					"after"
						| "before"
						| "count"
						| "limit"
						| "t"
						| "sort"
						| "feed"
						| "cursor"
						| "comment"
						| "context"
						| "depth"
						| "show"
						| "distance"
						| "f"
						| "adDistance"
						| "ad_posts_served"
						| "navigationSessionId"
						| "feedLength"
						| "ad_id"
						| "loadAfter"
						| "sr_detail"
						| "rdt"
						| "name"
						| "top-level"
						| "comments-remaining"
						| "startingDepth"
						| "commentPositionOffset"
						| "commentParentPositions"
				) {
				return Err(WebError::Cursor);
			}
			let same_sort = value.eq_ignore_ascii_case(&self.sort) || (matches!(self.sort.as_str(), "hot" | "confidence") && value.eq_ignore_ascii_case("best"));
			if (key == "sort" && !same_sort) || (key == "t" && !value.eq_ignore_ascii_case(&self.range)) || key == "before" {
				return Err(WebError::Cursor);
			}
			if matches!(key.as_ref(), "top-level") && !matches!(value.as_ref(), "true" | "false" | "1" | "0") {
				return Err(WebError::Cursor);
			}
			if matches!(key.as_ref(), "comments-remaining" | "startingDepth" | "commentPositionOffset") && !value.parse::<u32>().is_ok_and(|n| n <= 100000) {
				return Err(WebError::Cursor);
			}
			if key == "rdt" && (value.len() > 20 || value.is_empty() || !value.bytes().all(|b| b.is_ascii_digit())) {
				return Err(WebError::Cursor);
			}
			if key == "commentParentPositions" && !value.bytes().all(|b| b.is_ascii_digit() || b"[], ".contains(&b)) {
				return Err(WebError::Cursor);
			}
			if matches!(key.as_ref(), "ad_posts_served" | "feedLength") && !value.parse::<u32>().is_ok_and(|n| n <= 1_000_000) {
				return Err(WebError::Cursor);
			}
			if key == "navigationSessionId" && uuid::Uuid::parse_str(&value).is_err() {
				return Err(WebError::Cursor);
			}
		}
		Ok(format!("{}{}", url.path(), url.query().map_or(String::new(), |q| format!("?{q}"))))
	}
	fn script_action(&self, page: &str, action: &str, community: Option<&str>) -> Result<String, WebError> {
		let checked = self.validate_page(action, community).map_err(|_| WebError::Temporary("script_check_scope"))?;
		let base = url::Url::parse(self.origin()).unwrap();
		let current = base.join(page).map_err(|_| WebError::Temporary("script_check_scope"))?;
		let mut answer = base.join(&checked).map_err(|_| WebError::Temporary("script_check_scope"))?;
		if current.path().trim_end_matches('/') != answer.path().trim_end_matches('/') || answer.query_pairs().any(|pair| !current.query_pairs().any(|original| original == pair))
		{
			return Err(WebError::Temporary("script_check_scope"));
		}
		// Keep sort, range, pagination, focus and limits from the requested page.
		// Challenge fields are appended to this one request, never to saved cursors.
		answer.set_query(current.query());
		Ok(format!("{}{}", answer.path(), answer.query().map_or(String::new(), |q| format!("?{q}"))))
	}
	fn canonical_case_page(&self, page: &str, target: &str, community: Option<&str>) -> Result<Option<String>, WebError> {
		let checked = self.validate_page(target, community).map_err(|_| WebError::Temporary("canonical_scope"))?;
		let base = url::Url::parse(self.origin()).unwrap();
		let current = base.join(page).map_err(|_| WebError::Temporary("canonical_scope"))?;
		let mut next = base.join(&checked).map_err(|_| WebError::Temporary("canonical_scope"))?;
		if !current.path().eq_ignore_ascii_case(next.path())
			|| next.query_pairs().any(|(key, value)| {
				!current
					.query_pairs()
					.any(|(k, v)| key == k && (value == v || (matches!(key.as_ref(), "sort" | "t") && value.eq_ignore_ascii_case(&v))))
			}) {
			return Err(WebError::Temporary("canonical_scope"));
		}
		if current.path() == next.path() {
			return Ok(None);
		}
		next.set_query(current.query());
		Ok(Some(format!("{}{}", next.path(), next.query().map_or(String::new(), |q| format!("?{q}")))))
	}
}

#[derive(Default)]
struct Health {
	until: Option<Instant>,
}
impl Health {
	fn pause(&mut self, delay: Duration) {
		let until = Instant::now() + delay;
		self.until = Some(self.until.map_or(until, |prior| prior.max(until)));
	}
}
struct Transport {
	client: Arc<WreqClient>,
	health: Mutex<Health>,
}
impl std::fmt::Debug for Transport {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		f.debug_struct("Transport").finish_non_exhaustive()
	}
}
static TRANSPORTS: LazyLock<[Result<Arc<Transport>, String>; 4]> =
	LazyLock::new(|| std::array::from_fn(|i| build_transport(if i < 2 { RedditLane::Direct } else { RedditLane::Tor }).map(Arc::new)));
static TOR_SESSIONS: LazyLock<[Mutex<Result<Arc<Transport>, String>>; 2]> = LazyLock::new(|| std::array::from_fn(|i| Mutex::new(TRANSPORTS[2 + i].clone())));

fn active_transport(lane: RedditLane, quarantine: bool) -> Result<Arc<Transport>, WebError> {
	#[cfg(test)]
	if let Some((transport, _)) = MOCK.lock().unwrap().as_ref() {
		return Ok(transport.clone());
	}
	let result = if lane == RedditLane::Tor {
		TOR_SESSIONS[usize::from(quarantine)].lock().unwrap().clone()
	} else {
		TRANSPORTS[usize::from(quarantine)].clone()
	};
	result.map_err(|_| WebError::Temporary("transport"))
}

fn renew_tor_transport(previous: &Arc<Transport>, quarantine: bool) -> Result<Arc<Transport>, WebError> {
	#[cfg(test)]
	{
		let mut mock = MOCK.lock().unwrap();
		if let Some((current, _)) = mock.as_ref() {
			if Arc::ptr_eq(current, previous) {
				if let Some(next) = RENEW_MOCK.lock().unwrap().clone() {
					*mock = Some(next);
					TEST_RENEWALS.fetch_add(1, Ordering::SeqCst);
				}
			}
			return Ok(mock.as_ref().unwrap().0.clone());
		}
	}
	let mut slot = TOR_SESSIONS[usize::from(quarantine)].lock().unwrap();
	if slot.as_ref().is_ok_and(|current| Arc::ptr_eq(current, previous)) {
		let next = Arc::new(build_transport(RedditLane::Tor).map_err(|_| WebError::Temporary("transport"))?);
		*slot = Ok(next);
		info!("Renewed anonymous Reddit web identity after quota exhaustion: lane=tor");
	}
	slot.clone().map_err(|_| WebError::Temporary("transport"))
}
fn build_transport(lane: RedditLane) -> Result<Transport, String> {
	let emulation = Emulation::builder().profile(Profile::Chrome145).platform(Platform::Windows).headers(true).build();
	let mut builder = WreqClient::builder()
		.emulation(emulation)
		.redirect(Policy::none())
		.cookie_provider(Arc::new(Jar::default()));
	if lane == RedditLane::Direct {
		builder = builder.dns_resolver(RedditDnsResolver);
	} else {
		let conf = TOR_FALLBACK_CONFIG.as_ref().map_err(Clone::clone)?.as_ref().ok_or("Tor web transport is disabled")?;
		let isolated = tor_isolation_proxy_url(&conf.proxy_url, &format!("redlib-web-{:016x}", fastrand::u64(..)))?;
		builder = builder.proxy(Proxy::all(isolated).map_err(|_| "Invalid Tor web proxy")?);
	}
	Ok(Transport {
		client: Arc::new(builder.build().map_err(|_| "Could not build anonymous web transport")?),
		health: Mutex::new(Health::default()),
	})
}
struct Budget {
	wires: Arc<AtomicUsize>,
	bytes: Arc<AtomicUsize>,
	terminal: Mutex<Option<WebError>>,
	quota: Mutex<Option<Duration>>,
}
impl Budget {
	fn finish(&self, result: Result<Value, WebError>) -> Result<Value, WebError> {
		if let Some(error) = self.terminal.lock().unwrap().clone() {
			return Err(error);
		}
		if result.is_err() {
			if let Some(delay) = *self.quota.lock().unwrap() {
				return Err(WebError::RateLimited(delay));
			}
		}
		result
	}
	fn new() -> Self {
		Self {
			wires: Arc::new(AtomicUsize::new(0)),
			bytes: Arc::new(AtomicUsize::new(0)),
			terminal: Mutex::new(None),
			quota: Mutex::new(None),
		}
	}
	fn new_attempt(&self) -> Self {
		Self {
			wires: self.wires.clone(),
			bytes: self.bytes.clone(),
			terminal: Mutex::new(None),
			quota: Mutex::new(None),
		}
	}
}
#[cfg(test)]
static MOCK: LazyLock<Mutex<Option<(Arc<Transport>, String)>>> = LazyLock::new(|| Mutex::new(None));
#[cfg(test)]
static RENEW_MOCK: LazyLock<Mutex<Option<(Arc<Transport>, String)>>> = LazyLock::new(|| Mutex::new(None));
#[cfg(test)]
static TEST_RENEWALS: AtomicUsize = AtomicUsize::new(0);
#[derive(Clone)]
struct Context {
	lane: RedditLane,
	quarantine: bool,
	budget: Arc<Budget>,
	session: Arc<Transport>,
	#[cfg(test)]
	origin: Option<String>,
}
impl Context {
	fn note(&self, error: WebError) -> WebError {
		if matches!(error, WebError::RateLimited(_) | WebError::Resource(_)) {
			let mut terminal = self.budget.terminal.lock().unwrap();
			if terminal.is_none() {
				*terminal = Some(error.clone());
			}
		}
		error
	}
	fn transport(&self) -> Result<&Transport, WebError> {
		Ok(&self.session)
	}
	async fn fetch(&self, spec: &Spec, page: &str, community: Option<&str>, form: Option<&Form>) -> Result<String, WebError> {
		let mut page = spec.validate_page(page, community)?;
		let transport = self.transport()?;
		let pause = {
			transport
				.health
				.lock()
				.unwrap()
				.until
				.filter(|until| *until > Instant::now())
				.map(|until| until.saturating_duration_since(Instant::now()))
		};
		if let Some(delay) = pause {
			return Err(self.note(WebError::RateLimited(delay)));
		}
		let _permit = CAPACITY.acquire().await.map_err(|_| WebError::Temporary("capacity"))?;
		let mut script_fields = None;
		let mut answered_script = false;
		let mut followed_canonical = false;
		let mut redirects = 0;
		for _ in 0..5 {
			if let Some(error) = self.budget.terminal.lock().unwrap().clone() {
				return Err(error);
			}
			let pause = {
				transport
					.health
					.lock()
					.unwrap()
					.until
					.filter(|until| *until > Instant::now())
					.map(|until| until.saturating_duration_since(Instant::now()))
			};
			if let Some(delay) = pause {
				return Err(self.note(WebError::RateLimited(delay)));
			}
			if form.is_some() && !page.starts_with("/svc/shreddit/more-comments/") {
				return Err(WebError::Cursor);
			}
			let wire = self.budget.wires.fetch_add(1, Ordering::SeqCst);
			if wire >= MAX_WIRES {
				return Err(WebError::Temporary("wire_bound"));
			}
			#[cfg(not(test))]
			let origin = spec.origin();
			#[cfg(test)]
			let origin = self.origin.as_deref().unwrap_or_else(|| spec.origin());
			let mut request_url = url::Url::parse(&format!("{origin}{page}")).map_err(|_| WebError::Temporary("script_check_scope"))?;
			if let Some(fields) = script_fields.take() {
				request_url.query_pairs_mut().extend_pairs(fields);
			}
			let request = if let Some(form) = form {
				transport
					.client
					.post(request_url.as_str())
					.header(wreq_header::CONTENT_TYPE, "application/x-www-form-urlencoded")
					.body(url::form_urlencoded::Serializer::new(String::new()).append_pair("cursor", &form.cursor).finish())
			} else {
				transport.client.get(request_url.as_str())
			};
			let response = request
				.header(wreq_header::ACCEPT, "text/html, text/vnd.reddit.partial+html;q=0.9")
				.send()
				.await
				.map_err(|_| WebError::Temporary("transport"))?;
			let status = response.status().as_u16();
			let headers = response.headers();
			let remaining = headers
				.get("x-ratelimit-remaining")
				.and_then(|v| v.to_str().ok())
				.and_then(|s| parse_rate_limit_count(Some(s)));
			let retry = headers
				.get("retry-after")
				.and_then(|v| v.to_str().ok())
				.and_then(|s| parse_retry_after(Some(s), SystemTime::now()));
			if status == 429 || remaining == Some(0) {
				let reset = headers.get("x-ratelimit-reset").and_then(|v| v.to_str().ok()).and_then(|s| parse_delay_seconds(Some(s)));
				let delay = retry.or(reset).unwrap_or(Duration::from_secs(60)).max(Duration::from_secs(1));
				transport.health.lock().unwrap().pause(delay);
				let mut quota = self.budget.quota.lock().unwrap();
				*quota = Some(quota.map_or(delay, |prior| prior.max(delay)));
				drop(quota);
				if status == 429 {
					return Err(self.note(WebError::RateLimited(delay)));
				}
			}
			if (300..400).contains(&status) {
				if redirects == 2 {
					return Err(WebError::Temporary("redirect_bound"));
				}
				redirects += 1;
				let next = headers.get(wreq_header::LOCATION).and_then(|v| v.to_str().ok()).ok_or(WebError::Temporary("redirect"))?;
				let base = url::Url::parse(spec.origin()).unwrap();
				if base
					.join(next)
					.is_ok_and(|url| url.origin() == base.origin() && url.path().trim_end_matches('/') == "/login")
				{
					return Err(self.note(WebError::Resource("login_required")));
				}
				page = spec.validate_page(next, community).map_err(|_| WebError::Temporary("redirect_scope"))?;
				continue;
			}
			if status == 404 {
				return Err(self.note(WebError::Resource("post_not_found")));
			}
			if status == 401 {
				return Err(self.note(WebError::Resource("gated")));
			}
			if status >= 500 {
				return Err(WebError::Temporary("upstream"));
			}
			let html = headers
				.get(wreq_header::CONTENT_TYPE)
				.and_then(|v| v.to_str().ok())
				.is_some_and(|v| v.starts_with("text/html") || v.starts_with("text/vnd.reddit.partial+html"));
			if !html {
				return Err(WebError::Temporary("content_type"));
			}
			let mut bytes = Vec::new();
			let mut stream = response.bytes_stream();
			while let Some(chunk) = stream.next().await {
				let chunk = chunk.map_err(|_| WebError::Temporary("body"))?;
				if bytes.len().saturating_add(chunk.len()) > MAX_HTML || self.budget.bytes.fetch_add(chunk.len(), Ordering::SeqCst).saturating_add(chunk.len()) > MAX_TOTAL {
					return Err(WebError::Temporary("body_bound"));
				}
				bytes.extend_from_slice(&chunk);
			}
			let text = String::from_utf8(bytes).map_err(|_| WebError::Temporary("encoding"))?;
			#[cfg(test)]
			if let Some(dir) = env::var_os("REDLIB_LIVE_CAPTURE_DIR") {
				let dir = std::path::PathBuf::from(dir);
				let _ = std::fs::create_dir_all(&dir);
				let at = wire + 1;
				let _ = std::fs::write(dir.join(format!("response-{at}.html")), &text);
				let _ = std::fs::write(
					dir.join(format!("response-{at}.shape")),
					format!("status={status} path={} post={:?}", page.split('?').next().unwrap_or_default(), spec.post),
				);
			}
			if form.is_none() && ((200..300).contains(&status) || status == 403) {
				if let Some(check) = html::script_check(&text, &spec.communities).map_err(|error| self.note(error))? {
					if answered_script {
						return Err(WebError::Temporary("script_check_repeated"));
					}
					page = spec.script_action(&page, &check.action, community)?;
					script_fields = Some(check.fields);
					answered_script = true;
					info!("Completing anonymous Reddit script check: lane={} frontend={}", self.lane.label(), spec.origin());
					continue;
				}
			}
			if form.is_none() && (200..300).contains(&status) {
				if let Some(target) = html::canonical_shell_target(&text, &spec.communities).map_err(|error| self.note(error))? {
					if let Some(next) = spec.canonical_case_page(&page, &target, community)? {
						if followed_canonical {
							return Err(WebError::Temporary("canonical_loop"));
						}
						followed_canonical = true;
						page = next;
						info!("Following canonical Reddit web casing: lane={} frontend={}", self.lane.label(), spec.origin());
						continue;
					}
				}
			}
			if status == 403 {
				return match html::parse(&text, spec.post.as_deref(), &spec.communities, self.quarantine) {
					Err(WebError::Resource(reason)) => Err(self.note(WebError::Resource(reason))),
					_ => Err(WebError::Temporary("edge_rejected")),
				};
			}
			if !(200..300).contains(&status) {
				return Err(WebError::Temporary("http_status"));
			}
			return Ok(text);
		}
		Err(WebError::Temporary("redirect_bound"))
	}
}

async fn document(ctx: &Context, spec: &Spec, page: &str, community: Option<&str>, expand: bool) -> Result<html::Document, WebError> {
	let text = ctx.fetch(spec, page, community, None).await?;
	let names = community.filter(|s| !matches!(*s, "all" | "popular")).map_or(vec![], |s| vec![s.to_string()]);
	let text = if spec.post.is_none() && page.starts_with("/svc/shreddit/") {
		format!("<shreddit-feed>{text}</shreddit-feed>")
	} else {
		text
	};
	let mut parsed = if !expand && spec.post.is_some() {
		html::parse_metadata(&text, spec.post.as_deref().unwrap(), &names, ctx.quarantine)
	} else {
		html::parse(&text, spec.post.as_deref(), &names, ctx.quarantine)
	}
	.map_err(|error| ctx.note(error))?;
	if expand
		&& (parsed.posts.is_empty() || (spec.post.is_some() && parsed.comments.is_empty() && parsed.posts.first().is_some_and(|p| p["data"]["num_comments"].as_u64() != Some(0))))
	{
		let candidates = std::mem::take(&mut parsed.fragments);
		for fragment in candidates.into_iter().take(2) {
			let fragment = spec.validate_page(&fragment, community).map_err(|_| WebError::Temporary("fragment_scope"))?;
			let body = ctx.fetch(spec, &fragment, community, None).await?;
			let body = if spec.post.is_none() { format!("<shreddit-feed>{body}</shreddit-feed>") } else { body };
			let extra = html::parse(&body, spec.post.as_deref(), &names, ctx.quarantine).map_err(|error| ctx.note(error))?;
			if parsed.posts.is_empty() {
				parsed.posts = extra.posts;
			}
			parsed.comments.extend(extra.comments);
			parsed.more.extend(extra.more);
			parsed.next = extra.next;
			if !parsed.posts.is_empty() && (spec.post.is_none() || !parsed.comments.is_empty()) {
				break;
			}
		}
	}
	if parsed.posts.is_empty() {
		return Err(WebError::Temporary("missing_post"));
	}
	if expand && spec.post.is_some() && parsed.comments.is_empty() {
		if let Some(more) = parsed.more.iter().find(|m| m.parent == format!("t3_{}", spec.post.as_deref().unwrap())).cloned() {
			let target = spec.validate_page(&more.path, community).map_err(|_| WebError::Temporary("fragment_scope"))?;
			let form = Form {
				cursor: more.cursor,
				parent: more.parent,
			};
			let body = ctx.fetch(spec, &target, community, Some(&form)).await?;
			let extra = html::parse(&body, spec.post.as_deref(), &names, ctx.quarantine).map_err(|error| ctx.note(error))?;
			parsed.comments = extra.comments;
			parsed.more = extra.more;
			parsed.next = extra.next;
		}
	}
	if expand && spec.post.is_some() && parsed.comments.is_empty() && parsed.posts[0]["data"]["num_comments"].as_u64() != Some(0) {
		return Err(WebError::Temporary("missing_comment_tree"));
	}
	Ok(parsed)
}
fn encode_cursor(scope: &str, sources: Vec<Source>, lane: RedditLane, session: Option<Arc<Transport>>) -> Result<Option<String>, WebError> {
	if sources.iter().all(|s| s.done) {
		return Ok(None);
	}
	let token = uuid::Uuid::new_v4().simple().to_string();
	cached::Cached::cache_set(
		&mut *CURSORS.lock().unwrap(),
		token.clone(),
		Cursor {
			scope: scope.into(),
			sources,
			tor: lane == RedditLane::Tor,
			session,
		},
	);
	Ok(Some(format!("{PREFIX}{token}")))
}
fn listing(posts: Vec<Value>, after: Option<String>) -> Value {
	json!({"kind":"Listing","data":{"children":posts,"after":after,"before":null}})
}

async fn feed_source(ctx: Context, spec: Spec, name: String, source: Source) -> Result<(Vec<Value>, Source, Option<String>), WebError> {
	if source.done {
		return Ok((vec![], source, None));
	}
	let mut source = source;
	// Continuations belong to their anonymous jar. On renewal, locate the
	// stable public anchor using only links supplied to the replacement jar.
	if spec.recover {
		source.page = None;
		source.last = source.resume.clone().or(source.last);
		source.form = None;
	}
	let mut page = source.page.clone().unwrap_or_else(|| spec.initial(Some(&name)));
	let mut document = document(&ctx, &spec, &page, Some(&name), true).await?;
	if let Some(last) = &source.last {
		let last = last.clone();
		let mut found = false;
		for page_index in 0..8 {
			if let Some(index) = document.posts.iter().position(|p| p["data"]["name"].as_str() == Some(&last)) {
				document.posts.drain(..=index);
				found = true;
				break;
			}
			if page_index == 7 {
				return Err(WebError::Cursor);
			}
			let next = document.next.as_deref().ok_or(WebError::Cursor)?;
			page = spec.validate_page(next, Some(&name))?;
			document = self::document(&ctx, &spec, &page, Some(&name), true).await?;
		}
		if !found {
			return Err(WebError::Cursor);
		}
		if document.posts.is_empty() {
			if let Some(next) = document.next.as_deref() {
				page = spec.validate_page(next, Some(&name))?;
				document = self::document(&ctx, &spec, &page, Some(&name), true).await?;
				source.last = None;
			}
		}
	}
	let own_first = spec.communities.len() == 1 && !matches!(name.as_str(), "popular" | "all") && spec.cursor.is_none();
	if !own_first {
		let now = SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_secs() as i64;
		document.posts.retain(|p| {
			crate::utils::feed_post_visible(
				p["data"]["stickied"] == true || p["data"]["pinned"] == true,
				p["data"]["created_utc"].as_f64().unwrap_or(0.0),
				now,
			)
		});
	}
	let next = document
		.next
		.map(|next| spec.validate_page(&next, Some(&name)).map_err(|_| WebError::Temporary("fragment_scope")))
		.transpose()?;
	Ok((document.posts, Source { page: Some(page), ..source }, next))
}
async fn read_on_lane(ctx: Context, spec: Spec) -> Result<Value, WebError> {
	if spec.post.is_some() {
		let community = spec.communities.first().map(String::as_str);
		let initial = spec.initial(community);
		let page = spec.cursor.as_ref().and_then(|c| c.sources[0].page.as_deref()).unwrap_or(&initial);
		let mut doc = if spec.recover {
			let resume = spec.cursor.as_ref().and_then(|c| c.sources[0].resume.as_deref()).ok_or(WebError::Cursor)?;
			let mut doc = document(&ctx, &spec, &initial, community, true).await?;
			let mut found = false;
			for _ in 0..8 {
				if let Some(at) = doc.comments.iter().position(|c| c["data"]["name"].as_str() == Some(resume)) {
					doc.comments.drain(..=at);
					found = true;
					if !doc.comments.is_empty() {
						break;
					}
				}
				let root = format!("t3_{}", spec.post.as_deref().unwrap());
				let more = doc.more.iter().find(|m| m.parent == root);
				let form = more.map(|m| Form {
					cursor: m.cursor.clone(),
					parent: m.parent.clone(),
				});
				let page = more.map(|m| m.path.as_str()).or(doc.next.as_deref()).ok_or(WebError::Cursor)?;
				let page = spec.validate_page(page, community)?;
				let body = ctx.fetch(&spec, &page, community, form.as_ref()).await?;
				let names = community.map_or(vec![], |name| vec![name.to_string()]);
				let extra = html::parse(&body, spec.post.as_deref(), &names, ctx.quarantine).map_err(|error| ctx.note(error))?;
				doc.comments = extra.comments;
				doc.more = extra.more;
				doc.next = extra.next;
				if found && !doc.comments.is_empty() {
					break;
				}
			}
			if !found || doc.comments.is_empty() {
				return Err(WebError::Cursor);
			}
			doc
		} else if spec.cursor.is_some() {
			let form = spec.cursor.as_ref().and_then(|c| c.sources[0].form.as_ref());
			let mut original = document(&ctx, &spec, &initial, community, false).await?;
			let body = ctx.fetch(&spec, page, community, form).await?;
			let names = community.map_or(vec![], |s| vec![s.to_string()]);
			let extra = html::parse(&body, spec.post.as_deref(), &names, ctx.quarantine).map_err(|error| ctx.note(error))?;
			if extra.comments.is_empty() {
				return Err(WebError::Temporary("missing_comment_tree"));
			}
			original.comments = extra.comments;
			original.next = extra.next;
			original.more = extra.more;
			original
		} else {
			document(&ctx, &spec, page, community, true).await?
		};
		if let Some(focus) = &spec.focus {
			fn branch(v: &Value, id: &str, parents: usize) -> Option<(Value, usize)> {
				if v["data"]["name"].as_str() == Some(id) {
					return Some((v.clone(), 0));
				}
				for child in v["data"]["replies"]["data"]["children"].as_array().into_iter().flatten() {
					if let Some((result, depth)) = branch(child, id, parents) {
						if depth < parents {
							let mut parent = v.clone();
							parent["data"]["replies"] = listing(vec![result], None);
							return Some((parent, depth + 1));
						}
						return Some((result, depth));
					}
				}
				None
			}
			let found = doc
				.comments
				.iter()
				.find_map(|c| branch(c, &format!("t1_{focus}"), usize::from(spec.context)))
				.ok_or(WebError::Temporary("focused_comment_missing"))?;
			doc.comments = vec![found.0];
		}
		let resume = doc
			.comments
			.iter()
			.rev()
			.find(|c| c["kind"] == "t1")
			.and_then(|c| c["data"]["name"].as_str())
			.map(str::to_string);
		let more = doc
			.more
			.drain(..)
			.find(|m| spec.focus.is_none() && m.parent == format!("t3_{}", spec.post.as_deref().unwrap()));
		let form = more.as_ref().map(|m| Form {
			cursor: m.cursor.clone(),
			parent: m.parent.clone(),
		});
		let next = more
			.map(|m| m.path)
			.or(if spec.focus.is_none() { doc.next } else { None })
			.map(|page| spec.validate_page(&page, community))
			.transpose()?;
		let after = encode_cursor(
			&spec.scope,
			vec![Source {
				page: next.clone(),
				last: None,
				done: next.is_none(),
				served: 0,
				form,
				resume,
			}],
			ctx.lane,
			Some(ctx.session.clone()),
		)?;
		return Ok(json!([listing(doc.posts, None), listing(doc.comments, after)]));
	}
	let sources = spec.cursor.as_ref().map(|c| c.sources.clone()).unwrap_or_else(|| {
		vec![
			Source {
				page: None,
				last: None,
				done: false,
				served: 0,
				form: None,
				resume: None
			};
			spec.communities.len()
		]
	});
	let mut tasks = tokio::task::JoinSet::new();
	for (i, (name, source)) in spec.communities.iter().cloned().zip(sources).enumerate() {
		let context = ctx.clone();
		let spec = spec.clone();
		tasks.spawn(async move { (i, feed_source(context, spec, name, source).await) });
	}
	let mut failure = None;
	let mut sides = vec![
		(
			vec![],
			Source {
				page: None,
				last: None,
				done: true,
				served: 0,
				form: None,
				resume: None
			},
			None
		);
		spec.communities.len()
	];
	while let Some(result) = tasks.join_next().await {
		let (index, result) = result.map_err(|_| WebError::Temporary("parser"))?;
		match result {
			Ok(result) => sides[index] = result,
			Err(error) => {
				if failure.is_none() {
					failure = Some(error);
				}
			}
		}
	}
	if let Some(error) = ctx.budget.terminal.lock().unwrap().clone().or(failure) {
		return Err(error);
	}
	let balanced = matches!(spec.sort.as_str(), "hot" | "best" | "rising");
	let mut positions = vec![0; sides.len()];
	let mut posts = vec![];
	let mut seen = HashSet::new();
	let minimum = sides.iter().filter(|s| !s.0.is_empty()).map(|s| s.1.served).min().unwrap_or(0);
	for s in &mut sides {
		s.1.served = s.1.served.saturating_sub(minimum);
	}
	let rank = |p: &Value| {
		let created = p["data"]["created_utc"].as_f64().unwrap_or(0.0);
		let score = p["data"]["score"].as_f64().unwrap_or(0.0);
		match spec.sort.as_str() {
			"top" | "controversial" => score,
			"hot" | "best" | "rising" => score.signum() * score.abs().max(1.0).log10() + created / 45000.0,
			_ => created,
		}
	};
	while posts.len() < spec.limit {
		let next = (0..sides.len()).filter(|i| positions[*i] < sides[*i].0.len()).max_by(|a, b| {
			(if balanced {
				sides[*b].1.served.cmp(&sides[*a].1.served)
			} else {
				std::cmp::Ordering::Equal
			})
			.then_with(|| rank(&sides[*a].0[positions[*a]]).total_cmp(&rank(&sides[*b].0[positions[*b]])))
		});
		let Some(i) = next else { break };
		let post = sides[i].0[positions[i]].clone();
		positions[i] += 1;
		let id = post["data"]["name"].as_str().ok_or(WebError::Temporary("post_id"))?.to_string();
		sides[i].1.last = Some(id.clone());
		sides[i].1.resume = Some(id.clone());
		if seen.insert(id) {
			sides[i].1.served = sides[i].1.served.saturating_add(1);
			posts.push(post);
		}
	}
	for (i, side) in sides.iter_mut().enumerate() {
		if positions[i] == side.0.len() {
			side.1.page = side.2.take();
			side.1.last = None;
			side.1.done = side.1.page.is_none();
		}
	}
	if posts.is_empty() {
		return Err(WebError::Temporary("missing_feed"));
	}
	let after = encode_cursor(&spec.scope, sides.into_iter().map(|s| s.1).collect(), ctx.lane, Some(ctx.session.clone()))?;
	Ok(listing(posts, after))
}
async fn read(path: &str, quarantine: bool) -> Result<Value, WebError> {
	let spec = Spec::parse(path, quarantine)?;
	let frontend = spec.origin();
	let primary = spec
		.cursor
		.as_ref()
		.map_or_else(configured_primary_lane, |c| if c.tor { RedditLane::Tor } else { RedditLane::Direct });
	let context = |lane, session: Arc<Transport>, budget: Arc<Budget>| {
		#[cfg(test)]
		let mock = MOCK.lock().unwrap().clone();
		Context {
			lane,
			quarantine,
			budget,
			session,
			#[cfg(test)]
			origin: mock.and_then(|m| (!m.1.is_empty()).then_some(m.1)),
		}
	};
	let session = spec
		.cursor
		.as_ref()
		.and_then(|c| c.session.clone())
		.map(Ok)
		.unwrap_or_else(|| active_transport(primary, quarantine))?;
	let mut ctx = context(primary, session, Arc::new(Budget::new()));
	let first = tokio::time::timeout(
		Duration::from_secs(if primary == RedditLane::Tor && spec.communities.len() <= 1 { 8 } else { 30 }),
		read_on_lane(ctx.clone(), spec.clone()),
	)
	.await
	.unwrap_or(Err(WebError::Temporary("timeout")));
	let mut first = ctx.budget.finish(first);
	if primary == RedditLane::Tor && matches!(first, Err(WebError::RateLimited(_))) {
		let session = renew_tor_transport(&ctx.session, quarantine)?;
		ctx = context(primary, session, Arc::new(ctx.budget.new_attempt()));
		let mut retry = spec.clone();
		retry.recover = retry.cursor.is_some();
		first = tokio::time::timeout(Duration::from_secs(30), read_on_lane(ctx.clone(), retry))
			.await
			.unwrap_or(Err(WebError::Temporary("timeout")));
		first = ctx.budget.finish(first);
		if spec.cursor.is_some() && matches!(first, Err(WebError::Temporary("wire_bound" | "body_bound" | "timeout"))) {
			// A deep bookmark may exceed this read's recovery budget. Restart
			// once without it instead of retrying the same expensive scan.
			first = Err(WebError::Cursor);
		}
	}
	if let Err(error) = &first {
		info!(
			"Anonymous Reddit web attempt failed: lane={} frontend={} reason={}",
			primary.label(),
			frontend,
			error.label()
		);
	}
	if spec.cursor.is_none() && first.as_ref().err().is_some_and(WebError::retry_lane) {
		let alternate = if primary == RedditLane::Tor { RedditLane::Direct } else { RedditLane::Tor };
		if alternate == RedditLane::Direct || TOR_FALLBACK_CONFIG.as_ref().is_ok_and(Option::is_some) {
			let Ok(_standby) = STANDBY.try_acquire() else { return first };
			let session = active_transport(alternate, quarantine)?;
			let result = tokio::time::timeout(Duration::from_secs(30), read_on_lane(context(alternate, session, ctx.budget.clone()), spec))
				.await
				.unwrap_or(Err(WebError::Temporary("timeout")));
			let result = ctx.budget.finish(result);
			if let Err(error) = &result {
				info!(
					"Anonymous Reddit web attempt failed: lane={} frontend={} reason={}",
					alternate.label(),
					frontend,
					error.label()
				);
			}
			return result;
		}
	}
	first
}
#[cached(size = 256, time = 30, result = true)]
async fn cached_read(path: String, quarantine: bool) -> Result<Value, String> {
	let result = tokio::time::timeout(Duration::from_secs(60), read(&path, quarantine))
		.await
		.unwrap_or(Err(WebError::Temporary("timeout")));
	if let Err(error) = &result {
		info!("Anonymous Reddit web read failed: reason={}", error.label());
	}
	result.map_err(|error| error.message())
}
pub(super) async fn json(path: String, quarantine: bool) -> Result<Value, String> {
	Spec::parse(&path, quarantine).map_err(|error| error.message())?;
	coalesce_json_request(&FLIGHTS, (path.clone(), quarantine), move || async move {
		let value = cached_read(path.clone(), quarantine).await?;
		let after = value["data"]["after"].as_str().or_else(|| value[1]["data"]["after"].as_str());
		let stale = after.and_then(|after| after.strip_prefix(PREFIX)).is_some_and(|token| {
			cached::Cached::cache_get(&mut *CURSORS.lock().unwrap(), token).is_none_or(|cursor| {
				cursor.tor
					&& cursor
						.session
						.as_ref()
						.is_some_and(|session| session.health.lock().unwrap().until.is_some_and(|until| until > Instant::now()))
			})
		});
		if stale {
			cached::Cached::cache_remove(&mut *CACHED_READ.lock().await, &(path.clone(), quarantine));
			cached_read(path, quarantine).await
		} else {
			Ok(value)
		}
	})
	.await
}

#[cfg(test)]
#[path = "reddit_web_tests.rs"]
mod tests;
