/// Network speed monitoring: whole-machine speed / today's totals (real) +
/// session / non-session upload breakdown.
///
/// ## Layered methodology (why network bytes cannot be measured directly per process)
///
/// 2026-09-18 local experiments (see docs/key-rules.md #15):
/// - **Winsock send/receive bytes do not appear in any `GetProcessIoCounters`
///   counter** (during a 20MB download, Read was only 7.8KB and the 12MB of
///   Write was the on-disk mirror) — process IO counters only cover files/pipes/
///   devices, so liveio's streaming speed measurement is naturally immune to
///   network pollution;
/// - **TCP ESTATS (`SetPerTcpConnectionEStats`) is broken**: returns
///   ERROR_NOT_SUPPORTED for all connections (including this process's own),
///   even as administrator;
/// - ETW kernel network events require administrator.
///
/// => Without administrator, neither platform exposes a public primitive for
/// "network bytes sent/received per process". This module honestly layers
/// things across three tiers:
///
/// 1. **Whole-machine upload/download (real values)**: sum of interface
///    counters (Windows `GetIfTable` 32-bit octets with modular differencing;
///    macOS `getifaddrs` ifi_*bytes), both excluding loopback. Speed = ~1s
///    sliding-window differencing (aligned with Task Manager's ~1s refresh
///    cadence); today's cumulative totals are persisted across restarts
///    (`speed-panel-net.json`).
///    Note: if the machine goes through a local proxy (ZCode → 127.0.0.1
///    proxy process → internet), the whole-machine figure includes the proxy
///    tunnel's encryption overhead and mixes in other applications' traffic.
/// 2. **Upload composition breakdown**:
///    - **Session traffic (estimated ≈)**: usage library token counts × byte
///      coefficients (API conversation traffic carried by the CLI process;
///      request body ≈ input × 5 B/token, streaming response ≈ output × 8
///      B/token — order-of-magnitude reference values, the frontend marks
///      them with ≈);
///    - **Non-session upload (real lower bound)**: polls
///      `~/.zcode/v2/checkpoints/*/state.json`; a change in
///      `lastAcceptedManifestHash` = a snapshot artifact accepted by the
///      server, counted into today by
///      `lastCompressedSize.encryptedSizeBytes` (bytes after encrypted
///      compression); presence of `activeUpload` = upload in progress. When
///      the directory is blocked by an ACL, honestly show "unreadable".
///      The scope is the **panel observation period**: acceptances that
///      happened while not running are backfilled once, by recordedAt, at the
///      day's first start; multiple jumps between two polling ticks are
///      counted by final state (lower bound).
/// 3. **ZCode connection attribution (real values, Windows only)**: the TCP
///    connection table (OWNER_PID) grouped by process — CLI processes whose
///    command line contains `zcode.cjs` = session group (API traffic), the
///    remaining `zcode.exe` (Electron desktop main/renderer/utility
///    processes) = non-session group (snapshot upload, telemetry, etc.); each
///    group shows its ESTABLISHED connection count and remotes. Whole-machine
///    upload speed spike + non-session group connections appearing +
///    activeUpload = the live evidence chain of a snapshot upload.

use chrono::{Datelike, Local};
use serde::Serialize;
use std::collections::{HashMap, VecDeque};
use std::time::{Duration, Instant};

/// Whole-machine speed sliding window (aligned with Task Manager's ~1s refresh
/// cadence; short-window readings jump more than long-window ones — expected)
const NET_WINDOW_MS: i64 = 1_000;
/// Whole-machine sampling ring capacity (~4.5min @700ms)
const NET_RING_CAP: usize = 400;
/// Process-group refresh period (Toolhelp + command line reads, not every tick)
const PROC_REFRESH_EVERY: Duration = Duration::from_secs(5);
/// Checkpoint directory scan period
const CKPT_SCAN_EVERY: Duration = Duration::from_secs(2);
/// Today's cumulative persistence throttling (a forced save also happens on exit)
const NET_SAVE_EVERY: Duration = Duration::from_secs(30);


/// Session upload estimation coefficient (bytes/token): the request body is the
/// **uncached** prompt increment after JSON escaping (measured: with 98% cache
/// hits, whole-machine upload was only tens of KB — the cache-hit prompt
/// portion is not resent); English/code ~4 chars/token + escaping overhead,
/// pick 5. Order-of-magnitude reference value (frontend marks it with ≈)
pub const SESS_UP_BPT: f64 = 5.0;
/// Session download estimation coefficient: SSE event stream density.
/// Calibrated by measurement on 2026-09-18: during streaming, whole-machine
/// download ÷ tokens ≈ 731 B/token (upper bound including other apps'
/// traffic), UI pipeline coefficient bpt≈320 (lower bound); pick 400 in the
/// middle. Order-of-magnitude reference value
pub const SESS_DOWN_BPT: f64 = 400.0;

/// Session traffic estimation (pure function). The upload term uses the
/// **uncached prompt** (input already includes the cache-hit portion, and
/// cache hits are not resent — estimating as a full resend would inflate it
/// by tens of times, as proven by measured whole-machine daily upload of only
/// tens of KB); output = output + thinking tokens
pub fn sess_bytes_est(uncached_input_tokens: u64, output_tokens: u64) -> (u64, u64) {
    (
        (uncached_input_tokens as f64 * SESS_UP_BPT) as u64,
        (output_tokens as f64 * SESS_DOWN_BPT) as u64,
    )
}

/// Workspace live-status list (pure function, testable): state table →
/// snapshot upload record rows; sort = uploading > pending (not accepted) >
/// accepted, within the same state by recorded time descending. No
/// truncation — every workspace must be listable (explicit user request,
/// 2026-09-18)
pub(crate) fn ckpt_rows(states: &HashMap<String, CkptState>) -> Vec<crate::metrics::CkptStat> {
    let mut rows: Vec<crate::metrics::CkptStat> = states
        .iter()
        .map(|(hash, s)| crate::metrics::CkptStat {
            workspace: if s.workspace.is_empty() { "?".into() } else { s.workspace.clone() },
            bytes: s.artifact_bytes,
            recorded_ms: s.recorded_at.unwrap_or(0),
            accepted: s.accepted_hash.is_some(),
            uploading: s.uploading,
            // Subdirectory name = workspace hash; the frontend's "open
            // directory" builds the path from it
            hash: Some(hash.clone()),
        })
        .collect();
    rows.sort_by(|a, b| {
        b.uploading
            .cmp(&a.uploading)
            .then(a.accepted.cmp(&b.accepted))
            .then(b.recorded_ms.cmp(&a.recorded_ms))
    });
    rows
}

/// Scan the checkpoints directory → (status, observation list). Shared by
/// NetIo's per-tick observation and snapshot_guard's apply archiving (archive
/// first, then clear — key-rules #16)
pub(crate) fn scan_ckpt_states(base: &std::path::Path) -> (String, Vec<(String, CkptState)>) {
    let rd = match std::fs::read_dir(base) {
        Ok(rd) => rd,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return ("missing".into(), Vec::new()),
        // Permission denied (e.g. ACL lockdown) or other errors: honestly
        // report blocked
        Err(_) => return ("blocked".into(), Vec::new()),
    };
    let mut obs = Vec::new();
    for e in rd.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        if let Ok(text) = std::fs::read_to_string(e.path().join("state.json")) {
            if let Some(st) = parse_ckpt_state(&text) {
                obs.push((name, st));
            }
        }
    }
    ("ok".into(), obs)
}

