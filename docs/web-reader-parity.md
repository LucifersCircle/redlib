# Web reader behavior audit

This audit compares the anonymous HTML reader with the feed behavior in PRs #53 and #56, before the web backend migration. The web backend remains the default, honors the configured Tor primary, and uses no Reddit accounts or imported browser cookies.

| Behavior | Regression found | Restored behavior and verification |
| --- | --- | --- |
| Galleries | Native carousel slides were converted to an ordinary link. | Preserve slide order, originals, signed previews, dimensions, captions, source links and animated originals. Sanitized native DOM fixtures exercise feed and post rendering; Chromium checks slide navigation at mobile, tablet and desktop widths. |
| Article thumbnails | An avatar could stop the image search; dimensions encoded in preview URLs were lost. | Select the post's own Reddit-hosted image, retain preview variants and infer supplied dimensions. Missing dimensions no longer collapse the wrapper. |
| Pins | Native highlight cards were skipped because they sit outside the ordinary feed. | Resolve only validated, same-community public post links. Preserve all pins first on an individual subreddit's first page. Combined/home feeds and later pages keep the existing 24-hour creation-age rule. Metadata reuse is bounded and restricted to the same anonymous jar. |
| Feed length | A three-card HTML bootstrap became an entire Redlib page. | Follow validated fragments until the requested limit or the real end, default 25 and maximum 100. Tests cover limits 7/25/100, six sorts, one and 14 communities, and three pages. |
| Subscription ordering | Stopping at a fragment boundary shortened each community's contribution. | Preserve balanced turns for hot/best/rising across page boundaries; new/top/controversial retain global source-head ordering. Accept up to 64 subscriptions, matching the earlier backend. |
| Previous/Next | Previous was rejected and generated links dropped explicit limits, invalidating scope checks. | Track bounded server-side previous links, retain query preferences and restore first-page pin behavior when returning to the start. Real HTTP tests follow three pages in both directions. |
| Flair and votes | HTML flair text lacked the type required by the existing renderer; vote ratio was omitted. | Preserve text, validated Reddit emoji and supplied colors, inherit the page color when unspecified, and retain a supplied finite vote ratio. |
| Temporary login redirects | A connection-specific redirect was reported as a permanent sign-in requirement. | Treat the redirect as retryable without submitting a login form. A public-post fixture redirects twice before succeeding; quota and actual resource restrictions retain their separate behavior. |
| Page cache | The web migration shortened freshness and removed last-success fallback. | Restore 60-second feed and 180-second thread freshness, with a bounded last-success fallback during temporary failures. Explicit resource errors discard stale content. Tests check expiration, recovery and private-resource handling. |

Page filling, pin discovery, continuation recovery and retries share the existing request, byte, concurrency and deadline limits. HTML shells remain temporary failures. Expired or unavailable navigation anchors return to the first page with the feed preferences retained. Anonymous Tor quota renewal and isolated SOCKS transport remain covered by deterministic transport fixtures; a different SOCKS identity does not promise an unused exit IP.

Live checks separately exercise signed-out Reddit feeds, their Next links, a discovered post and the Redlib HTTP renderer. The subscription check uses 14 public communities and requires 25 rendered posts on both homepage pages, with every subscribed community represented on the first page. Raw captures stay local because they can contain anonymous session and continuation tokens.

## Remaining backend differences

The HTML backend supports public feeds and post/comment pages; it does not provide complete API feature parity. Regional Popular, search, user profiles, wiki, community metadata and short/share-link resolution remain unsupported. Rich fields absent from public HTML cannot be reconstructed reliably. Explicit access restrictions and persistent upstream challenges can still prevent an uncached read. A temporary login redirect alone is not evidence that account rotation is necessary.

The earlier video and text fixes remain in place: Reddit HLS/preview/packaged MP4 use the existing video player and media proxy, while native feed presentation wrappers are removed from post text without discarding user formatting or links.
