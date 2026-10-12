// One shared permission state for the whole UI. The backend owns the truth and
// pushes every change as `permissions:changed`; this store keeps the latest
// value so the setup card, the banner, Settings and the Record button all agree
// and all clear at the same moment.
import { useSyncExternalStore } from "react";
import { ipc, type PermissionsStatus } from "./ipc";

export interface PermissionsSnapshot {
  status: PermissionsStatus | null;
  /** A request or recheck is in flight (a macOS prompt may be up). */
  busy: boolean;
  error: string;
}

export const MIC_OFF_MESSAGE =
  "Microphone access is off for Muesli. Turn it on in System Settings → Privacy & Security → Microphone, then try again.";

export const isMicBlocked = (s: PermissionsStatus | null | undefined) => s?.microphone === "denied" || s?.microphone === "restricted";

let snapshot: PermissionsSnapshot = { status: null, busy: false, error: "" };
const listeners = new Set<() => void>();
let started = false;
let inflight: Promise<PermissionsStatus | null> | null = null;

function set(patch: Partial<PermissionsSnapshot>) {
  snapshot = { ...snapshot, ...patch };
  listeners.forEach((l) => l());
}

function start() {
  if (started) return;
  started = true;
  // Subscribe before the first fetch: an event emitted before the listener exists is lost.
  ipc.onPermissionsChanged((status) => set({ status })).catch(() => {});
  ipc
    .permissionsStatus()
    .then((status) => set({ status }))
    .catch((e) => set({ error: String(e) }));
}

/** Run one backend action at a time; a second caller shares the first one's result. */
function run(action: () => Promise<PermissionsStatus>): Promise<PermissionsStatus | null> {
  if (inflight) return inflight;
  set({ busy: true, error: "" });
  inflight = action()
    .then(
      (status) => {
        set({ status });
        return status;
      },
      (e) => {
        set({ error: String(e) });
        return snapshot.status;
      },
    )
    .finally(() => {
      inflight = null;
      set({ busy: false });
    });
  return inflight;
}

export const permissions = {
  /** Show the macOS prompts that are still pending, then check the system audio tap. */
  request: () => run(() => ipc.permissionsRequest()),
  /** Re-read both permissions; never prompts once macOS has decided. */
  recheck: () => run(() => ipc.permissionsRecheck()),
  openSettings: (pane: "microphone" | "system-audio") => ipc.permissionsOpenSettings(pane).catch((e) => set({ error: String(e) })),
  current: () => snapshot.status,
};

function subscribe(listener: () => void) {
  listeners.add(listener);
  start();
  return () => {
    listeners.delete(listener);
  };
}

export function usePermissions(): PermissionsSnapshot {
  return useSyncExternalStore(subscribe, () => snapshot);
}
