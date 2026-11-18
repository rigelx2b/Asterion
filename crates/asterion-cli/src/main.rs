//! `asterion` command-line entry point.

use std::collections::BTreeMap;
use std::error::Error;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use asterion_core::{Market, Symbol};
use asterion_data::DataError;
use asterion_data::dart_financials::{self, Period};
use asterion_data::data_go_kr::{
    ATTRIBUTION, ClientConfig, DataGoKrClient, INDICES, SOURCE_ID, collect_index, collect_symbol,
    fetch_snapshot, latest_available_date, missing_ranges, select_top_by_market_cap,
};
use asterion_data::fred::{self, FredClient, FredKey};
use asterion_data::kis::{self, KisClient, KisKeys};
use asterion_data::opendart::{self, DartClient, DartKey};
use asterion_data::reddit::{self, ArcticClient};
use asterion_data::validate::{Issue, missing_dates, validate_bars};
use asterion_data::{ProgressTracker, RawCache, RetryPolicy, ServiceKey, cache, progress};
use chrono::{NaiveDate, Utc};
use clap::{Args, Parser, Subcommand};

mod monitor;

type CliResult<T> = Result<T, Box<dyn Error>>;

#[derive(Debug, Parser)]
#[command(
    name = "asterion",
    version,
    about = "Collect and validate Korean daily stock prices from data.go.kr"
)]
struct Cli {
    #[arg(long, global = true, default_value = progress::DEFAULT_DIR, help = "Directory where run status snapshots are written and read")]
    status_dir: PathBuf,

    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    #[command(about = "Fetch uncached daily KRX stock prices from data.go.kr into raw cache")]
    Fetch(FetchArgs),
    #[command(
        about = "Validate cached prices for OHLC consistency, gaps, halts, and abnormal moves"
    )]
    Validate(ValidateArgs),
    #[command(
        about = "Train and evaluate multi-horizon deep neural network and statistical baselines"
    )]
    Train(TrainArgs),
    #[command(
        about = "Write universe file of top KOSPI/KOSDAQ common shares by market cap on a trading date"
    )]
    Universe(UniverseBuildArgs),
    #[command(
        about = "Fetch uncached daily KOSPI, KOSDAQ and KOSPI 200 index levels from data.go.kr into raw cache"
    )]
    FetchIndex(FetchIndexArgs),
    #[command(
        about = "Fetch uncached daily US market and macro series from the FRED API into raw cache"
    )]
    FetchMacro(FetchMacroArgs),
    #[command(about = "Start a local web dashboard to monitor live fetch and training progress")]
    Monitor(MonitorArgs),
    #[command(about = "Fetch uncached corporate disclosure filings from OpenDART into raw cache")]
    FetchDisclosures(FetchDisclosuresArgs),
    #[command(
        about = "Fetch uncached OpenDART financial statements into raw cache, key accounts first then full statements"
    )]
    FetchFinancials(FetchFinancialsArgs),
    #[command(
        about = "Forecast prices at every horizon from latest cached session and explain each with feature contributions"
    )]
    Predict(PredictArgs),
    #[command(
        about = "Score a saved, frozen model on the held-out test period using its saved scaler and calibration"
    )]
    Evaluate(EvaluateArgs),
    #[command(
        about = "Fit selective cutoffs and intervals on validation B, update model, and report net-of-cost test profit"
    )]
    Confidence(ConfidenceArgs),
    #[command(
        about = "Fetch today's per-stock investor flows (retail, foreign, institutional) from the KIS Open API"
    )]
    FetchInvestors(FetchInvestorsArgs),
    #[command(
        about = "Record today's avoid-the-bottom-decile picks of a frozen model in its write-once ledger"
    )]
    LedgerRecord(LedgerRecordArgs),
    #[command(
        about = "Score matured forward-test ledger entries against cached prices and report pass criteria"
    )]
    LedgerScore(LedgerScoreArgs),
    #[command(
        about = "Run purged walk-forward cross-validation comparing ranking models by top-decile long-only net returns"
    )]
    ResearchCv(ResearchCvArgs),
    #[command(
        about = "Fit four ranking arms on labelled history, freeze by fingerprint, and register paper hypothesis"
    )]
    RankFit(RankLedgerArgs),
    #[command(
        about = "Fetch uncached historical daily bars per stock from KIS daily chart API into raw cache (resumable)"
    )]
    FetchHistory(FetchHistoryArgs),
    #[command(
        about = "Fetch overseas daily price bars from KIS into raw cache with resumable sync and newest-page refresh"
    )]
    FetchOverseas(FetchOverseasArgs),
    #[command(
        about = "Record today's top-decile picks for each frozen ranking arm in the write-once paper ledger"
    )]
    RankRecord(RankLedgerArgs),
    #[command(
        about = "Score matured paper-ledger entries against realised prices, the universe, KOSPI, and the control arm"
    )]
    RankScore(RankLedgerArgs),
    #[command(
        about = "Fetch Reddit submissions from Arctic Shift into raw cache with resumable sync and 3-day provisional refresh"
    )]
    FetchReddit(FetchRedditArgs),
    #[command(
        about = "Rebuild per-subreddit UTC-day post count, sentiment, and Korea mention JSON from cached Reddit posts"
    )]
    RedditDaily(RedditDailyArgs),
}

#[derive(Debug, Args)]
struct FetchRedditArgs {
    #[arg(
        long,
        default_value = "config/reddit/subreddits.txt",
        help = "File with one subreddit name per line (text after # ignored)"
    )]
    list: PathBuf,

    #[arg(long, value_parser = parse_date, default_value = "2008-01-01", help = "Earliest UTC date to fetch, inclusive")]
    from: NaiveDate,

    #[arg(long, value_parser = parse_date, help = "Latest UTC date to fetch, inclusive (defaults to UTC today, max today; last 3 days provisional)")]
    to: Option<NaiveDate>,

    #[arg(long, default_value = reddit::DEFAULT_ROOT, help = "Directory for raw Arctic Shift Reddit responses and per-subreddit manifests")]
    root: PathBuf,

    #[arg(
        long,
        help = "Stop after this many API requests (default: no limit); unfinished weeks resume from last saved page"
    )]
    max_requests: Option<u32>,
}

#[derive(Debug, Args)]
struct RedditDailyArgs {
    #[arg(
        long,
        default_value = "config/reddit/subreddits.txt",
        help = "File with one subreddit name per line (text after # ignored)"
    )]
    list: PathBuf,

    #[arg(long, default_value = reddit::DEFAULT_ROOT, help = "Directory of cached Arctic Shift Reddit responses to aggregate")]
    root: PathBuf,

    #[arg(long, default_value = reddit::daily::DEFAULT_OUT, help = "Output JSON file (fully rewritten with daily aggregates)")]
    out: PathBuf,
}

#[derive(Debug, Args)]
struct FetchHistoryArgs {
    #[command(flatten)]
    universe: UniverseArgs,

    #[arg(long, value_parser = parse_date, default_value = "2010-01-01", help = "Earliest date to fetch, inclusive")]
    from: NaiveDate,

    #[arg(long, value_parser = parse_date, default_value = "2020-06-30", help = "Latest date to fetch, inclusive; overlaps data.go.kr for cross-checking")]
    to: NaiveDate,

    #[arg(
        long,
        default_value = "0",
        help = "Value for KIS FID_ORG_ADJ_PRC adjustment flag (0 or 1); cached separately"
    )]
    adjusted: String,

    #[arg(long, default_value = kis::HISTORY_ROOT, help = "Directory for raw KIS daily chart responses")]
    root: PathBuf,

    #[arg(long, help = "Stop after sending this many API requests")]
    max_requests: Option<u32>,
}

#[derive(Debug, Args)]
struct FetchOverseasArgs {
    #[arg(
        long,
        default_value = "config/universe/us-core.txt",
        help = "File listing EXCHANGE:SYMBOL per line (NAS, NYS, AMS); comments after # ignored"
    )]
    list: PathBuf,

    #[arg(long, value_parser = parse_date, default_value = "2010-01-01", help = "Earliest date to fetch, inclusive")]
    from: NaiveDate,

    #[arg(long, value_parser = parse_date, help = "Latest date to fetch, inclusive (defaults to today in KST)")]
    to: Option<NaiveDate>,

    #[arg(
        long,
        default_value = "1",
        help = "KIS MODP flag (1 for split-adjusted, 0 for raw); cached separately"
    )]
    adjusted: String,

    #[arg(long, default_value = kis::OVERSEAS_ROOT, help = "Directory for raw KIS overseas daily price responses")]
    root: PathBuf,
}

