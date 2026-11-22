use crate::browser;
use crate::state::{self, SharedState, Tab, UiState};
use tauri::{AppHandle, Manager, Url};

fn shared_state(app: &AppHandle) -> SharedState {
    app.state::<SharedState>().inner().clone()
}

// ---------------------------------------------------------------------------
// URL helpers
// ---------------------------------------------------------------------------

/// Accepts full URLs, bare domains, localhost and plain search queries.
fn normalize_url(input: &str) -> Result<Url, String> {
    let input = input.trim();
    if input.is_empty() {
        return Err("Введите URL или поисковый запрос".into());
    }

    let lower = input.to_ascii_lowercase();
    let candidate = if lower.starts_with("http://") || lower.starts_with("https://") {
        input.to_string()
    } else if lower.starts_with("localhost") || lower.starts_with("127.0.0.1") {
        format!("http://{input}")
    } else if input.contains(char::is_whitespace) || !input.contains('.') {
        let encoded: String = url::form_urlencoded::byte_serialize(input.as_bytes()).collect();
        format!("https://duckduckgo.com/?q={encoded}")
    } else {
        format!("https://{input}")
    };

    let parsed = Url::parse(&candidate).map_err(|error| format!("Некорректный URL: {error}"))?;
    match parsed.scheme() {
        "http" | "https" => Ok(parsed),
        _ => Err("Поддерживаются только http(s) URL".into()),
    }
}

fn title_from_url(raw: &str) -> String {
    let Ok(url) = Url::parse(raw) else {
        return raw.to_string();
    };
    let host = url.host_str().unwrap_or("").trim_start_matches("www.");
    match host {
        "youtube.com" | "m.youtube.com" | "music.youtube.com" => "YouTube".to_string(),
        "twitch.tv" | "m.twitch.tv" => "Twitch".to_string(),
        "google.com" | "accounts.google.com" => "Google".to_string(),
        "" => raw.to_string(),
        other => other.to_string(),
    }
}

fn active_label(shared: &SharedState) -> Option<String> {
    state::lock(shared).active_webview_label()
}

fn is_twitch_url(raw: &str) -> bool {
    Url::parse(raw)
        .ok()
        .and_then(|url| url.host_str().map(|host| host.to_ascii_lowercase()))
        .map(|host| host == "twitch.tv" || host.ends_with(".twitch.tv"))
        .unwrap_or(false)
}

fn canonical_cookie_name(raw: &str) -> Option<&'static str> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "auth-token" => Some("auth-token"),
        "twilight-user" => Some("twilight-user"),
        "persistent" => Some("persistent"),
        _ => None,
    }
}

fn upsert_cookie(cookies: &mut Vec<browser::ImportedCookie>, name: &'static str, value: &str) {
    let value = value.trim();
    if value.is_empty() {
        return;
    }
    match cookies.iter_mut().find(|cookie| cookie.name == name) {
        Some(cookie) => cookie.value = value.to_string(),
        None => cookies.push(browser::ImportedCookie::new(name, value)),
    }
}

/// Accepts either a bare `auth-token` value or a full cookie string copied
/// from the browser, e.g.
/// `auth-token=...; twilight-user=...; persistent=...`.
fn parse_twitch_cookie_input(
    input: &str,
    twilight_user: Option<&str>,
    persistent: Option<&str>,
) -> Result<Vec<browser::ImportedCookie>, String> {
    let input = input.trim();
    if input.is_empty() {
        return Err("Вставьте значение cookie auth-token".into());
    }

    let mut cookies = Vec::new();
    if input.contains('=') {
        for part in input.split(';') {
            let Some((name, value)) = part.split_once('=') else {
                continue;
            };
            if let Some(name) = canonical_cookie_name(name) {
                upsert_cookie(&mut cookies, name, value);
            }
        }
    } else {
        upsert_cookie(&mut cookies, "auth-token", input);
    }

    if let Some(value) = twilight_user {
        upsert_cookie(&mut cookies, "twilight-user", value);
    }
    if let Some(value) = persistent {
        upsert_cookie(&mut cookies, "persistent", value);
    }

    if !cookies.iter().any(|cookie| cookie.name == "auth-token") {
        return Err(
            "Не найден auth-token. Вставьте значение токена или строку cookie вида auth-token=..."
                .into(),
        );
    }

    Ok(cookies)
}

