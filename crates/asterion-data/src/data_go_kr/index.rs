use chrono::NaiveDate;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::response::{decimal_field, invalid, parse_raw_page, text_field};
use crate::error::DataError;

// `key` is the ASCII cache directory; `name` is the exact `idxNm` the API expects.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IndexSpec {
    pub key: &'static str,
    pub name: &'static str,
}

pub const KOSPI: IndexSpec = IndexSpec {
    key: "kospi",
    name: "코스피",
};
pub const KOSDAQ: IndexSpec = IndexSpec {
    key: "kosdaq",
    name: "코스닥",
};
pub const KOSPI200: IndexSpec = IndexSpec {
    key: "kospi200",
    name: "코스피 200",
};
pub const INDICES: [IndexSpec; 3] = [KOSPI, KOSDAQ, KOSPI200];

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct IndexQuote {
    pub date: NaiveDate,
    pub name: String,
    pub close: f64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct IndexPage {
    pub page_no: u32,
    pub has_more: bool,
    pub quotes: Vec<IndexQuote>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawIndexItem {
    #[serde(default)]
    bas_dt: Value,
    #[serde(default)]
    idx_nm: Value,
    #[serde(default)]
    clpr: Value,
}

pub fn parse_index_page(body: &str) -> Result<IndexPage, DataError> {
    let raw = parse_raw_page(body)?;
    let has_more = raw.has_more();
    let quotes = raw
        .items
        .into_iter()
        .map(|v| {
            let item: RawIndexItem = serde_json::from_value(v)?;
            let date_text = text_field(&item.bas_dt, "basDt")?;
            let date = NaiveDate::parse_from_str(&date_text, "%Y%m%d")
                .map_err(|_| invalid("basDt", &item.bas_dt))?;
            let close = decimal_field(&item.clpr)
                .filter(|c| *c > 0.0)
                .ok_or_else(|| invalid("clpr", &item.clpr))?;
            Ok(IndexQuote {
                date,
                name: text_field(&item.idx_nm, "idxNm")?,
                close,
            })
        })
        .collect::<Result<Vec<_>, DataError>>()?;
    Ok(IndexPage {
        page_no: raw.page_no,
        has_more,
        quotes,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_decimal_index_levels() {
        let body = r#"{"response":{"header":{"resultCode":"00","resultMsg":"NORMAL SERVICE."},"body":{"numOfRows":1000,"pageNo":1,"totalCount":2,"items":{"item":[
            {"basDt":"20240103","idxNm":"코스피","idxCsf":"KOSPI시리즈","clpr":"2607.31","vs":"-62.5","fltRt":"-2.34","mkp":"2643.54","hipr":"2643.73","lopr":"2607.31"},
            {"basDt":"20240102","idxNm":"코스피","idxCsf":"KOSPI시리즈","clpr":"2,669.81","vs":"14.53","fltRt":".55"}
        ]}}}}"#;
        let page = parse_index_page(body).expect("parse");
        assert_eq!(page.page_no, 1);
        assert!(!page.has_more);
        assert_eq!(page.quotes[0].name, KOSPI.name);
        assert_eq!(page.quotes[0].close, 2607.31);
        assert_eq!(page.quotes[1].close, 2669.81);
    }

    #[test]
    fn missing_close_names_the_field() {
        let body = r#"{"response":{"header":{"resultCode":"00","resultMsg":"OK"},"body":{"numOfRows":10,"pageNo":1,"totalCount":1,"items":{"item":[{"basDt":"20240103","idxNm":"코스피","clpr":""}]}}}}"#;
        match parse_index_page(body) {
            Err(DataError::InvalidField { field, .. }) => assert_eq!(field, "clpr"),
            other => panic!("unexpected {other:?}"),
        }
    }
}