#[derive(Debug, Args)]
struct RankLedgerArgs {
    #[command(flatten)]
    universe: UniverseArgs,

    #[command(flatten)]
    context: ContextArgs,

    #[arg(
        long,
        default_value = asterion_model::rank_ledger::DEFAULT_DIR,
        help = "Directory holding paper ledger hypothesis, frozen models, and daily entries"
    )]
    dir: PathBuf,
}

#[derive(Debug, Args)]
struct ResearchCvArgs {
    #[command(flatten)]
    universe: UniverseArgs,

    #[command(flatten)]
    context: ContextArgs,

    #[arg(
        long,
        default_value_t = 30,
        help = "Calendar days dropped from the end of each training window before its test window"
    )]
    embargo_days: u64,

    #[arg(
        long,
        help = "Score the 2025-onward window for reporting only, never for selection"
    )]
    report_burnt: bool,

    #[arg(
        long,
        help = "Run post-hoc diagnostics on the saved protocol only; never for selection"
    )]
    diagnostic: bool,

    #[arg(
        long,
        help = "Prepend cached KIS daily bars from fetch-history, rescaled onto data.go.kr series via 2020 overlap"
    )]
    with_history: bool,

    #[arg(
        long,
        default_value = kis::HISTORY_ROOT,
        help = "Directory of KIS daily chart responses from fetch-history (only read with --with-history)"
    )]
    history_root: PathBuf,

    #[arg(
        long,
        value_parser = parse_date,
        default_value = "2015-01-01",
        help = "Earliest sample date with --with-history; older bars warm up features (disclosures/financials start 2015)"
    )]
    history_from: NaiveDate,

    #[arg(
        long,
        help = "Allow running with an incomplete KIS history cache (smoke tests only)"
    )]
    allow_partial_history: bool,

    #[arg(
        long,
        default_value = "data/ledger/cv_2026-11-13/report.json",
        help = "Output path for the JSON cross-validation report"
    )]
    out: PathBuf,
}

#[derive(Debug, Args)]
struct ContextArgs {
    #[arg(long, default_value = cache::DEFAULT_INDEX_ROOT, help = "Directory with cached index levels for market features")]
    index_cache_dir: PathBuf,

    #[arg(long, default_value = fred::DEFAULT_ROOT, help = "Directory with cached FRED series for overseas market and macro features")]
    macro_cache_dir: PathBuf,

    #[arg(long, default_value = opendart::DEFAULT_ROOT, help = "Directory with cached OpenDART filing lists for disclosure event features")]
    disclosure_cache_dir: PathBuf,

    #[arg(long, default_value = dart_financials::DEFAULT_ROOT, help = "Directory with cached OpenDART financial statements for fundamental features")]
    financial_cache_dir: PathBuf,
}

impl ContextArgs {
    fn load(&self) -> CliResult<asterion_model::MarketContext> {
        Ok(asterion_model::MarketContext::load(
            &RawCache::for_index(&self.index_cache_dir),
            &RawCache::with_source(&self.macro_cache_dir, fred::SOURCE_ID),
            &RawCache::with_source(&self.disclosure_cache_dir, opendart::SOURCE_ID),
            &RawCache::with_source(&self.financial_cache_dir, dart_financials::SOURCE_ID),
        )?)
    }
}

#[derive(Debug, Args)]
struct EvaluateArgs {
    #[command(flatten)]
    universe: UniverseArgs,

    #[command(flatten)]
    context: ContextArgs,

    #[arg(
        long,
        default_value = "checkpoints/final_model.json",
        help = "Path to the trained model JSON file to load"
    )]
    model: PathBuf,
}

#[derive(Debug, Args)]
struct FetchInvestorsArgs {
    #[command(flatten)]
    universe: UniverseArgs,

    #[arg(long, default_value = kis::DEFAULT_ROOT, help = "Directory for raw KIS investor-flow responses (one file per stock per fetch day)")]
    root: PathBuf,

    #[arg(
        long,
        help = "Maximum number of API requests to send before stopping (default: no limit)"
    )]
    max_requests: Option<u32>,
}

#[derive(Debug, Args)]
struct LedgerRecordArgs {
    #[command(flatten)]
    universe: UniverseArgs,

    #[command(flatten)]
    context: ContextArgs,

    #[arg(
        long,
        default_value = "checkpoints/final_model.json",
        help = "Path to the frozen model JSON whose picks are recorded"
    )]
    model: PathBuf,

    #[arg(long, default_value = asterion_model::ledger::DEFAULT_ROOT, help = "Root directory of forward-test ledgers, containing per-model subdirectories")]
    ledger_root: PathBuf,
}

#[derive(Debug, Args)]
struct LedgerScoreArgs {
    #[arg(long, default_value = cache::DEFAULT_ROOT, help = "Directory for raw data.go.kr price responses used to compute realised returns")]
    cache_dir: PathBuf,

    #[arg(
        long,
        default_value = "checkpoints/final_model.json",
        help = "Path to the frozen model JSON whose ledger is scored"
    )]
    model: PathBuf,

    #[arg(long, default_value = asterion_model::ledger::DEFAULT_ROOT, help = "Root directory of forward-test ledgers, containing per-model subdirectories")]
    ledger_root: PathBuf,
}

#[derive(Debug, Args)]
struct ConfidenceArgs {
    #[command(flatten)]
    universe: UniverseArgs,

    #[command(flatten)]
    context: ContextArgs,

    #[arg(
        long,
        default_value = "checkpoints/final_model.json",
        help = "Path to trained model JSON file; fitted confidence model is written back into it"
    )]
    model: PathBuf,

    #[arg(long, default_value_t = asterion_model::confidence::DEFAULT_COST, help = "Round-trip trading cost as a price fraction (0.0025 = 0.25%), deducted before evaluating profit")]
    cost: f64,

    #[arg(
        long,
        help = "Output path for validation and test confidence JSON reports (default: next to the model)"
    )]
    out: Option<PathBuf>,
}

#[derive(Debug, Args)]
struct PredictArgs {
    #[command(flatten)]
    universe: UniverseArgs,

    #[command(flatten)]
    context: ContextArgs,

    #[arg(
        long,
        default_value = "checkpoints/final_model.json",
        help = "Path to the trained model JSON file to load"
    )]
    model: PathBuf,

    #[arg(
        long,
        default_value_t = 8,
        help = "Number of strongest feature contributions to list per horizon"
    )]
    top_k: usize,

    #[arg(
        long,
        help = "Write forecasts as JSON to this file instead of printing them"
    )]
    out: Option<PathBuf>,

    #[arg(long, default_value_t = asterion_model::confidence::DEFAULT_COST, help = "Round-trip trading cost as a fraction of price used for the profit signal (0.0025 = 0.25%)")]
    cost: f64,
}

#[derive(Debug, Args)]
struct FetchFinancialsArgs {
    #[arg(long, default_value_t = dart_financials::FIRST_YEAR, help = "First business year of key-account statements to fetch")]
    from_year: i32,

    #[arg(
        long,
        default_value_t = 2019,
        help = "First business year of full financial statements to fetch"
    )]
    full_from_year: i32,

    #[arg(long, default_value = dart_financials::DEFAULT_ROOT, help = "Directory for raw financial statement responses and manifests")]
    cache_dir: PathBuf,

    #[arg(long, default_value = opendart::DEFAULT_ROOT, help = "Directory of cached OpenDART filing lists used to map stock codes to DART corp codes")]
    filing_cache_dir: PathBuf,

    #[arg(
        long,
        default_value_t = 20000,
        help = "Maximum number of API requests to send in one run"
    )]
    max_requests: u32,

    #[arg(
        long,
        default_value = "data",
        help = "Directory whose total size is checked against the size limit"
    )]
    data_dir: PathBuf,

    #[arg(
        long,
        default_value_t = 50.0,
        help = "Stop once the data directory exceeds this many gigabytes"
    )]
    max_data_gb: f64,
}

