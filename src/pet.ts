// Desktop pet pet pack (Codex Pet format, from dsh-desk project, MIT)
// Sprite sheet: 1536 wide × 8 columns, each frame 192×cellHeight; row order corresponds to animations

import { SPEED_TIERS, fmtTps, speedColor, speedTierIndex } from "./gauges";

const FONT = `"Segoe UI", "Microsoft YaHei", sans-serif`;

/** Bubble task row limit (discard excess rows when they no longer fit; aggregate value stays on first row) */
const MAX_TASK_ROWS = 6;

/** Single concurrent task row (per-process measured, from snapshot.tasks; main.ts handles label assembly) */
export interface PetTask {
  label: string;
  tps: number;
  streaming: boolean;
}

/** Head bubble layout (interpolates between single/double row based on hover progress) */
interface BubbleLayout {
  rows: Array<{ label: string; value: string; color: string }>;
  /** Hover progress 0~1: interpolation factor for text and size */
  t: number;
  fs: number;
  labelFs: number;
  lineH: number;
  gapY: number;
  padX: number;
  padY: number;
  colGap: number;
  /** Already multiplied by hover progress: label column slides in with it, preventing values from being pushed out of the box */
  labelW: number;
  boxW: number;
  boxH: number;
  /** Bubble bottom anchor (near top of sprite head, canvas coordinates): bottom stays fixed, grows upward when switching between single/double rows */
  bottom: number;
  /** Bubble visibility (0 when idle): whole block fades in/out */
  vis: number;
}

/** Single animation: asset row; play = explicit play column order (0-based column index, can repeat/skip bad frames),
 *  defaults to playing 0..frames-1 frame by frame */
export interface AnimDef {
  row: number;
  frames: number;
  play?: number[];
}

export interface PetPack {
  id: string;
  displayName: string;
  sheet: string;
  sheetW: number;
  cellW: number;
  cellH: number;
  rows: number;
  /** Row order → animation name and frame count */
  anims: Record<string, AnimDef>;
  /** Idle group animation names: when idle, randomly pick a new one to play after each full row completes */
  idleAnims: string[];
  frameMs: number;
}

export const PET_PACKS: PetPack[] = [
  {
    id: "yuexinmiao",
    displayName: "Salary Cat",
    sheet: "pets/yuexinmiao/spritesheet.webp",
    sheetW: 1536,
    cellW: 192,
    cellH: 208,
    rows: 9,
    anims: {
      idle: { row: 0, frames: 6 },
      running_right: { row: 1, frames: 8 },
      running_left: { row: 2, frames: 8 },
      waving: { row: 3, frames: 4 },
      jumping: { row: 4, frames: 5 },
      failed: { row: 5, frames: 8 },
      waiting_permission: { row: 6, frames: 6 },
      running: { row: 7, frames: 6 },
      review: { row: 8, frames: 6 },
    },
    idleAnims: ["idle"],
    frameMs: 160,
  },
  {
    id: "maid-deepseek-whale",
    displayName: "Whale Maid",
    sheet: "pets/maid-deepseek-whale/spritesheet.webp",
    sheetW: 1536,
    cellW: 192,
    cellH: 208,
    rows: 11,
    anims: {
      idle: { row: 0, frames: 7 },
      running_right: { row: 1, frames: 8 },
      running_left: { row: 2, frames: 8 },
      waving: { row: 3, frames: 4 },
      jumping: { row: 4, frames: 5 },
      failed: { row: 5, frames: 8 },
      waiting_permission: { row: 6, frames: 6 },
      running: { row: 7, frames: 6 },
      review: { row: 8, frames: 6, play: [0, 1, 2, 3, 0] }, // Column 4 is a bad frame, skip it; column 5 graphic is too large, deprecated; last position uses column 0 instead
      idle_talk: { row: 9, frames: 8 },
      idle_shy: { row: 10, frames: 8 },
    },
    idleAnims: ["idle"], // Rows 9/10 talk/shy are registered for backup only, not participating in rotation for now
    frameMs: 160,
  },
];

export function packById(id: string): PetPack {
  return PET_PACKS.find((p) => p.id === id) ?? PET_PACKS[0];
}

