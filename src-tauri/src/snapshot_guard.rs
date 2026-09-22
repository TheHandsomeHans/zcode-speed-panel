/// Snapshot protection: block ZCode workspace snapshots from silently uploading (directory write lock, dual-platform).
///
/// ## Background (verified locally, 2026-09)
///
/// After signing in, ZCode packages the **entire workspace** (including full `.git/` history) into encrypted tar.gz files written to
/// `~/.zcode/v2/checkpoints/<workspace-hash>/pending/*.tar.gz.enc`, then fetches credentials via zcode.z.ai and uploads directly to Alibaba Cloud OSS; the settings toggle has no effect, and the credential API shares a domain with the model API, **so blocking the network is not viable**. Preventing ZCode from writing to that directory kills the snapshot pipeline while model chat/completions/tool calls continue to work normally; the only loss is "checkpoint rollback / timeline".
/// The lock is reversible at any time; when the directory is left empty, ZCode automatically rebuilds its contents. (Mechanism source: ferster blog "ZCode Silently Uploads Workspace Snapshots")
///
/// ## Implementation highlights
///
/// - **No network or process interference**: only filesystem operations on the directory (remove/create/lock, `std::process::Command` invoking system commands: mac `chflags` / win `icacls`), non-invasive to a running ZCode;
/// - **Lock detection = write probe**: create+delete a temp file inside the directory; creation failure means locked. Pure std implementation, cleaner than parsing `ls -lO` / libc `st_flags`;
/// - **Informed consent on the frontend** (`#guard-confirm` modal must explicitly disclose the loss of checkpoint rollback, see key-rules #16); apply/release execute immediately upon invocation, no second confirmation;
/// - **Protection counter**: lock timestamp and calls baseline persisted in `~/.zcode/speed-panel-guard.json`; the poller accumulates `blocked_rounds` per tick based on calls delta, **and the baseline calls_seen is also persisted** — otherwise after restart the in-memory last_calls resets to 0, and the first tick would count the full day's total (observed: 15 minutes inflated to 3412); rollback across days resets the baseline tick with 0 delta;
/// - **Directory already locked but no record** (user manually locked after reading docs / panel reinstall): the first tick that detects it backfills the baseline, counting rounds from that moment;
/// - **Archive before clearing**: before apply deletes checkpoints, the upload record rows (most recent snapshot per workspace) are saved to `~/.zcode/speed-panel-ckpt-history.json`, so the frontend can fully review "pre-protection original upload records" during protection (explicitly requested by user, 2026-09-18); repeated enable merges per workspace (new record overwrites old row for same workspace);
/// - **Platform lock mechanisms** (see `set_immutable` and key-rules #16): macOS `chflags uchg` immutable flag; Windows NTFS deny ACE (icacls denies create/write for current user SID, deny takes precedence over allow). Other platforms return `supported=false`, apply/release return an error string in English, frontend buttons disabled and labeled accordingly.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// Directory write lock, dual-platform (mac chflags / win icacls; other platforms refuse apply/release)
pub const SUPPORTED: bool = cfg!(any(target_os = "macos", target_os = "windows"));

/// Checkpoints directory scan throttle (poller ~700ms per tick, no need to hit filesystem every tick)
const SCAN_EVERY: std::time::Duration = std::time::Duration::from_secs(5);

/// Protection status pushed to the frontend with every metrics payload (serde camelCase)
#[derive(Clone, Debug, Default, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct SnapshotGuardStatus {
    /// Whether this platform supports directory write lock (mac chflags / win icacls; frontend disables button when false)
    pub supported: bool,
    /// Whether the checkpoints directory is currently locked (write probe failed)
    pub locked: bool,
    /// Lock timestamp (guard.json, epoch ms); backfilled by poller when directory is locked but no record exists
    pub locked_since_ms: Option<i64>,
    /// Conversation rounds elapsed since protection was enabled (poller accumulates by calls_today delta)
    pub blocked_rounds: u64,
    /// Accumulated artifact count (`**/pending/*.enc`)
    pub artifact_count: u64,
    /// Total artifact size (bytes)
    pub artifact_bytes: u64,
    /// Number of workspace directories
    pub workspace_count: u64,
    /// Σ failureCount (upload failure count recorded by ZCode itself)
    pub failure_count: u64,
    /// Pre-protection original upload records (archived at apply; frontend reviews fully during protection)
    pub history: Vec<crate::metrics::CkptStat>,
}

