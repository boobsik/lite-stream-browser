//! Chrome Native Messaging bridge for WebView2 extension pages.
//!
//! WebView2 does not implement `chrome.runtime.connectNative` for unpacked
//! extensions, so Chatterino+ cannot talk to its native host. This module
//! resolves the host from the Windows registry (the same places Chrome, Edge
//! and Firefox use), starts the host process with piped stdio and relays the
//! length-prefixed JSON frames to/from the webview through Tauri events.

use serde_json::{json, Value};
use std::{
    collections::HashMap,
    io::{Read, Write},
    path::{Path, PathBuf},
    process::{Child, ChildStdin, Command, Stdio},
    sync::{Mutex, OnceLock},
};
use tauri::{AppHandle, Emitter, Webview, Wry};

#[cfg(windows)]
use windows::Win32::System::Registry::{
    RegCloseKey, RegOpenKeyExW, RegQueryValueExW, HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE, HKEY,
    KEY_READ, REG_VALUE_TYPE, REG_SZ,
};

/// Chrome's protocol allows 1 MB host->browser frames and 32 MB the other way;
/// 16 MB is a safe middle ground and still prevents a malformed header from
/// allocating gigabytes.
const MAX_MESSAGE_SIZE: usize = 16 * 1024 * 1024;

struct NativePort {
    child: Child,
    stdin: ChildStdin,
    /// WebView that owns the port, so hosts are stopped when a tab sleeps.
    label: String,
}

static PORTS: OnceLock<Mutex<HashMap<String, NativePort>>> = OnceLock::new();

fn ports() -> &'static Mutex<HashMap<String, NativePort>> {
    PORTS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn lock_ports() -> std::sync::MutexGuard<'static, HashMap<String, NativePort>> {
    ports().lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Port ids become part of a Tauri event name, and host names become part of a
/// registry key. Never accept separators or wildcards from JavaScript.
fn is_safe_token(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-' || byte == b'.')
}

fn strip_quotes(value: &str) -> &str {
    let value = value.trim();
    if value.len() >= 2
        && ((value.starts_with('"') && value.ends_with('"'))
            || (value.starts_with('\'') && value.ends_with('\'')))
    {
        value[1..value.len() - 1].trim()
    } else {
        value
    }
}

/// Reads the default value of one of the NativeMessagingHosts registry keys.
/// The value points to the host's JSON manifest, not directly to the exe.
#[cfg(windows)]
fn read_registry_default(root: HKEY, subkey: &str) -> Option<String> {
    use windows_core::HSTRING;

    let subkey = HSTRING::from(subkey);
    let empty_name = HSTRING::from("");
    let mut key = HKEY::default();

    // SAFETY: `key` is only used when the call succeeds and is closed below.
    let status = unsafe { RegOpenKeyExW(root, &subkey, None, KEY_READ, &mut key) };
    if status.0 != 0 || key.is_invalid() {
        return None;
    }

    let mut value_type = REG_VALUE_TYPE(0);
    let mut byte_len = 0u32;
    // SAFETY: querying with a null buffer only asks for the required size.
    let status = unsafe {
        RegQueryValueExW(
            key,
            &empty_name,
            None,
            Some(&mut value_type),
            None,
            Some(&mut byte_len),
        )
    };
    if status.0 != 0 || byte_len == 0 || value_type != REG_SZ {
        let _ = unsafe { RegCloseKey(key) };
        return None;
    }

    let mut buffer = vec![0u16; byte_len as usize / 2 + 1];
    let mut query_len = byte_len;
    // SAFETY: buffer is sized from the first query, and query_len is updated.
    let status = unsafe {
        RegQueryValueExW(
            key,
            &empty_name,
            None,
            Some(&mut value_type),
            Some(buffer.as_mut_ptr() as *mut u8),
            Some(&mut query_len),
        )
    };
    let _ = unsafe { RegCloseKey(key) };
    if status.0 != 0 {
        return None;
    }

    let units = (query_len as usize / 2).min(buffer.len());
    let text = String::from_utf16_lossy(&buffer[..units])
        .trim_matches('\0')
        .trim()
        .to_string();
    if text.is_empty() {
        None
    } else {
        Some(text)
    }
}

