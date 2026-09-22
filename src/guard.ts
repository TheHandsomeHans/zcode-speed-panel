// Snapshot protection & upload record card (full panel, below the network monitoring card) protection control area: status badge +
// accumulated artifact statistics + enable/release buttons (self-drawn confirmation dialog, explicitly stating the loss of "checkpoint rollback /
// timeline" — user-requested informed consent, key-rules #16). The day's snapshot uploads and
// record list in the card are rendered by main.ts's renderSnapshot (everything snapshot-related is in this card).
// The backend snapshot_guard.rs uses directory write locks (macOS chflags immutable flag /
// Windows ACL denying create/write) to block ZCode workspace snapshots from being written to disk and uploaded: no network interference,
// does not affect model conversations/completions/tool calls; status is pushed every tick via the guard field of the metrics payload
// (src/main.ts calls renderGuard; when the field is absent, the control area is hidden, only records are shown).
import { fmtBytes } from "./gauges";
import type { CkptStat } from "./mock";

/** Protection status attached to the metrics payload (src-tauri/src/snapshot_guard.rs, camelCase) */
export interface GuardStatus {
  supported: boolean;
  locked: boolean;
  lockedSinceMs: number | null;
  blockedRounds: number;
  artifactCount: number;
  artifactBytes: number;
  workspaceCount: number;
  failureCount: number;
  /** Pre-protection original upload record archive (saved before apply clears it; fully reviewable during protection) */
  history: CkptStat[];
}

type InvokeFn = <T>(cmd: string, args?: Record<string, unknown>) => Promise<T | undefined>;

const $ = <T extends HTMLElement>(id: string): T => {
  const el = document.getElementById(id);
  if (!el) throw new Error(`missing #${id}`);
  return el as T;
};

/** Element references filled after initGuard (renderGuard is called at high frequency in metrics events) */
interface GuardEls {
  card: HTMLElement;
  scope: HTMLElement;
  badge: HTMLElement;
  applyBtn: HTMLButtonElement;
  releaseBtn: HTMLButtonElement;
  msg: HTMLElement;
  stats: HTMLElement;
  rounds: HTMLElement;
}

let els: GuardEls | null = null;
let latest: GuardStatus | null = null;

/** Render protection status (main.ts's metrics listener calls this per tick; null = no backend/mock, card stays hidden) */
export function renderGuard(g: GuardStatus | null): void {
  if (!g || !els) return;
  latest = g;
  const e = els;
  e.card.hidden = false;
  e.badge.textContent = g.locked ? "🔒 Protected" : "🔓 Unprotected";
  e.badge.classList.toggle("on", g.locked);
  // Buttons are mutually exclusive (enable ↔ release); disabled and truthfully labeled if platform not supported
  e.applyBtn.hidden = g.locked;
  e.releaseBtn.hidden = !g.locked;
  e.applyBtn.disabled = !g.supported;
  e.releaseBtn.disabled = !g.supported;
  if (!g.supported) {
    e.scope.textContent = "File lock only supports macOS / Windows";
  }
  // Three states of status (use plain language, not internal jargon):
  // Unprotected = real-time scanning statistics; Protection active · snapshots retained = recursive lock, files left read-only in place;
  // Protection active · snapshots deleted = empty directory locked, only the archived manifest is reviewable
  e.stats.textContent = g.locked
    ? g.artifactCount > 0
      ? `Protection active (snapshots retained): ${g.artifactCount} encrypted snapshots locked locally as read-only (total ${fmtBytes(g.artifactBytes)}) · ZCode cannot write new snapshots; records still viewable, click 📂 to open`
      : `Protection active (snapshots deleted): directory cleared and locked, ZCode cannot write new snapshots; pre-protection upload records fully preserved in the list below (${g.history.length} entries)`
    : g.artifactCount > 0
      ? `Locally accumulated ${g.artifactCount} encrypted snapshots · total ${fmtBytes(g.artifactBytes)} · ` +
        `covering ${g.workspaceCount} projects · ZCode recorded ${g.failureCount} upload failures`
      : `No ZCode snapshots found locally (checkpoints directory is empty, possibly never generated or already cleaned up)`;
  // Post-protection additional line: rounds of conversation since enabled (directory is unwritable while locked, new snapshots always 0)
  if (g.locked) {
    e.rounds.hidden = false;
    e.rounds.textContent = `${g.blockedRounds} rounds of conversation since protection enabled · 0 new snapshots written`;
  } else {
    e.rounds.hidden = true;
  }
}

