import { Loader2 } from "lucide-react";
import type { PermissionsStatus } from "../lib/ipc";
import { permissions, usePermissions } from "../lib/permissions";

type Pane = "microphone" | "system-audio";

function describe(status: PermissionsStatus): { text: string; pane: Pane | null; grant: boolean } {
  const { microphone: mic, systemAudio: sys } = status;
  if (mic === "denied" || mic === "restricted") {
    return { text: "Microphone access is off for Muesli, so recording cannot start.", pane: "microphone", grant: false };
  }
  if (sys === "denied") {
    const detail = status.systemAudioDetail ? ` (${status.systemAudioDetail}.)` : "";
    return { text: `System Audio Recording is off for Muesli, so only your side of a call would be recorded.${detail}`, pane: "system-audio", grant: false };
  }
  if (mic === "notDetermined" || sys === "notDetermined") {
    return { text: "Muesli needs Microphone and System Audio access before it can record.", pane: null, grant: true };
  }
  return { text: "Muesli could not check its permissions: the capture helper did not start.", pane: null, grant: false };
}

/**
 * One-line notice shown while a permission is missing, with the action that
 * fixes it. It reads the shared store, so it disappears on its own the moment
 * the backend reports both grants in place.
 */
export default function PermissionsBanner({ hidden }: { hidden: boolean }) {
  const { status, busy, error } = usePermissions();
  if (hidden || !status || status.ready) return null;

  const { text, pane, grant } = describe(status);

  return (
    <div className="mx-6 mb-2 flex flex-wrap items-center gap-x-3 gap-y-1 rounded-md border border-amber-300 bg-amber-50 px-3 py-2 text-xs text-amber-900 dark:border-amber-800 dark:bg-amber-950 dark:text-amber-100">
      <span>{text}</span>
      {grant && (
        <button type="button" onClick={() => permissions.request()} disabled={busy} className="underline disabled:opacity-60">
          Grant access
        </button>
      )}
      {pane && (
        <button type="button" onClick={() => permissions.openSettings(pane)} className="underline">
          Open System Settings
        </button>
      )}
      <button type="button" onClick={() => permissions.recheck()} disabled={busy} className="inline-flex items-center gap-1 underline disabled:opacity-60">
        {busy && <Loader2 className="size-3 animate-spin" />} Re-check
      </button>
      {error && <span className="text-red-700 dark:text-red-300">{error}</span>}
    </div>
  );
}
