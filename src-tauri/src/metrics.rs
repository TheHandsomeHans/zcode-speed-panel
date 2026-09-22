use chrono::{Datelike, Days, Local, NaiveTime, TimeZone, Utc};
use rusqlite::OpenFlags;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

/// A completed model invocation (from the ZCode usage database `model_usage` table).
#[derive(Clone, Debug)]
pub struct Call {
    /// Primary key of `model_usage`.
    #[allow(dead_code)]
    pub id: String,
    pub started_ms: i64,
    #[allow(dead_code)]
    pub first_token_ms: Option<i64>,
    pub completed_ms: i64,
    /// Pure generation duration: completed_at - first_token_at
    /// (falls back to duration_ms when first_token is missing).
    pub gen_ms: i64,
    pub output: u64,
    pub reasoning: u64,
    pub input: u64,
    pub cache_creation: u64,
    pub cache_read: u64,
    pub session: String,
}

impl Call {
    /// Rate numerator: output tokens + reasoning tokens (reasoning content is also streamed output).
    pub fn effective_out(&self) -> u64 {
        self.output + self.reasoning
    }
}

/// Per-task real-time detail (multiple only when tasks run concurrently): one CLI process = one task row.
/// Multiple sub-agents running in parallel within the same process cannot be separated at the byte level,
/// so they are faithfully reported as the process total.
#[derive(Serialize, Clone, Debug, Default)]
#[serde(rename_all = "camelCase")]
pub struct TaskStat {
    pub pid: u32,
    /// The in-progress session id this process belongs to (empty = not yet assigned streaming process).
    pub session: String,
    /// Number of in-progress sessions hosted by this process
    /// (>=2 = multiple tasks in one process; speed is the combined total).
    pub n_sessions: u32,
    pub tps: f64,
    /// Whether this process is currently in a streaming state (rate in the detection window exceeds threshold).
    pub streaming: bool,
}

/// ZCode connection detail row (filled by netio): one ESTABLISHED connection + owning process.
#[derive(Serialize, Clone, Debug, Default, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ConnStat {
    /// Remote "ip:port" (v6 is "[addr]:port").
    pub remote: String,
    /// Owning process pid.
    pub pid: u32,
    /// Process type label ("CLI session process" / "main process" / "render process" / "GPU process" /
    /// "utility process" / "crash reporter process") — both groups are ZCode's own processes, distinguished by role.
    pub proc: String,
}

/// Snapshot upload record row (filled by netio): the **most recent** snapshot status for each workspace,
/// from `~/.zcode/v2/checkpoints/*/state.json`. Deserialize is for reading back from the protected-history
/// file (speed-panel-ckpt-history.json); hash = the workspace subdirectory name under checkpoints
/// (used for "Open directory" inline action; omitted in archived older rows -> None).
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct CkptStat {
    /// Workspace display name (last segment of workspacePath).
    pub workspace: String,
    /// Byte count of the most recent compressed & encrypted snapshot.
    pub bytes: u64,
    /// Snapshot record time (recordedAt, epoch ms; 0 = unknown).
    pub recorded_ms: i64,
    /// Most recent snapshot has been accepted by the server (lastAcceptedManifestHash is non-empty).
    pub accepted: bool,
    /// activeUpload is in progress.
    pub uploading: bool,
    /// Workspace subdirectory name (hash) under checkpoints; archived older rows may be None.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hash: Option<String>,
}

/// Metric snapshot pushed to the frontend.
#[derive(Serialize, Clone, Debug, Default)]
#[serde(rename_all = "camelCase")]
pub struct Snapshot {
    pub current_tps: f64,
    pub avg_tps: f64,
    pub total_tokens: u64,
    pub output_tokens: u64,
    pub reasoning_tokens: u64,
    pub input_tokens: u64,
    pub cache_creation_tokens: u64,
    pub cache_read_tokens: u64,
    pub calls_today: u64,
    pub sessions_today: u64,
    pub is_live: bool,
    /// Call not yet persisted but inferred (from call interval) to still be generating;
    /// speed is the window-fallback value.
    pub is_estimating: bool,
    /// Real-time streaming has started but the 30s sliding window is not yet full
    /// (reading comes from the already-active interval; frontend shows "calculating").
    pub ramping: bool,
    /// Call has started but has not yet produced its first byte (TTFT / pipe produced no observed increment):
    /// show a "calculating…" hint instead of a misleading estimated value;
    /// frontend gauge / desktop companion shows ….
    pub is_starting: bool,
    /// True speed of completed calls in the last 10 minutes (persisted-call basis, same source as the speed curve).
    /// Some calls produced no increment bytes in the UI pipe during their lifetime (IO measurement unavailable);
    /// this is used as a fallback display value.
    pub window_tps: f64,
    /// True speed of the most recently completed call (persisted-call basis: output+reasoning / pure generation duration).
    /// 0 when there are no completed calls today; the small table in the top-right corner of the current-speed card
    /// uses it to display "last round".
    pub last_call_tps: f64,
    /// Peak single-call speed over the last 7 local calendar days (t/s). Admission criteria: see HistoryStats (valid first_token,
    /// pure generation >= 1s, effective output >= 300 tokens — millisecond-scale small calls have noisy timestamps
    /// and are not recorded). 0 when no qualifying calls exist within the window;
    /// shown as "Peak" in the small table at the bottom-right of the current-speed card.
    pub hist_max_tps: f64,
    /// 7-day average speed: Σeff ÷ Σgen_s of completed calls in the window (same basis as today's average,
    /// no filtering; slides off by local calendar day). Shown as "History" in the small table at the top-right of today's-average card.
    pub hist_avg_tps: f64,
    /// Current speed source: "io" = process-stream real measurement / "window" = window fallback / "idle" = idle.
    pub live_source: String,
    pub last_activity_ms: i64,
    pub now_ms: i64,
    pub rollout_dir: String,
    pub spark: Vec<f64>,
    /// Concurrent-task per-process detail (filled by the real-time path; frontend shows the task list when >= 2).
    pub tasks: Vec<TaskStat>,
    // ---- Network traffic monitoring (filled by netio.rs; basis described in that module's comments) ----
    /// Whether machine-wide interface counters are available (false on stub platforms; frontend hides the network card).
    pub net_available: bool,
    /// Machine-wide real-time upload/download speed (B/s, ~1s sliding-window measurement from interface counters).
    pub net_up_bps: f64,
    pub net_down_bps: f64,
    /// Machine-wide today's cumulative upload/download (real, persisted and continued across restarts).
    pub net_up_today: u64,
    pub net_down_today: u64,
    /// Session traffic estimate (approximate, token × factor): today's upload (request body) / download (streaming response).
    pub net_sess_up_today: u64,
    pub net_sess_down_today: u64,
    /// Accepted snapshot artifact bytes today (true lower bound of non-session upload, post-encryption-compression).
    pub net_ckpt_today: u64,
    pub net_ckpt_today_count: u32,
    /// List of today's accepted artifacts (time / workspace / size — answers "which ones", persisted across restarts).
    pub net_ckpt_today_list: Vec<CkptStat>,
    /// Whether any snapshot upload is in progress (activeUpload).
    pub net_ckpt_uploading: bool,
    /// checkpoints directory status: ok / missing / blocked (ACL restriction).
    pub net_ckpt_status: String,
    /// Snapshot upload records (list of most recent artifact status per workspace).
    pub net_ckpt_list: Vec<CkptStat>,
    /// Whether connection ownership attribution is available (Windows only).
    pub net_conns_available: bool,
    /// Connection count and detail for the session group (CLI processes) / desktop group (other zcode.exe)
    /// (each entry has remote + owning pid + process-type label; both groups are ZCode's own processes).
    pub net_cli_conns: u32,
    pub net_app_conns: u32,
    pub net_cli_conn_list: Vec<ConnStat>,
    pub net_app_conn_list: Vec<ConnStat>,
}

/// Current-speed statistics window.
const LIVE_WINDOW_MS: i64 = 10 * 60 * 1000;
/// If the time since the last completed call exceeds this, treat as idle and zero the current speed.
/// Estimate window: infer "still generating" from the median of today's inter-completion intervals;
/// beyond that, treat as idle.
const ESTIMATE_MIN_MS: i64 = 20 * 1000;
const ESTIMATE_MAX_MS: i64 = 240 * 1000;
const ESTIMATE_DEFAULT_MS: i64 = 60 * 1000;
/// Lower bound on very short generation durations, to avoid division-by-zero / extreme spikes.
const MIN_DUR_MS: i64 = 50;
/// Speed sparkline: 15 minutes, one bucket per 10 seconds (aligned to wall-clock boundaries for smooth frontend scrolling).
const SPARK_BUCKETS: usize = 90;
const SPARK_BUCKET_MS: i64 = 10_000;

