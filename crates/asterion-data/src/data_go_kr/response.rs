use asterion_core::Bar;
use chrono::NaiveDate;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::error::DataError;

/// One row of `getStockPriceInfo`. Prices are unadjusted KRW.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct KrxDailyQuote {
    pub date: NaiveDate,
    pub code: String,
    pub isin: String,
    pub name: String,
    /// `KOSPI`, `KOSDAQ` or `KONEX`.
    pub market_category: String,
    pub open: f64,
    pub high: f64,
    pub low: f64,
    pub close: f64,
    pub volume: u64,
    pub trading_value: u64,
    pub listed_shares: u64,
    pub market_cap: u64,
    // `vs`: close minus the exchange base price, which already reflects corporate actions.
    #[serde(default)]
    pub change: Option<f64>,
}

impl KrxDailyQuote {
    pub fn to_bar(&self) -> Bar {
        Bar {
            date: self.date,
            open: self.open,
            high: self.high,
            low: self.low,
            close: self.close,
            adj_close: None,
            volume: self.volume,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Page {
    pub page_no: u32,
    pub num_of_rows: u32,
    pub total_count: u64,
    pub quotes: Vec<KrxDailyQuote>,
}

impl Page {
    pub fn has_more(&self) -> bool {
        !self.quotes.is_empty()
            && u64::from(self.page_no) * u64::from(self.num_of_rows) < self.total_count
    }
}

// Paging fields plus undecoded rows, shared by every 1160100 service.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct RawPage {
    pub page_no: u32,
    pub num_of_rows: u32,
    pub total_count: u64,
    pub items: Vec<Value>,
}

impl RawPage {
    pub(crate) fn has_more(&self) -> bool {
        !self.items.is_empty()
            && u64::from(self.page_no) * u64::from(self.num_of_rows) < self.total_count
    }
}

#[derive(Deserialize)]
struct Envelope {
    response: RawResponse,
}

#[derive(Deserialize)]
struct RawResponse {
    header: RawHeader,
    #[serde(default)]
    body: Option<RawBody>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawHeader {
    result_code: String,
    #[serde(default)]
    result_msg: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawBody {
    #[serde(default)]
    num_of_rows: Value,
    #[serde(default)]
    page_no: Value,
    #[serde(default)]
    total_count: Value,
    #[serde(default)]
    items: Value,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawItem {
    #[serde(default)]
    bas_dt: Value,
    #[serde(default)]
    srtn_cd: Value,
    #[serde(default)]
    isin_cd: Value,
    #[serde(default)]
    itms_nm: Value,
    #[serde(default)]
    mrkt_ctg: Value,
    #[serde(default)]
    mkp: Value,
    #[serde(default)]
    hipr: Value,
    #[serde(default)]
    lopr: Value,
    #[serde(default)]
    clpr: Value,
    #[serde(default)]
    trqu: Value,
    #[serde(default)]
    tr_prc: Value,
    #[serde(default)]
    lstg_st_cnt: Value,
    #[serde(default)]
    mrkt_tot_amt: Value,
    #[serde(default)]
    vs: Value,
}

/// Parses one raw response body. The portal answers key/quota errors with an
/// XML envelope even when `resultType=json`, so that case is mapped to `Api`.
pub fn parse_page(body: &str) -> Result<Page, DataError> {
    let raw = parse_raw_page(body)?;
    let quotes = raw
        .items
        .into_iter()
        .map(|v| {
            serde_json::from_value::<RawItem>(v)
                .map_err(DataError::from)?
                .parse()
        })
        .collect::<Result<Vec<_>, _>>()?;
    Ok(Page {
        page_no: raw.page_no,
        num_of_rows: raw.num_of_rows,
        total_count: raw.total_count,
        quotes,
    })
}

pub(crate) fn parse_raw_page(body: &str) -> Result<RawPage, DataError> {
    let body = body.trim_start_matches('\u{feff}').trim_start();
    if body.starts_with('<') {
        return Err(xml_error(body));
    }

    let envelope: Envelope = serde_json::from_str(body)?;
    let header = envelope.response.header;
    if header.result_code != "00" {
        return Err(DataError::Api {
            code: header.result_code,
            message: header.result_msg,
        });
    }

    let Some(raw) = envelope.response.body else {
        return Ok(RawPage {
            page_no: 0,
            num_of_rows: 0,
            total_count: 0,
            items: Vec::new(),
        });
    };

    Ok(RawPage {
        page_no: u32_field(&raw.page_no, "pageNo")?,
        num_of_rows: u32_field(&raw.num_of_rows, "numOfRows")?,
        total_count: int_field(&raw.total_count, "totalCount")?,
        items: item_values(raw.items)?,
    })
}

// `items` is `{"item": [...]}`, `{"item": {...}}` for a single row, or `""` when empty.
fn item_values(items: Value) -> Result<Vec<Value>, DataError> {
    match items {
        Value::Null => Ok(Vec::new()),
        Value::String(s) if s.trim().is_empty() => Ok(Vec::new()),
        Value::Object(mut map) => match map.remove("item") {
            None | Some(Value::Null) => Ok(Vec::new()),
            Some(Value::Array(rows)) => Ok(rows),
            Some(row @ Value::Object(_)) => Ok(vec![row]),
            Some(other) => Err(invalid("items.item", &other)),
        },
        other => Err(invalid("items", &other)),
    }
}

impl RawItem {
    fn parse(self) -> Result<KrxDailyQuote, DataError> {
        let date_text = text_field(&self.bas_dt, "basDt")?;
        let date = NaiveDate::parse_from_str(&date_text, "%Y%m%d")
            .map_err(|_| invalid("basDt", &self.bas_dt))?;
        Ok(KrxDailyQuote {
            date,
            code: text_field(&self.srtn_cd, "srtnCd")?,
            isin: text_field(&self.isin_cd, "isinCd")?,
            name: text_field(&self.itms_nm, "itmsNm")?,
            market_category: text_field(&self.mrkt_ctg, "mrktCtg")?,
            open: price_field(&self.mkp, "mkp")?,
            high: price_field(&self.hipr, "hipr")?,
            low: price_field(&self.lopr, "lopr")?,
            close: price_field(&self.clpr, "clpr")?,
            volume: int_field(&self.trqu, "trqu")?,
            trading_value: int_field(&self.tr_prc, "trPrc")?,
            listed_shares: int_field(&self.lstg_st_cnt, "lstgStCnt")?,
            market_cap: int_field(&self.mrkt_tot_amt, "mrktTotAmt")?,
            change: decimal_field(&self.vs),
        })
    }
}

pub(crate) fn invalid(field: &'static str, value: &Value) -> DataError {
    let mut value = value.to_string();
    value.truncate(80);
    DataError::InvalidField { field, value }
}

pub(crate) fn text_field(v: &Value, field: &'static str) -> Result<String, DataError> {
    match v {
        Value::String(s) if !s.trim().is_empty() => Ok(s.trim().to_owned()),
        Value::Number(n) => Ok(n.to_string()),
        other => Err(invalid(field, other)),
    }
}

fn int_field(v: &Value, field: &'static str) -> Result<u64, DataError> {
    match v {
        Value::Number(n) => n.as_u64().ok_or_else(|| invalid(field, v)),
        Value::String(s) => s
            .trim()
            .replace(',', "")
            .parse::<u64>()
            .map_err(|_| invalid(field, v)),
        other => Err(invalid(field, other)),
    }
}

// Signed decimal such as "-1600", ".9" or "2655.28"; `None` when absent or malformed.
pub(crate) fn decimal_field(v: &Value) -> Option<f64> {
    let parsed = match v {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => s.trim().replace(',', "").parse::<f64>().ok(),
        _ => None,
    };
    parsed.filter(|x| x.is_finite())
}

fn u32_field(v: &Value, field: &'static str) -> Result<u32, DataError> {
    u32::try_from(int_field(v, field)?).map_err(|_| invalid(field, v))
}

// KRX prices are whole won; integers up to 2^53 convert to f64 exactly.
fn price_field(v: &Value, field: &'static str) -> Result<f64, DataError> {
    int_field(v, field).map(|p| p as f64)
}

fn xml_error(body: &str) -> DataError {
    let code = xml_tag(body, "returnReasonCode").or_else(|| xml_tag(body, "resultCode"));
    let message = xml_tag(body, "returnAuthMsg")
        .or_else(|| xml_tag(body, "errMsg"))
        .or_else(|| xml_tag(body, "resultMsg"));
    let mut fallback = body.to_owned();
    fallback.truncate(200);
    DataError::Api {
        code: code.unwrap_or_else(|| "xml".to_owned()),
        message: message.unwrap_or(fallback),
    }
}

fn xml_tag(body: &str, tag: &str) -> Option<String> {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let start = body.find(&open)? + open.len();
    let len = body[start..].find(&close)?;
    Some(body[start..start + len].trim().to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    const TWO_ROWS: &str = r#"{"response":{"header":{"resultCode":"00","resultMsg":"NORMAL SERVICE."},"body":{"numOfRows":2,"pageNo":1,"totalCount":3,"items":{"item":[
        {"basDt":"20240103","srtnCd":"005930","isinCd":"KR7005930003","itmsNm":"삼성전자","mrktCtg":"KOSPI","clpr":"77000","vs":"-1600","fltRt":"-2.04","mkp":"78500","hipr":"78800","lopr":"77000","trqu":"21753644","trPrc":"1688993813900","lstgStCnt":"5969782550","mrktTotAmt":"459673256350000"},
        {"basDt":"20240102","srtnCd":"005930","isinCd":"KR7005930003","itmsNm":"삼성전자","mrktCtg":"KOSPI","clpr":"78600","vs":"1600","fltRt":"2.08","mkp":"77500","hipr":"79000","lopr":"76700","trqu":"17142847","trPrc":"1335081957300","lstgStCnt":"5969782550","mrktTotAmt":"469224903430000"}
    ]}}}}"#;

    #[test]
    fn parses_rows_and_paging() {
        let page = parse_page(TWO_ROWS).expect("parse");
        assert_eq!(page.page_no, 1);
        assert_eq!(page.num_of_rows, 2);
        assert_eq!(page.total_count, 3);
        assert!(page.has_more());
        let q = &page.quotes[1];
        assert_eq!(q.date, NaiveDate::from_ymd_opt(2024, 1, 2).expect("date"));
        assert_eq!(q.code, "005930");
        assert_eq!(q.market_category, "KOSPI");
        assert_eq!(
            (q.open, q.high, q.low, q.close),
            (77500.0, 79000.0, 76700.0, 78600.0)
        );
        assert_eq!(q.volume, 17_142_847);
        assert_eq!(q.market_cap, 469_224_903_430_000);
        assert_eq!(q.change, Some(1600.0));
        assert_eq!(page.quotes[0].change, Some(-1600.0));
        assert!(q.to_bar().is_ohlc_consistent());
    }

    #[test]
    fn empty_items_string_is_an_empty_page() {
        let body = r#"{"response":{"header":{"resultCode":"00","resultMsg":"OK"},"body":{"numOfRows":1000,"pageNo":1,"totalCount":0,"items":""}}}"#;
        let page = parse_page(body).expect("parse");
        assert!(page.quotes.is_empty());
        assert!(!page.has_more());
    }

    #[test]
    fn single_item_object_is_accepted() {
        let body = r#"{"response":{"header":{"resultCode":"00","resultMsg":"OK"},"body":{"numOfRows":"10","pageNo":"1","totalCount":"1","items":{"item":
            {"basDt":"20240102","srtnCd":"000660","isinCd":"KR7000660001","itmsNm":"SK하이닉스","mrktCtg":"KOSPI","clpr":"138100","mkp":"136600","hipr":"138500","lopr":"135800","trqu":"2147483","trPrc":"295000000000","lstgStCnt":"728002365","mrktTotAmt":"100537126606500"}}}}}"#;
        let page = parse_page(body).expect("parse");
        assert_eq!(page.quotes.len(), 1);
        assert_eq!(page.quotes[0].code, "000660");
        assert_eq!(page.quotes[0].change, None, "vs is optional");
    }

    #[test]
    fn json_error_header_maps_to_api_error() {
        let body = r#"{"response":{"header":{"resultCode":"10","resultMsg":"INVALID_REQUEST_PARAMETER_ERROR"}}}"#;
        match parse_page(body) {
            Err(DataError::Api { code, message }) => {
                assert_eq!(code, "10");
                assert_eq!(message, "INVALID_REQUEST_PARAMETER_ERROR");
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn xml_error_envelope_maps_to_api_error() {
        let body = "<OpenAPI_ServiceResponse><cmmMsgHeader><errMsg>SERVICE ERROR</errMsg><returnAuthMsg>SERVICE_KEY_IS_NOT_REGISTERED_ERROR</returnAuthMsg><returnReasonCode>30</returnReasonCode></cmmMsgHeader></OpenAPI_ServiceResponse>";
        match parse_page(body) {
            Err(DataError::Api { code, message }) => {
                assert_eq!(code, "30");
                assert_eq!(message, "SERVICE_KEY_IS_NOT_REGISTERED_ERROR");
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn bad_number_names_the_field() {
        let body = TWO_ROWS.replacen("\"78500\"", "\"-\"", 1);
        match parse_page(&body) {
            Err(DataError::InvalidField { field, .. }) => assert_eq!(field, "mkp"),
            other => panic!("unexpected {other:?}"),
        }
    }
}
