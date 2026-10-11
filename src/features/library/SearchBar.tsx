import { useCallback, useEffect, useRef, useState, type ReactNode } from "react";
import { Calendar, ChevronDown, Clock, FileText, Loader2, MessageCircleQuestion, Search, Tag, Users, X } from "lucide-react";
import type { FacetCounts, FacetRef, IndexStatus, SpeakerMode } from "../../lib/ipc";
import { ipc } from "../../lib/ipc";
import { cn } from "../../lib/utils";
import { useOutsideClick } from "../../lib/useOutsideClick";

/** Everything apart from the query text; mirrors `SearchRequest`. */
export interface Filters {
  speakerIds: string[];
  speakerMode: SpeakerMode;
  dateFrom?: string;
  dateTo?: string;
  minSpeakers?: number;
  maxSpeakers?: number;
  minDurationMs?: number;
  maxDurationMs?: number;
  source?: "recorded" | "imported";
  hasSummary?: boolean;
  facets: FacetRef[];
}

export const emptyFilters: Filters = { speakerIds: [], speakerMode: "all", facets: [] };

export function filtersAreEmpty(f: Filters): boolean {
  return (
    f.speakerIds.length === 0 &&
    f.facets.length === 0 &&
    !f.dateFrom &&
    !f.dateTo &&
    f.minSpeakers == null &&
    f.maxSpeakers == null &&
    f.minDurationMs == null &&
    f.maxDurationMs == null &&
    !f.source &&
    f.hasSummary == null
  );
}

export type Mode = "search" | "ask";

const MIN = 60_000;

const speakerPresets: { label: string; min?: number; max?: number }[] = [
  { label: "1", min: 1, max: 1 },
  { label: "2", min: 2, max: 2 },
  { label: "3–4", min: 3, max: 4 },
  { label: "5+", min: 5 },
];
const durationPresets: { label: string; min?: number; max?: number }[] = [
  { label: "Under 15 min", max: 15 * MIN },
  { label: "15–45 min", min: 15 * MIN, max: 45 * MIN },
  { label: "45–90 min", min: 45 * MIN, max: 90 * MIN },
  { label: "Over 90 min", min: 90 * MIN },
];

function isoDay(d: Date): string {
  const z = new Date(d.getTime() - d.getTimezoneOffset() * MIN);
  return z.toISOString().slice(0, 10);
}

function datePreset(name: string): { from?: string; to?: string } {
  const now = new Date();
  const today = isoDay(now);
  if (name === "today") return { from: today, to: today };
  if (name === "week") {
    const d = new Date(now);
    const day = (d.getDay() + 6) % 7; // Monday = 0
    d.setDate(d.getDate() - day);
    return { from: isoDay(d), to: today };
  }
  if (name === "30") {
    const d = new Date(now);
    d.setDate(d.getDate() - 30);
    return { from: isoDay(d), to: today };
  }
  if (name === "year") return { from: `${now.getFullYear()}-01-01`, to: today };
  return {};
}

