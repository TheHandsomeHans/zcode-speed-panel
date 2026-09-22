/// Real-time speed measurement: polls the ZCode CLI process's IO write byte count
/// (the streamed rendering data written to the desktop UI pipe), which grows continuously
/// at tens of KB/s during model streaming output—this is the genuine real-time signal.
///
/// Platform primitives live in [`platform`]: Windows uses Toolhelp enumeration +
/// GetProcessIoCounters to read cumulative write bytes; macOS uses libproc enumeration +
/// proc_pid_rusage's ri_diskio_byteswritten.
///
/// - Process discovery: Windows filters `zcode.exe` with command line containing `zcode.cjs`;
///   macOS does exact-match on KERN_PROCARGS2 command line arguments for `zcode-cli`
///   (both support multiple concurrent processes)
/// - Disk-write deduction (Windows only): subtracts rollout/log/WAL growth from byte deltas
///   (the source of spikes at completion/flush time), applied **once** on the aggregated
///   stream (deducting per-process during multi-process summation would subtract the global
///   file increment N times). Deduction may produce negative individual ticks (when flush
///   and write are misaligned, convergence happens via interval totals); clamping to
///   non-negative only happens at window summary time—if clamped per tick, misaligned
///   disk-write increments would be permanently swallowed (measured readings collapsing
///   to 1/5 of the true value was caused by this). macOS does not perform this deduction
///   (rollout directory net change can be negative, which would contaminate the cleaned
///   stream; see platform::mac's tracked_files_total)
/// - Noise floor: BASE_NOISE + per-process adaptive heartbeat floor (capped, to prevent
///   the quantile from being "poisoned" by streaming increments during sustained output,
///   which would cause it to subtract its own output as noise); parameters differ between
///   platforms (mac idle measures strictly 0 bytes, static noise floor is 0)
/// - Burst rejection: single-tick raw deltas exceeding the threshold are dropped entirely,
///   not entering integration (Windows only: request body upload ~190KB/tick vs true
///   streaming ~52KB/tick are distinguishable; mac streaming itself is inherently a
///   single-tick burst pattern, threshold disabled in CleanParams)
/// - Byte-to-token conversion [consistency calibration]: after a call completes, uses the
///   identical cleaned stream as the display path, integrating bytes over [first_token,
///   completed] divided by true output_tokens for sliding self-calibration. The calibration
///   numerator and display numerator share the same source; any systematic deductions
///   (noise floor/disk-write/misalignment) are canceled out by the coefficient, and the
///   display value converges to the true t/s. Historical lesson: calibrating with the
///   uncleaned total byte stream while displaying with the cleaned stream caused a
///   2-3x inflated coefficient and systematically low readings due to the two paths
///   being inconsistent. Sample admission also carries a cross-process guard: samples
///   are rejected when other processes' byte share within the window is too high
///   (concurrent streaming in another window), preventing the attributing process's
///   integral from being contaminated by foreign bytes.
///   On mac, disk write bytes are page-cache asynchronous writeback counts (lagging
///   write() by seconds to tens of seconds), so the calibration window is extended to
///   completed + cal_grace_ms (delayed disk-write grace period; Windows=0 processes
///   per-tick), and outlier samples deviating more than the ratio from the active
///   coefficient are rejected (Windows disabled).

use crate::metrics::Call;
use std::collections::{HashMap, HashSet, VecDeque};
use std::time::{Duration, Instant};

/// Current speed statistics window (display span)
const WINDOW_MS: i64 = 30_000;
/// Process/file sampling ring capacity (~3min @700ms, covers calibration lookback interval)
const RING_CAP: usize = 260;
/// Process list refresh interval
const REFRESH_EVERY: Duration = Duration::from_secs(30);
/// Streaming detection threshold (cleaned rate)
const STREAMING_BPS: f64 = 4_000.0;
/// Start/stop is call-gated; this short window is only used to locate the span anchor (first-byte tick)
const DETECT_MS: i64 = 2_500;

/// Stop-detection fallback grace: gate is open (message row's completed has not been flushed to disk,
/// CLI side can delay by seconds to minutes) but after the streaming anchor appears, the cleaned rate
/// stays below the streaming threshold for this duration → determines generation has actually stopped;
/// reading goes to zero, no longer shows "generating/estimating". Calls on pipes with no anchor
/// established are unaffected (no bytes throughout is their normal state, served by window fallback);
/// byte pauses in normal streaming are far shorter than this value; on misdetection, byte recovery
/// self-heals on the same tick
const SILENT_STOP_MS: i64 = 15_000;
/// Startup hint window: max duration after gate opens but before any streaming byte is observed (TTFT).
/// Within the window, shows a "collecting stats…" hint (not a misleading estimated value);
/// if the window elapses with no bytes, treats it as a silent-pipe call and falls back to
/// a recent true-value estimate (≈)
const TTFT_HINT_MS: i64 = 20_000;
/// Rough upper bound on idle heartbeat noise floor (B/s), stacked with per-process adaptive
/// floor (capped) to filter heartbeat
const BASE_NOISE_BPS: f64 = 3_000.0;
/// Per-process adaptive heartbeat floor cap per tick (bytes/tick). Measured streaming can reach
/// ~75KB/s (52KB/tick); if the quantile floor is not capped, sustained streaming would elevate it,
/// causing it to subtract its own output as noise
const FLOOR_CAP_BYTES: f64 = 2_000.0;
/// Single-tick raw delta rejection threshold: request body upload measures ~190KB/tick (split >90KB),
/// true streaming max ~52KB/tick; midpoint drops the entire tick (neither bytes nor duration enter integration)
const BURST_TICK_BYTES: f64 = 100_000.0;
/// Initial byte/token coefficient (cleaned stream measures ~350-900; conservative value taken,
/// converges quickly after calibration)
const DEFAULT_BPT: f64 = 600.0;
/// Reasonable range for calibration samples (prevents outlier samples from polluting the median).
/// Under consistency calibration the coefficient absorbs systematic deductions (disk mirror /
/// noise floor / burst tick rejection); the range must be wide enough to avoid breaking convergence;
/// garbage samples are mainly blocked by CAL_MIN_TOKENS' call-size gate
const CAL_MIN: f64 = 100.0;
const CAL_MAX: f64 = 6_000.0;
/// Minimum call size for calibration samples: small calls have a large share of UI fixed-frame overhead;
/// forbidden from entering samples
const CAL_MIN_TOKENS: u64 = 300;
/// Per-round average speed drift auto-recalibration: one round = a continuous segment of the gate's
/// "in progress" signal; the display speed within a round (IO measured ticks) takes the arithmetic mean;
/// if the difference between the previous round's mean and the mean of the preceding DRIFT_ROUNDS
/// consecutive rounds is >= DRIFT_RATIO (either direction), a magnitude jump is detected
/// (model/tokenizer changed, old coefficient likely stale)
const DRIFT_ROUNDS: usize = 5;
const DRIFT_RATIO: f64 = 3.0;
/// Session→process attribution switch hysteresis: an attributed session only switches when the
/// candidate process's raw bytes within the call window are >= this multiple of the current
/// attribution process's. When two CLI windows stream concurrently, the top-writer flips per call
/// (2026-09-18 field: two consecutive calls in the same session attributed to two different pids,
/// calibration samples contaminated by the other window's bytes, coefficient swinging 262-764,
/// reading deviation 2-3x)
const ATTR_SWITCH_RATIO: f64 = 2.0;
/// Concurrent attribution dedup threshold: when the top process is already occupied by another
/// in-progress session, the second-highest process must reach this fraction of the top's window
/// bytes to reattribute to it. When two concurrent streams have similar rates (ratio ~1), idle
/// process noise leakage is ~0.1; 0.5 is the middle ground—corrects tie misattribution without
/// pushing a shared-process session to an idle process (2026-09-18 field: new task in same ZCode
/// window reuses the same app-server process, both sessions' bytes go through the same pid,
/// second-highest only has ~0.17 ratio of noise)
const ATTR_DEDUP_RATIO: f64 = 0.5;
/// Calibration sample cross-process guard: raw bytes of **other** processes within the call window
/// must not exceed this fraction of the attributing process's to admit the sample—when the other
/// window streams concurrently, the attributing process's cleaned integral window will inevitably
/// mix in foreign bytes, distorting the sample
const CROSS_PID_RATIO: f64 = 0.2;

/// Cleaning/calibration parameters (platform-parameterized). Windows column = long-term
/// measured tuned values (original constants above, do not change); macOS column corrected
/// based on 120s probe + 2026-09-17 true-value reconciliation (6 cal events, pred_tps
/// exactly matches true_tps when attribution is correct): during streaming
/// ri_diskio_byteswritten ≈195KB/s, idle strictly 0 bytes, single-tick delta burst pattern
/// 0,0,0,+225KB~1.5MB, B/token true value ≈650 (probe-period ≈3900 was a misjudgment),
/// disk count is page-cache asynchronous writeback (lagging write() by seconds to tens of seconds).
#[derive(Clone, Copy)]
pub struct CleanParams {
    /// Single-tick raw delta rejection threshold (Windows: midpoint between request body upload
    /// ~190KB/tick and true streaming ~52KB/tick, drops entire tick). mac: streaming itself is
    /// single-tick burst pattern (225KB~1.5MB is normal signal), set to u64::MAX to disable—
    /// 100KB threshold would drop all signal
    pub burst_tick_bytes: f64,
    /// Idle heartbeat noise floor rough upper bound (B/s). mac idle measures strictly 0 bytes,
    /// no static noise floor needed
    pub base_noise_bps: f64,
    /// Per-process adaptive heartbeat floor cap per tick (bytes/tick). Same on both platforms:
    /// cap only prevents quantile poisoning; when mac idle is always 0, adaptive self-reduces to 0
    pub floor_cap_bytes: f64,
    /// Calibration sample B/token reasonable range lower/upper bound. mac measured ≈3900,
    /// upper limit widened for margin
    pub cal_min: f64,
    pub cal_max: f64,
    /// Minimum call size for calibration samples (platform-independent)
    pub cal_min_tokens: u64,
    /// Initial byte/token coefficient prior. Windows long-term 600; mac true-value reconciliation
    /// (2026-09-17, 6 cal events) measured accepted samples 614/724, take 700—old value 2000
    /// came from 120s probe's ≈3900 misjudgment, cold-start reading 3x underestimated
    pub default_bpt: f64,
    /// Span anchor detection short window (first-byte tick). First version same on both platforms;
    /// mac may be tuned if state jitter occurs
    pub detect_ms: i64,
    /// Calibration delayed disk-write grace (ms): after call completion, pending waits this long
    /// before integrating; calibration integral and raw stats window upper limit are both extended
    /// to completed + grace. mac's ri_diskio_byteswritten is page-cache asynchronous writeback count,
    /// lagging write() by seconds to tens of seconds (measured 117s long call: 96% of bytes landed
    /// after completed, user stared at 0.7 t/s for two minutes while true value was 65.3);
    /// Windows' WriteTransferCount is synchronous count, takes 0 = per-tick processing
    pub cal_grace_ms: i64,
    /// Calibration sample outlier rejection ratio: sample B/token deviating from the current active
    /// coefficient by more than this ratio is rejected (half-complete samples from delayed disk write
    /// / misattributed samples do not enter median). 0 = disabled (Windows)
    pub cal_outlier_ratio: f64,
}

impl CleanParams {
    /// Windows long-term measured values (original constant values, do not change)
    #[allow(dead_code)] // mac build only referenced by tests; unused in bin build
    pub fn windows() -> Self {
        Self {
            burst_tick_bytes: BURST_TICK_BYTES,
            base_noise_bps: BASE_NOISE_BPS,
            floor_cap_bytes: FLOOR_CAP_BYTES,
            cal_min: CAL_MIN,
            cal_max: CAL_MAX,
            cal_min_tokens: CAL_MIN_TOKENS,
            default_bpt: DEFAULT_BPT,
            detect_ms: DETECT_MS,
            // Delayed disk-write grace and outlier rejection disabled on Windows:
            // WriteTransferCount is synchronous count, per-tick processing, no outlier filtering,
            // behavior is byte-for-byte equivalent to historical versions
            cal_grace_ms: 0,
            cal_outlier_ratio: 0.0,
        }
    }

    /// macOS measured values: burst IS the signal so rejection must be disabled, no static noise floor,
    /// coefficient prior takes 700 per true-value reconciliation, delayed disk-write grace 15s
    /// (page-cache asynchronous writeback lag), sample outlier 3x rejection
    #[cfg(target_os = "macos")]
    pub fn macos() -> Self {
        Self {
            burst_tick_bytes: u64::MAX as f64,
            base_noise_bps: 0.0,
            floor_cap_bytes: FLOOR_CAP_BYTES,
            cal_min: CAL_MIN,
            cal_max: 12_000.0,
            cal_min_tokens: CAL_MIN_TOKENS,
            default_bpt: 700.0,
            detect_ms: DETECT_MS,
            cal_grace_ms: 15_000,
            cal_outlier_ratio: 3.0,
        }
    }

    pub fn platform() -> Self {
        #[cfg(target_os = "macos")]
        {
            Self::macos()
        }
        #[cfg(not(target_os = "macos"))]
        {
            Self::windows()
        }
    }
}

