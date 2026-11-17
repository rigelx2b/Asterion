# Reddit 게시글 수집기 (Arctic Shift API) 설계

## 1) 목적과 이유

* **목적**: KRX 종목 예측 및 랭킹용 Rust 워크스페이스인 Asterion(`asterion-core`, `asterion-data`, `asterion-model`, `asterion-cli`)에서 활용할 수 있도록, Reddit 투자 커뮤니티 게시글을 수집해 일별 시장 심리 시그널(daily aggregates)을 생성하는 파이프라인을 구축한다.
* **배경**: Reddit 공식 Data API는 `2026-10-31`부로 신규 접근 요청 접수를 종료했으며, 2차 출처 기준 `2027-03`에 공개 Data API 종료가 예정되어 있다. 이에 API 키 없이 무료로 이용할 수 있는 서드파티 아카이브인 Arctic Shift(`https://arctic-shift.photon-reddit.com`)를 데이터 소스로 채택한다.
* **사용 범위 및 법적 전제**: 오너의 개인 비상업적 연구 용도로만 사용하며, 원시 텍스트는 로컬에만 보관하고 판매·배포·재배포하지 않는다. 모델은 추후 원문 텍스트가 아니라 일별 집계 수치만 소비한다.

---

## 2) 목표 / 비목표 / 반목표

### 목표 (Goals)
* `asterion-data`에 Arctic Shift `GET /api/posts/search` 기반의 서브레딧별 게시글(submissions) 수집기를 구현하고, 기존 `RawCache`(`crates/asterion-data/src/cache.rs`), `Http` / `RetryPolicy`(`crates/asterion-data/src/http.rs`), `ProgressTracker`(`crates/asterion-data/src/progress.rs`) 패턴에 맞춰 통합한다.
* 동일 `created_utc` 초에 여러 게시글이 몰려도 누락 없이 재개(resumable) 가능한 과거 백필과 일일 증분 갱신(약 D-1 신선도)을 단일 수집 경로로 지원한다.
* 로컬 원시 캐시로부터 시점 누출(look-ahead) 없는 일별 집계 파일(`data/derived/reddit_daily.json`)을 결정론적으로 전체 재생성한다.
* `asterion-cli`에 `fetch-reddit`, `reddit-daily` 서브커맨드를 추가하고, `ProgressTracker`가 `data/status`에 기록하는 스냅샷을 통해 기존 `monitor` 웹 대시보드에서 진행 상태를 확인할 수 있게 한다.

### 비목표 (Non-goals / Out of scope)
* **`MarketContext` 및 `features.rs` 연동**: `crates/asterion-model/src/features.rs`의 `NUM_FEATURES`는 고정 상수이며, `scripts/daily.sh`는 핑거프린팅된 동결 모델의 write-once 원장(`ledger.rs`, `rank_ledger.rs`)을 기록한다. 피처 추가는 모델 입력을 변경하므로, `MarketContext`(`crates/asterion-model/src/context.rs`) 편입 및 `research-cv` 비교 평가는 별도의 후속 설계로 분리한다.
* **댓글(comments) 수집 및 벌크 덤프(bulk dumps) 임포터**: 현재 범위에서 제외한다. 추후 댓글이 반드시 필요하다고 입증되면 별도로 추가한다.
* **`scripts/daily.sh` 편입**: 오너의 명시적 동의 전까지 자동 실행 파이프라인에 넣지 않는다.
* **CLI 도움말(help) 문구 작성**: 오너의 별도 텍스트 워크플로를 통해 추후 채운다.

### 반목표 (Anti-goals)
* `author` 또는 `author_*` 등 작성자·사용자 식별자 요청 및 저장.
* Reddit 원문 텍스트의 Git 커밋 또는 외부 공유·재배포(`/data/`는 이미 `.gitignore`에 포함되어 있음).
* 미래 정보를 암묵적으로 사용하는 피처 생성(예: 수집 시점의 성숙된 반응 지표 사용).
* 1인이 운영하는 무료 아카이브 서비스에 대한 과도한 요청(rate limit 무시, 병렬 폭주).

---

## 3) 제약