#[derive(Debug, Args)]
struct FetchDisclosuresArgs {
    #[arg(long, value_parser = parse_date, help = "First date to fetch, inclusive (YYYY-MM-DD or YYYYMMDD)")]
    from: NaiveDate,

    #[arg(long, value_parser = parse_date, help = "Last date to fetch, inclusive (defaults to yesterday in Korea)")]
    to: Option<NaiveDate>,

    #[arg(long, default_value = opendart::DEFAULT_ROOT, help = "Directory for raw OpenDART API responses and per-feed manifests")]
    cache_dir: PathBuf,

    #[arg(
        long,
        default_value_t = 8000,
        help = "Maximum number of API requests to send in a single run"
    )]
    max_requests: u32,
}

#[derive(Debug, Args)]
struct MonitorArgs {
    #[arg(
        long,
        default_value_t = 8787,
        help = "Local port the dashboard server listens on"
    )]
    port: u16,
}

#[derive(Debug, Args)]
struct UniverseArgs {
    #[arg(
        long,
        value_delimiter = ',',
        help = "Comma-separated KRX short codes (e.g. 005930,000660)"
    )]
    symbols: Vec<String>,

    #[arg(
        long,
        help = "Path to file with one KRX code per line; text after # is ignored"
    )]
    universe: Option<PathBuf>,

    #[arg(long, default_value = cache::DEFAULT_ROOT, help = "Directory for raw API responses and per-symbol manifests")]
    cache_dir: PathBuf,
}

#[derive(Debug, Args)]
struct FetchArgs {
    #[command(flatten)]
    universe: UniverseArgs,

    #[arg(long, value_parser = parse_date, help = "First date to fetch, inclusive (YYYY-MM-DD or YYYYMMDD)")]
    from: NaiveDate,

    #[arg(long, value_parser = parse_date, help = "Last date to fetch, inclusive (defaults to latest published portal date)")]
    to: Option<NaiveDate>,

    #[arg(
        long,
        help = "Print date ranges to request without calling API or requiring a key"
    )]
    dry_run: bool,
}

#[derive(Debug, Args)]
struct UniverseBuildArgs {
    #[arg(long, value_parser = parse_date, help = "Trading date for market cap ranking; set before validation to avoid look-ahead bias (YYYY-MM-DD or YYYYMMDD)")]
    as_of: NaiveDate,

    #[arg(
        long,
        default_value_t = 1000,
        help = "Number of largest stocks by market cap to keep"
    )]
    top: usize,

    #[arg(
        long,
        help = "Path of universe file to write (one KRX code per line with # stock name)"
    )]
    out: PathBuf,
}

#[derive(Debug, Args)]
struct FetchIndexArgs {
    #[arg(long, value_parser = parse_date, help = "First date to fetch, inclusive (YYYY-MM-DD or YYYYMMDD)")]
    from: NaiveDate,

    #[arg(long, value_parser = parse_date, help = "Last date to fetch, inclusive (defaults to latest published portal date)")]
    to: Option<NaiveDate>,

    #[arg(long, default_value = cache::DEFAULT_INDEX_ROOT, help = "Directory for raw index API responses and per-index manifests")]
    cache_dir: PathBuf,

    #[arg(
        long,
        help = "Override index API endpoint URL (e.g. to use older non-V2 service)"
    )]
    endpoint: Option<String>,
}

#[derive(Debug, Args)]
struct FetchMacroArgs {
    #[arg(long, value_parser = parse_date, help = "First date to fetch, inclusive (YYYY-MM-DD or YYYYMMDD)")]
    from: NaiveDate,

    #[arg(long, value_parser = parse_date, help = "Last date to fetch, inclusive (defaults to today in UTC)")]
    to: Option<NaiveDate>,

    #[arg(long, default_value = fred::DEFAULT_ROOT, help = "Directory for raw FRED API responses and per-series manifests")]
    cache_dir: PathBuf,
}

#[derive(Debug, Args)]
struct ValidateArgs {
    #[command(flatten)]
    universe: UniverseArgs,
}

#[derive(Debug, Args)]
struct TrainArgs {
    #[command(flatten)]
    universe: UniverseArgs,

    #[arg(long, default_value_t = 40, help = "Number of training epochs")]
    epochs: usize,

    #[arg(long, default_value_t = 1024, help = "Batch size")]
    batch_size: usize,

    #[arg(long, default_value_t = 0.002, help = "Learning rate")]
    lr: f64,

    #[arg(long, default_value_t = 192, help = "Hidden dimension")]
    hidden_dim: usize,

    #[arg(long, default_value_t = 42, help = "Random seed")]
    seed: u64,

    #[arg(
        long,
        default_value_t = 2,
        help = "Number of epochs between validation runs"
    )]
    eval_every: usize,

    #[arg(
        long,
        default_value_t = 5,
        help = "Validation runs without improvement before training stops early"
    )]
    patience: usize,

    #[arg(
        long,
        help = "Worker threads for training and evaluation (defaults to available cores, at most 16)"
    )]
    threads: Option<usize>,

    #[arg(
        long,
        help = "Score the held-out test split at the end; use once, for the final model only"
    )]
    report_test: bool,

    #[arg(long, value_enum, default_value_t = Target::Raw, help = "Target return to predict: raw (stock return) or excess (universe median-adjusted)")]
    target: Target,

    #[command(flatten)]
    context: ContextArgs,

    #[arg(
        long,
        default_value = "checkpoints/multi_horizon_net.json",
        help = "Output path for best trained weights and feature scaler JSON"
    )]
    model_out: PathBuf,

    #[arg(long, value_parser = parse_date, help = "First date of the validation period; earlier rows train the network (default: 2024-01-01)")]
    val_start: Option<NaiveDate>,

    #[arg(long, value_parser = parse_date, help = "First date of validation B (selects checkpoint); validation A fits calibration (default: 2024-07-01)")]
    calib_split: Option<NaiveDate>,

    #[arg(long, value_parser = parse_date, help = "First date of the held-out test period (default: 2025-01-01)")]
    test_start: Option<NaiveDate>,
}

