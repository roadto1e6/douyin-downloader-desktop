use anyhow::{anyhow, bail, Context, Result};
use regex::Regex;
use reqwest::blocking::Client;
use serde_json::{json, Value};
use std::{fs::{self, File}, io::{Read, Write}, net::TcpStream, os::windows::{io::AsRawHandle, process::CommandExt}, path::{Path, PathBuf}, process::{Child, Command, Stdio}, sync::{atomic::{AtomicBool, Ordering}, mpsc::Sender, Arc}, thread, time::{Duration, Instant}};
use tempfile::TempDir;
use tungstenite::{Message, WebSocket};
use url::Url;
use windows_sys::Win32::{Foundation::{CloseHandle, HANDLE}, System::JobObjects::*};

const NO_WINDOW: u32 = 0x08000000;
const MAX_BYTES: u64 = 1024 * 1024 * 1024;
const CONVERTER: &[u8] = include_bytes!("../assets/ffmpeg.exe.gz");

#[derive(Clone, Copy, Debug)]
pub enum Mode { Both, Video, Audio }
#[derive(Debug)]
pub enum Event { Progress(u32, String), Done(Vec<PathBuf>), Failed(String) }
fn progress(tx: &Sender<Event>, n: u32, text: impl Into<String>) { let _ = tx.send(Event::Progress(n, text.into())); }
fn check_cancel(cancel: &AtomicBool) -> Result<()> { if cancel.load(Ordering::Relaxed) { bail!("任务已取消"); } Ok(()) }

fn page_url(u: &Url) -> bool {
    u.scheme() == "https" && u.username().is_empty() && u.password().is_none() && u.port_or_known_default() == Some(443)
        && matches!(u.host_str(), Some("v.douyin.com" | "www.douyin.com" | "douyin.com" | "www.iesdouyin.com" | "iesdouyin.com"))
}
fn media_url(u: &Url) -> bool {
    u.scheme() == "https" && u.username().is_empty() && u.password().is_none() && u.port_or_known_default() == Some(443)
        && u.host_str().is_some_and(|h| h.ends_with(".douyinvod.com") || h.ends_with(".douyincdn.com"))
}
pub fn parse_link(text: &str) -> Result<Url> {
    for found in Regex::new(r#"https://[^\s<>\"]+"#)?.find_iter(text) {
        let clean = found.as_str().trim_end_matches(['，', '。', '；', '！', '）', '】', ',', ';', '!', ')', ']']);
        if let Ok(url) = Url::parse(clean) { if page_url(&url) { return Ok(url); } }
    }
    bail!("请粘贴有效的抖音视频链接，支持整段分享文字。")
}
fn client(seconds: u64) -> Result<Client> {
    Client::builder().redirect(reqwest::redirect::Policy::none()).user_agent("Mozilla/5.0").connect_timeout(Duration::from_secs(20)).timeout(Duration::from_secs(seconds)).build().context("无法初始化网络组件")
}
fn resolve_id(mut url: Url, cancel: &AtomicBool) -> Result<String> {
    let client = client(30)?;
    let pattern = Regex::new(r"^/(?:share/video|video)/(\d+)(?:/|$)")?;
    for _ in 0..8 {
        check_cancel(cancel)?;
        if !page_url(&url) { bail!("链接跳转到了不支持的网站。"); }
        if let Some(m) = pattern.captures(url.path()) { return Ok(m[1].to_owned()); }
        let response = client.get(url.clone()).send().context("无法访问分享链接，请检查网络后重试")?;
        if !response.status().is_redirection() { bail!("无法解析作品。请重新复制抖音视频分享链接。"); }
        url = url.join(response.headers().get("location").context("分享链接缺少跳转地址")?.to_str()?)?;
    }
    bail!("分享链接跳转次数过多。")
}