pub fn home_dir() -> Option<PathBuf> {
    std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .map(PathBuf::from)
}

pub fn usage_db_path() -> Option<PathBuf> {
    home_dir().map(|h| h.join(".zcode").join("cli").join("db").join("db.sqlite"))
}

fn now_ms() -> i64 {
    Utc::now().timestamp_millis()
}

fn local_midnight_utc_ms() -> i64 {
    local_day_start_ms(Utc::now().timestamp_millis())
}

/// Local-day start (UTC ms) for a given timestamp. Falls back to UTC day boundary on DST ambiguity/nonexistent moments.
fn local_day_start_ms(ts_ms: i64) -> i64 {
    let fallback = || ts_ms - ts_ms.rem_euclid(86_400_000);
    let Some(dt) = Local.timestamp_millis_opt(ts_ms).single() else {
        return fallback();
    };
    let tz = dt.timezone();
    dt.date_naive()
        .and_time(NaiveTime::MIN)
        .and_local_timezone(tz)
        .single()
        .map(|d| d.with_timezone(&Utc).timestamp_millis())
        .unwrap_or_else(fallback)
}

/// Historical stats window start (inclusive): today midnight minus HIST_WINDOW_DAYS-1 local calendar days.
/// Uses calendar-day rollback (Days::new) rather than millisecond delta — DST would make ms delta land on adjacent days.
fn hist_window_cutoff(today_start_ms: i64) -> i64 {
    let fallback = || today_start_ms - (HIST_WINDOW_DAYS - 1) * 86_400_000;
    let Some(today) = Local.timestamp_millis_opt(today_start_ms).single() else {
        return fallback();
    };
    let tz = today.timezone();
    let Some(day) = today.date_naive().checked_sub_days(Days::new((HIST_WINDOW_DAYS - 1) as u64))
    else {
        return fallback();
    };
    day.and_time(NaiveTime::MIN)
        .and_local_timezone(tz)
        .single()
        .map(|d| d.with_timezone(&Utc).timestamp_millis())
        .unwrap_or_else(fallback)
}

/// Today's aggregator: holds all calls for the day and computes all metrics (pure function, easy to test).
pub struct Aggregator {
    pub calls: Vec<Call>,
    pub today_ymd: (i32, u32, u32),
}

/// Historical statistics (last HIST_WINDOW_DAYS local calendar days, including today): one baseline scan
/// of pre-today rows within the window at startup, then incrementally accumulated with each poll and
/// slid off by local calendar day — crossing midnight discards the earliest whole-day bucket.
/// Historical average uses the same basis as today's average (no filtering);
/// historical peak has admission criteria — real-world millisecond-scale small calls (e.g. 24ms / 79 tokens)
/// in the measured DB can produce false records of thousands of t/s
/// (same rationale as excluding < 300 token calls from calibration samples).
/// Per-day bucketing rather than a pure accumulator, specifically so old calls can be retired
/// from Σ and the peak on expiry.
#[derive(Clone, Debug, Default)]
pub struct HistoryStats {
    /// One bucket per local day (ascending by day_start; baseline by completed_at ASC, increments only append new days),
    /// only retains days within the window. Aggregation and eviction are independent of bucket order.
    pub days: Vec<HistDay>,
}

/// Single-day aggregate bucket: peak/average accumulated per day, whole bucket discarded on expiry.
#[derive(Clone, Copy, Debug, Default)]
pub struct HistDay {
    /// Local-day start (UTC ms).
    pub day_start: i64,
    /// Σeff of completed calls on this day (numerator of historical average).
    pub total_eff: u64,
    /// Σgen_ms on this day (denominator of historical average;
    /// first_token missing falls back to duration then max(50)).
    pub total_gen_ms: i64,
    /// Peak single-call speed on this day (t/s): only counts calls with valid first_token, gen >= 1s, and eff >= 300.
    pub max_tps: f64,
}

/// Historical stats window: last 7 local calendar days (including today), sliding expiry at midnight each day.
pub const HIST_WINDOW_DAYS: i64 = 7;

/// Historical peak admission: lower bound on pure generation duration (ms) — short calls have noisy timestamps.
const HIST_MAX_MIN_GEN_MS: i64 = 1000;
/// Historical peak admission: lower bound on effective output tokens (same value as calibration sample admission).
const HIST_MAX_MIN_EFF: u64 = 300;

impl HistoryStats {
    /// Evict buckets before the window start (inclusive) — called every poll, bucket count <= window days.
    pub fn prune(&mut self, cutoff_day_start: i64) {
        self.days.retain(|b| b.day_start >= cutoff_day_start);
    }

    /// Accumulate one completed call into its local-day bucket (shared by baseline scan and per-poll incremental ingestion).
    /// gen_ms is the final poll-basis value (use completed-ft when ft is valid, otherwise duration fallback then max(50)).
    pub fn fold(&mut self, day_start: i64, ft: Option<i64>, completed_ms: i64, gen_ms: i64, eff: u64) {
        let idx = match self.days.iter().position(|b| b.day_start == day_start) {
            Some(i) => i,
            None => {
                self.days.push(HistDay { day_start, ..Default::default() });
                self.days.len() - 1
            }
        };
        let b = &mut self.days[idx];
        b.total_eff += eff;
        b.total_gen_ms += gen_ms.max(MIN_DUR_MS);
        // Peak-record admission: ft must be genuinely valid (duration-fallback rows are not trustworthy) + both lower bounds.
        let real_ft = matches!(ft, Some(f) if completed_ms > f);
        if real_ft && gen_ms >= HIST_MAX_MIN_GEN_MS && eff >= HIST_MAX_MIN_EFF {
            let tps = eff as f64 * 1000.0 / gen_ms as f64;
            if tps > b.max_tps {
                b.max_tps = tps;
            }
        }
    }

    pub fn total_eff(&self) -> u64 {
        self.days.iter().map(|b| b.total_eff).sum()
    }

    pub fn total_gen_ms(&self) -> i64 {
        self.days.iter().map(|b| b.total_gen_ms).sum()
    }

    pub fn avg_tps(&self) -> f64 {
        let gen = self.total_gen_ms();
        if gen > 0 {
            self.total_eff() as f64 / (gen as f64 / 1000.0)
        } else {
            0.0
        }
    }

    pub fn max_tps(&self) -> f64 {
        self.days.iter().map(|b| b.max_tps).fold(0.0, f64::max)
    }
}

/// Poll-basis pure generation duration: use completed-ft when ft is valid, otherwise duration_ms (only if > 0),
/// finally max(50) fallback. Shared by baseline scan and incremental ingestion to keep both paths consistent.
fn gen_ms_from(ft: Option<i64>, completed: i64, dur: Option<i64>) -> i64 {
    match ft {
        Some(f) if completed > f => completed - f,
        _ => dur.filter(|d| *d > 0).unwrap_or(MIN_DUR_MS),
    }
    .max(MIN_DUR_MS)
}


impl Aggregator {
    pub fn new() -> Self {
        Self {
            calls: Vec::new(),
            today_ymd: {
                let n = Local::now();
                (n.year(), n.month(), n.day())
            },
        }
    }

    pub fn ingest(&mut self, call: Call) {
        self.calls.push(call);
    }

    /// Cross-day cleanup: clear today's accumulators.
    pub fn rollover_if_needed(&mut self) {
        let n = Local::now();
        let ymd = (n.year(), n.month(), n.day());
        if ymd != self.today_ymd {
            self.today_ymd = ymd;
            self.calls.clear();
        }
    }

    pub fn calls(&self) -> &[Call] {
        &self.calls
    }