// ---------------------------------------------------------------------------
// Core operations (also used by the new-window handler on a worker thread)
// ---------------------------------------------------------------------------

pub fn open_tab_core(
    app: &AppHandle,
    shared: &SharedState,
    raw_url: String,
) -> Result<UiState, String> {
    let url = normalize_url(&raw_url)?;
    let id = {
        let mut guard = state::lock(shared);
        let id = guard.next_id;
        guard.next_id += 1;
        guard.tabs.push(Tab {
            id,
            title: title_from_url(url.as_str()),
            url: url.to_string(),
            sleeping: true,
            webview_label: None,
        });
        id
    };

    browser::activate_tab(app, shared, id)
}

// ---------------------------------------------------------------------------
// Tauri commands
// ---------------------------------------------------------------------------

#[tauri::command]
pub fn get_state(app: AppHandle) -> UiState {
    let shared = shared_state(&app);
    let snapshot = state::lock(&shared).snapshot();
    snapshot
}

#[tauri::command]
pub async fn open_tab(app: AppHandle, url: String) -> Result<UiState, String> {
    let shared = shared_state(&app);
    open_tab_core(&app, &shared, url)
}

#[tauri::command]
pub async fn switch_tab(app: AppHandle, id: u64) -> Result<UiState, String> {
    let shared = shared_state(&app);
    browser::activate_tab(&app, &shared, id)
}

#[tauri::command]
pub async fn close_tab(app: AppHandle, id: u64) -> Result<UiState, String> {
    let shared = shared_state(&app);

    let (label, next_id) = {
        let mut guard = state::lock(&shared);
        let Some(index) = guard.tabs.iter().position(|tab| tab.id == id) else {
            drop(guard);
            return Ok(browser::snapshot_and_emit(&app, &shared));
        };

        let removed = guard.tabs.remove(index);
        let was_active = guard.active == Some(id);
        if was_active {
            guard.active = None;
        }

        // Prefer the tab that took the closed tab's place, otherwise the last
        // one. Closing a background tab does not change the active WebView.
        let next_id = if was_active && !guard.tabs.is_empty() {
            let next_index = index.min(guard.tabs.len() - 1);
            Some(guard.tabs[next_index].id)
        } else {
            None
        };

        (removed.webview_label, next_id)
    };

    if let Some(label) = label {
        browser::close_webview(&app, &label);
    }

    match next_id {
        Some(id) => browser::activate_tab(&app, &shared, id),
        None => Ok(browser::snapshot_and_emit(&app, &shared)),
    }
}

#[tauri::command]
pub async fn navigate(app: AppHandle, url: String) -> Result<UiState, String> {
    let shared = shared_state(&app);
    let parsed = normalize_url(&url)?;
    let normalized = parsed.to_string();

    let active_id = state::lock(&shared).active;
    let Some(id) = active_id else {
        return open_tab_core(&app, &shared, normalized);
    };

    let label = {
        let mut guard = state::lock(&shared);
        let Some(tab) = guard.tab_mut(id) else {
            drop(guard);
            return open_tab_core(&app, &shared, normalized);
        };
        tab.url = normalized.clone();
        tab.title = title_from_url(&normalized);
        if tab.sleeping {
            None
        } else {
            tab.webview_label.clone()
        }
    };

    if let Some(label) = label {
        if let Some(webview) = app.get_webview(&label) {
            webview
                .navigate(parsed)
                .map_err(|error| format!("Не удалось открыть URL: {error}"))?;
            return Ok(browser::snapshot_and_emit(&app, &shared));
        }
    }

    // The tab was sleeping (or its WebView disappeared): recreate it from the
    // URL we just stored. Auth comes from the shared persistent profile.
    {
        let mut guard = state::lock(&shared);
        if let Some(tab) = guard.tab_mut(id) {
            tab.sleeping = true;
            tab.webview_label = None;
        }
    }
    browser::activate_tab(&app, &shared, id)
}

