import { memo, useCallback, useEffect, useRef, useState } from "react";
import { Link, useSearchParams } from "react-router";
import ReactMarkdown from "react-markdown";
import { open } from "@tauri-apps/plugin-dialog";
import { FileAudio, Loader2, Square } from "lucide-react";
import { ipc, type Citation, type FacetCounts, type IndexStatus, type JobProgress, type Meeting, type SearchRequest, type SearchResponse } from "../../lib/ipc";
import { cn, formatDuration } from "../../lib/utils";
import SearchBar, { emptyFilters, filtersAreEmpty, type Filters, type Mode } from "./SearchBar";
import SearchResultRow, { snippetHref } from "./SearchResultRow";

const stateLabel: Record<Meeting["state"], string> = {
  new: "New",
  recording: "Recording",
  recorded: "Queued",
  processing: "Processing",
  transcribed: "Transcribed",
  summarised: "Summarised",
  failed: "Failed",
};

const PAGE = 50;

/** Deep links from a meeting's facet strip: `/?topic=…`, `/?type=…`, `/?person=…`. */
function filtersFromParams(p: URLSearchParams): Filters {
  const f: Filters = { ...emptyFilters, speakerIds: [], facets: [] };
  for (const kind of ["topic", "type", "project", "org"]) {
    for (const v of p.getAll(kind)) f.facets.push({ kind, value: v });
  }
  f.speakerIds = p.getAll("person");
  return f;
}

function toRequest(query: string, f: Filters): SearchRequest {
  return {
    query,
    speakerIds: f.speakerIds,
    speakerMode: f.speakerMode,
    dateFrom: f.dateFrom ?? null,
    dateTo: f.dateTo ?? null,
    minSpeakers: f.minSpeakers ?? null,
    maxSpeakers: f.maxSpeakers ?? null,
    minDurationMs: f.minDurationMs ?? null,
    maxDurationMs: f.maxDurationMs ?? null,
    source: f.source ?? null,
    hasSummary: f.hasSummary ?? null,
    facets: f.facets,
    limit: PAGE,
  };
}

interface AskState {
  askId: string | null;
  question: string;
  text: string;
  status: "running" | "done" | "failed" | "cancelled";
  citations: Citation[];
  error?: string | null;
}

