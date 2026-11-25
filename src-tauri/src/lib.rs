mod browser;
mod commands;
mod metrics;
mod native_messaging;
mod state;

use state::{AppState, SharedState};
use std::sync::{Arc, Mutex};
use tauri::{AppHandle, Manager, WindowEvent};
use tauri_plugin_global_shortcut::{Code, GlobalShortcutExt, Modifiers, Shortcut, ShortcutState};

fn compact_shortcut() -> Shortcut {
    Shortcut::new(Some(Modifiers::CONTROL), Code::KeyH)
}

/// Ctrl+Tab switches to the next tab. The previous tab is always hibernated by
/// `activate_tab`, so the one-WebView memory guarantee is preserved.
fn next_tab_shortcut() -> Shortcut {
    Shortcut::new(Some(Modifiers::CONTROL), Code::Tab)
}

/// Registered only while the app window is focused, so the shortcuts are not
/// stolen from other applications and still work when the child WebView has
/// keyboard focus.
fn register_shortcuts(app: &AppHandle) {
    for shortcut in [compact_shortcut(), next_tab_shortcut()] {
        if let Err(error) = app.global_shortcut().register(shortcut) {
            eprintln!("[lite-stream] could not register {shortcut:?}: {error}");
        }
    }
}

fn unregister_shortcuts(app: &AppHandle) {
    for shortcut in [compact_shortcut(), next_tab_shortcut()] {
        let _ = app.global_shortcut().unregister(shortcut);
    }
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let handler_compact = compact_shortcut();
    let handler_next_tab = next_tab_shortcut();

    tauri::Builder::default()
        .plugin(
            tauri_plugin_global_shortcut::Builder::new()
                .with_handler(move |app, shortcut, event| {
                    if event.state != ShortcutState::Pressed {
                        return;
                    }

                    // Shortcuts must work while focus is inside the
                    // YouTube/Twitch child WebView. They are only registered
                    // while our window is focused, so we do not steal them
                    // from other apps.
                    let focused = app
                        .get_window("main")
                        .and_then(|window| window.is_focused().ok())
                        .unwrap_or(false);
                    if !focused {
                        return;
                    }

                    if shortcut == &handler_compact {
                        if let Err(error) = commands::toggle_compact_sync(app) {
                            eprintln!("[lite-stream] compact mode toggle failed: {error}");
                        }
                    } else if shortcut == &handler_next_tab {
                        if let Err(error) = commands::cycle_tab_sync(app, 1) {
                            eprintln!("[lite-stream] tab switch failed: {error}");
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

            // Register the shortcuts now; focus events keep them in sync later.
            register_shortcuts(app.handle());

            if let Some(window) = app.get_window("main") {
                let handle = app.handle().clone();
                window.on_window_event(move |event| match event {
                    WindowEvent::Resized(_) | WindowEvent::ScaleFactorChanged { .. } => {
                        browser::apply_layout(&handle);
                    }
                    WindowEvent::Focused(focused) => {
                        // Only own Ctrl+H / Ctrl+Tab while the browser window
                        // is focused.
                        if *focused {
                            register_shortcuts(&handle);
                        } else {
                            unregister_shortcuts(&handle);
                        }
                    }
                    WindowEvent::Destroyed => {
                        // Never leave a native host process behind on exit.
                        native_messaging::disconnect_all();
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
            commands::show_home,
            commands::cycle_tab,
            commands::import_twitch_token,
            commands::toggle_compact,
            commands::toggle_always_on_top,
            commands::set_webview_visible,
            commands::open_extensions_folder,
            commands::list_extensions,
            commands::get_runtime_stats,
            commands::toggle_ui_studio,
            commands::list_installed_extensions,
            commands::open_extension_popup,
            commands::close_extension_popup,
            commands::set_overlay_active,
            native_messaging::native_connect,
            native_messaging::native_post_message,
            native_messaging::native_disconnect,
        ])
        .run(tauri::generate_context!())
        .expect("error while running Lite Stream Browser");
}
