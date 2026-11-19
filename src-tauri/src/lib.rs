mod browser;
mod commands;
mod state;

use state::{AppState, SharedState};
use std::sync::{Arc, Mutex};
use tauri::{Manager, WindowEvent};
use tauri_plugin_global_shortcut::{Code, GlobalShortcutExt, Modifiers, Shortcut, ShortcutState};

fn compact_shortcut() -> Shortcut {
    Shortcut::new(Some(Modifiers::CONTROL), Code::KeyH)
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let handler_shortcut = compact_shortcut();

    tauri::Builder::default()
        .plugin(
            tauri_plugin_global_shortcut::Builder::new()
                .with_handler(move |app, shortcut, event| {
                    if event.state != ShortcutState::Pressed || shortcut != &handler_shortcut {
                        return;
                    }

                    // Ctrl+H must work while focus is inside the YouTube/Twitch
                    // child WebView. The shortcut is only registered while our
                    // window is focused, so we do not steal it from other apps.
                    let focused = app
                        .get_window("main")
                        .and_then(|window| window.is_focused().ok())
                        .unwrap_or(false);
                    if focused {
                        if let Err(error) = commands::toggle_compact_sync(app) {
                            eprintln!("[lite-stream] compact mode toggle failed: {error}");
                        }
                    }
                })
                .build(),
        )
        .setup(|app| {
            // Single persistent WebView2 profile shared by every tab. Cookies
            // and localStorage live here, so logins survive tab hibernation
            // and application restarts.
            let app_data = app.path().app_local_data_dir()?;
            let data_directory = app_data.join("browser-profile");
            std::fs::create_dir_all(&data_directory)?;

            // Unpacked Chrome/Edge extensions live next to the profile and are
            // installed into the WebView2 profile when tabs are created.
            let extensions_directory = app_data.join("extensions");
            if let Err(error) = browser::prepare_extensions_directory(&extensions_directory) {
                eprintln!("[lite-stream] {error}");
            }

            let shared: SharedState = Arc::new(Mutex::new(AppState::new(
                data_directory,
                extensions_directory,
            )));
            app.manage(shared);

            // Register the shortcut now; focus events keep it in sync later.
            if let Err(error) = app.global_shortcut().register(compact_shortcut()) {
                eprintln!("[lite-stream] could not register Ctrl+H: {error}");
            }

            if let Some(window) = app.get_window("main") {
                let handle = app.handle().clone();
                window.on_window_event(move |event| match event {
                    WindowEvent::Resized(_) | WindowEvent::ScaleFactorChanged { .. } => {
                        browser::apply_layout(&handle);
                    }
                    WindowEvent::Focused(focused) => {
                        // Only own Ctrl+H while the browser window is focused.
                        if *focused {
                            let _ = handle.global_shortcut().register(compact_shortcut());
                        } else {
                            let _ = handle.global_shortcut().unregister(compact_shortcut());
                        }
                    }
                    _ => {}
                });
            }

            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            commands::get_state,
            commands::open_tab,
            commands::switch_tab,
            commands::close_tab,
            commands::navigate,
            commands::go_back,
            commands::reload,
            commands::hibernate,
            commands::wake,
            commands::import_twitch_token,
            commands::toggle_compact,
            commands::toggle_always_on_top,
            commands::open_extensions_folder,
            commands::list_extensions,
        ])
        .run(tauri::generate_context!())
        .expect("error while running Lite Stream Browser");
}
