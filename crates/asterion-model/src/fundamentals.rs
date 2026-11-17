//! Point-in-time valuation and quality features from filed financial statements.

use std::collections::HashMap;

use asterion_data::dart_financials::FinancialReport;
use chrono::NaiveDate;

pub const FUNDAMENTAL_FEATURE_NAMES: [&str; NUM_FUNDAMENTAL_FEATURES] = [
    "fund_earnings_yield",
    "fund_book_to_market",
    "fund_sales_to_price",
    "fund_op_margin",
    "fund_roe",
    "fund_leverage",
    "fund_revenue_yoy",
    "fund_age",
    "fund_available",
];
pub const NUM_FUNDAMENTAL_FEATURES: usize = 9;

// Fiscal periods that ended longer ago than this are treated as missing (a company that
// stopped filing). An annual report can be the newest one for up to ~15 months.
const MAX_AGE_DAYS: i64 = 500;

#[derive(Debug, Clone, Copy, PartialEq)]
struct Snapshot {
    filed: NaiveDate,
    period_end: NaiveDate,
    revenue: Option<f64>,
    operating_income: Option<f64>,
    net_income: Option<f64>,
    assets: Option<f64>,
    liabilities: Option<f64>,
    equity: Option<f64>,
    revenue_yoy: Option<f64>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct FundamentalIndex {
    by_code: HashMap<String, Vec<Snapshot>>,
    len: usize,
}

fn ratio(num: Option<f64>, den: Option<f64>) -> Option<f64> {
    match (num, den) {
        (Some(n), Some(d)) if d.abs() > 0.0 => Some(n / d).filter(|v| v.is_finite()),
        _ => None,
    }
}

impl FundamentalIndex {
    pub fn from_reports(reports: impl IntoIterator<Item = FinancialReport>) -> Self {
        let mut by_code: HashMap<String, Vec<Snapshot>> = HashMap::new();
        let mut len = 0;
        for r in reports {
            let Some(period) = r.period else {
                continue;
            };
            // Year-to-date flows scaled to a full year.
            let annualise = 12.0 / f64::from(period.report.months());
            by_code
                .entry(r.stock_code.clone())
                .or_default()
                .push(Snapshot {
                    filed: r.filed,
                    period_end: period.end(),
                    revenue: r.revenue.map(|v| v * annualise),
                    operating_income: r.operating_income.map(|v| v * annualise),
                    net_income: r.net_income.map(|v| v * annualise),
                    assets: r.total_assets,
                    liabilities: r.total_liabilities,
                    equity: r.total_equity,
                    revenue_yoy: ratio(r.revenue, r.prior_revenue)
                        .filter(|_| r.prior_revenue.is_some_and(|p| p > 0.0))
                        .map(|x| x - 1.0),
                });
            len += 1;
        }
        for list in by_code.values_mut() {
            list.sort_by_key(|s| s.filed);
        }
        FundamentalIndex { by_code, len }
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    // Features at the close of `date` given that day's market cap. Only reports received
    // strictly before `date` are used, as the receipt time of day is unknown.
    pub fn features(
        &self,
        code: &str,
        date: NaiveDate,
        market_cap: f64,
    ) -> [f64; NUM_FUNDAMENTAL_FEATURES] {
        let mut out = [0.0; NUM_FUNDAMENTAL_FEATURES];
        let Some(list) = self.by_code.get(code) else {
            return out;
        };
        // Among reports already received, the one for the latest fiscal period: an
        // amendment of an old period, refiled years later, must not displace newer data.
        let n = list.partition_point(|s| s.filed < date);
        let Some(s) = list[..n]
            .iter()
            .max_by_key(|s| (s.period_end, s.filed))
            .copied()
        else {
            return out;
        };
        let age = (date - s.period_end).num_days();
        if age > MAX_AGE_DAYS || market_cap <= 0.0 {
            return out;
        }
        let cap = Some(market_cap);
        let positive_equity = s.equity.filter(|e| *e > 0.0);
        out[0] = ratio(s.net_income, cap).map_or(0.0, |v| v.clamp(-1.0, 1.0));
        out[1] = ratio(s.equity, cap).map_or(0.0, |v| v.clamp(-2.0, 10.0));
        out[2] = ratio(s.revenue, cap).map_or(0.0, |v| v.max(0.0).ln_1p());
        out[3] = ratio(s.operating_income, s.revenue.filter(|r| *r > 0.0))
            .map_or(0.0, |v| v.clamp(-2.0, 1.0));
        out[4] = ratio(s.net_income, positive_equity).map_or(0.0, |v| v.clamp(-2.0, 2.0));
        out[5] =
            ratio(s.liabilities, s.assets.filter(|a| *a > 0.0)).map_or(0.0, |v| v.clamp(0.0, 2.0));
        out[6] = s.revenue_yoy.map_or(0.0, |v| v.clamp(-1.0, 3.0));
        out[7] = age as f64 / 365.0;
        out[8] = 1.0;
        out
    }
}

#[cfg(test)]
mod tests {
    use asterion_data::dart_financials::{Period, ReportCode};

    use super::*;

    fn d(m: u32, day: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(2024, m, day).expect("date")
    }

    #[test]
    fn only_reports_filed_before_the_date_are_visible() {
        let report = FinancialReport {
            stock_code: "005930".to_owned(),
            filed: d(5, 14),
            period: Some(Period {
                year: 2024,
                report: ReportCode::Q1,
            }),
            consolidated: true,
            revenue: Some(100.0),
            operating_income: Some(10.0),
            net_income: Some(5.0),
            total_assets: Some(1000.0),
            total_liabilities: Some(400.0),
            total_equity: Some(600.0),
            prior_revenue: Some(80.0),
            prior_net_income: Some(4.0),
        };
        let index = FundamentalIndex::from_reports([report]);
        assert_eq!(index.features("005930", d(5, 14), 2000.0), [0.0; 9]);
        let f = index.features("005930", d(5, 15), 2000.0);
        // Q1 net income 5 annualised to 20 over a cap of 2000.
        assert!((f[0] - 0.01).abs() < 1e-12);
        assert!((f[1] - 0.3).abs() < 1e-12);
        assert!((f[3] - 0.1).abs() < 1e-12);
        assert!((f[5] - 0.4).abs() < 1e-12);
        assert!((f[6] - 0.25).abs() < 1e-12);
        assert_eq!(f[8], 1.0);
    }

    #[test]
    fn a_late_amendment_of_an_old_period_does_not_displace_newer_reports() {
        let report = |year: i32, filed: NaiveDate, net: f64| FinancialReport {
            stock_code: "000001".to_owned(),
            filed,
            period: Some(Period {
                year,
                report: ReportCode::Annual,
            }),
            consolidated: true,
            net_income: Some(net),
            total_equity: Some(1000.0),
            ..FinancialReport::default()
        };
        let index = FundamentalIndex::from_reports([
            report(2023, d(3, 20), 50.0),
            report(2016, d(4, 1), 999.0),
        ]);
        let f = index.features("000001", d(4, 10), 1000.0);
        assert!((f[0] - 0.05).abs() < 1e-12, "uses 2023, got {}", f[0]);
    }
}