export default function LibraryPage() {
  const [meetings, setMeetings] = useState<Meeting[]>([]);
  const [jobs, setJobs] = useState<Record<string, JobProgress>>({});
  const [error, setError] = useState("");
  const [params] = useSearchParams();
  const [query, setQuery] = useState(params.get("q") ?? "");
  const [filters, setFilters] = useState<Filters>(() => filtersFromParams(params));
  const [mode, setMode] = useState<Mode>("search");
  const [results, setResults] = useState<SearchResponse | null>(null);
  const [searching, setSearching] = useState(false);
  const [counts, setCounts] = useState<FacetCounts | null>(null);
  const [index, setIndex] = useState<IndexStatus | null>(null);
  const [ask, setAsk] = useState<AskState | null>(null);
  const requestNo = useRef(0);
  const askBuffer = useRef<{ askId: string; text: string } | null>(null);
  const flushTimer = useRef<number | null>(null);

  const active = query.trim() !== "" || !filtersAreEmpty(filters);

  const refresh = useCallback(() => ipc.meetingsList().then(setMeetings).catch((e) => setError(String(e))), []);

  const runSearch = useCallback(() => {
    if (!active) {
      setResults(null);
      setSearching(false);
      return;
    }
    const n = ++requestNo.current;
    setSearching(true);
    ipc
      .searchMeetings(toRequest(query, filters))
      .then((r) => {
        if (n !== requestNo.current) return; // a newer query is in flight
        setResults(r);
        setIndex(r.index);
        setError("");
      })
      .catch((e) => n === requestNo.current && setError(String(e)))
      .finally(() => n === requestNo.current && setSearching(false));
  }, [active, query, filters]);

  useEffect(() => {
    if (mode === "search") runSearch();
  }, [runSearch, mode]);

  // Chip values and counts follow the filters (not the query text).
  useEffect(() => {
    ipc.searchFacets(toRequest("", filters)).then(setCounts).catch(() => {});
  }, [filters]);

  useEffect(() => {
    refresh();
    ipc.searchStatus().then(setIndex).catch(() => {});
    let indexTimer: number | null = null;
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
      ipc.onSearchIndex((p) => {
        setIndex((i) => (i ? { ...i, pending: p.pending, queued: p.pending } : i));
        // Results can change as meetings get (re)indexed; coalesce bursts.
        if (indexTimer) window.clearTimeout(indexTimer);
        indexTimer = window.setTimeout(() => ipc.searchStatus().then(setIndex).catch(() => {}), 1500);
      }),
      ipc.onAskToken((e) => {
        if (!askBuffer.current || askBuffer.current.askId !== e.askId) askBuffer.current = { askId: e.askId, text: "" };
        askBuffer.current.text += e.token ?? "";
        if (flushTimer.current) return;
        flushTimer.current = window.setTimeout(() => {
          flushTimer.current = null;
          const b = askBuffer.current;
          if (b) setAsk((a) => (a && (a.askId === b.askId || a.askId === null) ? { ...a, askId: b.askId, text: b.text } : a));
        }, 60);
      }),
      ipc.onAskDone((e) => {
        if (e.status === "running") return;
        askBuffer.current = null;
        if (flushTimer.current) {
          window.clearTimeout(flushTimer.current);
          flushTimer.current = null;
        }
        setAsk((a) =>
          a ? { ...a, askId: e.askId, status: e.status, text: e.answer ?? a.text, citations: e.citations ?? [], error: e.error } : a,
        );
      }),
    ];
    return () => {
      subs.forEach((s) => s.then((f) => f()));
      if (indexTimer) window.clearTimeout(indexTimer);
      if (flushTimer.current) window.clearTimeout(flushTimer.current);
    };
  }, [refresh]);

  // Re-run the current search once indexing of the shown set settles.
  const indexedCount = index?.pending;
  useEffect(() => {
    if (active && mode === "search") runSearch();
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [indexedCount]);

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

  const askQuestion = async (question: string) => {
    const q = question.trim();
    if (!q) return;
    setError("");
    setAsk({ askId: null, question: q, text: "", status: "running", citations: [] });
    try {
      const r = await ipc.askMeetings(q, toRequest("", filters));
      setAsk((a) => (a ? { ...a, askId: r.askId } : a));
    } catch (e) {
      setAsk((a) => (a ? { ...a, status: "failed", error: String(e) } : a));
    }
  };

  const showResults = mode === "search" && active;

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
      <SearchBar
        query={query}
        onQuery={setQuery}
        filters={filters}
        onFilters={setFilters}
        counts={counts}
        index={index}
        mode={mode}
        onMode={setMode}
        onAsk={askQuestion}
        asking={ask?.status === "running"}
        semantic={results ? results.semantic : null}
      />
      {error && <p className="mt-3 rounded bg-red-50 p-2 text-sm text-red-700 dark:bg-red-950 dark:text-red-300">{error}</p>}

      {mode === "ask" && ask && <AskPanel ask={ask} onStop={() => ipc.askCancel().catch(() => {})} />}
      {mode === "ask" && !ask && (
        <p className="mt-6 text-sm text-neutral-500">
          Ask a question across your meetings, for example “What did we decide about pricing?”. Filters above narrow which meetings are consulted; the
          answer cites the excerpts it used.
        </p>
      )}

      {showResults && (
        <>
          <p className="mt-4 flex items-center gap-2 text-xs text-neutral-500">
            {searching && <Loader2 className="size-3.5 animate-spin" />}
            {results
              ? query.trim()
                ? `${results.totalHits} meeting(s) match${results.candidates !== results.totalHits ? ` (of ${results.candidates} after filters)` : ""}`
                : `${results.candidates} meeting(s) match the filters`
              : "Searching…"}
          </p>
          {results && results.hits.length === 0 ? (
            <p className="mt-4 text-sm text-neutral-500">
              Nothing found.{" "}
              {index && !index.semanticAvailable ? "Semantic search needs the EmbeddingGemma model (Models → Search)." : "Try fewer words, or clear a filter."}
            </p>
          ) : (
            results && (
              <ul className="mt-2 divide-y divide-neutral-200 rounded-lg border border-neutral-200 dark:divide-neutral-800 dark:border-neutral-800">
                {results.hits.map((h) => (
                  <SearchResultRow key={h.meeting.id} hit={h} />
                ))}
              </ul>
            )
          )}
        </>
      )}

      {mode === "search" && !active && (
        <>
          {meetings.length === 0 ? (
            <p className="mt-6 text-sm text-neutral-500">No meetings yet. Press Record in the sidebar before your call, or import a recording.</p>
          ) : (
            <ul className="mt-4 divide-y divide-neutral-200 rounded-lg border border-neutral-200 dark:divide-neutral-800 dark:border-neutral-800">
              {meetings.map((m) => (
                <MeetingRow key={m.id} meeting={m} job={jobs[m.id]} />
              ))}
            </ul>
          )}
        </>
      )}
    </div>
  );
}

