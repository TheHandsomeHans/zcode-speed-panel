// Model Speed Trends view (inside the curve card, toggled mutually exclusively
// with the overall output speed curve switch): per-model speed line chart (unified
// 90 buckets) and window statistics. Data comes from the backend model_stats command
// (read-only query against the usage library's model_usage table for aggregation,
// zero local storage); while the view is active it pulls every 5s, redraws with
// wall-clock phase shifting every 1s, and stops when deactivated.
// The time axis is fully identical to the overall curve (gauges.drawSpark): the same
// set of time range steps (determined by the range dropdown in main.ts; switching
// views does not change the range), the same bucket width and grid interval, buckets
// aligned to absolute wall-clock slots (backend div_euclid), x mapping anchored to
// the "next bucket boundary", grid lines at whole-minute marks, the curve shifts
// over time without deforming, and the two views' horizontal axes are pixel-aligned.
// The legend consists of clickable chips: toggle that model's visibility (the line
// and the bottom stats row are filtered together; at least one must remain — canceling
// everything auto-reverts to "Select All"), with "Select All / Top 3 only" at the end
// of the row; the selection is persisted to localStorage (modelStats.visible.v1).
import { fmtClock, fmtTokens, fmtTps, niceCeil } from "./gauges";

const FONT = `"Segoe UI", "Microsoft YaHei", sans-serif`;

/** Line/legend colors (cycled in series order) */
const PALETTE = ["#22d3ee", "#a78bfa", "#34d399", "#fbbf24", "#f87171", "#60a5fa", "#f472b6", "#4ade80"];

/** Max model name length in legend and stats rows (truncated with …; full name in title) */
const MODEL_NAME_MAX = 18;

/** Persistence key for model visibility selection (stores array of visible model names;
 *  falls back to "Select All" if not found or all models no longer exist) */
const VISIBLE_KEY = "modelStats.visible.v1";

const loadVisible = (): Set<string> => {
  try {
    const raw = localStorage.getItem(VISIBLE_KEY);
    const arr = raw ? (JSON.parse(raw) as unknown) : null;
    if (Array.isArray(arr)) {
      return new Set(arr.filter((x): x is string => typeof x === "string"));
    }
  } catch {
    // Corrupted save: ignore, fall back to "Select All"
  }
  return new Set();
};

export interface ModelBucket {
  tps: number;
  calls: number;
  tokens: number;
}

export interface ModelSeries {
  model: string;
  buckets: ModelBucket[];
  totalCalls: number;
  totalTokens: number;
  avgTps: number;
  peakTps: number;
  share: number;
}

export interface ModelStatsPayload {
  windowMin: number;
  bucketMs: number;
  nowMs: number;
  series: ModelSeries[];
}

type InvokeFn = <T>(cmd: string, args?: Record<string, unknown>) => Promise<T | undefined>;

const $ = <T extends HTMLElement>(id: string): T => {
  const el = document.getElementById(id);
  if (!el) throw new Error(`missing #${id}`);
  return el as T;
};

const shortModel = (name: string): string => (name.length > MODEL_NAME_MAX ? name.slice(0, MODEL_NAME_MAX) + "…" : name);

/** Canvas DPR adaptation following the same rules as gauges.drawSpark */
function fitCanvas(
  canvas: HTMLCanvasElement,
): { ctx: CanvasRenderingContext2D; w: number; h: number } | null {
  const w = canvas.clientWidth;
  const h = canvas.clientHeight;
  if (w < 8 || h < 8) return null;
  const dpr = window.devicePixelRatio || 1;
  const pw = Math.round(w * dpr);
  const ph = Math.round(h * dpr);
  if (canvas.width !== pw || canvas.height !== ph) {
    canvas.width = pw;
    canvas.height = ph;
  }
  const ctx = canvas.getContext("2d");
  if (!ctx) return null;
  ctx.setTransform(dpr, 0, 0, dpr, 0, 0);
  ctx.clearRect(0, 0, w, h);
  return { ctx, w, h };
}

/** Visible legend item: series + its original index in the full series list
 *  (determines color; stays stable after filtering) */
interface VisibleItem {
  s: ModelSeries;
  idx: number;
}

