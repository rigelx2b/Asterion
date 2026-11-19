# Asterion

## 1. 프로젝트 소개

Asterion은 한국 주식(KOSPI/KOSDAQ)의 미래 가격 및 수익률을 1, 3, 5, 10, 20 거래일(`h1`, `h3`, `h5`, `h10`, `h20`) 다중 지평(horizon)에서 예측하기 위한 Rust 기반 딥러닝 연구 시스템이다.

- **연구 목표**: 계획상 목표치는 엄격한 기준에서 `directional accuracy` 80%이며, **현재 시점까지 목표에 도달하지 못했다.**
- **성격**: 개인적·비상업적 연구 목적의 프로젝트이며, 투자 자문이나 매매 권유가 아니다.
- **기술 스택**: Rust stable 툴체인(`rustfmt`, `clippy`), Cargo workspace edition 2024 (`crates/*` 멤버 구성).

---

## 2. 현재 진행 상황 요약 (2026-11-19 기준)

- **목표 달성 여부**: 미달. 저장된 동결(frozen) 모델의 held-out test 기준 `directional accuracy`는 `h1` 51.3%~`h10` 57.4% 수준이며, `balanced accuracy`는 전 지평에서 약 50~51%로 클래스 불균형의 영향이 크다.
- **데이터 수집**:
  - **완료**: data.go.kr 일별 시세(2020-01-01..2026-11-13), 지수, FRED 거시 지표, OpenDART 공시, OpenDART 재무제표(`79,800 / 79,800` 요청, 실패 0건, 2026-11-18 01:38 KST 완료), KIS 종목별 투자자 수급, KIS 해외 일봉, KIS 과거 일봉(2010-01-01..2020-06-30, `2,812`종목, 2026-11-18 완료), KIS KOSPI/KOSDAQ 과거 지수(2010-01-01..2020-06-30, 2026-11-19 완료).
  - **완료(추가)**: KIS 과거 일봉(`fetch-history`, `2010-01-01..2020-06-30`) 전 종목 수집 완료(`2,812 / 2,812`종목, 2026-11-18 완료; 백엔드 오류 `OPSQ0008`로 실패했던 마지막 9종목 재수집 완료, 실패 0건).
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
  - `resume-collect.sh`: 메모리 부족으로 중단된 수집 작업 재개(`delay_secs` 대기 후 여유 물리 메모리가 `min_free_mb`(기본 2048 MB, 30초마다 재확인) 이상이 될 때까지 기다렸다가 `fetch-history`(`kr-union` 유니버스) 또는 `fetch-financials`를 재개하며, `data/logs/<job>-resume-<date>.log`에 로그 기록)
  - `collect-all.sh`, `expand-universe.sh`, `kis-probe.sh`, `train-full.sh`
- `docs/`:
  - `superpowers/specs`, `plans`: Reddit 수집기 설계 및 구현 계획
  - `api-application`: data.go.kr API 활용 계획

---

## 4. 데이터 수집

### 데이터 소스 및 현황

| 소스 | 대상 데이터 | CLI 명령 | 상태 (2026-11-19) |
| :--- | :--- | :--- | :--- |
| data.go.kr | KRX 일별 시세 (2020-01-01..2026-11-13) | `fetch` | 완료 |
| data.go.kr | KOSPI / KOSDAQ / KOSPI200 지수 | `fetch-index` | 완료 |
| FRED | 미국 시장 및 거시경제 시계열 | `fetch-macro` | 완료 |
| OpenDART | 공시 목록/내역 | `fetch-disclosures` | 완료 |
| OpenDART | 재무제표 | `fetch-financials` | 완료 (79,800 / 79,800 요청, 실패 0건, 2026-11-18 01:38 KST) |
| KIS Open API | 종목별 투자자 수급 (개인/외국인/기관) | `fetch-investors` | 완료 |
| KIS Open API | 해외 일봉 (`us-core.txt`) | `fetch-overseas` | 완료 |
| KIS Open API | 과거 일봉 (2010-01-01..2020-06-30, 2,812종목) | `fetch-history` | 완료 (2,812 / 2,812 종목, 실패 0건, 2026-11-18) |
| KIS Open API | KOSPI / KOSDAQ 과거 지수 (2010-01-01..2020-06-30) | `fetch-index-history` | 완료 (지수별 2,589거래일, 실패 0건, 2026-11-19) |
| OpenDART | 기업 고유번호 목록 (`corpCode.xml`, 상장폐지 기업 포함) | `fetch-corp-codes` | 완료 (120,028개 기업, 2026-11-19) |
| OpenDART | 2020년 이후 상장폐지 종목 공시·주요 재무 백필 (222종목) | `fetch-dart-backfill` | 완료 (2,759회 요청, 실패 0건, 2026-11-19) |
| Arctic Shift (Reddit) | 서브레딧 게시물 및 일별 집계 | `fetch-reddit`, `reddit-daily` | 수집기 구현 완료, 과거 백필은 덤프 적재 예정 |

