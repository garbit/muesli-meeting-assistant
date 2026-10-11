import { useCallback, useEffect, useState } from "react";
import { Plus, RotateCcw, Trash2 } from "lucide-react";
import { ipc, type Template } from "../../lib/ipc";
import { cn } from "../../lib/utils";
import { confirmDialog } from "../../lib/dialogs";

const VARIABLES = ["{{title}}", "{{date}}", "{{duration}}", "{{speakers}}", "{{notes}}", "{{transcript}}"];

const NEW_TEMPLATE_BODY =
  "Summarise this meeting.\n\n## TL;DR\n## Main points by speaker\n## Actions and takeaways\n\nMeeting: {{title}} ({{date}}, {{duration}})\nParticipants: {{speakers}}\n\nMy notes:\n{{notes}}\n\nTranscript:\n{{transcript}}";

export default function TemplatesPage() {
  const [templates, setTemplates] = useState<Template[]>([]);
  const [selected, setSelected] = useState<string | null>(null);
  const [name, setName] = useState("");
  const [body, setBody] = useState("");
  const [dirty, setDirty] = useState(false);
  const [error, setError] = useState("");

  const show = useCallback((t: Template | undefined) => {
    setSelected(t?.id ?? null);
    setName(t?.name ?? "");
    setBody(t?.bodyMd ?? "");
    setDirty(false);
  }, []);

  /** Reload the list and select `keep` (null → first template). */
  const load = useCallback(
    async (keep: string | null) => {
      try {
        const list = await ipc.templatesList();
        setTemplates(list);
        show(list.find((x) => x.id === keep) ?? list[0]);
      } catch (e) {
        setError(String(e));
      }
    },
    [show],
  );

  useEffect(() => {
    load(null);
  }, [load]);

  const select = async (t: Template) => {
    if (dirty && !(await confirmDialog("Discard unsaved changes?", { okLabel: "Discard", danger: true }))) return;
    show(t);
  };

  const save = async () => {
    try {
      const t = await ipc.templateUpsert({ id: selected ?? undefined, name, bodyMd: body });
      await load(t.id);
    } catch (e) {
      setError(String(e));
    }
  };

  const create = async () => {
    if (dirty && !(await confirmDialog("Discard unsaved changes?", { okLabel: "Discard", danger: true }))) return;
    try {
      const t = await ipc.templateUpsert({ name: "New template", bodyMd: NEW_TEMPLATE_BODY });
      await load(t.id);
    } catch (e) {
      setError(String(e));
    }
  };

  const remove = async (t: Template) => {
    if (!(await confirmDialog(`Delete template "${t.name}"?`, { okLabel: "Delete", danger: true }))) return;
    try {
      await ipc.templateDelete(t.id);
      await load(null);
    } catch (e) {
      setError(String(e));
    }
  };

  const reset = async (t: Template) => {
    if (!(await confirmDialog(`Restore the shipped version of "${t.name}"? Your edits to it are lost.`, { okLabel: "Restore", danger: true }))) return;
    try {
      await ipc.templateReset(t.id);
      await load(t.id);
    } catch (e) {
      setError(String(e));
    }
  };

  const current = templates.find((t) => t.id === selected);

  return (
    <div className="flex h-full">
      <aside className="w-60 shrink-0 border-r border-neutral-200 p-4 dark:border-neutral-800">
        <div className="flex items-center justify-between">
          <h1 className="text-sm font-semibold">Templates</h1>
          <button onClick={create} className="rounded p-1 hover:bg-neutral-200 dark:hover:bg-neutral-800" title="New template">
            <Plus className="size-4" />
          </button>
        </div>
        <ul className="mt-3 space-y-0.5">
          {templates.map((t) => (
            <li key={t.id}>
              <button
                onClick={() => select(t)}
                className={cn(
                  "w-full rounded-md px-2 py-1.5 text-left text-sm hover:bg-neutral-200/70 dark:hover:bg-neutral-800",
                  selected === t.id && "bg-neutral-200 font-medium dark:bg-neutral-800",
                )}
              >
                {t.name}
                {t.isBuiltin && <span className="ml-1 text-[10px] text-neutral-400">built-in</span>}
              </button>
            </li>
          ))}
        </ul>
      </aside>
      <section className="flex min-w-0 flex-1 flex-col p-6">
        {error && <p className="mb-3 rounded bg-red-50 p-2 text-sm text-red-700 dark:bg-red-950 dark:text-red-300">{error}</p>}
        {current ? (
          <>
            <div className="flex items-center gap-2">
              <input
                value={name}
                onChange={(e) => {
                  setName(e.target.value);
                  setDirty(true);
                }}
                className="min-w-0 flex-1 rounded-md border border-neutral-300 bg-transparent px-2 py-1 text-sm dark:border-neutral-700"
              />
              {current.isBuiltin ? (
                <button onClick={() => reset(current)} className="flex items-center gap-1 rounded-md border border-neutral-300 px-2 py-1 text-xs dark:border-neutral-700" title="Restore the shipped version">
                  <RotateCcw className="size-3.5" /> Reset
                </button>
              ) : (
                <button onClick={() => remove(current)} className="flex items-center gap-1 rounded-md border border-neutral-300 px-2 py-1 text-xs text-red-600 dark:border-neutral-700">
                  <Trash2 className="size-3.5" /> Delete
                </button>
              )}
              <button onClick={save} disabled={!dirty} className="rounded-md bg-amber-600 px-3 py-1 text-xs font-medium text-white disabled:opacity-50">
                Save
              </button>
            </div>
            <p className="mt-2 text-xs text-neutral-500">
              Variables:{" "}
              {VARIABLES.map((v) => (
                <code key={v} className="mr-1 rounded bg-neutral-200 px-1 dark:bg-neutral-800">
                  {v}
                </code>
              ))}
              The transcript is appended automatically if you leave it out.
            </p>
            <textarea
              value={body}
              onChange={(e) => {
                setBody(e.target.value);
                setDirty(true);
              }}
              spellCheck={false}
              className="mt-3 min-h-0 flex-1 resize-none rounded-md border border-neutral-300 bg-transparent p-3 font-mono text-xs leading-relaxed outline-none dark:border-neutral-700"
            />
          </>
        ) : (
          <p className="text-sm text-neutral-500">Select a template.</p>
        )}
      </section>
    </div>
  );
}