* **Arctic Shift API 동작 (`2026-11-16` 프로브 검증 결과)**:
  * 1인 운영 무료 아카이브이며 API 키 및 SLA가 없다.
  * 엔드포인트: `GET https://arctic-shift.photon-reddit.com/api/posts/search` (지원 파라미터: `subreddit`, `after`, `before`, `limit`, `sort=asc`, `fields` 쉼표 구분 목록).
  * `limit`은 `1..100` 범위만 허용된다(범위 외 요청 시 HTTP `400`과 본문 `{"data":null,"error":"'limit' must be between 1 and 100"}` 반환).
  * `after`는 ISO 날짜 또는 epoch 초를 받으며 **배타적(exclusive)**이다. 다수의 게시글이 동일한 `created_utc` 초를 공유한다.
  * 응답 헤더에는 `x-ratelimit-reset`과 `x-ratelimit-reset-at`이 포함되지만, 잔여 요청 횟수(remaining-count) 헤더는 없다.
  * 부하가 큰 쿼리는 `{"data":null,"error":"Timeout. Maybe slow down a bit"}`를 반환할 수 있다.
  * 서버 측 집계 엔드포인트(`/api/posts/search/aggregate`)는 모든 프로브 시도에서 타임아웃이 발생했으므로 사용할 수 없으며, 일별 집계는 로컬에서 계산해야 한다.
  * `fields` 파라미터는 화이트리스트만 허용한다. `retrieved_utc`, `upvote_ratio`, `removed_by_category`, `edited`, `stickied`를 요청하면 `{"data":null,"error":"'<name>' is not a valid field"}`가 반환된다.
  * 검증된 동작 필드 집합은 `id,created_utc,title,selftext,score,num_comments,link_flair_text,over_18,retrieved_on`이다.
  * `fields`를 생략한 전체 응답에는 `author`와 다수의 `author_*` 필드가 포함되므로, `fields` 지정으로 `author`를 원천 배제해야 한다.
  * 수집 지연 및 2차 패스: 준실시간 수집이 이루어지며(프로브 시점 최신 글은 작성 후 약 22분 경과, 최근 글의 `retrieved_on`은 `created_utc` 약 20초 후), 약 2일 뒤 점수 등을 갱신하는 2차 패스(`_meta.retrieved_2nd_on`)가 동작한다. 반면 `2021`년 등 과거 글은 `retrieved_on` 대신 `retrieved_utc`를 갖는 경우가 있어, `retrieved_on` 타임스탬프는 best-effort이며 응답에 없을 수 있다.
* **물량 및 백필 시간**:
  * `r/wallstreetbets` 프로브 기준: 평시(`2019-06-12`, `2025-06-11`)는 12~14시간당 약 100건(일 약 150~250건)이나, 피크일(`2021-01-28`)은 단일 100초 구간에 최소 100건 이상이 발생해 하루 수집에만 수백 페이지가 필요하다.
  * 전체 백필 소요 시간은 아직 실측되지 않았으며, 초당 약 1회 요청 기준 대략 1~2일로 추정된다. 빌더는 첫 1주치 백필 후 실측치를 보고해야 한다.
* **시점 정합성 (Point-in-time / Look-ahead 방지)**:
  * `score`, `num_comments`, `selftext`(모더레이션 후 `"[removed]"`로 바뀜)는 작성 시점이 아니라 수집 시점(`retrieved_on` / `retrieved_utc`, 작성 후 수초~수년 뒤)의 스냅샷이며 2차 패스로 갱신된다.
  * 프로브에서 최신 글은 `score = 1`, `num_comments = 0`으로 반환된 반면, 백필된 글은 성숙된 값이 반환됨을 확인했다. 따라서 기본 피처/집계에는 게시글 수와 `title` 텍스트만 사용하며, `score`/`num_comments`는 기본적으로 제외한다. (향후 추가 시 백필과 일일 갱신 모두에 동일한 고정 성숙 지연(maturity lag)을 적용해야 한다.) 이는 댓글 수를 활동량 대용 지표로 제안했던 이전 안을 정정한 결정이다.

---

## 4) 접근 방식 비교