#[derive(Default, Clone)]
pub struct LiveNow {
    /// Whether CLI processes were successfully discovered (false → frontend falls back to window/estimate display)
    pub available: bool,
    pub streaming: bool,
    /// Streaming has started but the 30s sliding window is not yet full (reading comes from the active interval;
    /// frontend shows "collecting stats")
    pub ramping: bool,
    /// Call has started but first byte not yet observed (TTFT, bounded within the hint window):
    /// frontend shows "collecting stats…" hint instead of an estimated value
    pub awaiting: bool,
    pub tps: f64,
    /// Cleaned pipe byte rate (B/s), for debug logging/reconciliation
    pub pipe_bps: f64,
    /// Number of processes in this tick's aggregated window whose contribution reached streaming magnitude
    /// (1 = single task; >1 = multi-task aggregate; idle process noise leakage not counted). Round drift
    /// detection only samples single-process rounds—natural throughput differences from task-count changes
    /// are not coefficient drift
    pub n_pids: usize,
    /// Per-task breakdown: real-time speed of each process in the display set (sum of breakdown = aggregate reading)
    pub tasks: Vec<TaskLive>,
    /// Diagnostic: detection-window cleaned rate (KB/s) of each tracked process. For multi-task
    /// troubleshooting/reconciliation (2026-09-18 troubleshooting: tick only had npids single field,
    /// could not answer "which process is writing")
    pub proc_bps: Vec<(u32, f64)>,
}

/// Per-task real-time breakdown (element of `LiveNow::tasks`): one CLI process = one task row.
/// Multiple parallel sub-agents within the same process are not separable at the byte level;
/// displayed honestly as that process's total
#[derive(Clone, Debug, Default)]
pub struct TaskLive {
    pub pid: u32,
    /// Attributed in-progress session (streaming process with no attribution record yet is None)
    pub session: Option<String>,
    /// Number of in-progress sessions carried by this process (>=2 = multi-task on same process;
    /// speed is the total, label see n_sessions)
    pub n_sessions: usize,
    pub tps: f64,
    pub streaming: bool,
}

/// Calibration and reconciliation event after a call completes (for debug logging)
#[derive(Clone, Debug, serde::Serialize)]
pub struct CalEvent {
    pub id: String,
    pub session: String,
    pub completed_ms: i64,
    /// Call true value (disk output+reasoning ÷ generation duration)
    pub true_tps: f64,
    pub gen_ms: i64,
    pub eff: u64,
    /// Raw write bytes over streaming interval (uncleaned, all processes summed; attribution and reconciliation baseline)
    pub raw_bytes: f64,
    /// Cleaned stream integral bytes over [first_token, completed] using the identical spec as display (this call's coefficient numerator)
    pub clean_bytes: f64,
    /// This call's cleaned-stream sample B/token (0 = not entered calibration)
    pub bpt_sample: f64,
    /// Coefficient in effect after this event
    pub bpt_now: f64,
    /// Not entered calibration (call too short / no valid bytes / outlier rejected)
    pub cal_skipped: bool,
    /// Process actually used for clean integral (attribution process integral branch; all-process sum branch is None)
    pub attr_pid: Option<u32>,
    /// Process with the largest raw bytes in raw_by_pid (misattribution diagnosis: attr and top
    /// inconsistent and clean much smaller than raw means wrong process attributed)
    pub top_pid: Option<u32>,
    /// Raw bytes of other processes within the call window (cross-process guard diagnosis: sample
    /// rejected when share exceeds CROSS_PID_RATIO of attributing process)
    pub others_bytes: f64,
}

// ============ Pure computation section (cross-platform, unit-testable): tick cleaning / interval integration / median ============

/// Cleaned single-tick pipe bytes. bytes can be negative: when disk flush and IO count are misaligned,
/// convergence happens via interval integration sum (Σ bytes = Σ raw delta − Σ disk-write − Σ noise floor);
/// cannot clamp per tick to 0
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct TickRow {
    /// Tick duration
    pub dt_ms: i64,
    /// Cleaned bytes (raw write − disk-write growth − noise floor, may be negative)
    pub bytes: f64,
    /// Tick end time (wall-clock ms)
    pub end_ms: i64,
}

/// Build cleaned tick sequence from cumulative write byte sequence (only deducts per-process noise floor;
/// tracked file growth is deducted uniformly on the aggregated stream by [`merge_streams`]—deducting
/// per-process during multi-process summation would subtract the global file increment N times).
/// bytes can be negative (disk-write misalignment canceled by interval integration)
pub(crate) fn build_rows(samples: &[(i64, u64)], min_delta: f64, p: &CleanParams) -> Vec<TickRow> {
    let floor_static = min_delta.min(p.floor_cap_bytes);
    let mut rows = Vec::with_capacity(samples.len());
    for w in samples.windows(2) {
        let (t0, w0) = w[0];
        let (t1, w1) = w[1];
        let dt_ms = t1 - t0;
        if dt_ms <= 0 {
            continue;
        }
        let raw = w1.saturating_sub(w0) as f64;
        // Request-body upload etc. single-tick burst: drop entire tick (neither integrate bytes nor count duration);
        // on mac burst IS the signal, threshold disabled in CleanParams
        if raw > p.burst_tick_bytes {
            continue;
        }
        let dt_s = dt_ms as f64 / 1000.0;
        rows.push(TickRow {
            dt_ms,
            bytes: raw - floor_static - p.base_noise_bps * dt_s,
            end_ms: t1,
        });
    }
    rows
}

/// Merge multiple process cleaned streams into one aggregated stream: sum bytes per tick, wall-clock
/// duration counted once (integrating per-process separately would also sum durations, diluting the rate
/// into a cross-process average); tracked file growth deducted once overall. Rows aligned by end_ms
/// (paired samples from same polling loop, timestamps consistent; intervals where a process is missing
/// naturally contribute 0 bytes). Single-stream input is arithmetically identical to historical
/// per-row file-deduction
pub(crate) fn merge_streams(streams: &[&[TickRow]], files: &[(i64, u64)]) -> Vec<TickRow> {
    use std::collections::BTreeMap;
    let mut merged: BTreeMap<i64, (i64, f64)> = BTreeMap::new();
    for rows in streams {
        for r in rows.iter() {
            let e = merged.entry(r.end_ms).or_insert((r.dt_ms, 0.0));
            // Same-tick dt must be consistent (paired samples from same polling loop)—if sampling times
            // ever diverge, merging by end_ms would silently split into two rows causing double-counted
            // duration; let it blow up here early
            debug_assert_eq!(e.0, r.dt_ms);
            e.1 += r.bytes;
        }
    }
    let mut out = Vec::with_capacity(merged.len());
    for (end_ms, (dt_ms, bytes)) in merged {
        // Tracked file growth within [t0, end) interval (half-open boundary, avoids double-counting adjacent intervals)
        let t0 = end_ms - dt_ms;
        let mut fg = 0f64;
        for f in files.windows(2) {
            let (ft0, fv0) = f[0];
            let (ft1, fv1) = f[1];
            if ft1 > t0 && ft0 < end_ms {
                fg += fv1.saturating_sub(fv0) as f64;
            }
        }
        out.push(TickRow {
            dt_ms,
            bytes: bytes - fg,
            end_ms,
        });
    }
    out
}

/// Tracked file total sequence → growth tick sequence (for per-task breakdown to apportion deduction by window)
pub(crate) fn file_growth_rows(files: &[(i64, u64)]) -> Vec<TickRow> {
    files
        .windows(2)
        .filter_map(|w| {
            let (t0, v0) = w[0];
            let (t1, v1) = w[1];
            let dt_ms = t1 - t0;
            if dt_ms <= 0 {
                return None;
            }
            Some(TickRow {
                dt_ms,
                bytes: v1.saturating_sub(v0) as f64,
                end_ms: t1,
            })
        })
        .collect()
}

/// Integrate [from_ms, to_ms] interval proportionally by time: boundary-crossing ticks are apportioned
/// by overlap duration. Returns (bytes, seconds). Rejected burst ticks contribute no duration, not diluting rate
pub(crate) fn integrate(rows: &[TickRow], from_ms: i64, to_ms: i64) -> (f64, f64) {
    let (mut bytes, mut secs) = (0f64, 0f64);
    for r in rows {
        let lo = (r.end_ms - r.dt_ms).max(from_ms);
        let hi = r.end_ms.min(to_ms);
        if hi <= lo {
            continue;
        }
        let ov = (hi - lo) as f64;
        let dt = r.dt_ms as f64;
        bytes += r.bytes * (ov / dt);
        secs += ov / 1000.0;
    }
    (bytes, secs)
}

/// Calibration coefficient maintenance: sliding-window sample median (pure function for easy testing)
pub(crate) fn median_bpt(samples: &mut VecDeque<f64>, sample: f64, cap: usize) -> f64 {
    samples.push_back(sample);
    while samples.len() > cap {
        samples.pop_front();
    }
    let mut sorted: Vec<f64> = samples.iter().copied().collect();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap());
    sorted[sorted.len() / 2]
}

/// Real-time display process set selection (pure function for easy testing):
/// - All in-progress sessions have live attributions → union of attributed pids: aggregates to true
///   total throughput during multi-task concurrency, also produces per-task breakdown;
/// - Any in-progress session has no attribution (new session / first call of a sub-agent not yet completed)
///   → None = all-process sum fallback: that session's own process has no attribution record yet,
///   computing only the union would miss it and show another window's speed; idle processes contribute
///   ≈0 after noise cleaning, negligible cost;
/// - No in-progress sessions → None (gate closed, reading zero, branch choice irrelevant)
pub(crate) fn pick_pid_set(
    inflight: &[(String, i64)],
    session_pid: &HashMap<String, u32>,
    active_pids: &HashSet<u32>,
) -> Option<Vec<u32>> {
    if inflight.is_empty() {
        return None;
    }
    let mut set = Vec::with_capacity(inflight.len());
    for (s, _) in inflight {
        match session_pid.get(s) {
            Some(pid) if active_pids.contains(pid) => set.push(*pid),
            // No attribution or attributed process dead → sum fallback (session_pid is pruned per tick
            // to alive pids; "dead" here can only be an intra-tick race, fallback equally safe)
            _ => return None,
        }
    }
    set.sort_unstable();
    set.dedup();
    Some(set)
}

/// Session→process attribution switch decision (pure function for easy testing): returns the attribution
/// pid to write this tick. Existing attribution carries hysteresis—only switches when the candidate top
/// process's window raw bytes >= ATTR_SWITCH_RATIO times the current attribution, preventing per-call
/// flipping between concurrent windows; with no current attribution or zero bytes in current attribution's
/// window (self-heal path for stale attribution), directly trusts top
pub(crate) fn should_reattribute(
    cur: Option<u32>,
    top: Option<u32>,
    raw_by_pid: &HashMap<u32, u64>,
) -> Option<u32> {
    let top = top?;
    match cur {
        Some(c) if c != top => {
            let cur_raw = *raw_by_pid.get(&c).unwrap_or(&0) as f64;
            let top_raw = *raw_by_pid.get(&top).unwrap_or(&0) as f64;
            if top_raw >= cur_raw * ATTR_SWITCH_RATIO && top_raw > cur_raw {
                Some(top)
            } else {
                Some(c)
            }
        }
        _ => Some(top),
    }
}

/// Session→process attribution decision (pure function for easy testing): returns the attribution pid
/// to write this tick. `owned` = set of pids already attributed by other **in-progress** sessions.
/// Three branches:
/// - First attribution: top is occupied and second-highest bytes reach ATTR_DEDUP_RATIO of top →
///   reattribute to second-highest (under concurrent ties, max_by picks top randomly, both sessions
///   crowd onto the same process, display set collapses to single process); if second-highest only
///   has noise ratio, keep top (true sharing)
/// - Current attribution occupied by another in-progress session: top is unoccupied and bytes reach
///   half of current attribution to switch—in occupied scenarios 2x hysteresis would lock in historical
///   misattribution
/// - Current attribution not occupied: keep 2x hysteresis original semantics (should_reattribute)
pub(crate) fn pick_attribution(
    cur: Option<u32>,
    raw_by_pid: &HashMap<u32, u64>,
    owned: &HashSet<u32>,
) -> Option<u32> {
    // Candidates sorted by window bytes descending, ties by pid ascending—iteration order deterministic,
    // no longer relies on HashMap order
    let mut cands: Vec<(u32, u64)> = raw_by_pid.iter().map(|(p, b)| (*p, *b)).collect();
    cands.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    let top = cands.first().copied()?;
    match cur {
        None => {
            if owned.contains(&top.0) {
                if let Some((pid, bytes)) = cands.get(1) {
                    if !owned.contains(pid)
                        && *bytes as f64 >= (top.1 as f64) * ATTR_DEDUP_RATIO
                        && *bytes >= 20_000
                    {
                        return Some(*pid);
                    }
                }
            }
            Some(top.0)
        }
        Some(c) if c != top.0 && owned.contains(&c) => {
            // Current attribution occupied: top unoccupied and bytes reach half of current attribution to switch
            // (in occupied scenario 2x hysteresis would lock in historical misattribution; if top also occupied, nowhere to go)
            if !owned.contains(&top.0)
                && top.1 as f64 >= (*raw_by_pid.get(&c).unwrap_or(&0) as f64) * ATTR_DEDUP_RATIO
            {
                Some(top.0)
            } else {
                Some(c)
            }
        }
        Some(c) => should_reattribute(Some(c), Some(top.0), raw_by_pid),
    }
}

