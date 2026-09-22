#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod autostart;
mod liveio;
mod metrics;
mod netio;
mod snapshot_guard;
mod updater;

use liveio::{LiveIo, RoundDrift};
use metrics::{home_dir, Engine, ModelStatsPayload, Snapshot};
use updater::Release;
use std::fs;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::Duration;
use tauri::menu::{Menu, MenuItem, PredefinedMenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::{AppHandle, Emitter, LogicalSize, Manager, PhysicalPosition, PhysicalSize, WindowEvent};

/// Window display mode: full panel / floating window
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Mode {
    Full,
    Float,
}

impl Mode {
    fn as_str(self) -> &'static str {
        match self {
            Mode::Full => "full",
            Mode::Float => "float",
        }
    }
    fn parse(s: &str) -> Mode {
        if s.trim() == "float" {
            Mode::Float
        } else {
            Mode::Full
        }
    }
}

/// Floating window style: mini gauge / speed pill / desktop pet
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum FloatStyle {
    Gauge,
    Pill,
    Pet,
}

impl FloatStyle {
    fn as_str(self) -> &'static str {
        match self {
            FloatStyle::Gauge => "gauge",
            FloatStyle::Pill => "pill",
            FloatStyle::Pet => "pet",
        }
    }
    fn parse(s: &str) -> FloatStyle {
        match s.trim() {
            "pill" => FloatStyle::Pill,
            "pet" => FloatStyle::Pet,
            _ => FloatStyle::Gauge,
        }
    }
}

/// Persisted state: mode, style, and the window position / pet size each mode
/// remembers on its own. Pet position and full-panel position are independent —
/// collapsing to the pet returns the pet to its own last position (anchored at
/// the window center when there is no memory, not the window's top-left corner),
/// and expanding returns the window to its own old position.
#[derive(serde::Serialize, serde::Deserialize, Default, Clone)]
struct Persisted {
    mode: String,
    style: String,
    /// Full panel's last position (physical pixels)
    #[serde(default)]
    full_pos: Option<(i32, i32)>,
    /// Floating window's last position (physical pixels)
    #[serde(default)]
    float_pos: Option<(i32, i32)>,
    /// Desktop pet floating window side length (logical pixels)
    #[serde(default)]
    pet_size: Option<f64>,
}

struct AppState {
    engine: Mutex<Engine>,
    mode: Mutex<Mode>,
    style: Mutex<FloatStyle>,
    live: Mutex<LiveIo>,
    /// Network traffic monitoring (netio.rs: whole-machine interface counters +
    /// connection attribution + snapshot upload evidence)
    net: Mutex<netio::NetIo>,
    /// Snapshot guard (snapshot_guard.rs: directory write lock via mac chflags /
    /// win icacls deny ACE, refreshed by the poller every tick)
    guard: Mutex<snapshot_guard::SnapshotGuard>,
    debug: Mutex<DebugLog>,
    persist: Mutex<Persisted>,
    /// Position persistence throttling (at most once per 2s while dragging,
    /// immediate on close/exit)
    last_pos_save: Mutex<Option<std::time::Instant>>,
    /// Status item at the top of the tray menu (disabled, display-only
    /// generation status)
    tray_status: Mutex<Option<tauri::menu::MenuItem<tauri::Wry>>>,
    /// Last status text written to the status item / tray tooltip (update only
    /// on change to avoid per-tick churn)
    tray_status_last: Mutex<String>,
    /// Whether the mac onboarding hint is pending pickup (one-time): setup runs
    /// before the event loop, so an emit there would necessarily be dropped
    /// before the page loads; the frontend invokes to claim it once ready
    tray_hint_pending: Mutex<bool>,
    /// macOS borderless multi-monitor safe-maximize memory: (restore physical
    /// position, restore physical size)
    saved_max_rect: Mutex<Option<(PhysicalPosition<i32>, PhysicalSize<u32>)>>,
    /// Current round (a contiguous "in-flight" gated segment) displayed-speed
    /// accumulation: (Σtps, measured ticks, whether a multi-process aggregated
    /// tick was seen)
    round_tps: Mutex<(f64, u32, bool)>,
    /// Whether the previous tick had in-flight calls (true→false edge = round
    /// end; settle the average and feed drift detection)
    round_was_inflight: Mutex<bool>,
    /// Round-average speed drift detection: last round's average vs the average
    /// of the previous 5 consecutive rounds ≥3x (both directions) → auto
    /// recalibration
    drift: Mutex<RoundDrift>,
    /// Last persisted coefficient sample queue (write speed-panel-cal.json only
    /// on change)
    cal_saved: Mutex<Vec<f64>>,
    /// Debounce counter for the pet's multi-task extra height (+1 with ≥2
    /// tasks / -1 with <2 tasks, confirmed after 3 ticks)
    pet_task_streak: Mutex<u32>,
    /// Multi-task extra height the pet window should currently have (0 or
    /// PET_TASK_EXTRA; the diff vs the actual window size is corrected by the
    /// poller every tick, and also catches up after mode/style switches)
    pet_task_extra: Mutex<f64>,
    /// In-app updates (updater.rs): latest Release, pre-downloaded artifact,
    /// and concurrency gate flags. All network operations run on background
    /// threads; failures on the automatic check path are always silent (see
    /// the updater.rs module comment)
    update: Mutex<UpdateMem>,
}

/// In-memory state for the update flow (not persisted: a check runs on every
/// launch, so there is no need to remember check times across launches)
#[derive(Default)]
struct UpdateMem {
    /// Time of the last successful Release lookup (network failures are not
    /// recorded, so the next hour still retries)
    last_check_ms: i64,
    checking: bool,
    downloading: bool,
    /// Newly found version (Some means an update is available)
    latest: Option<Release>,
    /// Pre-downloaded installer (tag, path)
    downloaded: Option<(String, PathBuf)>,
    /// Start installing automatically once the download completes (the user
    /// already clicked "Update Now", waiting for the download to arrive)
    install_when_ready: bool,
}

/// Debug log: records real-time displayed values, statistics, and the ground
/// truth after each completed call round, for "live reading vs persisted
/// stats" deviation analysis. Append-write JSONL, rotates when over the limit
/// keeping one generation; rotated old files older than 7 days are cleaned up
/// at startup.
struct DebugLog {
    file: Option<fs::File>,
    written: u64,
    last_heartbeat: std::time::Instant,
}

const DEBUG_LOG_MAX: u64 = 8 * 1024 * 1024;
/// How long rotated old logs are kept
const DEBUG_LOG_KEEP: std::time::Duration = std::time::Duration::from_secs(7 * 86400);

impl DebugLog {
    fn new() -> Self {
        let mut log = DebugLog { file: None, written: 0, last_heartbeat: std::time::Instant::now() };
        log.cleanup_rotated();
        log.reopen();
        log
    }

    fn path() -> Option<PathBuf> {
        home_dir().map(|h| h.join(".zcode").join("speed-panel-debug.jsonl"))
    }

    /// Automatic cleanup: delete rotated logs past the retention period
    /// (speed-panel-debug.jsonl.N)
    fn cleanup_rotated(&mut self) {
        let Some(p) = DebugLog::path() else { return };
        let Some(dir) = p.parent() else { return };
        let Ok(rd) = fs::read_dir(dir) else { return };
        for e in rd.flatten() {
            if !e.file_name().to_string_lossy().starts_with("speed-panel-debug.jsonl.") {
                continue;
            }
            let Ok(meta) = e.metadata() else { continue };
            if let Ok(mtime) = meta.modified() {
                if mtime < std::time::SystemTime::now() - DEBUG_LOG_KEEP {
                    let _ = fs::remove_file(e.path());
                }
            }
        }
    }

    fn reopen(&mut self) {
        if let Some(p) = DebugLog::path() {
            if let Ok(meta) = fs::metadata(&p) {
                self.written = meta.len();
            }
            self.file = fs::OpenOptions::new().create(true).append(true).open(&p).ok();
        }
    }

    fn write(&mut self, value: serde_json::Value) {
        use std::io::Write;
        if self.written > DEBUG_LOG_MAX {
            self.file = None;
            if let Some(p) = DebugLog::path() {
                let _ = fs::rename(&p, p.with_extension("jsonl.1"));
            }
            self.written = 0;
            self.reopen();
        }
        if let Some(f) = self.file.as_mut() {
            let _ = writeln!(f, "{}", value);
            self.written += value.to_string().len() as u64 + 1;
        }
    }
}

