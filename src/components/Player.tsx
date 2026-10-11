import { useEffect, useImperativeHandle, useRef, useState, forwardRef } from "react";
import { convertFileSrc } from "@tauri-apps/api/core";
import { Pause, Play, RotateCcw, RotateCw } from "lucide-react";
import { formatDuration } from "../lib/utils";

export interface PlayerHandle {
  seek(ms: number, play?: boolean): void;
}

const SKIP_MS = 15_000;
const RATES = [1, 1.25, 1.5, 2];

/**
 * Audio player for a meeting recording. Reports the playhead so the transcript
 * can highlight the current segment; `seek` lets a transcript click jump in.
 */
const Player = forwardRef<PlayerHandle, { path: string; onTime?: (ms: number) => void }>(function Player({ path, onTime }, ref) {
  const audio = useRef<HTMLAudioElement>(null);
  const [playing, setPlaying] = useState(false);
  const [time, setTime] = useState(0);
  const [duration, setDuration] = useState(0);
  const [rate, setRate] = useState(1);
  const [error, setError] = useState("");

  const setPosition = (ms: number) => {
    const a = audio.current;
    if (!a) return;
    a.currentTime = Math.max(0, ms) / 1000;
    setTime(Math.max(0, ms));
  };

  useImperativeHandle(
    ref,
    () => ({
      seek(ms, play = true) {
        setPosition(ms);
        if (play) audio.current?.play().catch((e) => setError(String(e)));
      },
    }),
    [],
  );

  useEffect(() => {
    if (audio.current) audio.current.playbackRate = rate;
  }, [rate]);

  // A different recording (navigating between meetings keeps this mounted).
  useEffect(() => {
    setTime(0);
    setDuration(0);
    setError("");
    setPlaying(false);
  }, [path]);

  const togglePlay = () => {
    const a = audio.current;
    if (!a) return;
    if (a.paused) a.play().catch((e) => setError(String(e)));
    else a.pause();
  };

  return (
    <div className="flex items-center gap-3 border-t border-neutral-200 bg-neutral-50 px-4 py-2 text-sm dark:border-neutral-800 dark:bg-neutral-900">
      <audio
        ref={audio}
        src={convertFileSrc(path)}
        preload="metadata"
        onPlay={() => setPlaying(true)}
        onPause={() => setPlaying(false)}
        onLoadedMetadata={(e) => {
          setDuration(e.currentTarget.duration * 1000);
          e.currentTarget.playbackRate = rate;
        }}
        onTimeUpdate={(e) => {
          const ms = e.currentTarget.currentTime * 1000;
          setTime(ms);
          onTime?.(ms);
        }}
        onError={() => setError("Cannot play this recording.")}
      />
      <button onClick={() => setPosition(time - SKIP_MS)} title="Back 15 s" className="text-neutral-500 hover:text-neutral-900 dark:hover:text-white">
        <RotateCcw className="size-4" />
      </button>
      <button onClick={togglePlay} className="flex size-8 items-center justify-center rounded-full bg-amber-600 text-white hover:bg-amber-700" title={playing ? "Pause" : "Play"}>
        {playing ? <Pause className="size-4" /> : <Play className="size-4 translate-x-px" />}
      </button>
      <button onClick={() => setPosition(Math.min(duration, time + SKIP_MS))} title="Forward 15 s" className="text-neutral-500 hover:text-neutral-900 dark:hover:text-white">
        <RotateCw className="size-4" />
      </button>
      <span className="w-12 text-right font-mono text-xs tabular-nums">{formatDuration(time)}</span>
      <input
        type="range"
        min={0}
        max={duration}
        step={100}
        value={Math.min(time, duration)}
        onChange={(e) => setPosition(Number(e.target.value))}
        className="flex-1 accent-amber-600"
        aria-label="Playback position"
      />
      <span className="w-12 font-mono text-xs tabular-nums text-neutral-500">{formatDuration(duration)}</span>
      <select value={rate} onChange={(e) => setRate(Number(e.target.value))} className="rounded border border-neutral-300 bg-transparent px-1 py-0.5 text-xs dark:border-neutral-700" title="Speed">
        {RATES.map((r) => (
          <option key={r} value={r}>
            {r}×
          </option>
        ))}
      </select>
      {error && <span className="text-xs text-red-600">{error}</span>}
    </div>
  );
});

export default Player;
