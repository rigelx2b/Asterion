use std::fs;
use std::path::{Path, PathBuf};

use asterion_core::{Provenance, Symbol};
use chrono::{DateTime, NaiveDate, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::data_go_kr::{
    INDEX_SOURCE_ID, IndexQuote, IndexSpec, KrxDailyQuote, SOURCE_ID, parse_index_page, parse_page,
};
use crate::error::DataError;

pub const DEFAULT_ROOT: &str = "data/raw/data_go_kr";
pub const DEFAULT_INDEX_ROOT: &str = "data/raw/data_go_kr_index";
const MANIFEST_FILE: &str = "manifest.json";

/// Fetched date ranges for one symbol. A range is only recorded after every
/// page of it was stored, so a crash mid-range just causes a re-fetch.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Manifest {
    pub source: String,
    pub code: String,
    pub ranges: Vec<FetchedRange>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FetchedRange {
    pub begin: NaiveDate,
    pub end: NaiveDate,
    pub pages: Vec<PageRecord>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PageRecord {
    /// File name inside the symbol directory.
    pub file: String,
    pub page_no: u32,
    /// Rows for this symbol inside the range (after filtering).
    pub rows: usize,
    pub provenance: Provenance,
}

impl Manifest {
    pub fn new(source: &str, code: &str) -> Self {
        Manifest {
            source: source.to_owned(),
            code: code.to_owned(),
            ranges: Vec::new(),
        }
    }

    pub fn covered(&self) -> Vec<(NaiveDate, NaiveDate)> {
        self.ranges.iter().map(|r| (r.begin, r.end)).collect()
    }
}

/// Raw response cache: `<root>/<key>/<begin>_<end>_p<page>.json` + `manifest.json`.
/// `key` is a KRX short code, an index key or a FRED series id.
#[derive(Debug, Clone)]
pub struct RawCache {
    root: PathBuf,
    source: &'static str,
}

// One hash-checked cached response and the inclusive range it was fetched for.
#[derive(Debug, Clone, PartialEq)]
pub struct CachedBody {
    pub begin: NaiveDate,
    pub end: NaiveDate,
    pub body: String,
}

impl RawCache {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self::with_source(root, SOURCE_ID)
    }

    pub fn for_index(root: impl Into<PathBuf>) -> Self {
        Self::with_source(root, INDEX_SOURCE_ID)
    }

    pub fn with_source(root: impl Into<PathBuf>, source: &'static str) -> Self {
        RawCache {
            root: root.into(),
            source,
        }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    // Keys become path segments, so only ASCII alphanumerics and '_' are accepted.
    fn key_dir(&self, key: &str) -> Result<PathBuf, DataError> {
        let safe = !key.is_empty() && key.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
        if safe {
            Ok(self.root.join(key))
        } else {
            Err(DataError::InvalidField {
                field: "cache_key",
                value: key.to_owned(),
            })
        }
    }

    pub fn load_manifest(&self, symbol: &Symbol) -> Result<Manifest, DataError> {
        self.load_manifest_for(symbol.code())
    }

    pub fn save_manifest(&self, symbol: &Symbol, manifest: &Manifest) -> Result<(), DataError> {
        self.save_manifest_for(symbol.code(), manifest)
    }

    pub fn load_manifest_for(&self, key: &str) -> Result<Manifest, DataError> {
        let path = self.key_dir(key)?.join(MANIFEST_FILE);
        match fs::read_to_string(&path) {
            Ok(text) => Ok(serde_json::from_str(&text)?),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                Ok(Manifest::new(self.source, key))
            }
            Err(e) => Err(DataError::io(&path, e)),
        }
    }

    pub fn save_manifest_for(&self, key: &str, manifest: &Manifest) -> Result<(), DataError> {
        let path = self.key_dir(key)?.join(MANIFEST_FILE);
        let text = serde_json::to_string_pretty(manifest)?;
        write_atomic(&path, text.as_bytes())
    }

    #[allow(clippy::too_many_arguments)]
    pub fn store_page(
        &self,
        symbol: &Symbol,
        begin: NaiveDate,
        end: NaiveDate,
        page_no: u32,
        body: &str,
        rows: usize,
        fetched_at: DateTime<Utc>,
    ) -> Result<PageRecord, DataError> {
        self.store_page_for(symbol.code(), begin, end, page_no, body, rows, fetched_at)
    }

    #[allow(clippy::too_many_arguments)]
    pub fn store_page_for(
        &self,
        key: &str,
        begin: NaiveDate,
        end: NaiveDate,
        page_no: u32,
        body: &str,
        rows: usize,
        fetched_at: DateTime<Utc>,
    ) -> Result<PageRecord, DataError> {
        let file = format!(
            "{}_{}_p{page_no}.json",
            begin.format("%Y%m%d"),
            end.format("%Y%m%d")
        );
        write_atomic(&self.key_dir(key)?.join(&file), body.as_bytes())?;
        Ok(PageRecord {
            file,
            page_no,
            rows,
            provenance: Provenance {
                source: self.source.to_owned(),
                fetched_at,
                raw_sha256: sha256_hex(body.as_bytes()),
            },
        })
    }

    pub fn load_bodies(&self, key: &str) -> Result<Vec<CachedBody>, DataError> {
        let manifest = self.load_manifest_for(key)?;
        let dir = self.key_dir(key)?;
        let mut bodies = Vec::new();
        for range in &manifest.ranges {
            for page in &range.pages {
                let path = dir.join(&page.file);
                let bytes = fs::read(&path).map_err(|e| DataError::io(&path, e))?;
                if sha256_hex(&bytes) != page.provenance.raw_sha256 {
                    return Err(DataError::CacheIntegrity {
                        file: path.display().to_string(),
                    });
                }
                bodies.push(CachedBody {
                    begin: range.begin,
                    end: range.end,
                    body: String::from_utf8_lossy(&bytes).into_owned(),
                });
            }
        }
        Ok(bodies)
    }

    /// All cached rows for `symbol`, hash-checked and sorted by date.
    /// Duplicates are kept so validation can report them.
    pub fn load_quotes(&self, symbol: &Symbol) -> Result<Vec<KrxDailyQuote>, DataError> {
        let mut quotes = Vec::new();
        for cached in self.load_bodies(symbol.code())? {
            quotes.extend(parse_page(&cached.body)?.quotes.into_iter().filter(|q| {
                q.code == symbol.code() && q.date >= cached.begin && q.date <= cached.end
            }));
        }
        quotes.sort_by_key(|q| q.date);
        Ok(quotes)
    }

    // Cached levels of one index, sorted by date with duplicate dates removed.
    pub fn load_index_quotes(&self, index: &IndexSpec) -> Result<Vec<IndexQuote>, DataError> {
        let mut quotes = Vec::new();
        for cached in self.load_bodies(index.key)? {
            quotes.extend(
                parse_index_page(&cached.body)?
                    .quotes
                    .into_iter()
                    .filter(|q| {
                        q.name == index.name && q.date >= cached.begin && q.date <= cached.end
                    }),
            );
        }
        quotes.sort_by_key(|q| q.date);
        quotes.dedup_by_key(|q| q.date);
        Ok(quotes)
    }

    /// Codes that have a manifest under the cache root.
    pub fn cached_codes(&self) -> Result<Vec<String>, DataError> {
        let entries = match fs::read_dir(&self.root) {
            Ok(entries) => entries,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(DataError::io(&self.root, e)),
        };
        let mut codes = Vec::new();
        for entry in entries {
            let entry = entry.map_err(|e| DataError::io(&self.root, e))?;
            if entry.path().join(MANIFEST_FILE).is_file() {
                codes.push(entry.file_name().to_string_lossy().into_owned());
            }
        }
        codes.sort();
        Ok(codes)
    }
}

pub fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

pub(crate) fn write_atomic(path: &Path, bytes: &[u8]) -> Result<(), DataError> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir).map_err(|e| DataError::io(dir, e))?;
    }
    let tmp = path.with_extension("tmp");
    fs::write(&tmp, bytes).map_err(|e| DataError::io(&tmp, e))?;
    fs::rename(&tmp, path).map_err(|e| DataError::io(path, e))
}

