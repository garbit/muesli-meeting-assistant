import { useEffect, useState } from "react";
import { useNavigate } from "react-router";
import { Circle, Loader2, Square } from "lucide-react";
import { ipc, type RecordingStatus } from "../lib/ipc";
import { cn, formatDuration } from "../lib/utils";

/**
 * Persistent record/stop control for the sidebar. Subscribes to recording
 * events itself so it works from every screen; while recording it shows the
 * elapsed time and mic/system levels and links to the live meeting.
 */
export default function RecordControl() {
  const navigate = useNavigate();
  const [recording, setRecording] = useState<RecordingStatus | null>(null);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState("");

  useEffect(() => {
    ipc.recordingStatus().then(setRecording).catch(() => {});
    const subs = [
      ipc.onRecordingStatus(setRecording),
      ipc.onRecordingChanged((p) => {
        if (!p.recording) setRecording(null);
        else setRecording((r) => r ?? { meetingId: p.meetingId, elapsedMs: 0, starting: false, status: {} });
      }),
    ];
    return () => {
      subs.forEach((s) => s.then((f) => f()));
    };
  }, []);

  const toggle = async () => {
    setBusy(true);
    setError("");
    try {
      if (recording) {
        const m = await ipc.recordingStop();
        setRecording(null);
        navigate(`/meetings/${m.id}`);
      } else {
        setRecording({ meetingId: "", elapsedMs: 0, starting: true, status: {} });
        const m = await ipc.recordingStart();
        setRecording({ meetingId: m.id, elapsedMs: 0, starting: false, status: {} });
        navigate(`/meetings/${m.id}`);
      }
    } catch (e) {
      setError(String(e));
      ipc.recordingStatus().then(setRecording).catch(() => setRecording(null));
    } finally {
      setBusy(false);
    }
  };

  const st = recording?.status ?? {};
  const live = recording && !recording.starting;
  // Between Stop and the finished file (encoding a long meeting takes a while).
  const finalising = st.state === "finalising";

  return (
    <div className="mt-auto border-t border-neutral-200 p-3 dark:border-neutral-800">
      {live ? (
        <div className="rounded-md border border-red-200 bg-red-50 p-2.5 dark:border-red-900 dark:bg-red-950">
          <button
            type="button"
            onClick={() => navigate(`/meetings/${recording.meetingId}`)}
            title="Open the live meeting"
            className="flex w-full items-center gap-2 text-left text-sm font-medium text-red-700 dark:text-red-300"
          >
            {finalising ? (
              <Loader2 className="size-3 animate-spin" />
            ) : (
              <span className="relative flex size-2.5">
                <span className="absolute inline-flex size-full animate-ping rounded-full bg-red-500 opacity-60" />
                <span className="relative inline-flex size-2.5 rounded-full bg-red-600" />
              </span>
            )}
            {finalising ? "Finalising…" : "Recording"}
            <span className="ml-auto font-mono text-xs tabular-nums">{formatDuration(recording.elapsedMs)}</span>
          </button>
          {!finalising && (
            <div className="mt-2 space-y-1">
              <Level label="Mic" value={st.micRms ?? 0} />
              <Level label="Call" value={st.sysRms ?? 0} warn={st.systemAudio === "silent" || st.systemAudio === "missing"} />
            </div>
          )}
          <button
            type="button"
            onClick={toggle}
            disabled={busy || finalising}
            className="mt-2 flex w-full items-center justify-center gap-1.5 rounded-md bg-red-600 py-1.5 text-sm font-medium text-white hover:bg-red-700 disabled:opacity-60"
          >
            {busy || finalising ? <Loader2 className="size-4 animate-spin" /> : <Square className="size-3.5" />} Stop
          </button>
        </div>
      ) : (
        <button
          type="button"
          onClick={toggle}
          disabled={busy || recording?.starting}
          className="flex w-full items-center justify-center gap-1.5 rounded-md bg-amber-600 py-2 text-sm font-medium text-white hover:bg-amber-700 disabled:opacity-60"
        >
          {busy || recording?.starting ? <Loader2 className="size-4 animate-spin" /> : <Circle className="size-4" />}
          {recording?.starting ? "Starting…" : "Record"}
        </button>
      )}
      {error && <p className="mt-2 text-xs text-red-600 dark:text-red-400">{error}</p>}
    </div>
  );
}

function Level({ label, value, warn }: { label: string; value: number; warn?: boolean }) {
  // RMS is tiny for speech (~0.01–0.1); a square root spreads it over the bar.
  const pct = Math.min(100, Math.round(Math.sqrt(value) * 160));
  return (
    <div className="flex items-center gap-2 text-[11px] text-neutral-600 dark:text-neutral-300">
      <span className="w-7">{label}</span>
      <span className={cn("inline-block h-1.5 flex-1 overflow-hidden rounded", warn ? "bg-amber-200" : "bg-red-200/70 dark:bg-red-900")}>
        <span className={cn("block h-full transition-[width] duration-100", warn ? "bg-amber-500" : "bg-red-600")} style={{ width: `${pct}%` }} />
      </span>
    </div>
  );
}