/// Interface counter differencing (pure function, testable): when wrap > 0,
/// do wrap-around differencing modulo the modulus (Windows 32-bit octets);
/// when wrap = 0, plain differencing (mac 64-bit) with regressions clamped
/// to 0 (counter reset). A single-interface single-tick increment over 2^31
/// is treated as anomalous (reset/index reuse) and clamped to 0 to prevent
/// fake traffic
pub(crate) fn wrap_delta(new: u64, old: u64, wrap: u64) -> u64 {
    let d = if wrap > 0 {
        ((new as i64 - old as i64).rem_euclid(wrap as i64)) as u64
    } else {
        new.saturating_sub(old)
    };
    if d >= (1u64 << 31) {
        0
    } else {
        d
    }
}

/// Network monitoring snapshot produced each tick (build_payload fills it
/// into the Snapshot pushed to the frontend)
#[derive(Clone, Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct NetNow {
    /// Whether whole-machine interface counters are available (false on stub
    /// platforms)
    pub available: bool,
    pub up_bps: f64,
    pub down_bps: f64,
    /// Whole-machine today's cumulative totals (real; continued across
    /// restarts via persistence)
    pub up_today: u64,
    pub down_today: u64,
    /// Whether connection attribution is available (Windows only)
    pub conns_available: bool,
    /// Deduplicated count of ESTABLISHED remotes in the session group (CLI
    /// processes)
    pub cli_conns: u32,
    /// Deduplicated count of remotes in the non-session group (Electron
    /// desktop processes)
    pub app_conns: u32,
    /// Connection details for both groups (remote + owning pid + process
    /// type label, deduplicated and sorted by remote+pid; the tooltip shows
    /// each entry as "which process connected where")
    pub cli_conn_list: Vec<crate::metrics::ConnStat>,
    pub app_conn_list: Vec<crate::metrics::ConnStat>,
    /// Checkpoints directory status: ok / missing (no directory) / blocked
    /// (unreadable, e.g. ACL lockdown)
    pub ckpt_status: String,
    /// An activeUpload is in progress
    pub ckpt_uploading: bool,
    /// Snapshot artifact bytes accepted today (after encrypted compression;
    /// lower bound for the panel observation period)
    pub ckpt_today_bytes: u64,
    pub ckpt_today_count: u32,
    /// List of artifacts accepted today (time/workspace/size — answers "which
    /// ones")
    pub ckpt_today_list: Vec<crate::metrics::CkptStat>,
    /// Snapshot upload records (live status of the most recent artifact per
    /// workspace, **no row-count cap** — all listed, the frontend list
    /// scrolls within a height cap; uploading > pending > accepted, within
    /// the same state by recorded time descending)
    pub ckpt_list: Vec<crate::metrics::CkptStat>,
}

// ============ Platform primitives (win / mac / stub — unified externally as netio::platform::*) ============

pub mod platform {
    /// Windows: GetIfTable sums interface octets (32-bit counters; the caller
    /// does modular differencing); GetExtendedTcpTable (OWNER_PID) enumerates
    /// v4+v6 connections grouped by process; Toolhelp + PEB command line
    /// distinguishes the CLI (zcode.cjs) from the Electron desktop app.
    /// Process/command-line identification criteria match
    /// liveio::platform::win (two independent implementations: liveio only
    /// discovers CLI processes, while here we also need the "remaining
    /// zcode.exe" for the non-session group)
    #[cfg(windows)]
    mod win {
        use std::collections::{HashMap, HashSet};
        use std::ffi::c_void;

