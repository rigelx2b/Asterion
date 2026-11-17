# Reddit Collector (Arctic Shift) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add `fetch-reddit` (resumable Arctic Shift submission collector) and `reddit-daily` (UTC-day aggregates) to Asterion.

**Architecture:** New module `asterion-data::reddit` (api / collect / daily) on top of the existing `Http`, `RawCache`, `missing_ranges` and `ProgressTracker`. Final weeks live in a `RawCache` at `data/raw/reddit/<sub>`, provisional days in a second `RawCache` at `data/raw/reddit/provisional/<sub>` that is wiped each run. Aggregation reads both and writes `data/derived/reddit_daily.json`.

**Tech Stack:** Rust 2024 workspace, reqwest, serde/serde_json, chrono, tokio, wiremock (dev). No new dependencies.

**Spec:** `docs/superpowers/specs/2026-11-16-reddit-collector-design.md`

## Global Constraints

- Endpoint `https://arctic-shift.photon-reddit.com/api/posts/search`; params `subreddit, after, before, limit=100, sort=asc, fields`.
- `fields=id,created_utc,title,selftext,score,num_comments,link_flair_text,over_18,retrieved_on` exactly. Never request `author`; strip every key starting with `author` before caching.
- `min_interval` = 1 s. `--max-requests` default unlimited.
- `RawCache` source id `arctic_shift_reddit`; provisional source id `arctic_shift_reddit_provisional`; root `data/raw/reddit`.
- Cache range = one UTC ISO week (Mon–Sun) clipped to the requested span. A chunk is final iff `chunk_end + 3 days <= today_utc`.
- Day bucketing = UTC calendar date of `created_utc`.
- Aggregates use post counts and `title` only; never `score`, `num_comments`, `selftext`. No stickied filter.
- `net_sentiment = (bull - bear) / (bull + bear + 1)`; `LEXICON_VERSION = "v1"`.
- No new crates (cargo-deny is configured in `deny.toml`).
- Clap `help`/`about` strings: write `__AGY__("...")` placeholders, then run `just-code-claude --replace-in-file crates/asterion-cli/src/main.rs -m "4 hydrogen" -e high` (owner rule). Do not hand-write help copy.
- Commits: owner rule says commit messages come from `just-code-claude --commit`, and the harness only commits when the owner asks. Each task ends with "stage", not commit. The working tree already has much uncommitted work; stage only this plan's files.
- Execute in place on `main` — no git worktree: most of the codebase (`http.rs`, `cache.rs`, `kis.rs`, all of `asterion-model`) is untracked, so a worktree would not contain it. Reviewers get the list of files touched, not a branch diff.
- `target/release/asterion.exe` is held open by running jobs (dashboard, KIS backfill); Windows cannot overwrite it. Use the debug profile (`cargo run -p asterion-cli -- …`) for smoke runs.
- Lints: workspace denies `unsafe_code`, warns `unwrap_used`; `cargo clippy --workspace --all-targets -- -D warnings` must pass.

## Review Focus

1. Older archive records serve `created_utc` as float (`1334162440.0`) or numeric string → must parse, never abort the week. (Task 2 test `created_utc_accepts_int_float_and_string`.)
2. Posts with missing/`null`/`[deleted]` title → counted in `post_count`, match no lexicon term. (Task 4 test `missing_title_counts_but_matches_nothing`.)
3. Bad names in `subreddits.txt` (`r/stocks`, `wall-street`, blank lines, `# comment`) → comments/blank ignored; invalid names rejected with a clear error before any HTTP request. (Task 5 test `parse_list_rejects_invalid_names`.)
4. `--from` after `--to`, or `--to` in the future → no requests for empty spans; `to` clamps to today UTC. (Task 3 test `plan_chunks_empty_and_future`.)
5. A day that was provisional last run and final now (or a run killed mid-week leaving page files outside the manifest) → aggregates count each post id once and ignore files not listed in a manifest. (Task 4 test `final_wins_over_provisional_and_orphans_ignored`.)