function AskPanel({ ask, onStop }: { ask: AskState; onStop: () => void }) {
  return (
    <div className="mt-4 rounded-lg border border-neutral-200 dark:border-neutral-800">
      <div className="flex items-center gap-2 border-b border-neutral-200 px-4 py-2 text-sm dark:border-neutral-800">
        <span className="min-w-0 flex-1 truncate font-medium">{ask.question}</span>
        {ask.status === "running" ? (
          <button onClick={onStop} className="flex items-center gap-1 rounded-md border border-neutral-300 px-2 py-1 text-xs dark:border-neutral-700">
            <Square className="size-3.5" /> Stop
          </button>
        ) : (
          <span className="text-xs text-neutral-500">{ask.status === "done" ? `${ask.citations.length} citation(s)` : ask.status}</span>
        )}
      </div>
      <div className="px-4 py-3">
        {ask.status === "running" && ask.text === "" ? (
          <p className="flex items-center gap-2 text-sm text-neutral-500">
            <Loader2 className="size-4 animate-spin" /> Finding the relevant excerpts and loading the model…
          </p>
        ) : ask.status === "running" ? (
          <pre className="whitespace-pre-wrap font-sans text-sm leading-relaxed">{ask.text}</pre>
        ) : ask.status === "failed" ? (
          <p className="text-sm text-red-700 dark:text-red-300">{ask.error ?? "The question could not be answered."}</p>
        ) : (
          <article className="prose prose-sm prose-neutral max-w-none dark:prose-invert">
            <ReactMarkdown>{ask.text}</ReactMarkdown>
          </article>
        )}
        {ask.citations.length > 0 && (
          <ol className="mt-3 space-y-1 border-t border-neutral-200 pt-3 text-xs dark:border-neutral-800">
            {ask.citations.map((c) => (
              <li key={c.n}>
                <Link to={snippetHref(c.meetingId, c)} className="flex gap-2 rounded px-1 py-0.5 hover:bg-amber-50 dark:hover:bg-amber-950">
                  <span className="w-6 shrink-0 font-mono text-neutral-500">[{c.n}]</span>
                  <span className="min-w-0 flex-1 truncate">
                    <span className="font-medium">{c.title}</span> · {c.date}
                    {c.speaker && c.startMs != null ? ` · ${c.speaker} at ${formatDuration(c.startMs)}` : c.section ? ` · ${c.section}` : ""}
                  </span>
                </Link>
              </li>
            ))}
          </ol>
        )}
      </div>
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