        #[link(name = "kernel32")]
        extern "system" {
            fn OpenProcess(access: u32, inherit: i32, pid: u32) -> *mut c_void;
            fn CloseHandle(h: *mut c_void) -> i32;
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
        #[link(name = "iphlpapi")]
        extern "system" {
            fn GetIfTable(table: *mut c_void, size: *mut u32, order: i32) -> u32;
            fn GetExtendedTcpTable(
                table: *mut c_void,
                size: *mut u32,
                order: i32,
                family: u32,
                class: u32,
                reserved: u32,
            ) -> u32;
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

        const PROCESS_QUERY_LIMITED: u32 = 0x1410;
        const TH32CS_SNAPPROCESS: u32 = 2;

        /// MIB_IFROW mirror (layout unchanged since NT4; key field offsets
        /// pinned at compile time). Only dwType/dwInOctets/dwOutOctets are
        /// used, but the table layout requires the full sizeof
        #[repr(C)]
        struct MibIfRow {
            wsz_name: [u16; 256],
            dw_index: u32,
            dw_type: u32,
            dw_mtu: u32,
            dw_speed: u32,
            dw_phys_addr_len: u32,
            b_phys_addr: [u8; 8],
            dw_admin_status: u32,
            dw_oper_status: u32,
            dw_last_change: u32,
            dw_in_octets: u32,
            dw_out_octets: u32,
            dw_in_ucast_pkts: u32,
            dw_in_nucast_pkts: u32,
            dw_in_discards: u32,
            dw_in_errors: u32,
            dw_in_unknown_protos: u32,
            dw_out_ucast_pkts: u32,
            dw_out_nucast_pkts: u32,
            dw_out_discards: u32,
            dw_out_errors: u32,
            dw_out_qlen: u32,
            dw_descr_len: u32,
            b_descr: [u8; 256],
        }

        const _: () = {
            assert!(std::mem::offset_of!(MibIfRow, dw_type) == 516);
            assert!(std::mem::offset_of!(MibIfRow, dw_in_octets) == 552);
            assert!(std::mem::offset_of!(MibIfRow, dw_out_octets) == 556);
            assert!(std::mem::size_of::<MibIfRow>() == 860);
        };

        /// IF_TYPE_SOFTWARE_LOOPBACK
        const IF_TYPE_LOOPBACK: u32 = 24;

        /// Interface counter wrap-around modulus: dwIn/dwOutOctets are 32-bit;
        /// differencing is done per interface modulo 2^32
        pub const NET_COUNTER_WRAP: u64 = 1 << 32;

        /// Counter rows for all non-loopback interfaces: (interface index,
        /// cumulative upload, cumulative download). Wrap-around correction is
        /// done by the caller via per-interface differencing (each interface
        /// wraps at a different time; summing first and then differencing
        /// would be wrong)
        pub fn net_ifaces() -> Option<Vec<(String, u64, u64)>> {
            unsafe {
                let mut size = 0u32;
                if GetIfTable(std::ptr::null_mut(), &mut size, 0) != 122 || size == 0 {
                    return None;
                }
                let mut buf = vec![0u8; size as usize];
                if GetIfTable(buf.as_mut_ptr().cast(), &mut size, 0) != 0 {
                    return None;
                }
                let n = buf.as_ptr().cast::<u32>().read_unaligned() as usize;
                let rows = buf.as_ptr().add(4).cast::<MibIfRow>();
                let mut out = Vec::with_capacity(n);
                for i in 0..n {
                    let r = rows.add(i).read_unaligned();
                    if r.dw_type == IF_TYPE_LOOPBACK {
                        continue;
                    }
                    out.push((r.dw_index.to_string(), r.dw_out_octets as u64, r.dw_in_octets as u64));
                }
                Some(out)
            }
        }

        #[repr(C)]
        #[derive(Clone, Copy)]
        struct TcpRow {
            state: u32,
            local_addr: u32,
            local_port: u32,
            remote_addr: u32,
            remote_port: u32,
            pid: u32,
        }

        #[repr(C)]
        #[derive(Clone, Copy)]
        struct Tcp6Row {
            state: u32,
            local_addr: [u8; 16],
            local_scope: u32,
            local_port: u32,
            remote_addr: [u8; 16],
            remote_scope: u32,
            remote_port: u32,
            pid: u32,
        }

        const TCP_TABLE_OWNER_PID_ALL: u32 = 5;
        const AF_INET: u32 = 2;
        const AF_INET6: u32 = 23;
        const MIB_TCP_STATE_ESTAB: u32 = 5;

        fn port(p: u32) -> u16 {
            ((p & 0xff) << 8 | (p >> 8) & 0xff) as u16
        }

        fn ipv4(v: u32) -> String {
            format!("{}.{}.{}.{}", v & 0xff, (v >> 8) & 0xff, (v >> 16) & 0xff, (v >> 24) & 0xff)
        }

        /// Uncompressed IPv6 text (::ffff: mapped addresses are also shown in
        /// full form — tooltip use only)
        fn ipv6(b: &[u8; 16]) -> String {
            let mut s = String::new();
            for i in 0..8 {
                if i > 0 {
                    s.push(':');
                }
                s.push_str(&format!("{:02x}{:02x}", b[i * 2], b[i * 2 + 1]));
            }
            s
        }

        /// Command line → process type label (the Electron shell's --type
        /// argument distinguishes the sub-processes; the CLI check comes
        /// first — zcode.cjs never appears in a renderer process command
        /// line)
        pub(crate) fn proc_label(cmd: &str) -> &'static str {
            if cmd.contains("zcode.cjs") {
                "CLI session process"
            } else if cmd.contains("crashpad") {
                "Crash reporter process"
            } else if cmd.contains("--type=renderer") {
                "Renderer process"
            } else if cmd.contains("--type=gpu-process") {
                "GPU process"
            } else if cmd.contains("--type=utility") {
                "Utility process"
            } else {
                "Main process"
            }
        }

        /// ESTABLISHED connections of ZCode-related processes (remote, owning
        /// pid), returned as two groups (cli_pids, app_pids) (each deduplicated
        /// by remote+pid and sorted). Returns None when the connection table
        /// cannot be read (connection attribution unavailable)
        pub fn zcode_conns(
            cli_pids: &HashMap<u32, String>,
            app_pids: &HashMap<u32, String>,
        ) -> Option<(Vec<(String, u32)>, Vec<(String, u32)>)> {
            let mut cli: HashSet<(String, u32)> = HashSet::new();
            let mut app: HashSet<(String, u32)> = HashSet::new();
            unsafe {
                let mut size = 0u32;
                GetExtendedTcpTable(std::ptr::null_mut(), &mut size, 0, AF_INET, TCP_TABLE_OWNER_PID_ALL, 0);
                if size > 0 {
                    let mut buf = vec![0u8; size as usize];
                    if GetExtendedTcpTable(buf.as_mut_ptr().cast(), &mut size, 0, AF_INET, TCP_TABLE_OWNER_PID_ALL, 0) == 0 {
                        let n = buf.as_ptr().cast::<u32>().read_unaligned() as usize;
                        let rows = buf.as_ptr().add(4).cast::<TcpRow>();
                        for i in 0..n {
                            let r = rows.add(i).read_unaligned();
                            if r.state != MIB_TCP_STATE_ESTAB {
                                continue;
                            }
                            let target = if cli_pids.contains_key(&r.pid) {
                                &mut cli
                            } else if app_pids.contains_key(&r.pid) {
                                &mut app
                            } else {
                                continue;
                            };
                            target.insert((format!("{}:{}", ipv4(r.remote_addr), port(r.remote_port)), r.pid));
                        }
                    }
                }
                let mut size6 = 0u32;
                GetExtendedTcpTable(std::ptr::null_mut(), &mut size6, 0, AF_INET6, TCP_TABLE_OWNER_PID_ALL, 0);
                if size6 > 0 {
                    let mut buf = vec![0u8; size6 as usize];
                    if GetExtendedTcpTable(buf.as_mut_ptr().cast(), &mut size6, 0, AF_INET6, TCP_TABLE_OWNER_PID_ALL, 0) == 0 {
                        let n = buf.as_ptr().cast::<u32>().read_unaligned() as usize;
                        let rows = buf.as_ptr().add(4).cast::<Tcp6Row>();
                        for i in 0..n {
                            let r = rows.add(i).read_unaligned();
                            if r.state != MIB_TCP_STATE_ESTAB {
                                continue;
                            }
                            let target = if cli_pids.contains_key(&r.pid) {
                                &mut cli
                            } else if app_pids.contains_key(&r.pid) {
                                &mut app
                            } else {
                                continue;
                            };
                            target.insert((format!("[{}]:{}", ipv6(&r.remote_addr), port(r.remote_port)), r.pid));
                        }
                    }
                }
            }
            let sort = |s: HashSet<(String, u32)>| {
                let mut v: Vec<(String, u32)> = s.into_iter().collect();
                v.sort();
                v
            };
            Some((sort(cli), sort(app)))
        }

        /// Same PEB → ProcessParameters → CommandLine (UNICODE_STRING @ 0x70)
        /// read chain as liveio::platform::win
        fn process_command_line(pid: u32) -> Option<String> {
            unsafe {
                let h = OpenProcess(PROCESS_QUERY_LIMITED, 0, pid);
                if h.is_null() {
                    return None;
                }
                let rd = |addr: usize, buf: &mut [u8]| -> bool {
                    let mut n = 0usize;
                    ReadProcessMemory(h, addr as *const c_void, buf.as_mut_ptr().cast(), buf.len(), &mut n) != 0
                };
                let read_cmdline = || -> Option<String> {
                    let mut pbi = [0u8; 48];
                    let mut ret: u32 = 0;
                    if NtQueryInformationProcess(h, 0, pbi.as_mut_ptr().cast(), 48, &mut ret) != 0 {
                        return None;
                    }
                    #[cfg(target_pointer_width = "64")]
                    {
                        let peb = usize::from_ne_bytes(pbi[8..16].try_into().ok()?);
                        if peb == 0 {
                            return None;
                        }
                        let mut pp_ptr = [0u8; 8];
                        if !rd(peb + 0x20, &mut pp_ptr) {
                            return None;
                        }
                        let pp = usize::from_ne_bytes(pp_ptr.try_into().ok()?);
                        if pp == 0 {
                            return None;
                        }
                        let mut us = [0u8; 16];
                        if !rd(pp + 0x70, &mut us) {
                            return None;
                        }
                        let len = u16::from_ne_bytes([us[0], us[1]]) as usize;
                        let buf_ptr = usize::from_ne_bytes(us[8..16].try_into().ok()?);
                        if len == 0 || buf_ptr == 0 {
                            return None;
                        }
                        let mut wbuf = vec![0u8; len];
                        if !rd(buf_ptr, &mut wbuf) {
                            return None;
                        }
                        let u16s: Vec<u16> = wbuf
                            .chunks_exact(2)
                            .map(|c| u16::from_ne_bytes([c[0], c[1]]))
                            .collect();
                        Some(String::from_utf16_lossy(&u16s))
                    }
                    #[cfg(not(target_pointer_width = "64"))]
                    {
                        None
                    }
                };
                let out = read_cmdline();
                CloseHandle(h);
                out
            }
        }

        /// Discover ZCode processes and group them (pid + process type label):
        /// (CLI processes = session group, remaining zcode.exe = desktop app
        /// group). CLI = exe name zcode.exe (case-insensitive) whose command
        /// line contains zcode.cjs; zcode.exe without zcode.cjs = the
        /// Electron desktop app (main/renderer/GPU/utility processes — the
        /// carriers of non-session traffic such as snapshot uploads). Both
        /// groups are ZCode's own processes, excluding other applications
        pub fn zcode_pid_groups() -> (Vec<(u32, String)>, Vec<(u32, String)>) {
            let mut cli = Vec::new();
            let mut app = Vec::new();
            unsafe {
                let snap = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
                if snap == -1 {
                    return (cli, app);
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
                if Process32FirstW(snap, &mut entry) != 0 {
                    loop {
                        let exe = String::from_utf16_lossy(
                            &entry.exe_file[..entry.exe_file.iter().position(|c| *c == 0).unwrap_or(260)],
                        );
                        if exe.eq_ignore_ascii_case("zcode.exe") {
                            match process_command_line(entry.process_id) {
                                Some(cmd) => {
                                    let label = proc_label(&cmd).to_string();
                                    if cmd.contains("zcode.cjs") {
                                        cli.push((entry.process_id, label));
                                    } else {
                                        app.push((entry.process_id, label));
                                    }
                                }
                                None => {} // Command line unreadable (permissions/race): not counted in any group
                            }
                        }
                        if Process32NextW(snap, &mut entry) == 0 {
                            break;
                        }
                    }
                }
                CloseHandle(snap as *mut c_void);
            }
            (cli, app)
        }
    }

    /// macOS: getifaddrs sums interface ifi_obytes/ifi_ibytes (64-bit,
    /// excluding lo0). Connection attribution (grouping TCP connections by
    /// process) is not implemented on mac — the benefit is concentrated on
    /// the Windows desktop (the forensic chain for snapshot upload
    /// monitoring); the mac panel honestly shows "connection details are
    /// Windows-only"
    #[cfg(target_os = "macos")]
    mod mac {
        use std::collections::{HashMap, HashSet};
        use std::ffi::{c_char, c_int, c_void};

        #[link(name = "System")]
        extern "C" {
            fn getifaddrs(ptr: *mut *mut IfAddrs) -> c_int;
            fn freeifaddrs(ptr: *mut IfAddrs);
        }

        /// struct ifaddrs mirror (flags is 4 bytes; the pointers after it
        /// require 8-byte alignment, hence the padding)
        #[repr(C)]
        struct IfAddrs {
            next: *mut IfAddrs,
            name: *const c_char,
            flags: u32,
            pad: u32,
            addr: *mut c_void,
            netmask: *mut c_void,
            dstaddr: *mut c_void,
            data: *mut c_void,
            spare: *mut c_void,
        }

        /// struct if_data64 (macOS 64-bit) mirror: ifi_ibytes=64 /
        /// ifi_obytes=72, checked against the xnu SDK net/if.h and pinned
        /// with assertions; an SDK layout change fails compilation directly —
        /// do not remove the assertions
        #[repr(C)]
        struct IfData64 {
            ifi_type: u8,
            ifi_typelen: u8,
            ifi_physical: u8,
            ifi_addrlen: u8,
            ifi_hdrlen: u8,
            ifi_recvquota: u8,
            ifi_xmitquota: u8,
            ifi_unused1: u8,
            ifi_mtu: u32,
            ifi_metric: u32,
            ifi_baudrate: u64,
            ifi_ipackets: u64,
            ifi_ierrors: u64,
            ifi_opackets: u64,
            ifi_oerrors: u64,
            ifi_collisions: u64,
            ifi_ibytes: u64,
            ifi_obytes: u64,
        }

        const _: () = {
            assert!(std::mem::offset_of!(IfData64, ifi_ibytes) == 64);
            assert!(std::mem::offset_of!(IfData64, ifi_obytes) == 72);
        };

        /// Interface counter wrap-around modulus: ifi_*bytes are 64-bit and
        /// effectively never wrap (0 = plain differencing)
        pub const NET_COUNTER_WRAP: u64 = 0;

        /// Counter rows for all non-loopback interfaces: (interface name,
        /// cumulative upload, cumulative download). getifaddrs returns
        /// multiple rows per interface by address family, so they must be
        /// deduplicated by interface name (otherwise bytes are doubled);
        /// loopback lo0 is excluded
        pub fn net_ifaces() -> Option<Vec<(String, u64, u64)>> {
            unsafe {
                let mut head: *mut IfAddrs = std::ptr::null_mut();
                if getifaddrs(&mut head) != 0 {
                    return None;
                }
                let mut seen: HashSet<String> = HashSet::new();
                let mut out = Vec::new();
                let mut p = head;
                while !p.is_null() {
                    let ifa = &*p;
                    if !ifa.name.is_null() && !ifa.data.is_null() {
                        let name = std::ffi::CStr::from_ptr(ifa.name).to_string_lossy().into_owned();
                        if name != "lo0" && seen.insert(name.clone()) {
                            let d = &*(ifa.data as *const IfData64);
                            out.push((name, d.ifi_obytes, d.ifi_ibytes));
                        }
                    }
                    p = ifa.next;
                }
                freeifaddrs(head);
                Some(out)
            }
        }

        /// Connection attribution is implemented for Windows only; mac
        /// returns None (the panel shows "unavailable")
        pub fn zcode_conns(
            _cli_pids: &HashMap<u32, String>,
            _app_pids: &HashMap<u32, String>,
        ) -> Option<(Vec<(String, u32)>, Vec<(String, u32)>)> {
            None
        }

        pub fn zcode_pid_groups() -> (Vec<(u32, String)>, Vec<(u32, String)>) {
            (Vec::new(), Vec::new())
        }
    }

    /// Other platforms: interface counters and connection attribution are
    /// both unavailable (the panel shows "not supported")
    #[cfg(not(any(windows, target_os = "macos")))]
    mod stub {
        use std::collections::HashMap;

        pub const NET_COUNTER_WRAP: u64 = 0;

        pub fn net_ifaces() -> Option<Vec<(String, u64, u64)>> {
            None
        }
        pub fn zcode_conns(
            _cli: &HashMap<u32, String>,
            _app: &HashMap<u32, String>,
        ) -> Option<(Vec<(String, u32)>, Vec<(String, u32)>)> {
            None
        }
        pub fn zcode_pid_groups() -> (Vec<(u32, String)>, Vec<(u32, String)>) {
            (Vec::new(), Vec::new())
        }
    }

    #[cfg(windows)]
    pub use win::*;
    #[cfg(target_os = "macos")]
    pub use mac::*;
    #[cfg(not(any(windows, target_os = "macos")))]
    pub use stub::*;
}

// ============ Checkpoint artifact evidence (parsing and differencing are pure functions, testable) ============

/// Summary of a single workspace's checkpoints state.json
#[derive(Clone, Debug, Default, PartialEq)]
pub struct CkptState {
    /// Workspace display name (last segment of workspacePath)
    pub workspace: String,
    /// Byte count of the most recent compressed encrypted artifact
    /// (lastCompressedSize.encryptedSizeBytes)
    pub artifact_bytes: u64,
    /// Manifest hash of the most recent artifact
    pub artifact_hash: Option<String>,
    /// Manifest hash accepted by the server (lastAcceptedManifestHash)
    pub accepted_hash: Option<String>,
    /// Whether an activeUpload is in progress
    pub uploading: bool,
    /// Artifact recorded time (lastCompressedSize.recordedAt, epoch ms)
    pub recorded_at: Option<i64>,
}

/// state.json → summary. Returns None on missing/corrupted fields (that
/// workspace is skipped for this tick)
pub(crate) fn parse_ckpt_state(json: &str) -> Option<CkptState> {
    let v: serde_json::Value = serde_json::from_str(json).ok()?;
    let lc = v.get("lastCompressedSize")?;
    let workspace = v
        .get("workspacePath")
        .and_then(|x| x.as_str())
        .map(|p| {
            p.rsplit(['\\', '/'])
                .find(|s| !s.is_empty())
                .unwrap_or(p)
                .to_string()
        })
        .unwrap_or_default();
    Some(CkptState {
        workspace,
        artifact_bytes: lc.get("encryptedSizeBytes").and_then(|x| x.as_u64()).unwrap_or(0),
        artifact_hash: lc.get("manifestHash").and_then(|x| x.as_str()).map(String::from),
        accepted_hash: v
            .get("lastAcceptedManifestHash")
            .and_then(|x| x.as_str())
            .filter(|s| !s.is_empty())
            .map(String::from),
        uploading: v.get("activeUpload").map_or(false, |x| !x.is_null()),
        recorded_at: lc.get("recordedAt").and_then(|x| x.as_i64()),
    })
}

/// Checkpoint diff event (the caller fills in time and wording)
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct CkptEvent {
    pub kind: &'static str,
    pub workspace: String,
    pub bytes: u64,
    /// The accepted event carries the artifact recorded time (recordedAt; 0
    /// for upload start/end events) — feeds the time column of the "N
    /// artifacts today" list
    pub recorded_ms: i64,
}

/// Apply one round of observations, per workspace (pure function, testable):
/// - `states`: directory name → previous observation (updated in-place to the
///   current observation)
/// - `day_start_ms`: local start of today (for recordedAt attribution)
/// - `count`: true = first scan of a brand-new day (backfills acceptances
///   that already happened today while the panel was not present)
///
/// Acceptance rule: accepted_hash changes to some new value (including
/// first-seen when count) → that artifact's bytes are counted into today
/// (only when recordedAt ≥ start of today — cross-day deduplication holds
/// naturally via this guard: the same hash's recordedAt is always earlier
/// than the start of a new day). Upload start/end only produce events, no
/// counting.
pub(crate) fn apply_ckpt_obs(
    states: &mut HashMap<String, CkptState>,
    obs: Vec<(String, CkptState)>,
    day_start_ms: i64,
    count: bool,
) -> Vec<CkptEvent> {
    let mut events = Vec::new();
    for (key, new) in obs {
        let old = states.insert(key, new.clone());
        let accepted_now = || {
            new.accepted_hash.is_some()
                && new.recorded_at.map_or(true, |t| t >= day_start_ms)
        };
        match old {
            None => {
                // First seen: a brand-new day (count=true) backfills today's
                // recorded acceptances; if the panel already ran today
                // (count=false), only build the baseline
                if count && accepted_now() {
                    events.push(CkptEvent {
                        kind: "accepted",
                        workspace: new.workspace.clone(),
                        bytes: new.artifact_bytes,
                        recorded_ms: new.recorded_at.unwrap_or(0),
                    });
                }
            }
            Some(old) => {
                if old.accepted_hash != new.accepted_hash && accepted_now() {
                    events.push(CkptEvent {
                        kind: "accepted",
                        workspace: new.workspace.clone(),
                        bytes: new.artifact_bytes,
                        recorded_ms: new.recorded_at.unwrap_or(0),
                    });
                }
                if !old.uploading && new.uploading {
                    events.push(CkptEvent {
                        kind: "upload_start",
                        workspace: new.workspace.clone(),
                        bytes: new.artifact_bytes,
                        recorded_ms: 0,
                    });
                } else if old.uploading && !new.uploading {
                    // The end moment does not judge success or failure:
                    // acceptance is determined by the accepted_hash diff
                    events.push(CkptEvent { kind: "upload_end", workspace: new.workspace.clone(), bytes: 0, recorded_ms: 0 });
                }
            }
        }
    }
    events
}

// ============ Main state machine ============

fn local_ymd() -> (i32, u32, u32) {
    let n = Local::now();
    (n.year(), n.month(), n.day())
}

fn ymd_str((y, m, d): (i32, u32, u32)) -> String {
    format!("{y:04}-{m:02}-{d:02}")
}

/// Local start of today (epoch ms). Falls back to now - 24h on failure (a
/// looser guard does not affect correctness: it is only the attribution
/// boundary for backfilled counting)
fn local_day_start_ms() -> i64 {
    use chrono::NaiveTime;
    let now = Local::now();
    now.date_naive()
        .and_time(NaiveTime::MIN)
        .and_local_timezone(now.timezone())
        .single()
        .map(|dt| dt.timestamp_millis())
        .unwrap_or_else(|| now.timestamp_millis() - 86_400_000)
}

fn net_file() -> Option<std::path::PathBuf> {
    crate::metrics::home_dir().map(|h| h.join(".zcode").join("speed-panel-net.json"))
}

pub struct NetIo {
    /// (time ms, unwrapped whole-machine cumulative upload, cumulative
    /// download) — monotonically increasing; speed-window differencing can
    /// subtract directly
    ring: VecDeque<(i64, u64, u64)>,
    /// Per-interface counter snapshot from the previous tick (per-interface
    /// differencing: each interface wraps at a different time; summing first
    /// and then differencing goes wrong once any interface wraps)
    ifaces: HashMap<String, (u64, u64)>,
    acc_up: u64,
    acc_down: u64,
    today_ymd: (i32, u32, u32),
    up_today: u64,
    down_today: u64,
    ckpt_today_bytes: u64,
    ckpt_today_count: u32,
    /// List of artifacts accepted today (workspace/bytes/recorded_ms; cleared
    /// on day change, persisted with speed-panel-net.json) — "N artifacts
    /// today" must be able to show which ones they are
    today_uploads: Vec<crate::metrics::CkptStat>,
    /// pid → process type label ("CLI session process" / "Main process" /
    /// "Renderer process" / …)
    cli_pids: HashMap<u32, String>,
    app_pids: HashMap<u32, String>,
    proc_refresh: Option<Instant>,
    ckpt_scan: Option<Instant>,
    ckpt_states: HashMap<String, CkptState>,
    ckpt_status: String,
    ckpt_uploading: bool,
    last_save: Option<Instant>,
    dirty: bool,
    /// Events pending write to the debug log (drained each tick by main.rs)
    pending_events: VecDeque<serde_json::Value>,
}

impl NetIo {
    pub fn new() -> Self {
        let mut io = NetIo {
            ring: VecDeque::new(),
            ifaces: HashMap::new(),
            acc_up: 0,
            acc_down: 0,
            today_ymd: local_ymd(),
            up_today: 0,
            down_today: 0,
            ckpt_today_bytes: 0,
            ckpt_today_count: 0,
            today_uploads: Vec::new(),
            cli_pids: HashMap::new(),
            app_pids: HashMap::new(),
            proc_refresh: None,
            ckpt_scan: None,
            ckpt_states: HashMap::new(),
            ckpt_status: String::new(),
            ckpt_uploading: false,
            last_save: None,
            dirty: false,
            pending_events: VecDeque::new(),
        };
        let fresh_day = io.load_persisted();
        // Baseline scan: a brand-new day backfills acceptances that "happened
        // today while the panel was absent" (counted into today); if the
        // panel already ran today (today's cumulative total restored), only
        // build the state baseline
        let (status, obs) = io.scan_ckpt();
        io.ckpt_status = status.clone();
        let events = apply_ckpt_obs(&mut io.ckpt_states, obs, local_day_start_ms(), fresh_day);
        io.handle_ckpt_events(events, fresh_day);
        io
    }