---

### Task 1: `Http::send_with` — body-level retry and rate-limit wait

**Files:**
- Modify: `crates/asterion-data/src/http.rs`
- Modify: `crates/asterion-data/src/error.rs`
- Modify: `crates/asterion-data/src/cache.rs` (make `write_atomic` `pub(crate)`)

**Interfaces:**
- Produces:
  - `pub(crate) enum BodyVerdict { Accept, Retry(String), Fatal(DataError) }`
  - `pub(crate) async fn send_with(&mut self, make: impl Fn(&reqwest::Client) -> reqwest::RequestBuilder, classify: impl Fn(&str) -> BodyVerdict) -> Result<String, DataError>`
  - `DataError::Busy { attempts: u32, message: String }` (Display: upstream busy after N attempts: message). `is_fatal_for_batch` returns `false` for it.
  - `pub(crate) fn write_atomic(path: &Path, bytes: &[u8]) -> Result<(), DataError>` in cache.rs.
- `send` becomes `self.send_with(make, |_| BodyVerdict::Accept)`; existing behaviour unchanged.

- [ ] **Step 1: Write failing unit tests in `http.rs` `#[cfg(test)] mod tests` (wiremock, `#[tokio::test]`)**

```rust
#[tokio::test]
async fn retry_verdict_retries_then_succeeds() {
    // mock: first response 200 `{"data":null,"error":"Timeout. Maybe slow down a bit"}` (up_to_n_times(1)),
    // then 200 `{"data":[]}`; classify = Retry when body contains "\"data\":null"
    assert_eq!(body, r#"{"data":[]}"#);
    assert_eq!(http.requests_sent(), 2);
}
#[tokio::test]
async fn retry_verdict_exhausted_is_busy() {
    // always data:null, max_attempts 2
    assert!(matches!(err, DataError::Busy { attempts: 2, .. }));
    assert!(!err.is_fatal_for_batch());
}
#[tokio::test]
async fn fatal_verdict_is_not_retried() {
    // classify returns Fatal(DataError::Api{..}); expect exactly 1 request
}
#[tokio::test]
async fn ratelimit_reset_header_extends_wait() {
    // 429 with header x-ratelimit-reset: 1, then 200; initial_backoff 1 ms
    assert!(start.elapsed() >= Duration::from_millis(900));
}
```

- [ ] **Step 2: Run** `cargo test -p asterion-data --lib http::tests` → FAIL (`send_with`/`Busy` undefined).

- [ ] **Step 3: Implement.** Retry wait = `max(backoff, x-ratelimit-reset seconds)` capped at 60 s; read the header from 429/5xx responses and from 2xx responses whose verdict is `Retry`. Header value is integer seconds; ignore if unparsable.

- [ ] **Step 4: Run** `cargo test -p asterion-data` → all PASS (existing tests unchanged).

- [ ] **Step 5: Stage** `git add crates/asterion-data/src/http.rs crates/asterion-data/src/error.rs crates/asterion-data/src/cache.rs`

---

### Task 2: `reddit::api` — post model, page parsing, client

**Files:**
- Create: `crates/asterion-data/src/reddit/mod.rs`, `crates/asterion-data/src/reddit/api.rs`
- Modify: `crates/asterion-data/src/lib.rs` (`pub mod reddit;`)
- Test: `crates/asterion-data/tests/reddit_collect.rs` (created here, extended in Task 3)