    pub fn snapshot(&self) -> Snapshot {
        let now = now_ms();
        let mut out_total = 0u64;
        let mut reason_total = 0u64;
        let mut input_total = 0u64;
        let mut cc_total = 0u64;
        let mut cr_total = 0u64;
        let mut dur_total = 0i64;
        let mut w_out = 0u64;
        let mut w_dur = 0i64;
        let mut last_completed = 0i64;
        // Numerator / denominator of the most recently completed call, for the "last round call speed" corner badge.
        let mut last_eff = 0u64;
        let mut last_gen = 0i64;
        let mut sessions: HashSet<&str> = HashSet::new();
        // Bucket aligned to wall-clock 10s boundaries: bucket index = slot-of-completion minus current-slot.
        let now_slot = now.div_euclid(SPARK_BUCKET_MS);
        let mut buckets = vec![(0u64, 0i64); SPARK_BUCKETS];

        for c in &self.calls {
            out_total += c.output;
            reason_total += c.reasoning;
            input_total += c.input;
            cc_total += c.cache_creation;
            cr_total += c.cache_read;
            dur_total += c.gen_ms.max(MIN_DUR_MS);
            if !c.session.is_empty() {
                sessions.insert(c.session.as_str());
            }
            if c.completed_ms >= last_completed {
                last_completed = c.completed_ms;
                last_eff = c.effective_out();
                last_gen = c.gen_ms.max(MIN_DUR_MS);
            }
            if c.completed_ms >= now - LIVE_WINDOW_MS {
                w_out += c.effective_out();
                w_dur += c.gen_ms.max(MIN_DUR_MS);
            }
            let slot = (now_slot - c.completed_ms.div_euclid(SPARK_BUCKET_MS)) as usize;
            if slot < SPARK_BUCKETS {
                let b = &mut buckets[SPARK_BUCKETS - 1 - slot];
                b.0 += c.effective_out();
                b.1 += c.gen_ms.max(MIN_DUR_MS);
            }
        }

        // Estimate window: median of today's inter-completion intervals (clamped to 20s~240s),
        // used during long thinking / long output periods (call not yet persisted) to keep displaying
        // a fallback value at the recent speed.
        let mut comps: Vec<i64> = self.calls.iter().map(|c| c.completed_ms).collect();
        comps.sort_unstable();
        comps.dedup();
        let mut gaps: Vec<i64> = comps
            .windows(2)
            .map(|w| w[1] - w[0])
            .filter(|g| *g > 0 && *g < 600_000)
            .collect();
        let grace_ms = if gaps.len() >= 3 {
            let start = gaps.len().saturating_sub(10);
            let tail = &mut gaps[start..];
            tail.sort_unstable();
            (tail[tail.len() / 2]).clamp(ESTIMATE_MIN_MS, ESTIMATE_MAX_MS)
        } else {
            ESTIMATE_DEFAULT_MS
        };

        let since = now - last_completed;
        // is_live is determined solely by real-time IO measurement (overwritten in main);
        // here we provide the window fallback based on call intervals.
        let is_estimating = last_completed > 0 && since <= grace_ms && w_dur > 0;
        let current_tps = if is_estimating && w_dur > 0 {
            w_out as f64 / (w_dur as f64 / 1000.0)
        } else {
            0.0
        };
        let avg_tps = if dur_total > 0 {
            (out_total + reason_total) as f64 / (dur_total as f64 / 1000.0)
        } else {
            0.0
        };
        let mut spark: Vec<f64> = buckets
            .iter()
            .map(|(o, d)| {
                if *d > 0 {
                    *o as f64 / (*d as f64 / 1000.0)
                } else {
                    0.0
                }
            })
            .collect();
        // During the estimate period, temporarily fill the rightmost bucket (the not-yet-persisted current call)
        // with the fallback value; it gets replaced by real data once the call completes.
        if is_estimating {
            if let Some(last) = spark.last_mut() {
                if *last <= 0.0 {
                    *last = current_tps;
                }
            }
        }
        let window_tps = if w_dur > 0 {
            w_out as f64 / (w_dur as f64 / 1000.0)
        } else {
            0.0
        };
        let last_call_tps = if last_gen > 0 {
            last_eff as f64 / (last_gen as f64 / 1000.0)
        } else {
            0.0
        };

        // Total-token basis matches ZCode official stats: input + output + reasoning + cache_creation;
        // cache hits (cache_read) represent prompt reuse, not new usage — displayed separately, not counted in the total.
        Snapshot {
            current_tps,
            avg_tps,
            total_tokens: out_total + reason_total + input_total + cc_total,
            output_tokens: out_total,
            reasoning_tokens: reason_total,
            input_tokens: input_total,
            cache_creation_tokens: cc_total,
            cache_read_tokens: cr_total,
            calls_today: self.calls.len() as u64,
            sessions_today: sessions.len() as u64,
            is_live: false,
            is_estimating,
            ramping: false,
            is_starting: false,
            window_tps,
            last_call_tps,
            hist_max_tps: 0.0,
            hist_avg_tps: 0.0,
            live_source: if is_estimating {
                "window".to_string()
            } else {
                "idle".to_string()
            },
            last_activity_ms: last_completed,
            now_ms: now,
            rollout_dir: String::new(),
            spark,
            tasks: Vec::new(),
            net_available: false,
            net_up_bps: 0.0,
            net_down_bps: 0.0,
            net_up_today: 0,
            net_down_today: 0,
            net_sess_up_today: 0,
            net_sess_down_today: 0,
            net_ckpt_today: 0,
            net_ckpt_today_count: 0,
            net_ckpt_today_list: Vec::new(),
            net_ckpt_uploading: false,
            net_ckpt_status: String::new(),
            net_ckpt_list: Vec::new(),
            net_conns_available: false,
            net_cli_conns: 0,
            net_app_conns: 0,
            net_cli_conn_list: Vec::new(),
            net_app_conn_list: Vec::new(),
        }
    }
}

/// ZCode usage database (read-only WAL) polling engine.
pub struct Engine {
    conn: Option<rusqlite::Connection>,
    agg: Aggregator,
    ingested: HashSet<String>,
    /// Historical stats (last 7 local calendar days): first poll scans pre-today rows within the window
    /// as a baseline, then incrementally accumulates with each poll's new calls and slides off by local day.
    hist: HistoryStats,
    hist_loaded: bool,
    pub db_path: Option<PathBuf>,
}

impl Engine {
    pub fn new() -> Self {
        let db_path = usage_db_path();
        let conn = db_path.as_ref().and_then(|p| {
            match rusqlite::Connection::open_with_flags(
                p,
                OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
            ) {
                Ok(c) => {
                    // Key WAL concurrency and read performance optimizations:
                    // 1. Set busy_timeout to 3 seconds to avoid returning SQLITE_BUSY immediately
                    //    when the ZCode CLI holds a write transaction or is checkpointing.
                    // 2. Enable query_only to guarantee read-only access.
                    let _ = c.busy_timeout(std::time::Duration::from_millis(3000));
                    let _ = c.execute_batch("PRAGMA query_only = ON;");
                    Some(c)
                }
                Err(e) => {
                    eprintln!("[zcode-speed-panel] usage DB open failed: {e}");
                    None
                }
            }
        });
        Self {
            conn,
            agg: Aggregator::new(),
            ingested: HashSet::new(),
            hist: HistoryStats::default(),
            hist_loaded: false,
            db_path,
        }
    }

    pub fn data_source_label(&self) -> String {
        self.db_path
            .as_ref()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_else(|| "(~/.zcode/cli/db/db.sqlite not found)".into())
    }