| 안 | 설명 | 장점 | 단점 |
| :--- | :--- | :--- | :--- |
| **A (채택)** | Arctic Shift Search API(`/api/posts/search`)로 서브레딧별 게시글(submissions)만 재개 가능하게 수집하고 일일 갱신에도 동일 경로 사용 | 구현 범위가 단순함. 기존 `RawCache`/`Http`/`RetryPolicy` 구조에 자연스럽게 맞음. 백필과 일일 갱신을 단일 코드 경로로 처리 | 댓글 미수집. 약 1 req/s 제한으로 초기 백필에 대략 1~2일 소요 추정. 무료 1인 서비스 가용성에 의존 |
| **B** | 벌크 덤프 활용 (Academic Torrents의 Watchful1 서브레딧별 `.zst`: `2005-06`..`2023-12` 및 `2024` 추가분, Arctic Shift 월별 덤프, Hugging Face Parquet) | 댓글 포함. API rate limit 없음 | 수동 토렌트 다운로드 필요, 수십 GB 용량, zstd NDJSON 임포터 구현 필요, 최근 몇 달은 결국 API 필요 |
| **C** | A + B 하이브리드 | 과거 댓글·게시글 커버리지가 가장 완전함 | 구현 작업량이 대략 2배로 증가 |

* **선택 및 이유**: 오너는 최초 C를 고려했으나 최종적으로 **"일단 A"**를 선택했다. 일별 시장 심리 시그널 구축이라는 초기 목표에는 게시글 수 및 제목 기반 집계로 충분하고, 백필과 일일 갱신을 하나의 재개 가능한 파이프라인으로 단순하게 유지할 수 있다. 향후 댓글 데이터가 필요하다고 입증되면 그때 B를 추가할 수 있다.

---

## 5) 결정된 설계

### 5.1 수집 대상 및 기간
* **설정 파일**: `config/reddit/subreddits.txt` (한 줄에 서브레딧 1개, `#` 주석 및 빈 줄 허용).
* **초기 대상 서브레딧**:
  * `wallstreetbets` (최초 게시글 `2012-04`)
  * `stocks` (`2008`)
  * `investing` (`2008`)
  * `StockMarket` (`2008`)
  * `korea` (`2008-12`)
  * `KoreanStocks` (`2019-02`)
* **수집 기간**: `2008-01-01`(또는 각 서브레딧 최초 게시글 시점) ~ 오늘(UTC).

### 5.2 API 호출, 필드 화이트리스트 및 속도·오류 제어
* **요청 파라미터**:
  * `subreddit`: 서브레딧 이름
  * `after`: epoch 초 (배타적 경계)
  * `before`: epoch 초
  * `limit`: `100`
  * `sort`: `asc`
  * `fields`: `id,created_utc,title,selftext,score,num_comments,link_flair_text,over_18,retrieved_on`
* **사용자 식별자 차단 (심층 방어)**:
  * `author` 및 사용자 식별 필드는 절대 요청하지 않는다.
  * 방어적 조치(defence in depth)로, 응답 JSON 객체에 `author`로 시작하는 키(`author*`)가 포함되어 있으면 캐시에 기록하기 전에 제거(strip)한다.
* **속도 제한 `[설계자 판단]`**:
  * 기존 `Http` / `RetryPolicy`를 사용하며 요청 간 `min_interval`은 `1초`로 설정한다.
  * 재시도 시 응답 헤더의 `x-ratelimit-reset`을 존중하여 대기한다. (현재 `crates/asterion-data/src/http.rs`의 `Http::send`는 `2xx` 응답 시 헤더를 반환하지 않고 본문만 반환하므로, `x-ratelimit-reset` 헤더 및 본문 `data: null` 오류 판정을 처리할 수 있도록 `Http` 인터페이스를 확장한다.)
* **오류 분류 및 재시도**:
  * **재시도 가능 (backoff via `RetryPolicy`)**: HTTP `429`, HTTP `5xx`, 네트워크/요청 타임아웃, 그리고 응답 JSON에서 `data`가 `null`이면서 `error`가 타임아웃 또는 rate 제한 메시지인 경우(예: `{"data":null,"error":"Timeout. Maybe slow down a bit"}`).
  * **즉시 실패 (fail-fast)**: HTTP `400` 및 `"is not a valid field"` 오류 본문(예: `{"data":null,"error":"'<name>' is not a valid field"}`)은 프로그래밍 오류로 간주해 재시도 없이 즉시 실패한다.
  * 어떤 범위든 모든 페이지가 성공하기 전에는 커버된 범위로 기록하지 않는다(단, 5.3절의 `[설계자 판단]` 무진행 절단(truncation) 예외 적용).