#[tauri::command]
pub async fn go_back(app: AppHandle) -> Result<UiState, String> {
    let shared = shared_state(&app);
    if let Some(label) = active_label(&shared) {
        if let Some(webview) = app.get_webview(&label) {
            webview
                .eval("history.back()")
                .map_err(|error| format!("history.back() failed: {error}"))?;
        }
    }
    Ok(browser::snapshot_and_emit(&app, &shared))
}

#[tauri::command]
pub async fn reload(app: AppHandle) -> Result<UiState, String> {
    let shared = shared_state(&app);
    if let Some(label) = active_label(&shared) {
        if let Some(webview) = app.get_webview(&label) {
            webview
                .reload()
                .map_err(|error| format!("reload failed: {error}"))?;
        }
    }
    Ok(browser::snapshot_and_emit(&app, &shared))
}

/// Destroys the active child WebView, keeping URL and title in `AppState`.
#[tauri::command]
pub async fn hibernate(app: AppHandle) -> Result<UiState, String> {
    let shared = shared_state(&app);

    let label = {
        let mut guard = state::lock(&shared);
        let Some(id) = guard.active else {
            drop(guard);
            return Ok(browser::snapshot_and_emit(&app, &shared));
        };
        guard.tab_mut(id).and_then(|tab| {
            tab.sleeping = true;
            tab.webview_label.take()
        })
    };

    if let Some(label) = label {
        browser::close_webview(&app, &label);
    }

    Ok(browser::snapshot_and_emit(&app, &shared))
}

/// Recreates the WebView of the active (sleeping) tab from its saved URL.
#[tauri::command]
pub async fn wake(app: AppHandle) -> Result<UiState, String> {
    let shared = shared_state(&app);
    let active = state::lock(&shared).active;

    match active {
        Some(id) => browser::activate_tab(&app, &shared, id),
        None => Ok(browser::snapshot_and_emit(&app, &shared)),
    }
}

#[tauri::command]
pub async fn toggle_compact(app: AppHandle) -> Result<UiState, String> {
    toggle_compact_sync(&app)
}

/// Also called directly from the global Ctrl+H shortcut handler.
pub fn toggle_compact_sync(app: &AppHandle) -> Result<UiState, String> {
    let shared = shared_state(app);
    {
        let mut guard = state::lock(&shared);
        guard.compact = !guard.compact;
    }
    browser::apply_layout(app);
    Ok(browser::snapshot_and_emit(app, &shared))
}

/// Imports a Twitch session by writing `auth-token` (and optional
/// `twilight-user`/`persistent`) cookies into the persistent WebView2 profile,
/// then reloads the active Twitch tab. The login request itself never goes
/// through the anti-bot check because the token comes from a normal browser.
#[tauri::command]
pub async fn import_twitch_token(
    app: AppHandle,
    token: String,
    twilight_user: Option<String>,
    persistent: Option<String>,
) -> Result<UiState, String> {
    let cookies =
        parse_twitch_cookie_input(&token, twilight_user.as_deref(), persistent.as_deref())?;
    let shared = shared_state(&app);

    let target = {
        let guard = state::lock(&shared);
        guard
            .active_tab()
            .filter(|tab| is_twitch_url(&tab.url))
            .map(|tab| tab.id)
            .or_else(|| {
                guard
                    .tabs
                    .iter()
                    .find(|tab| is_twitch_url(&tab.url))
                    .map(|tab| tab.id)
            })
    };

    // Use an existing Twitch tab or open one, so the cookie lands in the same
    // profile and the page can be reloaded immediately.
    match target {
        Some(id) => {
            browser::activate_tab(&app, &shared, id)?;
        }
        None => {
            let snapshot = open_tab_core(&app, &shared, "https://www.twitch.tv".to_string())?;
            if snapshot.active_id.is_none() {
                return Err("Не удалось открыть вкладку Twitch".into());
            }
        }
    }

    let label = state::lock(&shared)
        .active_webview_label()
        .ok_or_else(|| "WebView вкладки Twitch недоступен".to_string())?;
    let webview = app
        .get_webview(&label)
        .ok_or_else(|| "WebView вкладки Twitch не найден".to_string())?;

    browser::install_twitch_cookies(&webview, cookies)?;

    // The cookie is already committed at this point: reload so Twitch starts
    // an authenticated session.
    if let Some(webview) = app.get_webview(&label) {
        webview
            .reload()
            .map_err(|error| format!("Не удалось перезагрузить Twitch: {error}"))?;
    }

    Ok(browser::snapshot_and_emit(&app, &shared))
}

