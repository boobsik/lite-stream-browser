use crate::state::{self, SharedState, UiState};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;
use tauri::{
    webview::{NewWindowResponse, WebviewBuilder},
    AppHandle, Emitter, Manager, PhysicalPosition, PhysicalSize, Url, WebviewUrl, Window,
};

#[cfg(windows)]
use webview2_com::Microsoft::Web::WebView2::Win32::{
    ICoreWebView2BrowserExtension, ICoreWebView2BrowserExtensionList, ICoreWebView2Profile7,
    ICoreWebView2Settings2, ICoreWebView2_13, ICoreWebView2_2,
    COREWEBVIEW2_COOKIE_SAME_SITE_KIND_NONE,
};
#[cfg(windows)]
use webview2_com::{
    ProfileAddBrowserExtensionCompletedHandler, ProfileGetBrowserExtensionsCompletedHandler,
};
#[cfg(windows)]
use windows_core::Interface;

/// Height of the HTML tab bar, in logical pixels.
pub const PANEL_HEIGHT: f64 = 36.0;

/// Extra WebView2 launch flags.
///
/// Tauri/wry replaces its defaults when this is set, so the default
/// `--disable-features` list must be repeated here. The second flag stops
/// Chromium from advertising `navigator.webdriver = true`, which both Google
/// and Twitch use to reject embedded browsers.
const BROWSER_ARGS: &str = "--disable-features=msWebOOUI,msPdfOOUI,msSmartScreenProtection \
                            --disable-blink-features=AutomationControlled";

