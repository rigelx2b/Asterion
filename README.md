# Asterion

## 1. 프로젝트 소개

Asterion은 한국 주식(KOSPI/KOSDAQ)의 미래 가격 및 수익률을 1, 3, 5, 10, 20 거래일(`h1`, `h3`, `h5`, `h10`, `h20`) 다중 지평(horizon)에서 예측하기 위한 Rust 기반 딥러닝 연구 시스템이다.

- **연구 목표**: 계획상 목표치는 엄격한 기준에서 `directional accuracy` 80%이며, **현재 시점까지 목표에 도달하지 못했다.**
- **성격**: 개인적·비상업적 연구 목적의 프로젝트이며, 투자 자문이나 매매 권유가 아니다.
- **기술 스택**: Rust stable 툴체인(`rustfmt`, `clippy`), Cargo workspace edition 2024 (`crates/*` 멤버 구성).

---

## 2. 현재 진행 상황 요약 (2026-11-17 기준)

- **목표 달성 여부**: 미달. 저장된 동결(frozen) 모델의 held-out test 기준 `directional accuracy`는 `h1` 51.3%~`h10` 57.4% 수준이며, `balanced accuracy`는 전 지평에서 약 50~51%로 클래스 불균형의 영향이 크다.
- **데이터 수집**:
  - **완료**: data.go.kr 일별 시세(2020-01-01..2026-11-13), 지수, FRED 거시 지표, OpenDART 공시, KIS 종목별 투자자 수급, KIS 해외 일봉.
  - **진행 중(재개 가능, 재실행 시 누락 구간만 수집)**:
    - KIS 과거 일봉(2010-01-01..2020-06-30, 2,812개 종목 대상): 약 `514 / 2,812` 종목 완료.
    - OpenDART 재무제표: 약 `62,390 / 79,800` 요청 완료(일일 20,000건 호출 한도로 인해 여러 날에 걸쳐 진행 중).
  - **구현 완료 / 백필 대기**: Reddit 수집기(`fetch-reddit`) 및 일별 집계(`reddit-daily`) 구현 완료. Arctic Shift API 속도 제한으로 전체 과거 이력은 향후 덤프 파일 적재 예정.
- **검증 체계**: `rank-fit`으로 4개 랭킹 모델 arm을 동결 및 paper hypothesis로 등록했으며, `rank-record` / `rank-score` 기반 write-once forward paper ledger로 표본 외(out-of-sample) 검증을 진행 중이다.

---

## 3. 저장소 구조

```text
.
├── crates/
│   ├── asterion-core/     # 도메인 타입 정의
│   ├── asterion-data/     # 데이터 수집기, 원시 캐시, 검증/조정/진행 상태
│   ├── asterion-model/    # 피처, 라벨, 모델 학습/평가, 전략, 원장(ledger)
│   └── asterion-cli/      # CLI 바이너리 (`asterion`)
├── config/
│   ├── universe/          # 종목 유니버스 정의 및 시점별 스냅샷
│   └── reddit/            # subreddits.txt
├── scripts/               # 수집·학습·일간 운영 스크립트
├── docs/                  # 설계/계획 문서 및 API 활용 계획
├── .env.example           # API 키 환경변수 템플릿
└── data/                  # 원시 데이터 및 상태 파일 (git-ignored)
```

### Crates 구성

- `asterion-core`: 도메인 타입.
- `asterion-data`: 수집기 및 원시 캐시.
  - `data_go_kr`: data.go.kr KRX 일별 시세 및 KOSPI/KOSDAQ/KOSPI200 지수.
  - `kis`: 한국투자증권 Open API (종목별 과거 일봉, 해외 일봉, 종목별 투자자 수급 — 개인/외국인/기관).
  - `opendart`, `dart_financials`: OpenDART 공시 및 재무제표.
  - `fred`: FRED 미국 시장 및 거시경제 시계열.
  - `reddit`: Arctic Shift 아카이브 기반 Reddit 게시물 수집 및 일별 집계.
  - `cache`: 종목별 매니페스트, SHA-256 출처(provenance) 기록, 중단 후 재개(resumable)를 지원하는 원시 응답 캐시.
  - `http`: 요청 속도 제한(throttled HTTP), 제한된 재시도(bounded retries), 로그 내 시크릿 마스킹(secrets redacted).
  - `validate`: OHLC 정합성, 결측 구간(gaps), 거래정지(halts), 이상 급등락(abnormal moves) 검증.
  - `adjust`: 가격/데이터 조정.
  - `progress`: 실행 상태 스냅샷 기록.
- `asterion-model`:
  - `features`, `labels`, `baselines`, 신경망 모델 학습, `calibration`, 신뢰도/선택적 예측(confidence/selective prediction), `explain`(피처 기여도), `fundamentals`, 공시 피처, `research`(purged walk-forward CV), `strategy`, `ledger` / `rank_ledger`(write-once forward-test/paper 원장), `metrics`.