/// Full panel default size (logical pixels): height 800 lets all cards
/// (including the bottom chart card) be fully visible without scrolling on
/// open (natural content height ~760; network card ~220 with the snapshot
/// record list fixed at 5 rows)
const FULL_SIZE: (f64, f64) = (1000.0, 800.0);
/// Gauge floating window 148×118: height 118 keeps the main ring's arc bottom
/// 8px from the window's bottom edge, symmetric with the top:8px of the
/// "last round" mini ring in the top-right corner (main ring canvas is 116px
/// wide, radius derived from the canvas, arc bottom anchored in gauges.ts
/// MiniGauge — keep style.css #mini-gauge and the docs in sync when changing)
const FLOAT_GAUGE_SIZE: (f64, f64) = (148.0, 118.0);
const FLOAT_PILL_SIZE: (f64, f64) = (172.0, 72.0);
/// Desktop pet default side length (logical pixels), scroll-wheel zoom range
/// [100, 480]
const FLOAT_PET_SIZE: f64 = 200.0;
const PET_SIZE_MIN: f64 = 100.0;
const PET_SIZE_MAX: f64 = 480.0;
/// Reserved height for bubbles at the top of the pet window (logical pixels):
/// two bubble lines max out at ~51px (10 + 18×2 + 3 at fs=15) + margin.
/// Window = side × (side + reserve); the bubble's bottom edge is anchored
/// near the sprite's head and grows upward, so the sprite no longer shrinks
/// to make room for bubbles (pet.ts lays out against the bottom square area;
/// changing this value requires syncing both set_size call sites and the
/// pet.ts layout logic)
const PET_BUBBLE_RESERVE: f64 = 56.0;
/// Pet multi-task extra height (logical pixels): with ≥2 in-flight tasks
/// (debounced over 3 consecutive ticks) the window grows upward by this much
/// to make room for the bubbles' per-task lines (bottom edge stays put: shift
/// up by however much it grows). 96px fits 6 bubble lines at the default size
/// of 200 (live + 6 tasks + last round). Falling back is debounced the same
/// way
const PET_TASK_EXTRA: f64 = 96.0;

fn mode_file() -> Option<PathBuf> {
    home_dir().map(|h| h.join(".zcode").join("speed-panel-mode.txt"))
}

/// Coefficient sample persistence: warm-start after restart instead of
/// re-converging from the 600 prior every time (measured: with a high-speed
/// session's true coefficient ~160, a cold start under-reads by 2~3x and
/// takes ~25 minutes to converge)
fn cal_file() -> Option<PathBuf> {
    home_dir().map(|h| h.join(".zcode").join("speed-panel-cal.json"))
}

/// Restoration validity period: old samples become stale noise after a
/// model/tokenizer generation change; past the expiry, fall back to the prior
/// and re-converge
const CAL_STALE_MS: i64 = 14 * 24 * 3600 * 1000;

fn load_cal_samples() -> Vec<f64> {
    let raw = cal_file().and_then(|p| fs::read_to_string(p).ok());
    let Some(s) = raw else { return Vec::new() };
    let Ok(v) = serde_json::from_str::<serde_json::Value>(&s) else {
        eprintln!("[zcode-speed-panel] cal sample file corrupted, falling back to prior");
        return Vec::new();
    };
    let updated = v.get("updated_ms").and_then(|x| x.as_i64()).unwrap_or(0);
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);
    if now_ms - updated > CAL_STALE_MS {
        eprintln!("[zcode-speed-panel] cal samples expired after 14 days, falling back to prior");
        return Vec::new();
    }
    v.get("samples")
        .and_then(|x| x.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|x| x.as_f64())
                .collect::<Vec<f64>>()
        })
        .unwrap_or_default()
}

fn save_cal_samples(samples: &[f64]) {
    if let Some(path) = cal_file() {
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0);
        let json = serde_json::json!({ "updated_ms": now_ms, "samples": samples });
        if let Err(e) = fs::write(path, json.to_string()) {
            eprintln!("[zcode-speed-panel] failed to persist cal samples: {e}");
        }
    }
}

fn load_persisted() -> Persisted {
    let raw = mode_file().and_then(|p| fs::read_to_string(p).ok());
    match raw {
        Some(s) => match serde_json::from_str::<Persisted>(&s) {
            Ok(p) => p,
            // Legacy format: plain text "full"/"float"
            Err(_) => Persisted {
                mode: s,
                ..Default::default()
            },
        },
        None => Persisted::default(),
    }
}

fn save_all(app: &AppHandle) {
    let state = app.state::<AppState>();
    let mode = *state.mode.lock().unwrap();
    let style = *state.style.lock().unwrap();
    let p = state.persist.lock().unwrap().clone();
    // Persist the network's daily totals as well (shared by the exit /
    // position-save paths)
    state.net.lock().unwrap().save_forced();
    if let Some(path) = mode_file() {
        let json = serde_json::json!({
            "mode": mode.as_str(),
            "style": style.as_str(),
            "full_pos": p.full_pos,
            "float_pos": p.float_pos,
            "pet_size": p.pet_size,
        });
        let _ = fs::write(path, json.to_string());
    }
}

/// Pull the window fully back into the visible area of the monitor it is on
/// (with multiple monitors, locate by the window's current point)
fn clamp_to_screen(window: &tauri::WebviewWindow, x: i32, y: i32, w: u32, h: u32) -> (i32, i32) {
    let monitor = window
        .monitor_from_point(x as f64, y as f64)
        .ok()
        .flatten()
        .or_else(|| window.current_monitor().ok().flatten())
        .or_else(|| window.primary_monitor().ok().flatten());
    let Some(m) = monitor else {
        return (x, y);
    };
    let mp = m.position();
    let ms = m.size();
    let max_x = (mp.x + ms.width as i32 - w as i32).max(mp.x);
    let max_y = (mp.y + ms.height as i32 - h as i32).max(mp.y);
    (x.clamp(mp.x, max_x), y.clamp(mp.y, max_y))
}


fn apply_mode(window: &tauri::WebviewWindow, mode: Mode, style: FloatStyle, p: &Persisted, pet_extra: f64) {
    let scale = window.scale_factor().unwrap_or(1.0);
    match mode {
        Mode::Full => {
            let _ = window.set_min_size(Some(LogicalSize::new(720.0, 520.0)));
            let _ = window.set_size(LogicalSize::new(FULL_SIZE.0, FULL_SIZE.1));
            // Title bar: mac restores the native Overlay title bar — **real
            // system traffic lights** (red close / yellow minimize / green
            // fullscreen with native animation), content extends under the
            // title bar and the frontend leaves room on the left; also switch
            // to the Regular policy to show the Dock icon (only Regular apps
            // get native fullscreen, see the setup comment). Windows keeps
            // borderless + frontend-drawn — ▢ ✕ (float windows must be
            // borderless)
            #[cfg(target_os = "macos")]
            {
                let _ = window
                    .app_handle()
                    .set_activation_policy(tauri::ActivationPolicy::Regular);
                let _ = window.set_decorations(true);
            }
            #[cfg(not(target_os = "macos"))]
            let _ = window.set_decorations(false);
            let _ = window.set_resizable(true);
            let _ = window.set_always_on_top(false);
            let _ = window.set_skip_taskbar(false);
            let _ = window.set_shadow(true);
            // Return to the full panel's own old position (keep the current
            // top-left corner when there is no memory, clamped back into the
            // visible area)
            if let Some((x, y)) = p.full_pos {
                let (px, py) = clamp_to_screen(
                    window,
                    x,
                    y,
                    (FULL_SIZE.0 * scale) as u32,
                    (FULL_SIZE.1 * scale) as u32,
                );
                let _ = window.set_position(PhysicalPosition::new(px, py));
            }
        }
        Mode::Float => {
            let (w, h) = match style {
                FloatStyle::Gauge => FLOAT_GAUGE_SIZE,
                FloatStyle::Pill => FLOAT_PILL_SIZE,
                FloatStyle::Pet => {
                    let s = p.pet_size.unwrap_or(FLOAT_PET_SIZE).clamp(PET_SIZE_MIN, PET_SIZE_MAX);
                    // Top reserve band for two bubble lines: the sprite doesn't
                    // shrink, bubbles grow upward; the multi-task extra height
                    // (pet_task_extra) gives per-task lines room to grow too
                    (s, s + PET_BUBBLE_RESERVE + pet_extra)
                }
            };
            let _ = window.set_min_size(None::<LogicalSize<f64>>);
            let _ = window.set_size(LogicalSize::new(w, h));
            let _ = window.set_decorations(false);
            // mac: retract to Accessory — hide the Dock icon and stay resident
            // in the menu bar (the app doesn't quit; this and the Regular full
            // panel with a Dock icon are the two states, see the setup comment)
            #[cfg(target_os = "macos")]
            let _ = window
                .app_handle()
                .set_activation_policy(tauri::ActivationPolicy::Accessory);
            let _ = window.set_resizable(false);
            // Floating window: always on top, no taskbar entry, no native
            // shadow (the shadow would cover the transparent area outside the
            // rounded corners)
            let _ = window.set_always_on_top(true);
            let _ = window.set_skip_taskbar(true);
            let _ = window.set_shadow(false);
            // Position: the pet / floating window's own last position; when
            // there is no memory, anchor at the current window's center
            // (instead of following the top-left corner — fixing the old issue
            // where the collapsed pet always landed at the previous window's
            // top-left corner)
            let (pw, ph) = ((w * scale) as u32, (h * scale) as u32);
            let target = match p.float_pos {
                Some((x, y)) => clamp_to_screen(window, x, y, pw, ph),
                None => {
                    let cur = window.outer_position().unwrap_or_default();
                    let sz = window.outer_size().unwrap_or_default();
                    let cx = cur.x + sz.width as i32 / 2;
                    let cy = cur.y + sz.height as i32 / 2;
                    clamp_to_screen(window, cx - pw as i32 / 2, cy - ph as i32 / 2, pw, ph)
                }
            };
            let _ = window.set_position(PhysicalPosition::new(target.0, target.1));
        }
    }
}