/** One tps line per model: y axis 0~niceCeil(peak*1.25) (same quantization as the overall
 *  curve, so minor data changes don't cause the whole curve to rescale) + 3 horizontal
 *  grid lines; x axis uses the exact same mapping as drawSpark — right edge = next bucket
 *  boundary, grid lines at gridMs whole-minute marks, data points drawn at wall-clock
 *  bucket centers. Only draws models in visible; color uses each model's original index
 *  in the full list (color stays the same when hidden then shown again) */
function drawModelChart(
  canvas: HTMLCanvasElement,
  p: ModelStatsPayload,
  visible: VisibleItem[],
  nowMs: number,
  gridMs: number,
) {
  const fit = fitCanvas(canvas);
  if (!fit) return;
  const { ctx, w, h } = fit;
  const padL = 40;
  const padR = 10;
  const padT = 10;
  const padB = 20;
  const iw = w - padL - padR;
  const ih = h - padT - padB;
  const n = visible[0]?.s.buckets.length ?? p.series[0]?.buckets.length ?? 90;
  const bucketMs = p.bucketMs;
  const peak = Math.max(
    10,
    niceCeil(Math.max(0, ...visible.flatMap((it) => it.s.buckets.map((b) => b.tps))) * 1.25),
  );
  const yAt = (v: number) => padT + ih - (Math.min(v, peak) / peak) * ih;

  // 3 horizontal grid lines + tick labels (top = peak step, mid = half step, bottom = 0)
  ctx.strokeStyle = "rgba(255,255,255,0.06)";
  ctx.fillStyle = "rgba(139,147,167,0.7)";
  ctx.font = `10px ${FONT}`;
  ctx.lineWidth = 1;
  ctx.textAlign = "right";
  ctx.textBaseline = "middle";
  for (let i = 0; i <= 2; i++) {
    const y = padT + (ih * i) / 2;
    ctx.beginPath();
    ctx.moveTo(padL, y);
    ctx.lineTo(w - padR, y);
    ctx.stroke();
    ctx.fillText(fmtTps((peak * (2 - i)) / 2), padL - 6, y);
  }
  if (visible.length === 0) return;

  // ---- x-axis real-time ticks: same mapping as drawSpark (verifiable side-by-side).
  //      Latest bucket end time = next wall-clock bucket boundary; right edge is "now"
  //      (within <=1 bucket); as nowMs slides within a bucket the whole curve shifts
  //      left continuously while grid lines stay fixed at whole-minute marks.
  const dx = iw / n;
  const tLastEnd = Math.floor(nowMs / bucketMs) * bucketMs + bucketMs;
  const xAt = (t: number) => padL + iw - ((tLastEnd - t) / bucketMs) * dx;
  ctx.textAlign = "center";
  ctx.textBaseline = "top";
  let t = Math.ceil((tLastEnd - n * bucketMs) / gridMs) * gridMs;
  for (; t <= tLastEnd; t += gridMs) {
    const gx = xAt(t);
    if (gx < padL || gx > padL + iw) continue;
    ctx.strokeStyle = "rgba(255,255,255,0.05)";
    ctx.beginPath();
    ctx.moveTo(gx, padT);
    ctx.lineTo(gx, padT + ih);
    ctx.stroke();
    ctx.fillText(fmtClock(t).slice(0, 5), gx, h - padB + 4);
  }
  // Right edge: current time (right-aligned to avoid overflow)
  ctx.textAlign = "right";
  ctx.fillStyle = "rgba(139,147,167,0.9)";
  ctx.fillText(`Now ${fmtClock(nowMs).slice(0, 5)}`, padL + iw, h - padB + 4);

  ctx.lineWidth = 2;
  ctx.lineJoin = "round";
  for (const { s, idx } of visible) {
    ctx.strokeStyle = PALETTE[idx % PALETTE.length];
    ctx.beginPath();
    s.buckets.forEach((b, i) => {
      // Bucket i (0 = latest) center time: back (i+0.5) buckets from latest bucket right edge tLastEnd
      const x = xAt(tLastEnd - (i + 0.5) * bucketMs);
      const y = yAt(b.tps);
      if (i === 0) ctx.moveTo(x, y);
      else ctx.lineTo(x, y);
    });
    ctx.stroke();
  }
}

