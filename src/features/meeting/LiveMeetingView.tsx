import { useEffect, useRef, useState } from "react";
import { ipc, type LiveSegment } from "../../lib/ipc";
import { cn, formatDuration } from "../../lib/utils";

/**
 * The meeting screen while recording: notes on the left (what Granola calls
 * the notepad), the near-real-time transcript on the right with Me/Them labels
 * and a tentative tail, so the user can see that capture is working.
 */
export default function LiveMeetingView({
  meetingId,
  notes,
  onNotes,
  liveEnabled,
}: {
  meetingId: string;
  notes: string;
  onNotes: (v: string) => void;
  liveEnabled: boolean;
}) {
  const [confirmed, setConfirmed] = useState<LiveSegment[]>([]);
  const [volatile, setVolatile] = useState<LiveSegment | null>(null);
  const scroller = useRef<HTMLDivElement>(null);
  // Auto-scroll follows the newest text until the user scrolls up to read.
  const follow = useRef(true);

  useEffect(() => {
    setConfirmed([]);
    setVolatile(null);
    follow.current = true;
    const un = ipc.onRecordingTranscript((e) => {
      if (e.meetingId !== meetingId) return;
      if (e.segment.confirmed) {
        setConfirmed((c) => [...c, e.segment]);
        setVolatile(null);
      } else {
        setVolatile(e.segment);
      }
    });
    // The snapshot includes every segment emitted before it was taken, so it
    // supersedes whatever arrived through events in the meantime when it is longer.
    ipc
      .recordingLiveTranscript()
      .then((t) => {
        if (!t) return;
        setConfirmed((c) => (t.confirmed.length >= c.length ? t.confirmed : c));
        if (t.volatile) setVolatile((v) => v ?? t.volatile ?? null);
      })
      .catch(() => {});
    return () => {
      un.then((f) => f());
    };
  }, [meetingId]);

  useEffect(() => {
    const el = scroller.current;
    if (el && follow.current) el.scrollTop = el.scrollHeight;
  }, [confirmed, volatile]);

  return (
    <div className="grid h-full grid-cols-2 divide-x divide-neutral-200 dark:divide-neutral-800">
      <section className="flex min-h-0 flex-col">
        <h2 className="px-6 pt-4 text-xs font-semibold uppercase tracking-wide text-neutral-500">Your notes</h2>
        <textarea
          value={notes}
          onChange={(e) => onNotes(e.target.value)}
          placeholder="Write notes as you go. Keywords are enough; the summary combines them with the transcript when the meeting ends."
          className="min-h-0 flex-1 resize-none bg-transparent px-6 py-3 text-sm leading-relaxed outline-none"
          autoFocus
        />
      </section>
      <section className="flex min-h-0 flex-col">
        <h2 className="flex items-center gap-2 px-6 pt-4 text-xs font-semibold uppercase tracking-wide text-neutral-500">
          Live transcript
          <span className="relative flex size-2">
            <span className="absolute inline-flex size-full animate-ping rounded-full bg-red-500 opacity-60" />
            <span className="relative inline-flex size-2 rounded-full bg-red-600" />
          </span>
        </h2>
        <div
          ref={scroller}
          className="min-h-0 flex-1 overflow-auto px-6 py-3"
          onScroll={(e) => {
            const el = e.currentTarget;
            follow.current = el.scrollHeight - el.scrollTop - el.clientHeight < 40;
          }}
        >
          {!liveEnabled ? (
            <p className="text-sm text-neutral-500">Live transcript is off. Turn it on in Settings; the full transcript is produced when the meeting ends.</p>
          ) : confirmed.length === 0 && !volatile ? (
            <p className="text-sm text-neutral-500">Listening… the first words appear after about ten seconds of speech.</p>
          ) : (
            <ol className="space-y-2.5">
              {confirmed.map((s, i) => (
                <Line key={i} seg={s} />
              ))}
              {volatile && <Line seg={volatile} tentative />}
            </ol>
          )}
        </div>
      </section>
    </div>
  );
}

function Line({ seg, tentative }: { seg: LiveSegment; tentative?: boolean }) {
  const who = seg.who === "me" ? "Me" : seg.who === "them" ? "Them" : "";
  return (
    <li className={cn("flex gap-3 text-sm", tentative && "text-neutral-400")}>
      <span className="w-12 shrink-0 pt-0.5 font-mono text-xs text-neutral-400">{formatDuration(seg.startMs)}</span>
      <div>
        {who && <span className={cn("mr-2 font-medium", seg.who === "me" ? "text-amber-700" : "text-sky-700")}>{who}</span>}
        <span>{seg.text}</span>
        {tentative && <span className="ml-0.5 inline-block h-4 w-px animate-pulse bg-neutral-400 align-middle" />}
      </div>
    </li>
  );
}
