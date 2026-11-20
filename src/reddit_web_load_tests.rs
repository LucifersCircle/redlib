use super::*;

#[test]
fn test_reddit_web_load_budget_limits_remain_shared_after_renewal() {
	for sources in [1, 14, 32, 64] {
		let names = (0..sources).map(|index| format!("s{index}")).collect::<Vec<_>>().join("+");
		let spec = Spec::parse(&format!("/r/{names}/hot.json"), false).unwrap();
		let budget = Budget::for_spec(&spec);
		assert!(budget.wire_limit <= 384 && budget.byte_limit <= 256 * 1024 * 1024);
		let renewed = budget.new_attempt();
		assert_eq!((renewed.wire_limit, renewed.byte_limit), (budget.wire_limit, budget.byte_limit));
		for _ in 0..budget.wire_limit {
			renewed.reserve_wire().unwrap();
		}
		assert_eq!(budget.reserve_wire(), Err(WebError::Temporary("wire_bound")));
		assert_eq!(budget.wires.load(Ordering::SeqCst), budget.wire_limit, "denied reservations do not inflate the count");
		renewed.bytes.store(12345, Ordering::SeqCst);
		assert_eq!(budget.bytes.load(Ordering::SeqCst), 12345, "a replacement retains the original streamed-byte consumption");
	}
}

// Controlled local-fixture stress checks for the supported subscription bound.
// The fixture returns two native pins per source, each requiring a metadata GET.
#[test]
#[sealed_test(env=[("REDLIB_READ_BACKEND","web"),("REDLIB_TOR_FALLBACK","off"),("REDLIB_REDDIT_PRIMARY_LANE","direct")])]
fn test_reddit_web_load_supported_subscription_counts_fit_scoped_budget() {
	tokio::runtime::Runtime::new().unwrap().block_on(async {
		for count in [14, 32, 64] {
			let fixture = Fixture::new().await;
			fixture.mode.store(30, Ordering::SeqCst);
			*MOCK.lock().unwrap() = Some((fixture.transport.clone(), fixture.origin.clone()));
			let names = (0..count).map(|i| format!("s{i}")).collect::<Vec<_>>().join("+");
			let result = read(&format!("/r/{names}/hot.json?limit={count}"), false).await;
			let wires = fixture.wires.lock().unwrap();
			let pin_gets = wires.iter().filter(|(path, _, _, _)| path.contains("/comments/")).count();
			let page = result.unwrap_or_else(|error| panic!("{count} sources fit the scoped request budget: {error:?}"));
			assert_eq!(page["data"]["children"].as_array().unwrap().len(), count);
			assert_eq!(wires.len(), count * 3, "one root and two pin metadata requests per source");
			assert_eq!(pin_gets, count * 2);
		}
	});
}

// Different sort URLs bypass JSON request coalescing but share the same
// transport and pin metadata keys. This measures the cold-cache fanout.
#[test]
#[sealed_test(env=[("REDLIB_READ_BACKEND","web"),("REDLIB_TOR_FALLBACK","off"),("REDLIB_REDDIT_PRIMARY_LANE","direct")])]
fn test_reddit_web_load_mixed_sort_cold_reads_share_pin_metadata() {
	tokio::runtime::Runtime::new().unwrap().block_on(async {
		let fixture = Fixture::new().await;
		fixture.mode.store(30, Ordering::SeqCst);
		let sorts = ["hot", "new", "top", "rising", "controversial", "new", "best", "hot"];
		let mut tasks = tokio::task::JoinSet::new();
		for sort in sorts {
			let context = fixture.context();
			let spec = Spec::parse(&format!("/r/rust/{sort}.json?limit=1"), false).unwrap();
			tasks.spawn(async move { read_on_lane(context, spec).await });
		}
		while let Some(result) = tasks.join_next().await {
			assert!(result.unwrap().is_ok());
		}
		let wires = fixture.wires.lock().unwrap();
		let pin_gets = wires.iter().filter(|(path, _, _, _)| path.contains("/comments/")).count();
		println!("mixed-sort cold fixture: reads=8 wires={} pin_gets={pin_gets}", wires.len());
		assert_eq!(pin_gets, 2, "a single-flight pin cache should issue only two shared pin GETs");
	});
}

