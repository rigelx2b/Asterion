//! Per-subreddit UTC-day aggregates of post counts and title keywords.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs;
use std::path::Path;

use chrono::{DateTime, NaiveDate};
use serde::{Deserialize, Serialize};

use super::api::{RedditPost, parse_page};
use super::collect::{PROVISIONAL_DIR, final_cache, load_truncated, provisional_cache};
use crate::cache::{FetchedRange, RawCache, sha256_hex, write_atomic};
use crate::error::DataError;

pub const LEXICON_VERSION: &str = "v1";
pub const DEFAULT_OUT: &str = "data/derived/reddit_daily.json";
pub const BULL_TERMS: &[&str] = &["call", "calls", "moon", "buy", "long", "bull", "bullish"];
pub const BEAR_TERMS: &[&str] = &["put", "puts", "crash", "bear", "bearish", "sell", "short"];
pub const KOREA_TERMS: &[&str] = &["kospi", "samsung", "hynix", "hyundai", "ewy"];
const ROCKET: &str = "\u{1F680}";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct TitleTags {
    pub bull: bool,
    pub bear: bool,
    pub korea: bool,
}

/// Whole-token, case-insensitive matches; the rocket emoji counts as bullish.
pub fn tag_title(title: &str) -> TitleTags {
    let lower = title.to_lowercase();
    let tokens: Vec<&str> = lower
        .split(|c: char| !c.is_alphanumeric())
        .filter(|t| !t.is_empty())
        .collect();
    let any = |terms: &[&str]| tokens.iter().any(|t| terms.contains(t));
    TitleTags {
        bull: any(BULL_TERMS) || title.contains(ROCKET),
        bear: any(BEAR_TERMS),
        korea: any(KOREA_TERMS) || tokens.iter().any(|t| t.starts_with("korea")),
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DailyRecord {
    pub subreddit: String,
    pub date: NaiveDate,
    pub post_count: u32,
    pub bull_count: u32,
    pub bear_count: u32,
    pub net_sentiment: f64,
    pub korea_mentions: u32,
    #[serde(rename = "final")]
    pub is_final: bool,
    pub truncated: bool,
    pub lexicon_version: String,
}

fn utc_day(ts: i64) -> Option<NaiveDate> {
    DateTime::from_timestamp(ts, 0).map(|t| t.date_naive())
}

fn days_of(begin: NaiveDate, end: NaiveDate) -> impl Iterator<Item = NaiveDate> {
    begin.iter_days().take_while(move |d| *d <= end)
}

// Calls `visit` with each manifest-listed, hash-checked page of one range at a time,
// so memory stays bounded by a single page rather than the subreddit's history.
fn for_each_page(
    cache: &RawCache,
    subreddit: &str,
    range: &FetchedRange,
    mut visit: impl FnMut(Vec<RedditPost>),
) -> Result<(), DataError> {
    let dir = cache.root().join(subreddit);
    for page in &range.pages {
        let path = dir.join(&page.file);
        let bytes = fs::read(&path).map_err(|e| DataError::io(&path, e))?;
        if sha256_hex(&bytes) != page.provenance.raw_sha256 {
            return Err(DataError::CacheIntegrity {
                file: path.display().to_string(),
            });
        }
        visit(parse_page(&String::from_utf8_lossy(&bytes))?);
    }
    Ok(())
}

#[derive(Debug, Default, Clone, Copy)]
struct Tally {
    posts: u32,
    bull: u32,
    bear: u32,
    korea: u32,
}

impl Tally {
    fn add(&mut self, post: &RedditPost) {
        let tags = tag_title(post.title.as_deref().unwrap_or_default());
        self.posts += 1;
        self.bull += u32::from(tags.bull);
        self.bear += u32::from(tags.bear);
        self.korea += u32::from(tags.korea);
    }
}

/// One record per covered UTC day (final and provisional), zero-post days included.
/// Only manifest-listed pages are read and each post id counts once. Days covered
/// by a final range ignore provisional posts and flags, so final rows are reproducible.
pub fn aggregate_subreddit(root: &Path, subreddit: &str) -> Result<Vec<DailyRecord>, DataError> {
    let fcache = final_cache(root);
    let pcache = provisional_cache(root);
    let mut days: BTreeMap<NaiveDate, bool> = BTreeMap::new();
    let mut counts: HashMap<NaiveDate, Tally> = HashMap::new();

    // Final ranges are disjoint and a post has one creation day, so ids only
    // need deduping within a range.
    for range in fcache.load_manifest_for(subreddit)?.ranges {
        days.extend(days_of(range.begin, range.end).map(|d| (d, true)));
        let mut seen = HashSet::new();
        for_each_page(&fcache, subreddit, &range, |posts| {
            for post in posts {
                let Some(day) = utc_day(post.created_utc) else {
                    continue;
                };
                if day >= range.begin && day <= range.end && seen.insert(post.id.clone()) {
                    counts.entry(day).or_default().add(&post);
                }
            }
        })?;
    }

    let mut seen = HashSet::new();
    for range in pcache.load_manifest_for(subreddit)?.ranges {
        for day in days_of(range.begin, range.end) {
            days.entry(day).or_insert(false);
        }
        for_each_page(&pcache, subreddit, &range, |posts| {
            for post in posts {
                let Some(day) = utc_day(post.created_utc) else {
                    continue;
                };
                let fresh = days.get(&day) == Some(&false);
                if fresh && day >= range.begin && day <= range.end && seen.insert(post.id.clone()) {
                    counts.entry(day).or_default().add(&post);
                }
            }
        })?;
    }

    let mut truncated: HashSet<NaiveDate> = load_truncated(root, subreddit)?
        .into_iter()
        .filter_map(utc_day)
        .collect();
    truncated.extend(
        load_truncated(&root.join(PROVISIONAL_DIR), subreddit)?
            .into_iter()
            .filter_map(utc_day)
            .filter(|d| days.get(d) == Some(&false)),
    );

    Ok(days
        .into_iter()
        .map(|(date, is_final)| {
            let Tally {
                posts: post_count,
                bull,
                bear,
                korea,
            } = counts.get(&date).copied().unwrap_or_default();
            DailyRecord {
                subreddit: subreddit.to_owned(),
                date,
                post_count,
                bull_count: bull,
                bear_count: bear,
                net_sentiment: (f64::from(bull) - f64::from(bear))
                    / (f64::from(bull) + f64::from(bear) + 1.0),
                korea_mentions: korea,
                is_final,
                truncated: truncated.contains(&date),
                lexicon_version: LEXICON_VERSION.to_owned(),
            }
        })
        .collect())
}

/// Writes `records` as a pretty JSON array, creating the parent directory.
pub fn write_daily(path: &Path, records: &[DailyRecord]) -> Result<(), DataError> {
    let text = serde_json::to_string_pretty(records)?;
    write_atomic(path, text.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cache::FetchedRange;
    use crate::reddit::collect::{final_cache, provisional_cache};
    use chrono::{NaiveDate, Utc};
    use std::path::Path;

    const SUB: &str = "stocks";

    fn d(m: u32, day: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(2026, m, day).expect("valid date")
    }

    fn at(date: NaiveDate, h: u32, mi: u32, s: u32) -> i64 {
        date.and_hms_opt(h, mi, s)
            .expect("time")
            .and_utc()
            .timestamp()
    }

    fn body(posts: &[(&str, i64, Option<&str>)]) -> String {
        let items: Vec<serde_json::Value> = posts
            .iter()
            .map(|(id, t, title)| serde_json::json!({"id": id, "created_utc": t, "title": title}))
            .collect();
        serde_json::json!({ "data": items }).to_string()
    }

    // Stores one page for [begin, end] and records the range in the manifest.
    fn add_range(
        cache: &RawCache,
        begin: NaiveDate,
        end: NaiveDate,
        posts: &[(&str, i64, Option<&str>)],
    ) {
        let mut manifest = cache.load_manifest_for(SUB).expect("manifest");
        let page_no = u32::try_from(manifest.ranges.len() + 1).expect("small");
        let page = cache
            .store_page_for(
                SUB,
                begin,
                end,
                page_no,
                &body(posts),
                posts.len(),
                Utc::now(),
            )
            .expect("stored");
        manifest.ranges.push(FetchedRange {
            begin,
            end,
            pages: vec![page],
        });
        cache.save_manifest_for(SUB, &manifest).expect("saved");
    }

    fn agg(root: &Path) -> Vec<DailyRecord> {
        aggregate_subreddit(root, SUB).expect("aggregated")
    }

    #[test]
    fn tag_title_whole_tokens() {
        assert_eq!(
            tag_title("YOLO calls on SPY 🚀"),
            TitleTags {
                bull: true,
                bear: false,
                korea: false
            }
        );
        assert_eq!(tag_title("Shortage of chips"), TitleTags::default());
        assert_eq!(
            tag_title("Korean stocks & SK Hynix puts"),
            TitleTags {
                bull: false,
                bear: true,
                korea: true
            }
        );
        assert!(tag_title("EWY or KOSPI?").korea);
    }

    #[test]
    fn missing_title_counts_but_matches_nothing() {
        let tmp = tempfile::tempdir().expect("tmp");
        let day = d(11, 3);
        add_range(
            &final_cache(tmp.path()),
            day,
            day,
            &[
                ("a", at(day, 1, 0, 0), None),
                ("b", at(day, 2, 0, 0), Some("[deleted]")),
            ],
        );
        let r = agg(tmp.path());
        assert_eq!(r.len(), 1);
        assert_eq!(
            (r[0].post_count, r[0].bull_count, r[0].bear_count),
            (2, 0, 0)
        );
    }

    #[test]
    fn utc_bucketing_boundaries() {
        let tmp = tempfile::tempdir().expect("tmp");
        add_range(
            &final_cache(tmp.path()),
            d(11, 10),
            d(11, 11),
            &[
                ("a", at(d(11, 10), 23, 59, 59), Some("x")),
                ("b", at(d(11, 11), 0, 0, 0), Some("y")),
            ],
        );
        let r = agg(tmp.path());
        assert_eq!(
            r.iter().map(|x| (x.date, x.post_count)).collect::<Vec<_>>(),
            vec![(d(11, 10), 1), (d(11, 11), 1)]
        );
    }

    #[test]
    fn zero_rows_for_covered_days_and_net_formula() {
        let tmp = tempfile::tempdir().expect("tmp");
        let day = d(11, 3);
        add_range(
            &final_cache(tmp.path()),
            d(11, 2),
            d(11, 8),
            &[
                ("a", at(day, 1, 0, 0), Some("buy the dip")),
                ("b", at(day, 2, 0, 0), Some("calls calls calls")),
                ("c", at(day, 3, 0, 0), Some("crash incoming")),
            ],
        );
        let r = agg(tmp.path());
        assert_eq!(r.len(), 7);
        assert_eq!(r[0].post_count, 0);
        assert_eq!(r[0].net_sentiment, 0.0);
        assert_eq!((r[1].bull_count, r[1].bear_count), (2, 1));
        assert_eq!(r[1].net_sentiment, (2.0 - 1.0) / (2.0 + 1.0 + 1.0));
        assert!(
            r.iter()
                .all(|x| x.lexicon_version == LEXICON_VERSION && x.is_final)
        );
    }

    #[test]
    fn final_wins_over_provisional_and_orphans_ignored() {
        let tmp = tempfile::tempdir().expect("tmp");
        let day = d(11, 3);
        let fcache = final_cache(tmp.path());
        add_range(&fcache, day, day, &[("a", at(day, 1, 0, 0), Some("x"))]);
        add_range(
            &provisional_cache(tmp.path()),
            day,
            day,
            &[("a", at(day, 1, 0, 0), Some("x"))],
        );
        // A page left behind by an interrupted run is not in the manifest.
        fcache
            .store_page_for(
                SUB,
                day,
                day,
                9,
                &body(&[("z", at(day, 5, 0, 0), Some("x"))]),
                1,
                Utc::now(),
            )
            .expect("orphan");
        let r = agg(tmp.path());
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].post_count, 1);
        assert!(r[0].is_final);
    }

    #[test]
    fn page_bodies_filtered_to_range() {
        let tmp = tempfile::tempdir().expect("tmp");
        add_range(
            &final_cache(tmp.path()),
            d(11, 2),
            d(11, 8),
            &[
                ("a", at(d(11, 4), 1, 0, 0), Some("x")),
                ("b", at(d(11, 9), 0, 0, 0), Some("x")),
            ],
        );
        let r = agg(tmp.path());
        assert_eq!(r.iter().map(|x| x.post_count).sum::<u32>(), 1);
        assert!(r.iter().all(|x| x.date <= d(11, 8)));
    }

    #[test]
    fn final_days_ignore_stale_provisional_data() {
        let tmp = tempfile::tempdir().expect("tmp");
        let day = d(11, 3);
        add_range(
            &final_cache(tmp.path()),
            day,
            day,
            &[("a", at(day, 1, 0, 0), Some("x"))],
        );
        add_range(
            &provisional_cache(tmp.path()),
            day,
            day,
            &[("b", at(day, 2, 0, 0), Some("x"))],
        );
        let ppath = tmp
            .path()
            .join("provisional")
            .join(SUB)
            .join("truncated.json");
        std::fs::write(&ppath, format!(r#"{{"seconds":[{}]}}"#, at(day, 7, 0, 0))).expect("write");
        let r = agg(tmp.path());
        assert_eq!(
            r.iter()
                .map(|x| (x.date, x.post_count, x.is_final, x.truncated))
                .collect::<Vec<_>>(),
            vec![(day, 1, true, false)]
        );
    }

    #[test]
    fn flags_final_and_truncated() {
        let tmp = tempfile::tempdir().expect("tmp");
        let day = d(11, 3);
        add_range(&final_cache(tmp.path()), day, day, &[]);
        add_range(&provisional_cache(tmp.path()), d(11, 15), d(11, 15), &[]);
        let path = tmp.path().join(SUB).join("truncated.json");
        std::fs::write(&path, format!(r#"{{"seconds":[{}]}}"#, at(day, 7, 0, 0))).expect("write");
        let r = agg(tmp.path());
        assert_eq!(
            r.iter()
                .map(|x| (x.date, x.is_final, x.truncated))
                .collect::<Vec<_>>(),
            vec![(day, true, true), (d(11, 15), false, false)]
        );
    }

    #[test]
    fn write_daily_round_trips_with_final_key() {
        let tmp = tempfile::tempdir().expect("tmp");
        let day = d(11, 3);
        add_range(
            &final_cache(tmp.path()),
            day,
            day,
            &[("a", at(day, 1, 0, 0), Some("moon"))],
        );
        let records = agg(tmp.path());
        let out = tmp.path().join("derived").join("reddit_daily.json");
        write_daily(&out, &records).expect("written");
        let text = std::fs::read_to_string(&out).expect("read");
        assert!(text.contains("\"final\": true"), "{text}");
        let back: Vec<DailyRecord> = serde_json::from_str(&text).expect("parsed");
        assert_eq!(back, records);
    }
}