    /// Restore today's cumulative totals. Returns whether it is a "brand-new
    /// day" (true = persistence missing / not today)
    fn load_persisted(&mut self) -> bool {
        let Some(path) = net_file() else { return true };
        let Ok(raw) = std::fs::read_to_string(path) else { return true };
        let Ok(v) = serde_json::from_str::<serde_json::Value>(&raw) else {
            eprintln!("[zcode-speed-panel] net cumulative file corrupted, starting from zero");
            return true;
        };
        let day = v.get("day").and_then(|x| x.as_str()).unwrap_or("");
        if day != ymd_str(self.today_ymd) {
            return true; // Yesterday's totals: naturally reset across days
        }
        self.up_today = v.get("up").and_then(|x| x.as_u64()).unwrap_or(0);
        self.down_today = v.get("down").and_then(|x| x.as_u64()).unwrap_or(0);
        self.ckpt_today_bytes = v.get("ckpt").and_then(|x| x.as_u64()).unwrap_or(0);
        self.ckpt_today_count = v.get("ckpt_count").and_then(|x| x.as_u64()).unwrap_or(0) as u32;
        self.today_uploads = v.get("uploads")
            .and_then(|x| x.as_array())
            .map(|arr| {
                arr.iter()
                    .take(100)
                    .map(|u| crate::metrics::CkptStat {
                        workspace: u.get("ws").and_then(|x| x.as_str()).unwrap_or("?").to_string(),
                        bytes: u.get("bytes").and_then(|x| x.as_u64()).unwrap_or(0),
                        recorded_ms: u.get("at").and_then(|x| x.as_i64()).unwrap_or(0),
                        accepted: true,
                        uploading: false,
                        hash: None, // The persisted JSON only stores list fields, no directory name
                    })
                    .collect()
            })
            .unwrap_or_default();
        false
    }

