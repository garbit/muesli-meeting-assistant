import { useEffect, useState } from "react";
import { Merge, Trash2, User } from "lucide-react";
import { ipc, type Speaker } from "../../lib/ipc";
import { cn } from "../../lib/utils";
import { confirmDialog } from "../../lib/dialogs";
import InlinePrompt from "../../components/InlinePrompt";

export default function SpeakersPage() {
  const [speakers, setSpeakers] = useState<Speaker[]>([]);
  const [error, setError] = useState("");
  const [mergeFrom, setMergeFrom] = useState<string | null>(null);
  const [renaming, setRenaming] = useState<string | null>(null);

  const load = () => ipc.speakersList().then(setSpeakers).catch((e) => setError(String(e)));
  useEffect(() => {
    load();
  }, []);

  const rename = async (s: Speaker, name: string) => {
    setRenaming(null);
    if (name === s.name) return;
    await ipc.speakerUpdate(s.id, { name }).catch((e) => setError(String(e)));
    load();
  };

  const onRowClick = async (s: Speaker) => {
    if (!mergeFrom) return;
    if (mergeFrom === s.id) {
      setMergeFrom(null);
      return;
    }
    const from = speakers.find((x) => x.id === mergeFrom);
    if (await confirmDialog(`Merge "${from?.name}" into "${s.name}"? Their voices and meetings are combined.`, { okLabel: "Merge" })) {
      await ipc.speakerMerge(mergeFrom, s.id).catch((e) => setError(String(e)));
    }
    setMergeFrom(null);
    load();
  };

  const forget = async (s: Speaker) => {
    if (!(await confirmDialog(`Forget ${s.name}'s voice? Their meetings keep the name but new meetings will not recognise them.`, { okLabel: "Forget", danger: true }))) return;
    await ipc.speakerDelete(s.id).catch((e) => setError(String(e)));
    load();
  };

  return (
    <div className="p-8">
      <h1 className="text-xl font-semibold">People</h1>
      <p className="mt-1 text-sm text-neutral-500">
        Voices you have named. Muesli matches them automatically in new meetings. Everything stays on this Mac.
      </p>
      {error && <p className="mt-3 rounded bg-red-50 p-2 text-sm text-red-700 dark:bg-red-950 dark:text-red-300">{error}</p>}
      {mergeFrom && <p className="mt-3 text-xs text-amber-700">Pick the person to merge into, or click the same row to cancel.</p>}
      {speakers.length === 0 ? (
        <p className="mt-6 text-sm text-neutral-500">No one yet. Name speakers from a meeting's transcript tab.</p>
      ) : (
        <ul className="mt-4 divide-y divide-neutral-200 rounded-lg border border-neutral-200 dark:divide-neutral-800 dark:border-neutral-800">
          {speakers.map((s) => (
            <li
              key={s.id}
              onClick={() => onRowClick(s)}
              className={cn(
                "flex flex-wrap items-center gap-3 px-4 py-3",
                mergeFrom && mergeFrom !== s.id && "cursor-pointer hover:bg-amber-50 dark:hover:bg-amber-950",
                mergeFrom === s.id && "bg-amber-100 dark:bg-amber-900",
              )}
            >
              <User className="size-4 text-neutral-400" />
              <div className="min-w-0 flex-1">
                <div className="text-sm font-medium">
                  {s.name}
                  {s.isMe && <span className="ml-2 rounded bg-amber-100 px-1.5 py-0.5 text-[10px] text-amber-800">me</span>}
                </div>
                <div className="text-xs text-neutral-500">
                  {s.meetings} meeting{s.meetings === 1 ? "" : "s"} · {s.sampleCount} voice sample{s.sampleCount === 1 ? "" : "s"}
                  {s.lastSeenAt ? ` · last heard ${new Date(s.lastSeenAt).toLocaleDateString()}` : ""}
                </div>
              </div>
              <button
                onClick={(e) => {
                  e.stopPropagation();
                  setRenaming(renaming === s.id ? null : s.id);
                }}
                className="text-xs text-neutral-500 hover:underline"
              >
                Rename
              </button>
              <button
                onClick={(e) => {
                  e.stopPropagation();
                  ipc.speakerUpdate(s.id, { isMe: !s.isMe }).then(load).catch((er) => setError(String(er)));
                }}
                className="text-xs text-neutral-500 hover:underline"
              >
                {s.isMe ? "Not me" : "This is me"}
              </button>
              <button
                onClick={(e) => {
                  e.stopPropagation();
                  setMergeFrom(s.id);
                }}
                className="text-neutral-500 hover:text-neutral-900 dark:hover:text-white"
                title="Merge into another person"
              >
                <Merge className="size-4" />
              </button>
              <button
                onClick={(e) => {
                  e.stopPropagation();
                  forget(s);
                }}
                className="text-neutral-500 hover:text-red-600"
                title="Forget"
              >
                <Trash2 className="size-4" />
              </button>
              {renaming === s.id && (
                <div className="basis-full" onClick={(e) => e.stopPropagation()}>
                  <InlinePrompt label="Name" initial={s.name} onSubmit={(v) => rename(s, v)} onCancel={() => setRenaming(null)} />
                </div>
              )}
            </li>
          ))}
        </ul>
      )}
    </div>
  );
}