fn switch_mode(app: &AppHandle, mode: Mode) {
    let state = app.state::<AppState>();
    let style = *state.style.lock().unwrap();
    let prev = *state.mode.lock().unwrap();
    if mode == Mode::Float {
        *state.saved_max_rect.lock().unwrap() = None;
    }
    // Remember the window position under the old mode (each mode remembers
    // independently)
    if let Some(win) = app.get_webview_window("main") {
        if let Ok(pos) = win.outer_position() {
            let mut p = state.persist.lock().unwrap();
            match prev {
                Mode::Full => p.full_pos = Some((pos.x, pos.y)),
                Mode::Float => p.float_pos = Some((pos.x, pos.y)),
            }
        }
    }
    // Update the mode before applying the new size/position: Moved events
    // triggered during the apply are written back under the new mode
    *state.mode.lock().unwrap() = mode;
    let p = state.persist.lock().unwrap().clone();
    let pet_extra = *state.pet_task_extra.lock().unwrap();
    if let Some(window) = app.get_webview_window("main") {
        apply_mode(&window, mode, style, &p, pet_extra);
    }
    save_all(app);
    let _ = app.emit("mode", mode.as_str());
}

/// Collapse to floating window: full panel → switch to float mode; already
/// floating → bring up and focus. Shared as the fallback for CloseRequested /
/// mac menu-bar Cmd+Q / ExitRequested
fn collapse_to_float(app: &AppHandle) {
    let mode = *app.state::<AppState>().mode.lock().unwrap();
    if mode == Mode::Full {
        switch_mode(app, Mode::Float);
    } else {
        show_main(app);
    }
}

#[derive(Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct SnapshotPayload {
    snapshot: Snapshot,
    rollout_dir: String,
    mode: String,
    float_style: String,
    /// Snapshot guard status (attached to every tick, rendered by the
    /// frontend card)
    guard: snapshot_guard::SnapshotGuardStatus,
}