    fn save(&mut self, force: bool) {
        let due = force || self.last_save.map_or(true, |t| t.elapsed() > NET_SAVE_EVERY);
        if !due || !self.dirty {
            return;
        }
        self.last_save = Some(Instant::now());
        self.dirty = false;
        if let Some(path) = net_file() {
            let json = serde_json::json!({
                "day": ymd_str(self.today_ymd),
                "up": self.up_today,
                "down": self.down_today,
                "ckpt": self.ckpt_today_bytes,
                "ckpt_count": self.ckpt_today_count,
                // List of artifacts accepted today (older files lack this key
                // → empty list, only cumulative counts)
                "uploads": self.today_uploads.iter().map(|u| serde_json::json!({
                    "ws": u.workspace, "bytes": u.bytes, "at": u.recorded_ms,
                })).collect::<Vec<_>>(),
            });
            if let Err(e) = std::fs::write(&path, json.to_string()) {
                eprintln!("[zcode-speed-panel] failed to persist net cumulative totals: {e}");
            }
        }
    }

    /// Scan the checkpoints directory. Returns (status, observation list)
    fn scan_ckpt(&self) -> (String, Vec<(String, CkptState)>) {
        let Some(home) = crate::metrics::home_dir() else {
            return ("missing".into(), Vec::new());
        };
        scan_ckpt_states(&home.join(".zcode").join("v2").join("checkpoints"))
    }