- 모든 `fetch*` 명령은 중단 후 재실행 시 누락 구간만 이어서 수집하며(resumable), 진행 상태를 `data/status`에 기록한다.
- 장시간 소요되는 수집 작업은 백그라운드로 실행하며, 메모리 부족으로 중단된 경우 `resume-collect.sh`로 재개하고 진행 상황은 모니터링 대시보드(`http://127.0.0.1:8787`)에서 확인할 수 있다.
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

- 설정 (`2026-11-13` 실행): 선택·판정용 dev 테스트 폴드 `2022`/`2023`/`2024`(확장 학습, `30`일 embargo), 상위 10분위 롱온리(top-decile long-only), 거래비용 차감 기준.
- `gbdt` (dev 통합 `IC`): `h5` `0.138`, `h10` `0.159`, `h20` `0.189`.
- `ridge` (dev 통합 `IC`): `h5` `0.133`, `h10` `0.157`, `h20` `0.189`; 대조군 대비 증분 초과수익(incremental excess)은 유의미하지 않음(`t` `0.95–1.30`).
- `2025-01-01` 이후 burnt 구간(`--report-burnt`, 보고 전용·선택 미사용): `gbdt` `IC` `0.14–0.20`, `ridge` `IC` `0.13–0.18`, 증분 `t < 2.2`.
- 최종 판정: `composite6`, `ridge`, `gbdt` 모두 `passed=false`.

#### 2010–2019 과거 일봉 병합 (`research-cv --with-history`, 2026-11-19)

- **데이터 병합**: `research-cv --with-history`는 `data.go.kr` 시계열(`2020+`) 앞단에 KIS `2010–2019` 일봉을 `2020-01..06` 중첩 구간 종가 비율 중앙값으로 스케일 조정해 접합하며, 학습 샘플은 `2015-01-01`부터 시작한다. `2,212`개 종목이 접합되었고(`2020`년 이전 일봉 없음 `598`종목, 가격 기준 불안정으로 제외 `2`종목, 캐시 누락 `0`건), 패널 행 수는 `1,647,640`에서 `2,685,552`로 증가했다.
- **통제 실험 설계**: `--with-history` 미적용 대조 실행과 동일한 데이터·폴드(`2022`/`2023`/`2024` 테스트 윈도우, `30`일 embargo)·`55`개 피처 조건으로 비교했다. 대조군(`control_lowvol`, `composite6`)은 두 실행에서 완전히 동일하므로 테스트 행은 변하지 않고 학습 데이터만 달라진다.
- **통합(pooled) 성능 변화 (대조군 → 이력 추가)**: `gbdt` `IC`는 `h5` `0.138` → `0.140`, `h10` `0.159` → `0.163`, `h20` `0.189` → `0.194`로 소폭 상승했으나, 대조군 대비 `gbdt` 증분 초과수익(incremental excess)은 `h5` `+4.9%` → `+3.0%`(`t` `0.97` → `0.54`), `h10` `+5.5%` → `+5.1%`, `h20` `+5.9%` → `+5.9%`(`t` `1.37` → `1.31`)로 개선되지 않았고, `ridge` 증분 초과수익(`h20`)도 `+4.9%` → `+3.3%`(`t` `1.30` → `0.84`)로 하락했다. `gbdt` 평균 회전율(turnover)은 `0.54–0.62` → `0.61–0.65`로 증가했다.
- **판정**: 모든 후보 모델이 여전히 `passed=false`이며, 모든 증분 `t < 1.5`로 차이가 노이즈 범위 내에 있다. 따라서 과거 이력 추가에 따른 측정 가능한 이득은 없으며, 소폭의 `IC` 상승도 순 초과수익으로 이어지지 않았다.
- **유니버스·시가총액 한계**: `2020`년 이전 시가총액은 `2020`년 주식 수 기준을 사용했으며(학습 행에만 해당), 유니버스가 `2020+` 상장 종목 기반으로 구성되어 `2020`년 이전 상장폐지 종목이 누락(생존 편향)되었다.
- **피처 결손 및 설정**: 지수 병합 이전에는 지수 데이터가 `2020`년부터 시작해 `2020`년 이전 시장 대비 피처가 `0`으로 처리되었으나, `--with-index-history` 적용으로 이 문제가 해소되었다(시장 지수 병합 하위 절 참조). 다만 `2015`년 행에는 재무 데이터가 거의 없으며(`fund_available` `0.5%`), `--history-from` 설정은 테스트 폴드에서 튜닝하지 않았다.