    /// Poll the usage database, incrementally ingesting completed calls from today.
    /// Returns the newly added calls this round (for real-time IO module calibration).
    pub fn poll(&mut self) -> Vec<Call> {
        self.agg.rollover_if_needed();
        let today_start_ms = local_midnight_utc_ms();
        // Reset the ingested set on a day boundary.
        if self.agg.calls.is_empty() && !self.ingested.is_empty() {
            self.ingested.clear();
        }
        let Some(conn) = &self.conn else {
            return Vec::new();
        };
        // Historical baseline: first poll scans pre-today completed rows within the window (one local SQLite
        // read, milliseconds; today's rows are accumulated via the incremental path below, not double-counted).
        // Borrow conn and hist as separate fields to avoid a whole-self mut/immut borrow conflict.
        if !self.hist_loaded {
            self.hist_loaded = true;
            Self::scan_history_before(conn, &mut self.hist, today_start_ms);
        }
        // Sliding expiry: discard the earliest whole-day bucket on crossing midnight (every poll, bucket count <= window days)
        self.hist.prune(hist_window_cutoff(today_start_ms));
        let mut new_calls = Vec::new();
        let sql = concat!(
            "SELECT id, started_at, first_token_at, completed_at, duration_ms, ",
            "output_tokens, reasoning_tokens, input_tokens, ",
            "cache_creation_input_tokens, cache_read_input_tokens, session_id ",
            "FROM model_usage WHERE status='completed' AND completed_at >= ?1 ",
            "ORDER BY completed_at ASC"
        );
        let mut stmt = match conn.prepare_cached(sql) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("[zcode-speed-panel] usage DB prepare failed: {e}");
                return Vec::new();
            }
        };
        let rows = stmt
            .query_map([today_start_ms], |r| {
            let id: String = r.get(0)?;
            let started: i64 = r.get(1)?;
            let ft: Option<i64> = r.get(2)?;
            let completed: i64 = r.get(3)?;
            let dur: Option<i64> = r.get(4)?;
            // rusqlite does not support u64 column reads; read as i64 and cast.
            let out: i64 = r.get::<_, Option<i64>>(5)?.unwrap_or(0);
            let reason: i64 = r.get::<_, Option<i64>>(6)?.unwrap_or(0);
            let input: i64 = r.get::<_, Option<i64>>(7)?.unwrap_or(0);
            let cc: i64 = r.get::<_, Option<i64>>(8)?.unwrap_or(0);
            let cr: i64 = r.get::<_, Option<i64>>(9)?.unwrap_or(0);
            let session: String = r.get(10)?;
            Ok((
                id,
                started,
                ft,
                completed,
                dur,
                out.max(0) as u64,
                reason.max(0) as u64,
                input.max(0) as u64,
                cc.max(0) as u64,
                cr.max(0) as u64,
                session,
            ))
        });
        let rows = match rows {
            Ok(r) => r,
            Err(e) => {
                eprintln!("[zcode-speed-panel] usage DB query failed: {e}");
                return Vec::new();
            }
        };
        for row in rows.flatten() {
            let (id, started, ft, completed, dur, out, reason, input, cc, cr, session) = row;
            if self.ingested.contains(&id) {
                continue;
            }
            self.ingested.insert(id.clone());
            let gen_ms = gen_ms_from(ft, completed, dur);
            // Historical stats incremental accumulation (today's calls are always within the window; peak goes through
            // HistoryStats::fold admission criteria).
            let eff = out + reason;
            let day = local_day_start_ms(completed);
            self.hist.fold(day, ft, completed, gen_ms, eff);
            self.agg.ingest(Call {
                id,
                started_ms: started,
                first_token_ms: ft,
                completed_ms: completed,
                gen_ms,
                output: out,
                reasoning: reason,
                input,
                cache_creation: cc,
                cache_read: cr,
                session,
            });
            new_calls.push(self.agg.calls.last().unwrap().clone());
        }
        // Today's aggregation only retains today's data (ingested set is reset on cross-day rollover).
        self.agg.calls.retain(|c| c.started_ms >= today_start_ms);
        new_calls
    }

    pub fn snapshot(&self) -> Snapshot {
        let mut s = self.agg.snapshot();
        s.rollout_dir = self.data_source_label();
        s.hist_max_tps = self.hist.max_tps();
        s.hist_avg_tps = self.hist.avg_tps();
        s
    }

    /// Historical stats baseline scan: accumulate completed rows from window start (inclusive) to today midnight.
    /// Window start = today midnight minus HIST_WINDOW_DAYS-1 local calendar days (DST-safe),
    /// which is exactly a local-day boundary, so completed_at >= start means "local day within window".
    /// On failure, log only — do not panic (historical badge shows 0; today's incremental path proceeds normally).
    fn scan_history_before(
        conn: &rusqlite::Connection,
        hist: &mut HistoryStats,
        today_start_ms: i64,
    ) {
        let cutoff = hist_window_cutoff(today_start_ms);
        let sql = concat!(
            "SELECT first_token_at, completed_at, duration_ms, ",
            "output_tokens, reasoning_tokens FROM model_usage ",
            "WHERE status='completed' AND completed_at < ?1 AND completed_at >= ?2"
        );
        let mut query = || -> rusqlite::Result<()> {
            let mut stmt = conn.prepare_cached(sql)?;
            let rows = stmt.query_map([today_start_ms, cutoff], |r| {
                Ok((
                    r.get::<_, Option<i64>>(0)?,
                    r.get::<_, i64>(1)?,
                    r.get::<_, Option<i64>>(2)?,
                    r.get::<_, Option<i64>>(3)?.unwrap_or(0).max(0) as u64,
                    r.get::<_, Option<i64>>(4)?.unwrap_or(0).max(0) as u64,
                ))
            })?;
            for row in rows.flatten() {
                let (ft, completed, dur, out, reason) = row;
                let gen = gen_ms_from(ft, completed, dur);
                let day = local_day_start_ms(completed);
                hist.fold(day, ft, completed, gen, out + reason);
            }
            Ok(())
        };
        if let Err(e) = query() {
            eprintln!("[zcode-speed-panel] historical baseline scan failed: {e}");
        }
    }

    /// All calls ingested today (for the real-time module to determine the current session).
    pub fn calls(&self) -> &[Call] {
        &self.agg.calls()
    }

    /// Whether any call is in progress: check the latest assistant message row of recently active sessions.
    /// A message row is committed at call-start time (readable within <=200ms); the `time` object inside
    /// the row's `data` gets its `completed` field written when the call ends (including cancel/error) —
    /// faster than the model_usage completion row, and covers status='cancelled'/'error'
    /// (those calls never get a completed-status row; under the old basis they would be stuck on "generating"
    /// until the 10-minute fallback).
    /// Returns all in-flight (session, call-start-time) pairs, sorted by start time descending —
    /// when multiple tasks run concurrently (multiple windows / sub-agent sessions), the real-time speed
    /// is aggregated by process set rather than picking only the latest one.
    /// 10-minute cap as a fallback for rows whose completed field was never written after a crash.
    pub fn call_in_flight(&self) -> Vec<(String, i64)> {
        let Some(conn) = self.conn.as_ref() else {
            return Vec::new();
        };
        // Recently active sessions (session table ~1k rows; scanning a small table ordered by time_updated DESC is acceptable;
        // cap at 16: multi-task aggregation must cover all in-progress sessions; >6 concurrent sub-agents must not be missed);
        // the message table lacks a time_created single-column index, so a global ORDER BY is not feasible (~200ms per query in practice).
        let mut stmt = match conn.prepare_cached(
            "SELECT id FROM session ORDER BY time_updated DESC LIMIT 16",
        ) {
            Ok(s) => s,
            Err(_) => return Vec::new(),
        };
        let sessions: Vec<String> = match stmt.query_map([], |r| r.get::<_, String>(0)) {
            Ok(rows) => rows.flatten().collect(),
            Err(_) => return Vec::new(),
        };
        drop(stmt);

        let mut cands: Vec<(String, i64, bool)> = Vec::new();
        for sess in &sessions {
            // Per session, only look at the latest assistant row (uses the (session_id, time_created) composite index).
            let Ok(mut stmt) = conn.prepare_cached(
                "SELECT time_created, substr(data,1,120) FROM message \
                 WHERE session_id = ?1 ORDER BY time_created DESC LIMIT 8",
            ) else {
                continue;
            };
            let Ok(rows) = stmt.query_map([sess], |r| {
                Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?))
            }) else {
                continue;
            };
            for (created, prefix) in rows.flatten() {
                if !prefix.contains("\"assistant\"") {
                    continue;
                }
                let done = prefix.contains("\"completed\"");
                cands.push((sess.clone(), created, done));
                break;
            }
        }
        inflight_from_rows(&cands, Utc::now().timestamp_millis())
    }

    /// Model speed trend: read-only query over completed model_usage rows in the window,
    /// aggregated by model x time bucket. Computed on read (zero local storage, no index/writes to the usage DB);
    /// conn missing or query failure returns an empty payload (no panic). Aggregation basis: see aggregate_model_stats.
    pub fn model_stats(&self, window_min: i64) -> ModelStatsPayload {
        let window_min = clamp_chart_window(window_min);
        let now = now_ms();
        let Some(conn) = &self.conn else {
            eprintln!("[zcode-speed-panel] model_stats: usage DB unavailable");
            return empty_model_stats(window_min, now);
        };
        // Note: the model column in the model_usage table is actually named model_id (verified via PRAGMA table_info; there is no `model` column).
        let sql = concat!(
            "SELECT model_id, first_token_at, completed_at, duration_ms, ",
            "output_tokens, reasoning_tokens FROM model_usage ",
            "WHERE status='completed' AND completed_at >= ?1 ORDER BY completed_at ASC"
        );
        let cutoff = now - window_min * 60_000;
        let query = || -> rusqlite::Result<Vec<ModelUsageRow>> {
            let mut stmt = conn.prepare_cached(sql)?;
            let rows = stmt.query_map([cutoff], |r| {
                let model: String = r.get(0)?;
                let ft: Option<i64> = r.get(1)?;
                let completed: i64 = r.get(2)?;
                let dur: Option<i64> = r.get(3)?;
                // rusqlite does not support u64 column reads; read as i64 and cast (same basis as poll).
                let out: i64 = r.get::<_, Option<i64>>(4)?.unwrap_or(0);
                let reason: i64 = r.get::<_, Option<i64>>(5)?.unwrap_or(0);
                Ok(ModelUsageRow {
                    model,
                    first_token_at: ft,
                    completed_at: completed,
                    duration_ms: dur,
                    output_tokens: out.max(0) as u64,
                    reasoning_tokens: reason.max(0) as u64,
                })
            })?;
            Ok(rows.flatten().collect())
        };
        match query() {
            Ok(rows) => aggregate_model_stats(rows, window_min, now),
            Err(e) => {
                eprintln!("[zcode-speed-panel] model_stats query failed: {e}");
                empty_model_stats(window_min, now)
            }
        }
    }
}