### 5.3 페이지네이션 및 무진행 가드 (No-progress Guard)
* **페이지네이션 규칙**:
  1. 각 수집 구간의 첫 페이지부터 `sort=asc`, `limit=100`으로 요청한다.
  2. 다음 페이지는 `after = (이전 페이지의 최대 created_utc - 1)`로 요청한다. `after`가 배타적이므로 경계 초(`max created_utc`)에 동률로 존재하는 게시글을 재조회해 누락을 막고, 수신된 게시글은 `id` 기준으로 중복 제거(dedupe)한다.
  3. **정상 종료 조건**: 응답 페이지의 게시글 수가 `100`건 미만이거나, 페이지 내 최대 `created_utc`가 `before`에 도달하면 종료한다.
* **무진행 가드**:
  * 한 페이지의 `100`건 전체가 단일 `created_utc` 초에 몰려 있고, 다음 페이지(`after = 해당 초 - 1`)에서 새로운 `id`가 하나도 추가되지 않으면 무한 루프 대신 명시적 무진행 상태를 발생시킨다.
* **미완료 주간 및 상한 처리 `[설계자 판단]`**:
  * **무진행 가드 발동 시**: `after`의 1초 단위 해상도 한계로 API가 1초 내 100건 이상을 페이지네이션할 수 없는 경우이므로, 오류를 로그로 남기고 그때까지 확보한 게시글은 보존한 채 해당 주간을 절단 마커(truncation marker)와 함께 `manifest.json`에 커밋한다. 해당 일자의 `reddit_daily.json` 레코드에는 `truncated: true`를 전파하여 피크 주간이 영구적인 공백으로 남지 않게 한다.
  * **`--max-requests` 도달 시**: `--max-requests`의 기본값은 무제한이며, 주간 수집 도중 상한에 도달하면 해당 주간은 커밋하지 않고 다음 실행 시 다시 수집한다.

### 5.4 원시 캐시, 확정(Final) vs 잠정(Provisional) 데이터
* **캐시 단위 및 구조 `[설계자 판단]`**:
  * `RawCache` source ID는 `"arctic_shift_reddit"`, 캐시 키(`key`)는 서브레딧 이름, 기본 루트 경로는 `data/raw/reddit`이다.
  * **1개 `RawCache` 범위 = 서브레딧별 1 UTC 달력 주(calendar week)**. 피크 주간에서 장애가 나더라도 재수집 범위를 최대 1주로 제한하면서, 게시글이 적은 초기 연도에 일별 요청 낭비를 피하기 위한 선택이다.
  * `manifest.json`에는 해당 주의 모든 페이지가 완료된 뒤에만 범위를 커밋한다.
* **확정 vs 잠정 구간 `[설계자 판단]`**:
  * Arctic Shift의 약 2일 뒤 2차 패스(`_meta.retrieved_2nd_on`)가 지난 뒤 데이터를 고정하기 위해, **종료 시점이 현재보다 최소 3일 이전인 주(week)**만 확정(final) 주간으로 간주해 `manifest.json`에 커밋한다.
  * 마지막 확정 주간 이후의 날짜들(최근 3일 이내를 포함하는 최신 구간)은 `manifest.json`에 커밋하지 않고 별도의 잠정(provisional) 영역에 저장하며, 실행할 때마다 덮어쓰기로 재수집한다.
  * `--to` 옵션은 "이 날짜까지 수집하되, 최근 3일 이내 날짜는 잠정 구간으로 취급"함을 의미한다.
  * `reddit-daily`는 잠정 구간의 게시글도 포함해 집계하되 해당 일자 레코드에 `final: false`를 표시한다. 이를 통해 확정 데이터의 재현성을 유지하면서도 약 D-1 수준의 최신 시그널을 사용할 수 있다.

