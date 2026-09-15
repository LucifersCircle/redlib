# Redlib rate-limit resilience patch

Base: `redlib-org/redlib` and `LucifersCircle/redlib` at commit `a4d36e9`.

## What this patch implements

### 1. Request coalescing and two-layer caching

`client::json` now has a two-layer cache:

- The outer cache holds every result for 2 seconds and uses per-key synchronized writes. Concurrent requests for the same Reddit path therefore produce one upstream operation, and a burst of identical failures does not immediately retry Reddit once per visitor.
- The inner cache holds successful JSON responses for 60 seconds and retains 1,024 entries. If refreshing an expired entry fails, `cached`'s `result_fallback` returns the last successful value instead of exposing a brief upstream failure.

The previous cache held only 100 successful entries for 30 seconds, did not coalesce misses, and offered no stale-on-error behavior.

### 2. Global outbound concurrency cap

A process-wide Tokio semaphore caps simultaneous Reddit JSON API work. The default is 8 and can be changed with:

```env
REDLIB_REDDIT_MAX_CONCURRENCY=8
```

Values are clamped to 1-64. This applies to Reddit JSON calls, not media proxy streams, so a slow video download cannot consume all API permits.

### 3. Shared cooldown from Reddit rate-limit headers

HTTP 429 and HTTP 403 with `Retry-After` are treated as explicit rate limits. `Retry-After` is preferred, then `x-ratelimit-reset`, then a conservative 10-second default. The pause is shared across all requests and capped at 10 minutes. A successful response reporting zero remaining requests also starts the shared reset delay.

This stops every visitor from independently rediscovering an already-known upstream limit.

### 4. Short circuit breaker for malformed upstream failures

Three transport, body-read, empty-body, or invalid-JSON failures inside 10 seconds pause new Reddit API calls for 10 seconds. Valid Reddit JSON errors such as private, banned, gated, or quarantined communities do not count as upstream failures.

### 5. OAuth refresh correction

An empty body no longer triggers an OAuth refresh. Only a clear HTTP 401 or the existing low known OAuth budget starts rollover. This prevents an edge denial or temporary empty response from creating unnecessary OAuth traffic.

The local estimated rate-limit counter now uses saturating subtraction, fixing the possibility that decrementing zero wraps a `u16` to 65,535.

### 6. Privacy-conscious diagnostics

Failure logs report only a coarse endpoint class (`subreddit`, `user`, `api`, `search`, `comments`, or `other`), status, failure kind, and whether the circuit opened. They do not log subreddit names, usernames, search terms, or full request paths.

### 7. Docker Hub delivery

The old container workflow targeted Redlib's Quay repository and the default `Dockerfile` downloaded the upstream project's latest release. That would omit fork changes.

The replacement workflow:

- verifies the Rust code and deterministic resilience tests;
- builds the checked-out fork with `Dockerfile.ubuntu`;
- installs CA roots required by `wreq` and the `wget` binary used by the image health check;
- publishes `linux/amd64` and `linux/arm64` images;
- pushes `luciferscircle/redlib:latest` and an immutable `sha-<commit>` tag;
- runs on pushes to `main` and manually through `workflow_dispatch`.

Add these GitHub Actions repository secrets before pushing:

- `DOCKERHUB_USERNAME`: Docker Hub username
- `DOCKERHUB_TOKEN`: Docker Hub access token with read/write access

The included Compose file now uses `luciferscircle/redlib:latest`. For production rollback safety, deploy the generated `sha-<commit>` tag after the first successful build instead of leaving `latest` permanently pinned.

## Verification performed

### Passed

- `rustfmt --edition 2021 --check src/client.rs`
- `cargo check --locked`
- `cargo check --locked --all-targets`
- Unit: default, invalid, minimum, ordinary, and maximum concurrency parsing
- Unit: fractional, capped, negative, invalid, and absent delay parsing
- Unit: rate counter saturates at zero and does not wrap
- Unit: circuit breaker opens on the third failure and expires
- Unit: diagnostic endpoint classification excludes resource names
- Async unit: three simultaneous identical cache misses execute the inner operation once
- `git diff --check`

### Inconclusive or unavailable here

- The live Reddit integration test could not authenticate from this execution environment; Redlib's existing OAuth constructor exhausted its retries and terminated the test process. This occurred before a Reddit JSON response reached the patched code, so it neither validates nor disproves the patch.
- Docker is not installed in this execution environment, so the container image itself was not built locally. The workflow uses the repository's existing source-building `Dockerfile.ubuntu`, and GitHub Actions will perform the actual multi-platform build.
- Strict `cargo clippy --all-targets -- -D warnings` reaches the project but fails on five pre-existing warnings in `src/duplicates.rs` and `src/subreddit.rs`. No diagnostic pointed to the patched code.

## Recommended production test