**Interfaces:**
- Produces (in `reddit::api`, re-exported from `reddit`):
  - `pub const SOURCE_ID: &str = "arctic_shift_reddit"; pub const PROVISIONAL_SOURCE_ID: &str = "arctic_shift_reddit_provisional"; pub const BASE_URL: &str = "https://arctic-shift.photon-reddit.com"; pub const DEFAULT_ROOT: &str = "data/raw/reddit"; pub const FIELDS: &str = "id,created_utc,title,selftext,score,num_comments,link_flair_text,over_18,retrieved_on"; pub const PAGE_LIMIT: usize = 100; pub const MIN_INTERVAL: Duration = Duration::from_secs(1);`
  - `#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)] pub struct RedditPost { pub id: String, #[serde(deserialize_with = ...)] pub created_utc: i64, #[serde(default)] pub title: Option<String>, #[serde(default)] pub selftext: Option<String>, #[serde(default)] pub score: Option<i64>, #[serde(default)] pub num_comments: Option<i64>, #[serde(default)] pub link_flair_text: Option<String>, #[serde(default)] pub over_18: Option<bool>, #[serde(default)] pub retrieved_on: Option<i64> }`
  - `pub fn parse_page(body: &str) -> Result<Vec<RedditPost>, DataError>` — accepts `{"data":[...]}`; `data:null` → `DataError::Api { code: "arctic_shift".into(), message: <error> }`.
  - `pub(crate) fn classify(body: &str) -> BodyVerdict` — `data` array → Accept; `data:null` with error containing `is not a valid field` or `must be between` → Fatal(Api); other `data:null` → Retry(error); unparsable JSON → Fatal(Json).
  - `pub fn sanitize_page(body: &str) -> Result<String, DataError>` — returns `{"data":[...]}` with every object key starting with `author` removed.
  - `#[derive(Debug)] pub struct ArcticClient` with `pub fn new(base: &str, policy: RetryPolicy) -> Result<Self, DataError>` (uses `Http::new("", policy)`), `pub fn requests_sent(&self) -> u32`, `pub async fn fetch_page(&mut self, subreddit: &str, after: i64, before: i64) -> Result<String, DataError>` (returns sanitized body; uses `send_with` + `classify`).

- [ ] **Step 1: Write failing tests**

Unit tests in `api.rs`:
```rust
#[test] fn created_utc_accepts_int_float_and_string() {
    // data entries with created_utc 1334162440, 1334162440.0, "1334162440"
    assert!(posts.iter().all(|p| p.created_utc == 1_334_162_440));
}
#[test] fn sanitize_strips_author_keys() {
    // input objects with author, author_fullname, author_flair_text, title
    assert!(!out.contains("author")); assert!(out.contains("\"title\""));
}
#[test] fn classify_verdicts() {
    assert!(matches!(classify(r#"{"data":[]}"#), BodyVerdict::Accept));
    assert!(matches!(classify(r#"{"data":null,"error":"Timeout. Maybe slow down a bit"}"#), BodyVerdict::Retry(_)));
    assert!(matches!(classify(r#"{"data":null,"error":"'stickied' is not a valid field"}"#), BodyVerdict::Fatal(_)));
}
```
Integration test in `tests/reddit_collect.rs`:
```rust
#[tokio::test]
async fn fetch_page_sends_expected_query_without_author() {
    // Mock matches path /api/posts/search and query_param subreddit=stocks, after=100, before=200,
    // limit=100, sort=asc, fields=FIELDS; response includes an "author" key.
    let body = client.fetch_page("stocks", 100, 200).await.expect("page");
    assert!(!body.contains("author"));
    let req = &server.received_requests().await.expect("recorded")[0];
    assert!(!req.url.query().unwrap_or_default().contains("author"));
}
#[tokio::test]
async fn http_400_fails_fast() { /* 400 → Err(HttpStatus{status:400}); exactly 1 request */ }
```

- [ ] **Step 2: Run** `cargo test -p asterion-data reddit` → FAIL (module missing).
- [ ] **Step 3: Implement** `reddit/mod.rs` (declares `api`, `collect`, `daily`; Tasks 3–4 fill the latter two, start them as empty files) and `api.rs`.
- [ ] **Step 4: Run** `cargo test -p asterion-data reddit` → PASS.
- [ ] **Step 5: Stage** the new and modified files.

---

