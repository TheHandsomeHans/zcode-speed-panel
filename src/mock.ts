// Browser preview mode: simulates ZCode's model-io call stream for UI preview without Tauri

import type { GuardStatus } from "./guard";
import type { ModelStatsPayload } from "./model_stats";

/** Per-task real-time detail (multiple only during multi-task concurrency): one CLI process = one row */
export interface TaskStat {
  pid: number;
  /** Attached in-progress session id (empty = not-yet-attributed streaming process) */
  session: string;
  /** Number of in-progress sessions hosted by this process (≥2 = same-process multi-task, speed is combined) */
  nSessions: number;
  tps: number;
  streaming: boolean;
}

/** Snapshot upload record row (same shape as backend CkptStat) */
export interface CkptStat {
  workspace: string;
  bytes: number;
  recordedMs: number;
  accepted: boolean;
  uploading: boolean;
  /** Workspace subdirectory name under checkpoints (click 📂 to open); archived old rows lack this field */
  hash?: string;
}

/** ZCode connection detail row (same shape as backend ConnStat): both groups are ZCode's own processes */
export interface ConnStat {
  remote: string;
  pid: number;
  /** Process type label: CLI session process / Main process / Renderer process / GPU process / Utility process / Crash reporter process */
  proc: string;
}

export interface Snapshot {
  currentTps: number;
  avgTps: number;
  totalTokens: number;
  outputTokens: number;
  inputTokens: number;
  cacheCreationTokens: number;
  cacheReadTokens: number;
  callsToday: number;
  sessionsToday: number;
  isLive: boolean;
  isEstimating: boolean;
  /** Call started but first byte not yet arrived (TTFT): show "Collecting…" prompt instead of estimate */
  isStarting: boolean;
  /** Measured streaming started but 30s sliding window not yet filled (shows "Collecting") */
  ramping: boolean;
  /** True speed of calls completed in the last 10 minutes (on-disk spec) */
  windowTps: number;
  /** True speed of the most recently completed call (on-disk spec), used by the current-speed card top-right badge */
  lastCallTps: number;
  /** Peak single-call speed in the last 7 days (window and admission criteria see metrics.rs; mock gives a reasonable peak) */
  histMaxTps: number;
  /** 7-day average speed (Σeff ÷ Σgen of calls in window, same basis as today's average) */
  histAvgTps: number;
  liveSource: string;
  lastActivityMs: number;
  nowMs: number;
  rolloutDir: string;
  spark: number[];
  /** Concurrent task per-process detail (frontend shows task list when ≥ 2) */
  tasks: TaskStat[];
  // ---- Network traffic monitoring (netio.rs; browser preview uses simulated values) ----
  netAvailable: boolean;
  netUpBps: number;
  netDownBps: number;
  netUpToday: number;
  netDownToday: number;
  netSessUpToday: number;
  netSessDownToday: number;
  netCkptToday: number;
  netCkptTodayCount: number;
  /** List of artifacts accepted today (time/workspace/size) */
  netCkptTodayList: CkptStat[];
  netCkptUploading: boolean;
  netCkptStatus: string;
  /** Snapshot upload record (latest artifact status per workspace) */
  netCkptList: CkptStat[];
  netConnsAvailable: boolean;
  netCliConns: number;
  netAppConns: number;
  /** Connection detail (each entry contains remote + owning pid + process type label) */
  netCliConnList: ConnStat[];
  netAppConnList: ConnStat[];
  /** Snapshot guard status (snapshot_guard.rs; mock/browser preview lacks this field → rendered as unguarded) */
  guard?: GuardStatus;
}

interface MockCall {
  completed: number;
  duration: number;
  output: number;
  input: number;
  cache: number;
  session: string;
  /** Silent pipeline call: no incremental bytes in the entire segment, uses ≈ estimate display */
  silent: boolean;
  /** Fixed speed of the second process during concurrent tasks (0 = single task; fixed per call, not re-sampled per tick) */
  second: number;
  /** Model name (model detail view groups by this; same role as model_id in the real store) */
  model: string;
}

const MIN_DUR = 50;
const WINDOW = 10 * 60 * 1000;
const BUCKETS = 90;
const BUCKET = 10_000;

const rnd = (a: number, b: number) => a + Math.random() * (b - a);

/** Simulated model pool: primary model high frequency, two secondary models low frequency (demonstrates multiple lines in model detail view) */
const MODEL_POOL = ["claude-sonnet-4-5", "claude-sonnet-4-5", "glm-4.6", "deepseek-v3.2"];