fn build_payload(app: &AppHandle) -> SnapshotPayload {
    let state = app.state::<AppState>();
    let mode = *state.mode.lock().unwrap();
    let style = *state.style.lock().unwrap();
    let rollout_dir;
    let new_calls;
    let engine_calls: Vec<metrics::Call>;
    let inflight: Vec<(String, i64)>;
    let mut snapshot;
    {
        let mut engine = state.engine.lock().unwrap();
        new_calls = engine.poll();
        snapshot = engine.snapshot();
        rollout_dir = engine.data_source_label();
        engine_calls = engine.calls().to_vec();
        inflight = engine.call_in_flight();
    }
    // Real-time measurement: process IO write byte stream (true values). With
    // multi-task concurrency (multiple windows / subagents), aggregate over the
    // union of attributed processes of in-flight sessions; current speed =
    // true total throughput
    let now_ms = snapshot.now_ms;
    // Snapshot guard (snapshot_guard.rs): lock probing + blocked_rounds
    // incremental accumulation + throttled scanning, pushed to the frontend
    // card with the payload
    let guard_status = state.guard.lock().unwrap().tick(snapshot.calls_today, now_ms);
    // Network traffic monitoring: whole-machine interface counter diff +
    // connection attribution + checkpoint artifact evidence
    let net_now = state.net.lock().unwrap().tick(now_ms);
    let net_log_events: Vec<serde_json::Value> = state.net.lock().unwrap().take_events().into_iter().collect();
    snapshot.net_available = net_now.available;
    snapshot.net_up_bps = net_now.up_bps;
    snapshot.net_down_bps = net_now.down_bps;
    snapshot.net_up_today = net_now.up_today;
    snapshot.net_down_today = net_now.down_today;
    // Session traffic estimation (≈): the uploaded numerator uses uncached
    // prompt tokens (cache hits aren't re-sent; measured whole-machine daily
    // upload is only tens of KB), download is output tokens × SSE density
    // coefficient; the true lower bound of non-session uploads comes from
    // checkpoint artifacts
    let uncached_prompt = snapshot
        .input_tokens
        .saturating_add(snapshot.cache_creation_tokens)
        .saturating_sub(snapshot.cache_read_tokens);
    let (sess_up, sess_down) = netio::sess_bytes_est(
        uncached_prompt,
        snapshot.output_tokens + snapshot.reasoning_tokens,
    );
    snapshot.net_sess_up_today = sess_up;
    snapshot.net_sess_down_today = sess_down;
    snapshot.net_ckpt_today = net_now.ckpt_today_bytes;
    snapshot.net_ckpt_today_count = net_now.ckpt_today_count;
    snapshot.net_ckpt_today_list = net_now.ckpt_today_list.clone();
    snapshot.net_ckpt_uploading = net_now.ckpt_uploading;
    snapshot.net_ckpt_status = net_now.ckpt_status.clone();
    snapshot.net_ckpt_list = net_now.ckpt_list.clone();
    snapshot.net_conns_available = net_now.conns_available;
    snapshot.net_cli_conns = net_now.cli_conns;
    snapshot.net_app_conns = net_now.app_conns;
    snapshot.net_cli_conn_list = net_now.cli_conn_list.clone();
    snapshot.net_app_conn_list = net_now.app_conn_list.clone();
    let cal_event;
    let bpt_now;
    let pipe_bps;
    let npids;
    let proc_bps_log: Vec<(u32, f64)>;
    let infl_attr_log: Vec<(String, u32)>;
    {
        let mut live = state.live.lock().unwrap();
        if !live.history_done() {
            live.ingest_history(engine_calls.as_slice());
        }
        live.observe(&new_calls);
        live.set_inflight(inflight.clone());
        let live_now = live.measure(now_ms);
        cal_event = live.take_calibration();
        bpt_now = live.bytes_per_token();
        pipe_bps = live_now.pipe_bps;
        npids = live_now.n_pids;
        proc_bps_log = live_now.proc_bps.clone();
        infl_attr_log = live.inflight_attr();
        snapshot.tasks = live_now
            .tasks
            .iter()
            .map(|t| metrics::TaskStat {
                pid: t.pid,
                session: t.session.clone().unwrap_or_default(),
                n_sessions: t.n_sessions as u32,
                tps: t.tps,
                streaming: t.streaming,
            })
            .collect();
        // Persist as soon as the coefficient sample queue changes (new sample
        // ingested / manual or drift recalibration) for a warm restart
        {
            let q = live.cal_state();
            let mut saved = state.cal_saved.lock().unwrap();
            if *saved != q {
                save_cal_samples(&q);
                *saved = q;
            }
        }
        let ever_saw = live.ever_saw_procs();
        if live_now.available {
            if live_now.streaming {
                snapshot.is_live = true;
                snapshot.is_estimating = false;
                snapshot.ramping = live_now.ramping;
                snapshot.is_starting = live_now.awaiting;
                snapshot.live_source = "io".into();
                if live_now.awaiting {
                    // Startup phase (gate open, first bytes not yet arrived):
                    // show the "measuring…" hint instead of a misleading
                    // estimated value
                    snapshot.current_tps = 0.0;
                } else if live_now.tps < 1.0 && snapshot.window_tps > 0.0 {
                    // During some calls the UI pipeline has no incremental
                    // bytes (IO measures 0): fall back to the true speed of
                    // recently completed calls (same basis as the speed
                    // chart), marked ≈ estimated. ≈ is a global value on the
                    // persisted basis with no per-task measurement to break
                    // down, so clear the details
                    snapshot.current_tps = snapshot.window_tps;
                    snapshot.is_estimating = true;
                    snapshot.live_source = "window".into();
                    snapshot.tasks.clear();
                } else {
                    snapshot.current_tps = live_now.tps;
                }
                if let Some(last) = snapshot.spark.last_mut() {
                    *last = snapshot.current_tps;
                }
            } else {
                // IO available but the gate sees no calls → honestly idle
                // (true values first, not masked by estimates)
                snapshot.is_estimating = false;
                snapshot.ramping = false;
                snapshot.current_tps = 0.0;
                snapshot.live_source = "idle".into();
                if let Some(last) = snapshot.spark.last_mut() {
                    *last = 0.0;
                }
            }
        } else if !inflight.is_empty() && !ever_saw {
            // IO never available (IO probing environment unavailable / panel
            // just started and the process wasn't found yet): decide by the
            // message gate rather than blind estimation from call intervals —
            // only display when a call is in flight (estimate ≈ if there is
            // recent ground truth, otherwise the "measuring…" hint), and zero
            // out immediately when the gate stops. The old basis inferred from
            // the interval median and kept spinning "estimating" for up to
            // 240s after calls ended
            if !(snapshot.is_estimating && snapshot.current_tps > 0.0) {
                snapshot.is_estimating = false;
                snapshot.is_starting = true;
                snapshot.current_tps = 0.0;
                snapshot.live_source = "window".into();
                if let Some(last) = snapshot.spark.last_mut() {
                    *last = 0.0;
                }
            }
        } else {
            // Process seen before but currently unavailable (all CLIs exited),
            // or the gate has stopped → honestly idle
            snapshot.is_live = false;
            snapshot.is_estimating = false;
            snapshot.ramping = false;
            snapshot.current_tps = 0.0;
            snapshot.live_source = "idle".into();
            if let Some(last) = snapshot.spark.last_mut() {
                *last = 0.0;
            }
        }

    // ---- Debug log: real-time displayed values / statistics / ground truth after each round ----
    {
        let state = app.state::<AppState>();
        let mut log = state.debug.lock().unwrap();
        for ev in &net_log_events {
            log.write(ev.clone());
        }
        for c in &new_calls {
            log.write(serde_json::json!({
                "kind": "call",
                "t": now_ms,
                "id": c.id,
                "sess": &c.session[c.session.len().saturating_sub(8)..],
                "done": c.completed_ms,
                "gen_ms": c.gen_ms,
                "eff": c.effective_out(),
                "true_tps": (c.effective_out() as f64) / (c.gen_ms.max(50) as f64 / 1000.0),
            }));
        }
        if let Some(cal) = &cal_event {
            // Reconciliation: the cleaned stream integrated over this call's
            // interval ÷ generation seconds ÷ current coefficient = the
            // display-basis average t/s prediction for this call; comparing
            // against the persisted ground truth true_tps evaluates real-time
            // accuracy
            let pred_tps = if cal.gen_ms > 0 && cal.bpt_now > 0.0 {
                cal.clean_bytes / (cal.gen_ms as f64 / 1000.0) / cal.bpt_now
            } else {
                0.0
            };
            log.write(serde_json::json!({
                "kind": "cal",
                "t": now_ms,
                "id": cal.id,
                "gen_ms": cal.gen_ms,
                "eff": cal.eff,
                "true_tps": (cal.true_tps * 10.0).round() / 10.0,
                "raw_kb": (cal.raw_bytes / 1024.0 * 10.0).round() / 10.0,
                "clean_kb": (cal.clean_bytes / 1024.0 * 10.0).round() / 10.0,
                "attr_pid": cal.attr_pid,
                "top_pid": cal.top_pid,
                "others_kb": (cal.others_bytes / 1024.0 * 10.0).round() / 10.0,
                "bpt_sample": (cal.bpt_sample * 10.0).round() / 10.0,
                "bpt_now": (cal.bpt_now * 10.0).round() / 10.0,
                "pred_tps": (pred_tps * 10.0).round() / 10.0,
                "skipped": cal.cal_skipped,
            }));
        }
        let active = snapshot.is_live || snapshot.is_estimating || snapshot.is_starting;
        let heartbeat = log.last_heartbeat.elapsed() > std::time::Duration::from_secs(30);
        if active || heartbeat {
            log.last_heartbeat = std::time::Instant::now();
            let tail: Vec<f64> = snapshot
                .spark
                .iter()
                .rev()
                .take(3)
                .rev()
                .map(|v| (v * 10.0).round() / 10.0)
                .collect();
            // The multi-task troubleshooting trio: probe-window rates of
            // tracked processes / in-flight session count / session attribution
            // mapping
            let pids_json: serde_json::Map<String, serde_json::Value> = proc_bps_log
                .iter()
                .map(|(pid, kbps)| (pid.to_string(), serde_json::json!(kbps)))
                .collect();
            let attr_json: Vec<String> = infl_attr_log
                .iter()
                .map(|(s, pid)| format!("{}…{}", &s[s.len().saturating_sub(4)..], pid))
                .collect();
            log.write(serde_json::json!({
                "kind": "tick",
                "t": now_ms,
                "src": snapshot.live_source,
                "tps": (snapshot.current_tps * 10.0).round() / 10.0,
                "pipe": (pipe_bps / 10.0).round() * 10.0,
                "stream": snapshot.is_live,
                "ramp": snapshot.ramping,
                "start": snapshot.is_starting,
                "est": snapshot.is_estimating,
                "bpt": (bpt_now * 10.0).round() / 10.0,
                "avg": (snapshot.avg_tps * 10.0).round() / 10.0,
                "spark_tail": tail,
                "calls": snapshot.calls_today,
                "npids": npids,
                "infl": inflight.len(),
                "pids": pids_json,
                "attr": attr_json,
                "net_up": (net_now.up_bps / 1024.0 * 10.0).round() / 10.0,
                "net_dn": (net_now.down_bps / 1024.0 * 10.0).round() / 10.0,
                "cli_conn": net_now.cli_conns,
                "app_conn": net_now.app_conns,
                "ckpt_up": net_now.ckpt_uploading,
            }));
        }
    }

    }

    // ---- Round-average speed drift auto-recalibration: a round = one
    //      contiguous "in-flight" gated segment, averaging the displayed speed
    //      (io-measured ticks) within the round; last round's average vs the
    //      previous 5 consecutive rounds' average ≥3x (both directions)
    //      detects a magnitude shift (model/tokenizer change, stale
    //      coefficient) → drop coefficient samples and return to the prior.
    //      Multi-process aggregated rounds (multi-task concurrency) don't
    //      participate: throughput differences from a changing task count are
    //      not coefficient drift ----
    {
        let state = app.state::<AppState>();
        let now_inflight = !inflight.is_empty();
        let was_inflight = {
            let mut flag = state.round_was_inflight.lock().unwrap();
            std::mem::replace(&mut *flag, now_inflight)
        };
        if snapshot.is_live && snapshot.current_tps > 0.0 {
            let mut acc = state.round_tps.lock().unwrap();
            acc.0 += snapshot.current_tps;
            acc.1 += 1;
            if npids != 1 {
                acc.2 = true;
            }
        }
        if was_inflight && !now_inflight {
            // Round end: settle the average. Silent/estimated rounds (no
            // measured ticks) and multi-process aggregated rounds don't
            // participate in drift detection
            let (sum, n, saw_multi) = {
                let mut acc = state.round_tps.lock().unwrap();
                std::mem::take(&mut *acc)
            };
            if n > 0 && !saw_multi {
                let avg = sum / n as f64;
                let mut drift = state.drift.lock().unwrap();
                if let Some(base) = drift.observe(avg) {
                    let (bpt_old, bpt_new) = {
                        let mut live = state.live.lock().unwrap();
                        let old = live.bytes_per_token();
                        (old, live.reset_calibration())
                    };
                    state.debug.lock().unwrap().write(serde_json::json!({
                        "kind": "cal_reset",
                        "t": now_ms,
                        "reason": "auto",
                        "round_avg": (avg * 10.0).round() / 10.0,
                        "base_avg": (base * 10.0).round() / 10.0,
                        "bpt_old": (bpt_old * 10.0).round() / 10.0,
                        "bpt_new": (bpt_new * 10.0).round() / 10.0,
                    }));
                    // Same feedback as a manual trigger (⟳ button flashes ✓):
                    // an automatic trigger comes with a large coefficient
                    // deviation, exactly when the user needs this hint
                    let _ = app.emit("recalibrated", ());
                }
            }
        }
    }
    SnapshotPayload {
        rollout_dir,
        snapshot,
        mode: mode.as_str().to_string(),
        float_style: style.as_str().to_string(),
        guard: guard_status,
    }
}

#[tauri::command]
fn snapshot(app: AppHandle) -> SnapshotPayload {
    build_payload(&app)
}

/// Current epoch ms (for recording the snapshot guard's lock time)
fn epoch_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

// ---- Snapshot guard (snapshot_guard.rs): status is attached to the metrics
//      payload on every tick; these three commands let the frontend card
//      manually query / enable / release it (the informed-consent
//      confirmation dialogs for enable and release are in the frontend
//      #guard-confirm, see key-rules #16)----

#[tauri::command]
fn snapshot_guard_status(app: AppHandle) -> snapshot_guard::SnapshotGuardStatus {
    let state = app.state::<AppState>();
    let mut guard = state.guard.lock().unwrap();
    let calls = guard.last_calls_seen();
    guard.tick(calls, epoch_ms())
}

/// Enable the guard (the frontend has already passed the confirmation dialog;
/// keep_files = keep existing snapshots with a recursive lock / delete then
/// lock the empty directory)
#[tauri::command]
fn snapshot_guard_apply(
    app: AppHandle,
    keep_files: Option<bool>,
) -> Result<snapshot_guard::SnapshotGuardStatus, String> {
    let state = app.state::<AppState>();
    // The calls_today baseline at lock time takes the live true value (Engine
    // is a read-only aggregate, the one-time cost is acceptable)
    let calls = state.engine.lock().unwrap().snapshot().calls_today;
    let result = state
        .guard
        .lock()
        .unwrap()
        .apply(calls, epoch_ms(), keep_files.unwrap_or(false));
    result
}

/// Release the guard: recursive unlock (files untouched — in delete mode the
/// directory is already empty; in keep mode snapshots become writable again
/// in place)
#[tauri::command]
fn snapshot_guard_release(app: AppHandle) -> Result<snapshot_guard::SnapshotGuardStatus, String> {
    let state = app.state::<AppState>();
    let calls = state.engine.lock().unwrap().snapshot().calls_today;
    let result = state.guard.lock().unwrap().release(calls);
    result
}