### Task 3: `reddit::collect` — week chunks, pagination, final/provisional, truncation

**Files:**
- Create: `crates/asterion-data/src/reddit/collect.rs`
- Test: `crates/asterion-data/tests/reddit_collect.rs`

**Interfaces:**
- Consumes: `ArcticClient::fetch_page`, `parse_page`, `RawCache::{with_source, load_manifest_for, save_manifest_for, store_page_for}`, `data_go_kr::missing_ranges`, `write_atomic`.
- Produces:
  - `#[derive(Debug, Clone, Copy, PartialEq, Eq)] pub struct Chunk { pub begin: NaiveDate, pub end: NaiveDate, pub is_final: bool }`
  - `pub fn plan_chunks(covered: &[(NaiveDate, NaiveDate)], from: NaiveDate, to: NaiveDate, today: NaiveDate) -> Vec<Chunk>` — `to` clamped to `today`; missing ranges split at ISO-week boundaries; `is_final = end + 3 days <= today`.
  - `#[derive(Debug, Default, Clone, PartialEq)] pub struct RangeFetch { pub posts: Vec<RedditPost>, pub pages: Vec<String>, pub truncated_seconds: Vec<i64> }`
  - `pub async fn fetch_range(client: &mut ArcticClient, subreddit: &str, begin: NaiveDate, end: NaiveDate, budget: Option<u32>) -> Result<Option<RangeFetch>, DataError>` — `Ok(None)` when the budget runs out mid-range.
  - `#[derive(Debug, Default, Clone, PartialEq)] pub struct RedditOutcome { pub final_weeks: usize, pub provisional_days: usize, pub posts: usize, pub truncated_seconds: usize, pub budget_exhausted: bool }`
  - `pub async fn collect_subreddit(client: &mut ArcticClient, root: &Path, subreddit: &str, from: NaiveDate, to: NaiveDate, today: NaiveDate, budget: Option<u32>) -> Result<RedditOutcome, DataError>`
  - `pub fn final_cache(root: &Path) -> RawCache` (root, `SOURCE_ID`) and `pub fn provisional_cache(root: &Path) -> RawCache` (`root/provisional`, `PROVISIONAL_SOURCE_ID`).
  - `pub fn load_truncated(root: &Path, subreddit: &str) -> Result<Vec<i64>, DataError>` reading `<root>/<sub>/truncated.json` (`{"seconds":[...]}`; missing file → empty).

Pagination algorithm (`fetch_range`), `begin_ts = begin 00:00:00 UTC`, `end_ts = (end + 1 day) 00:00:00 UTC`:
```text
after = begin_ts - 1; seen = {}
loop:
  if budget reached: return None
  page = parse(fetch_page(sub, after, end_ts)); keep body
  new = page posts with id ∉ seen and begin_ts <= created_utc < end_ts; add to seen/posts
  if page.len() < PAGE_LIMIT: break
  max_ts = max created_utc in page
  if new.is_empty():
     if min_ts == max_ts: truncated_seconds.push(max_ts); after = max_ts   // skip the saturated second
     else: return Err(DataError::Api{code:"arctic_shift_no_progress", ..})
  else: after = max_ts - 1
  if after >= end_ts - 1: break
```

`collect_subreddit`:
- Final chunks first: `fetch_range`; on `Some`, store each page with `store_page_for(sub, begin, end, page_no, body, rows, Utc::now())`, push `FetchedRange { begin, end, pages }`, save manifest after each chunk, append truncated seconds to `truncated.json`. On `None`: set `budget_exhausted`, stop (nothing committed for that chunk).
- Provisional chunks only after every final chunk succeeded: fetch all provisional chunks into memory, then wipe `root/provisional/<sub>` and write them plus a fresh manifest. If any final or provisional fetch fails or the budget runs out, the previous provisional data stays untouched.
- `budget` counts `client.requests_sent()` against the cap.