/** Model details view controller: setActive(true) starts data polling and phase-shift
 *  redraw, false stops everything; refresh() re-pulls immediately when the stats range
 *  step changes (5s polling continues as normal).
 *  The canvas, legend, and stats row DOM is self-managed by this module; visibility
 *  toggling (CSS body.chart-view-model) and persistence live in main.ts — view toggle
 *  and time range both belong to the curve card as a whole. */
export interface ModelStatsController {
  setActive(active: boolean): void;
  refresh(): void;
}

/** Binds the model details view: 5s data polling, 1s phase-shift redraw.
 *  getRange returns the overall curve's current stats range and grid interval (single
 *  source of truth is main.ts's CHART_RANGES) — both views share the same time axis,
 *  toggling the switch does not change the range. */
export function initModelStats(
  invoke: InvokeFn,
  getRange: () => { windowMin: number; gridMs: number },
): ModelStatsController {
  const legend = $("model-legend");
  const canvas = $<HTMLCanvasElement>("model-chart");
  const empty = $("model-empty");
  const summary = $("model-summary");

  let active = false;
  let fetchTimer = 0;
  let slideTimer = 0;
  let lastPayload: ModelStatsPayload | null = null;
  // Model visibility selection: models in the Set are visible. At least one must
  // remain (canceling all auto-reverts to "Select All"), persisted to localStorage;
  // polling only redraws, so the in-memory selection naturally persists without flashing.
  let visible = loadVisible();
  /** Models seen this session: null = first frame not yet arrived; first frame
   *  trims/falls back per the save; afterwards newly appearing models (model change /
   *  new window) default to visible and aren't silently hidden by the old save. */
  let knownModels: Set<string> | null = null;

  const saveVisible = () => localStorage.setItem(VISIBLE_KEY, JSON.stringify([...visible]));

  /** Reconciles the visible set against the current payload's model list:
   *  first frame — models no longer present in the save are removed (if all are
   *  invalid, fall back to "Select All");
   *  subsequent frames — new models default to visible, disappeared models are
   *  removed (keeps the save clean). */
  const reconcileVisible = (models: string[]) => {
    if (knownModels === null) {
      for (const m of [...visible]) {
        if (!models.includes(m)) visible.delete(m);
      }
      if (visible.size === 0) models.forEach((m) => visible.add(m));
      knownModels = new Set(models);
      return;
    }
    for (const m of models) {
      if (!knownModels.has(m)) {
        knownModels.add(m);
        visible.add(m);
      }
    }
    for (const m of [...visible]) {
      if (!models.includes(m)) visible.delete(m);
    }
    if (visible.size === 0) models.forEach((m) => visible.add(m));
  };

  /** Toggles a model's visibility; when all are canceled, auto-reverts to "Select All" (at least one remains) */
  const toggleModel = (model: string, models: string[]) => {
    if (visible.has(model)) visible.delete(model);
    else visible.add(model);
    if (visible.size === 0) models.forEach((m) => visible.add(m));
    saveVisible();
    render(lastPayload);
  };

  const selectAll = (models: string[]) => {
    models.forEach((m) => visible.add(m));
    saveVisible();
    render(lastPayload);
  };

  /** Top 3 only: sort by total_tokens and take the top 3 (equivalent to "Select All" when fewer than 3 models) */
  const selectTop3 = (series: ModelSeries[]) => {
    const top3 = [...series].sort((a, b) => b.totalTokens - a.totalTokens).slice(0, 3).map((s) => s.model);
    visible = new Set(top3);
    saveVisible();
    render(lastPayload);
  };

  /** Empty state: placeholder when there is no payload / empty window / fetch fails (legend and stats row are also hidden) */
  const showEmpty = (text: string) => {
    empty.textContent = text;
    empty.style.display = "flex";
    legend.style.display = "none";
    summary.style.display = "none";
  };

  /** Renders one payload: legend chips, stats rows, and line chart (shows empty state when no data).
   *  Legend and stats row only list visible models; chip color uses the original index,
   *  so color stays the same when hidden then shown again. */
  const render = (p: ModelStatsPayload | null) => {
    if (!p) return;
    lastPayload = p;
    const models = p.series.map((s) => s.model);
    // Empty window doesn't change the selection (otherwise the save would be cleared
    // and the selection would be lost when data returns)
    if (models.length > 0) reconcileVisible(models);
    const visibleItems: VisibleItem[] = p.series
      .map((s, idx) => ({ s, idx }))
      .filter((it) => visible.has(it.s.model));
    const has = p.series.length > 0;
    empty.style.display = has ? "none" : "flex";
    if (!has) empty.textContent = "No call data in window";
    legend.style.display = has ? "flex" : "none";
    summary.style.display = has ? "flex" : "none";
    if (!has) return;
    legend.replaceChildren();
    summary.replaceChildren();
    p.series.forEach((s, i) => {
      const on = visible.has(s.model);
      const color = PALETTE[i % PALETTE.length];
      const chip = document.createElement("button");
      chip.type = "button";
      chip.className = on ? "model-chip on" : "model-chip";
      chip.title = `${s.model} (click to ${on ? "hide" : "show"})`;
      chip.setAttribute("aria-pressed", String(on));
      const dot = document.createElement("span");
      dot.className = "model-dot";
      dot.style.background = color;
      const name = document.createElement("span");
      name.className = "model-chip-name";
      name.textContent = shortModel(s.model);
      chip.append(dot, name);
      chip.addEventListener("click", () => toggleModel(s.model, models));
      legend.append(chip);

      if (!on) return;
      const row = document.createElement("span");
      row.className = "model-stat-row";
      const rdot = document.createElement("span");
      rdot.className = "model-dot";
      rdot.style.background = color;
      rdot.title = s.model;
      const text = document.createElement("span");
      text.textContent = `${shortModel(s.model)} · Avg ${fmtTps(s.avgTps)} t/s · Peak ${fmtTps(s.peakTps)} · ${s.totalCalls} calls · ${fmtTokens(s.totalTokens)} tokens (${(s.share * 100).toFixed(1)}%)`;
      text.title = s.model;
      row.append(rdot, text);
      summary.append(row);
    });
    // Row-end actions: Select All / Top 3 only
    const tools = document.createElement("span");
    tools.className = "model-legend-tools";
    const allBtn = document.createElement("button");
    allBtn.type = "button";
    allBtn.textContent = "Select All";
    allBtn.title = "Show all models";
    allBtn.addEventListener("click", () => selectAll(models));
    const top3Btn = document.createElement("button");
    top3Btn.type = "button";
    top3Btn.textContent = "Top 3 only";
    top3Btn.title = "Show only the top 3 models by token usage";
    top3Btn.addEventListener("click", () => selectTop3(p.series));
    tools.append(allBtn, top3Btn);
    legend.append(tools);
    drawModelChart(canvas, p, visibleItems, Date.now(), getRange().gridMs);
  };

  /** 1s phase-shift redraw: canvas only, no DOM rebuild — data only changes every 5s,
   *  during which the curve shifts left continuously with the wall clock. */
  const slide = () => {
    if (!active || !lastPayload || document.body.classList.contains("float-mode")) return;
    const items: VisibleItem[] = lastPayload.series
      .map((s, idx) => ({ s, idx }))
      .filter((it) => visible.has(it.s.model));
    if (items.length) drawModelChart(canvas, lastPayload, items, Date.now(), getRange().gridMs);
  };

  const fetchNow = () => {
    if (!active) return;
    // When collapsed to a floating window the main panel is hidden by CSS: skip the fetch
    // (automatically resumes when returning to the full panel)
    if (document.body.classList.contains("float-mode")) return;
    invoke<ModelStatsPayload>("model_stats", { windowMin: getRange().windowMin })
      .then((p) => {
        if (!active) return;
        if (p) render(p);
        else if (!lastPayload) showEmpty("Stats unavailable");
      })
      .catch((err) => {
        console.warn("[model_stats] invoke failed:", err);
        if (active && !lastPayload) showEmpty("Failed to read stats");
      });
  };

  const setActive = (on: boolean) => {
    if (active === on) return;
    active = on;
    window.clearInterval(fetchTimer);
    fetchTimer = 0;
    window.clearInterval(slideTimer);
    slideTimer = 0;
    if (on) {
      if (!lastPayload) showEmpty("Loading stats…");
      fetchNow();
      fetchTimer = window.setInterval(fetchNow, 5000);
      slideTimer = window.setInterval(slide, 1000);
    }
  };

  window.addEventListener("resize", slide);
  return {
    setActive,
    refresh: () => {
      if (active) fetchNow(); // Step change re-pulls immediately (5s timer continues pulling on the new step)
    },
  };
}