#[derive(Debug, Clone, Copy, clap::ValueEnum)]
enum Target {
    Raw,
    Excess,
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> ExitCode {
    // A missing .env is fine; the key may come from the real environment.
    let _ = dotenvy::dotenv();
    let cli = Cli::parse();
    let dir = cli.status_dir;
    let result = match cli.command {
        Command::Fetch(args) => {
            let mut progress = if args.dry_run {
                ProgressTracker::disabled("fetch")
            } else {
                ProgressTracker::start(&dir, "fetch")
            };
            let result = fetch(args, &mut progress).await;
            finish(progress, result)
        }
        Command::Validate(args) => validate(args),
        Command::Train(args) => {
            let mut progress = ProgressTracker::start(&dir, "train");
            let result = train(args, &mut progress);
            finish(progress, result)
        }
        Command::Universe(args) => build_universe(args).await,
        Command::FetchIndex(args) => {
            let mut progress = ProgressTracker::start(&dir, "fetch-index");
            let result = fetch_index(args, &mut progress).await;
            finish(progress, result)
        }
        Command::FetchMacro(args) => {
            let mut progress = ProgressTracker::start(&dir, "fetch-macro");
            let result = fetch_macro(args, &mut progress).await;
            finish(progress, result)
        }
        Command::Monitor(args) => run_monitor(args, &dir),
        Command::FetchDisclosures(args) => {
            let mut progress = ProgressTracker::start(&dir, "fetch-disclosures");
            let result = fetch_disclosures(args, &mut progress).await;
            finish(progress, result)
        }
        Command::Predict(args) => predict(args),
        Command::Evaluate(args) => {
            let mut progress = ProgressTracker::start(&dir, "evaluate");
            let result = evaluate(args, &mut progress);
            finish(progress, result)
        }
        Command::Confidence(args) => {
            let mut progress = ProgressTracker::start(&dir, "confidence");
            let result = confidence(args, &mut progress);
            finish(progress, result)
        }
        Command::FetchInvestors(args) => {
            let mut progress = ProgressTracker::start(&dir, "fetch-investors");
            let result = fetch_investors(args, &mut progress).await;
            finish(progress, result)
        }
        Command::LedgerRecord(args) => ledger_record(args),
        Command::LedgerScore(args) => ledger_score(args),
        Command::RankFit(args) => {
            let mut progress = ProgressTracker::start(&dir, "rank-fit");
            let result = rank_fit(args, &mut progress);
            finish(progress, result)
        }
        Command::RankRecord(args) => rank_record(args),
        Command::FetchOverseas(args) => {
            let mut progress = ProgressTracker::start(&dir, "fetch-overseas");
            let result = fetch_overseas(args, &mut progress).await;
            finish(progress, result)
        }
        Command::FetchHistory(args) => {
            let mut progress = ProgressTracker::start(&dir, "fetch-history");
            let result = fetch_history(args, &mut progress).await;
            finish(progress, result)
        }
        Command::RankScore(args) => rank_score(args),
        Command::ResearchCv(args) => {
            let mut progress = ProgressTracker::start(&dir, "research-cv");
            let result = research_cv(args, &mut progress);
            finish(progress, result)
        }
        Command::FetchFinancials(args) => {
            let mut progress = ProgressTracker::start(&dir, "fetch-financials");
            let result = fetch_financials(args, &mut progress).await;
            finish(progress, result)
        }
        Command::FetchReddit(args) => {
            let mut progress = ProgressTracker::start(&dir, "fetch-reddit");
            let result = fetch_reddit(args, &mut progress).await;
            finish(progress, result)
        }
        Command::RedditDaily(args) => {
            let mut progress = ProgressTracker::start(&dir, "reddit-daily");
            let result = reddit_daily(args, &mut progress);
            finish(progress, result)
        }
    };
    match result {
        Ok(code) => code,
        Err(e) => {
            eprintln!("asterion: {e}");
            ExitCode::FAILURE
        }
    }
}

// Records how the run ended before handing the result back.
fn finish(mut progress: ProgressTracker, result: CliResult<ExitCode>) -> CliResult<ExitCode> {
    progress.finish(result.as_ref().err().map(ToString::to_string));
    result
}

fn run_monitor(args: MonitorArgs, status_dir: &Path) -> CliResult<ExitCode> {
    let listener = monitor::bind(args.port)?;
    let url = format!("http://127.0.0.1:{}", listener.local_addr()?.port());
    println!("Dashboard running at {url} (open in browser, press Ctrl+C to stop)");
    monitor::serve(listener, status_dir)?;
    Ok(ExitCode::SUCCESS)
}

fn parse_date(s: &str) -> Result<NaiveDate, String> {
    NaiveDate::parse_from_str(s, "%Y-%m-%d")
        .or_else(|_| NaiveDate::parse_from_str(s, "%Y%m%d"))
        .map_err(|_| format!("invalid date '{s}': expected YYYY-MM-DD or YYYYMMDD"))
}

fn resolve_symbols(args: &UniverseArgs, cache: &RawCache) -> CliResult<Vec<Symbol>> {
    let mut codes: Vec<String> = args.symbols.clone();
    if let Some(path) = &args.universe {
        let text = fs::read_to_string(path)?;
        codes.extend(
            text.lines()
                .map(|line| line.split('#').next().unwrap_or("").trim())
                .filter(|code| !code.is_empty())
                .map(str::to_owned),
        );
    }
    if codes.is_empty() {
        codes = cache.cached_codes()?;
    }
    let mut symbols = codes
        .iter()
        .map(|code| Symbol::new(Market::Kr, code))
        .collect::<Result<Vec<_>, _>>()?;
    symbols.sort();
    symbols.dedup();
    Ok(symbols)
}

async fn fetch(args: FetchArgs, progress: &mut ProgressTracker) -> CliResult<ExitCode> {
    let cache = RawCache::new(&args.universe.cache_dir);
    let symbols = resolve_symbols(&args.universe, &cache)?;
    if symbols.is_empty() {
        return Err("cache is empty; specify symbols with --symbols or --universe".into());
    }

    let latest = latest_available_date(Utc::now());
    let to = args.to.unwrap_or(latest).min(latest);
    println!("range={}..{} latest_available={latest}", args.from, to);

    if args.dry_run {
        let mut planned = 0;
        for symbol in &symbols {
            let manifest = cache.load_manifest(symbol)?;
            for (b, e) in missing_ranges(&manifest.covered(), args.from, to) {
                println!("{} {b}..{e}", symbol.code());
                planned += 1;
            }
        }
        println!("planned_ranges={planned}");
        return Ok(ExitCode::SUCCESS);
    }

    let mut client = DataGoKrClient::new(ServiceKey::from_env()?, ClientConfig::default())?;
    let mut failures = 0;
    let mut rows = 0;
    progress.stage("fetching", symbols.len() as u64);
    for (i, symbol) in symbols.iter().enumerate() {
        progress.current(symbol.code());
        let outcome = collect_symbol(&mut client, &cache, symbol, args.from, to, latest).await;
        let fatal = outcome.as_ref().is_err_and(DataError::is_fatal_for_batch);
        match outcome {
            Ok(o) => {
                rows += o.rows;
                println!(
                    "{} ranges={} pages={} rows={}",
                    symbol.code(),
                    o.ranges_fetched,
                    o.pages,
                    o.rows
                );
            }
            Err(e) => {
                failures += 1;
                eprintln!("{}: {e}", symbol.code());
            }
        }
        report_fetch(progress, client.requests_sent(), failures, rows, i);
        if fatal {
            eprintln!("skipping remaining symbols: API error would repeat for all requests");
            break;
        }
    }
    println!("requests={} failures={failures}", client.requests_sent());
    println!("{ATTRIBUTION}");
    Ok(if failures == 0 {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    })
}

fn report_fetch(
    progress: &mut ProgressTracker,
    requests: u32,
    failures: u32,
    rows: usize,
    index: usize,
) {
    progress.counter("requests", f64::from(requests));
    progress.counter("failures", f64::from(failures));
    progress.counter("rows", rows as f64);
    progress.advance(index as u64 + 1);
}

async fn build_universe(args: UniverseBuildArgs) -> CliResult<ExitCode> {
    let mut client = DataGoKrClient::new(ServiceKey::from_env()?, ClientConfig::default())?;
    let snapshot = fetch_snapshot(&mut client, args.as_of).await?;
    if snapshot.is_empty() {
        return Err(format!(
            "no stock rows returned for {}; likely not a trading day, pick another date",
            args.as_of
        )
        .into());
    }

    let selected = select_top_by_market_cap(&snapshot, args.top);
    let mut text = format!(
        "# source={SOURCE_ID} as_of={} top={} selected={}\n",
        args.as_of,
        args.top,
        selected.len()
    );
    for quote in &selected {
        text.push_str(&format!("{}  # {}\n", quote.code, quote.name));
    }
    if let Some(dir) = args.out.parent().filter(|d| !d.as_os_str().is_empty()) {
        fs::create_dir_all(dir)?;
    }
    fs::write(&args.out, text)?;

    println!(
        "as_of={} listed={} selected={} out={}",
        args.as_of,
        snapshot.len(),
        selected.len(),
        args.out.display()
    );
    println!("requests={}", client.requests_sent());
    println!("{ATTRIBUTION}");
    Ok(ExitCode::SUCCESS)
}

async fn fetch_index(args: FetchIndexArgs, progress: &mut ProgressTracker) -> CliResult<ExitCode> {
    let cache = RawCache::for_index(&args.cache_dir);
    let latest = latest_available_date(Utc::now());
    let to = args.to.unwrap_or(latest).min(latest);
    println!("range={}..{} latest_available={latest}", args.from, to);

    let mut config = ClientConfig::default();
    if let Some(endpoint) = args.endpoint {
        config.index_endpoint = endpoint;
    }
    let mut client = DataGoKrClient::new(ServiceKey::from_env()?, config)?;
    let mut failures = 0;
    let mut rows = 0;
    progress.stage("fetching", INDICES.len() as u64);
    for (i, index) in INDICES.iter().enumerate() {
        progress.current(index.key);
        let outcome = collect_index(&mut client, &cache, index, args.from, to, latest).await;
        let fatal = outcome.as_ref().is_err_and(DataError::is_fatal_for_batch);
        match outcome {
            Ok(o) => {
                rows += o.rows;
                println!(
                    "{} ranges={} pages={} rows={}",
                    index.key, o.ranges_fetched, o.pages, o.rows
                );
            }
            Err(e) => {
                failures += 1;
                eprintln!("{}: {e}", index.key);
            }
        }
        report_fetch(progress, client.requests_sent(), failures, rows, i);
        if fatal {
            break;
        }
    }
    println!("requests={} failures={failures}", client.requests_sent());
    println!("{ATTRIBUTION}");
    Ok(if failures == 0 {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    })
}

async fn fetch_macro(args: FetchMacroArgs, progress: &mut ProgressTracker) -> CliResult<ExitCode> {
    let cache = RawCache::with_source(&args.cache_dir, fred::SOURCE_ID);
    let to = args.to.unwrap_or_else(|| Utc::now().date_naive());
    println!("range={}..{to}", args.from);

    let mut client = FredClient::new(FredKey::from_env()?, fred::ENDPOINT, RetryPolicy::default())?;
    let mut failures = 0;
    let mut rows = 0;
    progress.stage("fetching", fred::all_series().count() as u64);
    for (i, series) in fred::all_series().enumerate() {
        progress.current(series);
        match fred::collect_series(&mut client, &cache, series, args.from, to).await {
            Ok(o) => {
                rows += o.rows;
                println!("{series} ranges={} rows={}", o.ranges_fetched, o.rows);
            }
            Err(e) => {
                failures += 1;
                eprintln!("{series}: {e}");
            }
        }
        report_fetch(progress, client.requests_sent(), failures, rows, i);
    }
    println!("requests={} failures={failures}", client.requests_sent());
    println!("{}", fred::ATTRIBUTION);
    Ok(if failures == 0 {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    })
}

async fn fetch_reddit(
    args: FetchRedditArgs,
    progress: &mut ProgressTracker,
) -> CliResult<ExitCode> {
    let subreddits = reddit::parse_list(&fs::read_to_string(&args.list)?)?;
    let today = Utc::now().date_naive();
    let to = args.to.unwrap_or(today).min(today);
    println!("range={}..{to}", args.from);

    let policy = RetryPolicy {
        min_interval: reddit::MIN_INTERVAL,
        max_attempts: 5,
        initial_backoff: std::time::Duration::from_secs(2),
        timeout: std::time::Duration::from_secs(60),
    };
    // Arctic Shift answers ~1/3 of requests with an overload 422; ride out bursts
    // without dropping the page (the week checkpoint keeps already-stored pages).
    let mut client = ArcticClient::new(reddit::BASE_URL, policy)?
        .with_busy_rounds(6, std::time::Duration::from_secs(60));
    let mut failures = 0;
    let mut posts = 0;
    progress.stage("fetching", subreddits.len() as u64);
    for (i, sub) in subreddits.iter().enumerate() {
        progress.current(sub);
        let result = reddit::collect::collect_subreddit(
            &mut client,
            &args.root,
            sub,
            args.from,
            to,
            today,
            args.max_requests,
        )
        .await;
        let stop = match result {
            Ok(o) => {
                posts += o.posts;
                println!(
                    "{sub} final_weeks={} provisional_days={} posts={} truncated={}",
                    o.final_weeks, o.provisional_days, o.posts, o.truncated_seconds
                );
                o.budget_exhausted
            }
            Err(e) => {
                failures += 1;
                eprintln!("{sub}: {e}");
                e.is_fatal_for_batch()
            }
        };
        report_fetch(progress, client.requests_sent(), failures, posts, i);
        if stop {
            break;
        }
    }
    println!("requests={} failures={failures}", client.requests_sent());
    Ok(if failures == 0 {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    })
}

fn reddit_daily(args: RedditDailyArgs, progress: &mut ProgressTracker) -> CliResult<ExitCode> {
    let subreddits = reddit::parse_list(&fs::read_to_string(&args.list)?)?;
    progress.stage("aggregating", subreddits.len() as u64);
    let mut records = Vec::new();
    for (i, sub) in subreddits.iter().enumerate() {
        progress.current(sub);
        records.extend(reddit::daily::aggregate_subreddit(&args.root, sub)?);
        progress.advance(i as u64 + 1);
    }
    reddit::daily::write_daily(&args.out, &records)?;
    println!("records={}", records.len());
    Ok(ExitCode::SUCCESS)
}

async fn fetch_disclosures(
    args: FetchDisclosuresArgs,
    progress: &mut ProgressTracker,
) -> CliResult<ExitCode> {
    let cache = RawCache::with_source(&args.cache_dir, opendart::SOURCE_ID);
    // Filings of the current Korean day are still arriving, so stop at yesterday (KST).
    let yesterday = (Utc::now() + chrono::Duration::hours(9))
        .date_naive()
        .pred_opt()
        .ok_or("date out of range")?;
    let to = args.to.unwrap_or(yesterday).min(yesterday);
    println!("range={}..{to}", args.from);

    let mut plan = Vec::new();
    for feed in opendart::Feed::all() {
        for (begin, end) in opendart::pending_windows(&cache, feed, args.from, to)? {
            plan.push((feed, begin, end));
        }
    }
    // Oldest windows first across all feeds, so a capped run leaves a clean frontier.
    plan.sort_by_key(|(feed, begin, _)| (*begin, feed.key()));

    let mut client = DartClient::new(
        DartKey::from_env()?,
        opendart::ENDPOINT,
        RetryPolicy::default(),
    )?;
    let mut failures = 0;
    let mut rows = 0;
    let mut remaining = plan.len();
    progress.stage("fetching", plan.len() as u64);
    for (i, (feed, begin, end)) in plan.iter().enumerate() {
        if client.requests_sent() >= args.max_requests {
            break;
        }
        progress.current(&format!("{} {begin}", feed.key()));
        let outcome = opendart::collect_window(&mut client, &cache, *feed, *begin, *end).await;
        let fatal = outcome.as_ref().is_err_and(DataError::is_fatal_for_batch);
        match outcome {
            Ok(o) => {
                rows += o.rows;
                remaining -= 1;
                println!(
                    "{} {begin}..{end} pages={} rows={}",
                    feed.key(),
                    o.pages,
                    o.rows
                );
            }
            Err(e) => {
                failures += 1;
                eprintln!("{} {begin}..{end}: {e}", feed.key());
            }
        }
        report_fetch(progress, client.requests_sent(), failures, rows, i);
        if fatal {
            break;
        }
    }
    println!(
        "requests={} failures={failures} remaining_windows={remaining}",
        client.requests_sent()
    );
    println!("{}", opendart::ATTRIBUTION);
    Ok(if failures == 0 {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    })
}

// Total bytes of regular files under `dir`; unreadable entries count as zero.
fn dir_size(dir: &Path) -> u64 {
    let mut total = 0;
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(entries) = fs::read_dir(&d) else {
            continue;
        };
        for entry in entries.flatten() {
            match entry.metadata() {
                Ok(m) if m.is_dir() => stack.push(entry.path()),
                Ok(m) => total += m.len(),
                Err(_) => {}
            }
        }
    }
    total
}

