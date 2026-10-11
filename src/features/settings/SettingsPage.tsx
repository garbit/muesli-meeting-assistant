import { useEffect, useState } from "react";
import { check } from "@tauri-apps/plugin-updater";
import { relaunch } from "@tauri-apps/plugin-process";
import { ipc, type AppInfo, type PermissionsStatus, type Settings, type Template } from "../../lib/ipc";
import { confirmDialog } from "../../lib/dialogs";

export default function SettingsPage() {
  const [info, setInfo] = useState<AppInfo | null>(null);
  const [settings, setSettings] = useState<Settings | null>(null);
  const [templates, setTemplates] = useState<Template[]>([]);
  const [perms, setPerms] = useState<PermissionsStatus | null>(null);
  const [health, setHealth] = useState("");
  const [update, setUpdate] = useState<string>("");
  const [diag, setDiag] = useState<string>("");
  const [error, setError] = useState("");

  useEffect(() => {
    ipc.appInfo().then(setInfo).catch((e) => setError(String(e)));
    ipc.settingsGet().then(setSettings).catch((e) => setError(String(e)));
    ipc.templatesList().then(setTemplates).catch(() => {});
    ipc.permissionsStatus().then(setPerms).catch(() => {});
  }, []);

  const updateSettings = (patch: Partial<Settings>) => {
    if (!settings) return;
    const next = { ...settings, ...patch };
    setSettings(next);
    ipc.settingsSet(next).then(setSettings).catch((e) => setError(String(e)));
  };

  const checkUpdates = async () => {
    setUpdate("Checking…");
    try {
      const u = await check();
      if (!u) {
        setUpdate("You are on the latest version.");
        return;
      }
      if (!(await confirmDialog(`Muesli ${u.version} is available. Download and install now?`, { okLabel: "Update" }))) {
        setUpdate("");
        return;
      }
      setUpdate(`Downloading ${u.version}…`);
      await u.downloadAndInstall();
      await relaunch();
    } catch (e) {
      setUpdate(`Update check failed: ${String(e)}`);
    }
  };

  return (
    <div className="space-y-8 p-8">
      <h1 className="text-xl font-semibold">Settings</h1>
      {error && <p className="rounded bg-red-50 p-2 text-sm text-red-700 dark:bg-red-950 dark:text-red-300">{error}</p>}

      <section>
        <h2 className="text-xs font-semibold uppercase tracking-wide text-neutral-500">Permissions</h2>
        {perms?.codeHashChanged && (
          <p className="mt-2 rounded-md border border-amber-300 bg-amber-50 p-3 text-sm text-amber-900 dark:border-amber-800 dark:bg-amber-950 dark:text-amber-100">
            Muesli was updated. Because it is not yet signed with an Apple Developer ID, macOS forgets its permissions on every update. Re-grant
            Microphone and System Audio Recording below, then press "I've re-granted".
          </p>
        )}
        <dl className="mt-3 grid grid-cols-[12rem_1fr] gap-y-2 text-sm">
          <dt className="text-neutral-500">Microphone</dt>
          <dd className="flex items-center gap-3">
            {perms?.microphone ?? "…"}
            <button className="text-xs underline" onClick={() => ipc.permissionsOpenSettings("microphone").catch((e) => setError(String(e)))}>
              Open System Settings
            </button>
          </dd>
          <dt className="text-neutral-500">System audio</dt>
          <dd className="flex items-center gap-3">
            {perms?.systemAudio === "unknown" ? "granted on first recording" : (perms?.systemAudio ?? "…")}
            <button className="text-xs underline" onClick={() => ipc.permissionsOpenSettings("system-audio").catch((e) => setError(String(e)))}>
              Open System Settings
            </button>
          </dd>
        </dl>
        <div className="mt-3 flex gap-2">
          <button
            onClick={() => ipc.permissionsRequest().then(setPerms).catch((e) => setError(String(e)))}
            className="rounded-md border border-neutral-300 px-3 py-1.5 text-sm dark:border-neutral-700"
          >
            Request microphone access
          </button>
          {(perms?.codeHashChanged || perms?.firstRun) && (
            <button onClick={() => ipc.permissionsAcknowledge().then(setPerms).catch((e) => setError(String(e)))} className="rounded-md bg-amber-600 px-3 py-1.5 text-sm font-medium text-white">
              I've re-granted
            </button>
          )}
        </div>
      </section>

      <section>
        <h2 className="text-xs font-semibold uppercase tracking-wide text-neutral-500">Summaries</h2>
        <dl className="mt-3 grid grid-cols-[12rem_1fr] gap-y-3 text-sm">
          <dt className="text-neutral-500">Model preset</dt>
          <dd>
            <select
              value={settings?.preset ?? "balanced"}
              onChange={(e) => updateSettings({ preset: e.target.value })}
              className="rounded-md border border-neutral-300 bg-transparent px-2 py-1 dark:border-neutral-700"
            >
              {Object.entries(info?.presets ?? {}).map(([k, p]) => (
                <option key={k} value={k}>
                  {k[0].toUpperCase() + k.slice(1)} — {p.llm} ({p.minRamGb} GB+)
                </option>
              ))}
            </select>
          </dd>
          <dt className="text-neutral-500">Default template</dt>
          <dd>
            <select
              value={settings?.defaultTemplateId ?? "general"}
              onChange={(e) => updateSettings({ defaultTemplateId: e.target.value })}
              className="rounded-md border border-neutral-300 bg-transparent px-2 py-1 dark:border-neutral-700"
            >
              {templates.map((t) => (
                <option key={t.id} value={t.id}>
                  {t.name}
                </option>
              ))}
            </select>
          </dd>
          <dt className="text-neutral-500">Live transcript</dt>
          <dd>
            <input type="checkbox" checked={settings?.liveTranscript ?? true} onChange={(e) => updateSettings({ liveTranscript: e.target.checked })} /> show a
            near-real-time transcript while recording
          </dd>
          <dt className="text-neutral-500">Transcribe automatically</dt>
          <dd>
            <input type="checkbox" checked={settings?.autoTranscribe ?? true} onChange={(e) => updateSettings({ autoTranscribe: e.target.checked })} /> run the
            full transcription with speakers as soon as a recording stops
          </dd>
          <dt className="text-neutral-500">Summarise automatically</dt>
          <dd>
            <input type="checkbox" checked={settings?.autoSummarise ?? true} onChange={(e) => updateSettings({ autoSummarise: e.target.checked })} /> after each
            meeting is transcribed
          </dd>
        </dl>
      </section>

      <section>
        <h2 className="text-xs font-semibold uppercase tracking-wide text-neutral-500">Recordings</h2>
        <dl className="mt-3 grid grid-cols-[12rem_1fr] gap-y-3 text-sm">
          <dt className="text-neutral-500">Keep audio for</dt>
          <dd>
            <select
              value={settings?.retentionDays ?? 0}
              onChange={(e) => updateSettings({ retentionDays: Number(e.target.value) })}
              className="rounded-md border border-neutral-300 bg-transparent px-2 py-1 dark:border-neutral-700"
            >
              <option value={0}>Forever</option>
              <option value={7}>7 days</option>
              <option value={30}>30 days</option>
              <option value={90}>90 days</option>
            </select>
            <span className="ml-2 text-xs text-neutral-500">Transcripts and summaries are always kept.</span>
          </dd>
        </dl>
      </section>

      <section>
        <h2 className="text-xs font-semibold uppercase tracking-wide text-neutral-500">Diagnostics</h2>
        <p className="mt-1 text-xs text-neutral-500">
          Muesli writes a log to its data folder, and each meeting keeps its own capture and processing logs. The report below is what to
          attach to a bug report; it contains no transcript text.
        </p>
        <div className="mt-3 flex flex-wrap gap-2">
          <button
            className="rounded-md border border-neutral-300 px-3 py-1.5 text-sm dark:border-neutral-700"
            onClick={async () => {
              setDiag("Collecting…");
              try {
                const r = await ipc.diagnosticsReport();
                await navigator.clipboard.writeText(r);
                setDiag("Report copied to the clipboard.");
              } catch (e) {
                setDiag(`Failed: ${String(e)}`);
              }
            }}
          >
            Copy diagnostics report
          </button>
          <button className="rounded-md border border-neutral-300 px-3 py-1.5 text-sm dark:border-neutral-700" onClick={() => ipc.openLogsFolder().catch((e) => setError(String(e)))}>
            Show log file
          </button>
          <button className="rounded-md border border-neutral-300 px-3 py-1.5 text-sm dark:border-neutral-700" onClick={() => ipc.openDataFolder().catch((e) => setError(String(e)))}>
            Open data folder
          </button>
        </div>
        {diag && <p className="mt-2 text-xs text-neutral-500">{diag}</p>}
      </section>

      <section>
        <h2 className="text-xs font-semibold uppercase tracking-wide text-neutral-500">About</h2>
        <dl className="mt-3 grid grid-cols-[12rem_1fr] gap-y-2 text-sm">
          <dt className="text-neutral-500">Version</dt>
          <dd className="flex items-center gap-3">
            {info?.version ?? "…"}
            <button className="text-xs underline" onClick={checkUpdates}>
              Check for updates
            </button>
            {update && <span className="text-xs text-neutral-500">{update}</span>}
          </dd>
          <dt className="text-neutral-500">Data folder</dt>
          <dd className="font-mono text-xs">{info?.dataDir ?? "…"}</dd>
          <dt className="text-neutral-500">Model manifest</dt>
          <dd>v{info?.manifestVersion ?? "…"}</dd>
        </dl>
        <button
          className="mt-4 rounded-md border border-neutral-300 px-3 py-1.5 text-sm hover:bg-neutral-100 dark:border-neutral-700 dark:hover:bg-neutral-800"
          onClick={() => ipc.sidecarHealth().then((h) => setHealth(JSON.stringify(h))).catch((e) => setHealth(String(e)))}
        >
          Check sidecar
        </button>
        {health && <pre className="mt-3 rounded bg-neutral-100 p-3 text-xs dark:bg-neutral-900">{health}</pre>}
      </section>
    </div>
  );
}