let calls: MockCall[] = [];
let sessionNo = 1;
// Network monitoring simulation state: today's cumulative monotonically increasing; occasionally triggers a "snapshot upload" demo alert row
let netUpToday = rnd(2e8, 6e8);
let netDownToday = rnd(1e9, 4e9);
let mockCkptUploading = false;
let ckptNextToggle = Date.now() + rnd(15_000, 40_000);

function newCall(now: number): MockCall {
  if (Math.random() < 0.18) sessionNo++;
  const duration = Math.exp(rnd(Math.log(12000), Math.log(180000)));
  const tps = rnd(18, 70);
  const output = Math.max(60, Math.round((duration / 1000) * tps));
  // usage library semantics: cache_read is a subset of input, typical hit rate 90%+
  const input = Math.round(rnd(15000, 60000));
  return {
    completed: now + duration,
    duration,
    output,
    input,
    cache: Math.round(input * rnd(0.8, 0.99)),
    session: `mock-sess-${sessionNo}`,
    silent: Math.random() < 0.22,
    second: Math.random() < 0.3 ? rnd(15, 90) : 0,
    model: MODEL_POOL[Math.floor(Math.random() * MODEL_POOL.length)],
  };
}

function seedHistory(now: number) {
  let t = now - 3 * 3600 * 1000;
  while (t < now) {
    if (Math.random() < 0.72) {
      const burst = Math.round(rnd(3, 14));
      for (let i = 0; i < burst && t < now; i++) {
        const c = newCall(t);
        if (c.completed > now) break;
        calls.push(c);
        t = c.completed + rnd(300, 2500);
      }
      t += rnd(20000, 240000);
    } else {
      t += rnd(60000, 300000);
    }
  }
  sessionNo = Math.max(sessionNo, 6);
}