/// Fields of interest from a single workspace's state.json (parses output of pure function)
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct StateSummary {
    pub failure_count: u64,
}

/// Checkpoints directory scan summary
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct ScanSummary {
    pub artifact_count: u64,
    pub artifact_bytes: u64,
    pub workspace_count: u64,
    pub failure_count: u64,
}

/// state.json → summary (pure function, testable): malformed JSON / non-object returns None (caller
/// skips this workspace's failure count; artifacts and directory count are still counted accurately);
/// missing failureCount defaults to 0
pub(crate) fn parse_state_summary(json: &str) -> Option<StateSummary> {
    let v: serde_json::Value = serde_json::from_str(json).ok()?;
    if !v.is_object() {
        return None;
    }
    Some(StateSummary {
        failure_count: v.get("failureCount").and_then(|x| x.as_u64()).unwrap_or(0),
    })
}

/// Aggregate (pure function, testable): per-workspace (state parse result + pending artifact size list)
/// → total summary. Workspaces with malformed state (None) still contribute workspace/artifact count and
/// size, they just don't contribute failureCount
pub(crate) fn summarize_scans(scans: &[(Option<StateSummary>, Vec<u64>)]) -> ScanSummary {
    let mut s = ScanSummary::default();
    for (state, enc_sizes) in scans {
        s.workspace_count += 1;
        for sz in enc_sizes {
            s.artifact_count += 1;
            s.artifact_bytes += *sz;
        }
        if let Some(st) = state {
            s.failure_count += st.failure_count;
        }
    }
    s
}

/// guard.json (`~/.zcode/speed-panel-guard.json`): lock timestamp + calls baseline +
/// accumulated rounds + last tick's calls_seen (incremental baseline after restart, prevents full-day count from being bulk-loaded).
/// Missing default of any lock field means "not protected"; no extra keys persisted
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
struct GuardFile {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    locked_since_ms: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    calls_baseline: Option<u64>,
    #[serde(default)]
    blocked_rounds: u64,
    #[serde(default)]
    calls_seen: u64,
}

/// blocked_rounds incremental accumulation (pure function, testable): uses the **persisted** calls_seen
/// as baseline (rather than in-memory last_calls — restart resetting to 0 would bulk-load the full day count),
/// rollback across days saturates to 0
pub(crate) fn accrue_rounds(blocked: u64, calls_seen: u64, calls_today: u64) -> (u64, u64) {
    (blocked + calls_today.saturating_sub(calls_seen), calls_today)
}

/// Checkpoints subdirectory name validation (pure function, testable): whitelisted characters +
/// length limit — the "open directory" command builds paths from it; must reject path traversal
/// (.., slashes, absolute paths, hidden names, etc. are all rejected)
pub(crate) fn valid_hash_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 128
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

/// Checkpoints directory (shared by main.rs "open directory" command and protection)
pub(crate) fn checkpoints_dir() -> Option<PathBuf> {
    crate::metrics::home_dir().map(|h| h.join(".zcode").join("v2").join("checkpoints"))
}

fn guard_file_path() -> Option<PathBuf> {
    crate::metrics::home_dir().map(|h| h.join(".zcode").join("speed-panel-guard.json"))
}

/// Pre-protection original upload record archive (written before apply clears, fully reviewable during protection)
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct GuardHistory {
    saved_at_ms: i64,
    rows: Vec<crate::metrics::CkptStat>,
}

fn history_file_path() -> Option<PathBuf> {
    crate::metrics::home_dir().map(|h| h.join(".zcode").join("speed-panel-ckpt-history.json"))
}