/// Resolves the native host executable path from Chrome/Edge/Firefox registry
/// entries and the `path` field of the host manifest.
#[cfg(windows)]
fn host_executable(host_name: &str) -> Result<PathBuf, String> {
    let roots: [(HKEY, String); 4] = [
        (
            HKEY_CURRENT_USER,
            format!(r"SOFTWARE\Google\Chrome\NativeMessagingHosts\{host_name}"),
        ),
        (
            HKEY_LOCAL_MACHINE,
            format!(r"SOFTWARE\Google\Chrome\NativeMessagingHosts\{host_name}"),
        ),
        (
            HKEY_CURRENT_USER,
            format!(r"SOFTWARE\Microsoft\Edge\NativeMessagingHosts\{host_name}"),
        ),
        (
            HKEY_CURRENT_USER,
            format!(r"SOFTWARE\Mozilla\NativeMessagingHosts\{host_name}"),
        ),
    ];

    let mut manifest_value = None;
    for (root, subkey) in roots {
        if let Some(value) = read_registry_default(root, &subkey) {
            manifest_value = Some(value);
            break;
        }
    }
    let manifest_value =
        manifest_value.ok_or_else(|| format!("Хост Native Messaging `{host_name}` не зарегистрирован"))?;

    let manifest = PathBuf::from(strip_quotes(&manifest_value));
    let contents = std::fs::read_to_string(&manifest)
        .map_err(|error| format!("Не удалось прочитать манифест хоста: {error}"))?;
    let manifest_json: Value = serde_json::from_str(&contents)
        .map_err(|error| format!("Некорректный манифест хоста: {error}"))?;
    let raw_path = manifest_json
        .get("path")
        .and_then(Value::as_str)
        .map(strip_quotes)
        .ok_or_else(|| "В манифесте хоста нет поля `path`".to_string())?;
    if raw_path.is_empty() {
        return Err("Поле `path` манифеста хоста пусто".into());
    }

    let raw_path = Path::new(raw_path);
    let executable = if raw_path.is_absolute() {
        raw_path.to_path_buf()
    } else {
        manifest
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .join(raw_path)
    };
    if !executable.is_file() {
        return Err(format!("Исполняемый файл хоста не найден: {}", executable.display()));
    }
    Ok(executable)
}

#[cfg(not(windows))]
fn host_executable(_host_name: &str) -> Result<PathBuf, String> {
    Err("Native Messaging доступен только на Windows".into())
}

fn event_name(port_id: &str) -> String {
    format!("native-message:{port_id}")
}

fn emit(app: &AppHandle, label: &str, port_id: &str, payload: Value) {
    if let Err(error) = app.emit_to(label, &event_name(port_id), payload) {
        eprintln!("[lite-stream] native messaging emit failed: {error}");
    }
}

fn terminate(mut port: NativePort) {
    let _ = port.stdin.flush();
    // Closing stdin is the graceful Chrome protocol shutdown; kill/ wait only
    // guarantees cleanup when the host ignores it.
    drop(port.stdin);
    let _ = port.child.kill();
    let _ = port.child.wait();
}

/// Reads framed JSON from the host and forwards it to the owning webview.
fn pump_stdout(app: AppHandle, label: String, port_id: String, mut stdout: std::process::ChildStdout) {
    loop {
        let mut header = [0u8; 4];
        if let Err(error) = stdout.read_exact(&mut header) {
            if error.kind() != std::io::ErrorKind::UnexpectedEof {
                eprintln!("[lite-stream] native host stdout error: {error}");
            }
            break;
        }

        let length = u32::from_le_bytes(header) as usize;
        if length == 0 || length > MAX_MESSAGE_SIZE {
            emit(
                &app,
                &label,
                &port_id,
                json!({ "type": "error", "error": format!("Некорректная длина сообщения: {length}") }),
            );
            break;
        }

        let mut payload = vec![0u8; length];
        if stdout.read_exact(&mut payload).is_err() {
            break;
        }
        match serde_json::from_slice::<Value>(&payload) {
            Ok(message) => emit(
                &app,
                &label,
                &port_id,
                json!({ "type": "message", "message": message }),
            ),
            Err(error) => emit(
                &app,
                &label,
                &port_id,
                json!({ "type": "error", "error": format!("Некорректный JSON от хоста: {error}") }),
            ),
        }
    }

    emit(&app, &label, &port_id, json!({ "type": "disconnected" }));
    let stale = lock_ports().remove(&port_id);
    if let Some(port) = stale {
        terminate(port);
    }
}

