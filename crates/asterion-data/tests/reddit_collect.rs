use std::time::Duration;

use asterion_data::reddit::{ArcticClient, FIELDS};
use asterion_data::{DataError, RetryPolicy};
use wiremock::matchers::{method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

const SEARCH: &str = "/api/posts/search";

fn policy() -> RetryPolicy {
    RetryPolicy {
        min_interval: Duration::ZERO,
        max_attempts: 2,
        initial_backoff: Duration::from_millis(1),
        timeout: Duration::from_secs(5),
    }
}

#[tokio::test]
async fn fetch_page_sends_expected_query_without_author() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path(SEARCH))
        .and(query_param("subreddit", "stocks"))
        .and(query_param("after", "100"))
        .and(query_param("before", "200"))
        .and(query_param("limit", "100"))
        .and(query_param("sort", "asc"))
        .and(query_param("fields", FIELDS))
        .respond_with(ResponseTemplate::new(200).set_body_string(
            r#"{"data":[{"id":"a","created_utc":150,"author":"someone","title":"t"}]}"#,
        ))
        .mount(&server)
        .await;
    let mut client = ArcticClient::new(&server.uri(), policy()).expect("client");
    let body = client.fetch_page("stocks", 100, 200).await.expect("page");
    assert!(!body.contains("author"), "{body}");
    let requests = server.received_requests().await.expect("recorded");
    assert_eq!(requests.len(), 1);
    assert!(
        !requests[0]
            .url
            .query()
            .unwrap_or_default()
            .contains("author")
    );
    assert!(!FIELDS.contains("author"));
}

#[tokio::test]
async fn http_400_fails_fast() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path(SEARCH))
        .respond_with(
            ResponseTemplate::new(400)
                .set_body_string(r#"{"data":null,"error":"'stickied' is not a valid field"}"#),
        )
        .mount(&server)
        .await;
    let mut client = ArcticClient::new(&server.uri(), policy()).expect("client");
    let err = client.fetch_page("stocks", 1, 2).await.expect_err("400");
    assert!(
        matches!(&err, DataError::Api { message, .. } if message.contains("is not a valid field")),
        "{err:?}"
    );
    assert!(err.is_fatal_for_batch());
    assert_eq!(client.requests_sent(), 1);
}

mod collect {
    use super::*;
    use asterion_data::RawCache;
    use asterion_data::reddit::collect::{
        collect_subreddit, fetch_range, final_cache, load_truncated, provisional_cache,
    };
    use chrono::NaiveDate;
    use std::path::Path;

