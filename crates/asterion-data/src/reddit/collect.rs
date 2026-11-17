//! Resumable per-subreddit collection: final ISO weeks in the manifest, recent days provisional.

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

use chrono::{Datelike, Days, NaiveDate, Utc};
use serde::{Deserialize, Serialize};

use super::api::{
    ArcticClient, PAGE_LIMIT, PROVISIONAL_SOURCE_ID, RedditPost, SOURCE_ID, parse_page,
};
use crate::cache::{FetchedRange, PageRecord, RawCache, write_atomic};
use crate::data_go_kr::missing_ranges;
use crate::error::DataError;

// Arctic Shift re-scans posts about two days after ingest; days older than this are final.
const FINAL_AFTER_DAYS: u64 = 3;
pub(crate) const PROVISIONAL_DIR: &str = "provisional";
const STAGING_DIR: &str = "provisional_staging";
const MANIFEST_FILE: &str = "manifest.json";
const TRUNCATED_FILE: &str = "truncated.json";
const CHECKPOINT_FILE: &str = "checkpoint.json";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Chunk {
    pub begin: NaiveDate,
    pub end: NaiveDate,
    pub is_final: bool,
}

/// Uncovered parts of `[from, min(to, today)]`, split at ISO-week (Mon–Sun) boundaries.
pub fn plan_chunks(
    covered: &[(NaiveDate, NaiveDate)],
    from: NaiveDate,
    to: NaiveDate,
    today: NaiveDate,
) -> Vec<Chunk> {
    let to = to.min(today);
    if from > to {
        return Vec::new();
    }
    let mut chunks = Vec::new();
    for (begin, end) in missing_ranges(covered, from, to) {
        let mut cursor = begin;
        while cursor <= end {
            let to_sunday = u64::from(6 - cursor.weekday().num_days_from_monday());
            let week_end = cursor.checked_add_days(Days::new(to_sunday)).unwrap_or(end);
            let chunk_end = week_end.min(end);
            let is_final = chunk_end
                .checked_add_days(Days::new(FINAL_AFTER_DAYS))
                .is_some_and(|d| d <= today);
            chunks.push(Chunk {
                begin: cursor,
                end: chunk_end,
                is_final,
            });
            match chunk_end.succ_opt() {
                Some(next) => cursor = next,
                None => break,
            }
        }
    }
    chunks
}

/// Posts of one range plus each sanitized page body and its in-range row count.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct RangeFetch {
    pub posts: Vec<RedditPost>,
    pub pages: Vec<(String, usize)>,
    pub truncated_seconds: Vec<i64>,
}

fn day_start(d: NaiveDate) -> i64 {
    d.and_hms_opt(0, 0, 0)
        .map(|t| t.and_utc().timestamp())
        .unwrap_or_default()
}

/// `[begin_ts, end_ts)` epoch seconds of the UTC days `begin..=end`.
fn bounds(begin: NaiveDate, end: NaiveDate) -> (i64, i64) {
    (day_start(begin), end.succ_opt().map_or(i64::MAX, day_start))
}

/// Paging position inside one range; serializable so a week can resume mid-way.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct Cursor {
    after: i64,
    // Already-counted posts newer than `after`; the API only returns
    // `created_utc > after`, so older ids can never reappear.
    boundary: Vec<(String, i64)>,
    done: bool,
}

impl Cursor {
    fn start(begin_ts: i64) -> Self {
        // `after` is exclusive, so start one second early.
        Cursor {
            after: begin_ts - 1,
            boundary: Vec::new(),
            done: false,
        }
    }
}

struct Step {
    body: String,
    posts: Vec<RedditPost>,
    truncated: Option<i64>,
}