- [ ] **Step 1: Write failing tests** (pure tests in `collect.rs`, HTTP tests in `tests/reddit_collect.rs`; wiremock responses chosen by the `after` query param)

```rust
#[test] fn plan_chunks_splits_by_iso_week_and_marks_final() {
    // from 2026-11-02 (Mon) to 2026-11-16, today 2026-11-16, nothing covered
    assert_eq!(chunks, vec![
        Chunk{begin: d(11,2),  end: d(11,8),  is_final: true},
        Chunk{begin: d(11,9),  end: d(11,15), is_final: false}, // 15+3 > 16
        Chunk{begin: d(11,16), end: d(11,16), is_final: false},
    ]);
}
#[test] fn plan_chunks_empty_and_future() {
    assert!(plan_chunks(&[], d(11,10), d(11,1), d(11,16)).is_empty());
    assert_eq!(plan_chunks(&[], d(11,16), d(12,31), d(11,16)).last().map(|c| c.end), Some(d(11,16)));
}
#[test] fn plan_chunks_skips_covered_weeks() { /* covered (11-02,11-08) → first chunk starts 11-09 */ }

#[tokio::test] async fn tie_second_is_refetched_and_deduped() {
    // page1: 100 posts, last 3 share ts T; page2 (after=T-1): those 3 again + 2 new, len 5
    assert_eq!(fetch.posts.len(), 102);
    assert_eq!(after_params, vec![begin_ts - 1, T - 1]);
}
#[tokio::test] async fn saturated_second_is_truncated_and_skipped() {
    // page1 and page2: same 100 posts all at ts T; page3 (after=T): 1 post
    assert_eq!(fetch.truncated_seconds, vec![T]); assert_eq!(fetch.posts.len(), 101);
}
#[tokio::test] async fn busy_body_retried_then_range_not_committed() {
    // always data:null Timeout → Err(Busy); manifest has no ranges
}
#[tokio::test] async fn failure_on_second_page_commits_nothing() { /* page2 → 500 forever; manifest empty; rerun with healthy mock commits the week */ }
#[tokio::test] async fn budget_exhausted_mid_week_commits_nothing() { /* budget Some(1) with 2-page week → outcome.budget_exhausted, manifest empty */ }
#[tokio::test] async fn recent_days_go_to_provisional_and_are_refetched() {
    // today 2026-11-16, from 11-09: manifest under root/stocks empty; provisional manifest has 11-09..11-15 and 11-16;
    // second run issues the requests again and provisional files are replaced, not appended
}
#[tokio::test] async fn failed_backfill_keeps_previous_provisional() {
    // run 1 healthy → provisional 11-09..11-16 stored; run 2 with an older final week returning 500 →
    // Err, and provisional manifest/files identical to run 1
}
#[tokio::test] async fn posts_outside_range_are_dropped() { /* created_utc == end_ts excluded, begin_ts included */ }
```

- [ ] **Step 2: Run** `cargo test -p asterion-data reddit` → FAIL.
- [ ] **Step 3: Implement** `collect.rs` per the interfaces and algorithm above.
- [ ] **Step 4: Run** `cargo test -p asterion-data reddit` → PASS.
- [ ] **Step 5: Stage** files.

---

### Task 4: `reddit::daily` — lexicon and `reddit_daily.json`

**Files:**
- Create: `crates/asterion-data/src/reddit/daily.rs`