export default function SearchBar({
  query,
  onQuery,
  filters,
  onFilters,
  counts,
  index,
  mode,
  onMode,
  onAsk,
  asking,
  semantic,
}: {
  query: string;
  onQuery: (q: string) => void;
  filters: Filters;
  onFilters: (f: Filters) => void;
  counts: FacetCounts | null;
  index: IndexStatus | null;
  mode: Mode;
  onMode: (m: Mode) => void;
  onAsk: (question: string) => void;
  asking: boolean;
  /** The last response used vectors (false while keyword-only). */
  semantic: boolean | null;
}) {
  const [text, setText] = useState(query);
  const [open, setOpen] = useState<string | null>(null);
  const timer = useRef<number | null>(null);
  const inputRef = useRef<HTMLInputElement>(null);

  useEffect(() => setText(query), [query]);

  // Debounce typing in search mode; Ask only submits on Enter.
  const onType = (v: string) => {
    setText(v);
    if (mode === "ask") return;
    if (timer.current) window.clearTimeout(timer.current);
    timer.current = window.setTimeout(() => onQuery(v), 200);
  };
  useEffect(() => () => {
    if (timer.current) window.clearTimeout(timer.current);
  }, []);

  const set = (patch: Partial<Filters>) => onFilters({ ...filters, ...patch });
  const close = useCallback(() => setOpen(null), []);

  const topics = counts?.facets?.topic ?? [];
  const types = counts?.facets?.type ?? [];
  const activeFacet = (kind: string) => filters.facets.filter((f) => f.kind === kind).map((f) => f.value);
  const toggleFacet = (kind: string, value: string) => {
    const has = filters.facets.some((f) => f.kind === kind && f.value.toLowerCase() === value.toLowerCase());
    set({ facets: has ? filters.facets.filter((f) => !(f.kind === kind && f.value.toLowerCase() === value.toLowerCase())) : [...filters.facets, { kind, value }] });
  };

  const indexing = index && (index.pending > 0 || index.queued > 0 || index.running);
  const notice =
    index && !index.semanticAvailable
      ? "Keyword search only. Download EmbeddingGemma (334 MB) in Models for semantic search."
      : indexing
        ? `Indexing ${Math.max(0, index!.total - index!.pending)} of ${index!.total} meetings…`
        : semantic === false && mode === "search" && query.trim()
          ? "Keyword results only (the embedding model could not be loaded)."
          : null;

  return (
    <div className="mt-4 space-y-2">
      <div className="flex items-center gap-2">
        <div className="flex overflow-hidden rounded-md border border-neutral-300 text-xs dark:border-neutral-700">
          {(["search", "ask"] as Mode[]).map((m) => (
            <button
              key={m}
              onClick={() => {
                onMode(m);
                inputRef.current?.focus();
              }}
              className={cn("flex items-center gap-1 px-2 py-1.5 capitalize", mode === m ? "bg-neutral-200 font-medium dark:bg-neutral-800" : "text-neutral-500")}
            >
              {m === "search" ? <Search className="size-3.5" /> : <MessageCircleQuestion className="size-3.5" />} {m}
            </button>
          ))}
        </div>
        <div className="relative flex-1">
          <input
            ref={inputRef}
            value={text}
            onChange={(e) => onType(e.target.value)}
            onFocus={() => ipc.searchWarm().catch(() => {})}
            onKeyDown={(e) => {
              if (e.key === "Escape") {
                setText("");
                onQuery("");
              }
              if (e.key === "Enter") {
                if (timer.current) window.clearTimeout(timer.current);
                if (mode === "ask") onAsk(text);
                else onQuery(text);
              }
            }}
            placeholder={mode === "ask" ? "Ask a question about your meetings and press Enter…" : "Search transcripts, summaries and notes…"}
            className="w-full rounded-md border border-neutral-300 bg-transparent py-1.5 pr-8 pl-3 text-sm outline-none focus:border-amber-500 dark:border-neutral-700"
          />
          {(asking || text) && (
            <span className="absolute top-1/2 right-2 -translate-y-1/2 text-neutral-400">
              {asking ? (
                <Loader2 className="size-4 animate-spin" />
              ) : (
                <button
                  onClick={() => {
                    setText("");
                    onQuery("");
                  }}
                  title="Clear"
                >
                  <X className="size-4" />
                </button>
              )}
            </span>
          )}
        </div>
      </div>

      <div className="flex flex-wrap items-center gap-1.5 text-xs">
        <Chip
          id="people"
          open={open}
          setOpen={setOpen}
          onClose={close}
          icon={<Users className="size-3.5" />}
          label={filters.speakerIds.length ? `People: ${filters.speakerIds.length}` : "People"}
          active={filters.speakerIds.length > 0}
        >
          <div className="mb-2 flex items-center gap-2 text-[11px] text-neutral-500">
            Match
            {(["all", "any"] as SpeakerMode[]).map((m) => (
              <button key={m} onClick={() => set({ speakerMode: m })} className={cn("rounded px-1.5 py-0.5", filters.speakerMode === m ? "bg-amber-100 text-amber-800" : "hover:bg-neutral-100 dark:hover:bg-neutral-800")}>
                {m}
              </button>
            ))}
          </div>
          {(counts?.speakers ?? []).length === 0 && <p className="text-neutral-500">No named speakers yet.</p>}
          <ul className="max-h-60 space-y-0.5 overflow-auto">
            {(counts?.speakers ?? []).map((s) => {
              const checked = filters.speakerIds.includes(s.id);
              return (
                <li key={s.id}>
                  <label className="flex cursor-pointer items-center gap-2 rounded px-1 py-0.5 hover:bg-neutral-100 dark:hover:bg-neutral-800">
                    <input
                      type="checkbox"
                      checked={checked}
                      onChange={() => set({ speakerIds: checked ? filters.speakerIds.filter((x) => x !== s.id) : [...filters.speakerIds, s.id] })}
                    />
                    <span className="flex-1 truncate">{s.name}</span>
                    <span className="text-neutral-400">{s.count}</span>
                  </label>
                </li>
              );
            })}
          </ul>
        </Chip>

        <Chip
          id="date"
          open={open}
          setOpen={setOpen}
          onClose={close}
          icon={<Calendar className="size-3.5" />}
          label={filters.dateFrom || filters.dateTo ? `${filters.dateFrom ?? "…"} → ${filters.dateTo ?? "…"}` : "Date"}
          active={!!(filters.dateFrom || filters.dateTo)}
        >
          <div className="flex flex-wrap gap-1">
            {[
              ["today", "Today"],
              ["week", "This week"],
              ["30", "Last 30 days"],
              ["year", "This year"],
            ].map(([k, label]) => (
              <button key={k} onClick={() => set({ dateFrom: datePreset(k).from, dateTo: datePreset(k).to })} className="rounded border border-neutral-300 px-2 py-0.5 hover:bg-neutral-100 dark:border-neutral-700 dark:hover:bg-neutral-800">
                {label}
              </button>
            ))}
          </div>
          <div className="mt-2 flex items-center gap-2">
            <input type="date" value={filters.dateFrom ?? ""} min={counts?.dates.min ?? undefined} onChange={(e) => set({ dateFrom: e.target.value || undefined })} className="rounded border border-neutral-300 bg-transparent px-1 py-0.5 dark:border-neutral-700" />
            <span className="text-neutral-400">to</span>
            <input type="date" value={filters.dateTo ?? ""} max={counts?.dates.max ?? undefined} onChange={(e) => set({ dateTo: e.target.value || undefined })} className="rounded border border-neutral-300 bg-transparent px-1 py-0.5 dark:border-neutral-700" />
          </div>
        </Chip>

        <Chip
          id="speakers"
          open={open}
          setOpen={setOpen}
          onClose={close}
          icon={<Users className="size-3.5" />}
          label={filters.minSpeakers != null || filters.maxSpeakers != null ? `Speakers: ${speakerPresets.find((p) => p.min === filters.minSpeakers && p.max === filters.maxSpeakers)?.label ?? "custom"}` : "Speakers"}
          active={filters.minSpeakers != null || filters.maxSpeakers != null}
        >
          <Presets presets={speakerPresets} min={filters.minSpeakers} max={filters.maxSpeakers} onPick={(min, max) => set({ minSpeakers: min, maxSpeakers: max })} />
        </Chip>

        <Chip
          id="duration"
          open={open}
          setOpen={setOpen}
          onClose={close}
          icon={<Clock className="size-3.5" />}
          label={filters.minDurationMs != null || filters.maxDurationMs != null ? (durationPresets.find((p) => p.min === filters.minDurationMs && p.max === filters.maxDurationMs)?.label ?? "Duration: custom") : "Duration"}
          active={filters.minDurationMs != null || filters.maxDurationMs != null}
        >
          <Presets presets={durationPresets} min={filters.minDurationMs} max={filters.maxDurationMs} onPick={(min, max) => set({ minDurationMs: min, maxDurationMs: max })} />
        </Chip>

        <Chip id="source" open={open} setOpen={setOpen} onClose={close} icon={<FileText className="size-3.5" />} label={filters.source ? `Source: ${filters.source}` : "Source"} active={!!filters.source}>
          <div className="flex gap-1">
            {(["recorded", "imported"] as const).map((s) => (
              <button key={s} onClick={() => set({ source: filters.source === s ? undefined : s })} className={cn("rounded border px-2 py-0.5", filters.source === s ? "border-amber-500 bg-amber-50 dark:bg-amber-950" : "border-neutral-300 dark:border-neutral-700")}>
                {s}
              </button>
            ))}
          </div>
        </Chip>

        <Chip id="summary" open={open} setOpen={setOpen} onClose={close} icon={<FileText className="size-3.5" />} label={filters.hasSummary == null ? "Summary" : filters.hasSummary ? "Has summary" : "No summary"} active={filters.hasSummary != null}>
          <div className="flex gap-1">
            {[
              [true, "Has a summary"],
              [false, "No summary yet"],
            ].map(([v, label]) => (
              <button key={String(v)} onClick={() => set({ hasSummary: filters.hasSummary === v ? undefined : (v as boolean) })} className={cn("rounded border px-2 py-0.5", filters.hasSummary === v ? "border-amber-500 bg-amber-50 dark:bg-amber-950" : "border-neutral-300 dark:border-neutral-700")}>
                {label as string}
              </button>
            ))}
          </div>
        </Chip>

        {(topics.length > 0 || activeFacet("topic").length > 0) && (
          <Chip id="topic" open={open} setOpen={setOpen} onClose={close} icon={<Tag className="size-3.5" />} label={activeFacet("topic").length ? `Topics: ${activeFacet("topic").length}` : "Topics"} active={activeFacet("topic").length > 0}>
            <FacetList values={topics} active={activeFacet("topic")} onToggle={(v) => toggleFacet("topic", v)} />
          </Chip>
        )}
        {(types.length > 0 || activeFacet("type").length > 0) && (
          <Chip id="type" open={open} setOpen={setOpen} onClose={close} icon={<Tag className="size-3.5" />} label={activeFacet("type").length ? `Type: ${activeFacet("type").join(", ")}` : "Type"} active={activeFacet("type").length > 0}>
            <FacetList values={types} active={activeFacet("type")} onToggle={(v) => toggleFacet("type", v)} />
          </Chip>
        )}

        {!filtersAreEmpty(filters) && (
          <button onClick={() => onFilters(emptyFilters)} className="ml-1 flex items-center gap-1 text-neutral-500 hover:text-neutral-900 dark:hover:text-white">
            <X className="size-3.5" /> Clear filters
          </button>
        )}
        {counts && !filtersAreEmpty(filters) && <span className="ml-auto text-neutral-500">{counts.candidates} meeting(s) match the filters</span>}
      </div>
      {notice && <p className="text-xs text-neutral-500">{notice}</p>}
    </div>
  );
}

