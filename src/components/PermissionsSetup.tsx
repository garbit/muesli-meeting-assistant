import { useEffect, useRef, useState } from "react";
import { Check, Loader2, Mic, Volume2, X, type LucideIcon } from "lucide-react";
import type { PermissionsStatus } from "../lib/ipc";
import { permissions, usePermissions } from "../lib/permissions";

// Module-level: React StrictMode mounts twice in development and must not start two flows.
let flowStarted = false;

type Flow = "idle" | "running" | "done";
type RowState = "waiting" | "queued" | "granted" | "denied" | "undecided" | "unknown";
type Pane = "microphone" | "system-audio";

const STALE_HINT = "System Settings may still show Muesli as allowed: turn it off and on again, or remove it with the − button, then come back here.";

/**
 * First-launch (and first-launch-after-update) card. When the backend reports
 * that the prompts have not run for this build, it starts them at once and
 * shows what macOS is asking for; it closes itself once both grants are in and
 * stays, with instructions, when one was refused.
 */
export default function PermissionsSetup({ onVisibleChange }: { onVisibleChange: (visible: boolean) => void }) {
  const { status, busy, error } = usePermissions();
  const [flow, setFlow] = useState<Flow>("idle");
  const [visible, setVisible] = useState(false);
  const [allSet, setAllSet] = useState(false);
  const updated = useRef(false);

  useEffect(() => {
    onVisibleChange(visible);
  }, [visible, onVisibleChange]);

  // Start once, the first time the backend says this build still needs the prompts.
  useEffect(() => {
    if (!status?.needsSetup || flowStarted) return;
    flowStarted = true;
    updated.current = !status.firstRun;
    setFlow("running");
    // A first launch gets the card immediately. After an update only if the flow
    // is still running after a moment, so a silent success (macOS kept the
    // grants) never flashes a card.
    let timer: ReturnType<typeof setTimeout> | null = null;
    if (status.firstRun) setVisible(true);
    else timer = setTimeout(() => setVisible(true), 1500);
    permissions.request().finally(() => {
      if (timer) clearTimeout(timer);
      setFlow("done");
    });
  }, [status]);

  // After the flow: close shortly after everything is granted, otherwise keep
  // the card up (the focus re-probe updates it live when the user comes back
  // from System Settings).
  useEffect(() => {
    if (flow !== "done" || !status) return;
    if (!status.ready) {
      setVisible(true);
      return;
    }
    if (!visible) return;
    setAllSet(true);
    const t = setTimeout(() => {
      setVisible(false);
      setFlow("idle");
    }, 1500);
    return () => clearTimeout(t);
  }, [flow, status, visible]);

  if (!visible || !status) return null;

  const running = flow === "running" || busy;
  const micWaiting = running && status.microphone === "notDetermined";
  // A permission macOS has not decided (e.g. after a TCC reset) needs the prompt, not a re-read.
  const undecided = status.microphone === "notDetermined" || status.systemAudio === "notDetermined";
  const mic = rowState(status.microphone, running, false);
  const sys = rowState(status.systemAudio, running, micWaiting);

  return (
    <div className="mx-6 mb-4 rounded-lg border border-amber-300 bg-amber-50 p-5 text-sm text-amber-950 dark:border-amber-800 dark:bg-amber-950 dark:text-amber-50">
      <h2 className="text-base font-semibold">{allSet ? "All set" : updated.current ? "Muesli was updated" : "Welcome to Muesli"}</h2>
      <p className="mt-1 text-amber-900/80 dark:text-amber-100/80">
        {allSet
          ? "Both permissions are in place. Press Record whenever your next meeting starts."
          : updated.current
            ? "Because Muesli is not signed with a Developer ID yet, macOS treats each update as a new app and asks for its permissions again."
            : "Muesli records your side of a meeting from the microphone and the other participants from the system audio. macOS will ask for both now; nothing leaves this Mac."}
      </p>
      <ul className="mt-4 space-y-3">
        <Row icon={Mic} label="Microphone" state={mic} pane="microphone" hint={micHint(status, updated.current)} />
        <Row icon={Volume2} label="System audio" state={sys} pane="system-audio" hint={sysHint(status, updated.current)} />
      </ul>
      {error && <p className="mt-3 text-xs text-red-700 dark:text-red-300">{error}</p>}
      {!running && !allSet && (
        <div className="mt-4 flex gap-2">
          {undecided ? (
            <button
              type="button"
              onClick={() => permissions.request()}
              className="rounded-md bg-amber-600 px-3 py-1.5 text-sm font-medium text-white hover:bg-amber-700"
            >
              Grant access
            </button>
          ) : (
            <button
              type="button"
              onClick={() => permissions.recheck()}
              className="rounded-md bg-amber-600 px-3 py-1.5 text-sm font-medium text-white hover:bg-amber-700"
            >
              Re-check
            </button>
          )}
          <button
            type="button"
            onClick={() => {
              setVisible(false);
              setFlow("idle");
            }}
            className="rounded-md border border-amber-400 px-3 py-1.5 text-sm hover:bg-amber-100 dark:border-amber-700 dark:hover:bg-amber-900"
          >
            Continue anyway
          </button>
        </div>
      )}
    </div>
  );
}

