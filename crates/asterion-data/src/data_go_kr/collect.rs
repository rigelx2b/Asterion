use asterion_core::Symbol;
use chrono::{DateTime, Datelike, Days, FixedOffset, NaiveDate, Timelike, Utc, Weekday};

use super::client::{DataGoKrClient, PageRequest};
use super::index::{IndexSpec, parse_index_page};
use super::parse_page;
use crate::cache::{FetchedRange, RawCache};
use crate::error::DataError;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CollectOutcome {
    pub ranges_fetched: usize,
    pub pages: usize,
    pub rows: usize,
}

#[derive(Debug, Clone, Copy)]
enum Target<'a> {
    Stock(&'a str),
    Index(&'a IndexSpec),
}

struct PageSummary {
    page_no: u32,
    is_empty: bool,
    has_more: bool,
    // Dates of rows that belong to the target, before range filtering.
    dates: Vec<NaiveDate>,
}

impl Target<'_> {
    fn cache_key(&self) -> &str {
        match self {
            Target::Stock(code) => code,
            Target::Index(index) => index.key,
        }
    }

    async fn fetch(
        &self,
        client: &mut DataGoKrClient,
        begin: NaiveDate,
        end: NaiveDate,
        page_no: u32,
    ) -> Result<String, DataError> {
        match self {
            Target::Stock(code) => {
                let request = PageRequest {
                    code,
                    begin,
                    end,
                    page_no,
                };
                client.fetch_page(&request).await
            }
            Target::Index(index) => {
                client
                    .fetch_index_page(index.name, begin, end, page_no)
                    .await
            }
        }
    }

    fn summarize(&self, body: &str) -> Result<PageSummary, DataError> {
        match self {
            Target::Stock(code) => {
                let page = parse_page(body)?;
                Ok(PageSummary {
                    page_no: page.page_no,
                    is_empty: page.quotes.is_empty(),
                    has_more: page.has_more(),
                    // likeSrtnCd is a partial match, so keep only exact codes.
                    dates: page
                        .quotes
                        .iter()
                        .filter(|q| q.code == *code)
                        .map(|q| q.date)
                        .collect(),
                })
            }
            Target::Index(index) => {
                let page = parse_index_page(body)?;
                Ok(PageSummary {
                    page_no: page.page_no,
                    is_empty: page.quotes.is_empty(),
                    has_more: page.has_more,
                    dates: page
                        .quotes
                        .iter()
                        .filter(|q| q.name == index.name)
                        .map(|q| q.date)
                        .collect(),
                })
            }
        }
    }
}

/// Fetches whatever part of `[begin, end]` (clamped to `latest_available`) is
/// not cached yet. Already-cached ranges are never requested again.
pub async fn collect_symbol(
    client: &mut DataGoKrClient,
    cache: &RawCache,
    symbol: &Symbol,
    begin: NaiveDate,
    end: NaiveDate,
    latest_available: NaiveDate,
) -> Result<CollectOutcome, DataError> {
    let target = Target::Stock(symbol.code());
    collect_target(client, cache, target, begin, end, latest_available).await
}

// Same incremental contract as `collect_symbol`, for one market index.
pub async fn collect_index(
    client: &mut DataGoKrClient,
    cache: &RawCache,
    index: &IndexSpec,
    begin: NaiveDate,
    end: NaiveDate,
    latest_available: NaiveDate,
) -> Result<CollectOutcome, DataError> {
    let target = Target::Index(index);
    collect_target(client, cache, target, begin, end, latest_available).await
}

async fn collect_target(
    client: &mut DataGoKrClient,
    cache: &RawCache,
    target: Target<'_>,
    begin: NaiveDate,
    end: NaiveDate,
    latest_available: NaiveDate,
) -> Result<CollectOutcome, DataError> {
    let mut outcome = CollectOutcome::default();
    let end = end.min(latest_available);
    if begin > end {
        return Ok(outcome);
    }

    let key = target.cache_key();
    let mut manifest = cache.load_manifest_for(key)?;
    for (range_begin, range_end) in missing_ranges(&manifest.covered(), begin, end) {
        let mut pages = Vec::new();
        let mut last_row_date: Option<NaiveDate> = None;
        let mut page_no = 1;
        loop {
            let body = target
                .fetch(client, range_begin, range_end, page_no)
                .await?;
            let page = target.summarize(&body)?;
            if !page.is_empty && page.page_no != page_no {
                return Err(DataError::UnexpectedPage {
                    expected: page_no,
                    got: page.page_no,
                });
            }

            let rows: Vec<NaiveDate> = page
                .dates
                .into_iter()
                .filter(|d| *d >= range_begin && *d <= range_end)
                .collect();
            last_row_date = last_row_date.max(rows.iter().copied().max());
            outcome.rows += rows.len();

            pages.push(cache.store_page_for(
                key,
                range_begin,
                range_end,
                page_no,
                &body,
                rows.len(),
                Utc::now(),
            )?);
            if !page.has_more {
                break;
            }
            page_no += 1;
        }
        outcome.pages += pages.len();

        // Days before `latest_available` are final. For the newest days only
        // trust what was returned, so a late publication is picked up next run.
        let covered_end = if range_end < latest_available {
            Some(range_end)
        } else {
            last_row_date
        };
        if let Some(covered_end) = covered_end.filter(|d| *d >= range_begin) {
            manifest.ranges.push(FetchedRange {
                begin: range_begin,
                end: covered_end,
                pages,
            });
            cache.save_manifest_for(key, &manifest)?;
            outcome.ranges_fetched += 1;
        }
    }
    Ok(outcome)
}