/// Open a workspace's snapshot directory in the system file manager (the 📂 on
/// an upload record row; cross-platform: mac Finder / Windows Explorer). hash
/// is the subdirectory name under checkpoints; whitelist validation prevents
/// path traversal; a missing directory (snapshot deleted / not yet created)
/// reports an honest error
#[tauri::command]
fn open_checkpoint_dir(hash: String) -> Result<(), String> {
    if !snapshot_guard::valid_hash_name(&hash) {
        return Err("Invalid workspace directory name".into());
    }
    let dir = snapshot_guard::checkpoints_dir()
        .ok_or("Unable to locate the user directory")?
        .join(&hash);
    if !dir.is_dir() {
        return Err("The snapshot directory for this workspace does not exist (the snapshot may have been deleted or not yet created)".into());
    }
    #[cfg(target_os = "macos")]
    let st = std::process::Command::new("open").arg(&dir).spawn();
    #[cfg(target_os = "windows")]
    let st = std::process::Command::new("explorer").arg(&dir).spawn();
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        let _ = &dir;
        return Err("Only macOS / Windows are supported".into());
    }
    #[cfg(any(target_os = "macos", target_os = "windows"))]
    st.map(|_| ()).map_err(|e| format!("Failed to open directory: {e}"))
}

/// Model speed trends: read-only query aggregating the usage database by
/// model × bucket (the frontend polls every 5s while the details dialog is
/// open). The aggregation is computed on the fly inside Engine, zero local
/// storage, nothing written to the usage database
#[tauri::command]
fn model_stats(app: AppHandle, window_min: i64) -> ModelStatsPayload {
    let state = app.state::<AppState>();
    let engine = state.engine.lock().unwrap();
    engine.model_stats(window_min)
}

/// Output speed chart (time range selectable 15m/1h/6h/24h): read-only query
/// aggregating 90 buckets of tps from the usage database (all models merged,
/// old→new). The 15-minute range is on the same basis as the today spark data
/// in the metrics payload; the frontend polls every 5s on longer ranges and
/// blends the live speed into the newest bucket
#[tauri::command]
fn chart_stats(app: AppHandle, window_min: i64) -> metrics::ChartStatsPayload {
    let state = app.state::<AppState>();
    let engine = state.engine.lock().unwrap();
    engine.chart_stats(window_min)
}

#[tauri::command]
fn set_mode(app: AppHandle, mode: String, style: Option<String>) {
    if let Some(s) = style {
        let st = FloatStyle::parse(&s);
        *app.state::<AppState>().style.lock().unwrap() = st;
    }
    switch_mode(&app, Mode::parse(&mode));
}

#[tauri::command]
fn set_float_style(app: AppHandle, style: String) {
    let st = FloatStyle::parse(&style);
    {
        let state = app.state::<AppState>();
        *state.style.lock().unwrap() = st;
        let mode = *state.mode.lock().unwrap();
    if mode == Mode::Float {
        if let Some(window) = app.get_webview_window("main") {
            let p = state.persist.lock().unwrap().clone();
            let pet_extra = *state.pet_task_extra.lock().unwrap();
            apply_mode(&window, mode, st, &p, pet_extra);
        }
    }
    }
    save_all(&app);
    let _ = app.emit("float-style", st.as_str());
}

/// Pet scroll-wheel zoom: adjust the floating window's side length (logical
/// pixels) and persist it
#[tauri::command]
fn set_float_size(app: AppHandle, size: f64) {
    let size = size.clamp(PET_SIZE_MIN, PET_SIZE_MAX);
    {
        let state = app.state::<AppState>();
        state.persist.lock().unwrap().pet_size = Some(size);
        let mode = *state.mode.lock().unwrap();
        let style = *state.style.lock().unwrap();
        if mode == Mode::Float && style == FloatStyle::Pet {
            if let Some(window) = app.get_webview_window("main") {
                // Height includes the top bubble reserve band and the
                // multi-task extra height (same basis as apply_mode)
                let extra = *state.pet_task_extra.lock().unwrap();
                let _ = window.set_size(LogicalSize::new(size, size + PET_BUBBLE_RESERVE + extra));
            }
        }
    }
    save_all(&app);
}

/// Debounce and diff application for the pet's multi-task extra height: ≥2
/// in-flight tasks for 3 consecutive ticks → grow by PET_TASK_EXTRA; falling
/// back for 3 consecutive ticks → retract (the same 3-tick debounce as the
/// full panel's task card). Return immediately when want matches the applied
/// value, no window-size churn
fn update_pet_task_extra(app: &AppHandle, multi_now: bool) {
    let state = app.state::<AppState>();
    let streak = {
        let mut s = state.pet_task_streak.lock().unwrap();
        *s = if multi_now {
            (*s + 1).min(3)
        } else {
            s.saturating_sub(1)
        };
        *s
    };
    let want = if streak >= 3 { PET_TASK_EXTRA } else { 0.0 };
    let cur = *state.pet_task_extra.lock().unwrap();
    if (want - cur).abs() < f64::EPSILON {
        return;
    }
    *state.pet_task_extra.lock().unwrap() = want;
    apply_pet_size(app);
}

/// Set the window size from the current pet side length + bubble reserve band
/// + multi-task extra height, and shift the whole window up/down by the
/// height difference to keep the bottom edge (the sprite's feet) fixed on
/// screen; only takes effect in pet float mode; in other modes/styles only
/// the state value is updated, and apply_mode / this function catch up when
/// switching back
fn apply_pet_size(app: &AppHandle) {
    let state = app.state::<AppState>();
    let mode = *state.mode.lock().unwrap();
    let style = *state.style.lock().unwrap();
    if mode != Mode::Float || style != FloatStyle::Pet {
        return;
    }
    let extra = *state.pet_task_extra.lock().unwrap();
    let size = state
        .persist
        .lock()
        .unwrap()
        .pet_size
        .unwrap_or(FLOAT_PET_SIZE)
        .clamp(PET_SIZE_MIN, PET_SIZE_MAX);
    let Some(w) = app.get_webview_window("main") else {
        return;
    };
    let scale = w.scale_factor().unwrap_or(1.0);
    let Ok(outer) = w.outer_size() else {
        return;
    };
    let new_h = size + PET_BUBBLE_RESERVE + extra;
    let dy = ((new_h - outer.height as f64 / scale) * scale).round() as i32;
    let pos = w.outer_position().unwrap_or_default();
    let (nx, ny) = clamp_to_screen(&w, pos.x, pos.y - dy, outer.width, (new_h * scale) as u32);
    let _ = w.set_size(LogicalSize::new(size, new_h));
    let _ = w.set_position(PhysicalPosition::new(nx, ny));
}

/// Float window right-click menu "Quit": save state then exit the app
#[tauri::command]
fn quit_app(app: AppHandle) {
    save_all(&app);
    app.exit(0);
}

/// Manual recalibration (the ⟳ button at the top-left of the full panel's
/// current-speed card): discard the learned coefficient samples and return to
/// the platform prior, re-converging via subsequent calls; drift detection
/// history resets in sync
#[tauri::command]
fn recalibrate(app: AppHandle) {
    let state = app.state::<AppState>();
    let (bpt_old, bpt_new) = {
        let mut live = state.live.lock().unwrap();
        let old = live.bytes_per_token();
        (old, live.reset_calibration())
    };
    state.drift.lock().unwrap().reset();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);
    state.debug.lock().unwrap().write(serde_json::json!({
        "kind": "cal_reset",
        "t": now,
        "reason": "manual",
        "bpt_old": (bpt_old * 10.0).round() / 10.0,
        "bpt_new": (bpt_new * 10.0).round() / 10.0,
    }));
    let _ = app.emit("recalibrated", ());
}

/// mac onboarding hint (one-time): claimed by an explicit frontend invoke
/// once the page is ready — an emit inside setup would necessarily be dropped
/// before the page loads, so the frontend invokes to claim it instead. Always
/// returns false on non-mac
#[tauri::command]
fn tray_hint_once(app: AppHandle) -> bool {
    let state = app.state::<AppState>();
    let mut guard = state.tray_hint_pending.lock().unwrap();
    let pending = *guard;
    *guard = false;
    pending
}

fn toggle_window_maximize(window: &tauri::WebviewWindow) {
    if window.is_maximized().unwrap_or(false) {
        let _ = window.unmaximize();
    } else {
        let _ = window.maximize();
    }
}