/// Message gate pure evaluation: candidate (session, assistant-row creation time, whether it carries `completed`).
/// Per session, only the latest assistant row is considered (older unfinished rows are crash remnants,
/// already superseded by a newer row); among those, every session whose latest row is unfinished and fresh
/// is treated as in-progress (each counted independently for concurrent multi-task, to be aggregated by
/// process set in the real-time path), returned sorted by creation time descending.
pub(crate) fn inflight_from_rows(
    cands: &[(String, i64, bool)],
    now_ms: i64,
) -> Vec<(String, i64)> {
    let mut newest: HashMap<&str, &(String, i64, bool)> = HashMap::new();
    for row in cands {
        match newest.get(row.0.as_str()) {
            Some(prev) if prev.1 >= row.1 => {}
            _ => {
                newest.insert(row.0.as_str(), row);
            }
        }
    }
    let mut out: Vec<(String, i64)> = newest
        .values()
        .filter(|(_, created, done)| !done && now_ms - *created <= 600_000)
        .map(|(s, c, _)| (s.clone(), *c))
        .collect();
    out.sort_unstable_by(|a, b| b.1.cmp(&a.1));
    out
}

// ============ Model speed trend (aggregated by model x time bucket, for the chart card's model detail view; zero local storage) ============
// Time spec fully shared with chart_stats below: same set of window choices (15/60/360/1440 minutes),
// same bucket count (CHART_BUCKETS=90) and bucket width — when toggling the two views the x-axis aligns
// pixel-for-pixel; only the series change, the scale stays identical.

/// Single-model single-bucket aggregation.
#[derive(Serialize, Clone, Debug, Default, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ModelBucket {
    /// This bucket's tps = Σ(output+reasoning) ÷ Σ pure-generation seconds; 0 when no calls.
    pub tps: f64,
    pub calls: u64,
    pub tokens: u64,
}

/// One model's trend series + window summary.
#[derive(Serialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct ModelSeries {
    pub model: String,
    /// Always CHART_BUCKETS (90); 0 = newest bucket.
    pub buckets: Vec<ModelBucket>,
    pub total_calls: u64,
    pub total_tokens: u64,
    /// Full-window Σeff ÷ Σgen_s.
    pub avg_tps: f64,
    /// Maximum of per-bucket tps values.
    pub peak_tps: f64,
    /// This model's eff share of all models' eff (0~1).
    pub share: f64,
}

/// Return payload for the model_stats command.
#[derive(Serialize, Clone, Debug, Default)]
#[serde(rename_all = "camelCase")]
pub struct ModelStatsPayload {
    pub window_min: i64,
    pub bucket_ms: i64,
    pub now_ms: i64,
    /// Sorted by total_tokens descending.
    pub series: Vec<ModelSeries>,
}

/// A model_usage query row (input to the aggregation pure function).
struct ModelUsageRow {
    model: String,
    first_token_at: Option<i64>,
    completed_at: i64,
    duration_ms: Option<i64>,
    output_tokens: u64,
    reasoning_tokens: u64,
}

/// Raw per-model-per-bucket accumulator: (effective output tokens, generation ms, call count).
struct BucketAcc {
    eff: u64,
    gen_ms: i64,
    calls: u64,
}

/// Invalid window values snap to the nearest valid choice: directly reuse the chart's clamp_chart_window
/// (both views share the same set of choices; see CHART_WINDOW_CHOICES).

/// Empty payload (returned when conn is missing or query fails; no panic).
fn empty_model_stats(window_min: i64, now_ms: i64) -> ModelStatsPayload {
    ModelStatsPayload {
        window_min,
        bucket_ms: window_min * 60_000 / CHART_BUCKETS as i64,
        now_ms,
        series: Vec::new(),
    }
}

/// Pure function: aggregate completed call rows in the window into per-model 90-bucket trends and summaries (easy to unit-test).
/// Time spec (window choices / bucket count / bucket width) is fully consistent with aggregate_chart_stats.
/// - bucket index = now ÷ bucket_ms − completed ÷ bucket_ms (div_euclid, absolute wall-clock slot alignment,
///   same basis as chart_stats / today's spark: bucket boundaries are pinned to exact multiples of real time,
///   so when two queries land in the same slot their bucket contents are identical — the trend only translates
///   over time, never deforms);
///   0 = newest bucket, 89 = oldest bucket; out-of-bounds (including future beyond one slot) are dropped;
/// - gen_ms = completed − first_token; when first_token is missing or non-positive, fall back to duration_ms,
///   then max(50) fallback (same basis as today's aggregation);
/// - eff = output + reasoning; bucket tps = Σeff ÷ Σgen_s, 0 when no calls;
/// - series sorted by total_tokens descending.
fn aggregate_model_stats(
    rows: Vec<ModelUsageRow>,
    window_min: i64,
    now_ms: i64,
) -> ModelStatsPayload {
    let window_min = clamp_chart_window(window_min);
    let bucket_ms = window_min * 60_000 / CHART_BUCKETS as i64;
    // model -> (per-bucket accumulator, total eff, total gen_ms, total call count)
    let mut per_model: HashMap<String, (Vec<BucketAcc>, u64, i64, u64)> = HashMap::new();
    for r in rows {
        let gen = match r.first_token_at {
            Some(f) if r.completed_at > f => r.completed_at - f,
            _ => r.duration_ms.filter(|d| *d > 0).unwrap_or(MIN_DUR_MS),
        }
        .max(MIN_DUR_MS);
        let eff = r.output_tokens + r.reasoning_tokens;
        let slot = now_ms.div_euclid(bucket_ms) - r.completed_at.div_euclid(bucket_ms);
        if slot < 0 || slot as usize >= CHART_BUCKETS {
            continue;
        }
        let entry = per_model.entry(r.model).or_insert_with(|| {
            (
                (0..CHART_BUCKETS)
                    .map(|_| BucketAcc { eff: 0, gen_ms: 0, calls: 0 })
                    .collect(),
                0,
                0,
                0,
            )
        });
        let b = &mut entry.0[slot as usize];
        b.eff += eff;
        b.gen_ms += gen;
        b.calls += 1;
        entry.1 += eff;
        entry.2 += gen;
        entry.3 += 1;
    }

    let grand_eff: u64 = per_model.values().map(|e| e.1).sum();
    let mut series: Vec<ModelSeries> = per_model
        .into_iter()
        .map(|(model, (buckets, total_eff, total_gen, total_calls))| {
            let mut peak = 0.0f64;
            let buckets: Vec<ModelBucket> = buckets
                .into_iter()
                .map(|b| {
                    let tps = if b.gen_ms > 0 {
                        b.eff as f64 / (b.gen_ms as f64 / 1000.0)
                    } else {
                        0.0
                    };
                    if tps > peak {
                        peak = tps;
                    }
                    ModelBucket { tps, calls: b.calls, tokens: b.eff }
                })
                .collect();
            ModelSeries {
                model,
                buckets,
                total_calls,
                total_tokens: total_eff,
                avg_tps: if total_gen > 0 {
                    total_eff as f64 / (total_gen as f64 / 1000.0)
                } else {
                    0.0
                },
                peak_tps: peak,
                share: if grand_eff > 0 {
                    total_eff as f64 / grand_eff as f64
                } else {
                    0.0
                },
            }
        })
        .collect();
    series.sort_by(|a, b| b.total_tokens.cmp(&a.total_tokens).then(a.model.cmp(&b.model)));
    ModelStatsPayload { window_min, bucket_ms, now_ms, series }
}