// Re-measures the data directory every `every` checks so the walk stays cheap.
#[derive(Debug)]
struct SizeGuard {
    dir: PathBuf,
    limit: u64,
    checks: u32,
    every: u32,
    last: u64,
}

impl SizeGuard {
    fn new(dir: PathBuf, max_gb: f64, every: u32) -> Self {
        let last = dir_size(&dir);
        SizeGuard {
            dir,
            limit: (max_gb * 1e9) as u64,
            checks: 0,
            every: every.max(1),
            last,
        }
    }

    fn exceeded(&mut self) -> bool {
        self.checks += 1;
        if self.checks.is_multiple_of(self.every) {
            self.last = dir_size(&self.dir);
        }
        self.last > self.limit
    }
}

async fn fetch_financials(
    args: FetchFinancialsArgs,
    progress: &mut ProgressTracker,
) -> CliResult<ExitCode> {
    let cache = RawCache::with_source(&args.cache_dir, dart_financials::SOURCE_ID);
    let filings = RawCache::with_source(&args.filing_cache_dir, opendart::SOURCE_ID);
    let corps = dart_financials::sorted_corp_codes(&opendart::load_corp_codes(&filings)?);
    if corps.is_empty() {
        return Err("no corp codes in the filing cache; run fetch-disclosures first".into());
    }
    let today = (Utc::now() + chrono::Duration::hours(9)).date_naive();
    let mut guard = SizeGuard::new(args.data_dir.clone(), args.max_data_gb, 200);
    println!("corps={} data_bytes={}", corps.len(), guard.last);

    let mut client = DartClient::new(
        DartKey::from_env()?,
        opendart::ENDPOINT,
        RetryPolicy::default(),
    )?;
    let mut failures = 0;
    let mut rows = 0;
    let mut stop = None;

    let multi: Vec<Period> = dart_financials::periods(args.from_year, today)
        .into_iter()
        .filter(|p| dart_financials::multi_is_pending(&cache, *p).unwrap_or(true))
        .collect();
    progress.stage("key-accounts", multi.len() as u64);
    for (i, period) in multi.iter().enumerate() {
        if client.requests_sent() >= args.max_requests {
            stop = Some("max_requests");
            break;
        }
        if guard.exceeded() {
            stop = Some("max_data_gb");
            break;
        }
        let label = format!("{} {}", period.year, period.report.code());
        progress.current(&label);
        let outcome = dart_financials::collect_multi(&mut client, &cache, *period, &corps).await;
        let fatal = outcome.as_ref().is_err_and(DataError::is_fatal_for_batch);
        match outcome {
            Ok(o) => {
                rows += o.rows;
                println!("multi {label} pages={} reports={}", o.pages, o.rows);
            }
            Err(e) => {
                failures += 1;
                eprintln!("multi {label}: {e}");
            }
        }
        report_fetch(progress, client.requests_sent(), failures, rows, i);
        if fatal {
            stop = Some("api_error");
            break;
        }
    }

    if stop.is_none() {
        // Newest periods first: they matter most for features if the quota runs out.
        // Only periods past their filing deadline: an unfiled report costs two requests and
        // is asked again next run.
        let mut periods: Vec<Period> = dart_financials::periods(args.full_from_year, today)
            .into_iter()
            .filter(|p| p.is_final(today))
            .collect();
        periods.reverse();
        let total = periods.len() * corps.len();
        progress.stage("full-statements", total as u64);
        let mut done = 0;
        'outer: for period in &periods {
            for corp in &corps {
                done += 1;
                if !dart_financials::full_is_pending(&cache, corp, *period)? {
                    continue;
                }
                if client.requests_sent() >= args.max_requests {
                    stop = Some("max_requests");
                    break 'outer;
                }
                if guard.exceeded() {
                    stop = Some("max_data_gb");
                    break 'outer;
                }
                let label = format!("{corp} {} {}", period.year, period.report.code());
                progress.current(&label);
                let outcome =
                    dart_financials::collect_full(&mut client, &cache, corp, *period).await;
                let fatal = outcome.as_ref().is_err_and(DataError::is_fatal_for_batch);
                match outcome {
                    Ok(o) => rows += o.rows,
                    Err(e) => {
                        failures += 1;
                        eprintln!("full {label}: {e}");
                    }
                }
                report_fetch(progress, client.requests_sent(), failures, rows, done - 1);
                if client.requests_sent().is_multiple_of(500) {
                    println!(
                        "full progress={done}/{total} requests={} rows={rows}",
                        client.requests_sent()
                    );
                }
                if fatal {
                    stop = Some("api_error");
                    break 'outer;
                }
            }
        }
    }
    println!(
        "requests={} failures={failures} rows={rows} stop={} data_bytes={}",
        client.requests_sent(),
        stop.unwrap_or("complete"),
        dir_size(&args.data_dir)
    );
    println!("{}", opendart::ATTRIBUTION);
    // A quota error is the expected end of a run that drains the daily limit.
    Ok(
        if failures <= 1 && stop == Some("api_error") || failures == 0 {
            ExitCode::SUCCESS
        } else {
            ExitCode::FAILURE
        },
    )
}