/// Multi-monitor safe maximize/restore: macOS borderless windows' native
/// toggle_maximize jumps back to the main screen, so here we fill the monitor
/// containing the window's center point (avoiding the menu bar); Windows just
/// calls the system maximize
#[tauri::command]
fn toggle_maximize_safe(window: tauri::WebviewWindow, state: tauri::State<'_, AppState>) {
    #[cfg(windows)]
    {
        let _ = &state; // state is only used in the mac branch; silence the unused warning on Windows
        toggle_window_maximize(&window);
    }
    #[cfg(target_os = "macos")]
    {
        let mut saved = state.saved_max_rect.lock().unwrap();
        if let Some((pos, size)) = saved.take() {
            // Already maximized, restore
            let _ = window.set_size(size);
            let _ = window.set_position(pos);
        } else {
            // Not maximized, do the safe maximize
            let cur_pos = window.outer_position().unwrap_or_default();
            let cur_size = window.outer_size().unwrap_or_default();
            *saved = Some((cur_pos, cur_size));

            let cx = cur_pos.x + cur_size.width as i32 / 2;
            let cy = cur_pos.y + cur_size.height as i32 / 2;
            let monitor = window
                .monitor_from_point(cx as f64, cy as f64)
                .ok()
                .flatten()
                .or_else(|| window.current_monitor().ok().flatten())
                .or_else(|| window.primary_monitor().ok().flatten());

            if let Some(m) = monitor {
                let scale = m.scale_factor();
                let mp = m.position();
                let ms = m.size();
                // Avoid the macOS top menu bar, roughly 28pt tall
                let top_margin = (28.0 * scale) as i32;
                let target_x = mp.x;
                let target_y = mp.y + top_margin;
                let target_w = ms.width;
                let target_h = ms.height.saturating_sub(top_margin as u32);

                let _ = window.set_position(PhysicalPosition::new(target_x, target_y));
                let _ = window.set_size(PhysicalSize::new(target_w, target_h));
            } else {
                toggle_window_maximize(&window);
            }
        }
    }
    #[cfg(not(any(windows, target_os = "macos")))]
    {
        toggle_window_maximize(&window);
    }
}

// ---- In-app updates (updater.rs): check / pre-download / install orchestration, event-driven frontend card ----

/// Frontend "update" event payload: a flat structure branched by state
/// (available / downloading / ready / launching / error); unused fields are
/// left empty
#[derive(Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct UpdateEvent {
    state: &'static str,
    current_version: String,
    new_version: String,
    release_url: String,
    notes: String,
    downloaded_bytes: u64,
    total_bytes: u64,
    message: String,
}

/// Synchronous return of a manual check (check_update command): the frontend
/// shows a light toast from this
#[derive(Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
#[serde(tag = "kind")]
enum CheckOutcome {
    UpToDate { current: String },
    Available { current: String, new_version: String },
    Failed { message: String },
}

fn current_version(app: &AppHandle) -> String {
    app.package_info().version.to_string()
}

/// Release notes truncation (by character count, so an overly long body
/// doesn't blow up the frontend card; the frontend also has max-height)
fn truncate_chars(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        format!("{}…", s.chars().take(n).collect::<String>())
    }
}

fn update_event(state: &'static str, current: &str, rel: Option<&Release>) -> UpdateEvent {
    UpdateEvent {
        state,
        current_version: current.to_string(),
        new_version: rel.map(|r| r.version.clone()).unwrap_or_default(),
        release_url: rel.map(|r| r.url.clone()).unwrap_or_default(),
        notes: rel.map(|r| truncate_chars(&r.notes, 400)).unwrap_or_default(),
        downloaded_bytes: 0,
        total_bytes: rel.map(|r| r.asset_size).unwrap_or(0),
        message: String::new(),
    }
}

/// Run one check (shared by manual/auto): only record last_check when a
/// Release is fetched (network failures aren't recorded, retry next hour); on
/// a new version, emit + silent pre-download (no waiting at install time).
/// Failure outcomes are only returned to the manual caller for a toast; the
/// automatic path discards them
fn do_check(app: &AppHandle) -> CheckOutcome {
    let state = app.state::<AppState>();
    {
        let mut u = state.update.lock().unwrap();
        if u.checking {
            return CheckOutcome::Failed { message: "A check is already in progress".into() };
        }
        u.checking = true;
    }
    let current = current_version(app);
    let outcome = match updater::fetch_latest(&format!("zcode-speed-panel/{current}")) {
        None => CheckOutcome::Failed { message: "Network error or Release info unavailable".into() },
        Some(rel) => {
            {
                let mut u = state.update.lock().unwrap();
                u.last_check_ms = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_millis() as i64)
                    .unwrap_or(0);
            }
            if updater::is_newer(&rel.tag, &current) {
                let ev = update_event("available", &current, Some(&rel));
                state.update.lock().unwrap().latest = Some(rel.clone());
                let _ = app.emit("update", ev);
                let new_version = rel.version.clone();
                spawn_download(app.clone(), rel);
                CheckOutcome::Available { current, new_version }
            } else {
                CheckOutcome::UpToDate { current }
            }
        }
    };
    state.update.lock().unwrap().checking = false;
    outcome
}

/// Background installer pre-download: progress is pushed via "update" events
/// (250ms throttle), emitting ready when done; if the user already clicked
/// "Update Now" (install_when_ready), start the install right away. Automatic
/// pre-download failures are fully silent (retry at install time); only a
/// failure while waiting to install emits error
fn spawn_download(app: AppHandle, rel: Release) {
    let current = current_version(&app);
    {
        let state = app.state::<AppState>();
        let mut u = state.update.lock().unwrap();
        if let Some((tag, _)) = &u.downloaded {
            if *tag == rel.tag {
                // This version was already pre-downloaded (restoring state
                // after a frontend refresh also goes through here)
                drop(u);
                let _ = app.emit("update", update_event("ready", &current, Some(&rel)));
                return;
            }
        }
        if u.downloading {
            return;
        }
        u.downloading = true;
    }
    std::thread::spawn(move || {
        let mut ev = update_event("downloading", &current, Some(&rel));
        let mut last_emit = std::time::Instant::now();
        let progress_app = app.clone();
        let result = updater::download(&rel, &format!("zcode-speed-panel/{current}"), &mut |done, total| {
            if last_emit.elapsed() >= Duration::from_millis(250) {
                last_emit = std::time::Instant::now();
                ev.downloaded_bytes = done;
                ev.total_bytes = total;
                let _ = progress_app.emit("update", ev.clone());
            }
        });
        match result {
            Ok(path) => {
                let mut ev_ready = update_event("ready", &current, Some(&rel));
                ev_ready.downloaded_bytes = rel.asset_size;
                let launch = {
                    let state = app.state::<AppState>();
                    let mut u = state.update.lock().unwrap();
                    u.downloading = false;
                    u.downloaded = Some((rel.tag.clone(), path));
                    u.install_when_ready
                };
                let _ = app.emit("update", ev_ready);
                if launch {
                    launch_update(&app);
                }
            }
            Err(msg) => {
                let wait = {
                    let state = app.state::<AppState>();
                    let mut u = state.update.lock().unwrap();
                    u.downloading = false;
                    let wait = u.install_when_ready;
                    u.install_when_ready = false; // After a failure, wait for the user to click again; no auto-retry
                    wait
                };
                if wait {
                    // The user is waiting to install but it fails: tell them
                    // honestly (the only interrupting scenario; staying silent
                    // would make the "Update Now" button look broken)
                    let mut ev = update_event("error", &current, Some(&rel));
                    ev.message = msg;
                    let _ = app.emit("update", ev);
                }
            }
        }
    });
}

/// Launch the install: on Windows, run the NSIS installer and then exit the
/// app (the installer takes over; wait 600ms before exiting so the installer
/// doesn't hit the process lock of the still-exiting app); on macOS, open the
/// dmg and let the user drag into Applications (the app doesn't exit; the old
/// version keeps running until the user restarts)
fn launch_update(app: &AppHandle) {
    let state = app.state::<AppState>();
    let rel = state.update.lock().unwrap().latest.clone();
    let downloaded = state.update.lock().unwrap().downloaded.clone();
    let (Some(rel), Some((tag, path))) = (rel, downloaded) else {
        return;
    };
    if tag != rel.tag {
        return; // Don't install a stale artifact (when latest changes, the pre-download fetches the new package)
    }
    let mut ev = update_event("launching", &current_version(app), Some(&rel));
    #[cfg(target_os = "windows")]
    let msg = "Installer launched, the app will exit shortly…".to_string();
    #[cfg(target_os = "macos")]
    let msg = "Install image opened: drag zcode-speed-panel into Applications to overwrite the installation".to_string();
    ev.message = msg;
    let _ = app.emit("update", ev);
    if updater::launch_installer(&path).is_err() {
        let mut ev = update_event("error", &current_version(app), Some(&rel));
        ev.message = "Failed to launch the installer".into();
        let _ = app.emit("update", ev);
        return;
    }
    #[cfg(target_os = "windows")]
    {
        save_all(app);
        let handle = app.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(600));
            handle.exit(0);
        });
    }
}

/// Manual check (clicking the version number at the footer's bottom-right):
/// synchronously returns the result for a frontend toast; when there's an
/// update, the card is rendered by the "update" event (this command only
/// handles the result toast)
#[tauri::command]
fn check_update(app: AppHandle) -> CheckOutcome {
    do_check(&app)
}

