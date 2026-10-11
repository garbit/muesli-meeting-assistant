import { useCallback, useEffect, useMemo, useRef, useState, type Ref } from "react";
import { Link, useNavigate, useParams } from "react-router";
import { ArrowLeft, FileAudio, Loader2, RefreshCw, Square, Trash2, X } from "lucide-react";
import { ipc, type JobProgress, type LiveSegment, type MeetingDetail, type Settings, type Speaker } from "../../lib/ipc";
import { cn, formatDuration } from "../../lib/utils";
import { confirmDialog } from "../../lib/dialogs";
import InlinePrompt from "../../components/InlinePrompt";
import SummaryTab from "./SummaryTab";
import DiagnosticsTab from "./DiagnosticsTab";
import LiveMeetingView from "./LiveMeetingView";
import Player, { type PlayerHandle } from "../../components/Player";

type Tab = "notes" | "transcript" | "summary" | "diagnostics";

export default function MeetingPage() {
  const { id = "" } = useParams();
  const navigate = useNavigate();
  const [detail, setDetail] = useState<MeetingDetail | null>(null);
  const [tab, setTab] = useState<Tab>("transcript");
  const [job, setJob] = useState<JobProgress | null>(null);
  const [error, setError] = useState("");
  const [notes, setNotes] = useState("");
  const [renaming, setRenaming] = useState<string | null>(null);
  const [known, setKnown] = useState<Speaker[]>([]);
  const [isLive, setIsLive] = useState(false);
  const [settings, setSettings] = useState<Settings | null>(null);
  const [liveFallback, setLiveFallback] = useState<LiveSegment[]>([]);
  // Index of the transcript row under the playhead; only changes when the row does.
  const [activeIndex, setActiveIndex] = useState(-1);
  const player = useRef<PlayerHandle>(null);
  const activeRef = useRef<HTMLLIElement>(null);
  const rowsRef = useRef<{ startMs: number }[]>([]);
  const saveTimer = useRef<number | null>(null);
  const pendingNotes = useRef<string | null>(null);
  const seededFor = useRef<string | null>(null);

  const flushNotes = useCallback(async () => {
    if (saveTimer.current) {
      window.clearTimeout(saveTimer.current);
      saveTimer.current = null;
    }
    if (pendingNotes.current !== null) {
      const v = pendingNotes.current;
      pendingNotes.current = null;
      await ipc.meetingUpdate(id, { notesMd: v }).catch((e) => setError(String(e)));
    }
  }, [id]);

  const load = useCallback(async () => {
    await flushNotes();
    try {
      const d = await ipc.meetingGet(id);
      setDetail(d);
      // Seed the notes editor once per meeting; later reloads must not clobber typing.
      if (d && seededFor.current !== id) {
        seededFor.current = id;
        setNotes(d.meeting.notesMd);
      }
    } catch (e) {
      setError(String(e));
    }
  }, [id, flushNotes]);

  useEffect(() => {
    seededFor.current = null;
    setActiveIndex(-1);
    setJob(null);
    load();
    ipc.speakersList().then(setKnown).catch(() => {});
    ipc.settingsGet().then(setSettings).catch(() => {});
    ipc.recordingStatus().then((r) => setIsLive(!!r && r.meetingId === id)).catch(() => {});
    ipc.meetingLiveTranscript(id).then(setLiveFallback).catch(() => {});
    const subs = [
      ipc.onJobProgress((p) => {
        if (p.meetingId !== id) return;
        setJob(p);
        if (p.state !== "running") load();
      }),
      ipc.onRecordingChanged((p) => {
        if (p.meetingId !== id) return;
        setIsLive(p.recording);
        if (!p.recording) {
          ipc.meetingLiveTranscript(id).then(setLiveFallback).catch(() => {});
          load();
        }
      }),
    ];
    return () => {
      subs.forEach((s) => s.then((f) => f()));
      flushNotes();
    };
  }, [id, load, flushNotes]);

  // Keep the segment under the playhead in view.
  useEffect(() => {
    activeRef.current?.scrollIntoView({ block: "nearest" });
  }, [activeIndex]);

  const names = useMemo(() => {
    const map: Record<string, string> = {};
    detail?.speakers.forEach((s, i) => {
      map[s.diarLabel] = s.displayName || `Speaker ${i + 1}`;
    });
    return map;
  }, [detail]);

  // The transcript shown: the full one with speakers, or the live one until then.
  const rows: { startMs: number }[] = detail && detail.segments.length > 0 ? detail.segments : liveFallback;
  useEffect(() => {
    rowsRef.current = rows;
  }, [rows]);

  const onTime = useCallback((ms: number) => {
    const idx = indexAt(rowsRef.current, ms);
    setActiveIndex((cur) => (cur === idx ? cur : idx));
  }, []);

  const onNotes = (v: string) => {
    setNotes(v);
    pendingNotes.current = v;
    if (saveTimer.current) window.clearTimeout(saveTimer.current);
    saveTimer.current = window.setTimeout(() => {
      saveTimer.current = null;
      flushNotes();
    }, 600);
  };

  const assign = async (label: string, name: string) => {
    setRenaming(null);
    try {
      await ipc.speakerAssign(id, label, name);
      ipc.speakersList().then(setKnown).catch(() => {});
      await load();
    } catch (e) {
      setError(String(e));
    }
  };

  const deleteMeeting = async () => {
    if (!(await confirmDialog("Delete this meeting, its recording, transcript and summaries?", { okLabel: "Delete", danger: true }))) return;
    try {
      await ipc.meetingDelete(id);
      navigate("/");
    } catch (e) {
      setError(String(e));
    }
  };

  const deleteAudio = async () => {
    if (!(await confirmDialog("Delete this meeting's audio? The transcript and summary stay.", { okLabel: "Delete audio", danger: true }))) return;
    await ipc.recordingDelete(id).catch((e) => setError(String(e)));
    load();
  };

  if (!detail) {
    return <div className="p-8 text-sm text-neutral-500">{error || "Loading…"}</div>;
  }
  const m = detail.meeting;
  const processing = job?.state === "running" || m.state === "processing";
  const recordingPath = detail.recording?.path;
  const seek = (ms: number) => player.current?.seek(ms);

  if (isLive) {
    return (
      <div className="flex h-full flex-col">
        <header className="flex items-center gap-3 border-b border-neutral-200 px-6 py-3 dark:border-neutral-800">
          <Link to="/" className="text-neutral-500 hover:text-neutral-900 dark:hover:text-white">
            <ArrowLeft className="size-4" />
          </Link>
          <input
            className="min-w-0 flex-1 bg-transparent text-lg font-semibold outline-none"
            defaultValue={m.title}
            onBlur={(e) => {
              if (e.target.value !== m.title) ipc.meetingUpdate(id, { title: e.target.value }).catch((er) => setError(String(er)));
            }}
          />
          <span className="rounded-full bg-red-100 px-2 py-0.5 text-[11px] font-medium text-red-700 dark:bg-red-950 dark:text-red-300">Recording</span>
        </header>
        <div className="min-h-0 flex-1">
          <LiveMeetingView meetingId={id} notes={notes} onNotes={onNotes} liveEnabled={settings?.liveTranscript ?? true} />
        </div>
      </div>
    );
  }

  const bannerText = error || job?.error || (m.state === "failed" ? m.lastError : null);

  return (
    <div className="flex h-full flex-col">
      <header className="flex items-center gap-3 border-b border-neutral-200 px-6 py-3 dark:border-neutral-800">
        <Link to="/" className="text-neutral-500 hover:text-neutral-900 dark:hover:text-white">
          <ArrowLeft className="size-4" />
        </Link>
        <input
          className="min-w-0 flex-1 bg-transparent text-lg font-semibold outline-none"
          defaultValue={m.title}
          onBlur={(e) => {
            if (e.target.value !== m.title) ipc.meetingUpdate(id, { title: e.target.value }).then(load).catch((er) => setError(String(er)));
          }}
        />
        <span className="text-xs text-neutral-500">
          {m.durationMs ? formatDuration(m.durationMs) : ""} · {m.state}
        </span>
        {detail.recording && (
          <button title="Delete the audio recording (keeps transcript and summary)" onClick={deleteAudio} className="text-neutral-500 hover:text-neutral-900 dark:hover:text-white">
            <FileAudio className="size-4" />
          </button>
        )}
        {processing ? (
          <button title="Cancel processing" onClick={() => ipc.meetingCancelProcessing(id).catch((e) => setError(String(e)))} className="text-neutral-500 hover:text-red-600">
            <Square className="size-4" />
          </button>
        ) : (
          detail.recording && (
            <button title="Re-run processing" onClick={() => ipc.meetingProcess(id).catch((e) => setError(String(e)))} className="text-neutral-500 hover:text-neutral-900 dark:hover:text-white">
              <RefreshCw className="size-4" />
            </button>
          )
        )}
        <button title="Delete meeting" onClick={deleteMeeting} className="text-neutral-500 hover:text-red-600">
          <Trash2 className="size-4" />
        </button>
      </header>
      {job && job.state === "running" && (
        <div className="flex items-center gap-2 border-b border-amber-200 bg-amber-50 px-6 py-1.5 text-xs text-amber-800 dark:border-amber-900 dark:bg-amber-950 dark:text-amber-200">
          <Loader2 className="size-3.5 animate-spin" /> {job.stage} · {Math.round(job.fraction * 100)}%
        </div>
      )}
      {bannerText && (
        <div className="flex items-center gap-3 border-b border-red-200 bg-red-50 px-6 py-1.5 text-xs text-red-700 dark:bg-red-950 dark:text-red-300">
          <span className="min-w-0 flex-1 truncate" title={bannerText}>
            {bannerText}
          </span>
          {m.state === "failed" && !processing && detail.recording && (
            <button onClick={() => ipc.meetingProcess(id).catch((e) => setError(String(e)))} className="shrink-0 rounded border border-red-300 px-2 py-0.5 hover:bg-red-100 dark:hover:bg-red-900">
              Retry
            </button>
          )}
          <button onClick={() => setTab("diagnostics")} className="shrink-0 underline">
            Details
          </button>
          {error && (
            <button onClick={() => setError("")} title="Dismiss" className="shrink-0">
              <X className="size-3.5" />
            </button>
          )}
        </div>
      )}
      <nav className="flex gap-1 border-b border-neutral-200 px-6 dark:border-neutral-800">
        {(["notes", "transcript", "summary", "diagnostics"] as Tab[]).map((t) => (
          <button
            key={t}
            onClick={() => setTab(t)}
            className={cn(
              "border-b-2 px-3 py-2 text-sm capitalize",
              tab === t ? "border-amber-600 font-medium" : "border-transparent text-neutral-500 hover:text-neutral-900 dark:hover:text-white",
            )}
          >
            {t}
          </button>
        ))}
      </nav>
      <div className="min-h-0 flex-1 overflow-auto">
        {tab === "notes" && (
          <textarea
            value={notes}
            onChange={(e) => onNotes(e.target.value)}
            placeholder="Type notes during the meeting. They are passed to the summariser alongside the transcript."
            className="h-full w-full resize-none bg-transparent p-6 font-mono text-sm outline-none"
          />
        )}
        {tab === "transcript" && (
          <div className="p-6">
            {detail.speakers.length > 0 && (
              <div className="mb-4 flex flex-wrap items-center gap-2">
                {detail.speakers.map((s, i) => (
                  <button
                    key={s.diarLabel}
                    onClick={() => setRenaming(renaming === s.diarLabel ? null : s.diarLabel)}
                    className={cn(
                      "rounded-full border border-neutral-300 px-2.5 py-0.5 text-xs hover:bg-neutral-100 dark:border-neutral-700 dark:hover:bg-neutral-800",
                      renaming === s.diarLabel && "border-amber-500",
                    )}
                    title={`${Math.round(s.speechMs / 1000)}s of speech${s.micRatio != null ? ` · mic ${Math.round(s.micRatio * 100)}%` : ""}. Click to name this voice.`}
                  >
                    <span className={cn("mr-1 inline-block size-2 rounded-full", palette[i % palette.length])} />
                    {names[s.diarLabel]}
                  </button>
                ))}
                {renaming && (
                  <InlinePrompt
                    label="Who is this?"
                    initial={names[renaming]?.startsWith("Speaker ") ? "" : names[renaming]}
                    placeholder="Name, or “Me”"
                    suggestions={["Me", ...known.filter((k) => !k.isMe).map((k) => k.name)]}
                    onSubmit={(v) => assign(renaming, v)}
                    onCancel={() => setRenaming(null)}
                  />
                )}
              </div>
            )}
            {detail.segments.length > 0 ? (
              <ol className="space-y-1">
                {detail.segments.map((s, idx) => {
                  const i = Math.max(0, detail.speakers.findIndex((sp) => sp.diarLabel === s.diarLabel));
                  return (
                    <TranscriptRow
                      key={s.id}
                      ref={idx === activeIndex ? activeRef : undefined}
                      startMs={s.startMs}
                      speaker={names[s.diarLabel] ?? s.diarLabel}
                      speakerClass={textPalette[i % textPalette.length]}
                      text={s.text}
                      active={idx === activeIndex}
                      onClick={() => seek(s.startMs)}
                    />
                  );
                })}
              </ol>
            ) : liveFallback.length > 0 ? (
              <>
                <p className="mb-3 text-xs text-neutral-500">
                  {processing
                    ? "Transcribing with speakers… showing the live transcript meanwhile."
                    : m.state === "recorded"
                      ? "Live transcript from the meeting. Press ↻ to run the full transcription with speakers."
                      : "Live transcript from the meeting."}
                </p>
                <ol className="space-y-1">
                  {liveFallback.map((s, idx) => (
                    <TranscriptRow
                      key={idx}
                      ref={idx === activeIndex ? activeRef : undefined}
                      startMs={s.startMs}
                      speaker={s.who === "me" ? "Me" : s.who === "them" ? "Them" : undefined}
                      speakerClass={s.who === "me" ? "text-amber-700" : "text-sky-700"}
                      text={s.text}
                      active={idx === activeIndex}
                      onClick={() => seek(s.startMs)}
                    />
                  ))}
                </ol>
              </>
            ) : (
              <p className="text-sm text-neutral-500">{processing ? "Transcribing…" : m.state === "recorded" ? "Not transcribed yet. Press ↻ to transcribe." : "No transcript yet."}</p>
            )}
          </div>
        )}
        {tab === "summary" && <SummaryTab detail={detail} onChanged={load} />}
        {tab === "diagnostics" && <DiagnosticsTab meetingId={id} />}
      </div>
      {tab === "transcript" && recordingPath && <Player ref={player} path={recordingPath} onTime={onTime} />}
    </div>
  );
}