fn validate(args: ValidateArgs) -> CliResult<ExitCode> {
    let cache = RawCache::new(&args.universe.cache_dir);
    let symbols = resolve_symbols(&args.universe, &cache)?;

    let mut issues: BTreeMap<String, Vec<Issue>> = BTreeMap::new();
    let mut dates: BTreeMap<String, Vec<NaiveDate>> = BTreeMap::new();
    let mut summary = Vec::new();
    for symbol in &symbols {
        let quotes = cache.load_quotes(symbol)?;
        let bars: Vec<_> = quotes.iter().map(|q| q.to_bar()).collect();
        issues.insert(symbol.code().to_owned(), validate_bars(Market::Kr, &bars));
        dates.insert(
            symbol.code().to_owned(),
            bars.iter().map(|b| b.date).collect(),
        );
        summary.push((
            symbol.code().to_owned(),
            bars.len(),
            bars.first().map(|b| b.date),
            bars.last().map(|b| b.date),
        ));
    }
    for (code, missing) in missing_dates(&dates) {
        let list = issues.entry(code).or_default();
        list.extend(missing.into_iter().map(|date| Issue::MissingDate { date }));
        list.sort_by_key(Issue::date);
    }

    let mut total_errors = 0;
    for (code, rows, first, last) in summary {
        let list = issues.get(&code).map(Vec::as_slice).unwrap_or_default();
        let errors = list.iter().filter(|i| i.is_error()).count();
        total_errors += errors;
        let span = |d: Option<NaiveDate>| d.map_or_else(|| "-".to_owned(), |d| d.to_string());
        println!(
            "{code} rows={rows} first={} last={} errors={errors} warnings={}",
            span(first),
            span(last),
            list.len() - errors
        );
        for issue in list {
            match issue {
                Issue::ExtremeMove { date, ret } => {
                    println!("  {} {date} ret={ret:+.4}", issue.kind());
                }
                _ => println!("  {} {}", issue.kind(), issue.date()),
            }
        }
    }
    println!("symbols={} errors={total_errors}", symbols.len());
    println!("{ATTRIBUTION}");
    Ok(if total_errors == 0 {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    })
}

fn train(args: TrainArgs, progress: &mut ProgressTracker) -> CliResult<ExitCode> {
    let cache = RawCache::new(&args.universe.cache_dir);
    let symbols = resolve_symbols(&args.universe, &cache)?;
    if symbols.is_empty() {
        return Err("cache is empty; specify symbols with --symbols or --universe".into());
    }
    let defaults = asterion_model::TrainConfig::default();
    let config = asterion_model::TrainConfig {
        epochs: args.epochs,
        batch_size: args.batch_size,
        lr: args.lr,
        hidden_dim: args.hidden_dim,
        seed: args.seed,
        eval_every: args.eval_every,
        patience: args.patience,
        report_test: args.report_test,
        threads: args.threads.unwrap_or(defaults.threads),
        target: match args.target {
            Target::Raw => asterion_model::TargetKind::Raw,
            Target::Excess => asterion_model::TargetKind::Excess,
        },
        model_out: Some(args.model_out),
        val_start: args.val_start.unwrap_or(defaults.val_start),
        calib_split: args.calib_split.unwrap_or(defaults.calib_split),
        test_start: args.test_start.unwrap_or(defaults.test_start),
        ..defaults
    };
    let context = args.context.load()?;
    asterion_model::run_training(&cache, &symbols, &context, &config, progress)?;
    Ok(ExitCode::SUCCESS)
}

fn evaluate(args: EvaluateArgs, progress: &mut ProgressTracker) -> CliResult<ExitCode> {
    let cache = RawCache::new(&args.universe.cache_dir);
    let symbols = resolve_symbols(&args.universe, &cache)?;
    let model = asterion_model::SavedModel::load(&args.model)?;
    let context = args.context.load()?;
    asterion_model::evaluate_saved(&model, &cache, &symbols, &context, progress)?;
    Ok(ExitCode::SUCCESS)
}