fn load_history() -> GuardHistory {
    let Some(path) = history_file_path() else { return GuardHistory::default() };
    std::fs::read_to_string(path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

fn save_history(h: &GuardHistory) {
    if let Some(path) = history_file_path() {
        if let Err(e) = std::fs::write(&path, serde_json::to_string(h).unwrap_or_default()) {
            eprintln!("[zcode-speed-panel] Failed to archive snapshot history: {e}");
        }
    }
}

/// History merge (pure function, testable): new scan rows overwrite old rows for the same workspace
/// ("most recent per workspace" semantics); other workspaces preserved; sorted descending by record
/// time, capped at 500 rows to prevent archive bloat
pub(crate) fn merge_history(
    old: Vec<crate::metrics::CkptStat>,
    new: Vec<crate::metrics::CkptStat>,
) -> Vec<crate::metrics::CkptStat> {
    let mut map: HashMap<String, crate::metrics::CkptStat> =
        old.into_iter().map(|r| (r.workspace.clone(), r)).collect();
    for r in new {
        map.insert(r.workspace.clone(), r);
    }
    let mut rows: Vec<crate::metrics::CkptStat> = map.into_values().collect();
    rows.sort_by(|a, b| b.recorded_ms.cmp(&a.recorded_ms));
    rows.truncate(500);
    rows
}

/// Malformed/missing → default (not protected), no error — status detection uses actual directory lock state
fn load_guard_file() -> GuardFile {
    let Some(path) = guard_file_path() else { return GuardFile::default() };
    std::fs::read_to_string(path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

fn save_guard_file(f: &GuardFile) {
    if let Some(path) = guard_file_path() {
        if let Err(e) = std::fs::write(&path, serde_json::to_string(f).unwrap_or_default()) {
            eprintln!("[zcode-speed-panel] Failed to persist guard state: {e}");
        }
    }
}

/// Directory write lock, dispatched by platform (only called on SUPPORTED platforms):
/// - macOS: `chflags [-R] uchg/nouchg` (user-level immutable flag, no sudo needed).
///   recursive = lock the entire subtree including subdirs/files (keep mode must be recursive: uchg only
///   governs the directory's own entry table; locking only the root cannot block writes inside existing
///   workspace subdirectories, see key-rules #16);
/// - Windows: NTFS has no user-level immutable flag; the equivalent is a directory **deny ACE**
///   (`icacls /deny *<SID>:(OI)(CI)(WD,AD)`) — deny takes precedence over all allows;
///   the current user is refused create/write everywhere under this tree, reads are unaffected; (OI)(CI)
///   inheritable ACEs are automatically propagated by the system to the entire existing subtree (including
///   workspace subdirectories present before locking, equivalent to mac -R recursive), so recursive does
///   not need to branch. Unlock = `/remove:d` deletes that deny ACE (inherited copies on subtrees are
///   cleared along with automatic inheritance).
///   **Intentionally excludes D/DC (delete)**: observed (2026-09-20) that denying D also blocks pure
///   reads — tools that open files with DELETE permission (git-bash POSIX unlink emulation, some editors/
///   backup/sandbox layers) fail entirely; denying only WD/AD is sufficient to kill the snapshot write
///   pipeline (both new file creation and existing file modification are blocked), see key-rules #16.
fn set_immutable(dir: &Path, lock: bool, recursive: bool) -> Result<(), String> {
    if cfg!(target_os = "macos") {
        let flag = if lock { "uchg" } else { "nouchg" };
        let mut cmd = std::process::Command::new("chflags");
        if recursive {
            cmd.arg("-R");
        }
        let st = cmd
            .arg(flag)
            .arg(dir)
            .status()
            .map_err(|e| format!("Failed to execute chflags: {e}"))?;
        if st.success() {
            Ok(())
        } else {
            Err(format!("chflags {}{} did not succeed (exit {:?})", if recursive { "-R " } else { "" }, flag, st.code()))
        }
    } else if cfg!(windows) {
        let sid = current_sid()?;
        let mut cmd = std::process::Command::new("icacls");
        cmd.arg(dir);
        if lock {
            cmd.arg("/deny").arg(format!("*{sid}:(OI)(CI)(WD,AD)"));
        } else {
            cmd.arg("/remove:d").arg(format!("*{sid}"));
        }
        let st = cmd
            .status()
            .map_err(|e| format!("Failed to execute icacls: {e}"))?;
        if st.success() {
            Ok(())
        } else {
            Err(format!(
                "icacls {} did not succeed (exit {:?})",
                if lock { "/deny" } else { "/remove:d" },
                st.code()
            ))
        }
    } else {
        Err("File lock is only supported on macOS / Windows".into())
    }
}

/// Current user SID (parsed from whoami, cached in-process). icacls deny ACE must use SID
/// rather than username: when signed in with a Microsoft account, %USERNAME% differs from the
/// account principal name in ACLs (moqiq ≠ MicrosoftAccount\email), deny by name would not match
fn current_sid() -> Result<String, String> {
    static SID: std::sync::OnceLock<Result<String, String>> = std::sync::OnceLock::new();
    SID.get_or_init(|| {
        let out = std::process::Command::new("whoami")
            .args(["/user", "/fo", "csv", "/nh"])
            .output()
            .map_err(|e| format!("Failed to execute whoami: {e}"))?;
        if !out.status.success() {
            return Err(format!("whoami did not succeed (exit {:?})", out.status.code()));
        }
        parse_whoami_sid(&String::from_utf8_lossy(&out.stdout))
            .ok_or_else(|| "Could not parse current user SID from whoami output".to_string())
    })
    .clone()
}

/// `whoami /user /fo csv /nh` output → SID (pure function, testable). Observed output looks like
/// `"superdesktop\moqiq","S-1-5-21-…-1001"` (CRLF line endings, quoted fields);
/// takes the field starting with "S-1-", tolerant of extra columns/blank lines/quote variations
pub(crate) fn parse_whoami_sid(csv: &str) -> Option<String> {
    csv.lines().find_map(|line| {
        line.split(',').find_map(|f| {
            let f = f.trim().trim_matches('"');
            (f.starts_with("S-1-") && f.chars().all(|c| c.is_ascii_alphanumeric() || c == '-'))
                .then(|| f.to_string())
        })
    })
}

/// Write probe: directory exists and cannot create a temp file inside it = locked (uchg blocks
/// new entries inside the directory). Probe file is deleted immediately; directory absent = not locked
fn probe_locked(dir: &Path) -> bool {
    if !dir.is_dir() {
        return false;
    }
    let probe = dir.join(".speed-panel-lock-probe");
    match std::fs::File::create(&probe) {
        Ok(_) => {
            let _ = std::fs::remove_file(&probe);
            false
        }
        Err(_) => true,
    }
}

/// Scan checkpoints: for each workspace directory read state.json (skip if malformed) + pending/*.enc
/// file sizes, aggregate via the pure function summarize_scans
fn scan_checkpoints(dir: &Path) -> ScanSummary {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return ScanSummary::default();
    };
    let mut scans: Vec<(Option<StateSummary>, Vec<u64>)> = Vec::new();
    for e in rd.flatten() {
        if !e.file_type().map(|t| t.is_dir()).unwrap_or(false) {
            continue;
        }
        let ws = e.path();
        let state = std::fs::read_to_string(ws.join("state.json"))
            .ok()
            .and_then(|t| parse_state_summary(&t));
        let mut enc_sizes = Vec::new();
        if let Ok(rd2) = std::fs::read_dir(ws.join("pending")) {
            for f in rd2.flatten() {
                if f.file_name().to_string_lossy().ends_with(".enc") {
                    if let Ok(m) = f.metadata() {
                        enc_sizes.push(m.len());
                    }
                }
            }
        }
        scans.push((state, enc_sizes));
    }
    summarize_scans(&scans)
}

/// Snapshot protection state machine: guard.json in-memory mirror + last tick's calls + throttled scan cache.
/// Poller calls `tick` every tick; apply/release invoked by Tauri commands (frontend already passed confirmation modal)
pub struct SnapshotGuard {
    file: GuardFile,
    /// Last tick's calls_today (incremental accumulation; rollback across days resets baseline tick with 0 delta)
    last_calls: u64,
    scan: ScanSummary,
    last_scan: Option<std::time::Instant>,
    /// Pre-protection original upload record archive (written at apply; pushed to frontend via status for review)
    history: Vec<crate::metrics::CkptStat>,
}

impl SnapshotGuard {
    pub fn new() -> Self {
        let file = load_guard_file();
        // Incremental baseline restored from guard.json: first tick after restart has delta = real delta,
        // not calls_today - 0 (which would bulk-load the full day count into blocked_rounds)
        let last_calls = file.calls_seen;
        SnapshotGuard {
            file,
            last_calls,
            scan: ScanSummary::default(),
            last_scan: None,
            history: load_history().rows,
        }
    }

    /// For the independent status command to read the current calls metric (does not advance the counter)
    pub fn last_calls_seen(&self) -> u64 {
        self.last_calls
    }

    fn status(&self, locked: bool) -> SnapshotGuardStatus {
        SnapshotGuardStatus {
            supported: SUPPORTED,
            locked,
            locked_since_ms: self.file.locked_since_ms,
            blocked_rounds: self.file.blocked_rounds,
            artifact_count: self.scan.artifact_count,
            artifact_bytes: self.scan.artifact_bytes,
            workspace_count: self.scan.workspace_count,
            failure_count: self.scan.failure_count,
            history: self.history.clone(),
        }
    }

    /// Every poller tick: write probe to determine lock state → maintain blocked_rounds (persist on change) →
    /// throttled scan (5s) → assemble status
    pub fn tick(&mut self, calls_today: u64, now_ms: i64) -> SnapshotGuardStatus {
        let Some(dir) = checkpoints_dir() else {
            return SnapshotGuardStatus { supported: SUPPORTED, ..Default::default() };
        };
        let locked = probe_locked(&dir);
        if locked {
            if self.file.locked_since_ms.is_none() || self.file.calls_baseline.is_none() {
                // Directory is locked but no record exists (user manual chflags / panel reinstall lost state):
                // backfill baseline from now
                self.file.locked_since_ms = Some(now_ms);
                self.file.calls_baseline = Some(calls_today);
                self.last_calls = calls_today;
                self.file.calls_seen = calls_today;
                save_guard_file(&self.file);
            } else {
                // calls_today only grows within a day; rollback across days (saturating to 0 delta)
                // also resets the baseline tick, continuing accumulation from the new day's count. Baseline
                // uses persisted calls_seen (new() already restored into last_calls), so restart doesn't
                // eat the full day count
                let (blocked, seen) = accrue_rounds(
                    self.file.blocked_rounds,
                    self.last_calls,
                    calls_today,
                );
                self.last_calls = seen;
                if blocked != self.file.blocked_rounds {
                    self.file.blocked_rounds = blocked;
                    self.file.calls_seen = seen;
                    save_guard_file(&self.file);
                } else if self.file.calls_seen != seen {
                    self.file.calls_seen = seen;
                    save_guard_file(&self.file);
                }
            }
        } else {
            self.last_calls = calls_today;
            if self.file.locked_since_ms.is_some() {
                // Record exists but directory is already unlocked (external release / manual nouchg): clear state to reflect accurately
                self.file = GuardFile::default();
                save_guard_file(&self.file);
            }
        }
        if self.last_scan.map_or(true, |t| t.elapsed() > SCAN_EVERY) {
            self.last_scan = Some(std::time::Instant::now());
            self.scan = scan_checkpoints(&dir);
        }
        self.status(locked)
    }

    /// Enable protection (frontend already passed confirmation modal; keep_files = user chose to keep/delete existing snapshots):
    ///
    /// - **Keep mode** (keep_files=true): inventory upload records, then **recursively lock the entire subtree**
    ///   (mac `chflags -R uchg` / win inheritable deny ACE auto-propagates) — snapshot files are kept in place
    ///   (encrypted, read-only), the list can still be viewed and opened; records are not destroyed, no history
    ///   archive is written. Must lock entire tree: mac locking only the root cannot block writes inside existing
    ///   subdirectories, win (OI)(CI) inheritance similarly covers the entire subtree;
    /// - **Delete mode** (keep_files=false): **archive before clearing** (upload record rows merged into
    ///   ckpt-history.json, reviewable during protection) → recreate empty directory → lock root directory.
    ///
    /// Both paths record guard.json (lock timestamp + calls baseline) only after write probe verification passes
    pub fn apply(
        &mut self,
        calls_today: u64,
        now_ms: i64,
        keep_files: bool,
    ) -> Result<SnapshotGuardStatus, String> {
        if !SUPPORTED {
            return Err("File lock is only supported on macOS / Windows".into());
        }
        let dir = checkpoints_dir().ok_or("Cannot locate user directory")?;
        // Previous generation protection may be a recursive lock (keep mode); unlock entire tree first to make changes (idempotent)
        if probe_locked(&dir) {
            set_immutable(&dir, false, true)?;
        }
        if keep_files {
            std::fs::create_dir_all(&dir).map_err(|e| format!("Failed to recreate checkpoints directory: {e}"))?;
            set_immutable(&dir, true, true)?;
        } else {
            // Archive: before clearing, save the most recent snapshot record row per workspace (reuses
            // netio's parsing and row construction, metric fully consistent with the live list)
            let (_, obs) = crate::netio::scan_ckpt_states(&dir);
            let states: HashMap<String, _> = obs.into_iter().collect();
            let rows = crate::netio::ckpt_rows(&states);
            if !rows.is_empty() || !self.history.is_empty() {
                let merged = merge_history(std::mem::take(&mut self.history), rows);
                save_history(&GuardHistory { saved_at_ms: now_ms, rows: merged.clone() });
                self.history = merged;
            }
            if dir.exists() {
                std::fs::remove_dir_all(&dir).map_err(|e| format!("Failed to clear checkpoints: {e}"))?;
            }
            std::fs::create_dir_all(&dir).map_err(|e| format!("Failed to recreate checkpoints directory: {e}"))?;
            set_immutable(&dir, true, false)?;
        }
        if !probe_locked(&dir) {
            return Err("Lock did not take effect (write probe still succeeded); please check directory permissions".into());
        }
        self.file = GuardFile {
            locked_since_ms: Some(now_ms),
            calls_baseline: Some(calls_today),
            blocked_rounds: 0,
            calls_seen: calls_today,
        };
        self.last_calls = calls_today;
        self.scan = ScanSummary::default();
        self.last_scan = Some(std::time::Instant::now());
        // In keep mode, rescan immediately so the status row accurately shows "snapshots kept: N"
        if keep_files {
            self.scan = scan_checkpoints(&dir);
        }
        save_guard_file(&self.file);
        Ok(self.status(true))
    }

    /// Release protection: unlock entire tree (mac recursive nouchg / win remove deny ACE including inherited copies;
    /// compatible with keep mode's recursive lock); files are never touched — delete mode directory is already empty,
    /// keep mode snapshots are restored to writable in place and ZCode resumes automatically. Clears guard.json counters
    pub fn release(&mut self, calls_today: u64) -> Result<SnapshotGuardStatus, String> {
        if !SUPPORTED {
            return Err("File lock is only supported on macOS / Windows".into());
        }
        let dir = checkpoints_dir().ok_or("Cannot locate user directory")?;
        if probe_locked(&dir) {
            set_immutable(&dir, false, true)?;
        }
        self.file = GuardFile::default();
        self.last_calls = calls_today;
        save_guard_file(&self.file);
        let locked = probe_locked(&dir);
        Ok(self.status(locked))
    }
}

// ============ Tests ============
#[cfg(test)]
mod tests {
    use super::*;

    /// state.json parse + aggregate: failureCount sum, artifact count/size accumulation,
    /// malformed JSON tolerance (skipped workspaces still count artifacts and directories, just no failure contribution)
    #[test]
    fn state_summary_parse_and_aggregate() {
        let ok = parse_state_summary(
            r#"{"workspacePath":"/Users/x/proj","failureCount":3,
                "lastCompressedSize":{"encryptedSizeBytes":100,"workspaceSizeBytes":200}}"#,
        )
        .expect("should parse successfully");
        assert_eq!(ok.failure_count, 3);
        // missing failureCount defaults to 0
        assert_eq!(parse_state_summary(r#"{"workspacePath":"x"}"#).unwrap().failure_count, 0);
        // malformed JSON / non-object → None (caller skips)
        assert!(parse_state_summary("{oops").is_none());
        assert!(parse_state_summary("[]").is_none());

        let scans = vec![
            (Some(ok), vec![100, 50]),                        // 2 artifacts 150B, failure 3
            (None, vec![549_000_000]),                        // malformed state: failure not counted
            (Some(StateSummary { failure_count: 7 }), vec![]), // workspace with no artifacts
        ];
        let s = summarize_scans(&scans);
        assert_eq!(s.workspace_count, 3);
        assert_eq!(s.artifact_count, 3);
        assert_eq!(s.artifact_bytes, 549_000_150);
        assert_eq!(s.failure_count, 10);
        assert_eq!(summarize_scans(&[]), ScanSummary::default());
    }

    /// Protection status field serialization contract: camelCase keys + guard.json roundtrip
    /// (frontend SnapshotPayload.guard depends on key names; guard.json is the only cross-launch persistence)
    #[test]
    fn guard_status_serializes_locked_fields() {
        let st = SnapshotGuardStatus {
            supported: true,
            locked: true,
            locked_since_ms: Some(1_788_000_000_000),
            blocked_rounds: 42,
            artifact_count: 302,
            artifact_bytes: 302_000_000,
            workspace_count: 23,
            failure_count: 11,
            history: vec![crate::metrics::CkptStat {
                workspace: "proj".into(),
                bytes: 175_400_000,
                recorded_ms: 1_788_000_000_000,
                accepted: true,
                uploading: false,
                hash: Some("ab12cd34".into()),
            }],
        };
        let json = serde_json::to_value(&st).unwrap();
        assert_eq!(json["supported"], true);
        assert_eq!(json["locked"], true);
        assert_eq!(json["lockedSinceMs"], 1_788_000_000_000i64);
        assert_eq!(json["blockedRounds"], 42);
        assert_eq!(json["artifactCount"], 302);
        assert_eq!(json["artifactBytes"], 302_000_000);
        assert_eq!(json["workspaceCount"], 23);
        assert_eq!(json["failureCount"], 11);
        assert_eq!(json["history"][0]["workspace"], "proj");
        assert_eq!(json["history"][0]["recordedMs"], 1_788_000_000_000i64);
        // Default unprotected values: locked=false, timestamp null (frontend hides "after protection" row when null)
        let def = serde_json::to_value(SnapshotGuardStatus::default()).unwrap();
        assert_eq!(def["locked"], false);
        assert_eq!(def["lockedSinceMs"], serde_json::Value::Null);

        // guard.json roundtrip: lock fields preserved (including calls_seen incremental baseline); empty object all defaults;
        // default instance does not emit extra keys
        let f = GuardFile { locked_since_ms: Some(123), calls_baseline: Some(456), blocked_rounds: 7, calls_seen: 456 };
        let round: GuardFile = serde_json::from_str(&serde_json::to_string(&f).unwrap()).unwrap();
        assert_eq!(round, f);
        let empty: GuardFile = serde_json::from_str("{}").unwrap();
        assert_eq!(empty, GuardFile::default());
        assert!(!serde_json::to_string(&GuardFile::default()).unwrap().contains("lockedSinceMs"));
    }

    /// Incremental accumulation: normal delta added, rollback across days saturates to 0,
    /// baseline is persisted calls_seen (restart scenario delta counts only real delta,
    /// does not eat full-day count — incident: 15 minutes inflated to 3412)
    #[test]
    fn accrue_rounds_counts_real_delta_only() {
        assert_eq!(accrue_rounds(5, 100, 120), (25, 120)); // normal +20
        assert_eq!(accrue_rounds(5, 200, 50), (5, 50)); // rollback across days: 0 delta, reset baseline
        // Restart: calls_seen persisted 456, first tick after restart calls_today 470 → only +14
        // (old bug: in-memory reset to 0 → 470 - 0 = +470)
        assert_eq!(accrue_rounds(0, 456, 470), (14, 470));
        assert_eq!(accrue_rounds(0, 0, 0), (0, 0));
    }

    /// History merge: new rows overwrite old rows for same workspace, unknown workspaces preserved,
    /// sorted descending by timestamp, capped at limit (pre-protection records "archive before clearing", key-rules #16)
    #[test]
    fn merge_history_replaces_same_workspace_keeps_rest() {
        use crate::metrics::CkptStat;
        let row = |ws: &str, ms: i64, bytes: u64| CkptStat {
            workspace: ws.into(),
            bytes,
            recorded_ms: ms,
            accepted: true,
            uploading: false,
            hash: None,
        };
        let old = vec![row("a", 100, 10), row("b", 200, 20), row("c", 300, 30)];
        let new = vec![row("b", 900, 99), row("d", 800, 40)];
        let merged = merge_history(old, new);
        let names: Vec<&str> = merged.iter().map(|r| r.workspace.as_str()).collect();
        assert_eq!(names, vec!["b", "d", "c", "a"]); // descending by timestamp, b is already the 900 new row
        assert_eq!(merged[0].bytes, 99);
        // 500 cap truncation
        let many = (0..600).map(|i| row(&format!("w{i}"), i, 1)).collect();
        assert_eq!(merge_history(Vec::new(), many).len(), 500);
        assert_eq!(merge_history(Vec::new(), Vec::new()), Vec::new());
    }

    /// Directory name whitelist for "open directory": path traversal (../, slashes, absolute paths, dot-prefixed)
    /// all rejected; only ZCode-generated hash forms are allowed
    #[test]
    fn valid_hash_name_rejects_traversal() {
        assert!(valid_hash_name("ab12cd34"));
        assert!(valid_hash_name("A-b_C9"));
        assert!(!valid_hash_name(""));
        assert!(!valid_hash_name(".."));
        assert!(!valid_hash_name("a/b"));
        assert!(!valid_hash_name("a\\b"));
        assert!(!valid_hash_name("/etc"));
        assert!(!valid_hash_name(".hidden"));
        assert!(!valid_hash_name("a b"));
        assert!(!valid_hash_name("哈希"));
        assert!(!valid_hash_name(&"x".repeat(129)));
    }

    /// whoami /user /fo csv /nh → SID: observed two quoted columns CRLF; tolerant of extra columns,
    /// blank lines, unquoted format; no SID line (error output) returns None
    #[test]
    fn parse_whoami_sid_finds_sid_field() {
        let sid = "S-1-5-21-2444046543-1064250523-2101273865-1001";
        // Observed format (superdesktop, 2026-09-20)
        assert_eq!(
            parse_whoami_sid(&format!("\"superdesktop\\moqiq\",\"{sid}\"\r\n")),
            Some(sid.to_string())
        );
        // With third column (some versions output logon type) / multiline / unquoted
        assert_eq!(
            parse_whoami_sid(&format!("\"x\",\"{sid}\",\"7\"\n")),
            Some(sid.to_string())
        );
        assert_eq!(parse_whoami_sid(&format!("header noise\n{sid}\n")), Some(sid.to_string()));
        assert_eq!(parse_whoami_sid(""), None);
        assert_eq!(parse_whoami_sid("\"only user\",\"no sid here\""), None);
        // Looks-like but invalid (space/semicolon) not allowed — must be strict when spliced into icacls args
        assert_eq!(parse_whoami_sid("\"S-1-5 x\""), None);
    }

    /// Windows deny ACE full lifecycle (real icacls, locally verified guardian test):
    /// lock → create/modify on root and existing subdirs blocked while **reads unaffected** (including (OI)(CI)
    /// auto-propagation to subtrees that existed before locking) → unlock → all restored. Lock excludes D/DC
    /// rationale see `set_immutable` (key-rules #16: denying D also blocks reads for tools opening files with DELETE)
    #[test]
    #[cfg(windows)]
    fn windows_icacls_lock_roundtrip() {
        let dir = std::env::temp_dir().join(format!("sp-guard-test-{}", std::process::id()));
        // Leftover from a previous failed run may still be locked: best-effort unlock then clean up
        if dir.exists() {
            let _ = set_immutable(&dir, false, false);
            let _ = std::fs::remove_dir_all(&dir);
        }
        std::fs::create_dir_all(dir.join("sub")).expect("create temp directory");
        std::fs::write(dir.join("sub").join("f.txt"), "hi").expect("create test file");

        set_immutable(&dir, true, false).expect("icacls deny ACE should succeed");
        assert!(probe_locked(&dir), "write probe on root directory should fail after locking");
        assert!(
            std::fs::File::create(dir.join("sub").join("new")).is_err(),
            "create inside existing subdirectory should be blocked (inheritance propagation active)"
        );
        assert!(
            std::fs::write(dir.join("sub").join("f.txt"), "x").is_err(),
            "modify existing file should be blocked"
        );
        assert_eq!(
            std::fs::read_to_string(dir.join("sub").join("f.txt")).as_deref().ok(),
            Some("hi"),
            "lock must not block reads (scanning/record list depend on this)"
        );

        set_immutable(&dir, false, false).expect("icacls remove deny ACE should succeed");
        assert!(!probe_locked(&dir), "root directory should be writable after unlock");
        std::fs::write(dir.join("sub").join("new"), "x").expect("should be able to create after unlock");
        std::fs::remove_dir_all(&dir).expect("clean up temp directory");
    }
}
