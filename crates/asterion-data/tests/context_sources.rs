use std::time::Duration;

use asterion_data::data_go_kr::{
    ClientConfig, DataGoKrClient, KOSPI, collect_index, fetch_snapshot,
};
use asterion_data::fred::{self, FredClient, FredKey};
use asterion_data::{DataError, RawCache, RetryPolicy, ServiceKey};
use chrono::NaiveDate;
use wiremock::matchers::{method, path, query_param, query_param_is_missing};
use wiremock::{Mock, MockServer, ResponseTemplate};

const STOCK_PATH: &str = "/getStockPriceInfo";
const INDEX_PATH: &str = "/getStockMarketIndex";
const FRED_PATH: &str = "/fred/series/observations";
const KEY: &str = "test+key/==";

fn d(m: u32, day: u32) -> NaiveDate {
    NaiveDate::from_ymd_opt(2024, m, day).expect("valid date")
}

fn policy() -> RetryPolicy {
    RetryPolicy {
        min_interval: Duration::ZERO,
        max_attempts: 2,
        initial_backoff: Duration::from_millis(1),
        timeout: Duration::from_secs(5),
    }
}

fn krx_client(server: &MockServer) -> DataGoKrClient {
    let config = ClientConfig {
        endpoint: format!("{}{STOCK_PATH}", server.uri()),
        index_endpoint: format!("{}{INDEX_PATH}", server.uri()),
        min_interval: Duration::ZERO,
        max_attempts: 2,
        initial_backoff: Duration::from_millis(1),
        timeout: Duration::from_secs(5),
    };
    DataGoKrClient::new(ServiceKey::new(KEY), config).expect("client")
}

fn envelope(page_no: u32, total: u32, items: &[String]) -> String {
    format!(
        r#"{{"response":{{"header":{{"resultCode":"00","resultMsg":"NORMAL SERVICE."}},"body":{{"numOfRows":1000,"pageNo":{page_no},"totalCount":{total},"items":{{"item":[{}]}}}}}}}}"#,
        items.join(",")
    )
}

fn index_item(date: &str, name: &str, close: &str) -> String {
    format!(r#"{{"basDt":"{date}","idxNm":"{name}","clpr":"{close}"}}"#)
}

fn stock_item(date: &str, code: &str, market_cap: u64) -> String {
    format!(
        r#"{{"basDt":"{date}","srtnCd":"{code}","isinCd":"KR7{code}003","itmsNm":"X","mrktCtg":"KOSPI","clpr":"100","vs":"0","mkp":"100","hipr":"101","lopr":"99","trqu":"10","trPrc":"1000","lstgStCnt":"5","mrktTotAmt":"{market_cap}"}}"#
    )
}

#[tokio::test]
async fn index_is_requested_by_exact_name_and_cached() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path(INDEX_PATH))
        .and(query_param("serviceKey", KEY))
        .and(query_param("idxNm", KOSPI.name))
        .and(query_param("beginBasDt", "20240101"))
        .and(query_param("endBasDt", "20240201"))
        .respond_with(ResponseTemplate::new(200).set_body_string(envelope(
            1,
            3,
            &[
                index_item("20240103", KOSPI.name, "2607.31"),
                index_item("20240102", KOSPI.name, "2669.81"),
                index_item("20240102", "코스피 200", "358.79"),
            ],
        )))
        .expect(1)
        .mount(&server)
        .await;

    let dir = tempfile::tempdir().expect("tempdir");
    let cache = RawCache::for_index(dir.path());
    let mut client = krx_client(&server);

    let first = collect_index(&mut client, &cache, &KOSPI, d(1, 1), d(1, 31), d(6, 28))
        .await
        .expect("first");
    assert_eq!((first.ranges_fetched, first.pages, first.rows), (1, 1, 2));
    let second = collect_index(&mut client, &cache, &KOSPI, d(1, 1), d(1, 31), d(6, 28))
        .await
        .expect("second");
    assert_eq!(second.ranges_fetched, 0);
    assert_eq!(client.requests_sent(), 1);

    let quotes = cache.load_index_quotes(&KOSPI).expect("load");
    let closes: Vec<f64> = quotes.iter().map(|q| q.close).collect();
    assert_eq!(closes, vec![2669.81, 2607.31]);
}

