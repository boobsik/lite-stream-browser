fn main() {
    // Generate `allow-<command>` / `deny-<command>` permissions for app
    // commands. Native Messaging is called from chrome-extension:// pages,
    // which Tauri treats as remote origins, so those commands must be granted
    // by an explicit capability instead of relying on the local-only default.
    let attributes = tauri_build::Attributes::new().app_manifest(
        tauri_build::AppManifest::new().commands(&[
            "get_state",
            "open_tab",
            "switch_tab",
            "close_tab",
            "navigate",
            "go_back",
            "reload",
            "hibernate",
            "wake",
            "cycle_tab",
            "import_twitch_token",
            "toggle_compact",
            "toggle_always_on_top",
            "set_webview_visible",
            "open_extensions_folder",
            "list_extensions",
            "get_runtime_stats",
            "toggle_ui_studio",
            "list_installed_extensions",
            "open_extension_popup",
            "close_extension_popup",
            "set_overlay_active",
            "native_connect",
            "native_post_message",
            "native_disconnect",
        ]),
    );
    tauri_build::try_build(attributes).expect("failed to run tauri-build");
}