**Interfaces:**
- Consumes: `final_cache`, `provisional_cache`, `load_truncated`, `parse_page`, `RawCache::load_bodies`, manifests' ranges.
- Produces:
  - `pub const LEXICON_VERSION: &str = "v1"; pub const DEFAULT_OUT: &str = "data/derived/reddit_daily.json";`
  - `pub const BULL_TERMS: &[&str] = &["call","calls","moon","buy","long","bull","bullish"]; pub const BEAR_TERMS: &[&str] = &["put","puts","crash","bear","bearish","sell","short"];` plus the emoji `"🚀"` counted as bull by substring.
  - `pub const KOREA_TERMS: &[&str] = &["kospi","samsung","hynix","hyundai","ewy"];` plus any token starting with `korea`.
  - `#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)] pub struct TitleTags { pub bull: bool, pub bear: bool, pub korea: bool }`
  - `pub fn tag_title(title: &str) -> TitleTags` — lowercase, split on non-alphanumeric chars, whole-token match; counts are per post (a title matches a side at most once).
  - `#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)] pub struct DailyRecord { pub subreddit: String, pub date: NaiveDate, pub post_count: u32, pub bull_count: u32, pub bear_count: u32, pub net_sentiment: f64, pub korea_mentions: u32, pub r#final: bool, pub truncated: bool, pub lexicon_version: String }` (serialize field name `final`).
  - `pub fn aggregate_subreddit(root: &Path, subreddit: &str) -> Result<Vec<DailyRecord>, DataError>` — one record per covered day (final + provisional ranges), zero rows included, sorted by date; posts deduped by id with final copies preferred; only manifest-listed pages read. Stored bodies are whole pages, so each body's posts are filtered to its range's `begin..=end` UTC days (as `fred::load_series` filters by `cached.begin/end`) before counting.
  - `pub fn write_daily(path: &Path, records: &[DailyRecord]) -> Result<(), DataError>` — JSON array, pretty, via `write_atomic`, creating parent dir.

- [ ] **Step 1: Write failing tests** (unit tests in `daily.rs`, fixtures built with `tempfile` + `RawCache::store_page_for` / `save_manifest_for`)

```rust
#[test] fn tag_title_whole_tokens() {
    assert_eq!(tag_title("YOLO calls on SPY 🚀"), TitleTags{bull: true, bear: false, korea: false});
    assert_eq!(tag_title("Shortage of chips"), TitleTags::default()); // "shortage" ≠ "short"
    assert_eq!(tag_title("Korean stocks & SK Hynix puts"), TitleTags{bull: false, bear: true, korea: true});
}
#[test] fn missing_title_counts_but_matches_nothing() { /* title None and "[deleted]" → post_count 2, bull 0, bear 0 */ }
#[test] fn utc_bucketing_boundaries() { /* 2026-11-10T23:59:59Z → 11-10; 2026-11-11T00:00:00Z → 11-11 */ }
#[test] fn zero_rows_for_covered_days_and_net_formula() {
    // week 11-02..11-08 covered, posts only on 11-03: 2 bull, 1 bear
    assert_eq!(records.len(), 7);
    assert_eq!(records[1].net_sentiment, (2.0 - 1.0) / (2.0 + 1.0 + 1.0));
    assert_eq!(records[0].post_count, 0);
}
#[test] fn final_wins_over_provisional_and_orphans_ignored() {
    // same id in final and provisional; extra page file not in manifest → counted once, orphan ignored
}
#[test] fn page_bodies_filtered_to_range() {
    // range 11-02..11-08 whose stored page also holds a post at 2026-11-09T00:00:00Z → not counted anywhere
}
#[test] fn flags_final_and_truncated() { /* provisional day → final=false; day of a truncated second → truncated=true */ }
```

- [ ] **Step 2: Run** `cargo test -p asterion-data reddit::daily` → FAIL.
- [ ] **Step 3: Implement** `daily.rs`.
- [ ] **Step 4: Run** `cargo test -p asterion-data` → PASS.
- [ ] **Step 5: Stage** files.

---

### Task 5: CLI subcommands, subreddit list, smoke run

**Files:**
- Modify: `crates/asterion-cli/src/main.rs`
- Create: `config/reddit/subreddits.txt` (lines: `wallstreetbets`, `stocks`, `investing`, `StockMarket`, `korea`, `KoreanStocks`)
- Modify: `crates/asterion-data/src/reddit/mod.rs` (add `parse_list`)

