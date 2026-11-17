use chrono::NaiveDate;

use super::client::DataGoKrClient;
use super::response::{KrxDailyQuote, parse_page};
use crate::error::DataError;

// All rows the portal lists for one trading date; empty on non-trading days.
pub async fn fetch_snapshot(
    client: &mut DataGoKrClient,
    date: NaiveDate,
) -> Result<Vec<KrxDailyQuote>, DataError> {
    let mut quotes = Vec::new();
    let mut page_no = 1;
    loop {
        let body = client.fetch_snapshot_page(date, page_no).await?;
        let page = parse_page(&body)?;
        if !page.quotes.is_empty() && page.page_no != page_no {
            return Err(DataError::UnexpectedPage {
                expected: page_no,
                got: page.page_no,
            });
        }
        let more = page.has_more();
        quotes.extend(page.quotes.into_iter().filter(|q| q.date == date));
        if !more {
            break;
        }
        page_no += 1;
    }
    Ok(quotes)
}

// KRX common shares end in '0'; preferred shares use other final characters.
fn is_common_share(q: &KrxDailyQuote) -> bool {
    q.code.ends_with('0')
}

fn is_spac(q: &KrxDailyQuote) -> bool {
    q.name.contains("스팩")
}

// Largest `top` KOSPI/KOSDAQ common shares by market cap, ties broken by code.
pub fn select_top_by_market_cap(snapshot: &[KrxDailyQuote], top: usize) -> Vec<&KrxDailyQuote> {
    let mut eligible: Vec<&KrxDailyQuote> = snapshot
        .iter()
        .filter(|q| matches!(q.market_category.as_str(), "KOSPI" | "KOSDAQ"))
        .filter(|q| is_common_share(q) && !is_spac(q) && q.market_cap > 0)
        .collect();
    eligible.sort_by(|a, b| {
        b.market_cap
            .cmp(&a.market_cap)
            .then_with(|| a.code.cmp(&b.code))
    });
    eligible.dedup_by(|a, b| a.code == b.code);
    eligible.truncate(top);
    eligible
}

#[cfg(test)]
mod tests {
    use super::*;

    fn quote(code: &str, name: &str, market: &str, market_cap: u64) -> KrxDailyQuote {
        KrxDailyQuote {
            date: NaiveDate::from_ymd_opt(2023, 12, 28).expect("valid date"),
            code: code.to_owned(),
            isin: format!("KR7{code}003"),
            name: name.to_owned(),
            market_category: market.to_owned(),
            open: 100.0,
            high: 100.0,
            low: 100.0,
            close: 100.0,
            volume: 1,
            trading_value: 100,
            listed_shares: 1,
            market_cap,
            change: Some(0.0),
        }
    }

    #[test]
    fn keeps_largest_common_shares_only() {
        let snapshot = vec![
            quote("000020", "small", "KOSPI", 10),
            quote("005930", "big", "KOSPI", 900),
            quote("005935", "preferred", "KOSPI", 800),
            quote("123450", "konex", "KONEX", 700),
            quote("456780", "하나스팩1호", "KOSDAQ", 600),
            quote("247540", "mid", "KOSDAQ", 500),
            quote("000030", "zero", "KOSPI", 0),
        ];
        let codes: Vec<&str> = select_top_by_market_cap(&snapshot, 2)
            .iter()
            .map(|q| q.code.as_str())
            .collect();
        assert_eq!(codes, vec!["005930", "247540"]);
        assert_eq!(select_top_by_market_cap(&snapshot, 10).len(), 3);
    }
}
