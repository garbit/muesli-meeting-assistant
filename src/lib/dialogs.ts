// Native dialogs. WKWebView in Tauri does not implement window.confirm/prompt
// (they return false/null without showing anything), so everything goes
// through the dialog plugin or an inline component.
import { ask, message } from "@tauri-apps/plugin-dialog";

export async function confirmDialog(text: string, opts: { title?: string; okLabel?: string; danger?: boolean } = {}): Promise<boolean> {
  try {
    return await ask(text, {
      title: opts.title ?? "Muesli",
      kind: opts.danger ? "warning" : "info",
      okLabel: opts.okLabel ?? "OK",
      cancelLabel: "Cancel",
    });
  } catch {
    return false;
  }
}

export async function showError(text: string, title = "Something went wrong"): Promise<void> {
  try {
    await message(text, { title, kind: "error" });
  } catch {
    // dialog unavailable; the caller usually also renders the error inline
  }
}