/// Runs before any page script (document start) in every tab.
///
/// The native WebView2 engine is kept as-is, so the Chromium version in the UA
/// is always current. We only remove the Edge/WebView2 product tokens from the
/// JavaScript-visible UA and the Client Hints brands, and hide the automation
/// marker. This keeps Google and Twitch from treating the webview as an
/// unsupported embedded browser without freezing a stale Chrome version.
const INITIALIZATION_SCRIPT: &str = r#"
(() => {
  "use strict";

  // Do not expose the automation flag to page scripts.
  try {
    Object.defineProperty(navigator, "webdriver", {
      get: () => undefined,
      configurable: true,
    });
  } catch (_) {}

  // Keep the real WebView2 Chromium version but drop only the Edge/WebView2
  // product tokens, so the UA looks like regular desktop Chrome.
  const nativeUserAgent = String(navigator.userAgent || "");
  const userAgent = nativeUserAgent
    .replace(/\s+(?:Edg|EdgA|EdgiOS)\/[\d.]+/gi, "")
    .replace(/\s+WebView2\/[\d.]+/gi, "")
    .replace(/\s+Microsoft\s+Edge\s+WebView2/gi, "")
    .replace(/\s{2,}/g, " ")
    .trim();

  if (userAgent && userAgent !== nativeUserAgent) {
    Object.defineProperty(navigator, "userAgent", {
      get: () => userAgent,
      configurable: true,
    });
    Object.defineProperty(navigator, "appVersion", {
      get: () => userAgent.replace(/^Mozilla\//, ""),
      configurable: true,
    });
  }

  // Align Client Hints with the "Google Chrome" brand while preserving the
  // Chromium version reported by the installed WebView2 runtime.
  const nativeUAData = navigator.userAgentData;
  const versionMatch = userAgent.match(/(?:Chrome|Chromium)\/([\d.]+)/i);
  if (!nativeUAData || !versionMatch) {
    return;
  }

  const fullVersion = versionMatch[1];
  const majorVersion = fullVersion.split(".")[0];
  const nativeBrands = Array.from(nativeUAData.brands || []);
  const grease = nativeBrands.find(
    (brand) => !/chromium|google|edge|webview/i.test(brand.brand),
  );
  const brands = [
    grease
      ? { brand: grease.brand, version: grease.version }
      : { brand: "Not?A_Brand", version: "99" },
    { brand: "Google Chrome", version: majorVersion },
    { brand: "Chromium", version: majorVersion },
  ];

  const platform = nativeUAData.platform || "Windows";
  const patchedUAData = Object.create(nativeUAData);
  Object.defineProperty(patchedUAData, "brands", {
    get: () => brands,
    enumerable: true,
  });
  Object.defineProperty(patchedUAData, "platform", {
    get: () => platform,
    enumerable: true,
  });
  Object.defineProperty(patchedUAData, "toJSON", {
    configurable: true,
    value: () => ({ brands, mobile: false, platform }),
  });

  const nativeHighEntropyValues =
    nativeUAData.getHighEntropyValues.bind(nativeUAData);
  Object.defineProperty(patchedUAData, "getHighEntropyValues", {
    configurable: true,
    value: async (hints) => {
      const values = await nativeHighEntropyValues(hints);
      values.brands = brands;
      if (Array.isArray(values.fullVersionList)) {
        values.fullVersionList = brands.map((brand) => ({
          brand: brand.brand,
          version: fullVersion,
        }));
      }
      if ("uaFullVersion" in values) {
        values.uaFullVersion = fullVersion;
      }
      return values;
    },
  });

  Object.defineProperty(navigator, "userAgentData", {
    get: () => patchedUAData,
    configurable: true,
  });
})();
"#;

static WEBVIEW_SEQUENCE: AtomicU64 = AtomicU64::new(1);

/// Removes the Edge/WebView2 product tokens from a User-Agent string while
/// keeping the real Chromium version, e.g.
/// `... Chrome/157.0.0.0 Safari/537.36 Edg/157.0.0.0` ->
/// `... Chrome/157.0.0.0 Safari/537.36`.
fn strip_embedded_browser_tokens(user_agent: &str) -> String {
    user_agent
        .replace("Microsoft Edge WebView2", "")
        .split_whitespace()
        .filter(|token| {
            let token = token.to_ascii_lowercase();
            !(token.starts_with("edg/")
                || token.starts_with("edga/")
                || token.starts_with("edgios/")
                || token.starts_with("edge/")
                || token.contains("webview2"))
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// Rewrites the network User-Agent of one child WebView.
///
/// Twitch's `passport.twitch.tv` and Google's sign-in endpoints inspect the
/// HTTP `User-Agent` header (not just the JavaScript-visible UA), so the Edge
/// token has to be removed at the WebView2 settings level. The native UA is
/// read back first, which keeps the Chromium version of the installed
/// WebView2 runtime (no hardcoded/stale Chrome version).
#[cfg(windows)]
fn apply_clean_user_agent(webview: &tauri::Webview) {
    let result = webview.with_webview(|platform| unsafe {
        let settings = match platform
            .controller()
            .CoreWebView2()
            .and_then(|core| core.Settings())
        {
            Ok(settings) => settings,
            Err(error) => {
                eprintln!("[lite-stream] could not read WebView2 settings: {error}");
                return;
            }
        };

        // `UserAgent`/`SetUserAgent` live on ICoreWebView2Settings2, while
        // `Settings()` returns the base interface.
        let settings = match settings.cast::<ICoreWebView2Settings2>() {
            Ok(settings) => settings,
            Err(error) => {
                eprintln!("[lite-stream] WebView2 settings2 is unavailable: {error}");
                return;
            }
        };

        // The COM getter allocates the string with CoTaskMemAlloc; `take_pwstr`
        // copies it into a Rust `String` and frees the original buffer.
        let mut raw = webview2_com::CoTaskMemPWSTR::default().take();
        if let Err(error) = settings.UserAgent(&mut raw) {
            eprintln!("[lite-stream] could not read the WebView2 User-Agent: {error}");
            return;
        }

        let native = webview2_com::take_pwstr(raw);
        let cleaned = strip_embedded_browser_tokens(&native);
        if cleaned == native {
            return;
        }

        let cleaned_wide = windows_core::HSTRING::from(cleaned.as_str());
        if let Err(error) = settings.SetUserAgent(&cleaned_wide) {
            eprintln!("[lite-stream] could not set the WebView2 User-Agent: {error}");
        }
    });

    if let Err(error) = result {
        eprintln!("[lite-stream] could not access the WebView2 controller: {error}");
    }
}

#[cfg(not(windows))]
fn apply_clean_user_agent(_webview: &tauri::Webview) {}

/// One cookie to write into the shared WebView2 profile.
#[derive(Clone, Debug)]
pub struct ImportedCookie {
    pub name: String,
    pub value: String,
}

impl ImportedCookie {
    pub fn new(name: impl Into<String>, value: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            value: value.into(),
        }
    }
}

/// Writes Twitch session cookies into the profile and waits until WebView2 has
/// applied them, so the caller can safely reload the tab afterwards.
///
/// A one-year expiry plus the persistent `data_directory` means the session
/// survives application restarts without repeating the import.
#[cfg(windows)]
pub fn install_twitch_cookies(
    webview: &tauri::Webview,
    cookies: Vec<ImportedCookie>,
) -> Result<(), String> {
    let (tx, rx) = std::sync::mpsc::channel();
    webview
        .with_webview(move |platform| {
            let _ = tx.send(set_twitch_cookies(platform, &cookies));
        })
        .map_err(|error| format!("Не удалось получить доступ к WebView2: {error}"))?;

    rx.recv_timeout(std::time::Duration::from_secs(15))
        .map_err(|_| "WebView2 не подтвердил установку cookie вовремя".to_string())?
}

#[cfg(not(windows))]
pub fn install_twitch_cookies(
    _webview: &tauri::Webview,
    _cookies: Vec<ImportedCookie>,
) -> Result<(), String> {
    Err("Импорт cookie поддерживается только на Windows".into())
}

#[cfg(windows)]
fn set_twitch_cookies(
    platform: tauri::webview::PlatformWebview,
    cookies: &[ImportedCookie],
) -> Result<(), String> {
    unsafe {
        let core = platform
            .controller()
            .CoreWebView2()
            .map_err(|error| format!("Не удалось получить CoreWebView2: {error}"))?;

        // CookieManager was introduced in ICoreWebView2_2.
        let manager = core
            .cast::<ICoreWebView2_2>()
            .map_err(|error| format!("ICoreWebView2_2 недоступен: {error}"))?
            .CookieManager()
            .map_err(|error| format!("Не удалось получить CookieManager: {error}"))?;

        let domain = windows_core::HSTRING::from(".twitch.tv");
        let path = windows_core::HSTRING::from("/");
        let expires = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(|error| format!("Системные часы недоступны: {error}"))?
            .as_secs_f64()
            + 365.0 * 24.0 * 60.0 * 60.0;

        for cookie in cookies {
            let name = windows_core::HSTRING::from(cookie.name.as_str());
            let value = windows_core::HSTRING::from(cookie.value.as_str());

            let entry = manager
                .CreateCookie(&name, &value, &domain, &path)
                .map_err(|error| format!("Не удалось создать cookie {}: {error}", cookie.name))?;

            entry
                .SetIsSecure(true)
                .map_err(|error| format!("cookie {}: SetIsSecure: {error}", cookie.name))?;
            entry
                .SetIsHttpOnly(false)
                .map_err(|error| format!("cookie {}: SetIsHttpOnly: {error}", cookie.name))?;
            entry
                .SetSameSite(COREWEBVIEW2_COOKIE_SAME_SITE_KIND_NONE)
                .map_err(|error| format!("cookie {}: SetSameSite: {error}", cookie.name))?;
            entry
                .SetExpires(expires)
                .map_err(|error| format!("cookie {}: SetExpires: {error}", cookie.name))?;

            manager
                .AddOrUpdateCookie(&entry)
                .map_err(|error| format!("Не удалось сохранить cookie {}: {error}", cookie.name))?;
        }

        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Browser extensions
// ---------------------------------------------------------------------------

/// Where WebView2 remembers which unpacked folders were already added. The file
/// shares the extensions root, but only directories with a `manifest.json` are
/// treated as extensions, so the JSON is ignored by the scanner.
const EXTENSION_REGISTRY: &str = ".installed.json";

const EXTENSIONS_README: &str = "\
Lite Stream Browser — расширения WebView2
=========================================

Каждая подпапка с файлом manifest.json — это одно распакованное расширение
Chrome/Edge. Они автоматически подключаются к профилю browser-profile при
создании вкладки.

Куда положить расширения:
  * uBlock Origin — распакуйте релиз .zip из GitHub (папка с manifest.json);
  * 7TV — распакованное расширение из Chrome Web Store;
  * SponsorBlock — папка с manifest.json;
  * любые другие unpacked-расширения.

Не кладите сюда .crx/.zip и не вкладывайте папки друг в друга: сканер смотрит
только на верхний уровень каталога extensions.

После добавления новой папки перезапустите приложение. Уже открытую вкладку
можно перезагрузить (Ctrl+R) — WebView2 применит расширения при следующей
навигации.
";

/// One unpacked extension as seen by the home screen.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ExtensionInfo {
    pub name: String,
    pub version: String,
    pub path: String,
    pub id: Option<String>,
    /// `active` — loaded by the running profile, `disabled` — installed but
    /// turned off, `installed` — recorded as added, `pending` — new folder that
    /// will be installed with the next tab.
    pub status: String,
}

#[derive(Clone, Debug)]
pub struct LiveExtension {
    pub id: String,
    pub name: String,
    pub enabled: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct InstalledExtension {
    /// Canonical, lower-cased folder path — the deduplication key.
    key: String,
    name: String,
    id: String,
}

struct ExtensionFolder {
    key: String,
    path: PathBuf,
    name: String,
    version: String,
}

fn extension_key(path: &Path) -> String {
    path.canonicalize()
        .unwrap_or_else(|_| path.to_path_buf())
        .to_string_lossy()
        .replace('/', "\\")
        .to_lowercase()
}

fn read_manifest(dir: &Path) -> Option<(String, String)> {
    let raw = std::fs::read_to_string(dir.join("manifest.json")).ok()?;
    let manifest: serde_json::Value = serde_json::from_str(&raw).ok()?;

    let fallback = dir
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| "Расширение".to_string());

    let raw_name = manifest
        .get("name")
        .and_then(|value| value.as_str())
        .unwrap_or("")
        .trim();
    let name = if raw_name.is_empty() || raw_name.starts_with("__MSG_") {
        fallback
    } else {
        raw_name.to_string()
    };

    let version = manifest
        .get("version")
        .and_then(|value| value.as_str())
        .unwrap_or("")
        .trim()
        .to_string();

    Some((name, version))
}

fn scan_extension_folders(extensions_dir: &Path) -> Vec<ExtensionFolder> {
    let mut folders = Vec::new();
    let Ok(entries) = std::fs::read_dir(extensions_dir) else {
        return folders;
    };

    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let Some((name, version)) = read_manifest(&path) else {
            continue;
        };
        folders.push(ExtensionFolder {
            key: extension_key(&path),
            path,
            name,
            version,
        });
    }

    folders.sort_by_key(|folder| folder.name.to_lowercase());
    folders
}

fn registry_path(extensions_dir: &Path) -> PathBuf {
    extensions_dir.join(EXTENSION_REGISTRY)
}

fn load_registry(extensions_dir: &Path) -> Vec<InstalledExtension> {
    std::fs::read_to_string(registry_path(extensions_dir))
        .ok()
        .and_then(|raw| serde_json::from_str(&raw).ok())
        .unwrap_or_default()
}

fn save_registry(extensions_dir: &Path, records: &[InstalledExtension]) {
    let Ok(raw) = serde_json::to_string_pretty(records) else {
        return;
    };
    if let Err(error) = std::fs::write(registry_path(extensions_dir), raw) {
        eprintln!("[lite-stream] could not write the extensions registry: {error}");
    }
}

fn collect_infos(extensions_dir: &Path, live: &[LiveExtension]) -> Vec<ExtensionInfo> {
    let registry = load_registry(extensions_dir);

    scan_extension_folders(extensions_dir)
        .into_iter()
        .map(|folder| {
            let record = registry.iter().find(|record| record.key == folder.key);
            let mut info = ExtensionInfo {
                name: folder.name,
                version: folder.version,
                path: folder.path.to_string_lossy().into_owned(),
                id: record.map(|record| record.id.clone()),
                status: if record.is_some() {
                    "installed"
                } else {
                    "pending"
                }
                .to_string(),
            };

            if let Some(running) = live.iter().find(|extension| {
                info.id.as_deref() == Some(extension.id.as_str())
                    || extension.name.eq_ignore_ascii_case(&info.name)
            }) {
                info.id = Some(running.id.clone());
                info.status = if running.enabled {
                    "active"
                } else {
                    "disabled"
                }
                .to_string();
            }

            info
        })
        .collect()
}

/// Scans the extensions folder and returns what the home screen should show.
pub fn collect_extensions(
    webview: Option<&tauri::Webview>,
    extensions_dir: &Path,
) -> Vec<ExtensionInfo> {
    let live = webview
        .and_then(|webview| query_live_extensions(webview).ok())
        .unwrap_or_default();
    collect_infos(extensions_dir, &live)
}

/// Creates the extensions folder with a short Russian README (idempotent).
pub fn prepare_extensions_directory(extensions_dir: &Path) -> Result<(), String> {
    std::fs::create_dir_all(extensions_dir)
        .map_err(|error| format!("Не удалось создать папку расширений: {error}"))?;

    let readme = extensions_dir.join("README.txt");
    if !readme.exists() {
        if let Err(error) = std::fs::write(&readme, EXTENSIONS_README) {
            eprintln!("[lite-stream] could not write the extensions README: {error}");
        }
    }

    Ok(())
}

/// Installs every unpacked extension folder that is not in the registry yet and
/// returns the resulting list plus the number of newly added extensions.
///
/// WebView2 applies extensions on the next navigation, so the caller reloads
/// the tab when `added > 0`.
pub fn sync_extensions(
    webview: &tauri::Webview,
    extensions_dir: &Path,
) -> (Vec<ExtensionInfo>, usize) {
    if let Err(error) = prepare_extensions_directory(extensions_dir) {
        eprintln!("[lite-stream] {error}");
        return (Vec::new(), 0);
    }

    let folders = scan_extension_folders(extensions_dir);
    let mut registry = load_registry(extensions_dir);
    let known: HashSet<String> = registry.iter().map(|record| record.key.clone()).collect();
    let missing: Vec<PathBuf> = folders
        .iter()
        .filter(|folder| !known.contains(&folder.key))
        .map(|folder| folder.path.clone())
        .collect();

    let mut added = 0;
    if !missing.is_empty() {
        match add_extensions(webview, &missing) {
            Ok(results) => {
                for (path, outcome) in results {
                    match outcome {
                        Ok((id, name)) => {
                            let key = extension_key(&path);
                            match registry.iter_mut().find(|record| record.key == key) {
                                Some(record) => {
                                    record.id = id;
                                    record.name = name;
                                }
                                None => {
                                    registry.push(InstalledExtension { key, name, id });
                                    added += 1;
                                }
                            }
                        }
                        Err(error) => {
                            eprintln!(
                                "[lite-stream] could not install extension {}: {error}",
                                path.display()
                            );
                        }
                    }
                }
                if added > 0 {
                    save_registry(extensions_dir, &registry);
                }
            }
            Err(error) => {
                eprintln!("[lite-stream] could not install browser extensions: {error}");
            }
        }
    }

    (collect_infos(extensions_dir, &[]), added)
}

#[cfg(windows)]
type AddExtensionOutcome = (PathBuf, Result<(String, String), String>);

#[cfg(windows)]
fn add_extensions(
    webview: &tauri::Webview,
    paths: &[PathBuf],
) -> Result<Vec<AddExtensionOutcome>, String> {
    let total = paths.len();
    let (tx, rx) = std::sync::mpsc::channel();
    let paths = paths.to_vec();

    webview
        .with_webview(move |platform| unsafe {
            let profile = match platform
                .controller()
                .CoreWebView2()
                .and_then(|core| core.cast::<ICoreWebView2_13>())
                .and_then(|core| core.Profile())
                .and_then(|profile| profile.cast::<ICoreWebView2Profile7>())
            {
                Ok(profile) => profile,
                Err(error) => {
                    let message = format!("профиль WebView2 не поддерживает расширения: {error}");
                    for path in paths {
                        let _ = tx.send((path, Err(message.clone())));
                    }
                    return;
                }
            };

            for path in paths {
                let fallback = path.clone();
                let folder = windows_core::HSTRING::from(path.to_string_lossy().as_ref());
                let sender = tx.clone();
                let handler = ProfileAddBrowserExtensionCompletedHandler::create(Box::new(
                    move |result, extension| {
                        let outcome = match (result, extension) {
                            (Err(error), _) => Err(format!("WebView2: {error}")),
                            (Ok(()), None) => {
                                Err("WebView2 не вернул установленное расширение".to_string())
                            }
                            (Ok(()), Some(extension)) => read_extension_identity(&extension),
                        };
                        let _ = sender.send((fallback, outcome));
                        Ok(())
                    },
                ));

                if let Err(error) = profile.AddBrowserExtension(&folder, &handler) {
                    let _ = tx.send((path, Err(format!("AddBrowserExtension: {error}"))));
                }
            }
        })
        .map_err(|error| format!("Не удалось получить доступ к WebView2: {error}"))?;

    let mut results = Vec::with_capacity(total);
    while results.len() < total {
        match rx.recv_timeout(Duration::from_secs(20)) {
            Ok(result) => results.push(result),
            Err(_) => break,
        }
    }

    Ok(results)
}

#[cfg(not(windows))]
fn add_extensions(
    _webview: &tauri::Webview,
    _paths: &[PathBuf],
) -> Result<Vec<(PathBuf, Result<(String, String), String>)>, String> {
    Err("Расширения WebView2 поддерживаются только на Windows".into())
}

#[cfg(windows)]
pub fn query_live_extensions(webview: &tauri::Webview) -> Result<Vec<LiveExtension>, String> {
    let (tx, rx) = std::sync::mpsc::channel();

    webview
        .with_webview(move |platform| unsafe {
            let profile = match platform
                .controller()
                .CoreWebView2()
                .and_then(|core| core.cast::<ICoreWebView2_13>())
                .and_then(|core| core.Profile())
                .and_then(|profile| profile.cast::<ICoreWebView2Profile7>())
            {
                Ok(profile) => profile,
                Err(error) => {
                    let _ = tx.send(Err(format!("профиль WebView2 недоступен: {error}")));
                    return;
                }
            };

            let sender = tx.clone();
            let handler = ProfileGetBrowserExtensionsCompletedHandler::create(Box::new(
                move |result, list| {
                    let value = match (result, list) {
                        (Err(error), _) => Err(format!("WebView2: {error}")),
                        (Ok(()), None) => Ok(Vec::new()),
                        (Ok(()), Some(list)) => read_extension_list(&list),
                    };
                    let _ = sender.send(value);
                    Ok(())
                },
            ));

            if let Err(error) = profile.GetBrowserExtensions(&handler) {
                let _ = tx.send(Err(format!("GetBrowserExtensions: {error}")));
            }
        })
        .map_err(|error| format!("Не удалось получить доступ к WebView2: {error}"))?;

    rx.recv_timeout(Duration::from_secs(10))
        .map_err(|_| "WebView2 не вернул список расширений вовремя".to_string())?
}

#[cfg(not(windows))]
pub fn query_live_extensions(_webview: &tauri::Webview) -> Result<Vec<LiveExtension>, String> {
    Ok(Vec::new())
}

#[cfg(windows)]
unsafe fn read_extension_identity(
    extension: &ICoreWebView2BrowserExtension,
) -> Result<(String, String), String> {
    let mut id = webview2_com::CoTaskMemPWSTR::default().take();
    extension
        .Id(&mut id)
        .map_err(|error| format!("extension id: {error}"))?;
    let id = webview2_com::take_pwstr(id);

    let mut name = webview2_com::CoTaskMemPWSTR::default().take();
    extension
        .Name(&mut name)
        .map_err(|error| format!("extension name: {error}"))?;
    let name = webview2_com::take_pwstr(name);

    Ok((id, name))
}

#[cfg(windows)]
unsafe fn read_extension_list(
    list: &ICoreWebView2BrowserExtensionList,
) -> Result<Vec<LiveExtension>, String> {
    let mut count = 0u32;
    list.Count(&mut count)
        .map_err(|error| format!("Count: {error}"))?;

    let mut extensions = Vec::with_capacity(count as usize);
    for index in 0..count {
        let Ok(extension) = list.GetValueAtIndex(index) else {
            continue;
        };
        let (id, name) = read_extension_identity(&extension)?;
        let mut enabled = windows_core::BOOL(1);
        let enabled = match extension.IsEnabled(&mut enabled) {
            Ok(()) => enabled.as_bool(),
            Err(_) => true,
        };
        extensions.push(LiveExtension { id, name, enabled });
    }

    Ok(extensions)
}

pub fn snapshot_and_emit(app: &AppHandle, shared: &SharedState) -> UiState {
    let snapshot = state::lock(shared).snapshot();
    emit_state(app, &snapshot);
    snapshot
}

pub fn emit_state(app: &AppHandle, snapshot: &UiState) {
    if let Some(main) = app.get_webview("main") {
        if let Err(error) = main.emit("state-changed", snapshot) {
            eprintln!("[lite-stream] could not emit state: {error}");
        }
    }
}

/// Positions and resizes the active child WebView so it fills everything below
/// the 36 px panel. In compact mode the panel is gone and the WebView starts
/// at y = 0. Called on every window resize, DPI change and compact toggle.
pub fn apply_layout(app: &AppHandle) {
    let (label, compact) = {
        let shared = app.state::<SharedState>().inner().clone();
        let guard = state::lock(&shared);
        (guard.active_webview_label(), guard.compact)
    };

    let Some(label) = label else {
        return;
    };
    let Some(window) = app.get_window("main") else {
        return;
    };
    let Ok((y, width, height)) = child_bounds(&window, compact) else {
        return;
    };
    let Some(webview) = app.get_webview(&label) else {
        return;
    };

    if let Err(error) = webview.set_position(PhysicalPosition::new(0, y)) {
        eprintln!("[lite-stream] set_position failed: {error}");
    }
    if let Err(error) = webview.set_size(PhysicalSize::new(width, height)) {
        eprintln!("[lite-stream] set_size failed: {error}");
    }
}

/// Creates the child WebView for one tab and returns its unique label.
///
/// Important: this function must not be called on the Tauri main thread while
/// a synchronous command or event handler is running (WebView2 would
/// deadlock). Commands in this app are `async`, so they execute on a worker
/// thread, and the new-window handler explicitly spawns a thread.
pub fn create_child(
    app: &AppHandle,
    tab_id: u64,
    url: &str,
    compact: bool,
) -> Result<String, String> {
    let window = app
        .get_window("main")
        .ok_or_else(|| "main window is not available".to_string())?;

    let parsed_url = Url::parse(url).map_err(|error| format!("invalid URL: {error}"))?;
    let (y, width, height) = child_bounds(&window, compact)?;

    let data_directory = {
        let shared = app.state::<SharedState>().inner().clone();
        let guard = state::lock(&shared);
        guard.data_directory.clone()
    };

    if let Err(error) = std::fs::create_dir_all(&data_directory) {
        return Err(format!("could not create the profile directory: {error}"));
    }

    let label = format!(
        "tab-{tab_id}-{}",
        WEBVIEW_SEQUENCE.fetch_add(1, Ordering::Relaxed)
    );

    let url_app = app.clone();
    let title_app = app.clone();
    let popup_app = app.clone();

    let builder = WebviewBuilder::new(label.clone(), WebviewUrl::External(parsed_url))
        // One shared, persistent profile for every tab -> cookies, logins,
        // subscriptions and recommendations survive hibernation and restarts.
        .data_directory(data_directory)
        // Enables ICoreWebView2EnvironmentOptions::
        // SetAreBrowserExtensionsEnabled(true) through wry, so the profile can
        // load unpacked Chrome/Edge extensions from the extensions folder.
        .browser_extensions_enabled(true)
        // Keep the native, up-to-date system WebView2 engine and User-Agent
        // (Chromium 140+); only the JS-visible Edge/WebView2 tokens are
        // normalized by INITIALIZATION_SCRIPT below.
        .additional_browser_args(BROWSER_ARGS)
        .initialization_script(INITIALIZATION_SCRIPT)
        .focused(true)
        .on_page_load(move |_webview, payload| {
            update_tab_url(&url_app, tab_id, payload.url().to_string());
        })
        .on_document_title_changed(move |_webview, title| {
            update_tab_title(&title_app, tab_id, title);
        })
        .on_new_window(move |url, _features| {
            // target="_blank" links and OAuth popups become regular tabs so
            // they also use our persistent profile instead of a bare window.
            let app = popup_app.clone();
            let url = url.to_string();
            std::thread::spawn(move || {
                let shared = app.state::<SharedState>().inner().clone();
                if let Err(error) = crate::commands::open_tab_core(&app, &shared, url) {
                    eprintln!("[lite-stream] could not open popup as a tab: {error}");
                }
            });
            NewWindowResponse::Deny
        });

    let webview = window
        .add_child(
            builder,
            PhysicalPosition::new(0, y),
            PhysicalSize::new(width, height),
        )
        .map_err(|error| format!("could not create the child WebView: {error}"))?;

    // Google/Twitch sign-in servers inspect the HTTP User-Agent, so clean the
    // native WebView2 UA here as well, not only in the JavaScript bridge.
    apply_clean_user_agent(&webview);

    // Install unpacked extensions that appeared in the extensions folder since
    // the last tab was created. WebView2 applies them on the next navigation,
    // so reload once when something new was added.
    let extensions_directory = {
        let shared = app.state::<SharedState>().inner().clone();
        let guard = state::lock(&shared);
        guard.extensions_directory.clone()
    };
    let (_, added) = sync_extensions(&webview, &extensions_directory);
    if added > 0 {
        if let Err(error) = webview.reload() {
            eprintln!("[lite-stream] could not reload after installing extensions: {error}");
        }
    }

    // The tab bar was clicked, so page content should receive keyboard input.
    let _ = webview.set_focus();

    Ok(label)
}

pub fn close_webview(app: &AppHandle, label: &str) {
    if let Some(webview) = app.get_webview(label) {
        if let Err(error) = webview.close() {
            eprintln!("[lite-stream] could not close WebView {label}: {error}");
        }
    }
}

/// Activates a tab. Any previously active WebView is destroyed first (its
/// renderer is unloaded from RAM) and the target tab's WebView is created
/// lazily from its saved URL.
pub fn activate_tab(app: &AppHandle, shared: &SharedState, id: u64) -> Result<UiState, String> {
    let (close_labels, url, compact) = {
        let mut guard = state::lock(shared);

        let Some(index) = guard.tabs.iter().position(|tab| tab.id == id) else {
            return Err(format!("tab {id} does not exist"));
        };

        // Already active and alive: nothing to do.
        if guard.active == Some(id) && !guard.tabs[index].sleeping {
            let alive = guard.tabs[index]
                .webview_label
                .as_deref()
                .map(|label| app.get_webview(label).is_some())
                .unwrap_or(false);
            if alive {
                drop(guard);
                return Ok(snapshot_and_emit(app, shared));
            }
        }

        let mut close_labels = Vec::new();

        // Hibernate whichever tab is currently on screen.
        if let Some(previous_id) = guard.active {
            if previous_id != id {
                if let Some(previous) = guard.tab_mut(previous_id) {
                    if let Some(label) = previous.webview_label.take() {
                        close_labels.push(label);
                    }
                    previous.sleeping = true;
                }
            }
        }

        // Defensive: never leave two live WebViews for the same tab.
        if let Some(label) = guard.tabs[index].webview_label.take() {
            close_labels.push(label);
        }

        let url = guard.tabs[index].url.clone();
        let compact = guard.compact;
        (close_labels, url, compact)
    };

    // Lock is released: closing/creating WebViews may block on WebView2.
    for label in close_labels {
        close_webview(app, &label);
    }

    match create_child(app, id, &url, compact) {
        Ok(label) => {
            let snapshot = {
                let mut guard = state::lock(shared);
                guard.active = Some(id);
                if let Some(tab) = guard.tab_mut(id) {
                    tab.webview_label = Some(label);
                    tab.sleeping = false;
                }
                guard.snapshot()
            };
            emit_state(app, &snapshot);
            Ok(snapshot)
        }
        Err(error) => {
            let snapshot = {
                let mut guard = state::lock(shared);
                guard.active = Some(id);
                if let Some(tab) = guard.tab_mut(id) {
                    tab.webview_label = None;
                    tab.sleeping = true;
                }
                guard.snapshot()
            };
            emit_state(app, &snapshot);
            Err(error)
        }
    }
}

fn update_tab_url(app: &AppHandle, tab_id: u64, url: String) {
    let shared = app.state::<SharedState>().inner().clone();
    let snapshot = {
        let mut guard = state::lock(&shared);
        let Some(tab) = guard.tab_mut(tab_id) else {
            return;
        };
        if tab.url == url {
            return;
        }
        tab.url = url;
        guard.snapshot()
    };
    emit_state(app, &snapshot);
}

fn update_tab_title(app: &AppHandle, tab_id: u64, title: String) {
    let title = title.trim();
    if title.is_empty() {
        return;
    }

    let shared = app.state::<SharedState>().inner().clone();
    let snapshot = {
        let mut guard = state::lock(&shared);
        let Some(tab) = guard.tab_mut(tab_id) else {
            return;
        };
        if tab.title == title {
            return;
        }
        tab.title = title.to_string();
        guard.snapshot()
    };
    emit_state(app, &snapshot);
}

fn child_bounds(window: &Window, compact: bool) -> Result<(i32, u32, u32), String> {
    let size = window
        .inner_size()
        .map_err(|error| format!("could not read the window size: {error}"))?;
    let scale = window.scale_factor().unwrap_or(1.0);
    let panel = if compact { 0.0 } else { PANEL_HEIGHT * scale };
    let y = panel.round().max(0.0) as i32;
    let width = size.width.max(1);
    let height = (size.height as i32 - y).max(1) as u32;
    Ok((y, width, height))
}