fn find_browser() -> Result<PathBuf> {
    let variants = [("PROGRAMFILES", "Google/Chrome/Application/chrome.exe"), ("PROGRAMFILES(X86)", "Google/Chrome/Application/chrome.exe"), ("LOCALAPPDATA", "Google/Chrome/Application/chrome.exe"), ("PROGRAMFILES(X86)", "Microsoft/Edge/Application/msedge.exe"), ("PROGRAMFILES", "Microsoft/Edge/Application/msedge.exe"), ("LOCALAPPDATA", "Microsoft/Edge/Application/msedge.exe")];
    for (key, tail) in variants { if let Some(base) = std::env::var_os(key) { let p = PathBuf::from(base).join(tail); if p.is_file() { return Ok(p); } } }
    bail!("未找到 Chrome 或 Edge。请先安装其中一个浏览器后重试。")
}
struct Browser { child: Child, job: HANDLE, _profile: TempDir }
impl Browser {
    fn launch(cancel: &AtomicBool) -> Result<(Self, String)> {
        let profile = tempfile::Builder::new().prefix("douyin-browser-").tempdir()?;
        let mut child = Command::new(find_browser()?).args(["--headless=new", "--remote-debugging-port=0", "--remote-debugging-address=127.0.0.1", "--no-first-run", "--no-default-browser-check", "--disable-background-networking", "--autoplay-policy=no-user-gesture-required", "--mute-audio", "--window-size=1280,900"])
            .arg(format!("--user-data-dir={}", profile.path().display())).arg("about:blank").stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).creation_flags(NO_WINDOW).spawn().context("无法启动浏览器")?;
        let job = unsafe {
            let job = CreateJobObjectW(std::ptr::null(), std::ptr::null());
            if job.is_null() { let _ = child.kill(); let _ = child.wait(); bail!("无法初始化浏览器进程管理"); }
            let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
            limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            if SetInformationJobObject(job, JobObjectExtendedLimitInformation, &limits as *const _ as _, std::mem::size_of_val(&limits) as u32) == 0 || AssignProcessToJobObject(job, child.as_raw_handle() as HANDLE) == 0 {
                let _ = child.kill(); let _ = child.wait(); CloseHandle(job); bail!("无法管理浏览器进程，请关闭工具后重试。");
            }
            job
        };
        let mut browser = Self { child, job, _profile: profile };
        let deadline = Instant::now() + Duration::from_secs(25);
        while Instant::now() < deadline {
            check_cancel(cancel)?;
            if let Ok(text) = fs::read_to_string(browser._profile.path().join("DevToolsActivePort")) {
                let mut lines = text.lines();
                if let (Some(port), Some(path)) = (lines.next(), lines.next()) { if let Ok(port) = port.parse::<u16>() { return Ok((browser, format!("ws://127.0.0.1:{port}{path}"))); } }
            }
            if browser.child.try_wait()?.is_some() { bail!("浏览器意外退出，请检查 Chrome 或 Edge 是否可正常运行。"); }
            thread::sleep(Duration::from_millis(100));
        }
        bail!("浏览器启动超时，请重试。")
    }
}
impl Drop for Browser {
    fn drop(&mut self) {
        unsafe { if !self.job.is_null() { CloseHandle(self.job); } }
        let _ = self.child.kill(); let _ = self.child.wait();
        for _ in 0..20 { if fs::remove_dir_all(self._profile.path()).is_ok() { break; } thread::sleep(Duration::from_millis(50)); }
    }
}
struct Cdp { socket: WebSocket<TcpStream>, next: u64 }
impl Cdp {
    fn open(endpoint: &str) -> Result<Self> {
        let u = Url::parse(endpoint)?;
        let stream = TcpStream::connect_timeout(&format!("127.0.0.1:{}", u.port().context("浏览器端口无效")?).parse()?, Duration::from_secs(5))?;
        stream.set_read_timeout(Some(Duration::from_millis(500)))?;
        stream.set_write_timeout(Some(Duration::from_secs(5)))?;
        let (socket, _) = tungstenite::client(endpoint, stream).map_err(|e| anyhow!("无法连接本地浏览器：{e}"))?;
        Ok(Self { socket, next: 0 })
    }
    fn send(&mut self, method: &str, params: Value, session: Option<&str>) -> Result<u64> {
        self.next += 1;
        let mut body = json!({"id":self.next,"method":method,"params":params});
        if let Some(session) = session { body["sessionId"] = json!(session); }
        self.socket.send(Message::Text(body.to_string().into()))?;
        Ok(self.next)
    }
    fn read(&mut self) -> Result<Option<Value>> {
        match self.socket.read() {
            Ok(Message::Text(s)) => Ok(Some(serde_json::from_str(s.as_str())?)),
            Ok(Message::Close(_)) => bail!("浏览器连接已关闭"),
            Ok(_) => Ok(None),
            Err(tungstenite::Error::Io(e)) if matches!(e.kind(), std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock) => Ok(None),
            Err(e) => Err(anyhow!("浏览器通信失败：{e}")),
        }
    }
    fn call(&mut self, method: &str, params: Value, session: Option<&str>, cancel: &AtomicBool) -> Result<Value> {
        let id = self.send(method, params, session)?;
        let deadline = Instant::now() + Duration::from_secs(15);
        while Instant::now() < deadline {
            check_cancel(cancel)?;
            if let Some(v) = self.read()? { if v["id"].as_u64() == Some(id) { if v.get("error").is_some() { bail!("浏览器执行失败：{}",v["error"]); } return Ok(v["result"].clone()); } }
        }
        bail!("浏览器响应超时，请重试。")
    }
}
fn discover(vid: &str, cancel: &AtomicBool) -> Result<(Url, String)> {
    let (_browser, endpoint) = Browser::launch(cancel)?;
    let mut cdp = Cdp::open(&endpoint)?;
    let target = cdp.call("Target.createTarget", json!({"url":"about:blank"}), None, cancel)?;
    let attached = cdp.call("Target.attachToTarget", json!({"targetId":target["targetId"],"flatten":true}), None, cancel)?;
    let session = attached["sessionId"].as_str().context("无法打开作品页面")?;
    cdp.call("Network.enable", json!({}), Some(session), cancel)?;
    // Send without awaiting Page.navigate: media events may arrive before its reply.
    cdp.send("Page.navigate", json!({"url":format!("https://www.douyin.com/video/{vid}")}), Some(session))?;
    let deadline = Instant::now() + Duration::from_secs(65);
    while Instant::now() < deadline {
        check_cancel(cancel)?;
        if let Some(event) = cdp.read()? {
            if event["sessionId"].as_str() != Some(session) || event["method"] != "Network.responseReceived" { continue; }
            let raw = event["params"]["response"]["url"].as_str().unwrap_or("");
            let Ok(url) = Url::parse(raw) else { continue; };
            if media_url(&url) && url.query_pairs().any(|(k,v)| k == "__vid" && v == vid) {
                let title = cdp.call("Runtime.evaluate", json!({"expression":"document.title","returnByValue":true}), Some(session), cancel).ok()
                    .and_then(|v|v["result"]["value"].as_str().map(str::to_owned)).unwrap_or_else(||"抖音作品".to_owned());
                return Ok((url, title.trim_end_matches(" - 抖音").to_owned()));
            }
        }
    }
    bail!("未找到播放文件。作品可能需要登录、验证或已失效。请确认链接可公开播放后重试。")
}
fn download(mut url: Url, file: &Path, tx: &Sender<Event>, cancel: &AtomicBool) -> Result<()> {
    let client = client(240)?;
    for _ in 0..5 {
        check_cancel(cancel)?;
        if !media_url(&url) { bail!("播放文件跳转到了不支持的地址。"); }
        let mut response = client.get(url.clone()).header("Referer", "https://www.douyin.com/").send().context("下载连接失败，请检查网络")?;
        if response.status().is_redirection() { url = url.join(response.headers().get("location").context("播放文件跳转地址缺失")?.to_str()?)?; continue; }
        response.error_for_status_ref().context("播放文件不可用，请重试")?;
        let total = response.content_length();
        if total.is_some_and(|n| n > MAX_BYTES) { bail!("暂不支持超过 1 GB 的视频。"); }
        let mut output = File::create(file).context("无法写入保存目录，请选择可写文件夹")?;
        let mut size = 0u64;
        let mut buf = [0u8; 128 * 1024];
        let mut last = Instant::now() - Duration::from_secs(1);
        loop {
            check_cancel(cancel)?;
            let n = response.read(&mut buf).context("下载中断，请检查网络后重试")?;
            if n == 0 { break; }
            size += n as u64;
            if size > MAX_BYTES { bail!("暂不支持超过 1 GB 的视频。"); }
            output.write_all(&buf[..n]).context("保存失败，请检查磁盘剩余空间")?;
            if last.elapsed() > Duration::from_millis(200) {
                let n = total.filter(|t|*t>0).map(|t|30 + (size.saturating_mul(45)/t).min(45) as u32).unwrap_or(45);
                progress(tx,n,format!("正在下载视频… {:.1} MB",size as f64/1048576.0)); last = Instant::now();
            }
        }
        output.sync_all()?;
        if size < 1024 || total.is_some_and(|t| t != size) { bail!("文件下载不完整，请重试。"); }
        return Ok(());
    }
    bail!("播放文件跳转次数过多。")
}
fn converter(dir: &Path) -> Result<PathBuf> {
    let exe = dir.join("ffmpeg.exe");
    let mut gzip = flate2::read::GzDecoder::new(CONVERTER);
    let mut file = File::create(&exe)?;
    std::io::copy(&mut gzip,&mut file).context("无法释放内置音频组件，请检查磁盘空间")?;
    file.sync_all()?;
    Ok(exe)
}
fn run_ffmpeg(exe: &Path, args: &[&std::ffi::OsStr], timeout: Duration, cancel: &AtomicBool, scratch: &Path) -> Result<()> {
    let log = scratch.join("conversion.log");
    let mut child = Command::new(exe).args(args).stdin(Stdio::null()).stdout(Stdio::null()).stderr(File::create(&log)?).creation_flags(NO_WINDOW).spawn().context("无法启动内置音频组件")?;
    let start = Instant::now();
    loop {
        if cancel.load(Ordering::Relaxed) || start.elapsed() > timeout {
            let _ = child.kill(); let _ = child.wait();
            check_cancel(cancel)?; bail!("音视频处理超时，请重试。");
        }
        if let Some(status) = child.try_wait()? {
            if !status.success() { let detail = fs::read_to_string(&log).unwrap_or_default(); bail!("音视频处理失败：{}",detail.chars().take(500).collect::<String>()); }
            return Ok(());
        }
        thread::sleep(Duration::from_millis(100));
    }
}
fn safe_title(title: &str) -> String {
    let text: String = title.chars().map(|c|if c.is_control() || "<>:\"/\\|?*".contains(c) { '_' } else { c }).take(42).collect();
    let text = text.trim_matches([' ', '.']);
    if text.is_empty() { "抖音作品".to_owned() } else { text.to_owned() }
}
fn save_unique(source: &Path, dir: &Path, stem: &str, extension: &str) -> Result<PathBuf> {
    for i in 0..1000 {
        let name = if i == 0 { format!("{stem}.{extension}") } else { format!("{stem} ({i}).{extension}") };
        let target = dir.join(name);
        match File::options().write(true).create_new(true).open(&target) {
            Ok(mut output) => {
                let result = (|| -> Result<()> { let mut input=File::open(source)?; std::io::copy(&mut input,&mut output)?; output.sync_all()?; Ok(()) })();
                drop(output);
                if let Err(e) = result { let _=fs::remove_file(&target); return Err(e.context("保存失败，请检查磁盘剩余空间")); }
                return Ok(target);
            },
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e).context("无法写入保存目录"),
        }
    }
    bail!("同名文件过多，请选择其他保存目录。")
}
pub fn run(text: &str, mode: Mode, output: &Path, tx: &Sender<Event>, cancel: &Arc<AtomicBool>) -> Result<Vec<PathBuf>> {
    let link = parse_link(text)?;
    fs::create_dir_all(output).context("无法创建保存目录")?;
    // Stage in the selected folder so incomplete files never appear as finished downloads.
    let staging = tempfile::Builder::new().prefix(".douyin-task-").tempdir_in(output).context("保存目录不可写")?;
    progress(tx,5,"正在解析分享链接…");
    let vid = resolve_id(link,cancel)?;
    progress(tx,15,"正在打开作品并查找播放文件…");
    let (url,title) = discover(&vid,cancel)?;
    progress(tx,30,format!("正在下载：{title}"));
    let video = staging.path().join("source.mp4");
    download(url,&video,tx,cancel)?;
    check_cancel(cancel)?;
    progress(tx,78,"正在检查视频文件…");
    let tools = tempfile::Builder::new().prefix("douyin-audio-").tempdir()?;
    let ffmpeg = converter(tools.path())?;
    run_ffmpeg(&ffmpeg,&["-v".as_ref(),"error".as_ref(),"-i".as_ref(),video.as_os_str(),"-map".as_ref(),"0:v:0".as_ref(),"-frames:v".as_ref(),"1".as_ref(),"-f".as_ref(),"null".as_ref(),"-".as_ref()],Duration::from_secs(60),cancel,tools.path())?;
    let audio = staging.path().join("audio.mp3");
    if matches!(mode,Mode::Both|Mode::Audio) {
        progress(tx,85,"正在提取 MP3 音频…");
        run_ffmpeg(&ffmpeg,&["-y".as_ref(),"-v".as_ref(),"error".as_ref(),"-i".as_ref(),video.as_os_str(),"-vn".as_ref(),"-codec:a".as_ref(),"libmp3lame".as_ref(),"-q:a".as_ref(),"2".as_ref(),audio.as_os_str()],Duration::from_secs(300),cancel,tools.path())?;
        if fs::metadata(&audio)?.len() < 1024 { bail!("未提取到有效音频，该作品可能没有音轨。"); }
    }
    check_cancel(cancel)?;
    progress(tx,98,"正在保存文件…");
    let stem = format!("抖音_{}_{vid}",safe_title(&title));
    let mut saved = Vec::new();
    let result = (|| -> Result<()> {
        if matches!(mode,Mode::Both|Mode::Video) { saved.push(save_unique(&video,output,&stem,"mp4")?); }
        if matches!(mode,Mode::Both|Mode::Audio) { saved.push(save_unique(&audio,output,&stem,"mp3")?); }
        Ok(())
    })();
    if let Err(e) = result { for path in &saved { let _ = fs::remove_file(path); } return Err(e); }
    Ok(saved)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test] fn links_and_hosts() {
        assert_eq!(parse_link("复制打开抖音 https://v.douyin.com/abc/ 08/29").unwrap().as_str(),"https://v.douyin.com/abc/");
        for bad in ["https://v.douyin.com.evil.test/a", "https://evil.test/https://v.douyin.com/a", "https://user@v.douyin.com/a", "https://v.douyin.com:444/a", "http://v.douyin.com/a"] { assert!(parse_link(bad).is_err(),"{bad}"); }
        assert!(!media_url(&Url::parse("https://douyinvod.com.evil.test/a").unwrap()));
    }
    #[test] fn filenames_do_not_overwrite() {
        let dir=tempfile::tempdir().unwrap(); let source=dir.path().join("source");fs::write(&source,b"sample").unwrap();
        let a=save_unique(&source,dir.path(),"测试 #标题","mp4").unwrap();let b=save_unique(&source,dir.path(),"测试 #标题","mp4").unwrap();assert_ne!(a,b);assert_eq!(fs::read(a).unwrap(),b"sample");
        assert_eq!(safe_title("a/b:c?"),"a_b_c_");
    }
    #[test] fn cancellation_before_network() {
        let (tx,_)=std::sync::mpsc::channel();let cancel=Arc::new(AtomicBool::new(true));let dir=tempfile::tempdir().unwrap();
        assert!(run("https://www.douyin.com/video/123",Mode::Both,dir.path(),&tx,&cancel).unwrap_err().to_string().contains("取消"));
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(),0);
    }
}
