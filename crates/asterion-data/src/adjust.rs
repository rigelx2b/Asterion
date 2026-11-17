use asterion_core::Bar;

use crate::data_go_kr::KrxDailyQuote;

// Factors outside this band are treated as bad data and ignored.
const MIN_FACTOR: f64 = 0.01;
const MAX_FACTOR: f64 = 100.0;

// Ratio of the exchange base price to the previous close: 1.0 on ordinary days,
// 0.2 on a 5-for-1 split ex-date, and so on for bonus issues, rights and spin-offs.
pub fn base_price_factor(prev: &KrxDailyQuote, curr: &KrxDailyQuote) -> f64 {
    let Some(change) = curr.change else {
        return 1.0;
    };
    let base = curr.close - change;
    if prev.close <= 0.0 || base <= 0.0 || base == prev.close {
        return 1.0;
    }
    let factor = base / prev.close;
    if factor.is_finite() && (MIN_FACTOR..=MAX_FACTOR).contains(&factor) {
        factor
    } else {
        1.0
    }
}

// Back-adjusts OHLCV onto the latest share basis so returns are continuous across
// corporate actions. `quotes` must be sorted by date; repeated dates are dropped.
pub fn adjusted_bars(quotes: &[KrxDailyQuote]) -> Vec<Bar> {
    let mut unique: Vec<&KrxDailyQuote> = Vec::with_capacity(quotes.len());
    for q in quotes {
        if unique.last().is_none_or(|last| last.date != q.date) {
            unique.push(q);
        }
    }

    let mut bars = Vec::with_capacity(unique.len());
    let mut cumulative = 1.0;
    for i in (0..unique.len()).rev() {
        let q = unique[i];
        let close = q.close * cumulative;
        bars.push(Bar {
            date: q.date,
            open: q.open * cumulative,
            high: q.high * cumulative,
            low: q.low * cumulative,
            close,
            adj_close: Some(close),
            volume: (q.volume as f64 / cumulative).round() as u64,
        });
        if i > 0 {
            cumulative *= base_price_factor(unique[i - 1], q);
        }
    }
    bars.reverse();
    bars
}

#[cfg(test)]
mod tests {
    use chrono::NaiveDate;

    use super::*;

    fn quote(day: u32, close: f64, change: Option<f64>, volume: u64) -> KrxDailyQuote {
        KrxDailyQuote {
            date: NaiveDate::from_ymd_opt(2024, 4, day).expect("valid date"),
            code: "086520".to_owned(),
            isin: "KR7086520004".to_owned(),
            name: "X".to_owned(),
            market_category: "KOSDAQ".to_owned(),
            open: close,
            high: close,
            low: close,
            close,
            volume,
            trading_value: 0,
            listed_shares: 0,
            market_cap: 0,
            change,
        }
    }

    #[test]
    fn five_for_one_split_becomes_a_continuous_series() {
        // Real shape of 086520 on 2024-04-25: prev 517000, close 108100, vs +4700.
        let quotes = vec![
            quote(23, 500_000.0, Some(0.0), 100),
            quote(24, 517_000.0, Some(17_000.0), 100),
            quote(25, 108_100.0, Some(4_700.0), 500),
            quote(26, 110_000.0, Some(1_900.0), 500),
        ];
        let bars = adjusted_bars(&quotes);
        assert_eq!(bars.len(), 4);
        assert!((bars[0].close - 100_000.0).abs() < 1e-6);
        assert!((bars[1].close - 103_400.0).abs() < 1e-6);
        assert_eq!(bars[2].close, 108_100.0);
        assert_eq!(bars[3].close, 110_000.0);
        assert_eq!(bars[1].volume, 500);
        assert_eq!(bars[2].volume, 500);
        let ex_date_return = bars[2].close / bars[1].close - 1.0;
        assert!((ex_date_return - 4_700.0 / 103_400.0).abs() < 1e-12);
        assert_eq!(bars[1].adj_close, Some(bars[1].close));
    }

    #[test]
    fn ordinary_days_and_missing_change_are_untouched() {
        let quotes = vec![
            quote(1, 100.0, None, 10),
            quote(2, 110.0, Some(10.0), 10),
            quote(3, 50.0, None, 10),
        ];
        let bars = adjusted_bars(&quotes);
        let closes: Vec<f64> = bars.iter().map(|b| b.close).collect();
        assert_eq!(closes, vec![100.0, 110.0, 50.0]);
    }

    #[test]
    fn duplicate_dates_and_absurd_factors_are_ignored() {
        let quotes = vec![
            quote(1, 100.0, Some(0.0), 10),
            quote(1, 100.0, Some(0.0), 10),
            quote(2, 100.0, Some(99.999), 10),
        ];
        let bars = adjusted_bars(&quotes);
        assert_eq!(bars.len(), 2);
        assert_eq!(bars[0].close, 100.0);
    }
}