async fn fetch_investors(
    args: FetchInvestorsArgs,
    progress: &mut ProgressTracker,
) -> CliResult<ExitCode> {
    let cache = RawCache::new(&args.universe.cache_dir);
    let symbols = resolve_symbols(&args.universe, &cache)?;
    let policy = RetryPolicy {
        min_interval: kis::MIN_INTERVAL,
        ..RetryPolicy::default()
    };
    let mut client = KisClient::new(KisKeys::from_env()?, kis::BASE_URL, policy)?;
    let (mut failures, mut streak, mut rows, mut skipped) = (0u32, 0u32, 0usize, 0usize);
    progress.stage("fetching", symbols.len() as u64);
    for (i, symbol) in symbols.iter().enumerate() {
        if args
            .max_requests
            .is_some_and(|m| client.requests_sent() >= m)
        {
            break;
        }
        let code = symbol.code();
        progress.current(code);
        match kis::collect_code(&mut client, &args.root, code).await {
            Ok(Some(n)) => {
                rows += n;
                streak = 0;
            }
            Ok(None) => skipped += 1,
            // Auth and quota failures repeat for every code, so stop at once.
            Err(
                e @ (DataError::HttpStatus { status: 401 | 403 } | DataError::MissingServiceKey(_)),
            ) => {
                return Err(e.into());
            }
            Err(e) => {
                failures += 1;
                streak += 1;
                eprintln!("{code}: {e}");
                if streak >= 20 {
                    return Err(e.into());
                }
            }
        }
        report_fetch(progress, client.requests_sent(), failures, rows, i);
    }
    println!(
        "requests={} failures={failures} already_fetched_today={skipped} final_rows={rows}",
        client.requests_sent()
    );
    Ok(if failures == 0 {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    })
}

fn ledger_record(args: LedgerRecordArgs) -> CliResult<ExitCode> {
    let cache = RawCache::new(&args.universe.cache_dir);
    let symbols = resolve_symbols(&args.universe, &cache)?;
    let model = asterion_model::SavedModel::load(&args.model)?;
    let context = args.context.load()?;
    let out = asterion_model::ledger::record(
        &model,
        &args.model,
        &cache,
        &symbols,
        &context,
        &args.ledger_root,
    )?;
    println!(
        "as_of={} universe={} created={} path={}",
        out.as_of,
        out.universe,
        out.created,
        out.path.display()
    );
    Ok(ExitCode::SUCCESS)
}

fn ledger_score(args: LedgerScoreArgs) -> CliResult<ExitCode> {
    let cache = RawCache::new(&args.cache_dir);
    let score = asterion_model::ledger::score(&args.model, &cache, &args.ledger_root)?;
    asterion_model::ledger::print_score(&score);
    Ok(ExitCode::SUCCESS)
}

fn rank_fit(args: RankLedgerArgs, progress: &mut ProgressTracker) -> CliResult<ExitCode> {
    let cache = RawCache::new(&args.universe.cache_dir);
    let symbols = resolve_symbols(&args.universe, &cache)?;
    let context = args.context.load()?;
    let (samples, _) = asterion_model::train::load_samples(&cache, &symbols, &context, progress)?;
    let panel = asterion_model::research::Panel::build(samples);
    progress.stage("fitting", 0);
    let fp = asterion_model::rank_ledger::fit_and_register(&panel, &args.dir)?;
    println!(
        "rows={} fingerprint={fp} dir={}",
        panel.rows.len(),
        args.dir.display()
    );
    Ok(ExitCode::SUCCESS)
}

async fn fetch_history(
    args: FetchHistoryArgs,
    progress: &mut ProgressTracker,
) -> CliResult<ExitCode> {
    let cache = RawCache::new(&args.universe.cache_dir);
    let symbols = resolve_symbols(&args.universe, &cache)?;
    let policy = RetryPolicy {
        min_interval: kis::MIN_INTERVAL,
        ..RetryPolicy::default()
    };
    let mut client = KisClient::new(KisKeys::from_env()?, kis::BASE_URL, policy)?;
    let (mut failures, mut rows, mut requests) = (0u32, 0usize, 0usize);
    progress.stage("fetching", symbols.len() as u64);
    for (i, symbol) in symbols.iter().enumerate() {
        if args
            .max_requests
            .is_some_and(|m| client.requests_sent() >= m)
        {
            break;
        }
        let code = symbol.code();
        progress.current(code);
        match kis::collect_history(
            &mut client,
            &args.root,
            code,
            args.from,
            args.to,
            &args.adjusted,
        )
        .await
        {
            Ok((r, n)) => {
                requests += r;
                rows += n;
                println!("{code} requests={r} rows={n}");
            }
            Err(
                e @ (DataError::HttpStatus { status: 401 | 403 } | DataError::MissingServiceKey(_)),
            ) => {
                return Err(e.into());
            }
            Err(e) => {
                failures += 1;
                eprintln!("{code} error={e}");
            }
        }
        progress.advance(i as u64 + 1);
    }
    println!("fetch_history_done requests={requests} rows={rows} failures={failures}");
    Ok(if failures == 0 {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    })
}

async fn fetch_overseas(
    args: FetchOverseasArgs,
    progress: &mut ProgressTracker,
) -> CliResult<ExitCode> {
    let listings: Vec<(String, String)> = fs::read_to_string(&args.list)?
        .lines()
        .map(|l| l.split('#').next().unwrap_or("").trim())
        .filter(|l| !l.is_empty())
        .filter_map(|l| {
            let (ex, sym) = l.split_once(':')?;
            Some((ex.trim().to_uppercase(), sym.trim().to_uppercase()))
        })
        .collect();
    let to = args
        .to
        .unwrap_or_else(|| (Utc::now() + chrono::Duration::hours(9)).date_naive());
    let policy = RetryPolicy {
        min_interval: kis::MIN_INTERVAL,
        ..RetryPolicy::default()
    };
    let mut client = KisClient::new(KisKeys::from_env()?, kis::BASE_URL, policy)?;
    let (mut failures, mut rows, mut requests) = (0u32, 0usize, 0usize);
    progress.stage("fetching", listings.len() as u64);
    for (i, (ex, sym)) in listings.iter().enumerate() {
        progress.current(&format!("{ex}:{sym}"));
        match kis::collect_overseas(
            &mut client,
            &args.root,
            ex,
            sym,
            args.from,
            to,
            &args.adjusted,
        )
        .await
        {
            Ok((r, n)) => {
                requests += r;
                rows += n;
                println!("{ex}:{sym} requests={r} rows={n}");
            }
            Err(
                e @ (DataError::HttpStatus { status: 401 | 403 } | DataError::MissingServiceKey(_)),
            ) => {
                return Err(e.into());
            }
            Err(e) => {
                failures += 1;
                eprintln!("{ex}:{sym} error={e}");
            }
        }
        progress.advance(i as u64 + 1);
    }
    println!("fetch_overseas_done requests={requests} rows={rows} failures={failures}");
    Ok(if failures == 0 {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    })
}

fn rank_record(args: RankLedgerArgs) -> CliResult<ExitCode> {
    let cache = RawCache::new(&args.universe.cache_dir);
    let symbols = resolve_symbols(&args.universe, &cache)?;
    let context = args.context.load()?;
    let out = asterion_model::rank_ledger::record(&cache, &symbols, &context, &args.dir)?;
    println!(
        "as_of={} universe={} created={} path={}",
        out.as_of,
        out.universe,
        out.created,
        out.path.display()
    );
    Ok(ExitCode::SUCCESS)
}