function snapshot(now: number, pending: MockCall | null): Snapshot {
  let out = 0,
    input = 0,
    cache = 0,
    dur = 0,
    wOut = 0,
    wDur = 0,
    last = 0,
    lastTps = 0;
  const sessions = new Set<string>();
  const buckets = new Array<number>(BUCKETS).fill(0);
  const bucketDur = new Array<number>(BUCKETS).fill(0);
  const nowSlot = Math.floor(now / BUCKET);
  for (const c of calls) {
    const d = Math.max(MIN_DUR, c.duration);
    out += c.output;
    input += c.input;
    cache += c.cache;
    dur += d;
    if (c.completed >= last) {
      last = c.completed;
      lastTps = c.output / (d / 1000); // Consistent with backend: take the latest completed entry's eff ÷ pure generation duration
    }
    sessions.add(c.session);
    if (c.completed >= now - WINDOW) {
      wOut += c.output;
      wDur += d;
    }
    const slot = nowSlot - Math.floor(c.completed / BUCKET);
    if (slot >= 0 && slot < BUCKETS) {
      buckets[BUCKETS - 1 - slot] += c.output;
      bucketDur[BUCKETS - 1 - slot] += d;
    }
  }
  // Gating model consistent with backend: based on the in-progress call (pending).
  // Simulate TTFT ~2.5s: startup period shows "Collecting…"; about 1/5 of calls are silent pipeline (entire segment ≈ estimate)
  const pendingStart = pending ? pending.completed - pending.duration : 0;
  const ageSec = pending ? (now - pendingStart) / 1000 : Infinity;
  const isStarting = !!pending && ageSec < 2.5 && !pending.silent;
  const isLive = !!pending && ageSec >= 2.5 && !pending.silent;
  const isEstimating = !!pending && pending.silent && wDur > 0;
  const currentTps = isStarting
    ? 0
    : isLive || isEstimating
      ? wDur > 0
        ? wOut / (wDur / 1000)
        : 0
      : 0;
  // Simulate concurrent tasks: during some calls, a second CLI process is also streaming — current speed is aggregate
  // total throughput, task list shows two rows (previews multi-task UI; second task speed fixed per call)
  const ownTps = currentTps;
  const secondTps = pending && pending.second > 0 && isLive ? pending.second : 0;
  const currentAgg = currentTps + secondTps;
  const spark = buckets.map((o, i) => (bucketDur[i] > 0 ? o / (bucketDur[i] / 1000) : 0));
  if (isEstimating && spark[BUCKETS - 1] <= 0) {
    spark[BUCKETS - 1] = currentTps;
  }
  const tasks: TaskStat[] = [];
  if (pending && (isLive || isStarting)) {
    tasks.push({
      pid: 4000 + sessionNo,
      session: pending.session,
      nSessions: 1,
      tps: ownTps,
      streaming: isLive,
    });
    if (secondTps > 0) {
      tasks.push({
        pid: 7000 + sessionNo,
        session: `mock-sess-${sessionNo + 1}`,
        nSessions: 1,
        tps: secondTps,
        streaming: true,
      });
    }
  }
  return {
    currentTps: currentAgg,
    avgTps: dur > 0 ? out / (dur / 1000) : 0,
    totalTokens: out + input,
    outputTokens: out,
    inputTokens: input,
    cacheCreationTokens: 0,
    cacheReadTokens: cache,
    callsToday: calls.length,
    sessionsToday: sessions.size,
    isLive,
    isEstimating,
    isStarting,
    ramping: isLive && ageSec < 30,
    windowTps: wDur > 0 ? wOut / (wDur / 1000) : 0,
    lastCallTps: lastTps,
    // 7-day stats mock: peak = today's peak × 1.2, 7-day average slightly below today's average (diluted by multiple days)
    histMaxTps: Math.max(...spark, lastTps) * 1.2 || 312,
    histAvgTps: dur > 0 ? (out / (dur / 1000)) * 0.92 : 0,
    liveSource: isStarting || isLive ? "io" : isEstimating ? "window" : "idle",
    lastActivityMs: last,
    nowMs: now,
    rolloutDir: "（Browser preview · simulated data）",
    spark,
    tasks,
    // Network monitoring simulation: speed fluctuates with call activity, today's cumulative monotonically increasing
    netAvailable: true,
    netUpBps: isLive ? rnd(20_000, 90_000) : rnd(0, 3_000),
    netDownBps: isLive ? rnd(80_000, 400_000) : rnd(0, 8_000),
    netUpToday: netUpToday,
    netDownToday: netDownToday,
    // Same basis as backend: upload = uncached tokens (input−cache_read)×5, download = output×400
    netSessUpToday: Math.max(0, input - cache) * 5,
    netSessDownToday: out * 400,
    // Same as real machine: 3 artifacts today (1GB large artifact + two KB-level small artifacts)
    netCkptToday: 1024.0 * 1048576 + 990 + 1013,
    netCkptTodayCount: 3,
    netCkptTodayList: [
      { workspace: "GenePad-free", bytes: 1024.0 * 1048576, recordedMs: now - 7 * 3600_000, accepted: true, uploading: false },
      { workspace: "default", bytes: 990, recordedMs: now - 16 * 3600_000, accepted: true, uploading: false },
      { workspace: "zcode-speed-panel", bytes: 1013, recordedMs: now - 5 * 3600_000, accepted: true, uploading: false },
    ],
    netCkptUploading: mockCkptUploading,
    netCkptStatus: "ok",
    // Snapshot upload record simulation: uploading > pending > accepted (includes cross-day records to demonstrate month/day display),
    // 9 rows total to demonstrate "fixed 5-row display, rest scrollable"
    netCkptList: [
      { workspace: "GenePad", bytes: 549.2 * 1048576, recordedMs: now - 3600_000, accepted: !mockCkptUploading, uploading: mockCkptUploading },
      { workspace: "GenePad-free", bytes: 1024.0 * 1048576, recordedMs: now - 7 * 3600_000, accepted: true, uploading: false },
      { workspace: "zcode-speed-panel", bytes: 990, recordedMs: now - 4 * 3600_000, accepted: true, uploading: false },
      { workspace: "Gene_Editor-master", bytes: 522.5 * 1048576, recordedMs: now - 13 * 86400_000, accepted: true, uploading: false },
      { workspace: "notes-sync", bytes: 18.4 * 1048576, recordedMs: now - 2 * 86400_000, accepted: true, uploading: false },
      { workspace: "dotfiles", bytes: 2048, recordedMs: now - 3 * 86400_000, accepted: true, uploading: false },
      { workspace: "blog-hugo", bytes: 96.7 * 1048576, recordedMs: now - 4 * 86400_000, accepted: true, uploading: false },
      { workspace: "ml-bench", bytes: 733.0 * 1048576, recordedMs: now - 6 * 86400_000, accepted: true, uploading: false },
      { workspace: "scrape-tools", bytes: 4400, recordedMs: now - 8 * 86400_000, accepted: true, uploading: false },
    ],
    netConnsAvailable: true,
    netCliConns: isLive ? 2 : 1,
    netAppConns: mockCkptUploading ? 3 : 1,
    // Connection detail simulation: both groups are ZCode's own processes (CLI / Electron shell), labeled by process
    netCliConnList: [
      { remote: "61.170.79.24:443", pid: 41092, proc: "CLI Session" },
      { remote: "61.170.79.31:443", pid: 41092, proc: "CLI Session" },
    ],
    netAppConnList: mockCkptUploading
      ? [
          { remote: "61.151.230.245:443", pid: 18104, proc: "Main Process" },
          { remote: "oss-cn-hangzhou.aliyuncs.com:443", pid: 18104, proc: "Main Process" },
          { remote: "oss-cn-hangzhou.aliyuncs.com:443", pid: 18220, proc: "Utility Process" },
        ]
      : [{ remote: "61.151.230.245:443", pid: 18104, proc: "Main Process" }],
  };
}

