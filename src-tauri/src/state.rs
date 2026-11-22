use serde::Serialize;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard};

/// Shared application state. Every command and WebView callback accesses it
/// through this single mutex, so the state is always consistent.
pub type SharedState = Arc<Mutex<AppState>>;

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TabInfo {
    pub id: u64,
    pub title: String,
    pub url: String,
    pub sleeping: bool,
}

/// The subset of state that the 36 px tab bar needs. It is sent to the UI
/// after every mutation and also returned by every command.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UiState {
    pub tabs: Vec<TabInfo>,
    pub active_id: Option<u64>,
    pub compact: bool,
    pub always_on_top: bool,
    pub profile_path: String,
    pub extensions_path: String,
}

#[derive(Debug)]
pub struct Tab {
    pub id: u64,
    pub title: String,
    pub url: String,
    /// A sleeping tab has no child WebView at all, so its renderer process is
    /// gone. Only `title` and `url` are kept.
    pub sleeping: bool,
    /// Label of the live child WebView. The label includes a generation
    /// counter, so a freshly created WebView never collides with one that is
    /// still being torn down by WebView2 asynchronously.
    pub webview_label: Option<String>,
}

#[derive(Debug)]
pub struct AppState {
    pub tabs: Vec<Tab>,
    pub active: Option<u64>,
    pub next_id: u64,
    pub compact: bool,
    pub always_on_top: bool,
    pub data_directory: PathBuf,
    /// Unpacked Chrome/Edge extensions live next to the browser profile.
    pub extensions_directory: PathBuf,
    /// Cached live WebView2 extension metadata (id/name/enabled) to avoid blocking UI.
    pub extension_cache: Vec<crate::browser::LiveExtension>,
}

impl AppState {
    pub fn new(data_directory: PathBuf, extensions_directory: PathBuf) -> Self {
        Self {
            tabs: Vec::new(),
            active: None,
            next_id: 1,
            compact: false,
            always_on_top: false,
            data_directory,
            extensions_directory,
            extension_cache: Vec::new(),
        }
    }

    pub fn snapshot(&self) -> UiState {
        UiState {
            tabs: self
                .tabs
                .iter()
                .map(|tab| TabInfo {
                    id: tab.id,
                    title: tab.title.clone(),
                    url: tab.url.clone(),
                    sleeping: tab.sleeping || tab.webview_label.is_none(),
                })
                .collect(),
            active_id: self.active,
            compact: self.compact,
            always_on_top: self.always_on_top,
            profile_path: self.data_directory.to_string_lossy().into_owned(),
            extensions_path: self.extensions_directory.to_string_lossy().into_owned(),
        }
    }

    pub fn tab(&self, id: u64) -> Option<&Tab> {
        self.tabs.iter().find(|tab| tab.id == id)
    }

    pub fn tab_mut(&mut self, id: u64) -> Option<&mut Tab> {
        self.tabs.iter_mut().find(|tab| tab.id == id)
    }

    pub fn active_tab(&self) -> Option<&Tab> {
        self.active.and_then(|id| self.tab(id))
    }

    /// Label of the live WebView of the active tab, if it is awake.
    pub fn active_webview_label(&self) -> Option<String> {
        self.active_tab()
            .filter(|tab| !tab.sleeping)
            .and_then(|tab| tab.webview_label.clone())
    }
}

/// Lock helper that survives a poisoned mutex instead of taking the whole app
/// down (a panic in a WebView callback must not brick the tab bar).
pub fn lock(state: &SharedState) -> MutexGuard<'_, AppState> {
    state
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}