fn rank_score(args: RankLedgerArgs) -> CliResult<ExitCode> {
    let cache = RawCache::new(&args.universe.cache_dir);
    let context = args.context.load()?;
    let s = asterion_model::rank_ledger::score(&cache, &context.kospi, &args.dir)?;
    println!(
        "entries={} prices_to={:?} passed={:?}",
        s.entries, s.latest_price_date, s.passed
    );
    for a in &s.arms {
        println!(
            "h={} arm={} matured={} eff_periods={:.1} top_net={:+.4} universe_net={:+.4} kospi={:+.4} excess={:+.4} excess_t={:+.2} inc_vs_control={:+.4} inc_t={:+.2}",
            a.horizon,
            a.arm,
            a.matured,
            a.effective_periods,
            a.top_net_mean,
            a.universe_net_mean,
            a.kospi_mean,
            a.excess_mean,
            a.excess_t,
            a.increment_vs_control_mean,
            a.increment_vs_control_t
        );
    }
    Ok(ExitCode::SUCCESS)
}

fn research_cv(args: ResearchCvArgs, progress: &mut ProgressTracker) -> CliResult<ExitCode> {
    use asterion_model::research::{Fold, Panel, run_cv};
    let cache = RawCache::new(&args.universe.cache_dir);
    let symbols = resolve_symbols(&args.universe, &cache)?;
    let context = args.context.load()?;
    // Same adjustment flag as the fetch-history default.
    let history = args
        .with_history
        .then(|| asterion_model::train::HistoryOptions {
            root: args.history_root.clone(),
            adjusted: "0".to_owned(),
            samples_from: args.history_from,
        });
    let (samples, loaded, splice) = asterion_model::train::load_samples_with_history(
        &cache,
        &symbols,
        &context,
        progress,
        history.as_ref(),
    )?;
    if history.is_some() {
        println!(
            "history spliced={} prefix_bars={} no_history={} short_overlap={} unstable_basis={} gap={} missing_cache={}",
            splice.spliced,
            splice.prefix_bars,
            splice.no_history,
            splice.short_overlap,
            splice.unstable_basis,
            splice.gap,
            splice.missing_cache
        );
        if splice.missing_cache > 0 && !args.allow_partial_history {
            let missing = splice.missing_cache;
            return Err(format!("{missing} universe symbols lack KIS history cache; finish fetch-history first or pass --allow-partial-history for a smoke test only").into());
        }
        let names = asterion_model::history::COVERAGE_FEATURES.join(",");
        for (year, n, shares) in asterion_model::history::coverage_by_year(&samples) {
            let shares: Vec<String> = shares.iter().map(|v| format!("{v:.3}")).collect();
            println!(
                "coverage year={year} samples={n} nonzero[{names}]={}",
                shares.join(",")
            );
        }
    }
    let topics =
        |code: &str, date: NaiveDate| context.disclosures.topic_features(code, date).to_vec();
    let panel = Panel::build_with(
        samples,
        &asterion_model::disclosures::TOPIC_FEATURE_NAMES,
        &topics,
    );
    println!(
        "symbols_loaded={loaded} panel_rows={} dates={} features={}",
        panel.rows.len(),
        panel.groups.len(),
        panel.names.len()
    );
    let date = |y: i32| NaiveDate::from_ymd_opt(y, 1, 1).ok_or("invalid fold date");
    let folds = [
        Fold {
            test_from: date(2022)?,
            test_to: date(2023)?,
        },
        Fold {
            test_from: date(2023)?,
            test_to: date(2024)?,
        },
        Fold {
            test_from: date(2024)?,
            test_to: date(2025)?,
        },
    ];
    let burnt = if args.report_burnt {
        Some(Fold {
            test_from: date(2025)?,
            test_to: date(2100)?,
        })
    } else {
        None
    };
    if args.diagnostic {
        let mut windows = folds.to_vec();
        windows.push(Fold {
            test_from: date(2025)?,
            test_to: date(2100)?,
        });
        progress.stage("diagnostic", 0);
        let (diags, deciles) =
            asterion_model::research::run_diagnostic(&panel, &windows, args.embargo_days, &context);
        for d in &diags {
            let s = &d.summary;
            println!(
                "diag window={} candidate={} skip={} top_net_ann={:+.4} bench_ann={:+.4} kospi_ann={:+.4} kosdaq_ann={:+.4} excess_ann={:+.4} excess_t={:+.2} inc_ann={:+.4} inc_t={:+.2} turnover={:.3} bench_turnover={:.3}",
                d.window,
                d.candidate,
                d.skip_one_session,
                s.top_net_annual,
                s.bench_net_annual,
                d.kospi_annual,
                d.kosdaq_annual,
                s.excess_annual,
                s.excess_t,
                s.increment_annual,
                s.increment_t,
                s.mean_turnover,
                d.bench_turnover
            );
        }
        for r in &deciles {
            println!(
                "decile={} mean={:+.4} median={:+.4} n={}",
                r.decile, r.mean, r.median, r.count
            );
        }
        let out = args.out.with_file_name("diagnostic.json");
        fs::write(&out, serde_json::to_vec_pretty(&(diags, deciles))?)?;
        println!("report_out={}", out.display());
        return Ok(ExitCode::SUCCESS);
    }
    progress.stage("cross-validation", 0);
    let report = run_cv(&panel, &folds, burnt, args.embargo_days, &|line| {
        println!("{line}")
    });
    for s in report.dev.iter().chain(&report.burnt) {
        println!(
            "window={} h={} candidate={} ic={:+.4} ic_t={:+.2} top_net_ann={:+.4} bench_ann={:+.4} excess_ann={:+.4} excess_t={:+.2} inc_ann={:+.4} inc_t={:+.2} turnover={:.3}",
            s.window,
            s.horizon,
            s.candidate,
            s.ic_mean,
            s.ic_t,
            s.top_net_annual,
            s.bench_net_annual,
            s.excess_annual,
            s.excess_t,
            s.increment_annual,
            s.increment_t,
            s.mean_turnover
        );
    }
    for v in &report.verdicts {
        println!(
            "verdict candidate={} inc_ann={:+.4} inc_t={:+.2} ic_all_folds={} excess_pos_folds={} passed={}",
            v.candidate,
            v.increment_annual,
            v.increment_t,
            v.ic_positive_every_fold,
            v.excess_positive_folds,
            v.passed
        );
    }
    if let Some(dir) = args.out.parent() {
        fs::create_dir_all(dir)?;
    }
    fs::write(&args.out, serde_json::to_vec_pretty(&report)?)?;
    println!("report_out={}", args.out.display());
    Ok(ExitCode::SUCCESS)
}

fn confidence(args: ConfidenceArgs, progress: &mut ProgressTracker) -> CliResult<ExitCode> {
    let cache = RawCache::new(&args.universe.cache_dir);
    let symbols = resolve_symbols(&args.universe, &cache)?;
    let mut model = asterion_model::SavedModel::load(&args.model)?;
    let context = args.context.load()?;
    let (validation, test) = asterion_model::fit_confidence(
        &mut model, &cache, &symbols, &context, args.cost, progress,
    )?;
    model.save(&args.model)?;
    let out = args
        .out
        .unwrap_or_else(|| args.model.with_extension("confidence.json"));
    fs::write(&out, serde_json::to_vec_pretty(&[validation, test])?)?;
    println!(
        "model_out={} report_out={}",
        args.model.display(),
        out.display()
    );
    Ok(ExitCode::SUCCESS)
}

fn predict(args: PredictArgs) -> CliResult<ExitCode> {
    let cache = RawCache::new(&args.universe.cache_dir);
    let symbols = resolve_symbols(&args.universe, &cache)?;
    let model = asterion_model::SavedModel::load(&args.model)?;
    let context = args.context.load()?;
    let cross_section = resolve_symbols(
        &UniverseArgs {
            symbols: Vec::new(),
            universe: None,
            cache_dir: args.universe.cache_dir.clone(),
        },
        &cache,
    )?;
    let explanations = asterion_model::predict_latest(
        &model,
        &cache,
        &symbols,
        &cross_section,
        &context,
        args.top_k,
        args.cost,
    )?;
    let json = serde_json::to_string_pretty(&explanations)?;
    match &args.out {
        Some(path) => {
            fs::write(path, &json)?;
            for e in &explanations {
                for h in &e.horizons {
                    println!("{} {} {}", e.symbol, e.as_of, h.reason);
                }
            }
            println!("predictions={} out={}", explanations.len(), path.display());
        }
        None => println!("{json}"),
    }
    Ok(ExitCode::SUCCESS)
}