/** Bind card and confirmation dialog interactions; execution results after confirmation are immediately refreshed by renderGuard */
export function initGuard(invoke: InvokeFn): void {
  els = {
    card: $("guard-card"),
    scope: $("guard-scope"),
    badge: $("guard-badge"),
    applyBtn: $<HTMLButtonElement>("guard-apply"),
    releaseBtn: $<HTMLButtonElement>("guard-release"),
    msg: $("guard-msg"),
    stats: $("guard-stats"),
    rounds: $("guard-rounds"),
  };
  const modal = $("guard-confirm");
  const box = $("guard-confirm-box");
  const title = $("guard-confirm-title");
  const text = $("guard-confirm-text");
  const okBtn = $<HTMLButtonElement>("guard-confirm-ok");
  const keepBtn = $<HTMLButtonElement>("guard-confirm-keep");

  /** Current pending action of the dialog (null = closed state) */
  let pendingAction: "apply" | "release" | null = null;
  let busy = false;
  let msgTimer = 0;

  const flashMsg = (s: string, err = false) => {
    els!.msg.textContent = s;
    els!.msg.classList.toggle("error", err);
    window.clearTimeout(msgTimer);
    msgTimer = window.setTimeout(() => {
      els!.msg.textContent = "";
      els!.msg.classList.remove("error");
    }, 4000);
  };

  /** **Bold** markers converted to <b> (copy is fixed from literals below, no injection surface) */
  const appendRich = (p: HTMLParagraphElement, raw: string) => {
    raw.split("**").forEach((seg, i) => {
      const el = document.createElement(i % 2 ? "b" : "span");
      el.textContent = seg;
      p.append(el);
    });
  };

  const closeConfirm = () => {
    pendingAction = null;
    modal.style.display = "none";
  };

  /** Execute protection enable/release; apply takes keepFiles (retain mode = recursive lock, delete mode = clear then lock) */
  const runAction = (cmd: string, args: Record<string, unknown>, okMsg: string, btn: HTMLButtonElement) => {
    if (busy) return;
    busy = true;
    btn.disabled = true;
    invoke<GuardStatus>(cmd, args)
      .then((st) => {
        if (st) renderGuard(st);
        flashMsg(okMsg);
        closeConfirm();
      })
      .catch((err) => flashMsg(`Execution failed: ${err}`, true))
      .finally(() => {
        busy = false;
        btn.disabled = false;
      });
  };

  const openConfirm = (action: "apply" | "release") => {
    if (!latest) return;
    pendingAction = action;
    text.replaceChildren();
    if (action === "apply") {
      // Dual-mode confirmation: retain (recommended, snapshots left read-only in place) & delete (must explicitly show that original records
      // disappear + only backup manifest — key-rules #16 informed consent). Two shared points first,
      // mode differences noted separately
      title.textContent = "Enable Snapshot Protection?";
      keepBtn.hidden = false;
      okBtn.hidden = false;
      okBtn.textContent = "Delete Snapshots & Lock";
      okBtn.classList.add("danger-btn");
      const lines = [
        // Shared
        "Enabling will sacrifice the **「Checkpoint rollback / timeline」feature** — you will no longer be able to roll back to historical checkpoints",
        "Model conversations, code completions, and tool calls **are not affected at all**",
        // Retain mode
        `**「Keep & Lock」**: the ${latest.artifactCount} encrypted snapshots accumulated locally (total ${fmtBytes(latest.artifactBytes)}) **are kept in place (read-only)**, upload records remain fully viewable, click 📂 to open the snapshot directory`,
        // Delete mode (informed consent required item by item by the user)
        `**「Delete & Lock」**: deletes all snapshots — **the original upload records will disappear**; before deletion, a backup of the record manifest is auto-created (time / workspace / encrypted size / status), viewable in the snapshot upload record list during protection, **manifest only**, snapshot file details cannot be recovered after deletion`,
        // Shared
        "Protection can be disabled at any time (kept snapshots restore in place, empty directory auto-rebuilt by ZCode)",
      ];
      for (const raw of lines) {
        const p = document.createElement("p");
        appendRich(p, raw);
        text.append(p);
      }
    } else {
      title.textContent = "Disable Snapshot Protection?";
      keepBtn.hidden = true;
      okBtn.hidden = false;
      okBtn.textContent = "Confirm Disable";
      okBtn.classList.remove("danger-btn");
      const p = document.createElement("p");
      p.textContent = "After disabling, ZCode will resume snapshot capture and upload (can be blocked again at any time; kept snapshots restore as writable).";
      text.append(p);
    }
    modal.style.display = "flex";
  };

  els.applyBtn.addEventListener("click", () => openConfirm("apply"));
  els.releaseBtn.addEventListener("click", () => openConfirm("release"));
  keepBtn.addEventListener("click", () => {
    if (pendingAction !== "apply") return;
    runAction("snapshot_guard_apply", { keepFiles: true }, "Protection enabled (snapshots retained & locked) ✓", keepBtn);
  });
  okBtn.addEventListener("click", () => {
    const action = pendingAction;
    if (!action) return;
    if (action === "apply") {
      runAction("snapshot_guard_apply", { keepFiles: false }, "Protection enabled (snapshots deleted) ✓", okBtn);
    } else {
      runAction("snapshot_guard_release", {}, "Protection disabled ✓", okBtn);
    }
  });
  $("guard-confirm-cancel").addEventListener("click", closeConfirm);
  $("guard-confirm-close").addEventListener("click", closeConfirm);
  // Click outside dialog content to close (same interaction as the settings dialog)
  window.addEventListener("mousedown", (e) => {
    if (modal.style.display !== "flex") return;
    if (box.contains(e.target as Node)) return;
    closeConfirm();
  });
  window.addEventListener("keydown", (e) => {
    if (e.key === "Escape" && modal.style.display === "flex") closeConfirm();
  });
}
