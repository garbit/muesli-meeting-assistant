// Typed wrappers over Tauri commands and events. Keep this file the single
// place that knows command names and payload shapes.
import { invoke } from "@tauri-apps/api/core";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";

export interface ManifestModel {
  id: string;
  kind: "asr" | "diarisation" | "llm";
  name: string;
  organisation: string;
  licence: string;
  repo: string;
  runtime: "sidecar" | "llama.cpp";
  approxBytes: number;
  minRamGb: number;
  contextTokens?: number;
  preset?: string;
  default?: boolean;
  optional?: boolean;
  provenance?: string;
}

export interface Preset {
  asr: string;
  diarisation: string;
  llm: string;
  minRamGb: number;
}

export interface AppInfo {
  version: string;
  dataDir: string;
  manifestVersion: number;
  presets: Record<string, Preset>;
  models: ManifestModel[];
}

export interface ModelStatus {
  id: string;
  installed: boolean;
  bytesOnDisk: number;
  path?: string | null;
}

export interface DownloadProgress {
  id: string;
  fraction: number;
  bytesDone: number;
  bytesTotal: number;
  phase: string;
}

export interface Meeting {
  id: string;
  title: string;
  source: "recorded" | "imported";
  createdAt: string;
  startedAt?: string | null;
  endedAt?: string | null;
  durationMs?: number | null;
  notesMd: string;
  state: "new" | "recording" | "recorded" | "processing" | "transcribed" | "summarised" | "failed";
  lastError?: string | null;
}

export interface JobRow {
  id: string;
  meetingId?: string | null;
  kind: string;
  stage?: string | null;
  state: string;
  error?: string | null;
  createdAt: string;
  finishedAt?: string | null;
}

export interface MeetingLogs {
  processLog?: string | null;
  captureLog?: string | null;
  files: string[];
}

export interface Segment {
  id: number;
  idx: number;
  startMs: number;
  endMs: number;
  text: string;
  diarLabel: string;
  speakerId?: string | null;
  micRatio?: number | null;
}

export interface MeetingSpeaker {
  diarLabel: string;
  speakerId?: string | null;
  displayName?: string | null;
  micRatio?: number | null;
  speechMs: number;
}

export interface Summary {
  id: string;
  templateId?: string | null;
  modelId: string;
  bodyMd: string;
  status: string;
  createdAt: string;
}

export interface Recording {
  id: string;
  kind: "retained" | "import";
  path: string;
  durationMs?: number | null;
  bytes?: number | null;
}

export interface MeetingDetail {
  meeting: Meeting;
  recording?: Recording | null;
  segments: Segment[];
  speakers: MeetingSpeaker[];
  summaries: Summary[];
}

export interface RecordingStatus {
  meetingId: string;
  elapsedMs: number;
  starting: boolean;
  status: {
    state?: string;
    micRms?: number;
    sysRms?: number;
    inputDevice?: string;
    outputDevice?: string;
    gaps?: number;
    systemAudio?: "ok" | "silent" | "missing";
    droppedFrames?: number;
  } | null;
}

export interface Template {
  id: string;
  name: string;
  bodyMd: string;
  isBuiltin: boolean;
  sortOrder: number;
  updatedAt: string;
}

export interface SummaryEvent {
  meetingId: string;
  summaryId: string;
  token?: string | null;
  status: "running" | "done" | "failed" | "cancelled";
  error?: string | null;
}

export interface SummaryRunning {
  meetingId: string;
  summaryId: string;
}

export interface ExportBundle {
  summaryMd: string;
  transcriptTxt: string;
}

export interface Speaker {
  id: string;
  name: string;
  isMe: boolean;
  sampleCount: number;
  createdAt: string;
  lastSeenAt?: string | null;
  meetings: number;
}

export interface Settings {
  preset: string;
  retentionDays: number;
  defaultTemplateId: string;
  autoTranscribe: boolean;
  autoSummarise: boolean;
  liveTranscript: boolean;
}

export interface LiveSegment {
  text: string;
  confirmed: boolean;
  startMs: number;
  endMs: number;
  who?: "me" | "them" | null;
}

export interface LiveTranscript {
  confirmed: LiveSegment[];
  volatile?: LiveSegment | null;
}

export interface PermissionsStatus {
  /** "authorized" | "denied" | "restricted" | "notDetermined" | "unavailable" (capture helper missing). */
  microphone: string;
  /** "authorized" | "denied" | "notDetermined" (the tap has not been exercised for this build). */
  systemAudio: string;
  /** Why system audio is denied, when known. */
  systemAudioDetail?: string | null;
  codeHash: string;
  /** A different build was acknowledged before: this is the first launch after an update. */
  codeHashChanged: boolean;
  /** No build was ever acknowledged: a fresh install. */
  firstRun: boolean;
  /** Both grants confirmed. */
  ready: boolean;
  /** The prompts have not run for this build and something is missing; the UI runs them automatically. */
  needsSetup: boolean;
}

export interface JobProgress {
  jobId: string;
  meetingId: string;
  stage: string;
  fraction: number;
  state: "running" | "done" | "failed" | "cancelled";
  error?: string | null;
}

const on = <T,>(event: string, cb: (p: T) => void): Promise<UnlistenFn> => listen<T>(event, (e) => cb(e.payload));