#[cfg(test)]
mod tests {
    use asterion_core::Market;

    use super::*;

    fn date(y: i32, m: u32, d: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, d).expect("valid date")
    }

    const BODY: &str = r#"{"response":{"header":{"resultCode":"00","resultMsg":"OK"},"body":{"numOfRows":1000,"pageNo":1,"totalCount":2,"items":{"item":[
        {"basDt":"20240103","srtnCd":"005930","isinCd":"KR7005930003","itmsNm":"A","mrktCtg":"KOSPI","clpr":"77000","mkp":"78500","hipr":"78800","lopr":"77000","trqu":"1","trPrc":"1","lstgStCnt":"1","mrktTotAmt":"1"},
        {"basDt":"20240102","srtnCd":"005930","isinCd":"KR7005930003","itmsNm":"A","mrktCtg":"KOSPI","clpr":"78600","mkp":"77500","hipr":"79000","lopr":"76700","trqu":"1","trPrc":"1","lstgStCnt":"1","mrktTotAmt":"1"}
    ]}}}}"#;

    #[test]
    fn sha256_matches_known_vector() {
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn store_and_load_round_trip() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cache = RawCache::new(dir.path());
        let symbol = Symbol::new(Market::Kr, "005930").expect("symbol");

        assert!(
            cache
                .load_manifest(&symbol)
                .expect("empty")
                .ranges
                .is_empty()
        );
        assert!(cache.cached_codes().expect("codes").is_empty());

        let (b, e) = (date(2024, 1, 1), date(2024, 1, 31));
        let record = cache
            .store_page(&symbol, b, e, 1, BODY, 2, Utc::now())
            .expect("store");
        assert_eq!(record.file, "20240101_20240131_p1.json");

        let mut manifest = cache.load_manifest(&symbol).expect("manifest");
        manifest.ranges.push(FetchedRange {
            begin: b,
            end: e,
            pages: vec![record],
        });
        cache.save_manifest(&symbol, &manifest).expect("save");

        let quotes = cache.load_quotes(&symbol).expect("load");
        let dates: Vec<_> = quotes.iter().map(|q| q.date).collect();
        assert_eq!(dates, vec![date(2024, 1, 2), date(2024, 1, 3)]);
        assert_eq!(cache.cached_codes().expect("codes"), vec!["005930"]);
    }

    #[test]
    fn tampered_file_fails_integrity_check() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cache = RawCache::new(dir.path());
        let symbol = Symbol::new(Market::Kr, "005930").expect("symbol");
        let (b, e) = (date(2024, 1, 1), date(2024, 1, 31));
        let record = cache
            .store_page(&symbol, b, e, 1, BODY, 2, Utc::now())
            .expect("store");
        let mut manifest = Manifest::new(SOURCE_ID, "005930");
        manifest.ranges.push(FetchedRange {
            begin: b,
            end: e,
            pages: vec![record.clone()],
        });
        cache.save_manifest(&symbol, &manifest).expect("save");
        fs::write(dir.path().join("005930").join(&record.file), "{}").expect("tamper");

        assert!(matches!(
            cache.load_quotes(&symbol),
            Err(DataError::CacheIntegrity { .. })
        ));
    }

    #[test]
    fn unsafe_keys_are_rejected() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cache = RawCache::for_index(dir.path());
        for key in ["", "../x", "a/b", "코스피"] {
            assert!(matches!(
                cache.load_manifest_for(key),
                Err(DataError::InvalidField {
                    field: "cache_key",
                    ..
                })
            ));
        }
        let manifest = cache.load_manifest_for("kospi200").expect("empty manifest");
        assert_eq!(manifest.source, INDEX_SOURCE_ID);
    }
}