/// Calibration sample cross-process guard (pure function for easy testing): only admits the sample
/// when other processes' raw bytes within the call window do not exceed CROSS_PID_RATIO of the
/// attributing process's. When the other window streams concurrently, the attributing process's
/// cleaned integral window will inevitably mix in foreign bytes, distorting the B/token sample
pub(crate) fn cross_pid_ok(others_raw: f64, attr_raw: f64) -> bool {
    attr_raw > 0.0 && others_raw <= attr_raw * CROSS_PID_RATIO
}

/// Calibration sample admission: pipe integral must have substantial contribution (>= 20% of raw bytes),
/// and B/token falls within reasonable range without clamping. Silent-pipe calls (bytes all land in disk
/// at completion, measured samples can be as low as ~7 B/token) and abnormal-ratio samples are rejected
/// entirely, preventing median coefficient pollution.
/// Additionally, when the sample deviates from the current active coefficient bpt_now by more than
/// cal_outlier_ratio times, it is rejected (mac: half-complete samples from delayed disk write /
/// half samples from misattributed process do not enter median; Windows ratio=0 explicitly disabled,
/// behavior consistent with historical versions).
/// Returns (whether admitted, sample value)
pub(crate) fn cal_sample(
    eff: u64,
    clean_bytes: f64,
    raw_bytes: f64,
    bpt_now: f64,
    p: &CleanParams,
) -> (bool, f64) {
    if eff < p.cal_min_tokens || clean_bytes <= 0.0 || raw_bytes <= 0.0 {
        return (false, 0.0);
    }
    let ratio = clean_bytes / eff as f64;
    let usable = clean_bytes / raw_bytes >= 0.2;
    let in_range = usable && ratio >= p.cal_min && ratio <= p.cal_max;
    // Outlier rejection: outlier_ratio=0 (Windows) explicitly disabled, avoids 0-as-divisor/0-multiply misjudgment;
    // rejected sample value recorded as 0 (consistent with call-side cal_skipped → bpt_sample=0 spec)
    if in_range
        && p.cal_outlier_ratio > 0.0
        && bpt_now > 0.0
        && (ratio < bpt_now / p.cal_outlier_ratio || ratio > bpt_now * p.cal_outlier_ratio)
    {
        return (false, 0.0);
    }
    (in_range, ratio)
}

/// Calibration sample queue capacity (sliding window, includes preset prior placeholder)
const CAL_QUEUE_CAP: usize = 5;

/// Startup hint decision (pure function): gate open, no streaming anchor yet (first byte not arrived),
/// and within the hint window since call start. Outside the window, no anchor = silent-pipe call,
/// upper layer falls back to estimate display
pub(crate) fn awaiting_hint(
    inflight_started: Option<i64>,
    anchor: Option<i64>,
    now_ms: i64,
) -> bool {
    match (inflight_started, anchor) {
        (Some(started), None) => now_ms - started <= TTFT_HINT_MS,
        _ => false,
    }
}

/// Stop-detection fallback (pure function): gate open and streaming anchor established, but the last
/// time the streaming threshold was reached exceeds the grace → generation has actually stopped
/// (no longer shows "generating" while waiting for completed flush)
pub(crate) fn stale_stop(anchor: Option<i64>, last_stream_ms: Option<i64>, now_ms: i64) -> bool {
    anchor.is_some()
        && last_stream_ms.map_or(false, |t| now_ms - t > SILENT_STOP_MS)
}

/// Per-round average speed drift detection (pure function for easy testing): feeds display-speed means
/// round by round, compares with the mean of the preceding DRIFT_ROUNDS consecutive rounds; bidirectional
/// difference >= DRIFT_RATIO triggers a speed magnitude jump, should trigger recalibration
/// (`LiveIo::reset_calibration`). After trigger, history clears; new magnitude re-accumulates baseline,
/// avoiding repeated triggers from the same jump
#[derive(Default)]
pub struct RoundDrift {
    history: VecDeque<f64>,
}

impl RoundDrift {
    pub fn new() -> Self {
        Self {
            history: VecDeque::with_capacity(DRIFT_ROUNDS),
        }
    }

    /// Observe one round's display mean. Returns Some(baseline mean) = recalibration triggered
    /// (baseline for logging); rounds with mean <= 0 (silent pipe, no measured ticks) do not
    /// participate and do not enter history
    pub fn observe(&mut self, round_avg: f64) -> Option<f64> {
        if round_avg <= 0.0 {
            return None;
        }
        let base = (self.history.len() == DRIFT_ROUNDS)
            .then(|| self.history.iter().sum::<f64>() / DRIFT_ROUNDS as f64);
        if let Some(b) = base {
            if round_avg / b >= DRIFT_RATIO || b / round_avg >= DRIFT_RATIO {
                self.history.clear();
                return Some(b);
            }
        }
        self.history.push_back(round_avg);
        while self.history.len() > DRIFT_ROUNDS {
            self.history.pop_front();
        }
        None
    }

    /// Clear history (synchronized reset after manual recalibration; new baseline accumulates from zero)
    pub fn reset(&mut self) {
        self.history.clear();
    }
}

struct ProcRing {
    handle: platform::ProcHandle,
    samples: VecDeque<(i64, u64)>,
    /// Minimum per-tick delta for this process (adaptive heartbeat noise floor, capped when used)
    min_delta: f64,
}

/// Platform process primitive: process discovery / open handle / read cumulative write bytes / tracked file total.
/// Three implementations selected by cfg, external path unified as `liveio::platform::*` (examples reuse).
/// All FFI failure paths return None/empty Vec, panic forbidden
pub mod platform {
    /// Windows: Toolhelp enumeration + read command line to filter CLI child processes; GetProcessIoCounters
    /// reads cumulative write bytes since process start (WriteTransferCount, kernel-maintained, authoritative)
    #[cfg(windows)]
    mod win {
        use std::ffi::c_void;

        #[link(name = "kernel32")]
        extern "system" {
            fn OpenProcess(access: u32, inherit: i32, pid: u32) -> *mut c_void;
            fn CloseHandle(h: *mut c_void) -> i32;
            fn GetProcessIoCounters(h: *mut c_void, counters: *mut IoCounters) -> i32;
            fn ReadProcessMemory(
                h: *mut c_void,
                addr: *const c_void,
                buf: *mut c_void,
                size: usize,
                read: *mut usize,
            ) -> i32;
            fn CreateToolhelp32Snapshot(flags: u32, pid: u32) -> isize;
            fn Process32FirstW(snap: isize, entry: *mut ProcessEntry32W) -> i32;
            fn Process32NextW(snap: isize, entry: *mut ProcessEntry32W) -> i32;
        }
        #[repr(C)]
        struct IoCounters {
            read_ops: u64,
            write_ops: u64,
            other_ops: u64,
            read_bytes: u64,
            write_bytes: u64,
            other_bytes: u64,
        }

        #[repr(C)]
        struct ProcessEntry32W {
            size: u32,
            usage: u32,
            process_id: u32,
            default_heap_id: usize,
            module_id: u32,
            threads: u32,
            parent_process_id: u32,
            pri_class_base: i32,
            flags: u32,
            exe_file: [u16; 260],
        }

        const PROCESS_QUERY_LIMITED: u32 = 0x1410; // QUERY_INFORMATION | QUERY_LIMITED | VM_READ
        const TH32CS_SNAPPROCESS: u32 = 2;

        fn process_command_line(pid: u32) -> Option<String> {
            unsafe {
                let h = OpenProcess(PROCESS_QUERY_LIMITED, 0, pid);
                if h.is_null() {
                    return None;
                }
                // Classic approach: ProcessBasicInformation → PEB → ProcessParameters → CommandLine
                let rd = |addr: usize, buf: &mut [u8]| -> bool {
                    let mut n = 0usize;
                    ReadProcessMemory(h, addr as *const c_void, buf.as_mut_ptr().cast(), buf.len(), &mut n)
                        != 0
                };
                let mut pbi = [0u8; 48];
                let mut ret: u32 = 0;
                if NtQueryInformationProcess(h, 0, pbi.as_mut_ptr().cast(), 48, &mut ret) != 0 {
                    CloseHandle(h);
                    return None;
                }
                #[cfg(target_pointer_width = "64")]
                {
                    let peb = usize::from_ne_bytes(pbi[8..16].try_into().ok()?);
                    if peb == 0 {
                        CloseHandle(h);
                        return None;
                    }
                    let mut pp_ptr = [0u8; 8];
                    if !rd(peb + 0x20, &mut pp_ptr) {
                        CloseHandle(h);
                        return None;
                    }
                    let pp = usize::from_ne_bytes(pp_ptr.try_into().ok()?);
                    if pp == 0 {
                        CloseHandle(h);
                        return None;
                    }
                    // RTL_USER_PROCESS_PARAMETERS.CommandLine (UNICODE_STRING) @ 0x70
                    let mut us = [0u8; 16];
                    if !rd(pp + 0x70, &mut us) {
                        CloseHandle(h);
                        return None;
                    }
                    let len = u16::from_ne_bytes([us[0], us[1]]) as usize;
                    let buf_ptr = usize::from_ne_bytes(us[8..16].try_into().ok()?);
                    if len == 0 || buf_ptr == 0 {
                        CloseHandle(h);
                        return None;
                    }
                    let mut wbuf = vec![0u8; len];
                    if !rd(buf_ptr, &mut wbuf) {
                        CloseHandle(h);
                        return None;
                    }
                    let u16s: Vec<u16> = wbuf
                        .chunks_exact(2)
                        .map(|c| u16::from_ne_bytes([c[0], c[1]]))
                        .collect();
                    CloseHandle(h);
                    return Some(String::from_utf16_lossy(&u16s));
                }
                #[cfg(not(target_pointer_width = "64"))]
                {
                    CloseHandle(h);
                    None
                }
            }
        }

        pub fn discover_cli_pids() -> Vec<u32> {
            let mut pids = Vec::new();
            unsafe {
                let snap = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
                if snap == -1 {
                    return pids;
                }
                let mut entry = ProcessEntry32W {
                    size: std::mem::size_of::<ProcessEntry32W>() as u32,
                    usage: 0,
                    process_id: 0,
                    default_heap_id: 0,
                    module_id: 0,
                    threads: 0,
                    parent_process_id: 0,
                    pri_class_base: 0,
                    flags: 0,
                    exe_file: [0; 260],
                };
                let ok = Process32FirstW(snap, &mut entry);
                if ok != 0 {
                    loop {
                        let exe = String::from_utf16_lossy(
                            &entry.exe_file[..entry.exe_file.iter().position(|c| *c == 0).unwrap_or(260)],
                        );
                        if exe.eq_ignore_ascii_case("zcode.exe") {
                            if let Some(cmd) = process_command_line(entry.process_id) {
                                if cmd.contains("zcode.cjs") {
                                    pids.push(entry.process_id);
                                }
                            }
                        }
                        if Process32NextW(snap, &mut entry) == 0 {
                            break;
                        }
                    }
                }
                CloseHandle(snap as *mut c_void);
            }
            pids
        }

        /// Whether any ZCode process exists in the system (process name zcode.exe; desktop shell and
        /// CLI child processes share the name—no command line read, name match suffices). Used for
        /// auto-start follow-mode standby detection (autostart.rs): either desktop or CLI running
        /// counts as "ZCode is running". Difference from discover_cli_pids: that reads command line
        /// to precisely filter CLI child processes; here just name match, faster and broader
        pub fn any_zcode_process() -> bool {
            unsafe {
                let snap = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
                if snap == -1 {
                    return false;
                }
                let mut entry = ProcessEntry32W {
                    size: std::mem::size_of::<ProcessEntry32W>() as u32,
                    usage: 0,
                    process_id: 0,
                    default_heap_id: 0,
                    module_id: 0,
                    threads: 0,
                    parent_process_id: 0,
                    pri_class_base: 0,
                    flags: 0,
                    exe_file: [0; 260],
                };
                let mut found = false;
                let ok = Process32FirstW(snap, &mut entry);
                if ok != 0 {
                    loop {
                        let end = entry.exe_file.iter().position(|c| *c == 0).unwrap_or(260);
                        if String::from_utf16_lossy(&entry.exe_file[..end])
                            .eq_ignore_ascii_case("zcode.exe")
                        {
                            found = true;
                            break;
                        }
                        if Process32NextW(snap, &mut entry) == 0 {
                            break;
                        }
                    }
                }
                CloseHandle(snap as *mut c_void);
                found
            }
        }

        pub fn io_write_bytes(h: &ProcHandle) -> Option<u64> {
            let mut io = IoCounters {
                read_ops: 0,
                write_ops: 0,
                other_ops: 0,
                read_bytes: 0,
                write_bytes: 0,
                other_bytes: 0,
            };
            unsafe {
                if GetProcessIoCounters(h.handle as *mut c_void, &mut io) != 0 {
                    Some(io.write_bytes)
                } else {
                    None
                }
            }
        }
        #[link(name = "ntdll")]
        extern "system" {
            fn NtQueryInformationProcess(
                h: *mut c_void,
                class: u32,
                info: *mut c_void,
                len: u32,
                ret_len: *mut u32,
            ) -> i32;
        }

        /// Process handle: kernel handle opened via OpenProcess (reused long-lived, not opened/closed per tick)
        pub struct ProcHandle {
            handle: isize,
        }

