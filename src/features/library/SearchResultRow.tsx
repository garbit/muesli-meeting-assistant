import { memo, type ReactNode } from "react";
import { Link, useNavigate } from "react-router";
import { FileText, Heading, MessageSquare, StickyNote, Tag } from "lucide-react";
import type { SearchHit, Snippet } from "../../lib/ipc";
import { cn, formatDuration } from "../../lib/utils";

/** Where a snippet opens: the transcript at its time, or the summary at its section. */
export function snippetHref(meetingId: string, s: { kind: string; startMs?: number | null; section?: string | null }): string {
  if (s.kind === "transcript" && s.startMs != null) return `/meetings/${meetingId}?tab=transcript&t=${s.startMs}`;
  if (s.kind === "summary") return `/meetings/${meetingId}?tab=summary${s.section ? `&section=${encodeURIComponent(s.section)}` : ""}`;
  if (s.kind === "notes") return `/meetings/${meetingId}?tab=notes`;
  return `/meetings/${meetingId}`;
}

/** Render FTS highlights (« ») as <mark>. */
export function Highlighted({ text }: { text: string }) {
  if (!text.includes("«")) return <>{text}</>;
  const parts: ReactNode[] = [];
  let rest = text;
  let i = 0;
  while (rest.length > 0) {
    const a = rest.indexOf("«");
    if (a < 0) {
      parts.push(rest);
      break;
    }
    const b = rest.indexOf("»", a + 1);
    if (b < 0) {
      parts.push(rest);
      break;
    }
    if (a > 0) parts.push(rest.slice(0, a));
    parts.push(
      <mark key={i++} className="rounded-sm bg-amber-200 px-0.5 text-inherit dark:bg-amber-700">
        {rest.slice(a + 1, b)}
      </mark>,
    );
    rest = rest.slice(b + 1);
  }
  return <>{parts}</>;
}

const kindIcon: Record<string, ReactNode> = {
  transcript: <MessageSquare className="size-3.5" />,
  summary: <FileText className="size-3.5" />,
  notes: <StickyNote className="size-3.5" />,
  title: <Heading className="size-3.5" />,
  facets: <Tag className="size-3.5" />,
};

function snippetLabel(s: Snippet): string {
  if (s.kind === "transcript") return `${s.startMs != null ? formatDuration(s.startMs) : ""}${s.speaker ? ` ${s.speaker}` : ""}`.trim();
  if (s.kind === "summary") return s.section ? `## ${s.section}` : "Summary";
  if (s.kind === "notes") return "Notes";
  if (s.kind === "facets") return "Topics";
  return "Title";
}

const SearchResultRow = memo(function SearchResultRow({ hit }: { hit: SearchHit }) {
  const navigate = useNavigate();
  const m = hit.meeting;
  const date = m.startedAt ?? m.createdAt;
  const names = hit.speakers.map((s) => s.name).join(", ");
  return (
    <li>
      <Link to={`/meetings/${m.id}`} className="block px-4 py-3 hover:bg-neutral-100/70 dark:hover:bg-neutral-900">
        <div className="flex items-center gap-3">
          <div className="min-w-0 flex-1">
            <div className="truncate text-sm font-medium">{m.title}</div>
            <div className="truncate text-xs text-neutral-500">
              {new Date(date).toLocaleDateString()} · {m.durationMs ? formatDuration(m.durationMs) : "—"}
              {names && <> · {names}</>}
            </div>
          </div>
          <span className="shrink-0 rounded-full bg-neutral-200 px-2 py-0.5 text-[11px] text-neutral-700 dark:bg-neutral-800 dark:text-neutral-300">
            {hit.speakerCount} speaker{hit.speakerCount === 1 ? "" : "s"}
          </span>
        </div>
        {hit.snippets.length > 0 && (
          <ul className="mt-2 space-y-1">
            {hit.snippets.map((s) => (
              <li
                key={s.chunkId}
                onClick={(e) => {
                  e.preventDefault();
                  e.stopPropagation();
                  navigate(snippetHref(m.id, s));
                }}
                title={s.matchedBy === "both" ? "Matched by keyword and meaning" : s.matchedBy === "semantic" ? "Matched by meaning" : "Matched by keyword"}
                className={cn("flex gap-2 rounded px-2 py-1 text-xs hover:bg-amber-50 dark:hover:bg-amber-950", s.matchedBy === "semantic" && "border-l-2 border-sky-300 dark:border-sky-700")}
              >
                <span className="mt-0.5 shrink-0 text-neutral-400">{kindIcon[s.kind] ?? kindIcon.title}</span>
                <span className="w-28 shrink-0 truncate font-mono text-[11px] text-neutral-500">{snippetLabel(s)}</span>
                <span className="line-clamp-3 min-w-0 flex-1 whitespace-pre-line text-neutral-700 dark:text-neutral-300">
                  <Highlighted text={s.text} />
                </span>
              </li>
            ))}
          </ul>
        )}
      </Link>
    </li>
  );
});

export default SearchResultRow;