// ============ Output speed curve (selectable time range, for the chart card; zero local storage) ============

/// Valid chart time ranges (minutes): 15 minutes / 1 hour / 6 hours / 24 hours.
const CHART_WINDOW_CHOICES: [i64; 4] = [15, 60, 360, 1440];
/// Chart uses a uniform 90 buckets (same density as today's spark):
/// 15m -> 10s, 1h -> 40s, 6h -> 4min, 24h -> 16min.
const CHART_BUCKETS: usize = 90;

/// Return payload for the chart_stats command: a single series (all models merged) of tps by time bucket.
#[derive(Serialize, Clone, Debug, Default)]
#[serde(rename_all = "camelCase")]
pub struct ChartStatsPayload {
    pub window_min: i64,
    pub bucket_ms: i64,
    pub now_ms: i64,
    /// Always 90 buckets, old-to-new order (0 = oldest bucket, last = newest bucket).
    pub buckets: Vec<f64>,
}

/// Snap an invalid window value to the nearest valid choice (15 / 60 / 360 / 1440 minutes).
fn clamp_chart_window(window_min: i64) -> i64 {
    CHART_WINDOW_CHOICES
        .iter()
        .copied()
        .min_by_key(|&w| (w - window_min).abs())
        .unwrap_or(15)
}

/// Pure function: aggregate completed call rows in the window into 90-bucket tps
/// (same basis as today's spark: gen = poll-basis fallback chain; bucket tps = Σeff ÷ Σgen_s). Easy to unit-test.
fn aggregate_chart_stats(
    rows: Vec<ModelUsageRow>,
    window_min: i64,
    now_ms: i64,
) -> ChartStatsPayload {
    let window_min = clamp_chart_window(window_min);
    let bucket_ms = window_min * 60_000 / CHART_BUCKETS as i64;
    let mut acc = vec![(0u64, 0i64); CHART_BUCKETS]; // (Σeff, Σgen_ms)
    for r in rows {
        let gen = gen_ms_from(r.first_token_at, r.completed_at, r.duration_ms);
        let slot = now_ms.div_euclid(bucket_ms) - r.completed_at.div_euclid(bucket_ms);
        if slot < 0 || slot as usize >= CHART_BUCKETS {
            continue;
        }
        let b = &mut acc[CHART_BUCKETS - 1 - slot as usize];
        b.0 += r.output_tokens + r.reasoning_tokens;
        b.1 += gen;
    }
    ChartStatsPayload {
        window_min,
        bucket_ms,
        now_ms,
        buckets: acc
            .into_iter()
            .map(|(eff, gen)| {
                if gen > 0 {
                    eff as f64 / (gen as f64 / 1000.0)
                } else {
                    0.0
                }
            })
            .collect(),
    }
}

impl Engine {
    /// Output speed curve: read-only query over completed model_usage rows in the window,
    /// aggregated into 90-bucket tps (single series, all models merged, old-to-new).
    /// conn missing or query failure returns an all-zero payload.
    pub fn chart_stats(&self, window_min: i64) -> ChartStatsPayload {
        let window_min = clamp_chart_window(window_min);
        let now = now_ms();
        let empty = ChartStatsPayload {
            window_min,
            bucket_ms: window_min * 60_000 / CHART_BUCKETS as i64,
            now_ms: now,
            buckets: vec![0.0; CHART_BUCKETS],
        };
        let Some(conn) = &self.conn else {
            return empty;
        };
        // Same query as model_stats (one extra column model_id, ignored during aggregation —
        // keeping the SQL and row structure identical so the prepared-statement cache basis is shared).
        let sql = concat!(
            "SELECT model_id, first_token_at, completed_at, duration_ms, ",
            "output_tokens, reasoning_tokens FROM model_usage ",
            "WHERE status='completed' AND completed_at >= ?1 ORDER BY completed_at ASC"
        );
        let cutoff = now - window_min * 60_000;
        let query = || -> rusqlite::Result<Vec<ModelUsageRow>> {
            let mut stmt = conn.prepare_cached(sql)?;
            let rows = stmt.query_map([cutoff], |r| {
                let model: String = r.get(0)?;
                let ft: Option<i64> = r.get(1)?;
                let completed: i64 = r.get(2)?;
                let dur: Option<i64> = r.get(3)?;
                let out: i64 = r.get::<_, Option<i64>>(4)?.unwrap_or(0);
                let reason: i64 = r.get::<_, Option<i64>>(5)?.unwrap_or(0);
                Ok(ModelUsageRow {
                    model,
                    first_token_at: ft,
                    completed_at: completed,
                    duration_ms: dur,
                    output_tokens: out.max(0) as u64,
                    reasoning_tokens: reason.max(0) as u64,
                })
            })?;
            Ok(rows.flatten().collect())
        };
        match query() {
            Ok(rows) => aggregate_chart_stats(rows, window_min, now),
            Err(e) => {
                eprintln!("[zcode-speed-panel] chart_stats query failed: {e}");
                empty
            }
        }
    }
}

// ============ Tests ============
#[cfg(test)]
mod tests {
    use super::*;

    /// Message gate: latest assistant row without completed -> in-progress;
    /// rows that are completed, aged zombie rows (crash fallback), and "same-session newer completed rows" do not count;
    /// multi-session concurrency (multiple windows / sub-agents) all returned.
    #[test]
    fn inflight_from_rows_gating() {
        let now = 1_000_000i64;
        // First call of a fresh session: row not completed -> in-progress.
        let r = inflight_from_rows(&[("new".into(), now - 3_000, false)], now);
        assert_eq!(r, vec![("new".to_string(), now - 3_000)]);
        // Same session has a newer completed assistant row (old zombie row above) -> does not count.
        assert_eq!(
            inflight_from_rows(
                &[
                    ("a".into(), now - 60_000, false),      // crash remnant
                    ("a".into(), now - 30_000, true),       // session a's latest assistant row
                ],
                now
            ),
            Vec::new()
        );
        // Multi-session concurrency: all in-progress sessions returned, sorted by start time descending
        // (sub-agent sessions b/c started later than main session a; a's current round is already completed).
        let r = inflight_from_rows(
            &[
                ("a".into(), now - 40_000, true),
                ("b".into(), now - 5_000, false),
                ("c".into(), now - 20_000, false),
            ],
            now,
        );
        assert_eq!(
            r,
            vec![
                ("b".to_string(), now - 5_000),
                ("c".to_string(), now - 20_000),
            ]
        );
        // Unfinished but beyond the 10-minute fallback -> considered stopped.
        assert_eq!(
            inflight_from_rows(&[("z".into(), now - 601_000, false)], now),
            Vec::new()
        );
        assert_eq!(inflight_from_rows(&[], now), Vec::new());
    }

    fn call(completed: i64, gen_ms: i64, out: u64, reason: u64, input: u64, session: &str) -> Call {
        Call {
            id: format!("{}-{}", completed, out),
            started_ms: completed - gen_ms - 1000,
            first_token_ms: Some(completed - gen_ms),
            completed_ms: completed,
            gen_ms,
            output: out,
            reasoning: reason,
            input,
            cache_creation: 0,
            cache_read: 0,
            session: session.into(),
        }
    }