#### 2010–2019 시장 지수 병합 (`research-cv --with-index-history`, 2026-11-19)

- **데이터 수집·병합**: 신규 명령 `fetch-index-history`로 KIS 일봉 지수 차트 API(TR `FHKUP03500100`)에서 `2010-01-01..2020-06-30` 기간의 KOSPI(KIS 업종 코드 `0001`) 및 KOSDAQ(`1001`) 일별 지수를 수집했다. API가 호출당 최대 `50`행만 반환하므로 `60`역일 단위 윈도우로 요청하고 `50`행이 가득 찬 윈도우는 거부(미캐시)해 지수별 `2,589`거래일(`128`회 요청, 실패 `0`건)을 확보했다. `research-cv --with-index-history`(opt-in)는 `data.go.kr` 지수 시계열(`2020+`) 시작 이전 일자만 앞단에 접합하고 `2020+` 값은 유지한다. `2020-01..06` 중첩 `123`거래일에서 KIS와 `data.go.kr` 지수는 완전히 일치했으며(최대 상대 차이 `0`), 차이 `1e-4` 초과·중첩 `20`거래일 미만·앞단 공백 `14`일 초과 시 접합을 거부한다(관측된 최대 공백 `11`일, `2010-01-04`부터 지수별 `2,466`거래일 접합). 이에 따라 `2015–2019` 학습 행의 비영(nonzero) `mkt_ret_20` 비율이 `0.000`에서 `1.000`으로 상승했고 `2020`년 이전 `rel_ret_*`가 실제 시장 대비 수익률로 산출되었다.
- **통제 실험 설계**: 직전 `--with-history` 실행과 동일한 데이터·폴드(`2022`/`2023`/`2024`, `30`일 embargo)·`55`개 피처·`2,685,552`개 패널 행 조건에서 비교했다. 대조군(`control_lowvol`, `composite6`)은 바이트 단위로 동일하지는 않으나 리포트 수치가 부동소수점 반올림 오차 범위(합산 순서에 따른 차이 약 `1e-16`) 내에서 일치하므로 테스트 행은 변하지 않고 `2020`년 이전 학습 피처만 달라진다.
- **통합(pooled) 성능 변화**: 이전 대비 지수 병합 시 `gbdt` `IC`는 `h5` `0.140` → `0.140`, `h10` `0.163` → `0.162`, `h20` `0.194` → `0.194`를 기록했다. 대조군 대비 증분 초과수익은 `gbdt` `h5` `+3.0%`(`t` `0.54`) → `+2.1%`(`t` `0.38`), `h10` `+5.1%`(`t` `1.08`) → `+4.4%`(`t` `0.95`), `h20` `+5.9%`(`t` `1.31`) → `+6.0%`(`t` `1.33`), `ridge` `h20` `+3.3%`(`t` `0.84`) → `+3.3%`(`t` `0.84`), `gbdt_dart` `h20` `+6.3%`(`t` `1.40`) → `+5.3%`(`t` `1.18`)였다.
- **판정**: 모든 후보 모델이 여전히 `passed=false`이며 모든 증분 `t < 1.5`로 차이는 노이즈 수준이다. `2020`년 이전 시장 피처를 채워 넣어도 측정 가능한 성능 이득은 없었다.