// Exercise the user-facing subscription homepage through the local HTTP
// renderer. Sort/range pairs make each JSON URL a distinct flight.
#[test]
#[sealed_test(env=[("REDLIB_READ_BACKEND","web"),("REDLIB_TOR_FALLBACK","off"),("REDLIB_REDDIT_PRIMARY_LANE","direct")])]
fn test_reddit_web_load_subscription_home_renderer_8_and_16_cold_requests() {
	tokio::runtime::Runtime::new().unwrap().block_on(async {
		for workers in [8, 16] {
			cached::Cached::cache_clear(&mut *READ_CACHE.lock().await);
			let fixture = Fixture::new().await;
			fixture.mode.store(30, Ordering::SeqCst);
			*MOCK.lock().unwrap() = Some((fixture.transport.clone(), fixture.origin.clone()));
			let mut app = crate::server::Server::new();
			app.at("/").get(|r| Box::pin(crate::subreddit::community(r)));
			let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
			let address = listener.local_addr().unwrap();
			drop(listener);
			let server = tokio::spawn(app.listen(&address.to_string()));
			let http = hyper::Client::new();
			let base = (0..14).map(|i| format!("s{i}")).collect::<Vec<_>>();
			let started = Instant::now();
			let mut tasks = tokio::task::JoinSet::new();
			for worker in 0..workers {
				let http = http.clone();
				let mut names = base.clone();
				let rotation = worker % names.len();
				names.rotate_left(rotation);
				let names = names.join("+");
				let sorts = ["hot", "new", "top", "rising", "best", "controversial"];
				let ranges = ["all", "month", "year"];
				let sort = sorts[worker % sorts.len()];
				let range = ranges[worker / sorts.len()];
				tasks.spawn(async move {
					let request = hyper::Request::builder()
						.uri(format!("http://{address}/?t={range}"))
						.header("cookie", format!("subscriptions={names}; front_page=default; layout=card; theme=black; post_sort={sort}"))
						.body(Body::empty())
						.unwrap();
					let response = http.request(request).await.unwrap();
					let status = response.status();
					let body = hyper::body::to_bytes(response.into_body()).await.unwrap();
					let page = scraper::Html::parse_document(std::str::from_utf8(&body).unwrap());
					let titles = scraper::Selector::parse(".post_title").unwrap();
					(status, page.select(&titles).count(), body.len())
				});
			}
			while let Some(response) = tasks.join_next().await {
				let (status, posts, bytes) = response.unwrap();
				assert_eq!(status, hyper::StatusCode::OK);
				assert_eq!(posts, 25, "homepage renders the complete requested feed");
				assert!(bytes > 1000, "homepage renderer should return a full page");
			}
			let elapsed = started.elapsed();
			server.abort();
			let wires = fixture.wires.lock().unwrap();
			let pin_gets = wires.iter().filter(|(path, _, _, _)| path.contains("/comments/")).count();
			println!(
				"homepage renderer cold load: workers={workers} wires={} pin_gets={pin_gets} elapsed_ms={}",
				wires.len(),
				elapsed.as_millis()
			);
			assert_eq!(
				wires.len(),
				workers * 14 + 28,
				"each sort/range flight loads its 14 roots and the shared pins are fetched once"
			);
		}
	});
}