    #[test]
    fn snapshot_computes_speeds() {
        let now = now_ms();
        let mut agg = Aggregator::new();
        agg.ingest(call(now - 10_000, 10_000, 500, 40, 100, "a"));
        agg.ingest(call(now - 1_000, 8_000, 240, 60, 100, "a"));
        let s = agg.snapshot();
        // Pure generation rate: (500+40 + 240+60) / 18s = 46.7
        assert!((s.avg_tps - 840.0 / 18.0).abs() < 1e-9);
        assert!((s.current_tps - 840.0 / 18.0).abs() < 1e-9);
        assert!(!s.is_live); // is_live is overwritten in main based solely on real-time IO measurement
        // Total tokens = output 740 + reasoning 100 + input 200
        assert_eq!(s.total_tokens, 1040);
        assert_eq!(s.output_tokens, 740);
        assert_eq!(s.reasoning_tokens, 100);
        assert_eq!(s.sessions_today, 1);
        assert_eq!(s.spark.len(), SPARK_BUCKETS);
        assert_eq!(s.live_source, "window"); // with no IO probe, is_live=false -> window fallback
        // Last-round call speed = most recently completed call (now-1s): eff/gen = 300 / 8s
        assert!((s.last_call_tps - 37.5).abs() < 1e-9);
    }

    /// "Last-round call speed" takes the row with the latest completion time, independent of ingest order
    /// (DB query ordering may vary).
    #[test]
    fn last_call_tps_uses_latest_completed() {
        let now = now_ms();
        let mut agg = Aggregator::new();
        agg.ingest(call(now - 1_000, 4_000, 400, 0, 0, "a")); // 100 t/s
        agg.ingest(call(now - 30_000, 2_000, 100, 0, 0, "b")); // 50 t/s, completed earlier
        agg.ingest(call(now - 20_000, 5_000, 250, 50, 0, "c")); // 60 t/s, still earlier than now-1s
        let s = agg.snapshot();
        assert!((s.last_call_tps - 100.0).abs() < 1e-9);
        // Average speed and "last round" are different bases: total eff 800 / total 11s != 100
        assert!((s.avg_tps - 800.0 / 11.0).abs() < 1e-9);
    }

    #[test]
    fn speed_excludes_time_before_first_token() {
        let now = now_ms();
        let mut agg = Aggregator::new();
        agg.ingest(call(now - 5_000, 5_000, 500, 0, 0, "a"));
        let s1 = agg.snapshot();
        assert!((s1.avg_tps - 100.0).abs() < 1e-9);
        // Call B: request sent 30s ago (long TTFT / queuing), but pure generation is 5s with 500 output.
        agg.ingest(Call {
            id: "b".into(),
            started_ms: now - 30_000,
            first_token_ms: Some(now - 6_000),
            completed_ms: now - 1_000,
            gen_ms: 5_000,
            output: 500,
            reasoning: 0,
            input: 0,
            cache_creation: 0,
            cache_read: 0,
            session: "b".into(),
        });
        let s2 = agg.snapshot();
        // Denominator uses completed - first_token (excluding wait before first token), still 100 t/s.
        assert!((s2.avg_tps - 100.0).abs() < 1e-9);
    }

    fn mrow(
        model: &str,
        completed: i64,
        ft: Option<i64>,
        dur: Option<i64>,
        out: u64,
        reason: u64,
    ) -> ModelUsageRow {
        ModelUsageRow {
            model: model.into(),
            first_token_at: ft,
            completed_at: completed,
            duration_ms: dur,
            output_tokens: out,
            reasoning_tokens: reason,
        }
    }

    /// Two models x two buckets: tps/calls/tokens/avg/peak/share correct, buckets aligned, empty buckets are 0,
    /// series sorted by total_tokens descending, out-of-window (too early / future cross-slot) rows dropped.
    /// now is taken mid-slot (not boundary-aligned), same convention as the chart_stats test.
    #[test]
    fn model_stats_two_models_two_buckets() {
        let now = 1_700_000_005_000i64; // 5s into a 10s slot (15-min window has 10s bucket width)
        let rows = vec![
            // Model A: bucket 0 (gen 4s, eff 400 -> 100 t/s), bucket 1 (gen 1s, eff 100 -> 100 t/s)
            mrow("model-a", now - 5_000, Some(now - 9_000), Some(9_000), 300, 100),
            mrow("model-a", now - 15_000, Some(now - 16_000), Some(6_000), 100, 0),
            // Model B: bucket 0 (first_token missing, fall back to duration 2s, eff 400 -> 200 t/s),
            // bucket 2 (gen 3s, eff 1200 -> 400 t/s)
            mrow("model-b", now - 5_000, None, Some(2_000), 400, 0),
            mrow("model-b", now - 25_000, Some(now - 28_000), None, 900, 300),
            // Out-of-bounds: earlier than window (slot 100 >= 90) and future cross-slot (negative slot) should both be dropped.
            mrow("model-a", now - 1_000_000, Some(now - 1_001_000), None, 999, 0),
            mrow("model-a", now + 6_000, Some(now + 5_000), None, 999, 0),
        ];
        let p = aggregate_model_stats(rows, 15, now);
        assert_eq!(p.window_min, 15);
        assert_eq!(p.bucket_ms, 10_000);
        assert_eq!(p.now_ms, now);
        // Sorted by total_tokens descending: B(1600) first, A(500) second.
        assert_eq!(p.series.len(), 2);
        assert_eq!(p.series[0].model, "model-b");
        assert_eq!(p.series[1].model, "model-a");

        let b = &p.series[0];
        assert_eq!(b.buckets.len(), 90);
        assert_eq!(b.total_calls, 2);
        assert_eq!(b.total_tokens, 1600);
        assert!((b.avg_tps - 1600.0 / 5.0).abs() < 1e-9); // Σeff 1600 / 5s
        assert!((b.peak_tps - 400.0).abs() < 1e-9);
        assert!((b.share - 1600.0 / 2100.0).abs() < 1e-9);
        assert!((b.buckets[0].tps - 200.0).abs() < 1e-9);
        assert_eq!(b.buckets[0].calls, 1);
        assert_eq!(b.buckets[0].tokens, 400);
        assert_eq!(b.buckets[1].calls, 0); // empty bucket
        assert_eq!(b.buckets[1].tps, 0.0);
        assert_eq!(b.buckets[1].tokens, 0);
        assert!((b.buckets[2].tps - 400.0).abs() < 1e-9);
        assert_eq!(b.buckets[2].tokens, 1200);
        // Out-of-brows rows counted in no bucket.
        assert_eq!(b.buckets.iter().map(|x| x.calls).sum::<u64>(), 2);

        let a = &p.series[1];
        assert_eq!(a.total_calls, 2);
        assert_eq!(a.total_tokens, 500);
        assert!((a.avg_tps - 100.0).abs() < 1e-9); // 500 / 5s
        assert!((a.peak_tps - 100.0).abs() < 1e-9);
        assert!((a.share - 500.0 / 2100.0).abs() < 1e-9);
        assert!((a.buckets[0].tps - 100.0).abs() < 1e-9);
        assert!((a.buckets[1].tps - 100.0).abs() < 1e-9);
        assert_eq!(a.buckets[2].calls, 0);
    }

    /// Wall-clock alignment guard (same basis as chart_stats): when now slides within the same absolute slot,
    /// bucket contents stay identical — the trend should only translate over time, never deform
    /// (previously, bucketing by relative (now − completed) offset made bucket boundaries drift with query time,
    /// causing calls to bounce between adjacent buckets and the curve to slightly deform each poll).
    #[test]
    fn model_stats_wall_clock_aligned_buckets() {
        let mk = || {
            vec![
                mrow("m", 1_700_000_002_000, Some(1_699_999_990_000), Some(12_000), 600, 0),
                mrow("m", 1_699_999_990_000, Some(1_699_999_986_000), Some(4_000), 200, 0),
            ]
        };
        // The two now values differ by 5s but fall in the same absolute slot [1_700_000_000_000, 1_700_000_010_000).
        let a = aggregate_model_stats(mk(), 15, 1_700_000_003_000);
        let b = aggregate_model_stats(mk(), 15, 1_700_000_008_000);
        assert_eq!(a.bucket_ms, 10_000);
        assert_eq!(a.series[0].buckets, b.series[0].buckets);
        // Consistent with absolute-slot alignment: the two completed rows land in the newest bucket (0)
        // and the second-newest bucket (1) respectively.
        assert!(a.series[0].buckets[0].tps > 0.0);
        assert!(a.series[0].buckets[1].tps > 0.0);
        assert_eq!(a.series[0].buckets[2].calls, 0);
    }

