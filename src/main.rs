#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]
mod download;
use download::{Event,Mode};
use serde::Serialize;
use std::{path::PathBuf, sync::{atomic::{AtomicBool,Ordering},mpsc,Arc,Mutex},thread};
use tauri::Manager;

#[derive(Clone,Serialize)]
struct Snapshot { status:String, progress:u32, message:String, output:String, files:Vec<String> }
struct Shared { snapshot:Mutex<Snapshot>, cancel:Arc<AtomicBool>, closing:AtomicBool }
impl Shared {
    fn new()->Self {
        let home=std::env::var_os("USERPROFILE").map(PathBuf::from).unwrap_or_else(||PathBuf::from("."));
        Self { snapshot:Mutex::new(Snapshot { status:"idle".into(),progress:0,message:"准备就绪，粘贴链接即可开始。".into(),output:home.join("Downloads").join("抖音下载").to_string_lossy().into_owned(),files:Vec::new() }),cancel:Arc::new(AtomicBool::new(false)),closing:AtomicBool::new(false) }
    }
}
#[tauri::command]
fn get_state(state:tauri::State<'_,Arc<Shared>>)->Snapshot {state.snapshot.lock().unwrap().clone()}

#[tauri::command]
fn start_download(link:String,mode:String,app:tauri::AppHandle,state:tauri::State<'_,Arc<Shared>>)->Result<(),String> {
    if link.len()>16384 {return Err("分享文字过长，请只粘贴作品链接。".into());}
    download::parse_link(&link).map_err(|e|e.to_string())?;
    let mode=match mode.as_str(){"both"=>Mode::Both,"video"=>Mode::Video,"audio"=>Mode::Audio,_=>return Err("请选择下载类型。".into())};
    let shared=Arc::clone(state.inner());
    let output;
    {let mut s=shared.snapshot.lock().unwrap();if s.status=="running" {return Err("已有任务正在处理。".into());}output=PathBuf::from(&s.output);s.status="running".into();s.progress=0;s.message="正在准备…".into();s.files.clear();shared.cancel.store(false,Ordering::Relaxed);}
    let (tx,rx)=mpsc::channel();
    let cancel=Arc::clone(&shared.cancel);
    thread::spawn(move|| {let result=download::run(&link,mode,&output,&tx,&cancel);let _=tx.send(match result{Ok(files)=>Event::Done(files),Err(e)=>Event::Failed(format!("{e:#}"))});});
    thread::spawn(move||{
        for event in rx {
            let mut s=shared.snapshot.lock().unwrap();
            match event {
                Event::Progress(n,text)=>{s.progress=n;s.message=text;},
                Event::Done(files)=>{s.status="done".into();s.progress=100;s.message="下载完成，文件已保存。".into();s.files=files.iter().map(|p|p.to_string_lossy().into_owned()).collect();},
                Event::Failed(error)=>{s.status=if shared.cancel.load(Ordering::Relaxed){"cancelled"}else{"error"}.into();s.message=error;},
            }
        }
        if shared.closing.load(Ordering::Relaxed){app.exit(0);}
    });
    Ok(())
}
#[tauri::command]
fn cancel_download(state:tauri::State<'_,Arc<Shared>>) {
    state.cancel.store(true,Ordering::Relaxed);
    let mut s=state.snapshot.lock().unwrap();if s.status=="running"{s.message="正在取消并清理临时文件…".into();}
}
#[tauri::command]
async fn choose_folder(app:tauri::AppHandle,state:tauri::State<'_,Arc<Shared>>)->Result<Option<String>,String> {
    let current={let s=state.snapshot.lock().unwrap();if s.status=="running"{return Err("请先等待当前任务完成。".into());}s.output.clone()};
    let selected=tauri::async_runtime::spawn_blocking(move||rfd::FileDialog::new().set_title("选择下载保存目录").set_directory(current).pick_folder()).await.map_err(|e|e.to_string())?;
    if let Some(path)=selected {
        let value=path.to_string_lossy().into_owned();
        {let mut s=state.snapshot.lock().unwrap();if s.status=="running"{return Err("当前任务已开始，请结束后再修改目录。".into());}s.output=value.clone();}
        if let Ok(dir)=app.path().app_config_dir(){let _=std::fs::create_dir_all(&dir);let _=std::fs::write(dir.join("settings.json"),serde_json::json!({"output":value}).to_string());}
        Ok(Some(value))
    }else{Ok(None)}
}
#[tauri::command]
fn open_folder(state:tauri::State<'_,Arc<Shared>>)->Result<(),String> {
    use std::os::windows::process::CommandExt;
    let path=state.snapshot.lock().unwrap().output.clone();
    std::fs::create_dir_all(&path).map_err(|e|format!("无法打开保存目录：{e}"))?;
    std::process::Command::new("explorer.exe").arg(path).creation_flags(0x08000000).spawn().map_err(|e|e.to_string())?;Ok(())
}

#[tauri::command]
fn read_clipboard() -> Result<String, String> {
    use windows_sys::Win32::System::{DataExchange::{OpenClipboard, CloseClipboard, GetClipboardData, IsClipboardFormatAvailable}, Memory::{GlobalLock, GlobalUnlock, GlobalSize}};
    // Clipboard access occurs only after the user clicks Paste.
    unsafe {
        if OpenClipboard(std::ptr::null_mut()) == 0 { return Err("暂时无法读取剪贴板，请使用 Ctrl+V 粘贴。".into()); }
        struct ClipboardGuard;
        impl Drop for ClipboardGuard { fn drop(&mut self) { unsafe { CloseClipboard(); } } }
        let _clipboard = ClipboardGuard;
        const UNICODE_TEXT: u32 = 13;
        if IsClipboardFormatAvailable(UNICODE_TEXT) == 0 { return Ok(String::new()); }
        let handle = GetClipboardData(UNICODE_TEXT);
        if handle.is_null() { return Err("剪贴板没有可用文字。".into()); }
        let size = GlobalSize(handle) / 2;
        let data = GlobalLock(handle) as *const u16;
        if data.is_null() { return Err("暂时无法读取剪贴板，请使用 Ctrl+V 粘贴。".into()); }
        let slice = std::slice::from_raw_parts(data, size.min(16384));
        let length = slice.iter().position(|ch| *ch == 0).unwrap_or(slice.len());
        let text = String::from_utf16_lossy(&slice[..length]);
        GlobalUnlock(handle);
        Ok(text)
    }
}
#[tauri::command]
fn open_file(index: usize, state: tauri::State<'_, Arc<Shared>>) -> Result<(), String> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::UI::Shell::ShellExecuteW;
    let path = {
        let snapshot = state.snapshot.lock().unwrap();
        if snapshot.status != "done" { return Err("请先完成下载。".into()); }
        snapshot.files.get(index).cloned().ok_or("文件不存在。")?
    };
    if !std::path::Path::new(&path).is_file() { return Err("文件已移动或删除，请打开保存文件夹查看。".into()); }
    let operation: Vec<u16> = "open\0".encode_utf16().collect();
    let target: Vec<u16> = std::ffi::OsStr::new(&path).encode_wide().chain(std::iter::once(0)).collect();
    let result = unsafe { ShellExecuteW(std::ptr::null_mut(), operation.as_ptr(), target.as_ptr(), std::ptr::null(), std::ptr::null(), 1) };
    if result as isize <= 32 { return Err("无法打开文件，请检查默认播放器，或从保存文件夹打开。".into()); }
    Ok(())
}

fn self_test(args:&[String])->i32 {
    if args.len()!=4{return 2;}
    let output=PathBuf::from(&args[3]);let _=std::fs::create_dir_all(&output);
    let (tx,rx)=mpsc::channel();let progress_path=output.join("progress.txt");
    let observer=thread::spawn(move||{for e in rx{let _=std::fs::write(&progress_path,format!("{e:?}"));}});
    let result=download::run(&args[2],Mode::Both,&output,&tx,&Arc::new(AtomicBool::new(false)));drop(tx);let _=observer.join();
    let (report,code)=match result{Ok(files)=>(serde_json::json!({"ok":true,"files":files}),0),Err(e)=>(serde_json::json!({"ok":false,"error":format!("{e:#}")}),1)};
    let _=std::fs::write(output.join("self-test.json"),serde_json::to_vec_pretty(&report).unwrap());code
}
fn main(){
    let args:Vec<String>=std::env::args().collect();
    if args.get(1).is_some_and(|s|s=="--self-test"){std::process::exit(self_test(&args));}
    tauri::Builder::default().manage(Arc::new(Shared::new()))
        .setup(|app|{if let Ok(dir)=app.path().app_config_dir(){if let Ok(bytes)=std::fs::read(dir.join("settings.json")){if let Ok(value)=serde_json::from_slice::<serde_json::Value>(&bytes){if let Some(path)=value["output"].as_str(){app.state::<Arc<Shared>>().snapshot.lock().unwrap().output=path.into();}}}}Ok(())})
        .invoke_handler(tauri::generate_handler![get_state,start_download,cancel_download,choose_folder,open_folder,read_clipboard,open_file])
        .on_window_event(|window,event|{if let tauri::WindowEvent::CloseRequested{api,..}=event{let state=window.state::<Arc<Shared>>();let mut s=state.snapshot.lock().unwrap();if s.status=="running"{api.prevent_close();state.closing.store(true,Ordering::Relaxed);state.cancel.store(true,Ordering::Relaxed);s.message="正在清理任务，稍后自动关闭…".into();}}})
        .run(tauri::generate_context!()).expect("无法启动下载工具");
}