/// Sub-ranges of `[begin, end]` not inside any of `covered` (all inclusive).
pub fn missing_ranges(
    covered: &[(NaiveDate, NaiveDate)],
    begin: NaiveDate,
    end: NaiveDate,
) -> Vec<(NaiveDate, NaiveDate)> {
    let mut covered: Vec<_> = covered.iter().copied().filter(|(b, e)| b <= e).collect();
    covered.sort();

    let mut missing = Vec::new();
    let mut cursor = begin;
    for (cb, ce) in covered {
        if cursor > end {
            break;
        }
        if ce < cursor {
            continue;
        }
        if cb > end {
            break;
        }
        if cb > cursor {
            missing.push((cursor, cb.pred_opt().unwrap_or(cb)));
        }
        match ce.succ_opt() {
            Some(next) => cursor = cursor.max(next),
            None => return missing,
        }
    }
    if cursor <= end {
        missing.push((cursor, end));
    }
    missing
}

/// Latest trading date whose data should be published. The portal publishes
/// day D after 13:00 KST on the next business day (plan §5). Public holidays
/// are not modelled; a too-early guess just returns no rows for that day.
pub fn latest_available_date(now: DateTime<Utc>) -> NaiveDate {
    let kst = FixedOffset::east_opt(9 * 3600).expect("+09:00 is a valid offset");
    let local = now.with_timezone(&kst);
    let today = local.date_naive();
    let publish_day = if is_weekday(today) && local.hour() >= 13 {
        today
    } else {
        prev_weekday(today)
    };
    prev_weekday(publish_day)
}

fn is_weekday(d: NaiveDate) -> bool {
    !matches!(d.weekday(), Weekday::Sat | Weekday::Sun)
}

fn prev_weekday(d: NaiveDate) -> NaiveDate {
    let mut d = d.checked_sub_days(Days::new(1)).unwrap_or(d);
    while !is_weekday(d) {
        d = d.checked_sub_days(Days::new(1)).unwrap_or(d);
    }
    d
}

#[cfg(test)]
mod tests {
    use chrono::TimeZone;

    use super::*;

    fn d(m: u32, day: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(2024, m, day).expect("valid date")
    }

    #[test]
    fn nothing_covered_means_whole_range() {
        assert_eq!(
            missing_ranges(&[], d(1, 1), d(1, 31)),
            vec![(d(1, 1), d(1, 31))]
        );
    }

    #[test]
    fn gaps_between_and_around_covered_ranges() {
        let covered = [(d(1, 10), d(1, 15)), (d(1, 3), d(1, 5))];
        assert_eq!(
            missing_ranges(&covered, d(1, 1), d(1, 31)),
            vec![(d(1, 1), d(1, 2)), (d(1, 6), d(1, 9)), (d(1, 16), d(1, 31))]
        );
    }

    #[test]
    fn fully_covered_and_overlapping() {
        let covered = [(d(1, 1), d(1, 20)), (d(1, 10), d(2, 10))];
        assert!(missing_ranges(&covered, d(1, 5), d(2, 1)).is_empty());
        assert_eq!(
            missing_ranges(&covered, d(1, 5), d(2, 15)),
            vec![(d(2, 11), d(2, 15))]
        );
        assert_eq!(
            missing_ranges(&[(d(3, 1), d(3, 5))], d(1, 1), d(1, 31)),
            vec![(d(1, 1), d(1, 31))]
        );
    }

    fn kst(m: u32, day: u32, hour: u32) -> DateTime<Utc> {
        FixedOffset::east_opt(9 * 3600)
            .expect("offset")
            .with_ymd_and_hms(2024, m, day, hour, 0, 0)
            .single()
            .expect("valid time")
            .with_timezone(&Utc)
    }

    #[test]
    fn latest_available_follows_next_business_day_13h_rule() {
        // 2024-01-08 is a Monday.
        assert_eq!(
            latest_available_date(kst(1, 8, 14)),
            d(1, 5),
            "Mon after 13h -> Fri"
        );
        assert_eq!(
            latest_available_date(kst(1, 8, 12)),
            d(1, 4),
            "Mon before 13h -> Thu"
        );
        assert_eq!(
            latest_available_date(kst(1, 10, 13)),
            d(1, 9),
            "Wed 13h -> Tue"
        );
        assert_eq!(latest_available_date(kst(1, 6, 15)), d(1, 4), "Sat -> Thu");
        assert_eq!(latest_available_date(kst(1, 7, 9)), d(1, 4), "Sun -> Thu");
    }
}