#[tokio::test]
async fn snapshot_pages_through_one_trading_date() {
    let server = MockServer::start().await;
    for (page_no, code) in [(1, "005930"), (2, "000660")] {
        Mock::given(method("GET"))
            .and(path(STOCK_PATH))
            .and(query_param("basDt", "20240102"))
            .and(query_param("pageNo", page_no.to_string()))
            .and(query_param_is_missing("likeSrtnCd"))
            .respond_with(ResponseTemplate::new(200).set_body_string(format!(
                r#"{{"response":{{"header":{{"resultCode":"00","resultMsg":"OK"}},"body":{{"numOfRows":1,"pageNo":{page_no},"totalCount":2,"items":{{"item":[{}]}}}}}}}}"#,
                stock_item("20240102", code, 500)
            )))
            .expect(1)
            .mount(&server)
            .await;
    }

    let mut client = krx_client(&server);
    let snapshot = fetch_snapshot(&mut client, d(1, 2))
        .await
        .expect("snapshot");
    let codes: Vec<&str> = snapshot.iter().map(|q| q.code.as_str()).collect();
    assert_eq!(codes, vec!["005930", "000660"]);
}

fn fred_body(rows: &[(&str, &str)]) -> String {
    let observations: Vec<String> = rows
        .iter()
        .map(|(date, value)| format!(r#"{{"date":"{date}","value":"{value}"}}"#))
        .collect();
    format!(
        r#"{{"count":{},"observations":[{}]}}"#,
        rows.len(),
        observations.join(",")
    )
}

#[tokio::test]
async fn fred_series_is_covered_up_to_its_last_observation() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path(FRED_PATH))
        .and(query_param("series_id", fred::SP500))
        .and(query_param("api_key", "fredkey"))
        .and(query_param("file_type", "json"))
        .and(query_param("observation_start", "2024-01-01"))
        .and(query_param("observation_end", "2024-01-10"))
        .respond_with(ResponseTemplate::new(200).set_body_string(fred_body(&[
            ("2024-01-01", "."),
            ("2024-01-02", "4742.83"),
            ("2024-01-03", "4704.81"),
            ("2024-01-04", "."),
        ])))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path(FRED_PATH))
        .and(query_param("observation_start", "2024-01-04"))
        .respond_with(ResponseTemplate::new(200).set_body_string(fred_body(&[])))
        .expect(1)
        .mount(&server)
        .await;

    let dir = tempfile::tempdir().expect("tempdir");
    let cache = RawCache::with_source(dir.path(), fred::SOURCE_ID);
    let endpoint = format!("{}{FRED_PATH}", server.uri());
    let mut client = FredClient::new(FredKey::new("fredkey"), &endpoint, policy()).expect("client");

    let first = fred::collect_series(&mut client, &cache, fred::SP500, d(1, 1), d(1, 10))
        .await
        .expect("first");
    assert_eq!((first.ranges_fetched, first.rows), (1, 2));
    let manifest = cache.load_manifest_for(fred::SP500).expect("manifest");
    assert_eq!(manifest.source, fred::SOURCE_ID);
    assert_eq!(manifest.covered(), vec![(d(1, 1), d(1, 3))]);

    let second = fred::collect_series(&mut client, &cache, fred::SP500, d(1, 1), d(1, 10))
        .await
        .expect("second");
    assert_eq!(second.ranges_fetched, 0);
    assert_eq!(client.requests_sent(), 2);

    let series = fred::load_series(&cache, fred::SP500).expect("load");
    assert_eq!(series.len(), 2);
    assert_eq!(series[1].value, 4704.81);
}

#[tokio::test]
async fn fred_client_error_is_not_retried_and_hides_the_key() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(
            ResponseTemplate::new(400)
                .set_body_string(r#"{"error_code":400,"error_message":"Bad Request."}"#),
        )
        .expect(1)
        .mount(&server)
        .await;

    let endpoint = format!("{}{FRED_PATH}", server.uri());
    let mut client = FredClient::new(FredKey::new("fredkey"), &endpoint, policy()).expect("client");
    let err = client
        .fetch_observations(fred::VIX, d(1, 1), d(1, 2))
        .await
        .expect_err("400");
    assert!(matches!(err, DataError::HttpStatus { status: 400 }));
    assert!(!format!("{err} {client:?}").contains("fredkey"));
}