/// Frontend "Update Now" button: already pre-downloaded → launch the install
/// directly; otherwise mark pending-install and make sure the download thread
/// is running (installs automatically when ready, no second click needed)
#[tauri::command]
fn install_update(app: AppHandle) -> Result<(), String> {
    let state = app.state::<AppState>();
    let rel = state.update.lock().unwrap().latest.clone().ok_or("No update available")?;
    let ready = {
        let u = state.update.lock().unwrap();
        matches!(&u.downloaded, Some((tag, _)) if *tag == rel.tag)
    };
    if ready {
        launch_update(&app);
        return Ok(());
    }
    state.update.lock().unwrap().install_when_ready = true;
    spawn_download(app.clone(), rel); // No-op internally if already downloading
    Ok(())
}

/// Current version number (shown at the footer's bottom-right, sourced from
/// tauri.conf.json)
#[tauri::command]
fn app_version(app: AppHandle) -> String {
    current_version(&app)
}

// ---- Autostart (autostart.rs): read/write commands for the "Autostart"
//      section of the settings dialog ----
// The registry / LaunchAgent is the source of truth; get reads back the real
// state (frontend cache not trusted)

#[tauri::command]
fn autostart_get() -> String {
    autostart::current_mode().as_str().to_string()
}

/// Returns the actually-effective mode (read back after writing; failures
/// carry the error honestly to the frontend)
#[tauri::command]
fn autostart_set(mode: String) -> Result<String, String> {
    let parsed = autostart::AutostartMode::parse(&mode);
    autostart::set_mode(parsed)?;
    Ok(autostart::current_mode().as_str().to_string())
}

/// Export a text file (reports generated by the frontend, e.g. snapshot
/// upload records): writes to
/// `~/.zcode/speed-panel-exports/<file_name>` and returns the full path for
/// the frontend toast. The filename is whitelist-sanitized (only alphanumeric
/// ._-, guarding against path injection/traversal)
#[tauri::command]
fn export_text_file(file_name: String, text: String) -> Result<String, String> {
    const MAX_TEXT: usize = 4 * 1024 * 1024;
    if text.len() > MAX_TEXT {
        return Err("Content too large".into());
    }
    let cleaned: String = file_name
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_') { c } else { '_' })
        .collect();
    if cleaned.is_empty() || cleaned.starts_with('.') {
        return Err("Invalid file name".into());
    }
    let Some(home) = home_dir() else {
        return Err("Unable to locate the user directory".into());
    };
    let dir = home.join(".zcode").join("speed-panel-exports");
    std::fs::create_dir_all(&dir).map_err(|e| format!("Failed to create the export directory: {e}"))?;
    let path = dir.join(cleaned);
    std::fs::write(&path, text.as_bytes()).map_err(|e| format!("Failed to write: {e}"))?;
    Ok(path.to_string_lossy().into_owned())
}

/// Open a link in the system default browser (the release notes page). <a>
/// navigation inside the WebView is uncontrollable, so the backend opens
/// links uniformly; only https is accepted, preventing the frontend from
/// injecting schemes like file://
#[tauri::command]
fn open_url(url: String) -> Result<(), String> {
    if !url.starts_with("https://") {
        return Err("Only https links are supported".into());
    }
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        // CREATE_NO_WINDOW: avoids the cmd window flashing
        std::process::Command::new("cmd")
            .args(["/C", "start", "", &url])
            .creation_flags(0x0800_0000)
            .spawn()
            .map(|_| ())
            .map_err(|e| format!("Failed to open link: {e}"))
    }
    #[cfg(target_os = "macos")]
    {
        std::process::Command::new("open")
            .arg(&url)
            .spawn()
            .map(|_| ())
            .map_err(|e| format!("Failed to open link: {e}"))
    }
    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    {
        let _ = url;
        Err("Unsupported platform".into())
    }
}

/// Background update-check thread: after an 8s startup delay (avoiding the
/// startup SQLite/IO peak), check once; then wake hourly, only actually
/// sending a request when ≥24h has passed since the last successful check
/// (once a day). Failures are swallowed inside fetch_latest; the thread never
/// disturbs the user
fn update_loop(app: AppHandle) {
    std::thread::sleep(Duration::from_secs(8));
    let _ = do_check(&app);
    loop {
        std::thread::sleep(Duration::from_secs(3600));
        let due = {
            let state = app.state::<AppState>();
            let u = state.update.lock().unwrap();
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis() as i64)
                .unwrap_or(0);
            now - u.last_check_ms >= 24 * 3600 * 1000
        };
        if due {
            let _ = do_check(&app);
        }
    }
}

fn show_main(app: &AppHandle) {
    if let Some(win) = app.get_webview_window("main") {
        // mac: hint "app stays in the menu bar" when the window goes
        // hidden→shown (no Dock icon; the hint is how users find the entry
        // point after closing the window); already visible (e.g. woken by a
        // duplicate launch), don't disturb
        #[cfg(target_os = "macos")]
        let was_hidden = !win.is_visible().unwrap_or(true);
        let _ = win.unminimize();
        let _ = win.show();
        let _ = win.set_focus();
        #[cfg(target_os = "macos")]
        if was_hidden {
            let _ = app.emit("tray-hint", ());
        }
    }
}

fn hide_main(app: &AppHandle) {
    if let Some(win) = app.get_webview_window("main") {
        let _ = win.hide();
    }
}

fn toggle_main(app: &AppHandle) {
    if let Some(win) = app.get_webview_window("main") {
        if win.is_visible().unwrap_or(false) && !win.is_minimized().unwrap_or(false) {
            let _ = win.hide();
        } else {
            show_main(app);
        }
    }
}

/// Window moved: write the position back to memory under the current mode,
/// throttled to disk (at most once per 2s while dragging)
fn on_window_moved(app: &AppHandle, pos: PhysicalPosition<i32>) {
    let state = app.state::<AppState>();
    let mode = *state.mode.lock().unwrap();
    {
        let mut p = state.persist.lock().unwrap();
        match mode {
            Mode::Full => p.full_pos = Some((pos.x, pos.y)),
            Mode::Float => p.float_pos = Some((pos.x, pos.y)),
        }
    }
    let due = {
        let mut last = state.last_pos_save.lock().unwrap();
        let due = last.map_or(true, |t| t.elapsed() > Duration::from_secs(2));
        if due {
            *last = Some(std::time::Instant::now());
        }
        due
    };
    if due {
        save_all(app);
    }
}

/// Tray status: the top status item's text + the tray tooltip. Generated from
/// the snapshot state (generating / estimating / idle); written only when the
/// text changes (avoiding repeated sets every 700ms)
fn update_tray_status(app: &AppHandle, s: &Snapshot) {
    let state_word = if s.is_live || s.is_starting {
        "Generating"
    } else if s.is_estimating {
        "Estimating"
    } else {
        "Idle"
    };
    let text = if s.is_live || s.is_starting {
        format!("Generating {:.1} t/s", s.current_tps)
    } else if s.is_estimating {
        format!("Estimating ≈{:.1} t/s", s.current_tps)
    } else {
        "Idle".to_string()
    };
    let state = app.state::<AppState>();
    {
        let mut last = state.tray_status_last.lock().unwrap();
        if *last == text {
            return;
        }
        *last = text.clone();
    }
    if let Some(item) = state.tray_status.lock().unwrap().as_ref() {
        let _ = item.set_text(text);
    }
    if let Some(tray) = app.tray_by_id("main-tray") {
        let _ = tray.set_tooltip(Some(&format!("ZCode Speed Panel · {state_word}")));
    }
}

/// Background polling thread: incrementally parses model-io files and pushes
/// snapshots
fn poller(app: AppHandle) {
    loop {
        let payload = build_payload(&app);
        update_tray_status(&app, &payload.snapshot);
        // With multi-tasking (≥2 processes), after debounce, grow/retract the
        // pet window's per-task line space
        update_pet_task_extra(&app, payload.snapshot.tasks.len() >= 2);
        let _ = app.emit("metrics", &payload);
        std::thread::sleep(Duration::from_millis(700));
    }
}

