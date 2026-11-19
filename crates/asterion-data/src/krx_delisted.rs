//! KRX KIND delisting lists (상장폐지현황 Excel export: EUC-KR HTML tables) and a
//! classifier mapping each delisting reason to how a holder exits.

use std::fs;
use std::path::Path;

use chrono::NaiveDate;

use crate::error::DataError;

/// How a holder at the delisting leaves the position.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DelistingKind {
    /// Distress delisting: the residual claim is valued at zero.
    Distress,
    /// Merger, voluntary delisting, SPAC or fund expiry: exit at the last traded price.
    LastPrice,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DelistedListing {
    pub code: String,
    pub name: String,
    pub date: NaiveDate,
    pub reason: String,
}

// Ordered (needle, kind) rules over the reason with whitespace removed; first match
// wins. SPAC rules precede the generic "미제출" (missing filing) distress rule.
const RULES: &[(&str, DelistingKind)] = &[
    ("상장예비심사청구서미제출", DelistingKind::LastPrice),
    ("해산사유", DelistingKind::LastPrice),
    ("존속기간만료", DelistingKind::LastPrice),
    ("피흡수합병", DelistingKind::LastPrice),
    ("완전자회사", DelistingKind::LastPrice),
    ("신청에의한상장폐지", DelistingKind::LastPrice),
    ("상장폐지신청", DelistingKind::LastPrice),
    ("감사의견", DelistingKind::Distress),
    ("의견거절", DelistingKind::Distress),
    ("한정의견", DelistingKind::Distress),
    ("자본잠식", DelistingKind::Distress),
    ("자본전액잠식", DelistingKind::Distress),
    ("최종부도", DelistingKind::Distress),
    ("계속성", DelistingKind::Distress),
    ("시가총액미달", DelistingKind::Distress),
    ("매출액미달", DelistingKind::Distress),
    ("영업손실", DelistingKind::Distress),
    ("법정기한내미제출", DelistingKind::Distress),
    ("주식분산미달", DelistingKind::Distress),
];

/// Whether the row records a KOSDAQ name moving to the KOSPI board (still listed).
pub fn is_board_transfer(reason: &str) -> bool {
    let compact: String = reason.chars().filter(|c| !c.is_whitespace()).collect();
    compact == "유가증권시장상장"
}

/// Kind of one reason, or `None` when no rule matches (callers must fail on it).
pub fn classify(reason: &str) -> Option<DelistingKind> {
    let compact: String = reason.chars().filter(|c| !c.is_whitespace()).collect();
    RULES
        .iter()
        .find(|(needle, _)| compact.contains(needle))
        .map(|&(_, kind)| kind)
}

fn strip_tags(cell: &str) -> String {
    let mut out = String::with_capacity(cell.len());
    let mut in_tag = false;
    for c in cell.chars() {
        match c {
            '<' => in_tag = true,
            '>' => in_tag = false,
            _ if !in_tag => out.push(c),
            _ => {}
        }
    }
    out.replace("&nbsp;", " ").trim().to_owned()
}

fn cells(row: &str) -> Vec<String> {
    let lower = row.to_ascii_lowercase();
    let mut out = Vec::new();
    let mut pos = 0;
    while let Some(open) = lower[pos..].find("<td").map(|i| pos + i) {
        let Some(body) = lower[open..].find('>').map(|i| open + i + 1) else {
            break;
        };
        let end = lower[body..].find("</td").map_or(row.len(), |i| body + i);
        out.push(strip_tags(&row[body..end]));
        pos = end;
    }
    out
}

/// Rows of one decoded export; the header row has `<th>` cells and is skipped.
/// Columns: 번호, 회사명, 종목코드, 폐지일자, 폐지사유, 비고.
pub fn parse_html(html: &str) -> Result<Vec<DelistedListing>, DataError> {
    let lower = html.to_ascii_lowercase();
    let mut out = Vec::new();
    let mut pos = 0;
    while let Some(start) = lower[pos..].find("<tr").map(|i| pos + i) {
        let end = lower[start..]
            .find("</tr")
            .map_or(html.len(), |i| start + i);
        let row = cells(&html[start..end]);
        pos = end;
        if row.is_empty() {
            continue;
        }
        if row.len() < 5 {
            return Err(DataError::InvalidField {
                field: "krx_delisted_row",
                value: row.join("|"),
            });
        }
        let date = NaiveDate::parse_from_str(&row[3], "%Y-%m-%d").map_err(|_| {
            DataError::InvalidField {
                field: "krx_delisted_date",
                value: row[3].clone(),
            }
        })?;
        out.push(DelistedListing {
            code: row[2].clone(),
            name: row[1].clone(),
            date,
            reason: row[4].clone(),
        });
    }
    Ok(out)
}