    /// Diff events → today's cumulative totals + debug log events (the
    /// workspace live-status list is produced separately from the state table
    /// by `ckpt_rows`; event wording is not duplicated into the UI).
    /// backfill=true means the day's first-scan backfill
    fn handle_ckpt_events(&mut self, events: Vec<CkptEvent>, backfill: bool) {
        for ev in events {
            let ws = if ev.workspace.is_empty() { "?".to_string() } else { ev.workspace.clone() };
            let mb = (ev.bytes as f64 / 1048576.0 * 10.0).round() / 10.0;
            match ev.kind {
                "accepted" => {
                    self.ckpt_today_bytes += ev.bytes;
                    self.ckpt_today_count += 1;
                    self.today_uploads.push(crate::metrics::CkptStat {
                        workspace: ev.workspace.clone(),
                        bytes: ev.bytes,
                        recorded_ms: ev.recorded_ms,
                        accepted: true,
                        uploading: false,
                        hash: None, // Today's list is keyed by workspace, no directory name mixed in
                    });
                    while self.today_uploads.len() > 100 {
                        self.today_uploads.remove(0);
                    }
                    self.dirty = true;
                    self.pending_events.push_back(serde_json::json!({
                        "kind": "net", "ev": "ckpt_accepted", "ws": ws,
                        "mb": mb, "backfill": backfill,
                    }));
                }
                "upload_start" => {
                    self.pending_events.push_back(serde_json::json!({
                        "kind": "net", "ev": "ckpt_upload_start", "ws": ws, "mb": mb,
                    }));
                }
                _ => {
                    self.pending_events.push_back(serde_json::json!({
                        "kind": "net", "ev": "ckpt_upload_end", "ws": ws,
                    }));
                }
            }
        }
    }

    /// Take the events pending write to the debug log
    pub fn take_events(&mut self) -> VecDeque<serde_json::Value> {
        std::mem::take(&mut self.pending_events)
    }

    /// Force persist before exit (called by save_all)
    pub fn save_forced(&mut self) {
        self.dirty = true;
        self.save(true);
    }