    /// Window clamp (shared with the chart: clamp_chart_window: 999->1440, 0->15, 40->60, 400->360)
    /// and gen_ms missing fallback (first_token None -> duration_ms -> max(50) fallback).
    #[test]
    fn model_stats_window_clamp_and_gen_fallback() {
        assert_eq!(clamp_chart_window(999), 1440);
        assert_eq!(clamp_chart_window(0), 15);
        assert_eq!(clamp_chart_window(40), 60);
        assert_eq!(clamp_chart_window(400), 360);
        assert_eq!(clamp_chart_window(60), 60);
        assert_eq!(clamp_chart_window(360), 360);

        let now = 1_700_000_000_000i64;
        let rows = vec![
            mrow("m", now - 5_000, None, Some(5_000), 500, 0), // gen=5000ms
            mrow("m", now - 6_000, None, None, 100, 0),        // duration missing -> 50ms
            mrow("m", now - 7_000, Some(now - 7_000), Some(0), 100, 0), // ft non-positive (=completed) -> dur 0 non-positive -> 50ms
        ];
        // window_min=999 snaps to 1440 (24 hours): bucket width 960_000ms; all three rows land in the newest bucket.
        let p = aggregate_model_stats(rows, 999, now);
        assert_eq!(p.window_min, 1440);
        assert_eq!(p.bucket_ms, 960_000);
        assert_eq!(p.series.len(), 1);
        let s = &p.series[0];
        assert_eq!(s.buckets.len(), 90);
        assert_eq!(s.total_calls, 3);
        assert_eq!(s.total_tokens, 700);
        // Σeff 700 / (5s + 50ms + 50ms)
        assert!((s.avg_tps - 700.0 / 5.1).abs() < 1e-9);
        assert!((s.buckets[0].tps - 700.0 / 5.1).abs() < 1e-9);
        assert_eq!(s.peak_tps, s.buckets[0].tps);
        assert!((s.share - 1.0).abs() < 1e-9);
        // Empty input -> empty series
        let p = aggregate_model_stats(Vec::new(), 15, now);
        assert!(p.series.is_empty());
    }

    /// Historical stats: average does not filter (includes duration-fallback rows); peak-record admission --
    /// valid first_token + gen>=1s + eff>=300; all three required, any one missing excludes the call.
    #[test]
    fn history_stats_fold_and_admission() {
        let now = 1_700_000_000_000i64;
        let day = 1_700_000_000_000i64 - now.rem_euclid(86_400_000); // arbitrary local-day placeholder
        let mut h = HistoryStats::default();
        // Normal large call: 1000 tok / 4s = 250 t/s, admitted to peak.
        h.fold(day, Some(now - 5_000), now - 1_000, 4_000, 1_000);
        // Faster small call: 300 tok / 1.05s ~ 285.7 t/s, eff=300 qualifies -> should refresh peak.
        h.fold(day, Some(now - 3_000), now - 1_950, 1_050, 300);
        assert!((h.max_tps() - 300.0 * 1000.0 / 1050.0).abs() < 1e-9);
        // Fake-record trap: 79 tok / 24ms (real observed shape in the actual DB, 1580 t/s) -- eff and duration both too low.
        h.fold(day, Some(now - 100), now - 76, 24, 79);
        assert!((h.max_tps() - 300.0 * 1000.0 / 1050.0).abs() < 1e-9);
        // eff qualifies but duration too short (500 tok / 200ms = 2500 t/s) -> not admitted.
        h.fold(day, Some(now - 300), now - 100, 200, 500);
        assert!((h.max_tps() - 300.0 * 1000.0 / 1050.0).abs() < 1e-9);
        // duration-fallback row (ft missing): counted in average but not admitted to peak.
        h.fold(day, None, now - 60_000, 10_000, 2_000);
        assert!((h.max_tps() - 300.0 * 1000.0 / 1050.0).abs() < 1e-9);
        // Average = Σeff 3800 / Σgen (the 24ms row enters as MIN_DUR_MS=50)
        let total_eff = 1_000 + 300 + 79 + 500 + 2_000;
        let total_gen = 4_000 + 1_050 + 50 + 200 + 10_000;
        assert!((h.avg_tps() - total_eff as f64 / (total_gen as f64 / 1000.0)).abs() < 1e-9);
        assert_eq!(h.total_eff(), total_eff);
        assert_eq!(h.total_gen_ms(), total_gen);
        assert_eq!(h.days.len(), 1);
        // Empty DB
        assert_eq!(HistoryStats::default().avg_tps(), 0.0);
        assert_eq!(HistoryStats::default().max_tps(), 0.0);
    }

    /// Week window sliding eviction: discard the earliest whole-day bucket before the window start;
    /// peak and average are aggregated only from in-window day buckets (the day whose start equals the cutoff is retained).
    #[test]
    fn history_stats_week_window_prune() {
        let day = 86_400_000i64 * 20_000; // arbitrary placeholder day boundary aligned to local midnight
        let mut h = HistoryStats::default();
        // Three days each with one qualifying call: first day fastest at 400 t/s (should disappear after exiting window), next two days at 250 t/s
        h.fold(day, Some(day + 1_000), day + 3_500, 2_500, 1_000);
        h.fold(day + 86_400_000, Some(day + 86_400_001), day + 86_400_005, 4_000, 1_000);
        h.fold(day + 2 * 86_400_000, Some(day + 2 * 86_400_001), day + 2 * 86_400_005, 4_000, 1_000);
        assert_eq!(h.days.len(), 3);
        assert!((h.max_tps() - 400.0).abs() < 1e-9);
        // Window start = second day midnight: first day bucket exits window (400 t/s peak disappears with it),
        // the day at the window start is retained
        h.prune(day + 86_400_000);
        assert_eq!(h.days.len(), 2);
        assert!((h.max_tps() - 250.0).abs() < 1e-9);
        assert!((h.avg_tps() - 250.0).abs() < 1e-9);
        // Average recalculated from remaining buckets: Σeff 2000 / Σgen 8000ms
        assert_eq!(h.total_eff(), 2_000);
        assert_eq!(h.total_gen_ms(), 8_000);
        // Cutoff pushed past all three days: all exit window -> zeroed (not stale peak residue)
        h.prune(day + 3 * 86_400_000);
        assert!(h.days.is_empty());
        assert_eq!(h.max_tps(), 0.0);
        assert_eq!(h.avg_tps(), 0.0);
    }

    /// Chart aggregation: 90 buckets, old-to-new order, out-of-bounds dropped,
    /// gen fallback basis consistent with today's spark.
    /// now is taken mid-slot (not boundary-aligned) to keep "completed a few seconds ago" stable in the newest bucket.
    #[test]
    fn chart_stats_buckets_and_order() {
        let now = 1_700_000_005_000i64; // 5s into a 10s slot
        let rows = vec![
            // 15-min window has 10s bucket width: completed 3s ago -> same slot as now -> newest bucket (last).
            // gen = ft delta 12s, eff 1000 -> 83.3 t/s
            mrow("m", now - 3_000, Some(now - 15_000), Some(15_000), 1_000, 0),
            // Completed 95s ago -> 9 buckets from newest -> index 89-9=80; gen 4s eff 3000 -> 750 t/s
            mrow("m", now - 95_000, Some(now - 99_000), Some(9_000), 3_000, 0),
            // Out-of-window (20 minutes ago) -> dropped
            mrow("m", now - 1_200_000, Some(now - 1_204_000), None, 9_999, 0),
        ];
        let p = aggregate_chart_stats(rows, 15, now);
        assert_eq!(p.window_min, 15);
        assert_eq!(p.bucket_ms, 10_000);
        assert_eq!(p.buckets.len(), 90);
        assert!((p.buckets[89] - 1_000.0 * 1000.0 / 12_000.0).abs() < 1e-9); // newest bucket
        assert!((p.buckets[80] - 750.0).abs() < 1e-9);
        assert!((p.buckets[0] - 0.0).abs() < 1e-9); // empty bucket
    }

    /// Chart window clamp: 999->1440, 30->15, 90->60, 720->360; 1h window has 40s bucket width.
    #[test]
    fn chart_stats_window_clamp() {
        assert_eq!(clamp_chart_window(999), 1440);
        assert_eq!(clamp_chart_window(30), 15);
        assert_eq!(clamp_chart_window(90), 60);
        assert_eq!(clamp_chart_window(720), 360);
        assert_eq!(clamp_chart_window(15), 15);
        let p = aggregate_chart_stats(Vec::new(), 60, 1_700_000_000_000i64);
        assert_eq!(p.bucket_ms, 60 * 60_000 / 90);
        assert_eq!(p.buckets.len(), 90);
        assert!(p.buckets.iter().all(|v| *v == 0.0));
    }
}