#### 상장폐지 종목 OpenDART 누락 수정 (`--dart-backfill-root`, 2026-11-19)

- **문제**: 기존 OpenDART 공시 피드는 시장 구분(`corp_cls` `Y`/`K`)별로 수집되었으나, OpenDART가 각 기업의 현재 시장 구분을 보고하므로 상장폐지 기업(`E` 구분)이 누락되었다. 그 결과 `2020`년 이후 상장폐지된 유니버스 `222`개 종목은 종목코드-고유번호 매핑이 존재하지 않아, `2022–2024`년 테스트 폴드를 포함한 전체 상장 기간 동안 재무(`fund_*`) 및 공시(`dart_*`) 피처가 모두 `0`으로 처리되었다. 이에 따라 DART 데이터 결손이 사후 상장폐지 종목을 식별하는 표식(잠재적 선행참조 누수, look-ahead leak)으로 작용할 가능성이 있었다.
- **수정**: 신규 명령 `fetch-corp-codes`는 OpenDART `corpCode.xml`(`120,028`개 기업)을 내려받으며, 상장폐지 기업의 기존 종목코드를 유지해 `222/222`개 누락 종목을 모두 해소했고 기존 상장 `2,590`개 종목 매핑과도 전부 일치했다. 신규 명령 `fetch-dart-backfill`은 누락 종목의 주요 재무제표와 기업별 공시(`corp_code` 기준, `3`년 단위 윈도우)를 별도 캐시로 수집한다(`2,759`회 요청, 실패 `0`건; 공시 `222`종목 `100,694`건, 주요 재무 `215`종목 — 나머지 `7`종목은 재무제표가 없는 SPAC·펀드형 종목). 연구 옵션에는 반복 지정 가능한 `--dart-backfill-root`(opt-in)를 추가했으며, 기본 캐시와 백필 캐시에 동일 종목코드가 중복 존재하면 실행을 실패 처리해 기존 상장 종목 데이터는 변경되지 않도록 했다.
- **통제 실험 설계**: 백필을 적용하지 않은 동일 조건의 대조 실행(동일 명령, 이력 미병합, `2022`/`2023`/`2024` 폴드, `30`일 embargo, `55`개 피처)과 비교했다. 패널 행 수(`1,647,640`)와 `volatility_20`만 사용하는 `control_lowvol`은 완전히 동일하며, `fund_*` 피처를 사용하는 `composite6`은 수치가 달라질 수 있다.
- **결과·판정**: 통합(pooled) 기준(수정 전 → 수정 후) `gbdt` `IC`는 `h5` `0.1375` → `0.1385`, `h10` `0.1590` → `0.1600`, `h20` `0.1886` → `0.1892`, `ridge` `IC`는 `h20` `0.1887` → `0.1893`을 기록했다. 대조군 대비 증분 초과수익(incremental excess)은 `gbdt` `h20` `+5.9%`(`t` `1.37`) → `+6.0%`(`t` `1.46`), `ridge` `h20` `+4.9%`(`t` `1.30`) → `+4.8%`(`t` `1.31`), `composite6` `h20` `+3.6%`(`t` `0.80`) → `+3.9%`(`t` `0.84`)였다. 모든 후보 모델이 여전히 `passed=false`이며, 수정 후 `IC`가 하락하지 않고 최대 `0.001` 상승하는 데 그쳐 결손 데이터가 기존 보고 `IC`를 부풀렸다는 가설은 지지되지 않으며 차이는 노이즈 범위 내에 있다. 본 절에 앞서 기록된 CV 수치는 백필 미적용 상태에서 산출된 값이다.

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