#[tauri::command]
pub async fn toggle_always_on_top(app: AppHandle) -> Result<UiState, String> {
    let shared = shared_state(&app);
    let next = {
        let mut guard = state::lock(&shared);
        guard.always_on_top = !guard.always_on_top;
        guard.always_on_top
    };

    if let Some(window) = app.get_window("main") {
        window
            .set_always_on_top(next)
            .map_err(|error| format!("Не удалось переключить Always on Top: {error}"))?;
    }

    Ok(browser::snapshot_and_emit(&app, &shared))
}

// ---------------------------------------------------------------------------
// Browser extensions
// ---------------------------------------------------------------------------

fn extensions_directory(shared: &SharedState) -> std::path::PathBuf {
    state::lock(shared).extensions_directory.clone()
}

/// Creates the extensions folder if needed and opens it in Windows Explorer.
#[tauri::command]
pub fn open_extensions_folder(app: AppHandle) -> Result<String, String> {
    let shared = shared_state(&app);
    let directory = extensions_directory(&shared);
    browser::prepare_extensions_directory(&directory)?;

    #[cfg(windows)]
    {
        // explorer.exe returns a non-zero exit code even on success, so it is
        // spawned without waiting for the process.
        std::process::Command::new("explorer.exe")
            .arg(&directory)
            .spawn()
            .map_err(|error| format!("Не удалось открыть Проводник: {error}"))?;
    }

    Ok(directory.to_string_lossy().into_owned())
}

/// Returns unpacked extensions found in the extensions folder, merged with cached
/// live extension metadata when available.
#[tauri::command]
pub async fn list_extensions(app: AppHandle) -> Result<Vec<browser::ExtensionInfo>, String> {
    let shared = shared_state(&app);
    let directory = extensions_directory(&shared);
    browser::prepare_extensions_directory(&directory)?;
    browser::refresh_extension_cache(&app, &shared);
    let live = state::lock(&shared).extension_cache.clone();
    Ok(browser::collect_extensions_cached(&directory, &live))
}

// ---------------------------------------------------------------------------
// Navigation helpers used by the toolbar / global Ctrl+Tab shortcut
// ---------------------------------------------------------------------------

/// Cycles through tabs. A background WebView is always hibernated by
/// `activate_tab`, so Ctrl+Tab keeps the one-WebView memory guarantee.
#[tauri::command]
pub async fn cycle_tab(app: AppHandle, direction: Option<i32>) -> Result<UiState, String> {
    cycle_tab_sync(&app, direction.unwrap_or(1))
}

/// Also called from the global Ctrl+Tab shortcut handler.
pub fn cycle_tab_sync(app: &AppHandle, direction: i32) -> Result<UiState, String> {
    let shared = shared_state(app);

    let target = {
        let guard = state::lock(&shared);
        if guard.tabs.is_empty() {
            None
        } else {
            let step: isize = if direction < 0 { -1 } else { 1 };
            let index = guard
                .active
                .and_then(|id| guard.tabs.iter().position(|tab| tab.id == id))
                .map(|index| {
                    (index as isize + step).rem_euclid(guard.tabs.len() as isize) as usize
                })
                .unwrap_or(0);
            Some(guard.tabs[index].id)
        }
    };

    match target {
        Some(id) => browser::activate_tab(app, &shared, id),
        None => Ok(browser::snapshot_and_emit(app, &shared)),
    }
}