/// Fetches the page at `cursor` and advances it; `posts` are the new in-range ones.
async fn next_page(
    client: &mut ArcticClient,
    subreddit: &str,
    (begin_ts, end_ts): (i64, i64),
    cursor: &mut Cursor,
) -> Result<Step, DataError> {
    let body = client.fetch_page(subreddit, cursor.after, end_ts).await?;
    let page = parse_page(&body)?;
    let mut seen: HashSet<String> = cursor.boundary.iter().map(|(id, _)| id.clone()).collect();
    let mut posts = Vec::new();
    for post in &page {
        if (begin_ts..end_ts).contains(&post.created_utc) && seen.insert(post.id.clone()) {
            posts.push(post.clone());
        }
    }
    let mut truncated = None;
    if page.len() < PAGE_LIMIT {
        cursor.done = true;
    } else {
        let after = cursor.after;
        let max_ts = page.iter().map(|p| p.created_utc).max().unwrap_or(after);
        let min_ts = page.iter().map(|p| p.created_utc).min().unwrap_or(after);
        if !posts.is_empty() {
            // Re-read the boundary second: posts sharing it may straddle pages.
            cursor.after = max_ts - 1;
        } else if min_ts == max_ts {
            // 100+ posts in one second cannot be paged with second-granular `after`.
            truncated = Some(max_ts);
            cursor.after = max_ts;
        } else {
            return Err(DataError::Api {
                code: "arctic_shift_no_progress".to_owned(),
                message: format!("{subreddit} after={after}"),
            });
        }
        if cursor.after >= end_ts - 1 {
            cursor.done = true;
        }
    }
    let after = cursor.after;
    cursor.boundary.extend(
        posts
            .iter()
            .map(|p| (p.id.clone(), p.created_utc))
            .filter(|(_, ts)| *ts > after),
    );
    cursor.boundary.retain(|(_, ts)| *ts > after);
    Ok(Step {
        body,
        posts,
        truncated,
    })
}

/// Every post with `created_utc` in the UTC days `begin..=end`; `Ok(None)` if the
/// request budget runs out first.
pub async fn fetch_range(
    client: &mut ArcticClient,
    subreddit: &str,
    begin: NaiveDate,
    end: NaiveDate,
    budget: Option<u32>,
) -> Result<Option<RangeFetch>, DataError> {
    let range = bounds(begin, end);
    let mut cursor = Cursor::start(range.0);
    let mut out = RangeFetch::default();
    while !cursor.done {
        if budget.is_some_and(|b| client.requests_sent() >= b) {
            return Ok(None);
        }
        let step = next_page(client, subreddit, range, &mut cursor).await?;
        out.pages.push((step.body, step.posts.len()));
        out.posts.extend(step.posts);
        out.truncated_seconds.extend(step.truncated);
    }
    Ok(Some(out))
}

/// Pages of a final week stored so far; the week enters the manifest only when complete.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct Checkpoint {
    begin: NaiveDate,
    end: NaiveDate,
    cursor: Cursor,
    pages: Vec<PageRecord>,
    truncated_seconds: Vec<i64>,
}

fn checkpoint_path(root: &Path, subreddit: &str) -> PathBuf {
    root.join(subreddit).join(CHECKPOINT_FILE)
}

// A checkpoint of another range (e.g. `--from` changed) is ignored and later
// overwritten; its page files are not deleted because a committed week may own them.
fn load_checkpoint(root: &Path, subreddit: &str, chunk: Chunk) -> Result<Checkpoint, DataError> {
    let path = checkpoint_path(root, subreddit);
    let stored = match fs::read_to_string(&path) {
        Ok(text) => serde_json::from_str::<Checkpoint>(&text).ok(),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(DataError::io(&path, e)),
    };
    Ok(stored
        .filter(|c| (c.begin, c.end) == (chunk.begin, chunk.end))
        .unwrap_or_else(|| Checkpoint {
            begin: chunk.begin,
            end: chunk.end,
            cursor: Cursor::start(day_start(chunk.begin)),
            pages: Vec::new(),
            truncated_seconds: Vec::new(),
        }))
}

/// Fetches one final week page by page, checkpointing after each stored page.
/// `Ok(None)` if the budget runs out; the checkpoint lets the next run resume.
async fn fetch_final_week(
    client: &mut ArcticClient,
    cache: &RawCache,
    root: &Path,
    subreddit: &str,
    chunk: Chunk,
    budget: Option<u32>,
) -> Result<Option<Checkpoint>, DataError> {
    let mut cp = load_checkpoint(root, subreddit, chunk)?;
    let range = bounds(chunk.begin, chunk.end);
    let path = checkpoint_path(root, subreddit);
    while !cp.cursor.done {
        if budget.is_some_and(|b| client.requests_sent() >= b) {
            return Ok(None);
        }
        let step = next_page(client, subreddit, range, &mut cp.cursor).await?;
        let page_no = u32::try_from(cp.pages.len() + 1).unwrap_or(u32::MAX);
        cp.pages.push(cache.store_page_for(
            subreddit,
            chunk.begin,
            chunk.end,
            page_no,
            &step.body,
            step.posts.len(),
            Utc::now(),
        )?);
        cp.truncated_seconds.extend(step.truncated);
        // Page file first: a crash in between re-fetches this page into the same file.
        write_atomic(&path, serde_json::to_string(&cp)?.as_bytes())?;
    }
    Ok(Some(cp))
}