/** Animation play frame columns: explicit play sequence takes priority, otherwise 0..frames-1 */
function animCols(anim: AnimDef): number[] {
  if (anim.play?.length) return anim.play;
  return Array.from({ length: anim.frames }, (_, c) => c);
}

/** Desktop pet canvas: sprite animation + state switching + head speed bubble */
export class PetWidget {
  private canvas: HTMLCanvasElement;
  private pack: PetPack;
  private img: HTMLImageElement | null = null;
  private anim = "idle";
  /** Play index: position in current animation frame column list (animCols, bad frames already excluded) */
  private seq = 0;
  private lastFrameAt = 0;
  /** Numeric speed (used for tier selection; this.tps is the bubble display string) */
  private tpsNum = 0;
  /** 5/6 tier high-speed run direction: true=right (row 1)/false=left (row 2), switch direction after each run */
  private fastDir = true;
  private tps = "";
  /** Last-call average speed (most recently completed call, persisted metric); 0 = no completed calls today */
  private lastTps = 0;
  /** Concurrent task details (when ≥2, bubble expands into per-task rows, same metric as full panel task cards) */
  private tasks: PetTask[] = [];
  /** Always show last-call speed (right-click menu checkbox): bubble always two rows (expands directly on generation, no hover needed); visibility still follows generation state, hidden when idle */
  private alwaysLast = false;
  /** Mouse hover: bubble changes from single-row live speed to two rows (live speed / last-call speed) */
  private hover = false;
  /** Hover progress 0~1 (smooth transition, simultaneously drives bubble size and sprite offset) */
  private hoverT = 0;
  /** Bubble visibility 0~1: hidden when idle (except on hover), fade in/out simultaneously drives sprite offset */
  private visT = 0;
  private lastDrawAt = 0;
  private running = false;
  private est = false;
  private raf = 0;
  private expandBtn: HTMLElement | null = null;
  private cycleBtn: HTMLElement | null = null;

  constructor(canvas: HTMLCanvasElement, packId: string, onPackSwitch?: () => void) {
    this.canvas = canvas;
    this.pack = packById(packId);
    this.load();
    // Double-click to switch to next pet
    canvas.addEventListener("dblclick", () => {
      const i = PET_PACKS.findIndex((p) => p.id === this.pack.id);
      this.pack = packById(PET_PACKS[(i + 1) % PET_PACKS.length].id);
      this.load();
      onPackSwitch?.();
    });
    // Hover listener attached to the entire floating window (buttons are canvas siblings; attaching to canvas would misdetect leaving when moving to a button)
    const hoverTarget = canvas.parentElement ?? canvas;
    hoverTarget.addEventListener("mouseenter", () => {
      this.hover = true;
    });
    hoverTarget.addEventListener("mouseleave", () => {
      this.hover = false;
    });
  }

  private load() {
    this.img = null;
    // After switching packs, restart based on current state (animation key sets vary by pack, e.g. whale-specific talk/shy rows)
    this.anim = this.running ? this.runAnim() : this.pickIdle();
    this.seq = 0;
    const img = new Image();
    img.src = this.pack.sheet;
    img.onload = () => {
      this.img = img;
    };
  }

  setLive(tps: number, state: "idle" | "running" | "estimating" | "starting") {
    // Startup waiting (first byte not yet received) shows "…", consistent with the gauge's "calculating" hint
    this.tps = state === "starting" ? "…" : state === "estimating" ? "≈" + tps.toFixed(1) : tps.toFixed(1);
    this.tpsNum = tps;
    this.est = state === "estimating";
    // Only enter running group animation when actual streaming output is measured (or just started waiting); stay in idle rotation when falling back to estimated.
    // State/tier changes are never immediate: current animation must complete its full row, switching only takes effect at row end (see draw)
    this.running = state === "running" || state === "starting";
  }

  /** Idle random rotation: uniformly pick one from the pack's idle group (whale picks 1 of 3 including talk/shy, salary cat only stands),
   *  re-pick after each full row completes, may draw the same one consecutively */
  private pickIdle(): string {
    const keys = this.pack.idleAnims;
    return keys[Math.floor(Math.random() * keys.length)];
  }

