# ZCode Speed Panel (zcode-speed-panel)

> **English translation fork** of the original [Masterchiefm/zcode-speed-panel](https://github.com/Masterchiefm/zcode-speed-panel). All user-facing text (UI, documentation) has been translated from Chinese to English. The original project, design, and code are credited to [@Masterchiefm](https://github.com/Masterchiefm) and contributors. This fork is maintained for English-speaking users; for the canonical source and latest releases, please refer to the [upstream repository](https://github.com/Masterchiefm/zcode-speed-panel).

[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](LICENSE)
[![Build](https://github.com/Masterchiefm/zcode-speed-panel/actions/workflows/build.yml/badge.svg)](https://github.com/Masterchiefm/zcode-speed-panel/actions/workflows/build.yml)

A Tauri 2 + Rust desktop resident utility (Windows / macOS dual-platform): displays ZCode CLI model output speed and today's token usage in real time, and **monitors ZCode network upload/download** (distinguishing session traffic from snapshot uploads — ZCode silently packages and encrypts the entire workspace for cloud upload in the background; this tool shows you how much it has uploaded). Supports three floating window forms — desktop pet, mini gauge, and speed capsule — and can be minimized to the system tray (menu bar on mac).

<p><img src="app-icon.png" width="96" alt="App icon" /></p>

Two installation methods are recommended:
- **Manual install**: [⬇ Download latest Release](https://github.com/Masterchiefm/zcode-speed-panel/releases/latest)

Or

- **Let ZCode install it automatically**: (recommended for Mac users)

```text
Please read https://github.com/Masterchiefm/zcode-speed-panel on your own and install it for the user. If the network is poor, make good use of a proxy or ghproxy mirror.
```

Copy the prompt above and let ZCode install it for you directly.

## Screenshots

Full panel: three gauge sections for current speed / today's average / today's total + **network speed monitoring** (whole-machine speed, session upload estimate) + output speed curve (15 min / 1 hour / 6 hours / 24 hours four ranges selectable) + status bar (the current speed arc changes color by speed tier: 0–40 green / 40–80 yellow-green / 80–160 yellow / 160–240 orange / 240–320 red / 320+ magenta; the "Last call" small ring at the top-right of the current speed card shows the speed of the most recently completed call; the "Peak" small ring at the top-left and the "7d Avg" small ring at the top-right of the today's average card show the last-7-days highest per-call speed and last-7-days average speed, sliding expiry by local calendar day); the **Snapshot Protection & Upload Log** card is hidden by default and can be enabled in the top-bar ⚙ settings.

![Main UI](docs/images/main-ui.png)

Two floating window forms (the whole block is draggable, right-click menu to restore/quit):

| Desktop pet (hover bubble: real-time speed / last-call avg speed) | Mini gauge (arc changes color by speed tier) |
|---|---|
| ![Pet](docs/images/pet.png) | ![Mini gauge](docs/images/mini-gauge.png) |

Real-world scenario: ZCode desktop client working, the desktop pet floating beside the window, the bubble showing real-time speed and last-call avg speed in real time (t/s color changes by six tiers):

![Pet in action](docs/images/pet-in-action.png)

## Download & Install

**Recommended: use the pre-packaged files from Release** (Windows: two files per version, choose as needed; macOS: choose one dmg by machine type)

1. Open the [latest Release](https://github.com/Masterchiefm/zcode-speed-panel/releases/latest) and select by system under Assets:

   **Windows 10/11 x64** (requires WebView2 runtime, included with Win11):
   - `zcode-speed-panel_x.y.z_x64-setup.exe` — **installer** (recommended): double-click and follow the wizard to install, launch from the Start menu, overwrites previous versions for upgrade;
   - `zcode-speed-panel_x.y.z_x64-portable.exe` — **portable**: place in any directory and double-click to run, writes no registry entries, creates no Start menu shortcuts, delete the file to uninstall.

   **macOS 10.15+** (separate packages per architecture, no universal binary):
   - `zcode-speed-panel_x.y.z_x64.dmg` — Mac with Intel chip;
   - `zcode-speed-panel_x.y.z_aarch64.dmg` — Apple Silicon (M-series, requires macOS 11+).
   - Install: open the dmg, drag `zcode-speed-panel.app` into "Applications".

   > ⚠️ **macOS first launch will be blocked by Gatekeeper**: this project's mac package is **unsigned and notarized** (signing and notarization require a paid Apple Developer account). After dragging into "Applications", double-clicking will show "cannot be opened because Apple cannot check it for malicious software" (some system versions show "is damaged and cannot be opened"). Choose either of these two paths:
   >
   > 1. **Remove the quarantine** (fastest, no re-download): right-click `zcode-speed-panel.app` in "Applications" → **Open → click "Open" once more**. After authorizing once, double-clicking works normally; or run `xattr -cr /Applications/zcode-speed-panel.app` in Terminal to clear the quarantine flag for this app only (affects only this one application, does not change system security settings).
   > 2. **Build from source** (bypass Gatekeeper without touching its controls): follow [Development & Build](#development--build) to run `npm run tauri build` locally. A package you build yourself has no quarantine flag — install it and double-click to run. Choose this path if you don't want to relax any security settings on the downloaded package.

2. All versions for the same system have identical features; data is stored under `~/.zcode/` and they can coexist.

**macOS quit behavior**: In floating window / desktop pet form, the app **lives in the menu bar** (no Dock icon); in full panel form, it shows a Dock icon (Regular app identity, green traffic light supports **native fullscreen**); collapsing to a floating window auto-hides the Dock icon and does not quit the app. Clicking the window ✕ or pressing `Cmd+Q` **collapses to a floating window** rather than quitting — the only real quit paths are: **menu bar icon right-click → Quit**, or **floating window right-click → Quit**. If you can't find the window, click the app icon in the menu bar at the top-right of the screen to bring up the panel.

Other methods:

- **Download artifacts from a specific commit**: go to the [Actions](https://github.com/Masterchiefm/zcode-speed-panel/actions) page and select a successful Build run; download `windows` (contains both installer and portable exe) or `macos-x86_64-apple-darwin` / `macos-aarch64-apple-darwin` (each contains one dmg) from Artifacts to try the latest unreleased changes;
- **Build from source**: see [Development & Build](#development--build) below.

> Runtime environment: Windows 10/11 x64 (WebView2, included with Win11) or macOS 10.15+ (Intel) / 11+ (Apple Silicon). All data is read only from local files and processes; nothing is uploaded.

## 🙏 Seeking Desktop Pet Assets: Mass Spectrometer Girl

The current pet roster is too thin — we'd especially love to see **Mass Spectrometer Girl** join! If you have desktop pet assets of Mass Spectrometer Girl (or are willing to draw a set), **please contribute**: leave a message on the [call-for-assets issue](https://github.com/Masterchiefm/zcode-speed-panel/issues/1), or open a PR directly. The format is simple — one `pet.json` + one `spritesheet.webp` (refer to the existing pet packages in [`public/pets/`](public/pets/)). No coding skills needed; once assets are received we'll handle integration, and contributors will be acknowledged in the README and Release notes.

## Features

- **Floating window mode** (click the highlighted "⧉ Collapse to floating window" at the top-right, or click the window close button directly)
  - Three styles selectable (dropdown at the top-right of the full panel): **Desktop pet** (default: whale maid, sprite area 200×200 with an additional bubble reserve band above; **scroll wheel up/down to scale** 100~480; double-click or 🔄 to switch pet; idle standing; **during generation, actions switch by real-time speed tier**: 0–40 t/s trotting, 40–80 t/s walking, 80–240 t/s jumping, 240+ t/s sprinting left and right rapidly; during generation/estimation, the bubble above shows real-time speed, **hover to expand to multi-line: real-time speed / last-call avg speed** — the bubble grows upward above the pet, pet does not shrink or get occluded; **during concurrent multi-tasking the bubble auto-expands to per-task rows** (one row per CLI process, color-coded by tier, window auto-grows upward to make room for rows), bubble hidden during idle; right-click menu or the **"Always show last-call avg speed" toggle** in the full panel top bar (appears when desktop pet style is active) — when checked, the bubble always shows multi-line during generation (real-time speed + last-call avg speed together, no hover needed), bubble still hidden during idle) / **Mini gauge** (148×118, main ring with "last call" small ring beside it, symmetric top/bottom margins, arc readout scale consistent with the full panel current speed gauge) / **Speed capsule** (172×72, real-time speed on top, one line of "last-call avg speed" below)
  - Drag by holding anywhere; click ⤢, **double-click the floating window** (except desktop pet — double-click is used to switch pet) or **right-click menu → Restore window** to expand back to the full panel; right-click menu also allows **Quit** (in desktop pet style, the first menu item is the "Always show last-call avg speed" checkbox)
  - Desktop pet position and full panel position are **remembered independently**: when collapsed, the pet returns to its own last position (first time anchors to window center); when expanded, the window returns to its own old position — they do not interfere with each other
  - Mode, style, both positions, and pet size are all remembered and restored on next launch
- **Output speed curve** (time range dropdown selectable: **15 min** (default, 10s per bucket) / 1 hour (40s per bucket) / 6 hours (4 min per bucket) / 24 hours (16 min per bucket); selection persists across restarts; x-axis labeled with **real wall-clock times**, tick marks spaced by tier, the whole curve continuously shifts left over time, directly verifiable against real events; the 15 min tier uses in-memory aggregation; longer tiers compute on the fly from read-only queries to the ZCode usage database; the real-time speed is also blended into the latest bucket)
- **Model speed trend**: the **segmented toggle** at the top-right of the chart card ("Overall Curve / Model Details" text on left/right, color block slides to the active side) switches between the two views exclusively (selection persists across restarts; the model view is also embedded in the card, x-axis shares the same real wall-clock time and integer-minute ticks as the overall curve, translates over time without distortion), showing per-model speed line charts and statistics (avg speed / peak / call count / token share); **statistics range shares the same dropdown as the overall curve** (15 min / 1 hour / 6 hours / 24 hours, 90 same-width buckets, switching the toggle does not change the range, auto-refreshes every 5s); **legend supports multi-click selection** — hide interfering models (line chart and stats rows filtered in sync, at least one must remain), row-end "Select all / Top 3 only" (top three by token usage) shortcuts, selection persists across refreshes; computes on the fly from read-only queries to the ZCode usage database, writes no local data
- **Speed tier color coding**: the current speed gauge's arc and readout change color overall by speed — 0–40 t/s green, 40–80 yellow-green, 80–160 yellow, 160–240 orange, 240–320 red, 320+ magenta, speed known at a glance (some ultra-fast models far exceed 100 t/s, all six tiers covered); background track is always gray, floating window mini gauge and "last call" small ring follow the same rules
- **Real-time readout recalibration**: the **⟳** button at the top-left of the current speed card triggers manual recalibration at any time — discards learned byte→token coefficient samples and returns to the default prior to reconverge (use when real-time readings deviate significantly from the "last call" true value after switching models; button flashes ✓ to confirm). **Also triggers automatically**: takes the displayed speed mean per call; when the last-call mean differs from the previous consecutive 5-call means by ≥3× (either direction), it judges a speed order-of-magnitude shift (switched model/tokenizer, old coefficient expired) and auto-recalibrates
- **"Last call" mini-gauge** (top-right of the full panel current speed card, beside the main ring of the mini gauge floating window): shows the speed of the **most recently completed call** (on-disk basis: output + reasoning ÷ pure generation duration), serving as a cross-reference for real-time readings — when real-time measurement is affected by pipeline silence, compare against the last call's actual rate to judge whether the current reading is trustworthy; the capsule floating window and desktop pet hover bubble also show this value; both follow the same six-tier color change, hover shows basis description
- **"Peak / 7d Avg" mini-gauges** (top-left / top-right of the full panel today's average card): **last-7-days** highest **per-call** speed and **last-7-days average** speed (counted by local calendar day including today, sliding expiry at daily midnight, the earliest full day rolls out of the window). The last-7-days peak uses a qualifying basis — valid output ≥300 tokens and pure generation ≥1s (millisecond-scale micro-calls have large timestamp noise; a 24ms/79-token call in the database would calculate a fake 1580 t/s record); last-7-days average = Σ(output + reasoning) ÷ Σ pure generation duration across completed calls in the window (same basis as today's average, no filtering); does a baseline scan of the usage database within the window at startup then incrementally accumulates with each tick, hover shows basis description
- **Borderless window + custom-drawn title bar**: title bar matches the app style, hold to drag and move, double-click to safely maximize/restore (mac green traffic light is **native fullscreen**, consistent with mac language); **platform-native control buttons** (on macOS, located at the far left of the title bar as native red/yellow/green traffic light dots, symbols faintly appear on hover; on Windows, keeps the right-side `— ▢ ✕` custom-drawn controls); supports **multi-monitor safe maximize**, maximize on a secondary monitor fills it without jumping screens, restore remembers the secondary monitor position; window title bar shows current speed in real time (visible in taskbar / Alt+Tab); tray (menu bar on mac) left-click toggles show/hide, right-click menu has a **real-time status line** at the top (generating x.x t/s / estimating / idle), menu items (Show panel / Hide to tray / Floating window toggle / Quit); relaunch auto-raises the existing window. **mac**: native Overlay title bar + real system traffic lights (green dot native fullscreen, red collapse, yellow minimize); full panel shows Dock icon, floating window auto-hides (menu bar resident), `Cmd+Q` and clicking ✕ both collapse to floating window, each time the window shows it displays "App lives in menu bar" (auto-dismisses after 6 seconds); WebView Cmd+C/V/X/A are preserved by a custom "Edit" menu
- **In-app update**: the version number is shown at the bottom-right of the status bar, click to manually check for updates; the app also **silently** checks once per startup and once per day (no disturbance when no update or network error). When a new version is found, it auto-downloads in the background (bottom-right card shows progress and release notes), click "Update now" to install — Windows auto-launches the installer (app auto-exits for handoff), macOS opens the dmg for you to drag into "Applications"; the card also has a **"Manual download" button** linking directly to the Release page in your browser for manual download; closing with ✕ stops auto-prompting for that version (a small dot beside the version number serves as a reminder).
- **Network speed monitoring** (full panel "Network Speed Monitoring" card): shows real-time **whole-machine upload/download speed** and today's totals (measured via interface counters, **whole-machine = all application traffic on this machine, not just ZCode**, explicitly labeled on the card; ~1s sliding window, in bytes — Task Manager shows bits and only counts the selected NIC, discrepancies are due to basis differences; hover the value for a full explanation); **session traffic (≈ estimated, by token × coefficient)** from today's upload/download is listed separately alongside the whole-machine values for cross-reference; also shows **ZCode connection ownership** (hover to see remote endpoint and **owning process** for each connection): **ZCode session process** (CLI, conversation traffic) and **ZCode desktop client** (ZCode's Electron shell — carrier of non-conversation traffic like snapshot uploads) each hold several external connections — both are ZCode's own processes, no other apps included. A spike in whole-machine upload + a new desktop-client connection + "Snapshot upload in progress" appearing simultaneously on the snapshot protection card is on-scene evidence of another snapshot upload (see `docs/features.md` for basis details; there are no public primitives for per-process direct network byte measurement on Windows/macOS without admin, hence this layered design). Today's network accumulation persists across restarts.
- **Snapshot Protection & Upload Log** (full panel "Snapshot Protection & Upload Log" card — everything snapshot-related is on this card): upper section shows protection toggle and status (see next item); lower section shows **today's snapshot uploads** (monitors upload-accept events in `~/.zcode/v2/checkpoints/`; when ZCode encrypts and uploads the entire workspace to the cloud, the snapshot sizes are faithfully accumulated — **N today, workspace names listed below row by row, hover for per-entry details**, in-progress upload shown with a pulse indicator) and the **snapshot upload record list**: lists uploaded snapshots per workspace (time / workspace / encrypted size / status — uploading, pending, accepted; fixed 5-row display, scroll within the list for the rest, **selectable to copy or one-click export to text file** for record-keeping, row-end 📂 opens the snapshot directory in file manager); when the checkpoints directory is ACL-locked, "unreadable" is shown faithfully.
- **Snapshot protection (blocks silent workspace snapshot upload, macOS / Windows)**: After logging in, ZCode encrypts and uploads the **entire workspace (including `.git` history)** to the cloud; settings toggles are ineffective ([mechanism analysis article](https://blog.ferstar.org/posts/zcode-silent-workspace-snapshot-upload/)). When protection is enabled on the "Snapshot Protection & Upload Log" card, **choose one of two**: **Preserve & Lock** (recommended) — existing snapshots remain in place as read-only (mac recursive immutable lock / Windows directory ACL deny-write), upload records still display normally, row-end 📂 opens the snapshot directory in file manager; **Delete & Lock** — archive a record list first then clear all snapshots (original records disappear, only the list remains for review, confirmation dialog lists every entry explicitly). After locking, ZCode cannot write in, the snapshot upload chain is completely dead; **does not touch the network, model conversation / code completion / tool calls are completely unaffected**, the only loss is ZCode's "checkpoint rollback / timeline" feature (can be reversed at any time: preserved snapshots restored in place, empty directories auto-recreated). During protection, the card distinguishes between "snapshots preserved / deleted" two states and accumulates "N conversations since enabled · 0 new snapshots written"; card hover on the title shows protection principle and monitoring boundary explanation (does not promise 100% interception, direct-upload mechanism relies on traffic-layer detection).
- **Settings: module visibility & ordering** (top-bar ⚙ gear button, "Display Modules" section of the settings dialog): check the modules to show on the full panel and use ↑↓ to adjust their vertical order (list top-to-bottom = panel top-to-bottom, takes effect immediately and persists across restarts). Default display: Gauges / Network Speed Monitoring / Last 15 min Output Speed; **Snapshot Protection & Upload Log is hidden by default** — check to enable when needed; "Restore defaults" resets everything in one click. The concurrent-tasks card follows the gauges (not listed separately); module toggles do not affect the floating window or status bar.
- **Auto-start** (top-bar ⚙ settings dialog "Auto-start" section): choose one of three — **Off** (default) / **Start on boot** (resident after login, displays in the form it was when last exited) / **Follow ZCode startup** (silent standby after login: only tray icon, no window; auto-shows the panel when ZCode is detected running — desktop client or CLI). Windows writes registry `HKCU\…\Run`, macOS writes a LaunchAgent plist in `~/Library/LaunchAgents` — the **registry / plist is the single source of truth** — every time settings open it reads back the real state; manually editing the registry or deleting the plist is faithfully reflected, no local duplicate stored. The panel **will not auto-resurrect after manual quit** (relogin or manual launch restores it).
- Status bar: data source (usage database), today's call count, session count, last activity time

## Data Sources

- **Today's usage & average speed**: read-only polling of the ZCode usage database (`model_usage` table in `~/.zcode/cli/db/db.sqlite`; WAL mode does not affect the running client). Rate definition uses **pure generation duration**: denominator = `completed_at - first_token_at` (excludes first-token wait and queuing), numerator = `output_tokens + reasoning_tokens` (reasoning content is also streamed output). One table covers all sessions (including sub-agents).
- **Real-time speed (30s sliding window, measured in real time, second-level start/stop)**: does not rely on persisted data; directly measured from the CLI process's write byte stream, principles explained in [Real-time Speed Measurement Principles](#real-time-speed-measurement-principles) below. The "generating" state lights up on the same tick a call starts (new conversations also immediate); before the first byte arrives, the gauge / desktop pet shows **"…" (counting)** as a hint rather than an estimated value; end, cancel, or error all reset to zero within 1–2 ticks. **During concurrent multi-tasking (multi-window / sub-agent parallelism) the reading is the aggregate total throughput**; the full panel also shows per-task breakdowns (measured per CLI process, row totals = main gauge reading); the desktop pet bubble also expands per-task rows and the window auto-grows upward. New tasks opened within the same ZCode window reuse the same service process (separate processes per project only), byte-level separation is impossible — such tasks count as one combined row (session count labeled), speed is that process's combined total.
- **Network speed monitoring**: whole-machine speed / today's totals come from OS interface counters (measured); session traffic is estimated from today's token count × byte coefficient (≈ labeled); snapshot uploads come from accept events in `~/.zcode/v2/checkpoints/*/state.json` (real artifact size); ZCode connection ownership comes from process ownership in the TCP connection table (Windows only). Today's network accumulation persists across restarts (in `~/.zcode/speed-panel-net.json`). See the "Network Speed Monitoring" section in `docs/features.md` for details.
- During some calls the UI pipeline has no incremental bytes (measured: roughly half of calls are "flush-on-complete"), in which case it falls back to: gated as generating → show **recent completed call's real speed ≈** (same basis as the speed curve, the two no longer conflict); no calls at all → reset to zero and idle.

## Real-time Speed Measurement Principles

**The problem to solve**: ZCode only persists token counts on disk when a call completes (`model_usage` table). During streaming, there is no incremental data in the database at all — this is a common blind spot for all "post-completion statistics" tools: during long response generation, speed can only show 0 or the previous stale value.

**Key observation**: during model streaming output, the CLI process continuously writes rendering increments to the pipe leading to the desktop UI. Measured process write-byte rates (Windows):

| State | Write rate (per ~700ms tick) |
|---|---|
| Streaming output | 6 ~ 25 KB, fluctuates with generation cadence |
| Idle | only 1 ~ 2 KB heartbeat |
| Call completion instant | hundreds of KB spike (disk flush) |

This counter is maintained by the Windows kernel (`WriteTransferCount` from `GetProcessIoCounters`), authoritative, real-time, zero-overhead to read.

**macOS basis difference** (same cleaning / calibration pipeline, platform-specific parameters): process discovery uses `proc_listallpids` + `KERN_PROCARGS2` (CLI is forked from Electron Helper, must match command-line argument `zcode-cli` precisely); write-byte counter switches to `ri_diskio_byteswritten` from `proc_pid_rusage` (kernel-maintained cumulative process disk-write bytes). On mac, streaming bytes are **single-tick burst-shaped** (0,0,0,+225KB~1.5MB, ≈3900 B/token), unlike Windows's continuous trickle, therefore **burst rejection is disabled**, no static idle floor is set (idle measured strictly 0 bytes), initial coefficient and calibration interval are relaxed based on measurements; rollout-directory disk-write subtraction is turned off on mac (directory net change is often negative — cleanup rotation cannibalizes the cleaning stream, see the CleanParams platform table in `docs/features.md`).

**Measurement cycle (one tick per ~700ms, alongside the main poll)**:

1. **Discover processes**: Toolhelp32 enumerates all processes → filter `ZCode.exe` → read each process's full command line (ProcessBasicInformation → PEB → ProcessParameters → CommandLine), keeping only CLI child processes containing `zcode.cjs` (refreshed every 30s, excludes desktop shell / rendering processes; when no process is found, interval shortens to 2s — a newly launched CLI is observable within at most 2s)
2. **Sample**: read `WriteTransferCount` (cumulative bytes written since process start) for each CLI process, store in a ~3-minute ring buffer per process
3. **Start/stop determination (call gating)**: look at assistant message rows in the usage database message table — they are submitted **at the instant a call starts** (readable within ≤200ms), and the `time.completed` field within the row is **backfilled at the instant the call ends (including cancel/error)**. Scan the latest assistant row of recently active sessions: sessions **without** `completed` are all counted as in-progress (session-unlimited: the first call of a new conversation lights up on the same tick, no need to wait for its first completed row to persist; concurrent multi-tasking counted separately); rows **with** `completed` → reset to zero on the same tick. The usage database's `model_usage` rows only record `status='completed'`; **cancelled/errored calls never have a completed row** — early implementations that used completed rows for start/stop determination would leave such calls stuck at "generating" for up to 10 minutes. Tool execution / idle periods also have UI-state bursts in the pipeline, indistinguishable from model streaming at the byte level; gating reliably excludes them. When all owning processes of an in-progress session exit (crash / terminal close, no one to backfill `completed`), forced stop determination leaves no zombie "generating"
4. **Speed reading (30s sliding window ∩ active segment)**: takes the mean of cleaned bytes within the 30s window, but the left boundary is no earlier than the start of the current streaming segment — at startup, even with only a few ticks there is an immediate reading (status bar shows "counting…"), then the window gradually fills and smooths; pauses within the stream and the disk-flush spike at completion instant are not mixed in
5. **Subtract heartbeat noise**: each process takes 2× its own historical minimum increment as its idle floor for subtraction (adaptive, different idle floor per process), **capped at ~2KB per tick** — without capping, the quantile during sustained streaming gets raised by the streaming increment itself, subtracting its own output as noise (one of the measured culprits that collapses the reading to 1/5 of the true value)
6. **Subtract disk-flush spikes (negative ticks allowed)**: synchronously monitor rollout / log / WAL file size increments and subtract from write bytes. Disk flush and IO counting are misaligned: a single tick might show "file grew 60KB but only wrote 20KB" — **that tick is recorded as negative**, cancelled out by surrounding positive ticks in the interval sum; if clamped to 0 per tick, the misaligned increment is permanently swallowed (the other half of measured loss). Single-tick raw increments exceeding 100KB (request-body upload ~190KB/tick) are rejected entirely, bytes and duration excluded from integration
7. **Bytes → tokens (consistency calibration)**: divide by a **self-calibrating coefficient**. After each call completes, integrate bytes over the `[first_token, completed]` interval proportional to time using **exactly the same cleaning stream as the display path**, divided by the real `output + reasoning tokens` to produce one calibration sample; the median of the most recent 5 samples (including an initial 600 prior) is used as the current coefficient. **The numerator shares the same origin as the display numerator**: any systematic subtraction (noise floor / disk mirror / burst rejection) is automatically cancelled out by the coefficient, and the display value converges to the true t/s. **Calls with output <300 tokens are excluded from samples** — the UI fixed-frame overhead of small calls inflates the coefficient several-fold. The prior sample prevents a single outlier from dominating the coefficient on cold start; the learned sample queue **persists locally** (`~/.zcode/speed-panel-cal.json`, auto-expires after 14 days and reverts to the prior), so restarts reuse it directly instead of reconverging from the prior each time (session cold-start readings with coefficient deviating from prior can differ by 2–3×). The coefficient can be reset to the prior for reconvergence at any time: the current speed card ⟳ button triggers it manually, or auto-triggers when per-call mean drift is detected (last-call mean vs previous consecutive 5-call means ≥3×, either direction) — after switching models, when the old coefficient expires, readings can auto-correct back to the right track

**Startup prompt (counting…)**: when gating is open but the first byte has not yet arrived (TTFT / queuing), the gauge number and desktop pet bubble show **"…"** (a cyan breathing pulse arc hinting "connected, waiting for output"), no misleading estimated value is displayed; calls with no bytes after 20s are treated as pipeline-silent and fall back to the ≈ estimate below.

**Pipeline-silent fallback**: measured, a significant portion of calls (mostly those with empty `first_token_at`) write no increments to the UI pipe during generation; all bytes are flushed in a single burst at completion. For such calls, after the combination of gating (generating) and bytes (silent) is determined, the panel falls back to showing "real speed of recent 10-min completed calls" (≈ mark, exactly the same source as the speed curve), avoiding the contradiction of "chart shows 50, gauge shows 0".

**Concurrent multi-tasking: aggregate by process ownership**: after each call completes, take the process with the most write bytes during the streaming interval and mark it as the CLI process owning that session (with hysteresis: when ownership already exists, only switch if candidate process bytes ≥ 2× current ownership, no per-call flipping between concurrent windows). Real-time speed statistics take the **union** of owning processes of **all in-progress sessions** — when multi-window / sub-agent parallelism is active, the current reading is the real **total throughput**; the full panel also shows a "Concurrent Tasks" card below the gauges (one row per CLI process: owning sessions + respective speeds, total = main gauge reading; multiple sub-agents in parallel within the same window are not separable at the byte level, shown as that process's combined total). In-progress sessions with no ownership record yet (new session / sub-agent's first call not yet completed) trigger a **full-process-sum** fallback, never showing as another window's speed. After all CLIs exit, even if gating signal lingers, immediately reset to zero idle, no blind estimation by call interval. **Stop fallback**: after generation stops, the CLI's "complete" marker write can be delayed by seconds to minutes; if the gating signal lingers but the byte stream has been silent for more than 15 seconds, the panel also judges it as stopped and resets the reading to zero — it will not stay stuck at "generating" or the ≈ estimate.

**Measured effect** (title bar trace):

```
19:56:52  ⏸ 0.0 t/s     ← no task
19:56:53  ▶ … t/s        ← call started (TTFT), gauge / desktop pet shows "…" waiting for first byte
19:56:55  ▶ 18.2 t/s counting ← first byte appeared, responds on the same tick, immediate reading (30s sliding window building)
19:56:58  ▶ 37.5 t/s    ← sliding window fills, reading trends smooth
19:57:21  ▶ 9.5 t/s     ← end-segment cadence slows (truthfully reflected)
19:57:22  ⏸ 0.0 t/s     ← stop determined within 1–2 ticks after halting, immediate reset (cancel / error equally immediate)
```

**Accuracy explanation & reconciliation**: the bytes → tokens conversion is a statistically approximate one (cleaned-stream measurement ~200–900 B/token, fluctuates with UI frame content; consistency calibration makes its long-term integral converge to the true value). Precise token counts are still provided by the persisted record at each call completion, used for today's total and average — the two complement each other. After each call completes, the debug log writes one reconciliation event: `pred_tps =` cleaning-stream interval integral ÷ generation duration ÷ current coefficient, compared against the persisted true value `true_tps` to quantify real-time accuracy:

```bash
python scripts/live_vs_true.py            # real-time vs true value reconciliation (legacy format logs auto-degrade to tick replay)
```

- Today's total token count shares the same basis as ZCode's official statistics = `input + output + reasoning + cache_creation`; **cache hits (cache_read) are prompt reuse and not counted in the total**, shown in details as **cache hit rate** (cache_read ÷ input — the usage database's input is all prompt tokens, the cache-hit portion is already included in it). "Today" is determined by event completion time; auto-resets across days.

> Privacy: All data is read only from local files and processes; nothing is uploaded.

## Development & Build

Requirements: Node.js ≥ 20, Rust stable (Windows requires MSVC toolchain; macOS requires Xcode Command Line Tools), WebView2 runtime (included with Win11).

```bash
npm install
npm run tauri dev      # development & debugging (debug build connects to vite dev server)
npm run tauri build    # release build (frontend embedded + installer: Windows NSIS / macOS dmg)
```

> For mac local builds, if you need to explicitly declare the minimum OS version (CI's approach, consistent with this section's commands): append `--config '{"bundle":{"macOS":{"minimumSystemVersion":"10.15"}}}'` for Intel packages and prefix environment variable `MACOSX_DEPLOYMENT_TARGET=10.15` (the former writes LSMinimumSystemVersion in Info.plist, the latter determines the binary's minimum version; when unspecified, tauri defaults the plist to 10.13).

macOS cross-build Apple Silicon package (possible on an Intel Mac):

```bash
rustup target add aarch64-apple-darwin
MACOSX_DEPLOYMENT_TARGET=11.0 npm run tauri build -- --target aarch64-apple-darwin --bundles dmg --config '{"bundle":{"macOS":{"minimumSystemVersion":"11.0"}}}'
```

Artifact locations:

- Windows: executable `src-tauri/target/release/zcode-speed-panel.exe`, installer `src-tauri/target/release/bundle/nsis/*.exe`
- macOS: `src-tauri/target/release/bundle/macos/zcode-speed-panel.app` and `src-tauri/target/release/bundle/dmg/*.dmg` (cross-build artifacts are under `src-tauri/target/aarch64-apple-darwin/release/bundle/`)

Debug tool (bypasses UI, directly prints the engine's computation results against real data, including IO measurement availability):

```bash
cd src-tauri && cargo run --example dump
```

**Debug log**: while the panel is running, data is continuously appended to `~/.zcode/speed-panel-debug.jsonl` (8MB auto-rotation keeping one generation, **rotated-out old files over 7 days are auto-cleaned at startup**): `tick` (real-time display value / cleaning-pipeline byte rate `pipe` / active coefficient / trailing bucket of the stats chart, every tick during active periods + idle heartbeat every 30s), `call` (real tokens and real speed after each call completes), `cal` (each calibration & reconciliation: true value `true_tps`, raw / cleaned integral bytes `raw_kb`/`clean_kb`, sample coefficient `bpt_sample`, display-basis prediction `pred_tps`), `cal_reset` (manual / drift-auto recalibration: coefficient before/after values and trigger mean). Offline verification tools also record the first three event types:

```bash
cd src-tauri && cargo run --example verify -- 300 target/verify-log.jsonl   # sample for 5 minutes
python scripts/compare3.py src-tauri/target/verify-log.jsonl                # gauge vs stats chart vs true value three-source comparison
```

Unit tests:

```bash
cd src-tauri && cargo test
```

### Auto-build & Release (GitHub Actions)

Ordinary pushes do **not** trigger builds. Two packaging methods: pushing a `v*` tag (e.g. `git tag v0.2.0 && git push --tags`) auto-creates a [Release](https://github.com/Masterchiefm/zcode-speed-panel/releases), with Assets attaching **Windows installer** (`_x64-setup.exe`), **portable** (`_x64-portable.exe`), and **macOS dual-architecture dmg** (`_x64.dmg` = Intel, `_aarch64.dmg` = Apple Silicon, unsigned and unnotarized — see the bypass guide above for first launch); or on the [Actions](https://github.com/Masterchiefm/zcode-speed-panel/actions) page select Build → **Run workflow** (choose `main` branch) to manually trigger, with artifacts in the run's Artifacts (`windows` contains two exes; `macos-x86_64-apple-darwin` / `macos-aarch64-apple-darwin` each contain one dmg). mac minimum OS versions: x64 = 10.15, aarch64 = 11.0. Configuration in [`.github/workflows/build.yml`](.github/workflows/build.yml).

Before releasing, bump the version number in all three places together: `src-tauri/tauri.conf.json` (runtime authority), `src-tauri/Cargo.toml`, `package.json` — the in-app updater matches installer packages by Release asset name suffix; product naming and version number are the protocol it relies on; you cannot change only one place (see `docs/key-rules.md` #12).

## Browser Preview

After `npm run dev`, open <http://localhost:1420> directly in your browser; the page runs with mock data (auto-enters mock mode when a non-Tauri environment is detected), convenient for styling.

## Tech Stack

Tauri 2 (Rust backend: usage database polling + process IO measurement + network interface counters & TCP connection ownership + tray), native Canvas rendering for gauges and desktop pets (no chart library dependency), Vite + TypeScript.

## Acknowledgments

- [zcode-tps-monitor](https://github.com/shy3130/zcode-tps-monitor) (MIT): this project's rate definition draws from its pure generation duration basis (first_token_at start, reasoning tokens counted in numerator)
- [dsh-desk](https://github.com/Renakoni/dsh-desk) (MIT): desktop pet uses its built-in Codex Pet packages (Monthly Salary Cat, Maid-DeepSeek-Whale)

See [THIRD-PARTY-NOTICES.md](THIRD-PARTY-NOTICES.md) for details.

## About This Fork

This is an **English translation fork** maintained by [@TheHandsomeHans](https://github.com/TheHandsomeHans). All original work — concept, architecture, code, design — is by [@Masterchiefm](https://github.com/Masterchiefm). The upstream repository is at [Masterchiefm/zcode-speed-panel](https://github.com/Masterchiefm/zcode-speed-panel).

## License

[MIT](LICENSE)