export const ipc = {
  appInfo: () => invoke<AppInfo>("app_info"),
  modelsStatus: () => invoke<ModelStatus[]>("models_status"),
  modelsDownload: (id: string) => invoke<ModelStatus>("models_download", { id }),
  sidecarHealth: () => invoke<Record<string, unknown>>("sidecar_health"),

  meetingsList: () => invoke<Meeting[]>("meetings_list"),
  meetingGet: (id: string) => invoke<MeetingDetail | null>("meeting_get", { id }),
  meetingUpdate: (id: string, patch: { title?: string; notesMd?: string }) => invoke<void>("meeting_update", { id, ...patch }),
  meetingDelete: (id: string) => invoke<void>("meeting_delete", { id }),
  meetingRenameSpeaker: (meetingId: string, diarLabel: string, name: string) =>
    invoke<void>("meeting_rename_speaker", { meetingId, diarLabel, name }),
  meetingProcess: (id: string) => invoke<void>("meeting_process", { id }),
  meetingCancelProcessing: (meetingId: string) => invoke<boolean>("meeting_cancel_processing", { meetingId }),

  recordingStart: (title?: string) => invoke<Meeting>("recording_start", { title }),
  recordingStop: () => invoke<Meeting>("recording_stop"),
  recordingStatus: () => invoke<RecordingStatus | null>("recording_status"),
  recordingLiveTranscript: () => invoke<LiveTranscript | null>("recording_live_transcript"),
  meetingLiveTranscript: (meetingId: string) => invoke<LiveSegment[]>("meeting_live_transcript", { meetingId }),
  importFile: (path: string, title?: string) => invoke<Meeting>("import_file", { path, title }),

  diagnosticsReport: () => invoke<string>("diagnostics_report"),
  meetingJobs: (meetingId: string) => invoke<JobRow[]>("meeting_jobs", { meetingId }),
  meetingLogs: (meetingId: string, lines = 120) => invoke<MeetingLogs>("meeting_logs", { meetingId, lines }),
  openLogsFolder: () => invoke<void>("open_logs_folder"),
  openDataFolder: (meetingId?: string) => invoke<void>("open_data_folder", { meetingId }),

  settingsGet: () => invoke<Settings>("settings_get"),
  settingsSet: (settings: Settings) => invoke<Settings>("settings_set", { settings }),
  recordingDelete: (meetingId: string) => invoke<void>("recording_delete", { meetingId }),
  permissionsStatus: (refresh = false) => invoke<PermissionsStatus>("permissions_status", { refresh }),
  /** Runs the prompts that macOS has not shown yet, then exercises the system audio tap. */
  permissionsRequest: () => invoke<PermissionsStatus>("permissions_request"),
  /** Re-reads the microphone and exercises the tap again; never prompts once macOS has decided. */
  permissionsRecheck: () => invoke<PermissionsStatus>("permissions_recheck"),
  permissionsOpenSettings: (pane: "microphone" | "system-audio") => invoke<void>("permissions_open_settings", { pane }),

  speakersList: () => invoke<Speaker[]>("speakers_list"),
  speakerAssign: (meetingId: string, diarLabel: string, name: string, speakerId?: string) =>
    invoke<string>("speaker_assign", { meetingId, diarLabel, name, speakerId }),
  speakerUpdate: (id: string, patch: { name?: string; isMe?: boolean }) => invoke<void>("speaker_update", { id, ...patch }),
  speakerMerge: (from: string, into: string) => invoke<void>("speaker_merge", { from, into }),
  speakerDelete: (id: string) => invoke<void>("speaker_delete", { id }),

  templatesList: () => invoke<Template[]>("templates_list"),
  templateUpsert: (t: { id?: string; name: string; bodyMd: string }) => invoke<Template>("template_upsert", t),
  templateDelete: (id: string) => invoke<void>("template_delete", { id }),
  templateReset: (id: string) => invoke<Template>("template_reset", { id }),

  summaryGenerate: (meetingId: string, templateId?: string) => invoke<SummaryRunning>("summary_generate", { meetingId, templateId }),
  summaryStatus: () => invoke<SummaryRunning | null>("summary_status"),
  summaryCancel: () => invoke<boolean>("summary_cancel"),
  meetingExport: (meetingId: string, summaryId?: string) => invoke<ExportBundle>("meeting_export", { meetingId, summaryId }),
  meetingExportTo: (meetingId: string, path: string, summaryId?: string) =>
    invoke<string[]>("meeting_export_to", { meetingId, summaryId, path }),

  onModelsProgress: (cb: (p: DownloadProgress) => void) => on<DownloadProgress>("models:progress", cb),
  onSummaryToken: (cb: (p: SummaryEvent) => void) => on<SummaryEvent>("summary:token", cb),
  onSummaryDone: (cb: (p: SummaryEvent) => void) => on<SummaryEvent>("summary:done", cb),
  onRecordingStatus: (cb: (p: RecordingStatus) => void) => on<RecordingStatus>("recording:status", cb),
  onRecordingChanged: (cb: (p: { recording: boolean; meetingId: string }) => void) =>
    on<{ recording: boolean; meetingId: string }>("recording:changed", cb),
  onRecordingTranscript: (cb: (p: { meetingId: string; segment: LiveSegment }) => void) =>
    on<{ meetingId: string; segment: LiveSegment }>("recording:transcript", cb),
  onJobProgress: (cb: (p: JobProgress) => void) => on<JobProgress>("job:progress", cb),
  onPermissionsChanged: (cb: (p: PermissionsStatus) => void) => on<PermissionsStatus>("permissions:changed", cb),
};