function rowState(value: string, running: boolean, queued: boolean): RowState {
  switch (value) {
    case "authorized":
      return "granted";
    case "denied":
    case "restricted":
      return "denied";
    case "notDetermined":
      return running ? (queued ? "queued" : "waiting") : "undecided";
    default:
      return "unknown";
  }
}

function micHint(s: PermissionsStatus, updated: boolean): string | undefined {
  switch (s.microphone) {
    case "denied":
      return updated ? STALE_HINT : "Turn Muesli on under Privacy & Security → Microphone.";
    case "restricted":
      return "Microphone access is restricted on this Mac (a management profile or parental controls).";
    case "unavailable":
      return "The capture helper did not start, so Muesli cannot check. See the log via Settings → Diagnostics.";
    default:
      return undefined;
  }
}

function sysHint(s: PermissionsStatus, updated: boolean): string | undefined {
  if (s.systemAudio !== "denied") return undefined;
  const detail = s.systemAudioDetail ? `${capitalise(s.systemAudioDetail)}. ` : "";
  return detail + (updated ? STALE_HINT : "Turn Muesli on under Privacy & Security → Screen & System Audio Recording.");
}

const capitalise = (s: string) => s.charAt(0).toUpperCase() + s.slice(1);

function Row({ icon: Icon, label, state, hint, pane }: { icon: LucideIcon; label: string; state: RowState; hint?: string; pane: Pane }) {
  return (
    <li className="flex items-start gap-3">
      <Icon className="mt-0.5 size-4 shrink-0 text-amber-700 dark:text-amber-300" />
      <div className="min-w-0 flex-1">
        <div className="flex items-center gap-2 font-medium">
          {label}
          <StateBadge state={state} />
        </div>
        {hint && <p className="mt-0.5 text-xs text-amber-900/80 dark:text-amber-100/80">{hint}</p>}
        {state === "denied" && (
          <button type="button" onClick={() => permissions.openSettings(pane)} className="mt-1 text-xs underline">
            Open System Settings
          </button>
        )}
      </div>
    </li>
  );
}

function StateBadge({ state }: { state: RowState }) {
  switch (state) {
    case "granted":
      return (
        <span className="inline-flex items-center gap-1 text-xs font-normal text-green-700 dark:text-green-400">
          <Check className="size-3.5" /> Granted
        </span>
      );
    case "denied":
      return (
        <span className="inline-flex items-center gap-1 text-xs font-normal text-red-700 dark:text-red-400">
          <X className="size-3.5" /> Off
        </span>
      );
    case "waiting":
      return (
        <span className="inline-flex items-center gap-1 text-xs font-normal">
          <Loader2 className="size-3.5 animate-spin" /> macOS is asking…
        </span>
      );
    case "queued":
      return <span className="text-xs font-normal text-amber-900/70 dark:text-amber-100/70">next</span>;
    case "undecided":
      return <span className="text-xs font-normal text-amber-900/70 dark:text-amber-100/70">not decided yet</span>;
    default:
      return <span className="text-xs font-normal text-amber-900/70 dark:text-amber-100/70">could not check</span>;
  }
}