#[test]
#[sealed_test(env=[("REDLIB_READ_BACKEND","web"),("REDLIB_TOR_FALLBACK","off"),("REDLIB_REDDIT_PRIMARY_LANE","direct")])]
fn test_reddit_web_load_pin_failure_is_shared_then_retried() {
	tokio::runtime::Runtime::new().unwrap().block_on(async {
		let fixture = Fixture::new().await;
		fixture.mode.store(1, Ordering::SeqCst);
		let mut tasks = tokio::task::JoinSet::new();
		let barrier = Arc::new(tokio::sync::Barrier::new(8));
		for _ in 0..8 {
			let barrier = barrier.clone();
			let context = fixture.context();
			tasks.spawn(async move {
				barrier.wait().await;
				pin_metadata(context, "t3_abc".into(), "/r/rust/comments/abc/title/".into()).await
			});
		}
		while let Some(result) = tasks.join_next().await {
			assert!(matches!(result.unwrap(), Err(WebError::RateLimited(_))));
		}
		let failed_wires = fixture.wires.lock().unwrap().len();
		println!("pin quota flight: concurrent callers=8 wires={failed_wires}");
		assert_eq!(failed_wires, 1, "all callers should share the same failed upstream attempt");

		fixture.mode.store(0, Ordering::SeqCst);
		fixture.transport.health.lock().unwrap().until = None;
		let retry = pin_metadata(fixture.context(), "t3_abc".into(), "/r/rust/comments/abc/title/".into()).await;
		assert!(retry.is_ok(), "failed pin results must not be retained as cache entries");
		assert_eq!(fixture.wires.lock().unwrap().len(), failed_wires + 1);
	});
}

#[test]
#[sealed_test(env=[("REDLIB_READ_BACKEND","web"),("REDLIB_TOR_FALLBACK","off"),("REDLIB_REDDIT_PRIMARY_LANE","direct")])]
fn test_reddit_web_load_16_semantically_equivalent_json_paths_share_one_flight() {
	tokio::runtime::Runtime::new().unwrap().block_on(async {
		let fixture = Fixture::new().await;
		fixture.mode.store(30, Ordering::SeqCst);
		*MOCK.lock().unwrap() = Some((fixture.transport.clone(), fixture.origin.clone()));
		let mut tasks = tokio::task::JoinSet::new();
		for index in 0..16 {
			let path = if index % 2 == 0 {
				format!("/r/rust/hot.json?context=9876&limit=1&count={index}")
			} else {
				format!("/r/rust/hot.json?count={index}&limit=1&context=9876")
			};
			tasks.spawn(async move { json(path, false).await });
		}
		while let Some(result) = tasks.join_next().await {
			let page = result.unwrap().unwrap();
			assert_eq!(page["data"]["children"].as_array().unwrap().len(), 1);
		}
		let wires = fixture.wires.lock().unwrap();
		let pin_gets = wires.iter().filter(|(path, _, _, _)| path.contains("/comments/")).count();
		println!("semantic JSON flight: callers=16 wires={} pin_gets={pin_gets}", wires.len());
		assert_eq!(wires.len(), 3, "one root page and its two shared pin metadata reads");
		assert_eq!(pin_gets, 2);
	});
}