/** Simulated data for model detail view: aggregates call stream by model × absolute wall-clock slot (same
 *  basis as backend aggregate_model_stats: slot = now÷bucketMs − completed÷bucketMs,
 *  90 buckets, shared across four window levels with curves, out-of-bounds discarded, bucket tps = Σeff ÷ Σgen_s), for Tauri-free preview */
export function mockModelStats(windowMin: number): ModelStatsPayload {
  const win = [15, 60, 360, 1440].reduce((a, b) => (Math.abs(b - windowMin) < Math.abs(a - windowMin) ? b : a));
  const now = Date.now();
  const bucketMs = (win * 60_000) / 90;
  const cutoff = now - win * 60_000;
  interface Acc {
    eff: number;
    gen: number;
    calls: number;
  }
  const per = new Map<string, { slots: Acc[]; total: Acc }>();
  for (const c of calls) {
    if (c.completed < cutoff || c.completed > now) continue;
    const gen = Math.max(MIN_DUR, c.duration);
    const slot = Math.floor(now / bucketMs) - Math.floor(c.completed / bucketMs);
    if (slot < 0 || slot >= 90) continue;
    let e = per.get(c.model);
    if (!e) {
      e = { slots: Array.from({ length: 90 }, () => ({ eff: 0, gen: 0, calls: 0 })), total: { eff: 0, gen: 0, calls: 0 } };
      per.set(c.model, e);
    }
    const b = e.slots[slot];
    b.eff += c.output;
    b.gen += gen;
    b.calls += 1;
    e.total.eff += c.output;
    e.total.gen += gen;
    e.total.calls += 1;
  }
  const grandEff = [...per.values()].reduce((t, e) => t + e.total.eff, 0);
  const tpsOf = (b: Acc) => (b.gen > 0 ? b.eff / (b.gen / 1000) : 0);
  const series = [...per.entries()]
    .sort((a, b) => b[1].total.eff - a[1].total.eff || a[0].localeCompare(b[0]))
    .map(([model, e]) => ({
      model,
      buckets: e.slots.map((b) => ({ tps: tpsOf(b), calls: b.calls, tokens: b.eff })),
      totalCalls: e.total.calls,
      totalTokens: e.total.eff,
      avgTps: e.total.gen > 0 ? e.total.eff / (e.total.gen / 1000) : 0,
      peakTps: Math.max(0, ...e.slots.map(tpsOf)),
      share: grandEff > 0 ? e.total.eff / grandEff : 0,
    }));
  return { windowMin: win, bucketMs, nowMs: now, series };
}

export function startMock(onData: (s: Snapshot) => void) {
  const now = Date.now();
  seedHistory(now);
  let pending: MockCall | null = null;
  let nextStart = now + rnd(1000, 4000);

  const tick = () => {
    const t = Date.now();
    if (pending && t >= pending.completed) {
      calls.push(pending);
      // Keep only the most recent 30 minutes
      const cutoff = t - 30 * 60 * 1000;
      calls = calls.filter((c) => c.completed >= cutoff);
      pending = null;
      nextStart = t + rnd(200, 2500);
    }
    if (!pending && t >= nextStart) {
      pending = newCall(t);
    }
    // Network monitoring simulation: cumulative advances by simulated speed; snapshot upload segment starts/stops occasionally
    netUpToday += rnd(500, 120_000) * 0.4;
    netDownToday += rnd(2_000, 500_000) * 0.4;
    if (t >= ckptNextToggle) {
      mockCkptUploading = !mockCkptUploading;
      ckptNextToggle = t + (mockCkptUploading ? rnd(20_000, 50_000) : rnd(40_000, 120_000));
    }
    onData(snapshot(t, pending));
  };

  tick();
  setInterval(tick, 400);
}