/// Hides or shows the active child WebView without destroying it.
///
/// A native child WebView is composited above the main HTML webview, so the
/// settings dialog cannot cover it. Hiding keeps the stream alive (unlike
/// hibernation) while the user works in the settings panel.
#[tauri::command]
pub async fn set_webview_visible(app: AppHandle, visible: bool) -> Result<UiState, String> {
    let shared = shared_state(&app);
    let label = state::lock(&shared).active_webview_label();

    if let Some(label) = label {
        if let Some(webview) = app.get_webview(&label) {
            if visible {
                webview
                    .show()
                    .map_err(|error| format!("Не удалось показать WebView: {error}"))?;
                browser::apply_layout(&app);
                let _ = webview.set_focus();
            } else {
                webview
                    .hide()
                    .map_err(|error| format!("Не удалось скрыть WebView: {error}"))?;
                if let Some(main) = app.get_webview("main") {
                    let _ = main.set_focus();
                }
            }
        }
    }

    Ok(browser::snapshot_and_emit(&app, &shared))
}

/// RAM/CPU of the whole process tree, sampled for the home screen.
#[tauri::command]
pub fn get_runtime_stats() -> crate::metrics::RuntimeStats {
    crate::metrics::sample()
}

/// Toggles the Stream UI Studio panel in the active child WebView.
/// Returns `true` when the page reports a supported platform and toggle hook.
#[tauri::command]
pub async fn toggle_ui_studio(app: AppHandle) -> Result<bool, String> {
    let shared = shared_state(&app);
    let label = state::lock(&shared)
        .active_webview_label()
        .ok_or_else(|| "Нет активной вкладки".to_string())?;
    let webview = app
        .get_webview(&label)
        .ok_or_else(|| "WebView вкладки недоступен".to_string())?;

    let script = r#"(async () => {
      try {
        if (typeof window.__SUI_TOGGLE_PANEL__ === 'function') {
          return !!window.__SUI_TOGGLE_PANEL__();
        }
        return false;
      } catch (_) { return false; }
    })()"#;

    webview
        .eval(script)
        .map_err(|error| format!("Не удалось открыть редактор интерфейса: {error}"))?;
    Ok(true)
}

/// Lists installed unpacked extensions with popup/options/icon metadata.
/// Fast path: reads manifests/icons from disk and uses cached live extension IDs.
/// Pass refresh=true to query WebView2 for live IDs (slow; use on explicit refresh).
#[tauri::command]
pub fn list_installed_extensions(app: AppHandle, refresh: Option<bool>) -> Result<Vec<browser::InstalledExtensionInfo>, String> {
    let shared = shared_state(&app);
    let extensions_dir = {
        let app_state = state::lock(&shared);
        app_state.extensions_directory.clone()
    };
    browser::prepare_extensions_directory(&extensions_dir)?;

    if refresh.unwrap_or(false) {
        browser::refresh_extension_cache(&app, &shared);
    }

    let live = {
        let app_state = state::lock(&shared);
        app_state.extension_cache.clone()
    };
    Ok(browser::collect_installed_extensions_cached(&extensions_dir, &live))
}

/// Opens an extension popup in a compact child webview, or closes it if open.
#[tauri::command]
pub fn open_extension_popup(app: AppHandle, extension_id: String, popup_path: String) -> Result<bool, String> {
    let shared = shared_state(&app);
    browser::toggle_extension_popup(&app, &shared, &extension_id, &popup_path)
}

/// Closes the extension popup webview if it is open.
#[tauri::command]
pub fn close_extension_popup(app: AppHandle) -> bool {
    browser::close_extension_popup(&app)
}

/// Overlay visibility for modal dialogs (settings/extensions). Hides/shows active child webview.
#[tauri::command]
pub async fn set_overlay_active(app: AppHandle, active: bool) -> Result<UiState, String> {
    set_webview_visible(app, !active).await
}