    fn day(y: i32, m: u32, d: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, d).expect("valid date")
    }

    fn ts(d: NaiveDate) -> i64 {
        d.and_hms_opt(0, 0, 0)
            .expect("midnight")
            .and_utc()
            .timestamp()
    }

    fn page(posts: &[(String, i64)]) -> String {
        let items: Vec<String> = posts
            .iter()
            .map(|(id, t)| format!(r#"{{"id":"{id}","created_utc":{t},"title":"t"}}"#))
            .collect();
        format!(r#"{{"data":[{}]}}"#, items.join(","))
    }

    async fn on_after(server: &MockServer, after: i64, body: String) {
        Mock::given(method("GET"))
            .and(path(SEARCH))
            .and(query_param("after", after.to_string()))
            .respond_with(ResponseTemplate::new(200).set_body_string(body))
            .mount(server)
            .await;
    }

    async fn after_params(server: &MockServer) -> Vec<i64> {
        server
            .received_requests()
            .await
            .expect("recorded")
            .iter()
            .filter_map(|r| {
                r.url
                    .query_pairs()
                    .find(|(k, _)| k == "after")
                    .and_then(|(_, v)| v.parse().ok())
            })
            .collect()
    }

    fn client(server: &MockServer) -> ArcticClient {
        ArcticClient::new(&server.uri(), policy()).expect("client")
    }

    fn ranges(cache: &RawCache, sub: &str) -> Vec<(NaiveDate, NaiveDate)> {
        cache.load_manifest_for(sub).expect("manifest").covered()
    }

    fn manifest_text(root: &Path, sub: &str) -> String {
        std::fs::read_to_string(root.join("provisional").join(sub).join("manifest.json"))
            .expect("provisional manifest")
    }

    fn ok_empty() -> ResponseTemplate {
        ResponseTemplate::new(200).set_body_string(r#"{"data":[]}"#)
    }

    #[tokio::test]
    async fn tie_second_is_refetched_and_deduped() {
        let server = MockServer::start().await;
        let d = day(2024, 3, 4);
        let b = ts(d);
        let t = b + 97;
        let mut first: Vec<(String, i64)> = (0..97).map(|i| (format!("p{i}"), b + i)).collect();
        first.extend((97..100).map(|i| (format!("p{i}"), t)));
        let mut second: Vec<(String, i64)> = (97..100).map(|i| (format!("p{i}"), t)).collect();
        second.extend([("q0".to_owned(), t + 1), ("q1".to_owned(), t + 1)]);
        on_after(&server, b - 1, page(&first)).await;
        on_after(&server, t - 1, page(&second)).await;

        let mut c = client(&server);
        let fetch = fetch_range(&mut c, "stocks", d, d, None)
            .await
            .expect("ok")
            .expect("within budget");
        assert_eq!(fetch.posts.len(), 102);
        assert_eq!(after_params(&server).await, vec![b - 1, t - 1]);
    }

    #[tokio::test]
    async fn saturated_second_is_truncated_and_skipped() {
        let server = MockServer::start().await;
        let tmp = tempfile::tempdir().expect("tmp");
        let d = day(2024, 3, 4);
        let b = ts(d);
        let t = b + 10;
        let full: Vec<(String, i64)> = (0..100).map(|i| (format!("p{i}"), t)).collect();
        on_after(&server, b - 1, page(&full)).await;
        on_after(&server, t - 1, page(&full)).await;
        on_after(&server, t, page(&[("z".to_owned(), t + 1)])).await;

        let mut c = client(&server);
        let out = collect_subreddit(&mut c, tmp.path(), "stocks", d, d, day(2024, 4, 1), None)
            .await
            .expect("ok");
        assert_eq!(out.posts, 101);
        assert_eq!(out.truncated_seconds, 1);
        assert_eq!(ranges(&final_cache(tmp.path()), "stocks"), vec![(d, d)]);
        assert_eq!(load_truncated(tmp.path(), "stocks").expect("read"), vec![t]);
    }

    #[tokio::test]
    async fn busy_body_retried_then_range_not_committed() {
        let server = MockServer::start().await;
        let tmp = tempfile::tempdir().expect("tmp");
        Mock::given(method("GET"))
            .and(path(SEARCH))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_string(r#"{"data":null,"error":"Timeout. Maybe slow down a bit"}"#),
            )
            .mount(&server)
            .await;
        let d = day(2024, 3, 4);
        let mut c = client(&server);
        let err = collect_subreddit(&mut c, tmp.path(), "stocks", d, d, day(2024, 4, 1), None)
            .await
            .expect_err("busy");
        assert!(matches!(err, DataError::Busy { .. }), "{err:?}");
        assert_eq!(c.requests_sent(), 2);
        assert!(ranges(&final_cache(tmp.path()), "stocks").is_empty());
    }

    fn checkpoint_exists(root: &Path, sub: &str) -> bool {
        root.join(sub).join("checkpoint.json").exists()
    }

    #[tokio::test]
    async fn failure_on_second_page_resumes_from_checkpoint() {
        let tmp = tempfile::tempdir().expect("tmp");
        let d = day(2024, 3, 4);
        let b = ts(d);
        let full: Vec<(String, i64)> = (0..100).map(|i| (format!("p{i}"), b + i)).collect();
        let last = b + 99;

        let broken = MockServer::start().await;
        on_after(&broken, b - 1, page(&full)).await;
        Mock::given(method("GET"))
            .and(path(SEARCH))
            .and(query_param("after", (last - 1).to_string()))
            .respond_with(ResponseTemplate::new(500))
            .mount(&broken)
            .await;
        let mut c = client(&broken);
        collect_subreddit(&mut c, tmp.path(), "stocks", d, d, day(2024, 4, 1), None)
            .await
            .expect_err("500");
        assert!(ranges(&final_cache(tmp.path()), "stocks").is_empty());
        assert!(checkpoint_exists(tmp.path(), "stocks"));

        // Only the unfinished page is served: page 1 must come from the checkpoint.
        let healthy = MockServer::start().await;
        on_after(
            &healthy,
            last - 1,
            page(&[("p99".to_owned(), last), ("q".to_owned(), last + 1)]),
        )
        .await;
        let mut c = client(&healthy);
        let out = collect_subreddit(&mut c, tmp.path(), "stocks", d, d, day(2024, 4, 1), None)
            .await
            .expect("ok");
        assert_eq!(after_params(&healthy).await, vec![last - 1]);
        assert_eq!(out.final_weeks, 1);
        assert_eq!(out.posts, 101);
        assert_eq!(ranges(&final_cache(tmp.path()), "stocks"), vec![(d, d)]);
        assert!(!checkpoint_exists(tmp.path(), "stocks"));
        let records =
            asterion_data::reddit::daily::aggregate_subreddit(tmp.path(), "stocks").expect("agg");
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].post_count, 101);
    }

    #[tokio::test]
    async fn budget_exhausted_mid_week_resumes_next_run() {
        let server = MockServer::start().await;
        let tmp = tempfile::tempdir().expect("tmp");
        let d = day(2024, 3, 4);
        let b = ts(d);
        let full: Vec<(String, i64)> = (0..100).map(|i| (format!("p{i}"), b + i)).collect();
        on_after(&server, b - 1, page(&full)).await;
        on_after(&server, b + 98, page(&[])).await;
        let mut c = client(&server);
        let out = collect_subreddit(&mut c, tmp.path(), "stocks", d, d, day(2024, 4, 1), Some(1))
            .await
            .expect("ok");
        assert!(out.budget_exhausted);
        assert_eq!(c.requests_sent(), 1);
        assert!(ranges(&final_cache(tmp.path()), "stocks").is_empty());

        let mut c = client(&server);
        let out = collect_subreddit(&mut c, tmp.path(), "stocks", d, d, day(2024, 4, 1), None)
            .await
            .expect("ok");
        assert_eq!(after_params(&server).await, vec![b - 1, b + 98]);
        assert_eq!((out.final_weeks, out.posts), (1, 100));
        assert_eq!(ranges(&final_cache(tmp.path()), "stocks"), vec![(d, d)]);
    }

    #[tokio::test]
    async fn checkpoint_of_another_week_is_ignored() {
        let server = MockServer::start().await;
        let tmp = tempfile::tempdir().expect("tmp");
        let (d1, d2) = (day(2024, 3, 4), day(2024, 3, 11));
        let b1 = ts(d1);
        let full: Vec<(String, i64)> = (0..100).map(|i| (format!("p{i}"), b1 + i)).collect();
        on_after(&server, b1 - 1, page(&full)).await;
        on_after(&server, ts(d2) - 1, page(&[("w2".to_owned(), ts(d2))])).await;
        let mut c = client(&server);
        collect_subreddit(
            &mut c,
            tmp.path(),
            "stocks",
            d1,
            d1,
            day(2024, 4, 1),
            Some(1),
        )
        .await
        .expect("ok");
        assert!(checkpoint_exists(tmp.path(), "stocks"));

        let mut c = client(&server);
        let out = collect_subreddit(&mut c, tmp.path(), "stocks", d2, d2, day(2024, 4, 1), None)
            .await
            .expect("ok");
        assert_eq!((out.final_weeks, out.posts), (1, 1));
        assert_eq!(ranges(&final_cache(tmp.path()), "stocks"), vec![(d2, d2)]);
    }

    #[tokio::test]
    async fn busy_rounds_retry_page_before_failing() {
        let server = MockServer::start().await;
        let tmp = tempfile::tempdir().expect("tmp");
        // Three busy replies outlast one send (2 attempts) but not two rounds.
        Mock::given(method("GET"))
            .and(path(SEARCH))
            .respond_with(
                ResponseTemplate::new(422)
                    .set_body_string(r#"{"data":null,"error":"Timeout. Maybe slow down a bit"}"#),
            )
            .up_to_n_times(3)
            .with_priority(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path(SEARCH))
            .respond_with(ok_empty())
            .mount(&server)
            .await;
        let d = day(2024, 3, 4);
        let mut c = client(&server).with_busy_rounds(1, Duration::from_millis(1));
        let out = collect_subreddit(&mut c, tmp.path(), "stocks", d, d, day(2024, 4, 1), None)
            .await
            .expect("ok after busy rounds");
        assert_eq!(out.final_weeks, 1);
        assert_eq!(c.requests_sent(), 4);
    }

    #[tokio::test]
    async fn recent_days_go_to_provisional_and_are_refetched() {
        let server = MockServer::start().await;
        let tmp = tempfile::tempdir().expect("tmp");
        Mock::given(method("GET"))
            .and(path(SEARCH))
            .respond_with(ok_empty())
            .mount(&server)
            .await;
        let today = day(2026, 11, 16);
        let mut c = client(&server);
        for _ in 0..2 {
            let out = collect_subreddit(
                &mut c,
                tmp.path(),
                "stocks",
                day(2026, 11, 9),
                today,
                today,
                None,
            )
            .await
            .expect("ok");
            assert_eq!(out.final_weeks, 0);
            assert_eq!(out.provisional_days, 8);
        }
        assert_eq!(c.requests_sent(), 4);
        assert!(ranges(&final_cache(tmp.path()), "stocks").is_empty());
        assert_eq!(
            ranges(&provisional_cache(tmp.path()), "stocks"),
            vec![(day(2026, 11, 9), day(2026, 11, 15)), (today, today)]
        );
    }

    #[tokio::test]
    async fn provisional_content_is_replaced_not_appended() {
        let tmp = tempfile::tempdir().expect("tmp");
        let today = day(2026, 11, 16);
        let t = ts(today) + 60;
        for id in ["old", "new"] {
            let server = MockServer::start().await;
            Mock::given(method("GET"))
                .and(path(SEARCH))
                .respond_with(
                    ResponseTemplate::new(200).set_body_string(page(&[(id.to_owned(), t)])),
                )
                .mount(&server)
                .await;
            let mut c = client(&server);
            collect_subreddit(&mut c, tmp.path(), "stocks", today, today, today, None)
                .await
                .expect("ok");
        }
        let records =
            asterion_data::reddit::daily::aggregate_subreddit(tmp.path(), "stocks").expect("agg");
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].post_count, 1);
        assert!(
            !tmp.path()
                .join("provisional_staging")
                .join("stocks")
                .exists()
        );
        let files: Vec<String> = std::fs::read_dir(tmp.path().join("provisional").join("stocks"))
            .expect("dir")
            .map(|e| e.expect("entry").file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(files.len(), 2, "{files:?}"); // one page + manifest
    }

    #[tokio::test]
    async fn failed_backfill_keeps_previous_provisional() {
        let tmp = tempfile::tempdir().expect("tmp");
        let today = day(2026, 11, 16);
        let healthy = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path(SEARCH))
            .respond_with(ok_empty())
            .mount(&healthy)
            .await;
        let mut c = client(&healthy);
        collect_subreddit(
            &mut c,
            tmp.path(),
            "stocks",
            day(2026, 11, 9),
            today,
            today,
            None,
        )
        .await
        .expect("ok");
        let before = manifest_text(tmp.path(), "stocks");

        let broken = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path(SEARCH))
            .respond_with(ResponseTemplate::new(500))
            .mount(&broken)
            .await;
        let mut c = client(&broken);
        collect_subreddit(
            &mut c,
            tmp.path(),
            "stocks",
            day(2026, 11, 2),
            today,
            today,
            None,
        )
        .await
        .expect_err("500");
        assert_eq!(manifest_text(tmp.path(), "stocks"), before);
    }

    #[tokio::test]
    async fn posts_outside_range_are_dropped() {
        let server = MockServer::start().await;
        let d = day(2024, 3, 4);
        let b = ts(d);
        let end = ts(day(2024, 3, 5));
        on_after(
            &server,
            b - 1,
            page(&[("in".to_owned(), b), ("out".to_owned(), end)]),
        )
        .await;
        let mut c = client(&server);
        let fetch = fetch_range(&mut c, "stocks", d, d, None)
            .await
            .expect("ok")
            .expect("within budget");
        let ids: Vec<&str> = fetch.posts.iter().map(|p| p.id.as_str()).collect();
        assert_eq!(ids, vec!["in"]);
    }
}
