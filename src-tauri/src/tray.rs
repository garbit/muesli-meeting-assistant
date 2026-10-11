//! Menu-bar item: start/stop recording, elapsed time, open the main window.

use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::Arc;

use tauri::menu::{Menu, MenuItem, PredefinedMenuItem};
use tauri::tray::{TrayIconBuilder, TrayIconEvent};
use tauri::{AppHandle, Manager};

use crate::recording::Recorder;

const TRAY_ID: &str = "muesli-tray";
const ITEM_TOGGLE: &str = "toggle-recording";
const ITEM_OPEN: &str = "open";
const ITEM_QUIT: &str = "quit";

pub fn setup(app: &AppHandle) -> tauri::Result<()> {
    let toggle = MenuItem::with_id(app, ITEM_TOGGLE, "Start recording", true, None::<&str>)?;
    let open = MenuItem::with_id(app, ITEM_OPEN, "Open Muesli", true, None::<&str>)?;
    let quit = MenuItem::with_id(app, ITEM_QUIT, "Quit Muesli", true, None::<&str>)?;
    let menu = Menu::with_items(app, &[&toggle, &PredefinedMenuItem::separator(app)?, &open, &quit])?;

    let mut builder = TrayIconBuilder::with_id(TRAY_ID).menu(&menu).show_menu_on_left_click(true).tooltip("Muesli");
    if let Some(icon) = app.default_window_icon() {
        builder = builder.icon(icon.clone()).icon_as_template(false);
    }
    builder
        .on_menu_event(|app, event| match event.id().as_ref() {
            ITEM_TOGGLE => {
                let app = app.clone();
                tauri::async_runtime::spawn(async move {
                    let recorder = app.state::<Arc<Recorder>>().inner().clone();
                    let result = if recorder.is_recording().await { recorder.stop(&app).await.map(|_| ()) } else { recorder.start(&app, None).await.map(|_| ()) };
                    if let Err(e) = result {
                        tracing::error!("tray toggle failed: {e:#}");
                    }
                });
            }
            ITEM_OPEN => show_main(app),
            ITEM_QUIT => app.exit(0),
            _ => {}
        })
        .on_tray_icon_event(|tray, event| {
            if let TrayIconEvent::DoubleClick { .. } = event {
                show_main(tray.app_handle());
            }
        })
        .build(app)?;
    Ok(())
}

fn show_main(app: &AppHandle) {
    if let Some(w) = app.get_webview_window("main") {
        let _ = w.show();
        let _ = w.unminimize();
        let _ = w.set_focus();
    }
}

/// 0 = unknown, 1 = idle, 2 = recording. Lets the once-a-second title update skip
/// the comparatively expensive menu rebuild unless the state actually changed.
static MENU_STATE: AtomicU8 = AtomicU8::new(0);

/// Update the tray title and, when the state changed, the menu.
pub fn set_recording(app: &AppHandle, elapsed_ms: Option<u64>) {
    let Some(tray) = app.tray_by_id(TRAY_ID) else { return };
    match elapsed_ms {
        Some(ms) => {
            let s = ms / 1000;
            let title = if s >= 3600 {
                format!("● {}:{:02}:{:02}", s / 3600, (s % 3600) / 60, s % 60)
            } else {
                format!("● {}:{:02}", s / 60, s % 60)
            };
            let _ = tray.set_title(Some(title));
            let _ = tray.set_tooltip(Some("Muesli — recording"));
        }
        None => {
            let _ = tray.set_title(None::<&str>);
            let _ = tray.set_tooltip(Some("Muesli"));
        }
    }
    // Rebuild the menu with the right toggle label, only on a state change.
    let new_state: u8 = if elapsed_ms.is_some() { 2 } else { 1 };
    if MENU_STATE.swap(new_state, Ordering::SeqCst) == new_state {
        return;
    }
    let label = if elapsed_ms.is_some() { "Stop recording" } else { "Start recording" };
    if let (Ok(toggle), Ok(open), Ok(quit), Ok(sep)) = (
        MenuItem::with_id(app, ITEM_TOGGLE, label, true, None::<&str>),
        MenuItem::with_id(app, ITEM_OPEN, "Open Muesli", true, None::<&str>),
        MenuItem::with_id(app, ITEM_QUIT, "Quit Muesli", true, None::<&str>),
        PredefinedMenuItem::separator(app),
    )
        && let Ok(menu) = Menu::with_items(app, &[&toggle, &sep, &open, &quit]) {
            let _ = tray.set_menu(Some(menu));
        }
}
