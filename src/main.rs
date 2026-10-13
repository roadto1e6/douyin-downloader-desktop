#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]
mod download;
mod queue;
use queue::{Added, Parsed, Shared, Snapshot};
use std::{
    path::{Path, PathBuf},
    sync::Arc,
};
use tauri::Manager;

#[tauri::command]
fn get_state(state: tauri::State<'_, Arc<Shared>>) -> Snapshot {
    state.snapshot()
}
#[tauri::command]
fn preview_batch(text: String) -> Result<Parsed, String> {
    queue::parse_batch(&text)
}
#[tauri::command]
fn enqueue(
    text: String,
    mode: String,
    state: tauri::State<'_, Arc<Shared>>,
) -> Result<Added, String> {
    state.add(&text, &mode)
}
#[tauri::command]
fn pause_queue(paused: bool, state: tauri::State<'_, Arc<Shared>>) -> Result<(), String> {
    state.pause(paused)
}
#[tauri::command]
fn cancel_task(id: Option<u64>, state: tauri::State<'_, Arc<Shared>>) -> Result<(), String> {
    state.cancel(id)
}
#[tauri::command]
fn retry_tasks(id: Option<u64>, state: tauri::State<'_, Arc<Shared>>) -> Result<usize, String> {
    state.retry(id)
}
#[tauri::command]
fn clear_history(state: tauri::State<'_, Arc<Shared>>) -> Result<(), String> {
    state.clear_history()
}
#[tauri::command]
async fn choose_folder(state: tauri::State<'_, Arc<Shared>>) -> Result<Option<String>, String> {
    let current = state.snapshot().output;
    let chosen = tauri::async_runtime::spawn_blocking(move || {
        rfd::FileDialog::new()
            .set_title("为新任务选择保存目录")
            .set_directory(current)
            .pick_folder()
    })
    .await
    .map_err(|e| e.to_string())?;
    if let Some(path) = chosen {
        let value = path.to_string_lossy().into_owned();
        state.set_output(value.clone())?;
        Ok(Some(value))
    } else {
        Ok(None)
    }
}
fn decode_text(bytes: &[u8]) -> Result<String, String> {
    if bytes.len() > 1024 * 1024 {
        return Err("文本文件超过 1 MB，请拆分后导入。".into());
    }
    if bytes.starts_with(&[0xff, 0xfe]) || bytes.starts_with(&[0xfe, 0xff]) {
        if (bytes.len() - 2) % 2 != 0 {
            return Err("文本文件编码不完整。".into());
        }
        let little = bytes[0] == 0xff;
        let units: Vec<u16> = bytes[2..]
            .chunks_exact(2)
            .map(|b| {
                if little {
                    u16::from_le_bytes([b[0], b[1]])
                } else {
                    u16::from_be_bytes([b[0], b[1]])
                }
            })
            .collect();
        String::from_utf16(&units).map_err(|_| "请将文件另存为 UTF-8 后导入。".into())
    } else {
        String::from_utf8(
            bytes
                .strip_prefix(&[0xef, 0xbb, 0xbf])
                .unwrap_or(bytes)
                .to_vec(),
        )
        .map_err(|_| "请将文件另存为 UTF-8 后导入。".into())
    }
}
#[tauri::command]
async fn import_text() -> Result<Option<String>, String> {
    tauri::async_runtime::spawn_blocking(|| {
        let Some(path) = rfd::FileDialog::new()
            .set_title("导入链接文本")
            .add_filter("文本文件", &["txt"])
            .pick_file()
        else {
            return Ok(None);
        };
        let size = std::fs::metadata(&path).map_err(|e| e.to_string())?.len();
        if size > 1024 * 1024 {
            return Err("文本文件超过 1 MB，请拆分后导入。".into());
        }
        let text = decode_text(&std::fs::read(path).map_err(|e| e.to_string())?)?;
        if text.len() > 262144 {
            return Err("文字过长，请分批导入。".into());
        }
        Ok(Some(text))
    })
    .await
    .map_err(|e| e.to_string())?
}
#[tauri::command]
async fn export_failed(state: tauri::State<'_, Arc<Shared>>) -> Result<bool, String> {
    let text = state.failed_links();
    if text.is_empty() {
        return Err("目前没有失败任务。".into());
    }
    tauri::async_runtime::spawn_blocking(move || {
        let Some(path) = rfd::FileDialog::new()
            .set_title("导出失败链接")
            .set_file_name("失败链接.txt")
            .add_filter("文本文件", &["txt"])
            .save_file()
        else {
            return Ok(false);
        };
        std::fs::write(path, text).map_err(|e| e.to_string())?;
        Ok(true)
    })
    .await
    .map_err(|e| e.to_string())?
}
fn shell_open(path: &Path) -> Result<(), String> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::UI::Shell::ShellExecuteW;
    let verb: Vec<u16> = "open\0".encode_utf16().collect();
    let target: Vec<u16> = path
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    let result = unsafe {
        ShellExecuteW(
            std::ptr::null_mut(),
            verb.as_ptr(),
            target.as_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            1,
        )
    };
    if result as isize <= 32 {
        Err("无法打开，请检查文件是否存在及默认打开程序。".into())
    } else {
        Ok(())
    }
}
#[tauri::command]
fn open_folder(id: Option<u64>, state: tauri::State<'_, Arc<Shared>>) -> Result<(), String> {
    let s = state.snapshot();
    let path = if let Some(id) = id {
        s.tasks
            .iter()
            .find(|t| t.id == id)
            .map(|t| t.output.clone())
            .ok_or("任务不存在")?
    } else {
        s.output
    };
    std::fs::create_dir_all(&path).map_err(|e| e.to_string())?;
    shell_open(Path::new(&path))
}
#[tauri::command]
fn open_file(id: u64, index: usize, state: tauri::State<'_, Arc<Shared>>) -> Result<(), String> {
    let s = state.snapshot();
    let task = s.tasks.iter().find(|t| t.id == id).ok_or("任务不存在")?;
    let file = task.files.get(index).ok_or("文件不存在")?;
    if !Path::new(file).is_file() {
        return Err("文件已移动或删除，可以从保存目录查找。".into());
    }
    shell_open(Path::new(file))
}

