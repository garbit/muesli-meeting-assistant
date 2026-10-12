import { useEffect, useRef, useState } from "react";
import { Link } from "react-router";
import ReactMarkdown from "react-markdown";
import { save } from "@tauri-apps/plugin-dialog";
import { Copy, Download, Loader2, Sparkles, Square } from "lucide-react";
import { ipc, type MeetingDetail, type RelatedMeeting, type Template } from "../../lib/ipc";
import { formatDuration } from "../../lib/utils";

/**
 * Summary tab. Streaming text is accumulated in a ref and flushed to state on a
 * 60 ms timer so ReactMarkdown is not re-run on every token; while streaming the
 * text renders as pre-formatted, Markdown only once the summary is done.
 */
export default function SummaryTab({ detail, onChanged, section }: { detail: MeetingDetail; onChanged: () => void; section?: string | null }) {
  const meetingId = detail.meeting.id;
  const [templates, setTemplates] = useState<Template[]>([]);
  const [templateId, setTemplateId] = useState<string>(detail.summaries[0]?.templateId ?? "general");
  const [running, setRunning] = useState<{ summaryId: string; text: string } | null>(null);
  const [notice, setNotice] = useState<{ kind: "error" | "info"; text: string } | null>(null);
  const [copied, setCopied] = useState(false);
  const [related, setRelated] = useState<RelatedMeeting[]>([]);
  const bufferRef = useRef<{ summaryId: string; text: string } | null>(null);
  const flushTimer = useRef<number | null>(null);
  const articleRef = useRef<HTMLElement>(null);

  useEffect(() => {
    ipc.templatesList().then(setTemplates).catch(() => {});
  }, []);

  const latestId = detail.summaries.find((s) => s.status === "done")?.id;

  // Related meetings (by summary vector); hidden when the embedding model is missing.
  useEffect(() => {
    if (!latestId) {
      setRelated([]);
      return;
    }
    ipc.meetingRelated(meetingId).then(setRelated).catch(() => setRelated([]));
  }, [meetingId, latestId]);

  // A search deep link into a section: scroll its heading into view once rendered.
  useEffect(() => {
    if (!section || !latestId) return;
    const el = Array.from(articleRef.current?.querySelectorAll("h2") ?? []).find((h) => h.textContent?.trim() === section);
    el?.scrollIntoView({ block: "start" });
  }, [section, latestId]);

  // Pick up a summary that is already running (started by the pipeline or
  // before this tab mounted).
  useEffect(() => {
    ipc
      .summaryStatus()
      .then((s) => {
        if (s && s.meetingId === meetingId) {
          bufferRef.current = { summaryId: s.summaryId, text: "" };
          setRunning({ summaryId: s.summaryId, text: "" });
        }
      })
      .catch(() => {});
  }, [meetingId]);

  useEffect(() => {
    const scheduleFlush = () => {
      if (flushTimer.current) return;
      flushTimer.current = window.setTimeout(() => {
        flushTimer.current = null;
        if (bufferRef.current) setRunning({ ...bufferRef.current });
      }, 60);
    };
    const subs = [
      ipc.onSummaryToken((e) => {
        if (e.meetingId !== meetingId) return;
        if (!bufferRef.current || bufferRef.current.summaryId !== e.summaryId) {
          bufferRef.current = { summaryId: e.summaryId, text: "" };
        }
        bufferRef.current.text += e.token ?? "";
        scheduleFlush();
      }),
      ipc.onSummaryDone((e) => {
        if (e.meetingId !== meetingId) return;
        if (e.status === "running") {
          bufferRef.current = { summaryId: e.summaryId, text: "" };
          setRunning({ summaryId: e.summaryId, text: "" });
          return;
        }
        bufferRef.current = null;
        if (flushTimer.current) {
          window.clearTimeout(flushTimer.current);
          flushTimer.current = null;
        }
        setRunning(null);
        if (e.status === "failed") setNotice({ kind: "error", text: e.error ?? "Summary failed" });
        else if (e.status === "cancelled") setNotice({ kind: "info", text: "Summary cancelled." });
        else setNotice(null);
        onChanged();
      }),
    ];
    return () => {
      subs.forEach((s) => s.then((f) => f()));
      if (flushTimer.current) window.clearTimeout(flushTimer.current);
    };
  }, [meetingId, onChanged]);

  const generate = async () => {
    setNotice(null);
    try {
      const r = await ipc.summaryGenerate(meetingId, templateId);
      bufferRef.current = { summaryId: r.summaryId, text: "" };
      setRunning({ summaryId: r.summaryId, text: "" });
    } catch (e) {
      setNotice({ kind: "error", text: String(e) });
    }
  };

  const latest = detail.summaries.find((s) => s.status === "done");

  const copy = async () => {
    try {
      const b = await ipc.meetingExport(meetingId, latest?.id);
      await navigator.clipboard.writeText(b.summaryMd);
      setCopied(true);
      window.setTimeout(() => setCopied(false), 1500);
    } catch (e) {
      setNotice({ kind: "error", text: String(e) });
    }
  };

  const exportFiles = async () => {
    try {
      const safe = detail.meeting.title.replace(/[^\w\- ]+/g, "").trim() || "meeting";
      const path = await save({ defaultPath: `${safe}.md`, filters: [{ name: "Markdown", extensions: ["md"] }] });
      if (!path) return;
      const written = await ipc.meetingExportTo(meetingId, path, latest?.id);
      setNotice({ kind: "info", text: `Saved ${written.length} files to ${path.replace(/\/[^/]*$/, "")}` });
    } catch (e) {
      setNotice({ kind: "error", text: String(e) });
    }
  };

  return (
    <div className="flex h-full flex-col">
      <div className="flex items-center gap-2 border-b border-neutral-200 px-6 py-2 dark:border-neutral-800">
        <select
          value={templateId}
          onChange={(e) => setTemplateId(e.target.value)}
          disabled={running !== null}
          className="rounded-md border border-neutral-300 bg-transparent px-2 py-1 text-xs dark:border-neutral-700"
        >
          {templates.map((t) => (
            <option key={t.id} value={t.id}>
              {t.name}
            </option>
          ))}
        </select>
        {running !== null ? (
          <button onClick={() => ipc.summaryCancel().catch(() => {})} className="flex items-center gap-1 rounded-md border border-neutral-300 px-2 py-1 text-xs dark:border-neutral-700">
            <Square className="size-3.5" /> Stop
          </button>
        ) : (
          <button
            onClick={generate}
            disabled={detail.segments.length === 0}
            className="flex items-center gap-1 rounded-md bg-amber-600 px-2.5 py-1 text-xs font-medium text-white disabled:opacity-50"
          >
            <Sparkles className="size-3.5" /> {latest ? "Regenerate" : "Summarise"}
          </button>
        )}
        <span className="flex-1" />
        {latest && running === null && (
          <>
            <button onClick={copy} className="flex items-center gap-1 rounded-md border border-neutral-300 px-2 py-1 text-xs dark:border-neutral-700">
              <Copy className="size-3.5" /> {copied ? "Copied" : "Copy Markdown"}
            </button>
            <button onClick={exportFiles} className="flex items-center gap-1 rounded-md border border-neutral-300 px-2 py-1 text-xs dark:border-neutral-700">
              <Download className="size-3.5" /> Export…
            </button>
          </>
        )}
      </div>
      {notice && (
        <p
          className={
            notice.kind === "error"
              ? "mx-6 mt-3 rounded bg-red-50 p-2 text-sm text-red-700 dark:bg-red-950 dark:text-red-300"
              : "mx-6 mt-3 rounded bg-neutral-100 p-2 text-sm text-neutral-700 dark:bg-neutral-900 dark:text-neutral-300"
          }
        >
          {notice.text}
        </p>
      )}
      <div className="min-h-0 flex-1 overflow-auto px-6 py-4">
        {running !== null ? (
          running.text === "" ? (
            <p className="flex items-center gap-2 text-sm text-neutral-500">
              <Loader2 className="size-4 animate-spin" /> Loading the model and reading the transcript…
            </p>
          ) : (
            <pre className="whitespace-pre-wrap font-sans text-sm leading-relaxed">{running.text}</pre>
          )
        ) : latest ? (
          <>
            <article ref={articleRef} className="prose prose-sm prose-neutral max-w-none dark:prose-invert">
              <ReactMarkdown>{latest.bodyMd}</ReactMarkdown>
            </article>
            <p className="mt-6 text-[11px] text-neutral-400">
              {latest.modelId} · {new Date(latest.createdAt).toLocaleString()}
            </p>
            {related.length > 0 && (
              <section className="mt-6 rounded-lg border border-neutral-200 p-4 dark:border-neutral-800">
                <h3 className="text-xs font-semibold uppercase tracking-wide text-neutral-500">Related meetings</h3>
                <ul className="mt-2 divide-y divide-neutral-100 dark:divide-neutral-800">
                  {related.map((r) => (
                    <li key={r.meeting.id}>
                      <Link to={`/meetings/${r.meeting.id}`} className="flex items-center gap-3 py-2 text-sm hover:bg-neutral-50 dark:hover:bg-neutral-900">
                        <div className="min-w-0 flex-1">
                          <div className="truncate font-medium">{r.meeting.title}</div>
                          <div className="truncate text-xs text-neutral-500">
                            {new Date(r.meeting.startedAt ?? r.meeting.createdAt).toLocaleDateString()}
                            {r.meeting.durationMs ? ` · ${formatDuration(r.meeting.durationMs)}` : ""}
                            {r.sharedSpeakers.length > 0 && ` · with ${r.sharedSpeakers.join(", ")}`}
                            {r.sharedTopics.length > 0 && ` · ${r.sharedTopics.join(", ")}`}
                          </div>
                        </div>
                        <span className="shrink-0 text-[11px] text-neutral-400">{Math.round(r.score * 100)}%</span>
                      </Link>
                    </li>
                  ))}
                </ul>
              </section>
            )}
          </>
        ) : (
          <p className="text-sm text-neutral-500">No summary yet. Pick a template and press Summarise.</p>
        )}
      </div>
    </div>
  );
}