  /** Speed tier → running group animation: tier 1 row 7(running) / tier 2 row 8(review) / tiers 3,4 row 4(jumping),
   *  tiers 5,6 use rows 1,2 (running_right/left) running back and forth */
  private runAnim(): string {
    switch (speedTierIndex(this.tpsNum)) {
      case 0:
        return "running";
      case 1:
        return "review";
      case 2:
      case 3:
        return "jumping";
      default:
        return this.fastDir ? "running_right" : "running_left";
    }
  }

  /** Running group row-end switch: when still in tier 5/6 and animation unchanged, switch direction; otherwise take current tier's animation (tier changes take effect here) */
  private nextRunAnim(): string {
    const next = this.runAnim();
    if (next === this.anim && (next === "running_right" || next === "running_left")) {
      this.fastDir = !this.fastDir;
      return this.fastDir ? "running_right" : "running_left";
    }
    return next;
  }

  /** Last-call average speed (most recently completed call speed, persisted metric): used for hover bubble second row */
  setLast(tps: number) {
    this.lastTps = isFinite(tps) && tps > 0 ? tps : 0;
  }

  /** Concurrent task details (main.ts passes when ≥2 tasks, otherwise empty array) */
  setTasks(tasks: PetTask[]) {
    this.tasks = tasks;
  }

  /** Always show last-call speed (right-click menu checkbox, persistence handled by caller) */
  setAlwaysLast(on: boolean) {
    this.alwaysLast = on;
  }

  get packId(): string {
    return this.pack.id;
  }

  /** Switch to next pet (auto-save handled by caller) */
  cyclePack(): string {
    const i = PET_PACKS.findIndex((p) => p.id === this.pack.id);
    this.pack = packById(PET_PACKS[(i + 1) % PET_PACKS.length].id);
    this.load();
    return this.pack.id;
  }

  start() {
    const loop = (now: number) => {
      this.draw(now);
      this.raf = requestAnimationFrame(loop);
    };
    this.raf = requestAnimationFrame(loop);
  }

  stop() {
    cancelAnimationFrame(this.raf);
  }

  private draw(now: number) {
    const canvas = this.canvas;
    const w = canvas.clientWidth;
    const h = canvas.clientHeight;
    if (w < 8 || h < 8) return;
    const dpr = window.devicePixelRatio || 1;
    const pw = Math.round(w * dpr);
    const ph = Math.round(h * dpr);
    if (canvas.width !== pw || canvas.height !== ph) {
      canvas.width = pw;
      canvas.height = ph;
    }
    const ctx = canvas.getContext("2d");
    if (!ctx) return;
    ctx.setTransform(dpr, 0, 0, dpr, 0, 0);
    ctx.clearRect(0, 0, w, h);

    // Expansion progress (smooth transition): 0 = single-row live speed, 1 = full rows (live / per-task / last-call speed)
    const dt = Math.min(0.1, Math.max(0, (now - this.lastDrawAt) / 1000));
    this.lastDrawAt = now;
    const multi = this.tasks.length >= 2;
    const want = this.hover || this.alwaysLast || multi ? 1 : 0;
    this.hoverT += (want - this.hoverT) * (1 - Math.exp(-dt * 12));
    if (Math.abs(want - this.hoverT) < 0.002) this.hoverT = want;
    // Bubble visibility: hidden when idle (no generation task) (hover can check last-call); shown during generation/estimation/startup
    // waiting, forced visible on hover and multi-task (≥2 processes). Checking "always show last-call speed" only controls
    // row count (always two rows), not visibility—bubble fades in with live speed when generation starts
    const wantVis = this.hover || this.running || this.est || multi ? 1 : 0;
    this.visT += (wantVis - this.visT) * (1 - Math.exp(-dt * 12));
    if (Math.abs(wantVis - this.visT) < 0.002) this.visT = wantVis;

    const anim = this.pack.anims[this.anim] ?? this.pack.anims.idle;
    let cols = animCols(anim);
    if (now - this.lastFrameAt >= this.pack.frameMs) {
      this.lastFrameAt = now;
      this.seq++;
      if (this.seq >= cols.length) {
        // Can only switch to next animation after completing a full row: idle randomly picks a new one each round;
        // idle↔running switch and running group tier changes also only take effect at this row end
        this.seq = 0;
        this.anim = this.running ? this.nextRunAnim() : this.pickIdle();
        cols = animCols(this.pack.anims[this.anim] ?? this.pack.anims.idle);
      }
    }

    // Sprite geometry (pure pack constants, independent of whether image is loaded): window = bottom square sprite area + top
    // bubble reserve zone (main.rs PET_BUBBLE_RESERVE), sprite always scales to square area, does not shrink with
    // bubble row count; falls back to short-edge square when browser preview window lacks this ratio
    const sq = Math.min(w, h);
    const availH = Math.max(this.pack.cellH * 0.15, sq * 0.92);
    const scale = Math.min((w * 0.94) / this.pack.cellW, availH / this.pack.cellH);
    const dw = this.pack.cellW * scale;
    const dh = this.pack.cellH * scale;
    const dx = (w - dw) / 2;
    // Bubble bottom edge anchored near top of sprite head (~9% up the sprite): bottom stays fixed, grows upward when switching single/double rows
    const bubble = this.layoutBubble(ctx, w, h - 2 - dh * 0.91);

    const img = this.img;
    if (img) {
      const sx = cols[this.seq] * this.pack.cellW;
      const sy = anim.row * this.pack.cellH;
      // Bottom-aligned centered: sprite cell has built-in transparent margin, scaled up and pressed to bottom edge to avoid large blank space below feet
      ctx.drawImage(img, sx, sy, this.pack.cellW, this.pack.cellH, dx, h - dh - 2, dw, dh);

      // Buttons attached to right side of sprite feet (follows actual drawn width)
      if (!this.expandBtn) this.expandBtn = document.getElementById("float-pet-expand");
      if (!this.cycleBtn) this.cycleBtn = document.getElementById("float-pet-cycle");
      const rightGap = Math.max(4, w - (dx + dw) + 2);
      if (this.expandBtn) this.expandBtn.style.right = `${rightGap}px`;
      if (this.cycleBtn) this.cycleBtn.style.right = `${rightGap + 26}px`;
    }

    // Idle and not hovered: don't draw the whole block (not even the tail)
    if (bubble.vis > 0.01) this.paintBubble(ctx, w, bubble);
  }

