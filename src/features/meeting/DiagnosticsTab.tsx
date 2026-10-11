import { useEffect, useState } from "react";
import { FolderOpen, RefreshCw } from "lucide-react";
import { ipc, type JobRow, type MeetingLogs } from "../../lib/ipc";
import { cn } from "../../lib/utils";

/** Job history, the files in the meeting folder and the sidecar logs for one meeting. */
export default function DiagnosticsTab({ meetingId }: { meetingId: string }) {
  const [jobs, setJobs] = useState<JobRow[]>([]);
  const [logs, setLogs] = useState<MeetingLogs | null>(null);
  const [error, setError] = useState("");

  const load = () => {
    ipc.meetingJobs(meetingId).then(setJobs).catch((e) => setError(String(e)));
    ipc.meetingLogs(meetingId, 150).then(setLogs).catch((e) => setError(String(e)));
  };
  useEffect(load, [meetingId]);

  return (
    <div className="space-y-6 p-6 text-sm">
      <div className="flex items-center gap-2">
        <h2 className="text-xs font-semibold uppercase tracking-wide text-neutral-500">Processing history</h2>
        <span className="flex-1" />
        <button onClick={load} className="flex items-center gap-1 rounded-md border border-neutral-300 px-2 py-1 text-xs dark:border-neutral-700">
          <RefreshCw className="size-3.5" /> Refresh
        </button>
        <button onClick={() => ipc.openDataFolder(meetingId).catch((e) => setError(String(e)))} className="flex items-center gap-1 rounded-md border border-neutral-300 px-2 py-1 text-xs dark:border-neutral-700">
          <FolderOpen className="size-3.5" /> Open folder
        </button>
      </div>
      {error && <p className="rounded bg-red-50 p-2 text-sm text-red-700 dark:bg-red-950 dark:text-red-300">{error}</p>}
      {jobs.length === 0 ? (
        <p className="text-neutral-500">No processing jobs yet.</p>
      ) : (
        <table className="w-full text-xs">
          <thead className="text-left text-neutral-500">
            <tr>
              <th className="py-1 pr-3">Started</th>
              <th className="py-1 pr-3">Kind</th>
              <th className="py-1 pr-3">Stage</th>
              <th className="py-1 pr-3">State</th>
              <th className="py-1">Error</th>
            </tr>
          </thead>
          <tbody>
            {jobs.map((j) => (
              <tr key={j.id} className="border-t border-neutral-200 align-top dark:border-neutral-800">
                <td className="py-1 pr-3 whitespace-nowrap">{new Date(j.createdAt).toLocaleString()}</td>
                <td className="py-1 pr-3">{j.kind}</td>
                <td className="py-1 pr-3">{j.stage ?? ""}</td>
                <td className={cn("py-1 pr-3", j.state === "failed" ? "text-red-600" : j.state === "done" ? "text-emerald-600" : "")}>{j.state}</td>
                <td className="py-1 font-mono break-all">{j.error ?? ""}</td>
              </tr>
            ))}
          </tbody>
        </table>
      )}
      {logs && (
        <>
          <section>
            <h2 className="text-xs font-semibold uppercase tracking-wide text-neutral-500">Files</h2>
            <ul className="mt-1 font-mono text-xs text-neutral-600 dark:text-neutral-400">
              {logs.files.map((f) => (
                <li key={f}>{f}</li>
              ))}
            </ul>
          </section>
          {logs.captureLog && (
            <section>
              <h2 className="text-xs font-semibold uppercase tracking-wide text-neutral-500">Capture log</h2>
              <pre className="mt-1 max-h-64 overflow-auto rounded bg-neutral-100 p-3 text-[11px] leading-snug dark:bg-neutral-900">{logs.captureLog}</pre>
            </section>
          )}
          {logs.processLog && (
            <section>
              <h2 className="text-xs font-semibold uppercase tracking-wide text-neutral-500">Processing log</h2>
              <pre className="mt-1 max-h-96 overflow-auto rounded bg-neutral-100 p-3 text-[11px] leading-snug dark:bg-neutral-900">{logs.processLog}</pre>
            </section>
          )}
        </>
      )}
    </div>
  );
}
