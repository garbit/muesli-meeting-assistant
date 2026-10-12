import { useEffect, useMemo, useState } from "react";
import { Check, Download, Loader2 } from "lucide-react";
import { ipc, type AppInfo, type DownloadProgress, type ModelStatus } from "../../lib/ipc";
import { cn, formatBytes } from "../../lib/utils";

export default function ModelsPage() {
  const [info, setInfo] = useState<AppInfo | null>(null);
  const [status, setStatus] = useState<Record<string, ModelStatus>>({});
  const [progress, setProgress] = useState<Record<string, DownloadProgress>>({});
  const [busy, setBusy] = useState<string | null>(null);
  const [error, setError] = useState<string>("");

  const refresh = () =>
    ipc
      .modelsStatus()
      .then((list) => setStatus(Object.fromEntries(list.map((s) => [s.id, s]))))
      .catch((e) => setError(String(e)));

  useEffect(() => {
    ipc.appInfo().then(setInfo).catch((e) => setError(String(e)));
    refresh();
    const un = ipc.onModelsProgress((p) => setProgress((prev) => ({ ...prev, [p.id]: p })));
    return () => {
      un.then((f) => f());
    };
  }, []);

  const groups = useMemo(() => {
    const by: Record<string, AppInfo["models"]> = { asr: [], diarisation: [], llm: [] };
    info?.models.forEach((m) => by[m.kind]?.push(m));
    return by;
  }, [info]);

  const download = async (id: string) => {
    setBusy(id);
    setError("");
    try {
      await ipc.modelsDownload(id);
      await refresh();
    } catch (e) {
      setError(String(e));
    } finally {
      setBusy(null);
      setProgress((p) => {
        const { [id]: _omit, ...rest } = p;
        return rest;
      });
    }
  };

  return (
    <div className="p-8">
      <h1 className="text-xl font-semibold">Models</h1>
      <p className="mt-1 text-sm text-neutral-500">
        Only models on the approved list can be installed. Everything runs on this Mac.
      </p>
      {error && <p className="mt-3 rounded bg-red-50 p-2 text-sm text-red-700 dark:bg-red-950 dark:text-red-300">{error}</p>}
      {(["asr", "diarisation", "llm"] as const).map((kind) => (
        <section key={kind} className="mt-6">
          <h2 className="text-xs font-semibold uppercase tracking-wide text-neutral-500">
            {kind === "asr" ? "Speech to text" : kind === "diarisation" ? "Speaker diarisation" : "Summarisation"}
          </h2>
          <ul className="mt-2 divide-y divide-neutral-200 rounded-lg border border-neutral-200 dark:divide-neutral-800 dark:border-neutral-800">
            {groups[kind].map((m) => {
              const st = status[m.id];
              const pr = progress[m.id];
              return (
                <li key={m.id} className="flex items-center gap-4 px-4 py-3">
                  <div className="min-w-0 flex-1">
                    <div className="flex items-center gap-2 text-sm font-medium">
                      {m.name}
                      {m.default && <span className="rounded bg-amber-100 px-1.5 py-0.5 text-[10px] text-amber-800">default</span>}
                      {m.optional && <span className="rounded bg-neutral-200 px-1.5 py-0.5 text-[10px] dark:bg-neutral-800">optional</span>}
                    </div>
                    <div className="truncate text-xs text-neutral-500">
                      {m.organisation} · {m.licence} · ~{formatBytes(m.approxBytes)} · {m.minRamGb} GB RAM
                    </div>
                    {pr && (
                      <div className="mt-2 h-1.5 w-full overflow-hidden rounded bg-neutral-200 dark:bg-neutral-800">
                        <div className="h-full bg-amber-500 transition-all" style={{ width: `${Math.round(pr.fraction * 100)}%` }} />
                      </div>
                    )}
                  </div>
                  {st?.installed ? (
                    <span className="flex items-center gap-1 text-xs text-emerald-600">
                      <Check className="size-4" /> {formatBytes(st.bytesOnDisk)}
                    </span>
                  ) : (
                    <button
                      disabled={busy !== null}
                      onClick={() => download(m.id)}
                      className={cn(
                        "flex items-center gap-1 rounded-md border border-neutral-300 px-2.5 py-1 text-xs hover:bg-neutral-100 disabled:opacity-50 dark:border-neutral-700 dark:hover:bg-neutral-800",
                      )}
                    >
                      {busy === m.id ? <Loader2 className="size-3.5 animate-spin" /> : <Download className="size-3.5" />}
                      {busy === m.id ? (pr ? `${Math.round(pr.fraction * 100)}%` : "Starting…") : "Download"}
                    </button>
                  )}
                </li>
              );
            })}
          </ul>
        </section>
      ))}
    </div>
  );
}