fn main() {
    tauri::Builder::default()
        // On a duplicate launch, bring up the existing window
        .plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| {
            show_main(app);
        }))
        .manage(AppState {
            engine: Mutex::new(Engine::new()),
            mode: Mutex::new(Mode::Full),
            style: Mutex::new(FloatStyle::Gauge),
            live: Mutex::new(LiveIo::new()),
            net: Mutex::new(netio::NetIo::new()),
            guard: Mutex::new(snapshot_guard::SnapshotGuard::new()),
            debug: Mutex::new(DebugLog::new()),
            persist: Mutex::new(Persisted::default()),
            last_pos_save: Mutex::new(None),
            tray_status: Mutex::new(None),
            tray_status_last: Mutex::new(String::new()),
            tray_hint_pending: Mutex::new(cfg!(target_os = "macos")),
            saved_max_rect: Mutex::new(None),
            round_tps: Mutex::new((0.0, 0, false)),
            round_was_inflight: Mutex::new(false),
            drift: Mutex::new(RoundDrift::new()),
            cal_saved: Mutex::new(Vec::new()),
            pet_task_streak: Mutex::new(0),
            pet_task_extra: Mutex::new(0.0),
            update: Mutex::new(UpdateMem::default()),
        })
        .invoke_handler(tauri::generate_handler![
            snapshot,
            model_stats,
            chart_stats,
            set_mode,
            set_float_style,
            set_float_size,
            quit_app,
            toggle_maximize_safe,
            recalibrate,
            tray_hint_once,
            check_update,
            install_update,
            app_version,
            export_text_file,
            open_url,
            autostart_get,
            autostart_set,
            snapshot_guard_status,
            snapshot_guard_apply,
            snapshot_guard_release,
            open_checkpoint_dir
        ])
        .setup(|app| {
            // mac activation policy **switches dynamically** (apply_mode sets
            // it per mode, no longer fixed): full panel = Regular (with a
            // Dock icon — macOS only treats Regular apps as "proper apps", and
            // only they get native fullscreen Space via the green traffic
            // light; Accessory is always auxiliary fullscreen: fills the
            // screen but the menu bar stays, confirmed by testing on
            // 2026-09-19); collapsed float window = Accessory (hides the
            // Dock, stays resident in the menu bar, app doesn't quit)

            // mac: a custom app menu intercepts Cmd+Q as "collapse to
            // floating window" (no system quit item registered), plus an edit
            // menu to preserve the WebView's Cmd+C/V/X/A shortcuts
            #[cfg(target_os = "macos")]
            {
                macos_ui::install(app)?;
                app.on_menu_event(|app, ev| {
                    if ev.id().as_ref() == "collapse-to-float" {
                        save_all(app);
                        collapse_to_float(app);
                    }
                });
            }

            // ---- System tray ----
            // Top status item (disabled, not clickable; the poller refreshes
            // its text from the snapshot every tick)
            let status = MenuItem::with_id(app, "status", "Idle", false, None::<&str>)?;
            let show = MenuItem::with_id(app, "show", "Show Panel", true, None::<&str>)?;
            let hide = MenuItem::with_id(app, "hide", "Hide to Tray", true, None::<&str>)?;
            let toggle_float =
                MenuItem::with_id(app, "toggle-float", "Float Window / Full Panel", true, None::<&str>)?;
            let quit = MenuItem::with_id(app, "quit", "Quit", true, None::<&str>)?;
            let sep = PredefinedMenuItem::separator(app)?;
            let menu = Menu::with_items(
                app,
                &[&status, &sep, &show, &hide, &toggle_float, &quit],
            )?;
            app.state::<AppState>().tray_status.lock().unwrap().replace(status);

            let icon = tauri::image::Image::from_bytes(include_bytes!("../icons/32x32.png"))?;
            TrayIconBuilder::with_id("main-tray")
                .icon(icon)
                .tooltip("ZCode Speed Panel")
                .menu(&menu)
                .show_menu_on_left_click(false)
                .on_menu_event(|app, ev| match ev.id().as_ref() {
                    "show" => show_main(app),
                    "hide" => hide_main(app),
                    "toggle-float" => {
                        let cur = *app.state::<AppState>().mode.lock().unwrap();
                        switch_mode(app, if cur == Mode::Float { Mode::Full } else { Mode::Float });
                    }
                    "quit" => {
                        save_all(app);
                        app.exit(0);
                    }
                    _ => {}
                })
                .on_tray_icon_event(|tray, ev| {
                    if let TrayIconEvent::Click {
                        button: MouseButton::Left,
                        button_state: MouseButtonState::Up,
                        ..
                    } = ev
                    {
                        toggle_main(tray.app_handle());
                    }
                })
                .build(app)?;

            // ---- Close button = collapse to floating window; remember position on move (each mode independently) ----
            let win_handle = app.handle().clone();
            app.get_webview_window("main")
                .unwrap()
                .on_window_event(move |event| match event {
                    WindowEvent::CloseRequested { api, .. } => {
                        api.prevent_close();
                        // Clicking close = instantly become a floating window
                        // (not hidden to tray; full exit goes through the
                        // tray / context menu)
                        collapse_to_float(&win_handle);
                    }
                    WindowEvent::Moved(pos) => on_window_moved(&win_handle, *pos),
                    _ => {}
                });

            // ---- Restore the last display mode, style, and position before showing the window, avoiding a flash of the full size ----
            let persisted = load_persisted();
            let mode = Mode::parse(&persisted.mode);
            let style = FloatStyle::parse(&persisted.style);
            {
                let state = app.state::<AppState>();
                *state.mode.lock().unwrap() = mode;
                *state.style.lock().unwrap() = style;
                *state.persist.lock().unwrap() = persisted;
            }
            // Restore the last learned coefficient samples (missing file /
            // corrupted / older than 14 days → keep the 600 prior): after a
            // dev hot-restart or reboot the reading is immediately usable, no
            // re-converging from cold start every time
            {
                let state = app.state::<AppState>();
                let samples = load_cal_samples();
                if !samples.is_empty() {
                    let n = state.live.lock().unwrap().restore_cal(samples);
                    eprintln!("[zcode-speed-panel] restored {n} calibration samples");
                }
                *state.cal_saved.lock().unwrap() = state.live.lock().unwrap().cal_state();
            }
            let window = app.get_webview_window("main").unwrap();
            let p = app.state::<AppState>().persist.lock().unwrap().clone();
            let pet_extra = *app.state::<AppState>().pet_task_extra.lock().unwrap();
            apply_mode(&window, mode, style, &p, pet_extra);
            // Follow ZCode startup (autostart.rs follow mode, booted with
            // --zcode-follow): wait silently — no window shown, tray only;
            // the watcher thread wakes it once a ZCode process is found. The
            // single-instance plugin guarantees the argument only applies to
            // the "first instance at boot" (when an instance already exists,
            // this process never reaches setup; the wake callback just shows
            // the old instance's window)
            let follow_boot = autostart::follow_requested();
            if follow_boot {
                eprintln!("[zcode-speed-panel] follow-ZCode boot: waiting silently (tray resident)");
                let watch = app.handle().clone();
                std::thread::spawn(move || {
                    // The system is busy right at boot; rest 3s before starting detection
                    std::thread::sleep(Duration::from_secs(3));
                    loop {
                        if autostart::zcode_running() {
                            eprintln!("[zcode-speed-panel] ZCode process detected, showing the panel");
                            show_main(&watch);
                            break;
                        }
                        std::thread::sleep(Duration::from_secs(2));
                    }
                });
            } else {
                let _ = window.show();
            }
            // The mac onboarding hint is not emitted here: setup runs before
            // the event loop / WKWebView load, so an emit would be dropped —
            // instead the frontend invokes `tray_hint_once` once ready (one-time)

            // ---- Start the polling thread ----
            let poll_handle = app.handle().clone();
            std::thread::spawn(move || poller(poll_handle));

            // ---- Start the update-check thread (once at startup+8s, once a day while resident, silent) ----
            let update_handle = app.handle().clone();
            std::thread::spawn(move || update_loop(update_handle));
            Ok(())
        })
        .build(tauri::generate_context!())
        .expect("error while building zcode-speed-panel")
        .run(|app, event| match event {
            // Last line of defense: exit requests that aren't an explicit
            // exit(0) (e.g. the last window closing on mac, quitting before a
            // system logout) are always blocked and collapsed to the floating
            // window — real exit only happens via the tray "Quit" and the
            // float window's right-click "Quit" (with app.exit, code=Some,
            // let it through)
            tauri::RunEvent::ExitRequested { code: None, api, .. } => {
                api.prevent_exit();
                save_all(app);
                collapse_to_float(app);
            }
            // Save once more before the real exit (best-effort)
            tauri::RunEvent::Exit => {
                save_all(app);
            }
            _ => {}
        });
}

/// Mac-only UI: the app menu bar. Cmd+Q is intercepted as "collapse to
/// floating window" (in Accessory mode the app has no Dock/Cmd+Tab entry;
/// quitting outright would make users think the app is gone); no system quit
/// item is registered in the menu, ensuring exit only happens via the tray
/// and the float window's right-click. The edit submenu preserves
/// Cmd+C/V/X/A, otherwise the WebView's text-editing shortcuts would break
#[cfg(target_os = "macos")]
mod macos_ui {
    use super::*;
    use tauri::menu::{MenuItem, PredefinedMenuItem, Submenu};

    pub fn install(app: &tauri::App) -> tauri::Result<()> {
        let collapse = MenuItem::with_id(
            app,
            "collapse-to-float",
            "Hide as Floating Window",
            true,
            Some("CmdOrCtrl+Q"),
        )?;
        let app_menu = Submenu::with_id_and_items(
            app,
            "app",
            "zcode-speed-panel",
            true,
            &[&collapse],
        )?;
        let edit_menu = Submenu::with_id_and_items(
            app,
            "edit",
            "Edit",
            true,
            &[
                &PredefinedMenuItem::cut(app, None)?,
                &PredefinedMenuItem::copy(app, None)?,
                &PredefinedMenuItem::paste(app, None)?,
                &PredefinedMenuItem::select_all(app, None)?,
            ],
        )?;
        let menu = Menu::with_items(app, &[&app_menu, &edit_menu])?;
        app.set_menu(menu)?;
        Ok(())
    }
}
