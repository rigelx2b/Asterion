use std::time::Duration;

use asterion_core::{Market, Symbol};
use asterion_data::data_go_kr::{ClientConfig, DataGoKrClient, PageRequest, collect_symbol};
use asterion_data::{DataError, RawCache, ServiceKey};
use chrono::NaiveDate;
use wiremock::matchers::{method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

const PATH: &str = "/getStockPriceInfo";
const INDEX_PATH: &str = "/getStockMarketIndex";
const KEY: &str = "test+key/==";

fn d(m: u32, day: u32) -> NaiveDate {
    NaiveDate::from_ymd_opt(2024, m, day).expect("valid date")
}

fn client(server: &MockServer) -> DataGoKrClient {
    let config = ClientConfig {
        endpoint: format!("{}{PATH}", server.uri()),
        index_endpoint: format!("{}{INDEX_PATH}", server.uri()),
        min_interval: Duration::ZERO,
        max_attempts: 3,
        initial_backoff: Duration::from_millis(1),
        timeout: Duration::from_secs(5),
    };
    DataGoKrClient::new(ServiceKey::new(KEY), config).expect("client")
}

fn item(date: &str, code: &str) -> String {
    format!(
        r#"{{"basDt":"{date}","srtnCd":"{code}","isinCd":"KR7{code}003","itmsNm":"X","mrktCtg":"KOSPI","clpr":"100","mkp":"100","hipr":"101","lopr":"99","trqu":"10","trPrc":"1000","lstgStCnt":"5","mrktTotAmt":"500"}}"#
    )
}

fn body(page_no: u32, num_of_rows: u32, total: u32, items: &[String]) -> String {
    format!(
        r#"{{"response":{{"header":{{"resultCode":"00","resultMsg":"NORMAL SERVICE."}},"body":{{"numOfRows":{num_of_rows},"pageNo":{page_no},"totalCount":{total},"items":{{"item":[{}]}}}}}}}}"#,
        items.join(",")
    )
}

#[tokio::test]
async fn sends_plan_parameters_with_exclusive_end_date() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path(PATH))
        .and(query_param("serviceKey", KEY))
        .and(query_param("resultType", "json"))
        .and(query_param("likeSrtnCd", "005930"))
        .and(query_param("beginBasDt", "20240101"))
        .and(query_param("endBasDt", "20240201"))
        .and(query_param("numOfRows", "1000"))
        .and(query_param("pageNo", "1"))
        .respond_with(ResponseTemplate::new(200).set_body_string(body(1, 1000, 0, &[])))
        .expect(1)
        .mount(&server)
        .await;

    let mut client = client(&server);
    let req = PageRequest {
        code: "005930",
        begin: d(1, 1),
        end: d(1, 31),
        page_no: 1,
    };
    client.fetch_page(&req).await.expect("fetch");
}

#[tokio::test]
async fn client_errors_are_not_retried() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(401))
        .expect(1)
        .mount(&server)
        .await;

    let mut client = client(&server);
    let req = PageRequest {
        code: "005930",
        begin: d(1, 1),
        end: d(1, 31),
        page_no: 1,
    };
    let err = client.fetch_page(&req).await.expect_err("401");
    assert!(matches!(err, DataError::HttpStatus { status: 401 }));
    assert!(err.is_fatal_for_batch());
    assert_eq!(client.requests_sent(), 1);
}

#[tokio::test]
async fn server_errors_are_retried_up_to_the_limit() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(503))
        .expect(3)
        .mount(&server)
        .await;

    let mut client = client(&server);
    let req = PageRequest {
        code: "005930",
        begin: d(1, 1),
        end: d(1, 31),
        page_no: 1,
    };
    let err = client.fetch_page(&req).await.expect_err("503");
    assert!(matches!(err, DataError::HttpStatus { status: 503 }));
    assert!(!err.is_fatal_for_batch());
    assert!(!err.to_string().contains("test"), "no key in message");
}

#[tokio::test]
async fn collect_pages_caches_and_skips_cached_ranges() {
    let server = MockServer::start().await;
    // Page 1 includes a partial likeSrtnCd match that must be dropped.
    Mock::given(method("GET"))
        .and(query_param("pageNo", "1"))
        .respond_with(ResponseTemplate::new(200).set_body_string(body(
            1,
            2,
            3,
            &[item("20240102", "005930"), item("20240102", "105930")],
        )))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(query_param("pageNo", "2"))
        .respond_with(ResponseTemplate::new(200).set_body_string(body(
            2,
            2,
            3,
            &[item("20240103", "005930")],
        )))
        .expect(1)
        .mount(&server)
        .await;

    let dir = tempfile::tempdir().expect("tempdir");
    let cache = RawCache::new(dir.path());
    let symbol = Symbol::new(Market::Kr, "005930").expect("symbol");
    let mut client = client(&server);

    let latest = d(6, 28);
    let first = collect_symbol(&mut client, &cache, &symbol, d(1, 1), d(1, 31), latest)
        .await
        .expect("collect");
    assert_eq!((first.ranges_fetched, first.pages, first.rows), (1, 2, 2));

    // Same range again: fully cached, so no request is sent.
    let again = collect_symbol(&mut client, &cache, &symbol, d(1, 1), d(1, 31), latest)
        .await
        .expect("collect again");
    assert_eq!(again.pages, 0);
    assert_eq!(client.requests_sent(), 2);

    let quotes = cache.load_quotes(&symbol).expect("load");
    let dates: Vec<_> = quotes.iter().map(|q| q.date).collect();
    assert_eq!(dates, vec![d(1, 2), d(1, 3)]);
    assert!(quotes.iter().all(|q| q.code == "005930"));
}

#[tokio::test]
async fn newest_range_is_covered_only_up_to_returned_rows() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(query_param("beginBasDt", "20240101"))
        .respond_with(ResponseTemplate::new(200).set_body_string(body(
            1,
            1000,
            1,
            &[item("20240102", "005930")],
        )))
        .expect(1)
        .mount(&server)
        .await;
    // The follow-up run asks only for the day after the last returned row.
    Mock::given(method("GET"))
        .and(query_param("beginBasDt", "20240103"))
        .and(query_param("endBasDt", "20240104"))
        .respond_with(ResponseTemplate::new(200).set_body_string(body(1, 1000, 0, &[])))
        .expect(1)
        .mount(&server)
        .await;

    let dir = tempfile::tempdir().expect("tempdir");
    let cache = RawCache::new(dir.path());
    let symbol = Symbol::new(Market::Kr, "005930").expect("symbol");
    let mut client = client(&server);

    // latest_available = Jan 3, but the portal has only published Jan 2 so far.
    collect_symbol(&mut client, &cache, &symbol, d(1, 1), d(12, 31), d(1, 3))
        .await
        .expect("first");
    let manifest = cache.load_manifest(&symbol).expect("manifest");
    assert_eq!(manifest.covered(), vec![(d(1, 1), d(1, 2))]);

    collect_symbol(&mut client, &cache, &symbol, d(1, 1), d(12, 31), d(1, 3))
        .await
        .expect("second");
    assert_eq!(client.requests_sent(), 2);
}