/** One transcript line; clicking it seeks the player, the active one is highlighted. */
function TranscriptRow({
  ref,
  startMs,
  speaker,
  speakerClass,
  text,
  active,
  onClick,
}: {
  ref?: Ref<HTMLLIElement>;
  startMs: number;
  speaker?: string;
  speakerClass?: string;
  text: string;
  active: boolean;
  onClick: () => void;
}) {
  return (
    <li
      ref={ref}
      onClick={onClick}
      title="Click to listen from here"
      className={cn(
        "flex cursor-pointer gap-3 rounded-md px-2 py-1 text-sm hover:bg-neutral-100/70 dark:hover:bg-neutral-900",
        active && "bg-amber-50 ring-1 ring-amber-200 dark:bg-amber-950 dark:ring-amber-900",
      )}
    >
      <span className="w-12 shrink-0 pt-0.5 font-mono text-xs text-neutral-400">{formatDuration(startMs)}</span>
      <div>
        {speaker && <span className={cn("mr-2 font-medium", speakerClass)}>{speaker}</span>}
        <span className="text-neutral-800 dark:text-neutral-200">{text}</span>
      </div>
    </li>
  );
}

/** Index of the last row starting at or before `ms` (rows are in time order), or -1. */
function indexAt(rows: { startMs: number }[], ms: number): number {
  let lo = 0;
  let hi = rows.length - 1;
  let found = -1;
  while (lo <= hi) {
    const mid = (lo + hi) >> 1;
    if (rows[mid].startMs <= ms) {
      found = mid;
      lo = mid + 1;
    } else {
      hi = mid - 1;
    }
  }
  return found;
}

const palette = ["bg-amber-500", "bg-sky-500", "bg-emerald-500", "bg-fuchsia-500", "bg-rose-500", "bg-indigo-500"];
const textPalette = ["text-amber-700", "text-sky-700", "text-emerald-700", "text-fuchsia-700", "text-rose-700", "text-indigo-700"];
