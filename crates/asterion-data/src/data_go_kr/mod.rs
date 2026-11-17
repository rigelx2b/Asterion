//! 금융위원회_주식시세정보 (`GetStockSecuritiesInfoService_V2`) and
//! 금융위원회_지수시세정보 (`GetMarketIndexInfoService_V2`).
//!
//! 출처: 금융위원회, 공공데이터포털(data.go.kr)

mod client;
mod collect;
mod index;
mod response;
mod universe;

pub use client::{ClientConfig, DataGoKrClient, PageRequest};
pub use collect::{
    CollectOutcome, collect_index, collect_symbol, latest_available_date, missing_ranges,
};
pub use index::{
    INDICES, IndexPage, IndexQuote, IndexSpec, KOSDAQ, KOSPI, KOSPI200, parse_index_page,
};
pub use response::{KrxDailyQuote, Page, parse_page};
pub use universe::{fetch_snapshot, select_top_by_market_cap};

pub const SOURCE_ID: &str = "data_go_kr";
pub const INDEX_SOURCE_ID: &str = "data_go_kr_index";
pub const ENDPOINT: &str =
    "https://apis.data.go.kr/1160100/GetStockSecuritiesInfoService_V2/getStockPriceInfo_V2";
pub const INDEX_ENDPOINT: &str =
    "https://apis.data.go.kr/1160100/GetMarketIndexInfoService_V2/getStockMarketIndex_V2";
pub const MAX_ROWS_PER_PAGE: u32 = 1000;
/// Attribution required by the 제4유형 licence (plan §6).
pub const ATTRIBUTION: &str = "출처: 금융위원회, 공공데이터포털(data.go.kr)";