# KIS 과거 KOSPI/KOSDAQ 일별 지수 수집 (2010-01-01..2020-06-30, 재개 가능, data.go.kr 교차 검증용 2020년 중첩 포함)
asterion fetch-index-history

# OpenDART 기업 고유번호 목록 수집 (상장폐지 기업 포함)
asterion fetch-corp-codes

# 시장 구분 피드에서 누락된 유니버스 종목의 기업별 공시·주요 재무 백필 수집 (이후 연구 명령에 --dart-backfill-root data/raw/opendart_backfill/universe 전달)
asterion fetch-dart-backfill --universe config/universe/kr-union.txt --root data/raw/opendart_backfill/universe

# OpenDART 재무제표 수집
asterion fetch-financials

# 메모리 부족으로 중단된 수집 재개 (delay_secs 대기 후 여유 메모리 >= min_free_mb(기본 2048 MB, 30초마다 재확인) 시 실행, data/logs/<job>-resume-<date>.log 기록)
bash scripts/resume-collect.sh <history|financials> [delay_secs] [min_free_mb]

# Reddit 게시물 수집 (Arctic Shift)
asterion fetch-reddit --from 2024-01-01

# 로컬 모니터링 대시보드 실행 (http://127.0.0.1:8787)
asterion monitor
```

---

## 7. 다음 할 일

1. 유니버스가 `2020+` 상장 종목 기반으로 구성되어 생존 편향이 남아 있으므로, `2020`년 이전 유니버스에 `2020`년 이전 상장폐지 종목을 추가해 보완(KIS `2020`년 이전 KOSPI/KOSDAQ 지수 수집은 `2026-11-19` 완료, 측정 가능한 이득 없음; `5`절 참조). 현재 상장폐지 종목의 OpenDART 데이터는 `fetch-dart-backfill`로 수집 가능하며 `2015–2019`년 상장폐지 `157`종목의 KIS 일봉(`data/raw/kis_daily_delisted`)도 수집 완료되었으므로, 상장폐지 수익률 규칙(delisting-return rule)과 함께 학습 유니버스에 편입하는 작업만 남아 있다.
2. 저장 공간 확보 후 Arctic Shift 덤프 파일을 통한 Reddit 과거 이력 적재(최근 일자는 기존 API 유지).
3. **완료 (2026-11-19)**: 2010–2020 KIS 과거 일봉·OpenDART 재무제표를 피처에 반영해 purged walk-forward CV 재실행 완료(전 후보 미통과, 측정 가능한 개선 없음; 5절 `--with-history` 결과 참조).
4. Forward paper ledger 채점을 지속하며, 비용 차감 후 표본 외에서 유의미한 성능이 확인될 때만 우위(edge)를 인정.
5. `--dart-backfill-root`를 기본 설정으로 채택할지 여부와, 백필 미적용 상태에서 학습·동결된 forward paper ledger의 `rank-fit` arm들을 재학습(refit)할지 여부를 결정한다.

---

## 8. 데이터 이용 원칙 / 면책

- 본 저장소는 개인의 비상업적 딥러닝·계량 연구 목적으로만 사용되며, 특정 종목의 매수·매도를 권유하거나 투자 자문을 제공하지 않는다.
- API 키는 `.env` 파일에만 보관하며, `.env`와 `data/` 디렉터리는 `.gitignore`에 의해 버전 관리에서 제외된다. `data/raw/`의 원시 데이터는 커밋하지 않는다.
- Reddit 데이터는 Arctic Shift 아카이브를 통해 수집하며, 작성자/사용자 식별 필드는 수집 단계에서 제거하고 저장하지 않는다.
- 현재 모델은 계획된 목표 성능(80% `directional accuracy`)을 충족하지 못했으며, 검증된 표본 외 우위가 확보되기 전까지 어떤 실거래 성과도 전제하지 않는다.