        pub fn open_proc(pid: u32) -> Option<ProcHandle> {
            let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED, 0, pid) } as isize;
            if handle == 0 {
                None
            } else {
                Some(ProcHandle { handle })
            }
        }

        /// Tracked file total (all jsonl in rollout dir + CLI log dir + db WAL, the source of
        /// disk-write spikes), for the cleaned stream to deduct disk writes
        pub fn tracked_files_total() -> u64 {
            let mut total = 0u64;
            if let Some(home) = crate::metrics::home_dir() {
                for dir in [
                    home.join(".zcode/cli/rollout"),
                    home.join(".zcode/cli/log"),
                ] {
                    if let Ok(rd) = std::fs::read_dir(&dir) {
                        for e in rd.flatten() {
                            if let Ok(m) = e.metadata() {
                                total += m.len();
                            }
                        }
                    }
                }
                if let Ok(m) = std::fs::metadata(home.join(".zcode/cli/db/db.sqlite-wal")) {
                    total += m.len();
                }
            }
            total
        }
    }

    /// macOS: libproc enumeration (KERN_PROCARGS2 argv contains exact arg `zcode-cli`)
    /// + proc_pid_rusage's ri_diskio_byteswritten (kernel-maintained cumulative disk write bytes
    /// for the process, authoritative and zero-overhead to read)
    #[cfg(target_os = "macos")]
    mod mac {
        use std::ffi::{c_int, c_void};

        // Link name "proc" (library file /usr/lib/libproc.dylib, link name without lib prefix)
        #[link(name = "proc")]
        extern "C" {
            /// Note buffersize unit is **bytes** (not pid count), pass pid capacity × 4
            fn proc_listallpids(buffer: *mut c_void, buffersize: c_int) -> c_int;
            fn proc_pid_rusage(pid: c_int, flavor: c_int, buffer: *mut c_void) -> c_int;
        }
        #[link(name = "System")]
        extern "C" {
            fn sysctl(
                name: *const c_int,
                namelen: u32,
                oldp: *mut c_void,
                oldlenp: *mut usize,
                newp: *mut c_void,
                newlen: usize,
            ) -> c_int;
        }

        const CTL_KERN: c_int = 1;
        const KERN_PROCARGS2: c_int = 49;

        /// rusage_info_v4 field-by-field mirror (cross-checked with macOS SDK sys/resource.h).
        /// Note newer kernel layout has ri_proc_exit_abstime after ri_proc_start_abstime,
        /// which determines the offset of ri_diskio_byteswritten—field offsets are pinned by
        /// const assertions below at compile time; SDK layout changes will cause compile failure,
        /// do not delete assertions
        #[repr(C)]
        struct RusageInfoV4 {
            ri_uuid: [u8; 16],
            ri_user_time: u64,
            ri_system_time: u64,
            ri_pkg_idle_wkups: u64,
            ri_interrupt_wkups: u64,
            ri_pageins: u64,
            ri_wired_size: u64,
            ri_resident_size: u64,
            ri_phys_footprint: u64,
            ri_proc_start_abstime: u64,
            ri_proc_exit_abstime: u64,
            ri_child_user_time: u64,
            ri_child_system_time: u64,
            ri_child_pkg_idle_wkups: u64,
            ri_child_interrupt_wkups: u64,
            ri_child_pageins: u64,
            ri_child_elapsed_abstime: u64,
            ri_diskio_bytesread: u64,
            ri_diskio_byteswritten: u64,
            ri_cpu_time_qos_default: u64,
            ri_cpu_time_qos_maintenance: u64,
            ri_cpu_time_qos_background: u64,
            ri_cpu_time_qos_utility: u64,
            ri_cpu_time_qos_legacy: u64,
            ri_cpu_time_qos_user_initiated: u64,
            ri_cpu_time_qos_user_interactive: u64,
            ri_billed_system_time: u64,
            ri_serviced_system_time: u64,
            ri_logical_writes: u64,
            ri_lifetime_max_phys_footprint: u64,
            ri_instructions: u64,
            ri_cycles: u64,
            ri_billed_energy: u64,
            ri_serviced_energy: u64,
            ri_interval_max_phys_footprint: u64,
            ri_runnable_time: u64,
        }

        /// Compile-time assertion that key field offsets match the local SDK headers (C program
        /// measured offsetof: ri_proc_start_abstime=80, ri_diskio_byteswritten=152, sizeof=296);
        /// if assertions fail fix struct layout, do not delete assertions
        const _: () = {
            assert!(std::mem::offset_of!(RusageInfoV4, ri_proc_start_abstime) == 80);
            assert!(std::mem::offset_of!(RusageInfoV4, ri_diskio_byteswritten) == 152);
            assert!(std::mem::size_of::<RusageInfoV4>() == 296);
        };

        const RUSAGE_INFO_V4: c_int = 4;

        /// Process handle: pid + process start time captured at open (absolute time).
        /// Inconsistent start time at sample time = pid has been reused, treat as process exit and prune
        pub struct ProcHandle {
            pid: u32,
            start_abstime: u64,
        }

        fn read_rusage(pid: u32) -> Option<RusageInfoV4> {
            // 512-byte buffer >= sizeof(RusageInfoV4)=296, accommodates future field growth
            let mut buf = [0u8; 512];
            let ok = unsafe {
                proc_pid_rusage(pid as c_int, RUSAGE_INFO_V4, buf.as_mut_ptr().cast::<c_void>())
            };
            if ok != 0 {
                return None;
            }
            Some(unsafe { buf.as_ptr().cast::<RusageInfoV4>().read_unaligned() })
        }

        /// Enumerate ZCode CLI processes (multiple can coexist). Identification: KERN_PROCARGS2 argv
        /// contains exact arg "zcode-cli" (CLI is forked from Electron Helper; proc_pidpath only
        /// gets "ZCode Helper" executable path, cannot distinguish from other Helper processes;
        /// measured CLI process argv[1] == "zcode-cli").
        /// proc_listallpids two-pass: first pass null buffer to get pid count, then get list
        /// (buffersize unit is bytes); treat m <= 0 as failure, return empty
        pub fn discover_cli_pids() -> Vec<u32> {
            let mut pids = Vec::new();
            unsafe {
                let n = proc_listallpids(std::ptr::null_mut(), 0);
                if n <= 0 {
                    return pids;
                }
                let cap = (n + 16) as usize;
                let mut buf = vec![0i32; cap];
                let m = proc_listallpids(buf.as_mut_ptr().cast::<c_void>(), (cap * 4) as c_int);
                if m <= 0 {
                    return pids;
                }
                let mut scratch = vec![0u8; 64 * 1024];
                for pid in &buf[..m as usize] {
                    if *pid > 0 && argv_has_cli_marker(*pid as u32, &mut scratch) {
                        pids.push(*pid as u32);
                    }
                }
            }
            pids
        }

        /// Whether exact string "zcode-cli" exists in KERN_PROCARGS2 packed region.
        /// Layout: [nargs: i32][argv0 … (argv0 followed by alignment NUL padding) argv1..][envp…];
        /// alignment padding and empty args are hard to distinguish, do not precisely reconstruct
        /// argv boundaries—directly scan all NUL-terminated strings for exact match (measured
        /// CLI process arg region has an independent "zcode-cli" string; envp strings are all
        /// KEY=VALUE form, no name collision; same breadth as Windows side "command line contains
        /// zcode.cjs"). 64KB covers normal processes; parse failure / insufficient perms treated
        /// as non-match (no panic)
        fn argv_has_cli_marker(pid: u32, buf: &mut [u8]) -> bool {
            let mib = [CTL_KERN, KERN_PROCARGS2, pid as c_int];
            let mut len = buf.len();
            let ok = unsafe {
                sysctl(
                    mib.as_ptr(),
                    3,
                    buf.as_mut_ptr().cast::<c_void>(),
                    &mut len,
                    std::ptr::null_mut(),
                    0,
                )
            };
            if ok != 0 || len < 4 {
                return false;
            }
            let mut pos = 4usize;
            while pos < len {
                while pos < len && buf[pos] == 0 {
                    pos += 1; // Skip NUL / alignment padding
                }
                if pos >= len {
                    break;
                }
                let start = pos;
                while pos < len && buf[pos] != 0 {
                    pos += 1;
                }
                if &buf[start..pos] == b"zcode-cli" {
                    return true;
                }
            }
            false
        }

        /// Whether any ZCode process exists in the system (desktop or CLI, either counts).
        /// Used for auto-start follow-mode standby detection (autostart.rs)
        pub fn any_zcode_process() -> bool {
            unsafe {
                let n = proc_listallpids(std::ptr::null_mut(), 0);
                if n <= 0 {
                    return false;
                }
                let cap = (n + 16) as usize;
                let mut buf = vec![0i32; cap];
                let m = proc_listallpids(buf.as_mut_ptr().cast::<c_void>(), (cap * 4) as c_int);
                if m <= 0 {
                    return false;
                }
                let mut scratch = vec![0u8; 64 * 1024];
                for pid in &buf[..m as usize] {
                    if *pid > 0 && proc_is_zcode(*pid as u32, &mut scratch) {
                        return true;
                    }
                }
            }
            false
        }

        /// KERN_PROCARGS2 "ZCode process" check: argv[0] (executable path, first NUL-terminated
        /// string after 4-byte nargs header) ends with /ZCode = desktop main process;
        /// arg region contains exact string zcode-cli = CLI child process (reuses discover's spec)
        fn proc_is_zcode(pid: u32, buf: &mut [u8]) -> bool {
            let mib = [CTL_KERN, KERN_PROCARGS2, pid as c_int];
            let mut len = buf.len();
            let ok = unsafe {
                sysctl(
                    mib.as_ptr(),
                    3,
                    buf.as_mut_ptr().cast::<c_void>(),
                    &mut len,
                    std::ptr::null_mut(),
                    0,
                )
            };
            if ok != 0 || len < 4 {
                return false;
            }
            if let Some(end) = buf[4..len].iter().position(|b| *b == 0).map(|p| p + 4) {
                if buf[4..end].to_ascii_lowercase().ends_with(b"/zcode") {
                    return true;
                }
            }
            argv_has_cli_marker(pid, buf)
        }

        pub fn open_proc(pid: u32) -> Option<ProcHandle> {
            let ru = read_rusage(pid)?;
            Some(ProcHandle {
                pid,
                start_abstime: ru.ri_proc_start_abstime,
            })
        }

        /// Cumulative write bytes since process start (ri_diskio_byteswritten). Returns None if
        /// process exited or pid reused (start_abstime changed); pruned by upper layer on that tick
        pub fn io_write_bytes(h: &ProcHandle) -> Option<u64> {
            let ru = read_rusage(h.pid)?;
            if ru.ri_proc_start_abstime != h.start_abstime {
                return None;
            }
            Some(ru.ri_diskio_byteswritten)
        }

        /// macOS does not do tracked file deduction: measured 120s probe rollout dir du net change
        /// was negative (CLI cleanup rotation), negative deltas would contaminate the cleaned stream;
        /// and constant 0 avoids per-tick directory scanning
        pub fn tracked_files_total() -> u64 {
            0
        }
    }

    /// Other platforms: IO probing unavailable (panel falls back to window/estimate display)
    #[cfg(not(any(windows, target_os = "macos")))]
    mod stub {
        pub struct ProcHandle;

        pub fn discover_cli_pids() -> Vec<u32> {
            Vec::new()
        }
        pub fn any_zcode_process() -> bool {
            false
        }
        pub fn open_proc(_pid: u32) -> Option<ProcHandle> {
            None
        }
        pub fn io_write_bytes(_h: &ProcHandle) -> Option<u64> {
            None
        }
        pub fn tracked_files_total() -> u64 {
            0
        }
    }

    #[cfg(windows)]
    pub use win::*;
    #[cfg(target_os = "macos")]
    pub use mac::*;
    #[cfg(not(any(windows, target_os = "macos")))]
    pub use stub::*;
}

pub struct LiveIo {
    procs: HashMap<u32, ProcRing>,
    last_refresh: Option<Instant>,
    /// (time ms, tracked file cumulative bytes)
    files_hist: VecDeque<(i64, u64)>,
    pending: VecDeque<Call>,
    cal: VecDeque<f64>,
    /// Cleaning/calibration parameters (platform-parameterized, locked at startup)
    params: CleanParams,
    bytes_per_token: f64,
    last_result: LiveNow,
    /// Session → most recent CLI process that generated output for it
    session_pid: HashMap<String, u32>,
    /// Most recent time cleaned rate reached streaming threshold (stop-detection fallback timing origin)
    last_stream_ms: Option<i64>,
    /// Currently focused session = most recently completed call's session (only used as gate seed
    /// when no call baseline exists)
    current_session: Option<String>,
    attributed: HashSet<String>,
    history_done: bool,
    /// All in-progress calls (Engine determines from message table): (session, call start time);
    /// during multi-task concurrency, real-time speed aggregates by attributed process union
    inflight: Vec<(String, i64)>,
    /// Span anchor for this call segment (first time streaming threshold reached, wall-clock ms)
    active_since: Option<i64>,
    /// Most recent calibration event (for debug logging)
    pending_cal: Option<CalEvent>,
    /// Whether CLI processes were ever discovered in this process's lifetime (distinguishes
    /// "never available" from "exited")
    ever_saw_procs: bool,
}

