//! Per-stock disclosure (DART filing) events turned into leakage-safe features.

use std::collections::HashMap;

use asterion_data::opendart::{Filing, FilingKind};
use chrono::{Days, NaiveDate};

pub const DISCLOSURE_FEATURE_NAMES: [&str; NUM_DISCLOSURE_FEATURES] = [
    "dart_new",
    "dart_cnt_30",
    "dart_periodic_7",
    "dart_major_7",
    "dart_ownership_7",
    "dart_exchange_7",
    "dart_earnings_7",
    "dart_contract_7",
    "dart_capital_7",
    "dart_buyback_7",
];
pub const NUM_DISCLOSURE_FEATURES: usize = 10;

/// Title-topic features registered in data/ledger/cv_2026-11-13/addendum_dart_topics.json.
/// Used only by the research panel; the base feature vector is unchanged.
pub const TOPIC_FEATURE_NAMES: [&str; NUM_TOPIC_FEATURES] = [
    "dart_rights_issue_30",
    "dart_cb_bw_30",
    "dart_conversion_30",
    "dart_risk_90",
    "dart_control_change_30",
    "dart_shareholder_return_30",
    "dart_inquiry_30",
    "dart_contract_cancel_90",
    "dart_correction_30",
    "dart_ir_30",
    "dart_lawsuit_90",
    "dart_reduction_halt_90",
];
pub const NUM_TOPIC_FEATURES: usize = 12;
const TOPIC_WINDOWS: [u64; NUM_TOPIC_FEATURES] = [30, 30, 30, 90, 30, 30, 30, 90, 30, 30, 90, 90];

// Bit i set when the title matches topic i of TOPIC_FEATURE_NAMES.
fn extra_topics(title: &str) -> u16 {
    let t: String = title.chars().filter(|c| !c.is_whitespace()).collect();
    let any = |needles: &[&str]| needles.iter().any(|n| t.contains(n));
    let rules = [
        any(&["유상증자결정"]) && !t.contains("무상"),
        any(&[
            "전환사채권발행결정",
            "신주인수권부사채권발행결정",
            "교환사채권발행결정",
        ]),
        any(&[
            "전환청구권행사",
            "신주인수권행사",
            "교환청구권행사",
            "전환가액의조정",
            "행사가액의조정",
        ]),
        any(&[
            "불성실공시법인지정",
            "횡령",
            "배임",
            "회생절차",
            "상장적격성",
            "관리종목",
            "거래처와의거래중단",
            "생산중단",
            "벌금등의부과",
            "감사의견",
        ]) && !t.contains("미지정"),
        any(&["최대주주변경", "경영권변경"]),
        any(&["현금ㆍ현물배당결정", "주식소각결정", "기업가치제고계획"]),
        any(&["조회공시요구", "풍문또는보도에대한해명"]),
        any(&["공급계약해지"]),
        any(&["기재정정"]),
        any(&["기업설명회개최"]),
        any(&["소송등의제기", "소송등의판결"]),
        any(&["감자결정", "주권매매거래정지"]) && !t.contains("해제") && !t.contains("기간변경"),
    ];
    rules.iter().enumerate().fold(
        0u16,
        |bits, (i, hit)| if *hit { bits | (1 << i) } else { bits },
    )
}

const SHORT_WINDOW_DAYS: u64 = 7;
const LONG_WINDOW_DAYS: u64 = 30;

const EARNINGS: u8 = 1;
const CONTRACT: u8 = 1 << 1;
const CAPITAL: u8 = 1 << 2;
const BUYBACK: u8 = 1 << 3;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Event {
    date: NaiveDate,
    kind: FilingKind,
    topics: u8,
    extra: u16,
}

// Topic bits from the report title; DART titles are matched with spaces removed.
fn topics(title: &str) -> u8 {
    let title: String = title.chars().filter(|c| !c.is_whitespace()).collect();
    let has = |needle: &str| title.contains(needle);
    let mut bits = 0;
    if has("잠정)실적") || has("매출액또는손익구조") {
        bits |= EARNINGS;
    }
    if has("공급계약") {
        bits |= CONTRACT;
    }
    if has("유상증자") || has("전환사채") || has("신주인수권부사채") || has("교환사채")
    {
        bits |= CAPITAL;
    }
    if has("자기주식") && has("취득") {
        bits |= BUYBACK;
    }
    bits
}

