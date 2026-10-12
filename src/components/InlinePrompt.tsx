import { useEffect, useRef, useState } from "react";

/**
 * Small inline text prompt (replacement for window.prompt, which WKWebView
 * does not implement). Optional suggestions render as a datalist.
 */
export default function InlinePrompt({
  label,
  initial,
  placeholder,
  suggestions = [],
  onSubmit,
  onCancel,
}: {
  label: string;
  initial?: string;
  placeholder?: string;
  suggestions?: string[];
  onSubmit: (value: string) => void;
  onCancel: () => void;
}) {
  const [value, setValue] = useState(initial ?? "");
  const ref = useRef<HTMLInputElement>(null);
  const listId = `inline-prompt-${label.replace(/\W+/g, "-")}`;

  useEffect(() => {
    ref.current?.focus();
    ref.current?.select();
  }, []);

  return (
    <form
      className="flex items-center gap-2 rounded-md border border-amber-300 bg-amber-50 px-2 py-1.5 text-sm dark:border-amber-800 dark:bg-amber-950"
      onSubmit={(e) => {
        e.preventDefault();
        const v = value.trim();
        if (v) onSubmit(v);
      }}
    >
      <label className="text-xs text-neutral-600 dark:text-neutral-300">{label}</label>
      <input
        ref={ref}
        list={suggestions.length ? listId : undefined}
        value={value}
        placeholder={placeholder}
        onChange={(e) => setValue(e.target.value)}
        onKeyDown={(e) => e.key === "Escape" && onCancel()}
        className="min-w-40 rounded border border-neutral-300 bg-white px-2 py-0.5 text-sm outline-none dark:border-neutral-700 dark:bg-neutral-900"
      />
      {suggestions.length > 0 && (
        <datalist id={listId}>
          {suggestions.map((s) => (
            <option key={s} value={s} />
          ))}
        </datalist>
      )}
      <button type="submit" className="rounded bg-amber-600 px-2 py-0.5 text-xs font-medium text-white">
        Save
      </button>
      <button type="button" onClick={onCancel} className="text-xs text-neutral-500 hover:underline">
        Cancel
      </button>
    </form>
  );
}