/// Starts a native host and binds it to the calling webview/port id.
#[tauri::command]
pub async fn native_connect(
    port_id: String,
    host_name: String,
    origin: Option<String>,
    app: AppHandle,
    webview: Webview<Wry>,
) -> Result<(), String> {
    let port_id = port_id.trim();
    let host_name = host_name.trim();
    if !is_safe_token(port_id) {
        return Err("Некорректный id порта Native Messaging".into());
    }
    if !is_safe_token(host_name) {
        return Err("Некорректное имя native host".into());
    }
    if lock_ports().contains_key(port_id) {
        return Err(format!("Порт {port_id} уже подключён"));
    }

    let executable = host_executable(host_name)?;
    let mut command = Command::new(&executable);
    command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    // Chrome always passes the parent window and the calling extension origin.
    // Some hosts validate the origin argument, so keep the same argument shape.
    command.arg("--parent-window=0");
    if let Some(origin) = origin.filter(|value| !value.is_empty()) {
        command.arg(origin);
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        command.creation_flags(CREATE_NO_WINDOW);
    }
    let mut child = command
        .spawn()
        .map_err(|error| format!("Не удалось запустить native host: {error}"))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| "У native host нет stdout".to_string())?;
    let stdin = child
        .stdin
        .take()
        .ok_or_else(|| "У native host нет stdin".to_string())?;

    let label = webview.label().to_string();
    {
        let mut ports = lock_ports();
        if ports.contains_key(port_id) {
            drop(stdin);
            let _ = child.kill();
            let _ = child.wait();
            return Err(format!("Порт {port_id} уже подключён"));
        }
        ports.insert(
            port_id.to_string(),
            NativePort {
                child,
                stdin,
                label: label.clone(),
            },
        );
    }

    // Announce the connection before starting the reader so a host that exits
    // immediately cannot emit `disconnected` before `connected`.
    emit(&app, &label, port_id, json!({ "type": "connected" }));

    let reader_app = app.clone();
    let reader_label = label.clone();
    let reader_port = port_id.to_string();
    if let Err(error) = std::thread::Builder::new()
        .name("native-messaging-reader".into())
        .spawn(move || pump_stdout(reader_app, reader_label, reader_port, stdout))
    {
        if let Some(port) = lock_ports().remove(port_id) {
            terminate(port);
        }
        return Err(format!("Не удалось запустить поток чтения хоста: {error}"));
    }

    Ok(())
}

/// Writes one length-prefixed JSON frame to the host stdin.
#[tauri::command]
pub async fn native_post_message(port_id: String, payload_json: String) -> Result<(), String> {
    if !is_safe_token(&port_id) {
        return Err("Некорректный id порта Native Messaging".into());
    }
    if payload_json.is_empty() || payload_json.len() > MAX_MESSAGE_SIZE {
        return Err("Некорректный размер сообщения Native Messaging".into());
    }
    // Reject non-JSON early instead of desynchronising the host's stdio.
    serde_json::from_str::<Value>(&payload_json)
        .map_err(|error| format!("Некорректный JSON: {error}"))?;

    let mut ports = lock_ports();
    let port = ports
        .get_mut(&port_id)
        .ok_or_else(|| format!("Порт {port_id} не подключён"))?;
    let header = (payload_json.len() as u32).to_le_bytes();
    port.stdin
        .write_all(&header)
        .and_then(|_| port.stdin.write_all(payload_json.as_bytes()))
        .and_then(|_| port.stdin.flush())
        .map_err(|error| format!("Не удалось отправить сообщение хосту: {error}"))
}

/// Stops the host process for a port. Safe to call twice.
#[tauri::command]
pub async fn native_disconnect(port_id: String) -> Result<(), String> {
    if !is_safe_token(&port_id) {
        return Err("Некорректный id порта Native Messaging".into());
    }
    let port = lock_ports().remove(&port_id);
    if let Some(port) = port {
        terminate(port);
    }
    Ok(())
}

/// Stops every host started by one WebView (tab sleep, tab close, popup close).
pub fn disconnect_webview(label: &str) {
    let stale: Vec<NativePort> = {
        let mut ports = lock_ports();
        let labels: Vec<String> = ports
            .iter()
            .filter(|(_, port)| port.label == label)
            .map(|(id, _)| id.clone())
            .collect();
        labels
            .into_iter()
            .filter_map(|id| ports.remove(&id))
            .collect()
    };
    for port in stale {
        terminate(port);
    }
}

/// Called when the app window is destroyed so no native host is left running.
pub fn disconnect_all() {
    let remaining: Vec<NativePort> = lock_ports().drain().map(|(_, port)| port).collect();
    for port in remaining {
        terminate(port);
    }
}