// Filing events per stock code, each list sorted by receipt date.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct DisclosureIndex {
    by_code: HashMap<String, Vec<Event>>,
    len: usize,
}

impl DisclosureIndex {
    pub fn from_filings(filings: impl IntoIterator<Item = Filing>) -> Self {
        let mut by_code: HashMap<String, Vec<Event>> = HashMap::new();
        let mut len = 0;
        for filing in filings {
            by_code.entry(filing.stock_code).or_default().push(Event {
                date: filing.date,
                kind: filing.kind,
                topics: topics(&filing.title),
                extra: extra_topics(&filing.title),
            });
            len += 1;
        }
        for events in by_code.values_mut() {
            events.sort_by_key(|e| e.date);
        }
        DisclosureIndex { by_code, len }
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    // Features for the close of `date`. Only filings received strictly before `date`
    // count: the receipt time is unknown, so a same-day filing may postdate the close.
    // `prev_bar` is the stock's previous trading date.
    pub fn features(
        &self,
        code: &str,
        prev_bar: NaiveDate,
        date: NaiveDate,
    ) -> [f64; NUM_DISCLOSURE_FEATURES] {
        let mut out = [0.0; NUM_DISCLOSURE_FEATURES];
        let Some(events) = self.by_code.get(code) else {
            return out;
        };
        let back = |days: u64| date.checked_sub_days(Days::new(days)).unwrap_or(date);
        let (short_start, long_start) = (back(SHORT_WINDOW_DAYS), back(LONG_WINDOW_DAYS));
        let lo = events.partition_point(|e| e.date < long_start);
        let hi = events.partition_point(|e| e.date < date);

        let mut counts = [0u32; 6];
        for event in &events[lo..hi] {
            counts[1] += 1;
            if event.date >= prev_bar {
                counts[0] += 1;
            }
            if event.date < short_start {
                continue;
            }
            let slot = match event.kind {
                FilingKind::Periodic => 2,
                FilingKind::Major => 3,
                FilingKind::Ownership => 4,
                FilingKind::Exchange => 5,
            };
            counts[slot] += 1;
            for (i, topic) in [EARNINGS, CONTRACT, CAPITAL, BUYBACK]
                .into_iter()
                .enumerate()
            {
                if event.topics & topic != 0 {
                    out[6 + i] = 1.0;
                }
            }
        }
        for (slot, count) in counts.into_iter().enumerate() {
            out[slot] = f64::from(count).ln_1p();
        }
        out
    }

    /// ln(1 + count) of each title topic in its window, from filings received strictly
    /// before `date`.
    pub fn topic_features(&self, code: &str, date: NaiveDate) -> [f64; NUM_TOPIC_FEATURES] {
        let mut out = [0.0; NUM_TOPIC_FEATURES];
        let Some(events) = self.by_code.get(code) else {
            return out;
        };
        let hi = events.partition_point(|e| e.date < date);
        for (i, &days) in TOPIC_WINDOWS.iter().enumerate() {
            let start = date.checked_sub_days(Days::new(days)).unwrap_or(date);
            let lo = events.partition_point(|e| e.date < start);
            let n = events[lo..hi.max(lo)]
                .iter()
                .filter(|e| e.extra & (1 << i) != 0)
                .count();
            out[i] = (n as f64).ln_1p();
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn d(day: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(2024, 3, day).expect("valid date")
    }

    fn filing(code: &str, day: u32, kind: FilingKind, title: &str) -> Filing {
        Filing {
            receipt_no: format!("{code}{day}{title}"),
            stock_code: code.to_owned(),
            date: d(day),
            kind,
            title: title.to_owned(),
        }
    }

    fn feature(values: &[f64], name: &str) -> f64 {
        let idx = DISCLOSURE_FEATURE_NAMES
            .iter()
            .position(|n| *n == name)
            .expect("known feature");
        values[idx]
    }

    #[test]
    fn extra_topics_follow_the_addendum() {
        let bit = |name: &str| {
            1u16 << TOPIC_FEATURE_NAMES
                .iter()
                .position(|n| *n == name)
                .expect("known topic")
        };
        assert_eq!(
            extra_topics("주요사항보고서(유상증자결정)"),
            bit("dart_rights_issue_30")
        );
        assert_eq!(extra_topics("주요사항보고서(무상증자결정)"), 0);
        assert_eq!(
            extra_topics("주요사항보고서(전환사채권발행결정)"),
            bit("dart_cb_bw_30")
        );
        assert_eq!(extra_topics("불성실공시법인미지정"), 0);
        assert_eq!(extra_topics("횡령ㆍ배임혐의발생"), bit("dart_risk_90"));
        assert_eq!(extra_topics("주권매매거래정지해제"), 0);
        assert_eq!(
            extra_topics("[기재정정]단일판매ㆍ공급계약체결"),
            bit("dart_correction_30")
        );
    }

    #[test]
    fn topic_features_ignore_same_day_and_old_filings() {
        let index = DisclosureIndex::from_filings([
            filing(
                "000001",
                1,
                FilingKind::Major,
                "주요사항보고서(유상증자결정)",
            ),
            filing(
                "000001",
                20,
                FilingKind::Major,
                "주요사항보고서(유상증자결정)",
            ),
        ]);
        let f = index.topic_features("000001", d(20));
        assert!(
            (f[0] - 2f64.ln()).abs() < 1e-12,
            "only the day-1 filing counts"
        );
    }

    #[test]
    fn titles_map_to_topics() {
        assert_eq!(topics("연결재무제표기준영업(잠정)실적(공정공시)"), EARNINGS);
        assert_eq!(topics("[기재정정]단일판매ㆍ공급계약체결"), CONTRACT);
        assert_eq!(topics("주요사항보고서(유상증자결정)"), CAPITAL);
        assert_eq!(topics("주요사항보고서(자기주식 취득 결정)"), BUYBACK);
        assert_eq!(topics("자기주식처분결과보고서"), 0);
        assert_eq!(topics("분기보고서 (2024.03)"), 0);
    }

    #[test]
    fn same_day_filings_are_invisible_and_windows_are_respected() {
        let index = DisclosureIndex::from_filings([
            filing("005930", 1, FilingKind::Periodic, "사업보고서 (2023.12)"),
            filing("005930", 18, FilingKind::Exchange, "단일판매ㆍ공급계약체결"),
            filing(
                "005930",
                19,
                FilingKind::Major,
                "주요사항보고서(유상증자결정)",
            ),
            filing(
                "005930",
                20,
                FilingKind::Exchange,
                "영업(잠정)실적(공정공시)",
            ),
            filing(
                "000660",
                19,
                FilingKind::Ownership,
                "임원ㆍ주요주주특정증권등소유상황보고서",
            ),
        ]);
        assert_eq!(index.len(), 5);

        // Close of the 20th, previous bar on the 19th: the 20th filing is not visible yet.
        let f = index.features("005930", d(19), d(20));
        assert_eq!(feature(&f, "dart_new"), 1f64.ln_1p());
        assert_eq!(feature(&f, "dart_cnt_30"), 3f64.ln_1p());
        assert_eq!(
            feature(&f, "dart_periodic_7"),
            0.0,
            "outside the 7-day window"
        );
        assert_eq!(feature(&f, "dart_major_7"), 1f64.ln_1p());
        assert_eq!(feature(&f, "dart_exchange_7"), 1f64.ln_1p());
        assert_eq!(feature(&f, "dart_ownership_7"), 0.0);
        assert_eq!(feature(&f, "dart_contract_7"), 1.0);
        assert_eq!(feature(&f, "dart_capital_7"), 1.0);
        assert_eq!(feature(&f, "dart_earnings_7"), 0.0);

        let next = index.features("005930", d(20), d(21));
        assert_eq!(feature(&next, "dart_earnings_7"), 1.0);
        assert_eq!(feature(&next, "dart_new"), 1f64.ln_1p());

        assert_eq!(index.features("999999", d(19), d(20)), [0.0; 10]);
    }
}