- `asterion-cli`:
  - 바이너리 `asterion`. 주요 서브커맨드: `fetch`, `validate`, `universe`, `fetch-index`, `fetch-macro`, `fetch-disclosures`, `fetch-financials`, `fetch-investors`, `fetch-history`, `fetch-overseas`, `fetch-reddit`, `reddit-daily`, `train`, `predict`, `evaluate`, `confidence`, `research-cv`, `rank-fit`, `rank-record`, `rank-score`, `ledger-record`, `ledger-score`, `monitor`.
  - `monitor`는 `http://127.0.0.1:8787`에서 수집 및 학습 진행 상황을 확인하는 로컬 웹 대시보드를 제공한다.

### Config, Scripts, Docs

- `config/universe/`:
  - `kr-union.txt` (KRX 2,812개 종목 코드, union 유니버스)
  - `kr-top1000.txt`, `kr-all-20231228.txt`, `kr-large-caps.txt`
  - `us-core.txt` (`EXCHANGE:SYMBOL` 형식의 해외 ETF/주식 51개)
  - `snapshots/` (point-in-time 유니버스 스냅샷)
- `config/reddit/subreddits.txt`: 수집 대상 서브레딧 목록.
- `scripts/`:
  - `daily.sh`: 일간 시세 수집 + 투자자 수급 수집 + `rank-record`
  - `collect-all.sh`, `expand-universe.sh`, `kis-probe.sh`, `train-full.sh`
- `docs/`:
  - `superpowers/specs`, `plans`: Reddit 수집기 설계 및 구현 계획
  - `api-application`: data.go.kr API 활용 계획

---

## 4. 데이터 수집

### 데이터 소스 및 현황

| 소스 | 대상 데이터 | CLI 명령 | 상태 (2026-11-17) |
| :--- | :--- | :--- | :--- |
| data.go.kr | KRX 일별 시세 (2020-01-01..2026-11-13) | `fetch` | 완료 |
| data.go.kr | KOSPI / KOSDAQ / KOSPI200 지수 | `fetch-index` | 완료 |
| FRED | 미국 시장 및 거시경제 시계열 | `fetch-macro` | 완료 |
| OpenDART | 공시 목록/내역 | `fetch-disclosures` | 완료 |
| OpenDART | 재무제표 | `fetch-financials` | 진행 중 (~62,390 / 79,800 요청, 일일 20,000건 제한) |
| KIS Open API | 종목별 투자자 수급 (개인/외국인/기관) | `fetch-investors` | 완료 |
| KIS Open API | 해외 일봉 (`us-core.txt`) | `fetch-overseas` | 완료 |
| KIS Open API | 과거 일봉 (2010-01-01..2020-06-30, 2,812종목) | `fetch-history` | 진행 중 (~514 / 2,812 종목) |
| Arctic Shift (Reddit) | 서브레딧 게시물 및 일별 집계 | `fetch-reddit`, `reddit-daily` | 수집기 구현 완료, 과거 백필은 덤프 적재 예정 |

- 모든 `fetch*` 명령은 중단 후 재실행 시 누락 구간만 이어서 수집하며(resumable), 진행 상태를 `data/status`에 기록한다.
- 원시 데이터는 `data/raw/` 하위에 저장되며, Git 저장소에 커밋하지 않는다.

### Reddit 수집기 세부 사항

- 공식 Reddit API가 아닌 제3자 아카이브인 **Arctic Shift API**를 사용한다.
- 저장 필드: `id`, 생성 시각(created time), `title`, `selftext`, `score`, `num_comments`, `flair`, `over_18`, `retrieved_on`. 작성자/사용자(author/user) 관련 필드는 수집 시 제거하며 저장하지 않는다.
- 확정된 ISO 주차(final ISO weeks)는 매니페스트에 커밋되며, 최근 3일은 잠정(provisional) 구간으로 간주해 재수집한다.
- 각 확정 주차는 페이지 단위로 체크포인트되어, 수집이 중단되더라도 마지막으로 저장된 페이지부터 재개한다. 과부하 응답(HTTP 422)에 대해서는 추가 재시도 라운드를 수행한다.
- Arctic Shift API의 지속 가능 처리량은 분당 약 4~5회 요청 수준이므로, API만으로 전체 과거 이력을 수집하면 수 주가 소요된다. 따라서 저장 공간이 확보되는 대로 과거 이력은 Arctic Shift 덤프 파일로 적재할 계획이다.
- `reddit-daily`는 서브레딧별 UTC 기준 일일 게시물 수, 제목 기반 강세/약세(bullish/bearish) 감성, 한국(Korea) 언급 횟수를 집계한다.