  /** Bubble layout: rows = live speed + (one row per process when multi-tasking) + last-call speed, interpolates with expansion progress
   *  between "single-row live speed" and "full rows with labels". Bottom anchor fixed (near top of sprite head),
   *  grows upward as rows increase; drops task rows when height insufficient (aggregate stays on first row), shrinks
   *  font size when width insufficient (pet window can shrink to 100px, browser preview without multi-task height adapts similarly) */
  private layoutBubble(ctx: CanvasRenderingContext2D, w: number, bottom: number): BubbleLayout {
    const padX = 11;
    // Padding/line-height set to match old single-row bubble height (12 + 15×1.2 ≈ original 26px),
    // otherwise single-row state would unnecessarily overlap the sprite slightly
    const padY = 5;
    const colGap = 8;
    const gapY = 3;
    const t = this.hoverT;
    const liveColor = this.est ? "#fbbf24" : this.running ? "#22d3ee" : "#8b93a7";
    const rows: Array<{ label: string; value: string; color: string }> = [
      { label: "Live Speed", value: `${this.tps} t/s`, color: liveColor },
    ];
    for (const task of this.tasks.slice(0, MAX_TASK_ROWS)) {
      rows.push({
        label: task.label,
        value: task.streaming ? `${fmtTps(task.tps)} t/s` : "Idle",
        color: task.streaming ? speedColor(task.tps, SPEED_TIERS) : "#8b93a7",
      });
    }
    rows.push({
      label: "Last-call Speed",
      value: this.lastTps > 0 ? `${fmtTps(this.lastTps)} t/s` : "--",
      color: speedColor(this.lastTps, SPEED_TIERS),
    });

    let fs = Math.max(11, Math.min(15, w * 0.075));
    let labelFs = Math.max(8, fs * 0.78);
    let lineH = fs * 1.2;
    // Height adaptation: bubble bottom anchored, grows upward, available height = bottom anchor − top margin;
    // when insufficient, drop task rows from the back (last row is last-call speed, guaranteeing at least live/last-call two rows of info)
    const boxHFor = (n: number) => padY * 2 + lineH * n + gapY * (n - 1);
    const availH = Math.max(padY * 2 + lineH, bottom - 2);
    while (rows.length > 2 && boxHFor(rows.length) > availH) {
      rows.splice(rows.length - 2, 1);
    }

    let labelW = 0;
    let valueW = 0;
    let valueW0 = 0;
    const maxBoxW = Math.max(40, w - 6);
    // During transition, lay out by expanded width (otherwise text would overflow first then shrink)
    const expanded = t > 0.01 || this.hover || this.alwaysLast || this.tasks.length >= 2;
    for (;;) {
      ctx.font = `600 ${fs}px ${FONT}`;
      valueW0 = ctx.measureText(rows[0].value).width;
      valueW = expanded ? Math.max(...rows.map((r) => ctx.measureText(r.value).width)) : valueW0;
      ctx.font = `500 ${labelFs}px ${FONT}`;
      labelW = expanded ? Math.max(...rows.map((r) => ctx.measureText(r.label).width)) : 0;
      if (padX * 2 + labelW + colGap + valueW <= maxBoxW || fs <= 8) break;
      fs -= 1;
      labelFs = Math.max(8, fs * 0.78);
      lineH = fs * 1.2;
    }

    // Collapsed width = needed for single-row live speed; expanded width = label column + widest value
    const boxW1 = padX * 2 + valueW0;
    const boxW2 = padX * 2 + labelW + colGap + valueW;
    const boxH1 = padY * 2 + lineH;
    const boxH2 = boxHFor(rows.length);
    const vis = this.visT;
    return {
      rows,
      t,
      vis,
      fs,
      labelFs,
      lineH,
      gapY,
      padX,
      padY,
      colGap,
      labelW: labelW * t,
      boxW: boxW1 + (boxW2 - boxW1) * t,
      boxH: boxH1 + (boxH2 - boxH1) * t,
      bottom,
    };
  }