impl LiveIo {
    pub fn new() -> Self {
        // Coefficient queue preset with default as prior sample: during cold start a single outlier
        // sample cannot monopolize the median; needs 2 real samples to move the coefficient;
        // after 5 samples the prior is naturally pushed out
        let params = CleanParams::platform();
        let mut cal = VecDeque::with_capacity(5);
        cal.push_back(params.default_bpt);
        Self {
            procs: HashMap::new(),
            last_refresh: None,
            files_hist: VecDeque::new(),
            pending: VecDeque::new(),
            cal,
            params,
            bytes_per_token: params.default_bpt,
            last_result: LiveNow::default(),
            session_pid: HashMap::new(),
            last_stream_ms: None,
            current_session: None,
            attributed: HashSet::new(),
            history_done: false,
            inflight: Vec::new(),
            active_since: None,
            pending_cal: None,
            ever_saw_procs: false,
        }
    }

    /// Inject today's existing calls at startup, used to determine current session
    pub fn ingest_history(&mut self, calls: &[Call]) {
        if let Some(latest) = calls.iter().max_by_key(|c| c.completed_ms) {
            self.current_session = Some(latest.session.clone());
        }
        self.history_done = true;
    }

    pub fn history_done(&self) -> bool {
        self.history_done
    }

    pub fn observe(&mut self, new_calls: &[Call]) {
        for c in new_calls {
            self.pending.push_back(c.clone());
            // Most recently completed call's session = currently focused session
            self.current_session = Some(c.session.clone());
        }
        while self.pending.len() > 8 {
            self.pending.pop_front();
        }
    }

    /// Update "call in progress" signal per tick (Engine derives from message table vs completion rows;
    /// during multi-task concurrency, all in-progress sessions)
    pub fn set_inflight(&mut self, inflight: Vec<(String, i64)>) {
        self.inflight = inflight;
    }

    /// For debug logging: attribution mapping of in-progress sessions (session id → pid; sessions without attribution omitted)
    pub fn inflight_attr(&self) -> Vec<(String, u32)> {
        self.inflight
            .iter()
            .filter_map(|(s, _)| self.session_pid.get(s).map(|p| (s.clone(), *p)))
            .collect()
    }

    /// Current active byte→token coefficient (for debug logging)
    pub fn bytes_per_token(&self) -> f64 {
        self.bytes_per_token
    }

    /// Whether CLI processes were ever discovered. Distinguishes "never available" (IO probing
    /// unavailable environment, estimate fallback allowed) from "discovered then all exited"
    /// (CLI closed, should not continue showing generating/estimating)
    pub fn ever_saw_procs(&self) -> bool {
        self.ever_saw_procs
    }

    /// Take the most recent calibration event (if any)
    pub fn take_calibration(&mut self) -> Option<CalEvent> {
        self.pending_cal.take()
    }

    /// Recalibrate (current speed stuck manual button / per-round drift auto-trigger): discard
    /// learned coefficient samples, return to platform-prior cold-start state (prior placeholder
    /// prevents single-sample monopoly), reconverge from subsequent completed-call samples.
    /// pending calls retained—session→process attribution still needs processing; their old-magnitude
    /// samples are pushed out by new samples within 1-2 rounds under the sliding window.
    /// Returns coefficient after reset
    pub fn reset_calibration(&mut self) -> f64 {
        self.cal.clear();
        self.cal.push_back(self.params.default_bpt);
        self.bytes_per_token = self.params.default_bpt;
        self.bytes_per_token
    }

    /// Export coefficient sample queue (for persistence; after recalibration it's [prior];
    /// persist on queue change to preserve "recalibration intent" across restarts)
    pub fn cal_state(&self) -> Vec<f64> {
        self.cal.iter().copied().collect()
    }

    /// Restore coefficient sample queue from persistence: only accepts finite values within range,
    /// injects into queue (same capacity as real-time calibration, oldest dropped on overflow),
    /// active coefficient recomputed as upper median of restored queue (same spec as calibration
    /// path). Empty / all-invalid leaves prior unchanged, returns actual accepted count
    pub fn restore_cal(&mut self, samples: Vec<f64>) -> usize {
        let valid: Vec<f64> = samples
            .into_iter()
            .filter(|v| v.is_finite() && *v >= self.params.cal_min && *v <= self.params.cal_max)
            .collect();
        let n = valid.len();
        for v in valid {
            self.cal.push_back(v);
        }
        while self.cal.len() > CAL_QUEUE_CAP {
            self.cal.pop_front();
        }
        let mut sorted: Vec<f64> = self.cal.iter().copied().collect();
        sorted.sort_by(|a, b| a.partial_cmp(b).unwrap());
        self.bytes_per_token = sorted[sorted.len() / 2];
        n
    }

    /// Call once per polling cycle. now_ms is wall-clock milliseconds (same source as Engine snapshot)
    pub fn measure(&mut self, now_ms: i64) -> LiveNow {
        let now = Instant::now();
        // Periodically refresh CLI process set; when no processes found, there exist in-progress
        // sessions without attribution records (newly opened second window—display uses all-process
        // sum fallback; if new process not discovered, reads another window's speed), or >= 2
        // in-progress sessions (multi-task concurrency, dedup needs to see new process byte
        // distribution ASAP), shorten to 2s instead of waiting full 30s
        let unattributed_inflight =
            !self.inflight.is_empty() && self.inflight.iter().any(|(s, _)| !self.session_pid.contains_key(s));
        let refresh_due = self
            .last_refresh
            .map_or(true, |t| now.duration_since(t) > REFRESH_EVERY);
        let quick_due = self
            .last_refresh
            .map_or(true, |t| now.duration_since(t) > Duration::from_secs(2));
        if refresh_due
            || ((self.procs.is_empty() || unattributed_inflight || self.inflight.len() >= 2) && quick_due)
        {
            self.last_refresh = Some(now);
            let found = platform::discover_cli_pids();
            self.procs.retain(|pid, _| found.contains(pid));
            for pid in found {
                if let Some(handle) = platform::open_proc(pid) {
                    self.procs.entry(pid).or_insert_with(|| ProcRing {
                        handle,
                        samples: VecDeque::new(),
                        min_delta: f64::MAX,
                    });
                }
            }
            if !self.procs.is_empty() {
                self.ever_saw_procs = true;
            }
        }

        // Sample write bytes and tracked file total (paired same tick, timestamps consistent)
        self.procs.retain(|_, ring| {
            match platform::io_write_bytes(&ring.handle) {
                Some(w) => {
                    ring.samples.push_back((now_ms, w));
                    while ring.samples.len() > RING_CAP {
                        ring.samples.pop_front();
                    }
                    true
                }
                None => false, // Process exited
            }
        });
        // Synchronously prune session mappings for dead PIDs, prevents memory leaks and PID-reuse dirty attribution
        let active_pids: HashSet<u32> = self.procs.keys().copied().collect();
        self.session_pid.retain(|_, pid| active_pids.contains(pid));
        if self.session_pid.len() > 200 {
            self.session_pid.clear();
        }
        let ft = platform::tracked_files_total();
        self.files_hist.push_back((now_ms, ft));
        while self.files_hist.len() > RING_CAP {
            self.files_hist.pop_front();
        }
        let files: Vec<(i64, u64)> = self.files_hist.iter().copied().collect();

        // Cleaned tick sequence (display and calibration share the same stream, ensuring spec consistency)
        let mut rows_by_pid: HashMap<u32, Vec<TickRow>> = HashMap::new();
        for (pid, ring) in self.procs.iter_mut() {
            let samples: Vec<(i64, u64)> = ring.samples.iter().copied().collect();
            // Low-decile × 2 of per-tick minimum delta as this process's heartbeat noise floor
            // (bytes/tick); when too few samples, don't use adaptive floor (avoids eating startup
            // signal during cold start). Capped inside build_rows when used, prevents poisoning
            // by own increments during sustained streaming
            let mut deltas: Vec<f64> = samples
                .windows(2)
                .map(|w| w[1].1.saturating_sub(w[0].1) as f64)
                .collect();
            if !deltas.is_empty() {
                deltas.sort_by(|a: &f64, b: &f64| a.partial_cmp(b).unwrap());
                ring.min_delta = if deltas.len() >= 20 {
                    deltas[deltas.len() / 10] * 2.0
                } else {
                    0.0
                };
            }
            rows_by_pid.insert(*pid, build_rows(&samples, ring.min_delta, &self.params));
        }

        // ---- Calibration + session→process attribution (process after call completes) ----
        // Attribution uses raw bytes (uncleaned): disk-write misalignment / noise deduction does not
        // affect the "which process is writing" judgment. Coefficient numerator uses the identical
        // cleaned stream as display integrated over [first_token, completed]; systematic deductions
        // are canceled by the coefficient, display converges to true t/s
        while let Some(call) = self.pending.front().cloned() {
            if now_ms - call.completed_ms > 120_000 {
                self.pending.pop_front();
                continue;
            }
            // Delayed disk-write grace (mac): disk write bytes are page-cache asynchronous writeback
            // count; after completed, wait full grace before integrating, letting dirty pages enter
            // the count. During grace, stays at queue front; > 120s discard judgment above ensures
            // no backlog. Windows=0 skips this branch, per-tick processing unchanged
            if self.params.cal_grace_ms > 0 && now_ms - call.completed_ms < self.params.cal_grace_ms
            {
                break;
            }
            let stream_start_ms = (call.completed_ms - call.gen_ms.min(300_000)).max(0);
            // Integral/stats window upper limit extended to completed + grace: mac's dirty pages
            // lag in writeback; numerator (clean) and denominator spec (raw) both extended;
            // pred spec still uses real gen_ms
            let window_end_ms = call.completed_ms + self.params.cal_grace_ms;
            let mut raw_by_pid: HashMap<u32, u64> = HashMap::new();
            for (pid, ring) in self.procs.iter() {
                let mut acc = 0u64;
                for (a, b) in ring.samples.iter().zip(ring.samples.iter().skip(1)) {
                    if b.0 >= stream_start_ms && a.0 <= window_end_ms {
                        acc += b.1.saturating_sub(a.1);
                    }
                }
                raw_by_pid.insert(*pid, acc);
            }
            let raw_total = raw_by_pid.values().sum::<u64>() as f64;
            // Candidates sorted by bytes descending, ties by pid ascending (deterministic);
            // shared between attribution and event recording
            let mut cands: Vec<(u32, u64)> = raw_by_pid.iter().map(|(p, b)| (*p, *b)).collect();
            cands.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
            let top_pid = cands.first().map(|(p, _)| *p);
            if !self.attributed.contains(&call.id) {
                self.attributed.insert(call.id.clone());
                if self.attributed.len() > 4_000 {
                    self.attributed.clear();
                }
                if raw_total > 20_000.0 {
                    // Pids already occupied by other in-progress sessions (concurrent dedup:
                    // on ties don't crowd onto the same process; when two sessions truly share
                    // one process, second-highest only has noise ratio, unaffected)
                    let owned: HashSet<u32> = self
                        .inflight
                        .iter()
                        .filter(|(s, _)| s != &call.session)
                        .filter_map(|(s, _)| self.session_pid.get(s).copied())
                        .collect();
                    if let Some(pid) =
                        pick_attribution(self.session_pid.get(&call.session).copied(), &raw_by_pid, &owned)
                    {
                        self.session_pid.insert(call.session.clone(), pid);
                    }
                }
            }
            // Clean integral: prefer attributed process (same aggregated stream as display path,
            // tracked file growth also deducted once); fall back to all-process aggregation when unattributed
            let mut attr_pid = None;
            let clean_bytes = match self.session_pid.get(&call.session) {
                Some(pid) if rows_by_pid.contains_key(pid) => {
                    attr_pid = Some(*pid);
                    let streams = [&rows_by_pid[pid][..]];
                    integrate(
                        &merge_streams(&streams, &files),
                        stream_start_ms,
                        window_end_ms,
                    )
                    .0
                }
                _ => {
                    let streams: Vec<&[TickRow]> =
                        rows_by_pid.values().map(|v| &v[..]).collect();
                    integrate(
                        &merge_streams(&streams, &files),
                        stream_start_ms,
                        window_end_ms,
                    )
                    .0
                }
            };
            // Numerator-denominator pairing: when clean comes from attributed process, raw baseline
            // also uses attributed process's (all-process raw would count other window's bytes into
            // denominator, causing clean/raw guard misjudgment). Cross-process guard reference:
            // attributed process bytes; unattributed all-process sum branch uses top process as
            // reference—that branch's integral also mixes other window's bytes, cannot be let through
            let top_raw = top_pid
                .map_or(0.0, |p| *raw_by_pid.get(&p).unwrap_or(&0) as f64);
            let attr_raw = attr_pid
                .map_or(raw_total, |p| *raw_by_pid.get(&p).unwrap_or(&0) as f64);
            let guard_ref = attr_pid.map_or(top_raw, |_| attr_raw);
            let others_raw = (raw_total - guard_ref).max(0.0);
            let eff = call.effective_out();
            let true_tps = eff as f64 / (call.gen_ms.max(50) as f64 / 1000.0);
            let (mut in_cal, bpt_sample) =
                cal_sample(eff, clean_bytes, attr_raw, self.bytes_per_token, &self.params);
            if in_cal && !cross_pid_ok(others_raw, guard_ref) {
                in_cal = false;
            }
            if in_cal {
                self.bytes_per_token = median_bpt(&mut self.cal, bpt_sample, CAL_QUEUE_CAP);
            }
            self.pending_cal = Some(CalEvent {
                id: call.id.clone(),
                session: call.session.clone(),
                completed_ms: call.completed_ms,
                true_tps,
                gen_ms: call.gen_ms,
                eff,
                raw_bytes: raw_total,
                clean_bytes,
                bpt_sample: if in_cal { bpt_sample } else { 0.0 },
                bpt_now: self.bytes_per_token,
                cal_skipped: !in_cal,
                attr_pid,
                top_pid,
                others_bytes: others_raw,
            });
            self.pending.pop_front();
        }

        // Display process set: all in-progress sessions have live attributions → attributed pid union
        // (multi-task concurrency aggregates to true total throughput); any session without attribution
        // (new session / sub-agent's first call not yet completed) → all-process sum fallback, to avoid
        // missing new processes without attribution records and showing another window's speed
        let pid_set = pick_pid_set(&self.inflight, &self.session_pid, &active_pids);
        // Aggregated stream: per-tick byte sum over the set + tracked file growth deducted once overall
        // (duration also counted once—integrating per-process separately would sum wall-clock, diluting
        // rate into cross-process average)
        let display_rows: Vec<TickRow> = {
            let streams: Vec<&[TickRow]> = match &pid_set {
                Some(set) => set
                    .iter()
                    .filter_map(|p| rows_by_pid.get(p).map(|v| &v[..]))
                    .collect(),
                None => rows_by_pid.values().map(|v| &v[..]).collect(),
            };
            merge_streams(&streams, &files)
        };
        let span = |from_ms: i64| -> (f64, f64) { integrate(&display_rows, from_ms, now_ms) };

        // Start/stop decision (call gate): message table's assistant row is submitted at call start
        // instant; the row's data.time.completed is backfilled at end (including cancel/error)—the gate
        // directly trusts this signal and is not session-limited (first call in new conversation lights
        // up same tick, no need to wait for first completion row); end/cancel zeroes same tick. Tool
        // execution / standby periods also have UI state bursts on the pipe; the gate reliably excludes
        // them. Process guard: when all in-progress sessions' attributed processes have exited (after
        // crash / terminal close, no one backfills completed), force stop detection, leaving no zombie
        // "generating"; sessions without attribution are not judged dead. When gate unavailable (no call
        // exists to baseline), degrades to pure byte decision.
        let proc_gone = {
            let mut any_alive = false;
            for (s, _) in &self.inflight {
                match self.session_pid.get(s) {
                    Some(pid) if self.procs.contains_key(pid) => any_alive = true,
                    Some(_) => {}
                    None => any_alive = true,
                }
            }
            !self.inflight.is_empty() && !any_alive
        };
        let (det_b, det_s) = span(now_ms - self.params.detect_ms);
        let detect_bps = if det_s > 0.0 { det_b / det_s } else { 0.0 };
        let gate_on = !proc_gone
            && (!self.inflight.is_empty()
                || (self.current_session.is_none() && detect_bps > STREAMING_BPS));
        if gate_on {
            // Span anchor: first time cleaned rate reaches streaming threshold within this streaming
            // segment. After flow resumes following flow break (stop-detection fallback triggered /
            // aggregate segment switch) re-anchor—30s sliding window starts from new segment, not
            // diluting reading with silent segment
            if detect_bps > STREAMING_BPS {
                if !self.last_result.streaming || self.active_since.is_none() {
                    self.active_since = Some(now_ms);
                }
                // Stop-detection fallback timing: continuously renewed while cleaned rate stays at threshold
                self.last_stream_ms = Some(now_ms);
            }
        } else {
            self.active_since = None;
            self.last_stream_ms = None;
        }

        // Span: 30s sliding window ∩ [first-byte tick, now] cleaned stream integral. First-byte tick
        // has a real reading (previously TTFT, shows "collecting stats"); steady state covers full 30s
        // (smoothing); stops same tick zeroes. Negative ticks from disk flush misalignment cancel within
        // window, only clamped non-negative at summary
        let pipe_bps = if gate_on {
            match self.active_since {
                Some(anchor) => {
                    let from = anchor.max(now_ms - WINDOW_MS);
                    let (b, s) = span(from);
                    if s > 0.0 {
                        (b / s).max(0.0)
                    } else {
                        0.0
                    }
                }
                None => 0.0, // First byte not arrived (TTFT), shows collecting stats
            }
        } else {
            0.0
        };
        // Stop-detection fallback: gate still open (completed not flushed) but cleaned flow has ceased
        // for longer than grace after anchor → generation actually stopped, zero display; upper layer
        // streaming=false takes idle branch, no longer uses window fallback to hang "generating". Byte
        // recovery self-heals same tick (timer refreshes with flow)
        let streaming =
            gate_on && !stale_stop(self.active_since, self.last_stream_ms, now_ms);
        let ramping = streaming
            && (self.active_since.is_none()
                || self
                    .active_since
                    .map_or(false, |a| now_ms - a < WINDOW_MS));
        // Startup hint: gate open but first byte not arrived (TTFT, takes latest-started session),
        // bounded within hint window—within window shows "collecting stats…", if window elapses with
        // no bytes, upper layer falls back to estimate (silent-pipe call)
        let awaiting = streaming
            && awaiting_hint(
                self.inflight.iter().map(|(_, t)| *t).max(),
                self.active_since,
                now_ms,
            );

        // Per-task breakdown: cleaned rate of each process in the display set within the aggregated
        // window. File growth apportioned by each process's **positive** byte share (negative-tick
        // processes don't participate in apportioning, their own value clamped to 0, otherwise breakdown
        // sum would exceed aggregate reading); empty when gate closed / not streaming / startup (TTFT)
        let mut tasks: Vec<TaskLive> = Vec::new();
        let mut active_pid_count = 0usize;
        if streaming && !awaiting {
            let from = self
                .active_since
                .map_or(now_ms, |a| a.max(now_ms - WINDOW_MS));
            let (_, wall_s) = integrate(&display_rows, from, now_ms);
            let fg_w = integrate(&file_growth_rows(&files), from, now_ms).0;
            let pids: Vec<u32> = match &pid_set {
                Some(set) => set.clone(),
                None => rows_by_pid.keys().copied().collect(),
            };
            let mut per: Vec<(u32, f64, bool)> = Vec::with_capacity(pids.len());
            let mut sum_pos = 0.0;
            for pid in &pids {
                let Some(rows) = rows_by_pid.get(pid) else { continue };
                let (b, _) = integrate(rows, from, now_ms);
                sum_pos += b.max(0.0);
                let (db, ds) = integrate(rows, now_ms - self.params.detect_ms, now_ms);
                let dbps = if ds > 0.0 { db / ds } else { 0.0 };
                per.push((*pid, b, dbps > STREAMING_BPS));
            }
            // Number of processes whose window contribution reached streaming magnitude (drift detection's
            // "single-process round" criterion; idle process noise leakage not counted)
            active_pid_count = per
                .iter()
                .filter(|(_, b, _)| *b > STREAMING_BPS * wall_s)
                .count();
            for (pid, b, is_stream) in per {
                let share = if sum_pos > 0.0 { b.max(0.0) / sum_pos } else { 0.0 };
                let tps = if wall_s > 0.0 && self.bytes_per_token > 0.0 {
                    ((b - fg_w * share) / wall_s).max(0.0) / self.bytes_per_token
                } else {
                    0.0
                };
                tasks.push(TaskLive {
                    pid,
                    session: None,
                    n_sessions: 0,
                    tps,
                    streaming: is_stream,
                });
            }
            // Session label and count: in-progress sessions fill to their attributed process by attribution;
            // multi-session on same process (new task in same ZCode window reuses app-server) honestly
            // counted as n_sessions, speed is process total—cannot split at byte level, label takes first session
            for (s, _) in &self.inflight {
                if let Some(pid) = self.session_pid.get(s) {
                    if let Some(t) = tasks.iter_mut().find(|t| t.pid == *pid) {
                        t.n_sessions += 1;
                        if t.session.is_none() {
                            t.session = Some(s.clone());
                        }
                    }
                }
            }
            // Idle processes with no attributed session don't occupy breakdown slots; stable sort by speed descending
            tasks.retain(|t| t.streaming || t.session.is_some());
            tasks.sort_by(|a, b| b.tps.partial_cmp(&a.tps).unwrap_or(std::cmp::Ordering::Equal));
        }
        let n_pids = if streaming && !awaiting {
            active_pid_count
        } else {
            0
        };
        // Diagnostic: detection-window cleaned rate (KB/s) of each tracked process (for tick debug logging)
        let proc_bps: Vec<(u32, f64)> = rows_by_pid
            .iter()
            .map(|(pid, rows)| {
                let (db, ds) = integrate(rows, now_ms - self.params.detect_ms, now_ms);
                (*pid, if ds > 0.0 { (db / ds / 1024.0 * 10.0).round() / 10.0 } else { 0.0 })
            })
            .collect();

        let result = LiveNow {
            available: !self.procs.is_empty(),
            streaming,
            ramping,
            awaiting,
            tps: if streaming {
                pipe_bps / self.bytes_per_token
            } else {
                0.0
            },
            pipe_bps,
            n_pids,
            tasks,
            proc_bps,
        };
        self.last_result = result.clone();
        result
    }
}