### 5.5 일별 집계 (`reddit_daily.json`) 및 시점 정합성
* **UTC 일자 버킷팅 및 미래 누출 방지**:
  * 모든 게시글은 `created_utc`의 **UTC 달력 날짜** 기준으로 버킷팅한다.
  * 향후 모델/피처 소비자는 `crates/asterion-model/src/features.rs`가 기존 매크로 피처에 `DailySeries::index_at(date, true, ...)`(`crates/asterion-model/src/context.rs`)를 호출하듯, KRX 거래일 `D`에 대해 `strict = true`로 `< D`인 관측치만 읽어야 한다. `D-1 23:59 UTC`는 `D 08:59 KST`이므로 KRX 장 마감(`15:30 KST`) 이전 데이터만 사용되어 정보 누출이 없다.
* **성숙 편향 배제**:
  * `score`, `num_comments`, `selftext`는 원시 캐시에만 저장하고, 일별 집계 계산에는 사용하지 않는다.
* **고정(stickied) 게시글 필터 미적용 `[설계자 판단]`**:
  * `stickied`는 `fields`로 요청할 수 없고 값 자체도 수집 시점 스냅샷(실시간 수집 시 고정 상태, 백필 시 해제 상태)이므로 필터링하지 않고 모든 게시글을 집계한다. 일일 고정 스레드로 인한 약 1~2건/일 수준의 노이즈는 수용한다.
* **빈 날짜(Zero rows) 명시 `[설계자 판단]`**:
  * 수집이 완료된 날짜 중 게시글이 0건인 날은 생략하지 않고 명시적인 0건 레코드로 기록하여, "게시글 없음"과 "미수집"을 구분한다.
* **집계 산식 및 출력 `[설계자 판단]`**:
  * 별도 집계 단계(`reddit-daily`)가 원시 캐시(확정 주간 + 잠정 영역) 전체로부터 `data/derived/reddit_daily.json`을 매번 처음부터 재생성한다(페이지 경계 재조회 중복은 `id` 기준 1회만 집계).
  * 서브레딧 × UTC 일자별 산출 항목:
    * `post_count`: 해당 UTC 일자의 게시글 수
    * `bull_count`, `bear_count`: `title`에 대한 소형 고정 영어 키워드 렉시콘 매칭 수 (예: 강세 `calls`, `moon`, `🚀`, `buy`, `long` vs 약세 `puts`, `crash`, `bear`, `sell`, `short`; 정확한 목록은 코드에 고정하고 버전을 부여)
    * `net_sentiment`: `(bull_count - bear_count) / (bull_count + bear_count + 1)`
    * `korea_mentions`: `title`이 `Korea`, `KOSPI`, `Samsung`, `Hynix`, `Hyundai`, `EWY` 중 하나를 대소문자 무시(case-insensitive)로 포함하는 게시글 수
    * 상태 플래그 및 메타: `final` (`bool`), `truncated` (`bool`), `lexicon_version`

### 5.6 CLI 및 모니터링
* `crates/asterion-cli/src/main.rs`에 다음 두 서브커맨드를 추가한다(`fetch-macro`, `fetch-investors`와 동일하게 clap 서브커맨드로 구성).
  * `fetch-reddit`:
    * `--list`: 서브레딧 설정 파일 경로 (기본값 `config/reddit/subreddits.txt`)
    * `--from`: 시작 날짜 (기본값 `2008-01-01`)
    * `--to`: 종료 날짜 (기본값 오늘 UTC)
    * `--root`: 원시 캐시 루트 경로 (기본값 `data/raw/reddit`)
    * `--max-requests`: 1회 실행당 최대 요청 수 (기본값 무제한)
    * `--status-dir`: `ProgressTracker` 스냅샷 디렉터리(기존 CLI 전역 옵션 연계)
  * `reddit-daily`:
    * 원시 캐시로부터 `data/derived/reddit_daily.json`을 생성한다.
* 두 커맨드 모두 `ProgressTracker`를 통해 `data/status`에 진행 상태를 기록해 기존 `monitor` 웹 대시보드에서 확인할 수 있게 한다.
* CLI 도움말(`about`/`help`) 문자열은 추후 오너의 텍스트 워크플로로 작성하므로 빌더는 최소 플레이스홀더 수준으로 둔다.

---

## 6) 빌더에게 맡기는 부분

