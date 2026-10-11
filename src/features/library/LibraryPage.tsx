import { memo, useEffect, useState } from "react";
import { Link } from "react-router";
import { open } from "@tauri-apps/plugin-dialog";
import { FileAudio } from "lucide-react";
import { ipc, type JobProgress, type Meeting } from "../../lib/ipc";
import { cn, formatDuration } from "../../lib/utils";

const stateLabel: Record<Meeting["state"], string> = {
  new: "New",
  recording: "Recording",
  recorded: "Queued",
  processing: "Processing",
  transcribed: "Transcribed",
  summarised: "Summarised",
  failed: "Failed",
};

export default function LibraryPage() {
  const [meetings, setMeetings] = useState<Meeting[]>([]);
  const [jobs, setJobs] = useState<Record<string, JobProgress>>({});
  const [error, setError] = useState("");

  const refresh = () => ipc.meetingsList().then(setMeetings).catch((e) => setError(String(e)));

  useEffect(() => {
    refresh();
    const subs = [
      ipc.onRecordingChanged(() => refresh()),
      ipc.onJobProgress((p) => {
        setJobs((j) => {
          const next = { ...j, [p.meetingId]: p };
          if (p.state !== "running") delete next[p.meetingId];
          return next;
        });
        if (p.state !== "running") refresh();
      }),
    ];
    return () => {
      subs.forEach((s) => s.then((f) => f()));
    };
  }, []);

  const importFile = async () => {
    const picked = await open({
      multiple: false,
      directory: false,
      filters: [{ name: "Audio or video", extensions: ["m4a", "mp3", "wav", "aac", "mp4", "mov", "aiff", "caf", "flac"] }],
    });
    if (!picked) return;
    setError("");
    try {
      await ipc.importFile(picked as string);
      await refresh();
    } catch (e) {
      setError(String(e));
    }
  };

  return (
    <div className="p-8">
      <div className="flex items-center justify-between">
        <h1 className="text-xl font-semibold">Meetings</h1>
        <div className="flex items-center gap-2">
          <button
            onClick={importFile}
            className="flex items-center gap-1.5 rounded-md border border-neutral-300 px-3 py-1.5 text-sm hover:bg-neutral-100 dark:border-neutral-700 dark:hover:bg-neutral-800"
          >
            <FileAudio className="size-4" /> Import…
          </button>
        </div>
      </div>
      {error && <p className="mt-3 rounded bg-red-50 p-2 text-sm text-red-700 dark:bg-red-950 dark:text-red-300">{error}</p>}
      {meetings.length === 0 ? (
        <p className="mt-6 text-sm text-neutral-500">No meetings yet. Press Record in the sidebar before your call, or import a recording.</p>
      ) : (
        <ul className="mt-4 divide-y divide-neutral-200 rounded-lg border border-neutral-200 dark:divide-neutral-800 dark:border-neutral-800">
          {meetings.map((m) => (
            <MeetingRow key={m.id} meeting={m} job={jobs[m.id]} />
          ))}
        </ul>
      )}
    </div>
  );
}

const MeetingRow = memo(function MeetingRow({ meeting: m, job }: { meeting: Meeting; job?: JobProgress }) {
  return (
    <li>
      <Link to={`/meetings/${m.id}`} className="flex items-center gap-4 px-4 py-3 hover:bg-neutral-100/70 dark:hover:bg-neutral-900">
        <div className="min-w-0 flex-1">
          <div className="truncate text-sm font-medium">{m.title}</div>
          <div className="text-xs text-neutral-500">
            {new Date(m.createdAt).toLocaleString()} · {m.durationMs ? formatDuration(m.durationMs) : "—"} · {m.source}
          </div>
          {m.state === "failed" && m.lastError && (
            <div className="mt-1 truncate text-xs text-red-600 dark:text-red-400" title={m.lastError}>
              {m.lastError}
            </div>
          )}
          {job && job.state === "running" && (
            <div className="mt-1.5 flex items-center gap-2 text-xs text-neutral-500">
              <div className="h-1 w-40 overflow-hidden rounded bg-neutral-200 dark:bg-neutral-800">
                <div className="h-full bg-amber-500" style={{ width: `${Math.round(job.fraction * 100)}%` }} />
              </div>
              {job.stage}
            </div>
          )}
        </div>
        <span
          className={cn(
            "rounded-full px-2 py-0.5 text-[11px]",
            m.state === "failed"
              ? "bg-red-100 text-red-700"
              : m.state === "processing" || m.state === "recording"
                ? "bg-amber-100 text-amber-800"
                : "bg-neutral-200 text-neutral-700 dark:bg-neutral-800 dark:text-neutral-300",
          )}
        >
          {stateLabel[m.state] ?? m.state}
        </span>
      </Link>
    </li>
  );
});