    /// Called every tick (poller ~700ms)
    pub fn tick(&mut self, now_ms: i64) -> NetNow {
        // Reset on day change (both whole-machine and artifact totals count
        // today only)
        let ymd = local_ymd();
        if ymd != self.today_ymd {
            self.today_ymd = ymd;
            self.up_today = 0;
            self.down_today = 0;
            self.ckpt_today_bytes = 0;
            self.ckpt_today_count = 0;
            self.today_uploads.clear();
            self.dirty = true;
        }

        // Whole-machine interface counters → per-interface differencing
        // (wrap-around correction, see wrap_delta) → ring + today's totals.
        // The ring stores unwrapped monotonic cumulative values; the speed
        // window subtracts directly
        let available;
        if let Some(rows) = platform::net_ifaces() {
            available = true;
            let (mut du, mut dd) = (0u64, 0u64);
            for (k, up, down) in &rows {
                if let Some(&(ou, od)) = self.ifaces.get(k) {
                    du += wrap_delta(*up, ou, platform::NET_COUNTER_WRAP);
                    dd += wrap_delta(*down, od, platform::NET_COUNTER_WRAP);
                }
            }
            self.ifaces = rows.iter().map(|(k, u, d)| (k.clone(), (*u, *d))).collect();
            self.acc_up += du;
            self.acc_down += dd;
            if du > 0 || dd > 0 {
                self.up_today += du;
                self.down_today += dd;
                self.dirty = true;
            }
            self.ring.push_back((now_ms, self.acc_up, self.acc_down));
            while self.ring.len() > NET_RING_CAP {
                self.ring.pop_front();
            }
        } else {
            available = false;
        }

        // ~1s sliding-window differencing for speed (earliest sample within
        // the window vs latest; cumulative values are monotonic, subtract
        // directly; aligned with Task Manager's ~1s refresh cadence — jumpier
        // readings are expected)
        let (up_bps, down_bps) = {
            let r = &self.ring;
            match (r.front(), r.back()) {
                (Some(&(t0, _, _)), Some(&(t1, u1, d1))) if t1 > t0 => {
                    let from = now_ms - NET_WINDOW_MS;
                    let (bt, bu, bd) = r
                        .iter()
                        .find(|&&(t, _, _)| t >= from)
                        .copied()
                        .unwrap_or((t0, u1, d1));
                    let secs = (t1 - bt).max(1) as f64 / 1000.0;
                    (u1.saturating_sub(bu) as f64 / secs, d1.saturating_sub(bd) as f64 / secs)
                }
                _ => (0.0, 0.0),
            }
        };

        // Process-group refresh (enumerating the connection table each tick
        // is cheap; process + command line scanning happens once per 5s)
        let due = self.proc_refresh.map_or(true, |t| t.elapsed() > PROC_REFRESH_EVERY);
        if due {
            self.proc_refresh = Some(Instant::now());
            let (cli, app) = platform::zcode_pid_groups();
            self.cli_pids = cli.into_iter().collect();
            self.app_pids = app.into_iter().collect();
        }

        // Connection attribution (only the Windows implementation returns
        // Some): each connection is tagged with its owning pid, and ConnStat
        // assembly carries the process type label (one process can have
        // multiple connections)
        let conns = platform::zcode_conns(&self.cli_pids, &self.app_pids);
        let conns_available = conns.is_some();
        let mut cli_conn_list: Vec<crate::metrics::ConnStat> = Vec::new();
        let mut app_conn_list: Vec<crate::metrics::ConnStat> = Vec::new();
        if let Some((cli_raw, app_raw)) = conns {
            for (remote, pid) in cli_raw {
                let proc = self.cli_pids.get(&pid).cloned().unwrap_or_default();
                cli_conn_list.push(crate::metrics::ConnStat { remote, pid, proc });
            }
            for (remote, pid) in app_raw {
                let proc = self.app_pids.get(&pid).cloned().unwrap_or_default();
                app_conn_list.push(crate::metrics::ConnStat { remote, pid, proc });
            }
        }
        let cli_conns = cli_conn_list.len() as u32;
        let app_conns = app_conn_list.len() as u32;

        // Checkpoint scan (2s throttle)
        if self.ckpt_scan.map_or(true, |t| t.elapsed() > CKPT_SCAN_EVERY) {
            self.ckpt_scan = Some(Instant::now());
            let (status, obs) = self.scan_ckpt();
            self.ckpt_status = status;
            let events = apply_ckpt_obs(&mut self.ckpt_states, obs, local_day_start_ms(), false);
            self.ckpt_uploading = self.ckpt_states.values().any(|s| s.uploading);
            if !events.is_empty() {
                self.handle_ckpt_events(events, false);
            }
        }

        self.save(false);

        NetNow {
            available,
            up_bps: up_bps.max(0.0),
            down_bps: down_bps.max(0.0),
            up_today: self.up_today,
            down_today: self.down_today,
            conns_available,
            cli_conns,
            app_conns,
            cli_conn_list,
            app_conn_list,
            ckpt_status: self.ckpt_status.clone(),
            ckpt_uploading: self.ckpt_uploading,
            ckpt_today_bytes: self.ckpt_today_bytes,
            ckpt_today_count: self.ckpt_today_count,
            ckpt_today_list: self.today_uploads.clone(),
            ckpt_list: ckpt_rows(&self.ckpt_states),
        }
    }
}

// ============ Tests ============
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sess_est_scales_with_tokens() {
        // Upload counts the uncached prompt (cache hits are not resent),
        // download counts output tokens × 400
        let (up, down) = sess_bytes_est(1000, 2000);
        assert!((up as f64 - 5000.0).abs() < 1e-6);
        assert!((down as f64 - 800_000.0).abs() < 1e-6);
    }

    #[test]
    fn parse_ckpt_state_fields() {
        let json = r#"{
            "workspacePath": "C:\\Users\\moqiq\\PycharmProjects\\primer_re",
            "lastCompressedSize": {
                "encryptedSizeBytes": 574522944,
                "workspaceSizeBytes": 100,
                "manifestHash": "abc123",
                "recordedAt": 1788290688837
            },
            "lastAcceptedManifestHash": "abc123",
            "activeUpload": null
        }"#;
        let s = parse_ckpt_state(json).expect("should parse successfully");
        assert_eq!(s.workspace, "primer_re");
        assert_eq!(s.artifact_bytes, 574522944);
        assert_eq!(s.artifact_hash.as_deref(), Some("abc123"));
        assert_eq!(s.accepted_hash.as_deref(), Some("abc123"));
        assert!(!s.uploading);
        assert_eq!(s.recorded_at, Some(1788290688837));
        // Corrupted JSON / missing lastCompressedSize → None
        assert!(parse_ckpt_state("{").is_none());
        assert!(parse_ckpt_state(r#"{"workspacePath":"x"}"#).is_none());
        // Non-null activeUpload → uploading; empty lastAcceptedManifestHash
        // is treated as absent
        let json2 = r#"{
            "workspacePath": "/tmp/w",
            "lastCompressedSize": {"encryptedSizeBytes": 5, "manifestHash": "h1"},
            "lastAcceptedManifestHash": "",
            "activeUpload": {"encryptedArtifactPath": "x.enc"}
        }"#;
        let s2 = parse_ckpt_state(json2).expect("should parse successfully");
        assert!(s2.uploading);
        assert_eq!(s2.accepted_hash, None);
    }