/// Reads one KIND export file (EUC-KR, or UTF-8 when it decodes cleanly).
pub fn load(path: &Path) -> Result<Vec<DelistedListing>, DataError> {
    let bytes = fs::read(path).map_err(|e| DataError::io(path, e))?;
    let html = match std::str::from_utf8(&bytes) {
        Ok(text) => text.to_owned(),
        Err(_) => {
            let (text, _, had_errors) = encoding_rs::EUC_KR.decode(&bytes);
            if had_errors {
                return Err(DataError::InvalidField {
                    field: "krx_delisted_encoding",
                    value: path.display().to_string(),
                });
            }
            text.into_owned()
        }
    };
    parse_html(&html)
}

#[cfg(test)]
mod tests {
    use super::*;

    // Every distinct reason among the 2015-2019 delisted names in the KIND exports.
    const LAST_PRICE: &[&str] = &[
        "상장예비심사 청구서 미제출로 관리종목 지정 후 1개월 이내 동 사유 미해소",
        "해산 사유 발생",
        "신청에 의한 상장폐지",
        "피흡수합병",
        "지주회사(최대주주등)의 완전자회사화 등",
        "존속기간 만료",
        "타법인의 완전자회사로 편입",
        "상장폐지 신청('17.06.22)",
        "상장폐지신청('15.6.11)",
        "상장폐지신청('17.02.07)",
        "상장폐지 신청('15.01.15)",
    ];
    const DISTRESS: &[&str] = &[
        "감사의견 의견거절",
        "자본전액잠식",
        "기업의 계속성 및 경영의 투명성 등을 종합적으로 고려하여 상장폐지기준에 해당한다고 결정",
        "감사의견거절(감사범위 제한)",
        "발행한 어음 또는 수표가 주거래은행에 의하여 최종부도로 결정되거나 거래은행에 의한 거래정지",
        "감사의견거절(감사범위 제한 및 계속기업 존속능력에 대한 불확실성)",
        "기업의 계속성, 경영의 투명성 또는 그 밖에 코스닥시장의 건전성 등을 종합적으로 고려하여 상장폐지 기준에 해당한다고 결정",
        "기업의 계속성, 경영의 투명성 및 기타 공익과 투자자 보호 등을 종합적으로 고려하여 상장폐지 기준에 해당한다고 결정",
        "감사범위제한으로 인한 한정의견",
        "자본전액잠식 등",
        "'14사업연도 자본잠식률 50%이상 사유로 관리종목 지정 후 '15사업연도 반기 감사의견 거절",
        "시가총액 미달",
        "매출액 미달 2년 계속",
        "감사의견 부적정",
        "제28조제1항제9호에 따라 관리종목 지정 후 공시규정 제19조 제1항에 따른 분기보고서 법정기한 내 미제출",
        "감사의견거절(범위제한 및 계속기업 존속능력에 대한 불확실성)",
        "'16사업연도 감사의견거절(계속기업 존속능력에 대한 불확실성) 및 '17사업연도 반기 감사의견거절(계속기업 존속능력에 대한 불확실성)",
        "감사의견 거절(내부회계관리제도상의 취약점, 계속기업가정의 불확실성 및 재고자산 관련 감사범위 제한)",
        "최근 5사업연도 연속 영업손실 발생",
        "감사의견 거절(감사범위 제한 및 기업회계기준 위배)",
        "2반기 연속 자본잠식률 50% 이상",
        "감사의견 거절(계속기업 존속능력에 대한 불확실성 및 감사범위 제한)",
        "감사의견 거절(감사범위 제한 및 계속기업 존속능력에 대한 불확실성)",
        "감사의견 거절(감사범위 제한)",
        "감사의견거절(감사범위제한)",
    ];

    #[test]
    fn every_observed_reason_is_classified() {
        assert_eq!(LAST_PRICE.len() + DISTRESS.len(), 36);
        for r in LAST_PRICE {
            assert_eq!(classify(r), Some(DelistingKind::LastPrice), "{r}");
        }
        for r in DISTRESS {
            assert_eq!(classify(r), Some(DelistingKind::Distress), "{r}");
        }
        assert_eq!(classify("상장폐지 사유 미상"), None);
        assert!(is_board_transfer("유가증권시장 상장"));
        assert!(!is_board_transfer("피흡수합병"));
    }

    #[test]
    fn export_rows_are_parsed_and_header_skipped() {
        let html = "<table><tr><th>번호</th><th>회사명</th><th>종목코드</th><th>폐지일자</th><th>폐지사유</th><th>비고</th></tr>\
            <tr><td>170</td><td>사조해표</td><td style='x'>079660</td><td>2019-06-26</td><td>해산 사유 발생</td><td></td></tr></table>";
        let rows = parse_html(html).expect("parse");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].code, "079660");
        assert_eq!(rows[0].name, "사조해표");
        assert_eq!(
            rows[0].date,
            NaiveDate::from_ymd_opt(2019, 6, 26).expect("date")
        );
        assert_eq!(rows[0].reason, "해산 사유 발생");
        assert!(parse_html("<tr><td>1</td><td>x</td></tr>").is_err());
    }
}