// ============ Tests ============
#[cfg(test)]
mod tests {
    use super::*;

    const TICK: i64 = 700;

    /// Build (time, cumulative bytes) sample sequence: per-tick delta given by deltas
    fn series(start_ms: i64, deltas: &[f64]) -> Vec<(i64, u64)> {
        let mut out = Vec::with_capacity(deltas.len() + 1);
        let mut acc = 0u64;
        out.push((start_ms, acc));
        for (i, d) in deltas.iter().enumerate() {
            acc += *d as u64;
            out.push((start_ms + (i as i64 + 1) * TICK, acc));
        }
        out
    }

    #[test]
    fn burst_tick_dropped_entirely() {
        // 190KB request-body burst tick dropped entirely (neither bytes nor duration enter integration);
        // 52KB true streaming tick retained
        let s = series(0, &[190_000.0, 52_000.0, 52_000.0]);
        let rows = build_rows(&s, 0.0, &CleanParams::windows());
        assert_eq!(rows.len(), 2);
        assert!(rows.iter().all(|r| r.bytes < 52_000.0));
        // Burst tick doesn't even contribute duration: two retained ticks' dt totals 1.4s
        let secs = integrate(&rows, i64::MIN, i64::MAX).1;
        assert!((secs - 1.4).abs() < 1e-9);
    }

    #[test]
    fn floor_capped_against_ring_poisoning() {
        // Adaptive floor poisoned to 50KB/tick (quantile elevated during sustained streaming);
        // after capping, each tick deducts at most FLOOR_CAP + BASE_NOISE×dt, 52KB streaming tick keeps bulk
        let s = series(0, &[52_000.0; 4]);
        let rows = build_rows(&s, 50_000.0, &CleanParams::windows());
        let floor = FLOOR_CAP_BYTES + BASE_NOISE_BPS * (TICK as f64 / 1000.0);
        for r in &rows {
            assert!((r.bytes - (52_000.0 - floor)).abs() < 1e-6);
        }
    }

    /// Aggregated stream: file growth deducted once overall (deducting per-process during multi-process
    /// summation would deduct N times); wall-clock duration also counted once (integrating per-process
    /// separately would sum durations, diluting rate into average)
    #[test]
    fn merge_streams_deducts_file_growth_once_and_counts_secs_once() {
        // Two processes each write 52KB same tick, file grows 10KB per tick
        let a = series(0, &[52_000.0; 4]);
        let b = series(0, &[52_000.0; 4]);
        let files: Vec<(i64, u64)> = (0..5)
            .map(|i| (a[0].0 + i as i64 * TICK, i as u64 * 10_000))
            .collect();
        let rows_a = build_rows(&a, 0.0, &CleanParams::windows());
        let rows_b = build_rows(&b, 0.0, &CleanParams::windows());
        let merged = merge_streams(&[&rows_a, &rows_b], &files);
        let floor = BASE_NOISE_BPS * (TICK as f64 / 1000.0);
        // Single tick = 2×(52_000 − floor) − 10_000 (file deducted once), not
        // 2×(52_000 − floor − 10_000)
        for r in &merged {
            assert!((r.bytes - (2.0 * (52_000.0 - floor) - 10_000.0)).abs() < 1e-6);
        }
        // Duration counted once: 4 ticks = 2.8s (integrating per-process separately then summing would give 5.6s)
        let (b_sum, secs) = integrate(&merged, i64::MIN, i64::MAX);
        assert!((secs - 2.8).abs() < 1e-9, "wall-clock duration should be counted once: {secs}");
        // Byte total = 2 processes × 4 ticks × (52_000−floor) − 4 ticks file growth (deducted once each)
        assert!((b_sum - (8.0 * (52_000.0 - floor) - 40_000.0)).abs() < 1e-6);
        // Single-stream input arithmetically equivalent to historical "per-row file deduction":
        // flush-misalignment tick negative, interval cancellation
        let s = series(0, &[20_000.0, 60_000.0, 20_000.0]);
        let flush_files = vec![
            (s[0].0, 0u64),
            (s[1].0, 0u64),
            (s[2].0, 60_000u64),
            (s[3].0, 60_000u64),
        ];
        let single = merge_streams(&[&build_rows(&s, 0.0, &CleanParams::windows())], &flush_files);
        assert!(single[1].bytes < 0.0, "flush tick should be negative: {}", single[1].bytes);
        let (b2, _) = integrate(&single, i64::MIN, i64::MAX);
        assert!((b2 - (100_000.0 - 60_000.0 - 3.0 * floor)).abs() < 1e-6);
    }