#[derive(Debug, Default, Clone, PartialEq)]
pub struct RedditOutcome {
    pub final_weeks: usize,
    pub provisional_days: usize,
    pub posts: usize,
    pub truncated_seconds: usize,
    pub budget_exhausted: bool,
}

pub fn final_cache(root: &Path) -> RawCache {
    RawCache::with_source(root, SOURCE_ID)
}

pub fn provisional_cache(root: &Path) -> RawCache {
    RawCache::with_source(root.join(PROVISIONAL_DIR), PROVISIONAL_SOURCE_ID)
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct Truncated {
    seconds: Vec<i64>,
}

fn truncated_path(root: &Path, subreddit: &str) -> PathBuf {
    root.join(subreddit).join(TRUNCATED_FILE)
}

/// Saturated seconds recorded under `<root>/<subreddit>`; empty when none.
pub fn load_truncated(root: &Path, subreddit: &str) -> Result<Vec<i64>, DataError> {
    let path = truncated_path(root, subreddit);
    match fs::read_to_string(&path) {
        Ok(text) => Ok(serde_json::from_str::<Truncated>(&text)?.seconds),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(e) => Err(DataError::io(&path, e)),
    }
}

fn add_truncated(root: &Path, subreddit: &str, seconds: &[i64]) -> Result<(), DataError> {
    if seconds.is_empty() {
        return Ok(());
    }
    let mut all = load_truncated(root, subreddit)?;
    all.extend_from_slice(seconds);
    all.sort_unstable();
    all.dedup();
    let text = serde_json::to_string(&Truncated { seconds: all })?;
    write_atomic(&truncated_path(root, subreddit), text.as_bytes())
}

fn store_range(
    cache: &RawCache,
    subreddit: &str,
    chunk: Chunk,
    fetch: &RangeFetch,
) -> Result<FetchedRange, DataError> {
    let now = Utc::now();
    let mut pages = Vec::with_capacity(fetch.pages.len());
    for (i, (body, rows)) in fetch.pages.iter().enumerate() {
        let page_no = u32::try_from(i + 1).unwrap_or(u32::MAX);
        pages.push(cache.store_page_for(
            subreddit,
            chunk.begin,
            chunk.end,
            page_no,
            body,
            *rows,
            now,
        )?);
    }
    Ok(FetchedRange {
        begin: chunk.begin,
        end: chunk.end,
        pages,
    })
}

fn days_in(chunk: Chunk) -> usize {
    usize::try_from((chunk.end - chunk.begin).num_days() + 1).unwrap_or_default()
}

/// Fetches uncovered final weeks (committed one by one), then replaces the
/// provisional days in one step. A failure or exhausted budget leaves the
/// previous provisional data untouched.
pub async fn collect_subreddit(
    client: &mut ArcticClient,
    root: &Path,
    subreddit: &str,
    from: NaiveDate,
    to: NaiveDate,
    today: NaiveDate,
    budget: Option<u32>,
) -> Result<RedditOutcome, DataError> {
    let cache = final_cache(root);
    let mut manifest = cache.load_manifest_for(subreddit)?;
    let chunks = plan_chunks(&manifest.covered(), from, to, today);
    let mut outcome = RedditOutcome::default();

    for chunk in chunks.iter().copied().filter(|c| c.is_final) {
        let Some(cp) = fetch_final_week(client, &cache, root, subreddit, chunk, budget).await?
        else {
            outcome.budget_exhausted = true;
            return Ok(outcome);
        };
        outcome.final_weeks += 1;
        outcome.posts += cp.pages.iter().map(|p| p.rows).sum::<usize>();
        outcome.truncated_seconds += cp.truncated_seconds.len();
        // Marker first: a crash after it only re-fetches the week; the reverse loses the flag.
        add_truncated(root, subreddit, &cp.truncated_seconds)?;
        manifest.ranges.push(FetchedRange {
            begin: chunk.begin,
            end: chunk.end,
            pages: cp.pages,
        });
        cache.save_manifest_for(subreddit, &manifest)?;
        // A leftover checkpoint of a committed week is ignored: its range is no longer planned.
        remove_file_if_present(&checkpoint_path(root, subreddit))?;
    }

    let mut provisional = Vec::new();
    for chunk in chunks.iter().copied().filter(|c| !c.is_final) {
        let Some(fetch) = fetch_range(client, subreddit, chunk.begin, chunk.end, budget).await?
        else {
            outcome.budget_exhausted = true;
            return Ok(outcome);
        };
        provisional.push((chunk, fetch));
    }
    if provisional.is_empty() {
        return Ok(outcome);
    }

    // Build the new provisional set beside the old one, then swap it in. The old
    // manifest goes first, so a partly failed delete never leaves it naming missing pages.
    let proot = root.join(PROVISIONAL_DIR);
    let staging_root = root.join(STAGING_DIR);
    let staging = staging_root.join(subreddit);
    remove_dir_if_present(&staging)?;
    let scache = RawCache::with_source(&staging_root, PROVISIONAL_SOURCE_ID);
    let mut pmanifest = scache.load_manifest_for(subreddit)?;
    for (chunk, fetch) in &provisional {
        pmanifest
            .ranges
            .push(store_range(&scache, subreddit, *chunk, fetch)?);
        add_truncated(&staging_root, subreddit, &fetch.truncated_seconds)?;
        outcome.provisional_days += days_in(*chunk);
        outcome.posts += fetch.posts.len();
        outcome.truncated_seconds += fetch.truncated_seconds.len();
    }
    scache.save_manifest_for(subreddit, &pmanifest)?;

    let target = proot.join(subreddit);
    remove_file_if_present(&target.join(MANIFEST_FILE))?;
    remove_dir_if_present(&target)?;
    fs::create_dir_all(&proot).map_err(|e| DataError::io(&proot, e))?;
    fs::rename(&staging, &target).map_err(|e| DataError::io(&target, e))?;
    Ok(outcome)
}

fn remove_file_if_present(path: &Path) -> Result<(), DataError> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(DataError::io(path, e)),
    }
}

