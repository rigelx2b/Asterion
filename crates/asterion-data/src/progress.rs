//! Run status snapshots written by long commands and polled by the monitor dashboard.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

pub const DEFAULT_DIR: &str = "data/status";
// Minimum spacing between writes triggered by fine-grained step updates.
const STEP_WRITE_INTERVAL: Duration = Duration::from_millis(1000);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunState {
    Running,
    Completed,
    Failed,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EpochPoint {
    pub epoch: u32,
    pub train_loss: f64,
    pub val_accuracy: Option<f64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RunStatus {
    pub kind: String,
    pub state: RunState,
    pub pid: u32,
    pub started_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub stage: String,
    pub stage_started_at: DateTime<Utc>,
    // Units of the current stage (symbols, epochs, ...).
    pub done: u64,
    pub total: u64,
    // Progress inside the unit in flight (batches of the running epoch).
    pub step_done: u64,
    pub step_total: u64,
    pub current: String,
    pub counters: BTreeMap<String, f64>,
    pub epochs: Vec<EpochPoint>,
    pub tables: Vec<serde_json::Value>,
    pub error: Option<String>,
}

// Writing is best effort: a failed status write must never abort the run it describes.
#[derive(Debug)]
pub struct ProgressTracker {
    path: Option<PathBuf>,
    status: RunStatus,
    last_write: Option<Instant>,
}

impl ProgressTracker {
    // Tracks a run of `kind` in `<dir>/<kind>.json`, replacing the previous run's snapshot.
    pub fn start(dir: &Path, kind: &str) -> Self {
        let mut tracker = Self::build(Some(dir.join(format!("{kind}.json"))), kind);
        tracker.flush();
        tracker
    }

    // Keeps status in memory only.
    pub fn disabled(kind: &str) -> Self {
        Self::build(None, kind)
    }

    fn build(path: Option<PathBuf>, kind: &str) -> Self {
        let now = Utc::now();
        ProgressTracker {
            path,
            status: RunStatus {
                kind: kind.to_owned(),
                state: RunState::Running,
                pid: std::process::id(),
                started_at: now,
                updated_at: now,
                stage: String::new(),
                stage_started_at: now,
                done: 0,
                total: 0,
                step_done: 0,
                step_total: 0,
                current: String::new(),
                counters: BTreeMap::new(),
                epochs: Vec::new(),
                tables: Vec::new(),
                error: None,
            },
            last_write: None,
        }
    }

    pub fn status(&self) -> &RunStatus {
        &self.status
    }

    pub fn stage(&mut self, stage: &str, total: u64) {
        self.status.stage = stage.to_owned();
        self.status.stage_started_at = Utc::now();
        self.status.done = 0;
        self.status.total = total;
        self.status.step_done = 0;
        self.status.step_total = 0;
        self.status.current.clear();
        self.flush();
    }

    // Names the unit about to be worked on without counting it as done.
    pub fn current(&mut self, current: &str) {
        self.status.current = current.to_owned();
        self.flush_throttled();
    }

    pub fn advance(&mut self, done: u64) {
        self.status.done = done;
        self.status.step_done = 0;
        self.flush_throttled();
    }

    pub fn step(&mut self, done: u64, total: u64) {
        self.status.step_done = done;
        self.status.step_total = total;
        self.flush_throttled();
    }

    pub fn counter(&mut self, name: &str, value: f64) {
        self.status.counters.insert(name.to_owned(), value);
    }

    pub fn epoch(&mut self, point: EpochPoint) {
        self.status.epochs.push(point);
        self.flush();
    }

    pub fn table<T: Serialize>(&mut self, table: &T) {
        if let Ok(value) = serde_json::to_value(table) {
            self.status.tables.push(value);
            self.flush();
        }
    }

    pub fn finish(&mut self, error: Option<String>) {
        self.status.state = if error.is_some() {
            RunState::Failed
        } else {
            RunState::Completed
        };
        self.status.error = error;
        self.flush();
    }

    fn flush_throttled(&mut self) {
        if self
            .last_write
            .is_none_or(|at| at.elapsed() >= STEP_WRITE_INTERVAL)
        {
            self.flush();
        }
    }

    pub fn flush(&mut self) {
        self.status.updated_at = Utc::now();
        self.last_write = Some(Instant::now());
        let Some(path) = &self.path else {
            return;
        };
        let _ = write_atomically(path, &self.status);
    }
}

impl Drop for ProgressTracker {
    // A tracker dropped mid-run means the command bailed out early.
    fn drop(&mut self) {
        if self.status.state == RunState::Running {
            self.finish(Some(String::new()));
        }
    }
}

// Readers never observe a half-written file: write a sibling, then rename over the target.
fn write_atomically(path: &Path, status: &RunStatus) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("json.tmp");
    fs::write(&tmp, serde_json::to_vec(status)?)?;
    fs::rename(&tmp, path)
}

// Every readable snapshot in `dir`; a missing directory is an empty list.
pub fn load_all(dir: &Path) -> Vec<RunStatus> {
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut runs: Vec<RunStatus> = entries
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "json"))
        .filter_map(|path| fs::read(path).ok())
        .filter_map(|bytes| serde_json::from_slice(&bytes).ok())
        .collect();
    runs.sort_by(|a, b| a.kind.cmp(&b.kind));
    runs
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshot_round_trips_through_the_status_directory() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut tracker = ProgressTracker::start(dir.path(), "train");
        tracker.stage("training", 50);
        tracker.counter("samples_train", 12.0);
        tracker.epoch(EpochPoint {
            epoch: 1,
            train_loss: 0.5,
            val_accuracy: None,
        });
        tracker.advance(1);
        tracker.flush();

        let runs = load_all(dir.path());
        assert_eq!(runs.len(), 1);
        assert_eq!(&runs[0], tracker.status());
        assert_eq!(runs[0].state, RunState::Running);
        assert_eq!((runs[0].done, runs[0].total), (1, 50));
        assert!(!dir.path().join("train.json.tmp").exists());
    }

    #[test]
    fn finish_and_early_drop_record_the_outcome() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut ok = ProgressTracker::start(dir.path(), "fetch");
        ok.finish(None);
        drop(ProgressTracker::start(dir.path(), "train"));

        let runs = load_all(dir.path());
        assert_eq!(runs[0].state, RunState::Completed);
        assert_eq!(runs[0].error, None);
        assert_eq!(runs[1].state, RunState::Failed);
    }

    #[test]
    fn disabled_tracker_and_missing_directory_touch_no_files() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut tracker = ProgressTracker::disabled("fetch");
        tracker.stage("fetching", 3);
        tracker.finish(None);
        assert!(load_all(dir.path()).is_empty());
        assert!(load_all(&dir.path().join("absent")).is_empty());
    }
}