    /// Display process set: all in-progress sessions have live attributions → attributed pid union;
    /// any session without attribution (new session / sub-agent's first call not yet completed)
    /// → None = all-process sum fallback
    #[test]
    fn pick_pid_set_union_and_fallback() {
        let mut sp = HashMap::new();
        sp.insert("a".to_string(), 1u32);
        sp.insert("b".to_string(), 2u32);
        let active: HashSet<u32> = [1u32, 2u32, 3u32].into_iter().collect();
        // Two sessions each attributed → union (deduped, ascending)
        let inflight = vec![("b".to_string(), 5i64), ("a".to_string(), 3i64)];
        assert_eq!(
            pick_pid_set(&inflight, &sp, &active),
            Some(vec![1u32, 2u32])
        );
        // Two sessions attributed to same process (main session and its sub-agent) → single-element set
        sp.insert("c".to_string(), 1u32);
        let inflight = vec![("a".to_string(), 3i64), ("c".to_string(), 9i64)];
        assert_eq!(pick_pid_set(&inflight, &sp, &active), Some(vec![1u32]));
        // New session without attribution → all-process sum fallback (must not miss its own process)
        let inflight = vec![("a".to_string(), 3i64), ("new".to_string(), 9i64)];
        assert_eq!(pick_pid_set(&inflight, &sp, &active), None);
        // Attributed process dead → same fallback
        let dead: HashSet<u32> = [2u32].into_iter().collect();
        let inflight = vec![("a".to_string(), 3i64)];
        assert_eq!(pick_pid_set(&inflight, &sp, &dead), None);
        // No in-progress sessions → None (gate closed)
        assert_eq!(pick_pid_set(&[], &sp, &active), None);
    }

    /// Attribution switch hysteresis: with existing attribution, only switches when candidate process's
    /// window bytes >= 2x, preventing per-call flipping between concurrent windows (2026-09-18 field:
    /// consecutive calls in same session attribution swung, coefficient polluted swinging 262-764,
    /// reading deviation 2-3x)
    #[test]
    fn should_reattribute_hysteresis() {
        let raws: HashMap<u32, u64> = [(1u32, 100_000u64), (2u32, 150_000u64), (3u32, 300_000u64)]
            .into_iter()
            .collect();
        // No current attribution → directly trust top
        assert_eq!(should_reattribute(None, Some(2), &raws), Some(2));
        // Top same as current attribution → unchanged
        assert_eq!(should_reattribute(Some(1), Some(1), &raws), Some(1));
        // Top only 1.5x (below hysteresis) → keep current attribution, no flip
        assert_eq!(should_reattribute(Some(1), Some(2), &raws), Some(1));
        // Top 3x (>= 2x hysteresis) → switch
        assert_eq!(should_reattribute(Some(1), Some(3), &raws), Some(3));
        // Current attribution has zero bytes in window (stale attribution) → self-heal switch to top
        let raws0: HashMap<u32, u64> = [(9u32, 0u64), (3u32, 300_000u64)].into_iter().collect();
        assert_eq!(should_reattribute(Some(9), Some(3), &raws0), Some(3));
        // No top (all zero bytes) → no change
        assert_eq!(should_reattribute(Some(1), None, &raws), None);
    }

    /// Attribution dedup (concurrent tie): on first attribution, top occupied by another in-progress
    /// session, second-highest bytes reach half → reattribute to second-highest, two sessions don't crowd
    /// onto same process (display set collapses to single process); second-highest only has noise ratio
    /// (truly sharing one app-server process) → keep top.
    /// 2026-09-18 field: new task in same ZCode window reuses same app-server (others only ~0.17 ratio
    /// noise), cross-project tasks split to different app-servers (concurrent tie ~1)
    #[test]
    fn pick_attribution_dedup_on_tie() {
        // First attribution, top(1) occupied, second-highest(2) bytes 95% → attribute to 2
        let raws: HashMap<u32, u64> = [(1u32, 4_000_000u64), (2u32, 3_800_000u64)]
            .into_iter()
            .collect();
        let owned: HashSet<u32> = [1u32].into_iter().collect();
        assert_eq!(pick_attribution(None, &raws, &owned), Some(2));
        // Second-highest only noise ratio (~0.17) → keep shared top
        let raws: HashMap<u32, u64> = [(1u32, 4_000_000u64), (2u32, 700_000u64)]
            .into_iter()
            .collect();
        assert_eq!(pick_attribution(None, &raws, &owned), Some(1));
        // Tie with equal bytes → bytes-descending tie broken by pid ascending, top=1 occupied → attribute to 2
        let raws: HashMap<u32, u64> = [(2u32, 4_000_000u64), (1u32, 4_000_000u64)]
            .into_iter()
            .collect();
        assert_eq!(pick_attribution(None, &raws, &owned), Some(2));
        // Top not occupied → directly attribute to top (no dedup needed)
        let no_owned: HashSet<u32> = HashSet::new();
        assert_eq!(pick_attribution(None, &raws, &no_owned), Some(1));
        // Second-highest bytes too small (<20KB gate) → keep top
        let raws: HashMap<u32, u64> = [(1u32, 4_000_000u64), (2u32, 30_000u64)]
            .into_iter()
            .collect();
        assert_eq!(pick_attribution(None, &raws, &owned), Some(1));
    }

    /// Attribution dedup (occupied self-heal): when current attribution is occupied by another in-progress
    /// session, top unoccupied and bytes reach half of current attribution to switch (in occupied scenario
    /// 2x hysteresis would lock in historical misattribution); current attribution not occupied → keep
    /// 2x hysteresis original semantics
    #[test]
    fn pick_attribution_relaxed_switch_when_owned() {
        let owned: HashSet<u32> = [1u32].into_iter().collect();
        // Current attribution 1 occupied, top 2 (highest window bytes) unoccupied and 95% → switch
        // (original 2x hysteresis would lock in this kind of historical misattribution)
        let raws: HashMap<u32, u64> = [(2u32, 4_000_000u64), (1u32, 3_800_000u64)]
            .into_iter()
            .collect();
        assert_eq!(pick_attribution(Some(1), &raws, &owned), Some(2));
        // Top below half of current attribution → keep
        let raws: HashMap<u32, u64> = [(2u32, 1_900_000u64), (1u32, 4_000_000u64)]
            .into_iter()
            .collect();
        assert_eq!(pick_attribution(Some(1), &raws, &owned), Some(1));
        // Current attribution not occupied → original 2x hysteresis (top 95% below 2x, no switch)
        let no_owned: HashSet<u32> = HashSet::new();
        let raws: HashMap<u32, u64> = [(2u32, 4_000_000u64), (1u32, 3_800_000u64)]
            .into_iter()
            .collect();
        assert_eq!(pick_attribution(Some(1), &raws, &no_owned), Some(1));
        // Top also occupied (both pids owned) → nowhere to go, keep current attribution
        let both_owned: HashSet<u32> = [1u32, 2u32].into_iter().collect();
        assert_eq!(pick_attribution(Some(1), &raws, &both_owned), Some(1));
    }

    /// Calibration sample cross-process guard: other processes' bytes within window <= 20% of attributing
    /// process's to admit sample
    #[test]
    fn cross_pid_guard_thresholds() {
        // Only attributing process writing bytes → pass
        assert!(cross_pid_ok(0.0, 500_000.0));
        // Other processes exactly 20% → pass (boundary inclusive)
        assert!(cross_pid_ok(100_000.0, 500_000.0));
        // Exceeds 20% → reject (other window concurrent streaming)
        assert!(!cross_pid_ok(100_001.0, 500_000.0));
        // Attributing process zero bytes → reject (no baseline to pair)
        assert!(!cross_pid_ok(0.0, 0.0));
    }

    #[test]
    fn integrate_prorates_boundary_ticks() {
        let rows = vec![TickRow { dt_ms: 1000, bytes: 1000.0, end_ms: 10_000 }];
        // Take only the latter half of the tick [9_500, 10_000]: bytes and duration each count half
        let (b, s) = integrate(&rows, 9_500, 10_000);
        assert!((b - 500.0).abs() < 1e-9);
        assert!((s - 0.5).abs() < 1e-9);
        // Completely non-overlapping interval contributes nothing
        let (b, _) = integrate(&rows, 10_500, 11_000);
        assert_eq!(b, 0.0);
    }

    #[test]
    fn median_bpt_sliding() {
        let mut s = VecDeque::new();
        assert!((median_bpt(&mut s, 600.0, 5) - 600.0).abs() < 1e-9);
        median_bpt(&mut s, 800.0, 5);
        median_bpt(&mut s, 400.0, 5);
        assert_eq!(s.len(), 3);
        // Odd sample count takes exact middle; even takes upper median element ([400,500,600,800] → 600)
        assert!((median_bpt(&mut s, 500.0, 5) - 600.0).abs() < 1e-9);
    }

    #[test]
    fn prior_sample_prevents_single_sample_takeover() {
        // Cold start: after preset prior, a single outlier sample (e.g. bpt=179) cannot monopolize coefficient
        let mut s = VecDeque::new();
        s.push_back(DEFAULT_BPT);
        let b1 = median_bpt(&mut s, 179.0, 5);
        assert!((b1 - DEFAULT_BPT).abs() < 1e-9, "single sample should not shake prior: {b1}");
        // Two real samples start moving the median ([179, 552, 600] → 552)
        let b2 = median_bpt(&mut s, 552.0, 5);
        assert!((b2 - 552.0).abs() < 1e-9);
    }

    #[test]
    fn awaiting_hint_window() {
        // Gate open, first byte not arrived: within hint window (20s) is true
        assert!(awaiting_hint(Some(1_000), None, 5_000));
        assert!(awaiting_hint(Some(1_000), None, 21_000));
        // Past window → no longer hint (upper layer falls back to estimate: silent-pipe call)
        assert!(!awaiting_hint(Some(1_000), None, 21_001));
        // Anchor already established → not startup phase
        assert!(!awaiting_hint(Some(1_000), Some(2_000), 5_000));
        // No in-progress call → no hint
        assert!(!awaiting_hint(None, None, 5_000));
        assert!(!awaiting_hint(None, Some(1_000), 5_000));
    }

    /// Stop-detection fallback: gate open (completed not flushed) but cleaned flow ceased past grace
    /// after anchor → stop detected. Field instance (2026-09-17 log): call stopped, before completed
    /// flush window fallback kept hanging "generating + ≈last round speed", user perception
    /// "stopped but still generating, slowly declining"
    #[test]
    fn stale_stop_after_silent_window() {
        // Anchor established, last stream time within grace → still streaming
        assert!(!stale_stop(Some(1_000), Some(16_000), 16_000));
        // Ceased exactly 15s → not exceeded (> judgment), still streaming
        assert!(!stale_stop(Some(1_000), Some(1_000), 16_000));
        // Ceased past 15s → stop detected
        assert!(stale_stop(Some(1_000), Some(1_000), 16_001));
        // Anchor not established (silent-pipe call, no bytes throughout) → never stop-detected, served by window fallback
        assert!(!stale_stop(None, None, 100_000));
        assert!(!stale_stop(None, Some(1_000), 100_000));
        // Anchor exists but stream time never recorded (theoretically unreachable: anchor established means stream) → no stop
        assert!(!stale_stop(Some(1_000), None, 100_000));
    }

    /// Per-round speed drift: previous round mean vs preceding 5 consecutive rounds mean, bidirectional
    /// >= 3x triggers recalibration
    #[test]
    fn round_drift_triggers_on_threefold_jump() {
        let mut d = RoundDrift::new();
        for v in [40.0, 42.0, 38.0, 41.0, 39.0] {
            assert!(d.observe(v).is_none(), "baseline accumulation phase should not trigger");
        }
        // Previous round 120 = exactly 3x baseline mean 40 → trigger, returns baseline for logging
        let base = d.observe(120.0).expect("3x upward jump should trigger");
        assert!((base - 40.0).abs() < 1e-9);
        // After trigger history cleared: same magnitude next round does not trigger
        assert!(d.observe(120.0).is_none());
    }

    #[test]
    fn round_drift_needs_five_round_history() {
        let mut d = RoundDrift::new();
        for _ in 0..4 {
            assert!(d.observe(40.0).is_none());
        }
        // History fewer than 5 rounds: even extreme upward jump does not trigger; round enters history normally
        assert!(d.observe(4_000.0).is_none());
        // After reaching 5 rounds, mixed baseline (4×40 + 4000 = 832) vs old magnitude 40 still differs > 3x
        assert!(d.observe(40.0).is_some());
    }

    /// Reverse (looking back after switching to faster model, or fast→slow): baseline 90 vs previous
    /// round 30 = 1/3 → also triggers
    #[test]
    fn round_drift_downward_jump_triggers() {
        let mut d = RoundDrift::new();
        for _ in 0..5 {
            assert!(d.observe(90.0).is_none());
        }
        assert!(d.observe(30.0).is_some());
    }

    /// Normal fluctuation within 2.5x does not trigger; sliding window keeps only latest 5 rounds,
    /// old magnitude naturally pushed out
    #[test]
    fn round_drift_moderate_change_and_window_cap() {
        let mut d = RoundDrift::new();
        for _ in 0..5 {
            assert!(d.observe(40.0).is_none());
        }
        assert!(d.observe(100.0).is_none(), "2.5x upward jump should not trigger");
        for _ in 0..5 {
            assert!(d.observe(100.0).is_none());
        }
        // Baseline now all 100, jump back to 40 exactly 2.5x → no trigger
        assert!(d.observe(40.0).is_none());
    }