---

## 5. 모델·평가 결과

모든 결과는 저장된 동결 모델의 held-out test 세트(지평별 약 1.1M 샘플) 및 walk-forward CV 기준이다.

### 방향성 예측 성능 (Held-out Test)

| Horizon | Directional Accuracy | Balanced Accuracy | IC |
| :--- | :--- | :--- | :--- |
| `h1` | 51.3% | 50.2% | 0.033 |
| `h3` | 54.4% | 50.4% | 0.033 |
| `h5` | 54.3% | 50.2% | 0.054 |
| `h10` | 57.4% | 51.4% | 0.073 |
| `h20` | 56.9% | 50.3% | 0.061 |

- 목표치인 `directional accuracy` 80%에 크게 미달한다.
- `balanced accuracy`가 전 지평에서 약 50~51%에 머물러 있어, 원시 정확도(raw accuracy) 대부분은 클래스 불균형에서 기인한 것으로 판단된다.

### 신뢰도 필터링 및 단순 전략 검증

- **Selective confidence**: 1% `coverage` 구간에서 `h1` `hit rate`는 80.3%로, naive 기준(80.8%) 대비 우위가 없다.
- **"Avoid the bottom decile" 전략** (거래비용 0.25% 반영): `h1` 연환산 초과수익 `+4.5%` (`t=2.10`)를 기록했으나, 저변동성 대조군(low-volatility control arm)도 유사한 수준(`+5.4%`, `t=1.93`)을 보였다.

### Purged Walk-Forward CV (랭킹 모델)

- 설정: 2025-01-01부터의 윈도우, top-decile long-only, 비용 차감 후 기준.
- `gbdt`: `IC` ~0.14–0.20 (`h5`–`h20`)
- `ridge`: `IC` ~0.13–0.18
- 대조군 대비 증분 초과수익(incremental excess)은 통계적으로 유의하지 않다(`t < 2.2`).
- 최종 판정: `composite6`, `ridge`, `gbdt` 모두 `passed=false`.

### Forward Paper Ledger

- `rank-fit`을 통해 1,646,883개 라벨 행으로 학습된 4개 랭킹 arm을 지문(fingerprinted)과 함께 동결하고 paper hypothesis로 등록했다.
- `rank-record`와 `rank-score`로 write-once forward paper ledger를 운영하여 표본 외(out-of-sample) 성능을 추적 중이다.

---

## 6. 빠른 시작

### 빌드 및 검증

```bash
cargo build --release
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
```

### 환경 변수 설정

`.env.example`을 `.env`로 복사한 뒤 필요한 API 키를 입력한다.

- data.go.kr service key
- KIS app key / secret
- OpenDART key
- FRED key

### 실행 예시

```bash
# KRX 일별 시세 수집 (2020-01-01 이후)
target/release/asterion fetch --universe config/universe/kr-union.txt --from 2020-01-01

# KIS 과거 일봉 수집 (재개 가능)
asterion fetch-history --universe config/universe/kr-union.txt

# OpenDART 재무제표 수집
asterion fetch-financials

# Reddit 게시물 수집 (Arctic Shift)
asterion fetch-reddit --from 2024-01-01

# 로컬 모니터링 대시보드 실행 (http://127.0.0.1:8787)
asterion monitor
```

---

## 7. 다음 할 일

1. KIS 과거 일봉(2010-01-01..2020-06-30) 및 OpenDART 재무제표 수집 완료.
2. 저장 공간 확보 후 Arctic Shift 덤프 파일을 통한 Reddit 과거 이력 적재(최근 일자는 기존 API 유지).
3. 2010–2020 KIS 과거 일봉과 OpenDART 재무제표를 피처에 반영하고 purged walk-forward CV 재실행.
4. Forward paper ledger 채점을 지속하며, 비용 차감 후 표본 외에서 유의미한 성능이 확인될 때만 우위(edge)를 인정.

---

## 8. 데이터 이용 원칙 / 면책

- 본 저장소는 개인의 비상업적 딥러닝·계량 연구 목적으로만 사용되며, 특정 종목의 매수·매도를 권유하거나 투자 자문을 제공하지 않는다.
- API 키는 `.env` 파일에만 보관하며, `.env`와 `data/` 디렉터리는 `.gitignore`에 의해 버전 관리에서 제외된다. `data/raw/`의 원시 데이터는 커밋하지 않는다.
- Reddit 데이터는 Arctic Shift 아카이브를 통해 수집하며, 작성자/사용자 식별 필드는 수집 단계에서 제거하고 저장하지 않는다.
- 현재 모델은 계획된 목표 성능(80% `directional accuracy`)을 충족하지 못했으며, 검증된 표본 외 우위가 확보되기 전까지 어떤 실거래 성과도 전제하지 않는다.