fn remove_dir_if_present(dir: &Path) -> Result<(), DataError> {
    match fs::remove_dir_all(dir) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(DataError::io(dir, e)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn d(m: u32, day: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(2026, m, day).expect("valid date")
    }

    #[test]
    fn plan_chunks_splits_by_iso_week_and_marks_final() {
        let chunks = plan_chunks(&[], d(11, 2), d(11, 16), d(11, 16));
        assert_eq!(
            chunks,
            vec![
                Chunk {
                    begin: d(11, 2),
                    end: d(11, 8),
                    is_final: true
                },
                Chunk {
                    begin: d(11, 9),
                    end: d(11, 15),
                    is_final: false
                },
                Chunk {
                    begin: d(11, 16),
                    end: d(11, 16),
                    is_final: false
                },
            ]
        );
    }

    #[test]
    fn plan_chunks_empty_and_future() {
        assert!(plan_chunks(&[], d(11, 10), d(11, 1), d(11, 16)).is_empty());
        assert_eq!(
            plan_chunks(&[], d(11, 16), d(12, 31), d(11, 16))
                .last()
                .map(|c| c.end),
            Some(d(11, 16))
        );
    }

    #[test]
    fn plan_chunks_skips_covered_weeks() {
        let chunks = plan_chunks(&[(d(11, 2), d(11, 8))], d(11, 2), d(11, 16), d(11, 16));
        assert_eq!(chunks.first().map(|c| c.begin), Some(d(11, 9)));
    }

    #[test]
    fn plan_chunks_clips_partial_weeks() {
        // 2026-11-04 is a Wednesday; the first chunk runs to that week's Sunday.
        let chunks = plan_chunks(&[], d(11, 4), d(11, 10), d(11, 30));
        assert_eq!(
            chunks,
            vec![
                Chunk {
                    begin: d(11, 4),
                    end: d(11, 8),
                    is_final: true
                },
                Chunk {
                    begin: d(11, 9),
                    end: d(11, 10),
                    is_final: true
                },
            ]
        );
    }
}