**Interfaces:**
- Consumes: `ArcticClient`, `collect_subreddit`, `aggregate_subreddit`, `write_daily`, `ProgressTracker`, `report_fetch`, `finish`, `parse_date`.
- Produces:
  - `pub fn parse_list(text: &str) -> Result<Vec<String>, DataError>` in `reddit` — strips `#…`, trims, skips blanks, rejects names not matching `[A-Za-z0-9_]{1,21}` with `DataError::InvalidField { field: "subreddit", .. }`.
  - `Command::FetchReddit(FetchRedditArgs)`: `--list` (default `config/reddit/subreddits.txt`), `--from` (default `2008-01-01`), `--to: Option<NaiveDate>`, `--root` (default `reddit::DEFAULT_ROOT`), `--max-requests: Option<u32>`. Progress kind `"fetch-reddit"`, stage `"fetching"` over subreddits; prints `"{sub} final_weeks={} provisional_days={} posts={} truncated={}"` and a final `requests=… failures=…` line. Stops the loop when budget is exhausted. Exit FAILURE if any subreddit failed.
  - `Command::RedditDaily(RedditDailyArgs)`: `--list`, `--root`, `--out` (default `reddit::DEFAULT_OUT`). Progress kind `"reddit-daily"`. Prints `records=N`.
  - `RetryPolicy { min_interval: reddit::MIN_INTERVAL, max_attempts: 5, initial_backoff: 2 s, timeout: 60 s }`.

- [ ] **Step 1: Write failing test** in `reddit/mod.rs`:
```rust
#[test] fn parse_list_rejects_invalid_names() {
    assert_eq!(parse_list("# c\nstocks\n\n KoreanStocks # kr\n").expect("ok"), vec!["stocks", "KoreanStocks"]);
    assert!(parse_list("r/stocks").is_err());
    assert!(parse_list("wall-street").is_err());
}
```
- [ ] **Step 2: Run** `cargo test -p asterion-data parse_list` → FAIL; implement; → PASS.
- [ ] **Step 3: Add the two subcommands** in `main.rs` following `fetch_macro` (match arm with `ProgressTracker::start(&dir, "fetch-reddit")` + `finish`). Every `about`/`help` text is a `__AGY__("Context: … | Intent: … | Tone: concise CLI help | Target Language: English")` placeholder.
- [ ] **Step 4a: Back up** `cp crates/asterion-cli/src/main.rs data/tmp/main.rs.bak` (file is uncommitted; git cannot restore it).
- [ ] **Step 4: Fill help strings** `just-code-claude --replace-in-file crates/asterion-cli/src/main.rs -m "4 hydrogen" -e high`; then `grep -c __AGY__ crates/asterion-cli/src/main.rs` → `0`, and `diff data/tmp/main.rs.bak crates/asterion-cli/src/main.rs` shows only the placeholder lines changed.
- [ ] **Step 5: Full verification** `cargo fmt --all -- --check && cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace` → all PASS.
- [ ] **Step 6: Live smoke** (one request per second; KIS jobs unaffected — different host):
  - `echo KoreanStocks > data/tmp/kr.txt && cargo run -p asterion-cli -- fetch-reddit --list data/tmp/kr.txt --from 2024-03-01 --to 2024-03-31` → exit 0, `failures=0`.
  - `cargo run -p asterion-cli -- reddit-daily --list data/tmp/kr.txt` → `records=31`.
  - Cross-check one day's `post_count` against a manual `curl` of the API with the same `after/before`.
- [ ] **Step 7: Peak measurement** `fetch-reddit` for `wallstreetbets` `--from 2021-01-25 --to 2021-01-31`; report requests, wall time, truncated seconds. Extrapolate full-backfill time for the owner. If this all-or-nothing peak week takes hours, stop and take cache granularity (week vs day) back to the owner before the full backfill.
- [ ] **Step 8: Stage** files; ask the owner before committing (commit message via `just-code-claude --commit`).