* `asterion-data` 내부 모듈 배치 (`crates/asterion-data/src/reddit.rs` 또는 하위 모듈 구조 권장).
* `RetryPolicy`의 구체적인 재시도 횟수(`max_attempts`)와 초기/최대 백오프 수치, `x-ratelimit-reset` 헤더 파싱 세부 로직.
* `data/derived/reddit_daily.json`의 정확한 JSON 스키마 세부 사항(최상위 구조, 레코드 필드명/정렬 순서, 날짜 직렬화 형식 등).
* `ProgressTracker`의 `stage` 이름 및 `counters` 키 이름.
* 잠정(provisional) 영역의 디렉터리/파일 명명 방식 및 원시 페이지 저장 형식.
* 렉시콘 키워드 최종 목록(예시 키워드 포함) 및 단어 경계/이모지 매칭 세부 규칙, `lexicon_version` 값 형식.

---

## 7) 검증 방법

### 7.1 `wiremock` 통합 테스트 (`crates/asterion-data/tests`)
다음 시나리오를 모두 `wiremock`으로 검증한다.
1. **동률 초 페이지네이션 및 중복 제거**: 경계 `created_utc` 초에 여러 게시글이 걸칠 때 `after = max_created_utc - 1` 재조회 후 `id` 기준 중복 제거가 정확히 동작하고 누락이 없는지 확인.
2. **배타적 `after` 처리**: `after`가 배타적임에도 첫 페이지 및 경계 전환 시 게시글 누락이 없는지 확인.
3. **`data: null` 오류 재시도 및 범위 미커밋**: 타임아웃/속도 제한 성격의 `data: null` 오류 응답 시 `RetryPolicy`로 재시도하고, 최종 실패하면 `manifest.json`에 범위가 커밋되지 않음을 확인.
4. **HTTP `400` 즉시 실패**: `400` 응답 시 재시도 없이 즉시 실패함을 확인.
5. **잘못된 필드 오류 본문 즉시 실패**: `{"data":null,"error":"'<name>' is not a valid field"}` 응답 시 HTTP `400`과 동일하게 재시도 없이 즉시 실패함을 확인.
6. **무진행 가드 및 절단 커밋**: 100건 전체가 1초에 속해 다음 페이지에서 신규 `id`가 없을 때 무한 루프 없이 중단되며, 확보된 게시글을 보존하고 절단 마커와 함께 커밋되어 집계 시 `truncated=true`로 이어지는지 확인.
7. **주간 단위 원자적 커밋**: 주간 내 모든 페이지가 성공한 경우에만 `manifest.json`에 커밋되고, 중간 페이지 실패 시 커밋되지 않음을 확인.
8. **3일 이내 잠정(provisional) 구간 처리**: 종료일이 현재로부터 3일 이내인 주간은 `manifest.json`에 커밋되지 않고 잠정 영역에 저장되며, 재실행 시 재수집됨을 확인.
9. **`author` 미요청 및 방어적 제거**: 요청 쿼리의 `fields`에 `author`가 포함되지 않음을 단언하고, 목 응답에 `author` 또는 `author_*` 키가 섞여 오더라도 캐시 저장 전 제거됨을 확인.

### 7.2 단위 테스트
* **렉시콘 및 지표 계산**: `bull_count`, `bear_count`, `net_sentiment`, `korea_mentions` 계산과 `lexicon_version` 기록, 빈 날짜 zero row 생성 검증.
* **UTC 일자 버킷팅**: `created_utc` 경계 시각(`23:59:59 UTC` vs `00:00:00 UTC`)이 정확한 UTC 달력 날짜로 분류되는지 검증.

### 7.3 라이브 스모크 실행 및 백필 실측
1. **단기 스모크 테스트**: `KoreanStocks` 서브레딧 1개월 구간에 대해 `fetch-reddit`과 `reddit-daily`를 실행하고, 수집 건수를 수동 API 조회 결과와 대조 검증한다.
2. **피크 주간 백필 실측 보고**: `2021-01-28`을 포함한 `wallstreetbets` 1주일 구간을 백필하여 페이지 수, 실제 소요 시간, 재시도/무진행 발생 여부를 측정해 보고한다.