#[test]
#[sealed_test(env=[("REDLIB_READ_BACKEND","web"),("REDLIB_TOR_FALLBACK","off"),("REDLIB_REDDIT_PRIMARY_LANE","direct")])]
fn test_reddit_web_load_delayed_16_worker_home_and_next_round_trip() {
	tokio::runtime::Runtime::new().unwrap().block_on(async {
		cached::Cached::cache_clear(&mut *READ_CACHE.lock().await);
		let fixture = Fixture::new().await;
		fixture.mode.store(38, Ordering::SeqCst);
		*MOCK.lock().unwrap() = Some((fixture.transport.clone(), fixture.origin.clone()));
		let mut app = crate::server::Server::new();
		app.at("/").get(|r| Box::pin(crate::subreddit::community(r)));
		app.at("/r/:sub/:sort").get(|r| Box::pin(crate::subreddit::community(r)));
		let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
		let address = listener.local_addr().unwrap();
		drop(listener);
		let server = tokio::spawn(app.listen(&address.to_string()));
		let http = hyper::Client::new();
		let names = (0..14).map(|i| format!("stress{i}")).collect::<Vec<_>>().join("+");
		let sorts = ["hot", "new", "top", "rising", "best", "controversial"];
		let ranges = ["all", "month", "year"];
		let started = Instant::now();
		let mut tasks = tokio::task::JoinSet::new();
		for worker in 0..16 {
			let http = http.clone();
			let cookie = format!(
				"subscriptions={names}; front_page=default; layout=card; theme=black; post_sort={}",
				sorts[worker % sorts.len()]
			);
			let range = ranges[worker / sorts.len()];
			tasks.spawn(async move {
				let home = tokio::time::timeout(Duration::from_secs(30), async {
					let request = hyper::Request::builder()
						.uri(format!("http://{address}/?t={range}"))
						.header("cookie", &cookie)
						.body(Body::empty())
						.unwrap();
					let response = http.request(request).await.unwrap();
					let status = response.status();
					let body = hyper::body::to_bytes(response.into_body()).await.unwrap();
					(status, body)
				})
				.await
				.expect("homepage response must finish within 30 seconds");
				assert_eq!(home.0, hyper::StatusCode::OK);
				let text = std::str::from_utf8(&home.1).unwrap();
				assert!(!text.contains("http-equiv=\"refresh\""), "homepage must not be a retry page");
				let (next, home_ids) = {
					let doc = scraper::Html::parse_document(text);
					let titles = scraper::Selector::parse(".post_title").unwrap();
					assert_eq!(doc.select(&titles).count(), 25);
					let posts = scraper::Selector::parse(".post[id]").unwrap();
					let home_ids = doc
						.select(&posts)
						.filter_map(|element| element.value().attr("id").map(str::to_owned))
						.collect::<HashSet<_>>();
					let next_selector = scraper::Selector::parse("a[accesskey='N']").unwrap();
					let next = doc
						.select(&next_selector)
						.next()
						.and_then(|element| element.value().attr("href"))
						.expect("every complete home page should provide a Next link")
						.to_string();
					(next, home_ids)
				};
				assert!(
					(next.starts_with('/') || next.starts_with('?')) && next.contains("after="),
					"Next must preserve its local cursor link: {next}"
				);
				let next_url = url::Url::parse(&format!("http://{address}/")).unwrap().join(&next).unwrap().to_string();
				let next_page = tokio::time::timeout(Duration::from_secs(30), async {
					let request = hyper::Request::builder().uri(next_url).header("cookie", &cookie).body(Body::empty()).unwrap();
					let response = http.request(request).await.unwrap();
					let status = response.status();
					let body = hyper::body::to_bytes(response.into_body()).await.unwrap();
					(status, body)
				})
				.await
				.expect("Next response must finish within 30 seconds");
				assert_eq!(next_page.0, hyper::StatusCode::OK);
				let text = std::str::from_utf8(&next_page.1).unwrap();
				assert!(!text.contains("http-equiv=\"refresh\""), "Next must not be a retry page");
				let doc = scraper::Html::parse_document(text);
				let titles = scraper::Selector::parse(".post_title").unwrap();
				assert_eq!(doc.select(&titles).count(), 25);
				let posts = scraper::Selector::parse(".post[id]").unwrap();
				let next_ids = doc.select(&posts).filter_map(|element| element.value().attr("id")).collect::<HashSet<_>>();
				assert!(home_ids.iter().all(|id| !next_ids.contains(id.as_str())), "Next must resume beyond every home post");
			});
		}
		while let Some(result) = tasks.join_next().await {
			result.unwrap();
		}
		let elapsed = started.elapsed();
		server.abort();
		let wires = fixture.wires.lock().unwrap();
		let pins = wires.iter().filter(|(path, _, _, _)| path.contains("/comments/")).count();
		println!(
			"delayed home+Next: workers=16 responses=32 wires={} pin_gets={pins} elapsed_ms={}",
			wires.len(),
			elapsed.as_millis()
		);
		assert_eq!(pins, 28, "each of 14 shared pins is fetched once despite 16 cold home requests");
		assert!(wires.len() < 800, "32 scoped requests stay within the expected aggregate work bound");
	});
}