    /// Acceptance diffing: count only when accepted_hash changes; recordedAt
    /// earlier than the start of today is not counted (cross-day
    /// deduplication guard); upload start/end only produce events; a
    /// brand-new day's first scan backfills
    #[test]
    fn apply_ckpt_obs_counts_acceptance_diffs() {
        let day_start = 1_000_000i64;
        let mk = |acc: Option<&str>, bytes: u64, rec: i64, up: bool| CkptState {
            workspace: "ws".into(),
            artifact_bytes: bytes,
            artifact_hash: Some("h".into()),
            accepted_hash: acc.map(String::from),
            uploading: up,
            recorded_at: Some(rec),
        };
        let mut states = HashMap::new();
        // Tick 1: baseline (no acceptance)
        let ev = apply_ckpt_obs(&mut states, vec![("a".into(), mk(None, 100, day_start - 10, false))], day_start, false);
        assert!(ev.is_empty());
        // Tick 2: accepted (recordedAt today) → counted + event
        let ev = apply_ckpt_obs(&mut states, vec![("a".into(), mk(Some("h"), 100, day_start + 5, false))], day_start, false);
        assert_eq!(ev, vec![CkptEvent { kind: "accepted", workspace: "ws".into(), bytes: 100, recorded_ms: day_start + 5 }]);
        // Tick 3: no change → no event
        let ev = apply_ckpt_obs(&mut states, vec![("a".into(), mk(Some("h"), 100, day_start + 5, false))], day_start, false);
        assert!(ev.is_empty());
        // Tick 4: new artifact swapped in and accepted → counted once more
        let ev = apply_ckpt_obs(&mut states, vec![("a".into(), mk(Some("h2"), 250, day_start + 9, false))], day_start, false);
        assert_eq!(ev.len(), 1);
        assert_eq!(ev[0].bytes, 250);
        // Upload start/end: events without counting
        let ev = apply_ckpt_obs(&mut states, vec![("a".into(), mk(Some("h2"), 250, day_start + 9, true))], day_start, false);
        assert_eq!(ev, vec![CkptEvent { kind: "upload_start", workspace: "ws".into(), bytes: 250, recorded_ms: 0 }]);
        let ev = apply_ckpt_obs(&mut states, vec![("a".into(), mk(Some("h2"), 250, day_start + 9, false))], day_start, false);
        assert_eq!(ev, vec![CkptEvent { kind: "upload_end", workspace: "ws".into(), bytes: 0, recorded_ms: 0 }]);
        // Yesterday's acceptance (recordedAt < start of today) is not counted
        // — cross-day deduplication
        let mut states2 = HashMap::new();
        let ev = apply_ckpt_obs(&mut states2, vec![("b".into(), mk(Some("old"), 999, day_start - 1, false))], day_start, false);
        assert!(ev.is_empty());
        // A brand-new day's first scan backfills: today's recorded acceptance
        // must be counted (count=true)
        let mut states3 = HashMap::new();
        let ev = apply_ckpt_obs(&mut states3, vec![("c".into(), mk(Some("n"), 42, day_start + 1, false))], day_start, true);
        assert_eq!(ev, vec![CkptEvent { kind: "accepted", workspace: "ws".into(), bytes: 42, recorded_ms: day_start + 1 }]);
        // First-seen when the panel already ran today (count=false) is not
        // backfilled
        let mut states4 = HashMap::new();
        let ev = apply_ckpt_obs(&mut states4, vec![("d".into(), mk(Some("n"), 42, day_start + 1, false))], day_start, false);
        assert!(ev.is_empty());
    }

    /// Interface counter differencing: 32-bit wrap-around modulo yields the
    /// correct increment; regression (reset) clamps to 0; hugely anomalous
    /// increments (≥2^31, index reuse/reset misread as wrap-around) also
    /// clamp to 0
    #[test]
    fn wrap_delta_handles_32bit_wrap_and_resets() {
        const W: u64 = 1 << 32;
        // Normal increment
        assert_eq!(wrap_delta(500, 100, W), 400);
        // Wrap-around: from 2^32−300 across zero to 196; real increment = 300 + 196
        assert_eq!(wrap_delta(196, 4_294_967_296 - 300, W), 496);
        // mac (wrap=0): counter regression (reset) clamps to 0, no fake traffic
        assert_eq!(wrap_delta(100, 500, 0), 0);
        assert_eq!(wrap_delta(900, 500, 0), 400);
        // Counter reset (millions-scale drop to a small value): interpreting
        // it as wrap-around would yield a fake increment ≥2^31, clamp to 0.
        // Note: a reset whose regression is <2^31 is inherently
        // indistinguishable from wrap-around with 32-bit counters
        assert_eq!(wrap_delta(1000, 1_000_000, W), 0);
    }

    /// Process type labels (Windows criteria): the CLI check comes before
    /// --type — zcode.cjs never appears in a renderer process command line,
    /// so the two checks do not conflict
    #[test]
    #[cfg(windows)]
    fn proc_label_by_command_line() {
        use crate::netio::platform::proc_label;
        assert_eq!(proc_label(r#""C:\...\zcode.exe" "C:\...\zcode.cjs" app-server"#), "CLI session process");
        assert_eq!(proc_label(r#""C:\...\ZCode.exe" --type=renderer --field-trial-handle=x"#), "Renderer process");
        assert_eq!(proc_label(r#""C:\...\ZCode.exe" --type=gpu-process"#), "GPU process");
        assert_eq!(proc_label(r#""C:\...\ZCode.exe" --type=utility --utility-sub-type=net"#), "Utility process");
        assert_eq!(proc_label(r#""C:\...\ZCode.exe" --type=crashpad-handler"#), "Crash reporter process");
        assert_eq!(proc_label(r#""C:\...\ZCode.exe" --js-flags=..."#), "Main process");
    }

    /// Snapshot upload record list: uploading > pending > accepted, within
    /// the same state by time descending; no truncation — all workspaces
    /// listed
    #[test]
    fn ckpt_rows_sorted_all_workspaces() {
        let mut st = HashMap::new();
        let mk = |ws: &str, bytes: u64, rec: i64, acc: bool, up: bool| {
            (
                ws.to_string(),
                CkptState {
                    workspace: ws.into(),
                    artifact_bytes: bytes,
                    artifact_hash: Some("h".into()),
                    accepted_hash: acc.then(|| "h".to_string()),
                    uploading: up,
                    recorded_at: Some(rec),
                },
            )
        };
        st.insert(mk("old-acc", 100, 1, true, false).0.clone(), mk("old-acc", 100, 1, true, false).1);
        st.insert(mk("new-acc", 200, 9, true, false).0.clone(), mk("new-acc", 200, 9, true, false).1);
        st.insert(mk("pending", 300, 5, false, false).0.clone(), mk("pending", 300, 5, false, false).1);
        st.insert(mk("flying", 400, 3, false, true).0.clone(), mk("flying", 400, 3, false, true).1);
        let rows = ckpt_rows(&st);
        assert_eq!(
            rows.iter().map(|r| r.workspace.as_str()).collect::<Vec<_>>(),
            vec!["flying", "pending", "new-acc", "old-acc"]
        );
        // No truncation: all 20 workspaces listed
        let mut big = HashMap::new();
        for i in 0..20 {
            let (k, v) = mk(&format!("ws{i}"), 1, i, true, false);
            big.insert(k, v);
        }
        assert_eq!(ckpt_rows(&big).len(), 20);
        // Empty table → empty list
        assert!(ckpt_rows(&HashMap::new()).is_empty());
    }
}
