use std::env;
use std::sync::LazyLock;
use std::time::Duration;

pub(crate) const REDDIT_ONION: &str = "reddittorjg6rue252oqsxryoxengawnmo46qy4kyii5wtqnwfj4ooad.onion";

const DIRECT_API_BASE: &str = "https://oauth.reddit.com";
const DIRECT_API_HOST: &str = "oauth.reddit.com";
const DIRECT_AUTH_BASE: &str = "https://www.reddit.com";
const DIRECT_AUTH_HOST: &str = "www.reddit.com";
const DIRECT_SHORT_HOST: &str = "redd.it";

const TOR_API_BASE: &str = "https://oauth.reddittorjg6rue252oqsxryoxengawnmo46qy4kyii5wtqnwfj4ooad.onion";
const TOR_API_HOST: &str = "oauth.reddittorjg6rue252oqsxryoxengawnmo46qy4kyii5wtqnwfj4ooad.onion";
const TOR_AUTH_BASE: &str = "https://www.reddittorjg6rue252oqsxryoxengawnmo46qy4kyii5wtqnwfj4ooad.onion";
const TOR_AUTH_HOST: &str = "www.reddittorjg6rue252oqsxryoxengawnmo46qy4kyii5wtqnwfj4ooad.onion";

const DIRECT_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const TOR_REQUEST_TIMEOUT: Duration = Duration::from_secs(90);

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub(crate) enum RedditLane {
	Direct,
	Tor,
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub(crate) struct RedditOrigin {
	pub(crate) base: &'static str,
	pub(crate) host: &'static str,
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub(crate) struct TorFallbackConfig {
	pub(crate) proxy_url: String,
}

pub(crate) static TOR_FALLBACK_CONFIG: LazyLock<Result<Option<TorFallbackConfig>, String>> = LazyLock::new(|| {
	let enabled = env::var("REDLIB_TOR_FALLBACK").ok();
	let proxy = env::var("REDLIB_TOR_PROXY").ok();
	TorFallbackConfig::parse(enabled.as_deref(), proxy.as_deref())
});

impl RedditLane {
	pub(crate) fn label(self) -> &'static str {
		match self {
			Self::Direct => "direct",
			Self::Tor => "tor",
		}
	}

	pub(crate) fn api_origin(self) -> RedditOrigin {
		match self {
			Self::Direct => RedditOrigin {
				base: DIRECT_API_BASE,
				host: DIRECT_API_HOST,
			},
			Self::Tor => RedditOrigin {
				base: TOR_API_BASE,
				host: TOR_API_HOST,
			},
		}
	}

	pub(crate) fn auth_origin(self) -> RedditOrigin {
		match self {
			Self::Direct => RedditOrigin {
				base: DIRECT_AUTH_BASE,
				host: DIRECT_AUTH_HOST,
			},
			Self::Tor => RedditOrigin {
				base: TOR_AUTH_BASE,
				host: TOR_AUTH_HOST,
			},
		}
	}

	pub(crate) fn request_timeout(self) -> Duration {
		match self {
			Self::Direct => DIRECT_REQUEST_TIMEOUT,
			Self::Tor => TOR_REQUEST_TIMEOUT,
		}
	}

	pub(crate) fn accepts_redirect_host(self, host: &str) -> bool {
		match self {
			Self::Direct => matches!(host, DIRECT_API_HOST | DIRECT_AUTH_HOST | DIRECT_SHORT_HOST),
			Self::Tor => matches!(host, TOR_API_HOST | TOR_AUTH_HOST),
		}
	}
}

impl TorFallbackConfig {
	fn parse(enabled: Option<&str>, proxy: Option<&str>) -> Result<Option<Self>, String> {
		if !matches!(enabled, Some("on" | "true" | "1" | "yes")) {
			return Ok(None);
		}

		let proxy_url = proxy.ok_or_else(|| "REDLIB_TOR_PROXY must be set when REDLIB_TOR_FALLBACK is enabled".to_string())?;
		let parsed = url::Url::parse(proxy_url).map_err(|_| "REDLIB_TOR_PROXY must be a valid socks5h URL".to_string())?;
		if parsed.scheme() != "socks5h"
			|| parsed.host_str().is_none()
			|| parsed.port().is_none()
			|| !parsed.username().is_empty()
			|| parsed.password().is_some()
			|| parsed.query().is_some()
			|| parsed.fragment().is_some()
			|| parsed.path() != "/"
		{
			return Err("REDLIB_TOR_PROXY must use socks5h://host:port without credentials, a path, query, or fragment".to_string());
		}

		Ok(Some(Self { proxy_url: proxy_url.to_string() }))
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn tor_fallback_requires_explicit_valid_socks5h_proxy() {
		assert_eq!(TorFallbackConfig::parse(None, None), Ok(None));
		assert_eq!(TorFallbackConfig::parse(Some("off"), Some("socks5h://tor:9050")), Ok(None));
		assert!(TorFallbackConfig::parse(Some("on"), None).is_err());
		assert!(TorFallbackConfig::parse(Some("on"), Some("socks5://tor:9050")).is_err());
		assert!(TorFallbackConfig::parse(Some("on"), Some("socks5h://user:pass@tor:9050")).is_err());
		assert_eq!(
			TorFallbackConfig::parse(Some("on"), Some("socks5h://tor:9050")),
			Ok(Some(TorFallbackConfig {
				proxy_url: "socks5h://tor:9050".to_string(),
			}))
		);
	}

	#[test]
	fn each_lane_has_matching_origins_and_redirect_allowlist() {
		assert_eq!(RedditLane::Direct.api_origin().host, "oauth.reddit.com");
		assert_eq!(RedditLane::Direct.auth_origin().host, "www.reddit.com");
		assert!(RedditLane::Direct.accepts_redirect_host("redd.it"));
		assert!(!RedditLane::Direct.accepts_redirect_host(TOR_API_HOST));

		assert!(RedditLane::Tor.api_origin().host.ends_with(REDDIT_ONION));
		assert!(RedditLane::Tor.auth_origin().host.ends_with(REDDIT_ONION));
		assert!(RedditLane::Tor.accepts_redirect_host(TOR_AUTH_HOST));
		assert!(!RedditLane::Tor.accepts_redirect_host(DIRECT_API_HOST));
		assert!(RedditLane::Tor.request_timeout() > RedditLane::Direct.request_timeout());
	}
}