    /// Silent rounds with no measured ticks (mean 0) do not participate in detection, do not pollute baseline
    #[test]
    fn round_drift_ignores_zero_round() {
        let mut d = RoundDrift::new();
        for v in [50.0, 0.0, 50.0, 0.0, 50.0, 0.0, 50.0] {
            assert!(d.observe(v).is_none());
        }
        // 4 instances of 50 enter history (0 all ignored), 5th 50 fills baseline, no trigger
        assert!(d.observe(50.0).is_none());
        assert!(d.observe(200.0).is_some());
    }

    /// Recalibration: coefficient and sample queue return to platform prior (cold-start state)
    #[test]
    fn reset_calibration_restores_prior() {
        let mut io = LiveIo::new();
        io.cal.clear();
        io.cal.extend([420.0, 380.0, 455.0]);
        io.bytes_per_token = 420.0;
        let bpt = io.reset_calibration();
        assert!((bpt - io.params.default_bpt).abs() < 1e-9);
        assert_eq!(io.cal.len(), 1, "queue should only contain prior placeholder");
        assert!((io.bytes_per_token - io.params.default_bpt).abs() < 1e-9);
    }

    /// Restore from persistence: out-of-range / non-finite values rejected, active coefficient recomputed
    /// as upper median of restored queue (same spec as calibration path); empty restore leaves prior unchanged
    #[test]
    fn restore_cal_filters_and_recomputes_median() {
        let mut io = LiveIo::new();
        // 30 below lower bound, 99999 above upper bound (above both platforms' CAL_MAX upper bound),
        // NaN non-finite → rejected; 500/540 enter queue → [prior,500,540]
        let n = io.restore_cal(vec![500.0, 540.0, 30.0, 99_999.0, f64::NAN]);
        assert_eq!(n, 2);
        assert!((io.bytes_per_token() - 540.0).abs() < 1e-9);
        // Empty restore leaves state unchanged
        assert_eq!(io.restore_cal(vec![]), 0);
        assert!((io.bytes_per_token() - 540.0).abs() < 1e-9);
        // Capacity push-out: inject 5 more valid values, queue keeps latest 5 (600/500/540 pushed out)
        let q0 = io.cal_state();
        assert_eq!(q0.len(), 3);
        io.restore_cal(vec![450.0, 460.0, 470.0, 480.0, 490.0]);
        assert_eq!(io.cal_state().len(), CAL_QUEUE_CAP);
        assert!(!io.cal_state().contains(&io.params.default_bpt));
        // [450,460,470,480,490] upper median = 470
        assert!((io.bytes_per_token() - 470.0).abs() < 1e-9);
    }

    /// Sample admission cases taken from real debug logs (2026-09-17 field):
    /// silent-pipe calls produce ~7 B/token garbage samples, must be rejected entirely not clamped then queued
    #[test]
    fn cal_sample_rejects_silent_pipe_calls() {
        // bpt_now passes Windows prior (outlier rejection disabled on that platform, value doesn't affect result)
        // Silent call: 887 tokens, pipe integral only 5.9KB, raw bytes ~1MB → reject
        let (ok, _) = cal_sample(887, 5_939.0, 1_048_576.0, DEFAULT_BPT, &CleanParams::windows());
        assert!(!ok);
        // Normal call: 1028 tokens, cleaned 526.5KB / raw ~900KB → accept, sample ≈524
        let (ok, v) = cal_sample(
            1028,
            526.5 * 1024.0,
            900.0 * 1024.0,
            DEFAULT_BPT,
            &CleanParams::windows(),
        );
        assert!(ok);
        assert!((v - 524.0).abs() < 15.0);
        // Small call: 64 tokens → reject
        assert!(!cal_sample(64, 50_000.0, 80_000.0, DEFAULT_BPT, &CleanParams::windows()).0);
        // Lean but real: 449 tokens, cleaned 67.5KB (ratio ~150 B/token, 52% of raw) → accept
        let (ok, v) = cal_sample(
            449,
            67.5 * 1024.0,
            130.0 * 1024.0,
            DEFAULT_BPT,
            &CleanParams::windows(),
        );
        assert!(ok);
        assert!((v - 150.0).abs() < 5.0);
        // Out-of-range ratio (>6000) → reject
        assert!(!cal_sample(
            500,
            500.0 * 6000.0 * 1.1,
            500.0 * 6000.0 * 1.2,
            DEFAULT_BPT,
            &CleanParams::windows()
        )
        .0);
    }

    /// mac parameter literals (kept identical to `CleanParams::macos()`; literal construction ensures
    /// tests compile and run on Windows too)
    fn mac_params() -> CleanParams {
        CleanParams {
            burst_tick_bytes: u64::MAX as f64,
            base_noise_bps: 0.0,
            floor_cap_bytes: FLOOR_CAP_BYTES,
            cal_min: CAL_MIN,
            cal_max: 12_000.0,
            cal_min_tokens: CAL_MIN_TOKENS,
            default_bpt: 700.0,
            detect_ms: DETECT_MS,
            cal_grace_ms: 15_000,
            cal_outlier_ratio: 3.0,
        }
    }

    /// mac outlier rejection (2026-09-17 reconciliation measured): half-complete sample from delayed
    /// disk write (34s call only integrated half the bytes → 186 B/token) deviates from active coefficient
    /// 700 by more than 3x boundary → rejected, does not enter median; normal samples (true value 614/724
    /// range) accepted normally
    #[test]
    fn mac_outlier_sample_rejected() {
        let mac = mac_params();
        // Normal sample ≈650 B/token, within [700/3, 700×3] → accept
        let (ok, v) = cal_sample(1_000, 650_000.0, 900_000.0, 700.0, &mac);
        assert!(ok);
        assert!((v - 650.0).abs() < 1e-6);
        // Half-complete sample 186 (>cal_min=100, clean/raw=62%, all existing checks pass):
        // 186 < 700/3≈233 → outlier rejected, sample recorded as 0
        let (ok, v) = cal_sample(1_000, 186_000.0, 300_000.0, 700.0, &mac);
        assert!(!ok);
        assert_eq!(v, 0.0);
        // High outlier: 2500 > 700×3=2100 (still within cal_max=12000) → reject
        assert!(!cal_sample(1_000, 2_500_000.0, 3_000_000.0, 700.0, &mac).0);
        // Same half-complete sample on Windows (ratio=0 disabled) not rejected, equivalent to existing behavior
        assert!(cal_sample(1_000, 186_000.0, 300_000.0, DEFAULT_BPT, &CleanParams::windows()).0);
    }

    /// mac delayed disk-write grace: disk write bytes are page-cache asynchronous writeback count,
    /// counted seconds to tens of seconds after write() (measured 117s long call: 96% of bytes landed
    /// after completed, user stared at 0.7 t/s for two minutes while true value was 65.3). grace=15s
    /// extends calibration integral window to completed+15s, lagging bytes enter numerator, sample
    /// recovers true value; Windows spec's [.., completed] window loses almost everything
    #[test]
    fn mac_grace_window_captures_delayed_disk_writes() {
        const TRUE_TPS: f64 = 50.0;
        const BPT_TRUE: f64 = 3_900.0; // mac streaming pipe byte density
        let gen_ms = 30_000i64;
        let n = (gen_ms / TICK) as usize; // 43 ticks
        let total = TRUE_TPS * BPT_TRUE * (gen_ms as f64 / 1000.0); // 5.85MB
        let t0 = 1_000_000i64;
        // During call disk count almost motionless; dirty pages writeback in 4 concentrated ticks ~7~9.8s after completed
        let mut deltas = vec![0.0; n];
        deltas.extend(std::iter::repeat(0.0).take(10)); // ~7s silent after completion
        let chunk = total / 4.0;
        deltas.extend(std::iter::repeat(chunk).take(4));
        let samples = series(t0, &deltas);
        // mac does no files deduction (tracked_files_total always 0)
        let files = samples.iter().map(|(t, _)| (*t, 0u64)).collect::<Vec<_>>();
        let rows = merge_streams(&[&build_rows(&samples, 0.0, &mac_params())], &files);
        let call_end = t0 + (n as i64) * TICK;
        let stream_start = call_end - gen_ms;
        let eff = (TRUE_TPS * (gen_ms as f64 / 1000.0)) as u64; // 1500 tok

        // Windows spec [.., completed]: bytes not yet on disk, almost entirely lost
        let (no_grace, _) = integrate(&rows, stream_start, call_end);
        assert!(
            no_grace < total * 0.5,
            "window without grace should not see most bytes: {no_grace}"
        );
        // grace window [.., completed+15s]: all lagging disk-write bytes counted, sample recovers true value
        let (clean, _) = integrate(&rows, stream_start, call_end + mac_params().cal_grace_ms);
        let bpt = clean / eff as f64;
        assert!(
            (bpt - BPT_TRUE).abs() / BPT_TRUE < 0.05,
            "grace window sample {bpt:.0} should be close to true value {BPT_TRUE:.0}"
        );
    }

    /// Synthetic end-to-end: drives cleaning/integration/calibration per measure()'s spec,
    /// asserts "display value after calibration ≈ true value".
    ///
    /// Scenario aligned to measured field: 30s call, true value 50 t/s, UI pipe 600 B/token,
    /// disk mirror 50% of streaming bytes, heartbeat noise, poisoned adaptive floor, first-tick request burst.
    #[test]
    fn synthetic_call_converges_to_true_tps() {
        const TRUE_TPS: f64 = 50.0;
        const BPT_TRUE: f64 = 600.0;
        const FLUSH_RATIO: f64 = 0.5; // disk mirrors half of streaming bytes
        const NOISE: f64 = 1_500.0; // heartbeat/log noise (folded into write bytes)

        let gen_ms = 30_000i64;
        let n = (gen_ms / TICK) as usize; // 43 ticks
        let stream_per_tick = TRUE_TPS * BPT_TRUE * (TICK as f64 / 1000.0); // 21_000 B
        let mut deltas = Vec::with_capacity(n + 1);
        deltas.push(190_000.0); // Request body upload (first-tick burst, should be dropped entirely)
        for _ in 0..n {
            deltas.push(stream_per_tick + NOISE);
        }
        let t0 = 1_000_000i64;
        let samples = series(t0, &deltas);
        // File growth: visible same tick as writes
        let mut files = vec![(samples[0].0, 0u64)];
        for (i, d) in deltas.iter().enumerate() {
            let prev = files[i].1;
            files.push((samples[i + 1].0, prev + (*d * FLUSH_RATIO) as u64));
        }

        let rows = merge_streams(
            &[&build_rows(&samples, 40_000.0 /* poisoned floor */, &CleanParams::windows())],
            &files,
        );
        let call_end = t0 + (deltas.len() as i64) * TICK;
        let stream_start = call_end - gen_ms;

        // Consistency calibration: numerator = integral of the same cleaned stream over the call interval
        let (clean, cov_s) = integrate(&rows, stream_start, call_end);
        let eff = (TRUE_TPS * (gen_ms as f64 / 1000.0)) as u64; // 1500 tok
        let bpt = (clean / eff as f64).clamp(CAL_MIN, CAL_MAX);
        assert!(
            cov_s > (gen_ms as f64 / 1000.0) * 0.9,
            "integral should cover call interval: {cov_s}"
        );

        // Steady-state display: 30s sliding window full
        let (wb, ws) = integrate(&rows, call_end - WINDOW_MS, call_end);
        let pipe_bps = (wb / ws).max(0.0);
        let shown = pipe_bps / bpt;
        assert!(
            (shown - TRUE_TPS).abs() / TRUE_TPS < 0.05,
            "post-calibration display {shown:.1} should be close to true value {TRUE_TPS}"
        );
    }

    /// Disk flush delayed into one large block (worst misalignment): positive and negative ticks cancel
    /// over interval total, consistency calibration still converges to true value
    #[test]
    fn delayed_flush_still_converges() {
        const TRUE_TPS: f64 = 50.0;
        const BPT_TRUE: f64 = 600.0;
        let gen_ms = 30_000i64;
        let n = (gen_ms / TICK) as usize;
        let stream_per_tick = TRUE_TPS * BPT_TRUE * (TICK as f64 / 1000.0);
        let deltas: Vec<f64> = std::iter::once(190_000.0)
            .chain(std::iter::repeat(stream_per_tick).take(n))
            .collect();
        let t0 = 1_000_000i64;
        let samples = series(t0, &deltas);
        // All disk-write bytes delayed to last tick visible at once (single large flush)
        let total_flush: u64 = (deltas.iter().sum::<f64>() * 0.5) as u64;
        let mut files = Vec::with_capacity(deltas.len() + 1);
        for (i, (t, _)) in samples.iter().enumerate() {
            let v = if i == samples.len() - 1 { total_flush } else { 0 };
            files.push((*t, v));
        }
        let rows = merge_streams(&[&build_rows(&samples, 0.0, &CleanParams::windows())], &files);
        let call_end = t0 + (deltas.len() as i64) * TICK;
        let (clean, _) = integrate(&rows, call_end - gen_ms, call_end);
        let eff = (TRUE_TPS * (gen_ms as f64 / 1000.0)) as u64;
        let bpt = (clean / eff as f64).clamp(CAL_MIN, CAL_MAX);
        let (wb, ws) = integrate(&rows, call_end - WINDOW_MS, call_end);
        let shown = ((wb / ws).max(0.0)) / bpt;
        assert!(
            (shown - TRUE_TPS).abs() / TRUE_TPS < 0.05,
            "delayed flush scenario display {shown:.1} should be close to true value {TRUE_TPS}"
        );
    }
}
