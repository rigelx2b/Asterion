//! Korean display labels for forecast explanations.

// Template for the forecast headline; placeholders are replaced at runtime.
pub const REASON_HEADLINE: &str = "{h}거래일 후 예상 주가는 {price}원({ret}% 변동)이며, 평균 오차는 약 ±{err}%, 예상 정확도 점수는 1점 만점에 {score}점입니다.";
// Template for one forecast driver; placeholders are replaced at runtime.
pub const REASON_DRIVER: &str = "{group} 부문의 {feature} 지표가 평소 수준 대비 {z} 표준편차 벗어나 예측치를 {contrib}%p 변동시켰습니다.";

// Clause for the market-wide adjustment; the placeholder is replaced at runtime.
pub const REASON_MARKET: &str =
    "오늘 전체 종목의 평균 예측을 반영한 시장 조정으로 본 예측치가 {mkt}%p 변동되었습니다.";

// Confidence and profit-signal clauses; placeholders are replaced at runtime.
pub const REASON_INTERVAL: &str = "과거 검증 데이터 기준 실현 주가가 {level}% 확률로 {low}~{high}원 사이에 형성되며, 예측가 대비 1% 이내에 안착할 확률은 {hit}%입니다.";
pub const REASON_ISSUED: &str =
    "과거 검증 기준 1% 이내 적중률이 80% 이상을 기록하여 신뢰도 기준을 통과했습니다.";
pub const REASON_ABSTAIN: &str =
    "1% 정확도 목표 대비 예상 오차가 커 단일 예측 가격 대신 예측 범위만 제공합니다.";
pub const REASON_PROFIT: &str = "왕복 거래 비용 {cost}%를 반영한 후 플러스 수익을 기록할 추정 확률은 {prob}%로, 수익 신호 기준을 충족합니다.";
pub const REASON_NO_PROFIT: &str = "왕복 거래 비용 {cost}%를 반영한 후 플러스 수익을 기록할 추정 확률이 {prob}%에 머물러 수익 신호에 해당하지 않습니다.";
pub const REASON_ILLIQUID: &str =
    "당일 거래대금이 부족하거나 거래가 정지되어 신뢰도 및 수익 신호 제공 대상에서 제외되었습니다.";

pub fn feature_label(name: &str) -> &str {
    match name {
        "log_ret_1" => "1일 로그 수익률",
        "log_ret_2" => "2일 로그 수익률",
        "log_ret_3" => "3일 로그 수익률",
        "log_ret_5" => "5일 로그 수익률",
        "log_ret_10" => "10일 로그 수익률",
        "log_ret_20" => "20일 로그 수익률",
        "volatility_5" => "5일 일간 수익률 변동성",
        "volatility_20" => "20일 일간 수익률 변동성",
        "sma_5_ratio" => "5일 이동평균 대비 종가 괴리율",
        "sma_20_ratio" => "20일 이동평균 대비 종가 괴리율",
        "sma_60_ratio" => "60일 이동평균 대비 종가 괴리율",
        "intraday_range" => "종가 대비 당일 고저가 변동폭",
        "close_to_open" => "당일 시가 대비 종가 변동률",
        "volume_ratio_20" => "20일 평균 대비 당일 거래량",
        "volume_momentum" => "20일 대비 5일 평균 거래량",
        "mkt_ret_1" => "소속 시장 지수 1일 수익률",
        "mkt_ret_5" => "소속 시장 지수 5일 수익률",
        "mkt_ret_20" => "소속 시장 지수 20일 수익률",
        "mkt_vol_20" => "소속 지수 20일 변동성",
        "rel_ret_5" => "시장 대비 5일 수익률",
        "rel_ret_20" => "시장 대비 20일 수익률",
        "sp500_ret_1" => "S&P 500 전일 수익률",
        "sp500_ret_5" => "S&P 500 5일 수익률",
        "nasdaq_ret_1" => "나스닥 전일 수익률",
        "vix_log" => "VIX 지수",
        "vix_chg_5" => "VIX 5일 변동률",
        "us10y_chg_5" => "미국 10년물 국채금리 5일 변동폭",
        "usdkrw_ret_1" => "원/달러 환율 1일 변동률",
        "usdkrw_ret_5" => "원/달러 환율 5일 변동률",
        "dart_new" => "전일 이후 신규 DART 공시",
        "dart_cnt_30" => "최근 30일 DART 공시",
        "dart_periodic_7" => "최근 7일 정기보고서 공시",
        "dart_major_7" => "최근 7일 주요사항보고서 공시",
        "dart_ownership_7" => "최근 7일 지분 및 임원 공시",
        "dart_exchange_7" => "최근 7일 거래소 공시",
        "dart_earnings_7" => "최근 7일 잠정 실적 공시",
        "dart_contract_7" => "최근 7일 공급계약 공시",
        "dart_capital_7" => "최근 7일 자본조달 공시",
        "dart_buyback_7" => "최근 7일 자사주 매입 공시",
        "log_ret_60" => "60일 로그 수익률",
        "high_52w_ratio" => "52주 최고가 대비 이격도",
        "low_52w_ratio" => "52주 최저가 대비 이격도",
        "max_ret_20" => "최근 20일 최대 일일 상승률",
        "log_mcap" => "로그 시가총액",
        "turnover_20" => "20일 평균 거래대금 회전율",
        "amihud_20" => "비유동성 지표",
        "wti_ret_5" => "WTI 원유 5일 수익률",
        "dollar_ret_5" => "광의 달러 지수 5일 수익률",
        "usdjpy_ret_5" => "USD/JPY 5일 변동률",
        "usdcny_ret_5" => "USD/CNY 5일 변동률",
        "us2y_chg_5" => "미국 2년물 국채금리 5일 변동",
        "term_spread" => "미국 국채 10년-2년 금리차",
        "baa_spread" => "미국 Baa 회사채 스프레드",
        "baa_spread_chg_5" => "미국 회사채 스프레드 5일 변동",
        "vxn_log" => "나스닥 변동성 지수",
        "breakeven_chg_5" => "미국 10년 기대인플레이션 5일 변동",
        "fund_earnings_yield" => "이익수익률 (PER 역수)",
        "fund_book_to_market" => "장부시가비율 (PBR 역수)",
        "fund_sales_to_price" => "주가 대비 매출액",
        "fund_op_margin" => "영업이익률",
        "fund_roe" => "자기자본이익률(ROE)",
        "fund_leverage" => "총자산 대비 부채 비율",
        "fund_revenue_yoy" => "전년 대비 매출액 증가율",
        "fund_age" => "최근 결산 후 경과 기간",
        "fund_available" => "재무제표 제공 여부",
        other => other,
    }
}

pub fn group_label(group: &str) -> &str {
    match group {
        "momentum" => "주가 모멘텀 및 추세",
        "volatility" => "변동성 및 장중 변동",
        "liquidity_size" => "거래량, 유동성 및 기업 규모",
        "korea_market" => "국내 증시 지수 동향",
        "global_macro" => "미국 시장, 금리, 환율, 원자재",
        "disclosures" => "DART 기업 공시",
        "fundamentals" => "재무제표 펀더멘털",
        other => other,
    }
}