struct Instance(windows_sys::Win32::Foundation::HANDLE);
impl Drop for Instance {
    fn drop(&mut self) {
        unsafe {
            windows_sys::Win32::Foundation::CloseHandle(self.0);
        }
    }
}
fn instance(key: &str) -> Option<Instance> {
    use std::hash::{Hash, Hasher};
    use windows_sys::Win32::{
        Foundation::{CloseHandle, GetLastError, ERROR_ALREADY_EXISTS},
        System::Threading::CreateMutexW,
        UI::WindowsAndMessaging::{MessageBoxW, MB_ICONINFORMATION, MB_OK},
    };
    let mut hash = std::collections::hash_map::DefaultHasher::new();
    key.to_lowercase().hash(&mut hash);
    let name: Vec<u16> = format!("Local\\DouyinDownloaderDesktop-{}\0", hash.finish())
        .encode_utf16()
        .collect();
    unsafe {
        let handle = CreateMutexW(std::ptr::null(), 0, name.as_ptr());
        if handle.is_null() {
            return None;
        }
        if GetLastError() == ERROR_ALREADY_EXISTS {
            CloseHandle(handle);
            let message: Vec<u16> = "下载工具已经在运行，请切换到现有窗口。\0"
                .encode_utf16()
                .collect();
            let title: Vec<u16> = "抖音下载\0".encode_utf16().collect();
            MessageBoxW(
                std::ptr::null_mut(),
                message.as_ptr(),
                title.as_ptr(),
                MB_OK | MB_ICONINFORMATION,
            );
            None
        } else {
            Some(Instance(handle))
        }
    }
}
fn main() {
    let args: Vec<String> = std::env::args().collect();
    let override_dir = args
        .windows(2)
        .find(|a| a[0] == "--data-dir")
        .map(|a| PathBuf::from(&a[1]));
    let key = override_dir
        .as_ref()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|| "default".into());
    let Some(_instance) = instance(&key) else {
        return;
    };
    tauri::Builder::default()
        .setup(move |app| {
            let dir = if let Some(dir) = override_dir.clone() {
                dir
            } else {
                app.path().app_config_dir()?
            };
            let shared = Arc::new(Shared::load(dir.join("queue-v2.json")));
            let handle = app.handle().clone();
            shared.start(move || {
                if let Some(window) = handle.get_webview_window("main") {
                    let _ = window.destroy();
                }
            });
            app.manage(shared);
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            get_state,
            preview_batch,
            enqueue,
            pause_queue,
            cancel_task,
            retry_tasks,
            clear_history,
            choose_folder,
            import_text,
            export_failed,
            open_folder,
            open_file,
            read_clipboard
        ])
        .on_window_event(|window, event| {
            if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                // Let the worker own exit after persistence and browser cleanup.
                // Destroy the window only when the worker has finished cleanup.
                api.prevent_close();
                window.state::<Arc<Shared>>().shutdown();
            }
        })
        .run(tauri::generate_context!())
        .expect("无法启动下载工具");
}
#[cfg(test)]
mod text_tests {
    use super::*;
    #[test]
    fn accepts_boms_and_rejects_invalid() {
        assert_eq!(decode_text(b"\xef\xbb\xbfhello").unwrap(), "hello");
        assert_eq!(decode_text(&[0xff, 0xfe, 0x2d, 0x4e]).unwrap(), "中");
        assert!(decode_text(&[0xff, 0xfe, 0]).is_err());
        assert!(decode_text(&[0xff, 0xab]).is_err());
    }
}
#[tauri::command]
fn read_clipboard() -> Result<String, String> {
    use windows_sys::Win32::System::{
        DataExchange::{
            CloseClipboard, GetClipboardData, IsClipboardFormatAvailable, OpenClipboard,
        },
        Memory::{GlobalLock, GlobalSize, GlobalUnlock},
    };
    // Clipboard access occurs only after the user clicks Paste.
    unsafe {
        if OpenClipboard(std::ptr::null_mut()) == 0 {
            return Err("暂时无法读取剪贴板，请使用 Ctrl+V 粘贴。".into());
        }
        struct ClipboardGuard;
        impl Drop for ClipboardGuard {
            fn drop(&mut self) {
                unsafe {
                    CloseClipboard();
                }
            }
        }
        let _clipboard = ClipboardGuard;
        const UNICODE_TEXT: u32 = 13;
        if IsClipboardFormatAvailable(UNICODE_TEXT) == 0 {
            return Ok(String::new());
        }
        let handle = GetClipboardData(UNICODE_TEXT);
        if handle.is_null() {
            return Err("剪贴板没有可用文字。".into());
        }
        let size = GlobalSize(handle) / 2;
        let data = GlobalLock(handle) as *const u16;
        if data.is_null() {
            return Err("暂时无法读取剪贴板，请使用 Ctrl+V 粘贴。".into());
        }
        let slice = std::slice::from_raw_parts(data, size.min(65536));
        let length = slice.iter().position(|ch| *ch == 0).unwrap_or(slice.len());
        let text = String::from_utf16_lossy(&slice[..length]);
        GlobalUnlock(handle);
        Ok(text)
    }
}
