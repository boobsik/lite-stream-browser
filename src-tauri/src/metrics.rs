use serde::Serialize;

/// Live consumption of the whole application process tree (the Tauri host plus
/// every msedgewebview2.exe child). Returned to the home screen.
#[derive(Clone, Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RuntimeStats {
    pub ram_mb: u64,
    pub cpu_percent: f32,
    pub processes: u32,
    pub sampled_at: u64,
}

#[cfg(windows)]
mod platform {
    use super::RuntimeStats;
    use std::collections::HashSet;
    use std::sync::{Mutex, OnceLock};
    use std::time::{Instant, SystemTime, UNIX_EPOCH};
    use windows::Win32::Foundation::{CloseHandle, FILETIME};
    use windows::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W,
        TH32CS_SNAPPROCESS,
    };
    use windows::Win32::System::ProcessStatus::{GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS};
    use windows::Win32::System::SystemInformation::{GetSystemInfo, SYSTEM_INFO};
    use windows::Win32::System::Threading::{
        GetCurrentProcess, GetCurrentProcessId, GetProcessTimes, OpenProcess,
        PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_VM_READ,
    };

    static PREVIOUS_CPU: Mutex<Option<(Instant, u64)>> = Mutex::new(None);
    static LOGICAL_CORES: OnceLock<f64> = OnceLock::new();

    struct ProcessRow {
        pid: u32,
        parent: u32,
    }

    fn filetime_ticks(value: &FILETIME) -> u64 {
        ((value.dwHighDateTime as u64) << 32) | value.dwLowDateTime as u64
    }

    fn snapshot_processes() -> Vec<ProcessRow> {
        let mut rows = Vec::new();
        // SAFETY: the snapshot handle is checked before every call and closed
        // before returning. PROCESSENTRY32W is initialised with its real size.
        unsafe {
            let Ok(snapshot) = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) else {
                return rows;
            };

            let mut entry = PROCESSENTRY32W {
                dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32,
                ..Default::default()
            };

            if Process32FirstW(snapshot, &mut entry).is_ok() {
                loop {
                    rows.push(ProcessRow {
                        pid: entry.th32ProcessID,
                        parent: entry.th32ParentProcessID,
                    });
                    if Process32NextW(snapshot, &mut entry).is_err() {
                        break;
                    }
                }
            }

            let _ = CloseHandle(snapshot);
        }
        rows
    }

    fn process_tree(root: u32) -> Vec<u32> {
        let rows = snapshot_processes();
        let mut tree = HashSet::new();
        tree.insert(root);

        // WebView2 browser/renderer/GPU processes are descendants of the Tauri
        // host. The loop walks the parent links transitively.
        loop {
            let mut changed = false;
            for row in &rows {
                if row.pid != 0 && tree.contains(&row.parent) && tree.insert(row.pid) {
                    changed = true;
                }
            }
            if !changed {
                break;
            }
        }

        tree.into_iter().collect()
    }

    fn collect_usage(root: u32) -> (u64, u64, u32) {
        let mut ram_bytes = 0u64;
        let mut cpu_ticks = 0u64;
        let mut count = 0u32;

        for pid in process_tree(root) {
            // SAFETY: handles are closed after use; pseudo-handle for the
            // current process must not be closed.
            unsafe {
                let (handle, owned) = if pid == root {
                    (GetCurrentProcess(), false)
                } else {
                    match OpenProcess(
                        PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_VM_READ,
                        false,
                        pid,
                    ) {
                        Ok(handle) => (handle, true),
                        Err(_) => continue,
                    }
                };

                let mut counters = PROCESS_MEMORY_COUNTERS {
                    cb: std::mem::size_of::<PROCESS_MEMORY_COUNTERS>() as u32,
                    ..Default::default()
                };
                if GetProcessMemoryInfo(handle, &mut counters, counters.cb).is_ok() {
                    ram_bytes = ram_bytes.saturating_add(counters.WorkingSetSize as u64);
                    count += 1;
                }

                let (mut created, mut exited, mut kernel, mut user) = (
                    FILETIME::default(),
                    FILETIME::default(),
                    FILETIME::default(),
                    FILETIME::default(),
                );
                if GetProcessTimes(handle, &mut created, &mut exited, &mut kernel, &mut user).is_ok()
                {
                    cpu_ticks =
                        cpu_ticks.saturating_add(filetime_ticks(&kernel) + filetime_ticks(&user));
                }

                if owned {
                    let _ = CloseHandle(handle);
                }
            }
        }

        (ram_bytes, cpu_ticks, count)
    }

    fn logical_cores() -> f64 {
        *LOGICAL_CORES.get_or_init(|| {
            // SAFETY: GetSystemInfo fills a plain out-parameter.
            let mut info = SYSTEM_INFO::default();
            unsafe { GetSystemInfo(&mut info) };
            f64::from(info.dwNumberOfProcessors.max(1))
        })
    }

    pub fn sample() -> RuntimeStats {
        let now = Instant::now();
        // SAFETY: GetCurrentProcessId has no preconditions.
        let root = unsafe { GetCurrentProcessId() };
        let (ram_bytes, cpu_ticks, processes) = collect_usage(root);

        let cpu_percent = {
            let mut previous = PREVIOUS_CPU
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());

            let value = match *previous {
                Some((at, ticks)) => {
                    let elapsed = now.duration_since(at).as_secs_f64().max(0.001);
                    let delta_cpu = cpu_ticks.saturating_sub(ticks) as f64 * 1e-7;
                    // Normalise by the number of logical cores so the number
                    // matches the 0..100 scale of Task Manager.
                    (delta_cpu / elapsed / logical_cores() * 100.0) as f32
                }
                None => 0.0,
            };

            *previous = Some((now, cpu_ticks));
            value.clamp(0.0, 100.0)
        };

        RuntimeStats {
            ram_mb: ram_bytes / (1024 * 1024),
            cpu_percent,
            processes,
            sampled_at: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|value| value.as_millis() as u64)
                .unwrap_or(0),
        }
    }
}

#[cfg(not(windows))]
mod platform {
    use super::RuntimeStats;

    pub fn sample() -> RuntimeStats {
        RuntimeStats::default()
    }
}

pub fn sample() -> RuntimeStats {
    platform::sample()
}