1. Push the patch to a temporary branch and run the Docker Hub workflow manually.
2. Deploy its immutable `sha-<commit>` image alongside the current container, but expose only one instance publicly at a time unless they have separate upstream state.
3. Start with `REDLIB_REDDIT_MAX_CONCURRENCY=8`. If residual denials correlate with bursts, test 4; if pages queue during ordinary traffic without rate-limit responses, test 12.
4. Record for at least 48 hours: total page requests, upstream JSON attempts, 401/403/429/5xx counts, circuit openings, cache hit/coalescing counts, p50/p95 response latency, and stale responses.
5. Compare rates per 1,000 page requests against the current image. Roll back to the previous immutable image if ordinary errors, latency, or OAuth refresh frequency increase materially.

## Further improvement plan

### Phase A: make the patch fully testable without Reddit

- Extract the transport behind a small trait and use a local mock HTTP server.
- Verify 429 plus `Retry-After`, 403 policy responses, 401 refresh, valid private/banned JSON, empty bodies, malformed JSON, slow responses, redirects, and transport failures.
- Add deterministic tests proving stale-on-error behavior and the 2-second error cache, not only the underlying coalescing primitive.
- Add a load test that sends mixed hot-key and high-cardinality traffic and asserts an upper bound on upstream calls.

### Phase B: improve scheduling rather than identity rotation

- Replace the fixed semaphore with an adaptive governor driven by remaining/reset headers, while retaining hard minimum and maximum concurrency.
- Add a bounded queue and request deadline so excess traffic is shed instead of consuming unbounded tasks and memory.
- Add jitter when a cooldown ends to avoid all queued work resuming simultaneously.
- Permit at most one retry for idempotent GET/HEAD requests, only for transient transport/5xx failures and only when no shared cooldown is active.
- Apply a related but separate limit to canonical-path HEAD requests after measuring whether they materially consume Reddit budget.
- Give interactive page requests priority over RSS refreshers and obvious crawlers if Redlib later gains request classes.

### Phase C: refine caching

- Normalize equivalent cache keys by ordering harmless query parameters and removing tracking parameters.
- Use endpoint-specific TTLs: longer for metadata/about responses, shorter for listings and comments.
- Cache stable negative application results such as private, banned, gated, and quarantined for longer than transient failures.
- Implement stale-while-revalidate so one request refreshes a popular expired entry in the background while visitors receive stale data immediately.
- Add memory-size awareness; 1,024 large JSON values can use substantially more memory than 1,024 small entries.
- Consider conditional requests with ETag/Last-Modified only if Reddit's API behavior and rate accounting show a real benefit.

### Phase D: harden OAuth and transport

- Keep one coherent browser/OS emulation for the process lifetime. Do not rotate fingerprints per request or in response to failures.
- Log the selected emulation once at startup and compare profiles over multi-day windows rather than changing them reactively.
- Separate OAuth budget exhaustion, OAuth invalidation, edge denial, upstream 5xx, and transport failure into typed errors.
- Add exponential backoff and jitter to OAuth acquisition; avoid process exit from a library path so tests and supervisors get a structured failure.
- Guard the token daemon's `expires_in - 120` calculation against underflow.
- Evaluate a `wreq`/`wreq-util` update as an isolated branch with media range, redirect, proxy, TLS fingerprint, HTTP/2, and OAuth regression tests. Do not combine that dependency migration with resilience logic.

### Phase E: observability and operational controls

- Add counters for cache hits, stale fallbacks, coalesced waits, upstream attempts, response classes, cooldown seconds, circuit openings, OAuth refresh reasons, and semaphore wait time.
- Export a Prometheus-compatible metrics endpoint or structured log events without full URLs or query values.
- Add a readiness check that distinguishes "Redlib process is alive" from "Reddit access is currently usable."
- Configure crawler controls and per-client limits at Cloudflare, while exempting only known health checks. Do not globally cache personalized/rendered HTML without auditing cookies and preference-dependent output.
- For multiple Redlib replicas, move rate state, single-flight leases, and hot cache entries to a shared store; otherwise each replica independently spends the same upstream budget.

### Phase F: safer releases

- Publish immutable version and commit tags in addition to `latest`.
- Add an amd64 container smoke test that boots the image and checks `/settings` and `/info` before publishing.
- Generate an SBOM, sign images, and attach provenance after the basic build is stable.
- Canary new images, compare the metrics above, then promote an immutable tag to production.

## Deliberately excluded

- Rotating proxies, source IPs, tokens, or TLS/browser fingerprints to evade Reddit controls.
- Retrying every failure.
- Treating every 403 or empty body as a rate limit.
- A site-wide CDN cache rule for all Redlib HTML.

Those approaches either amplify traffic, break identity consistency, conceal the actual failure class, or risk serving preference-dependent pages incorrectly.