  private paintBubble(ctx: CanvasRenderingContext2D, w: number, b: BubbleLayout) {
    const bx = w / 2 - b.boxW / 2;
    // Bottom anchored, grows upward; when reserve zone insufficient (browser preview etc.), falls back to top of canvas
    const by = Math.max(2, b.bottom - b.boxH);
    const bg = "rgba(13,20,36,0.88)";

    // Plate and tail fade in/out with visibility (whole block disappears when idle)
    ctx.globalAlpha = b.vis;
    ctx.fillStyle = bg;
    ctx.strokeStyle = this.est
      ? "rgba(251,191,36,0.75)"
      : this.running
        ? "rgba(34,211,238,0.75)"
        : "rgba(255,255,255,0.22)";
    ctx.lineWidth = 1;
    ctx.beginPath();
    ctx.roundRect(bx, by, b.boxW, b.boxH, Math.min(13, b.boxH / 2));
    ctx.fill();
    ctx.stroke();
    // Bubble little tail
    ctx.beginPath();
    ctx.moveTo(w / 2 - 5, by + b.boxH - 1);
    ctx.lineTo(w / 2 + 5, by + b.boxH - 1);
    ctx.lineTo(w / 2, by + b.boxH + 7);
    ctx.closePath();
    ctx.fillStyle = bg;
    ctx.fill();

    // Clip by bubble's current height: during transition, second row reveals as bubble grows taller, won't draw outside the box first
    ctx.save();
    ctx.beginPath();
    ctx.rect(bx, by, b.boxW, b.boxH);
    ctx.clip();
    ctx.textAlign = "left";
    ctx.textBaseline = "middle";
    b.rows.forEach((r, i) => {
      if (i > 0 && b.t <= 0.01) return;
      const cy = by + b.padY + b.lineH * (i + 0.5) + b.gapY * i;
      let x = bx + b.padX;
      if (r.label && b.t > 0.01) {
        ctx.globalAlpha = b.t * b.vis;
        ctx.font = `500 ${b.labelFs}px ${FONT}`;
        ctx.fillStyle = "#8b93a7";
        ctx.fillText(r.label, x, cy);
        x += b.labelW + b.colGap;
      }
      ctx.globalAlpha = (i > 0 ? b.t : 1) * b.vis;
      ctx.font = `600 ${b.fs}px ${FONT}`;
      ctx.fillStyle = r.color;
      ctx.fillText(r.value, x, cy);
    });
    ctx.restore();
    ctx.globalAlpha = 1;
  }
}