function Chip({
  id,
  open,
  setOpen,
  onClose,
  icon,
  label,
  active,
  children,
}: {
  id: string;
  open: string | null;
  setOpen: (id: string | null) => void;
  onClose: () => void;
  icon: ReactNode;
  label: string;
  active: boolean;
  children: ReactNode;
}) {
  const isOpen = open === id;
  const ref = useOutsideClick<HTMLDivElement>(onClose, isOpen);
  return (
    <div ref={ref} className="relative">
      <button
        onClick={() => setOpen(isOpen ? null : id)}
        className={cn(
          "flex items-center gap-1 rounded-full border px-2.5 py-1",
          active ? "border-amber-500 bg-amber-50 text-amber-900 dark:bg-amber-950 dark:text-amber-100" : "border-neutral-300 hover:bg-neutral-100 dark:border-neutral-700 dark:hover:bg-neutral-800",
        )}
      >
        {icon} {label} <ChevronDown className="size-3" />
      </button>
      {isOpen && (
        <div className="absolute top-full left-0 z-20 mt-1 min-w-56 rounded-md border border-neutral-200 bg-white p-2 shadow-lg dark:border-neutral-700 dark:bg-neutral-900">
          {children}
        </div>
      )}
    </div>
  );
}

function Presets({ presets, min, max, onPick }: { presets: { label: string; min?: number; max?: number }[]; min?: number; max?: number; onPick: (min?: number, max?: number) => void }) {
  return (
    <div className="flex flex-wrap gap-1">
      {presets.map((p) => {
        const on = p.min === min && p.max === max;
        return (
          <button key={p.label} onClick={() => (on ? onPick(undefined, undefined) : onPick(p.min, p.max))} className={cn("rounded border px-2 py-0.5", on ? "border-amber-500 bg-amber-50 dark:bg-amber-950" : "border-neutral-300 dark:border-neutral-700")}>
            {p.label}
          </button>
        );
      })}
    </div>
  );
}

function FacetList({ values, active, onToggle }: { values: { value: string; count: number }[]; active: string[]; onToggle: (v: string) => void }) {
  const lower = active.map((a) => a.toLowerCase());
  const extra = active.filter((a) => !values.some((v) => v.value.toLowerCase() === a.toLowerCase())).map((a) => ({ value: a, count: 0 }));
  return (
    <ul className="max-h-60 space-y-0.5 overflow-auto">
      {[...extra, ...values].map((v) => (
        <li key={v.value}>
          <label className="flex cursor-pointer items-center gap-2 rounded px-1 py-0.5 hover:bg-neutral-100 dark:hover:bg-neutral-800">
            <input type="checkbox" checked={lower.includes(v.value.toLowerCase())} onChange={() => onToggle(v.value)} />
            <span className="flex-1 truncate">{v.value}</span>
            {v.count > 0 && <span className="text-neutral-400">{v.count}</span>}
          </label>
        </li>
      ))}
    </ul>
  );
}
