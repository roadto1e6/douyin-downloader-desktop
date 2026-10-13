use crate::download::{self, Event, Mode};
use serde::{Deserialize, Serialize};
use std::{
    collections::HashSet,
    fs,
    io::Write,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc, Arc, Condvar, Mutex,
    },
    thread,
    time::{SystemTime, UNIX_EPOCH},
};

#[derive(Clone, Serialize, Deserialize, Debug)]
pub struct Task {
    pub id: u64,
    pub link: String,
    pub mode: String,
    pub output: String,
    pub status: String,
    pub progress: u32,
    pub message: String,
    pub title: String,
    pub video_id: String,
    pub files: Vec<String>,
    pub created_at: u64,
    pub attempts: u32,
}
impl Task {
    fn terminal(&self) -> bool {
        !matches!(self.status.as_str(), "queued" | "running")
    }
    fn reusable(&self) -> bool {
        matches!(self.status.as_str(), "done" | "skipped")
            && !self.files.is_empty()
            && self.files.iter().all(|p| Path::new(p).is_file())
    }
}
#[derive(Clone, Serialize, Deserialize)]
pub struct Snapshot {
    pub tasks: Vec<Task>,
    pub paused: bool,
    pub output: String,
    pub mode: String,
    pub next_id: u64,
    #[serde(default)]
    pub warning: String,
}
impl Default for Snapshot {
    fn default() -> Self {
        let home = std::env::var_os("USERPROFILE")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("."));
        Self {
            tasks: vec![],
            paused: false,
            output: home
                .join("Downloads/抖音下载")
                .to_string_lossy()
                .into_owned(),
            mode: "both".into(),
            next_id: 1,
            warning: String::new(),
        }
    }
}
#[derive(Serialize)]
pub struct Added {
    pub added: usize,
    pub duplicates: usize,
    pub ignored: usize,
    pub ids: Vec<u64>,
}
#[derive(Serialize)]
pub struct Parsed {
    pub links: Vec<String>,
    pub duplicates: usize,
    pub ignored: usize,
}
pub fn parse_batch(text: &str) -> Result<Parsed, String> {
    if text.len() > 262144 {
        return Err("分享文字过长，请分批添加。".into());
    }
    let pattern = regex::Regex::new(r#"https?://[^\s<>\"，。；！）】]+"#).unwrap();
    let mut links = vec![];
    let mut seen = HashSet::new();
    let mut duplicates = 0;
    let mut ignored = 0;
    for m in pattern.find_iter(text) {
        match download::parse_link(m.as_str()) {
            Ok(mut url) => {
                let direct = regex::Regex::new(r"^/(?:share/video|video)/(\d+)(?:/|$)").unwrap();
                let canonical = if let Some(c) = direct.captures(url.path()) {
                    format!("https://www.douyin.com/video/{}", &c[1])
                } else {
                    url.set_query(None);
                    url.set_fragment(None);
                    url.to_string().trim_end_matches('/').to_owned() + "/"
                };
                if seen.insert(canonical.clone()) {
                    links.push(canonical)
                } else {
                    duplicates += 1;
                }
            }
            Err(_) => ignored += 1,
        }
    }
    if links.len() > 100 {
        return Err("每批最多添加 100 个不同链接，请分批导入。".into());
    }
    Ok(Parsed {
        links,
        duplicates,
        ignored,
    })
}
fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}
pub struct Engine {
    pub snapshot: Snapshot,
    pub cancel: Option<Arc<AtomicBool>>,
    pub closing: bool,
}
pub struct Shared {
    pub inner: Mutex<Engine>,
    pub wake: Condvar,
    pub path: PathBuf,
}
impl Shared {
    pub fn load(path: PathBuf) -> Self {
        let mut s = Snapshot::default();
        if let Ok(bytes) = fs::read(&path) {
            match serde_json::from_slice::<Snapshot>(&bytes) {
                Ok(saved) => {
                    s = saved;
                    let mut pending = false;
                    for task in &mut s.tasks {
                        if task.status == "running" {
                            task.status = "queued".into();
                            task.progress = 0;
                            task.message = "上次中断，等待重新下载".into();
                        }
                        pending |= task.status == "queued";
                    }
                    if pending {
                        s.paused = true;
                        s.warning =
                            "已恢复未完成任务。点击“继续队列”后重新下载；已完成文件不会重复下载。"
                                .into();
                    }
                    s.next_id = s.next_id.max(
                        s.tasks
                            .iter()
                            .map(|t| t.id)
                            .max()
                            .unwrap_or(0)
                            .saturating_add(1),
                    );
                }
                Err(_) => {
                    let backup = path.with_extension(format!("corrupt-{}.json", now()));
                    s.warning = if fs::rename(&path, &backup).is_ok() {
                        "历史记录损坏，已备份原文件。本次从空队列开始。"
                    } else {
                        "历史记录无法读取，请检查应用数据目录。"
                    }
                    .into();
                }
            }
        } else if let Some(parent) = path.parent() {
            if let Ok(bytes) = fs::read(parent.join("settings.json")) {
                if let Ok(v) = serde_json::from_slice::<serde_json::Value>(&bytes) {
                    if let Some(p) = v["output"].as_str() {
                        s.output = p.into();
                    }
                }
            }
        }
        Self {
            inner: Mutex::new(Engine {
                snapshot: s,
                cancel: None,
                closing: false,
            }),
            wake: Condvar::new(),
            path,
        }
    }
    pub fn snapshot(&self) -> Snapshot {
        self.inner.lock().unwrap().snapshot.clone()
    }
    fn persist(&self, s: &Snapshot) -> Result<(), String> {
        let parent = self.path.parent().ok_or("无法定位任务存储目录")?;
        fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        let mut file = tempfile::NamedTempFile::new_in(parent).map_err(|e| e.to_string())?;
        file.write_all(&serde_json::to_vec_pretty(s).map_err(|e| e.to_string())?)
            .map_err(|e| e.to_string())?;
        file.as_file().sync_all().map_err(|e| e.to_string())?;
        file.persist(&self.path)
            .map_err(|e| format!("无法保存任务历史：{e}"))?;
        Ok(())
    }
    fn save_or_warn(&self, e: &mut Engine) {
        if let Err(err) = self.persist(&e.snapshot) {
            e.snapshot.warning = err;
        }
    }
    fn edit<T>(
        &self,
        operation: impl FnOnce(&mut Snapshot) -> Result<T, String>,
    ) -> Result<T, String> {
        let mut e = self.inner.lock().unwrap();
        if e.closing {
            return Err("应用正在退出，请重新打开后操作。".into());
        }
        let mut next = e.snapshot.clone();
        let result = operation(&mut next)?;
        self.persist(&next)?;
        e.snapshot = next;
        self.wake.notify_all();
        Ok(result)
    }
    pub fn add(&self, text: &str, mode: &str) -> Result<Added, String> {
        if !matches!(mode, "both" | "audio" | "video") {
            return Err("请选择下载类型。".into());
        }
        let parsed = parse_batch(text)?;
        if parsed.links.is_empty() {
            return Err("没有找到有效的抖音链接。支持粘贴多条链接或整段分享文字。".into());
        }
        self.edit(|s| {
            let mut ids = vec![];
            let mut duplicates = parsed.duplicates;
            for link in parsed.links {
                if s.tasks.iter().any(|t| {
                    t.link == link
                        && t.mode == mode
                        && t.output == s.output
                        && (!t.terminal() || t.reusable())
                }) {
                    duplicates += 1;
                    continue;
                }
                if s.tasks.iter().filter(|t| !t.terminal()).count() >= 100 {
                    return Err("队列最多保留 100 个未完成任务，请完成部分任务后再添加。".into());
                }
                while s.tasks.len() >= 500 {
                    if let Some(index) = s.tasks.iter().position(Task::terminal) {
                        s.tasks.remove(index);
                    } else {
                        return Err("任务数量已达上限".into());
                    }
                }
                let id = s.next_id;
                s.next_id += 1;
                ids.push(id);
                s.tasks.push(Task {
                    id,
                    link,
                    mode: mode.into(),
                    output: s.output.clone(),
                    status: "queued".into(),
                    progress: 0,
                    message: "等待下载".into(),
                    title: String::new(),
                    video_id: String::new(),
                    files: vec![],
                    created_at: now(),
                    attempts: 0,
                });
            }
            s.mode = mode.into();
            Ok(Added {
                added: ids.len(),
                ids,
                duplicates,
                ignored: parsed.ignored,
            })
        })
    }
    pub fn pause(&self, paused: bool) -> Result<(), String> {
        self.edit(|s| {
            s.paused = paused;
            if !paused {
                s.warning.clear();
            }
            Ok(())
        })
    }
    pub fn set_output(&self, output: String) -> Result<(), String> {
        self.edit(|s| {
            s.output = output;
            Ok(())
        })
    }
    pub fn cancel(&self, id: Option<u64>) -> Result<(), String> {
        let mut e = self.inner.lock().unwrap();
        let mut next = e.snapshot.clone();
        let mut cancel_running = false;
        for task in &mut next.tasks {
            if id.is_none() || id == Some(task.id) {
                if task.status == "queued" {
                    task.status = "cancelled".into();
                    task.message = "已取消".into();
                } else if task.status == "running" {
                    task.message = "正在取消并清理临时文件…".into();
                    cancel_running = true;
                }
            }
        }
        self.persist(&next)?;
        e.snapshot = next;
        if cancel_running {
            if let Some(c) = &e.cancel {
                c.store(true, Ordering::Relaxed);
            }
        }
        Ok(())
    }
    pub fn retry(&self, id: Option<u64>) -> Result<usize, String> {
        self.edit(|s| {
            let targets: Vec<usize> = s
                .tasks
                .iter()
                .enumerate()
                .filter(|(_, t)| {
                    matches!(t.status.as_str(), "error" | "cancelled")
                        && (id == Some(t.id) || id.is_none() && t.status == "error")
                })
                .map(|(i, _)| i)
                .collect();
            if targets.len() + s.tasks.iter().filter(|t| !t.terminal()).count() > 100 {
                return Err("未完成任务超过 100 个，请稍后重试。".into());
            }
            for i in &targets {
                let t = &s.tasks[*i];
                if s.tasks.iter().any(|other| {
                    other.id != t.id
                        && !other.terminal()
                        && other.link == t.link
                        && other.mode == t.mode
                        && other.output == t.output
                }) {
                    return Err("相同任务已经在队列中。".into());
                }
            }
            let count = targets.len();
            for i in targets {
                let t = &mut s.tasks[i];
                t.status = "queued".into();
                t.progress = 0;
                t.message = "等待重试".into();
                t.files.clear();
            }
            Ok(count)
        })
    }
    pub fn clear_history(&self) -> Result<(), String> {
        self.edit(|s| {
            s.tasks.retain(|t| !t.terminal());
            Ok(())
        })
    }
    pub fn shutdown(&self) -> bool {
        let mut e = self.inner.lock().unwrap();
        e.closing = true;
        e.snapshot.paused = true;
        let active = e.cancel.is_some();
        if let Some(c) = &e.cancel {
            c.store(true, Ordering::Relaxed);
        }
        self.save_or_warn(&mut e);
        self.wake.notify_all();
        active
    }
    pub fn failed_links(&self) -> String {
        self.snapshot()
            .tasks
            .iter()
            .filter(|t| t.status == "error")
            .map(|t| t.link.as_str())
            .collect::<Vec<_>>()
            .join("\r\n")
    }
    pub fn start(self: &Arc<Self>, on_exit: impl Fn() + Send + 'static) {
        let shared = Arc::clone(self);
        thread::spawn(move || {
            shared.worker();
            on_exit();
        });
    }
    fn worker(self: &Arc<Self>) {
        loop {
            let (task, cancel) = {
                let mut e = self.inner.lock().unwrap();
                while !e.closing
                    && (e.snapshot.paused || !e.snapshot.tasks.iter().any(|t| t.status == "queued"))
                {
                    e = self.wake.wait(e).unwrap();
                }
                if e.closing {
                    return;
                }
                let i = e
                    .snapshot
                    .tasks
                    .iter()
                    .position(|t| t.status == "queued")
                    .unwrap();
                let c = Arc::new(AtomicBool::new(false));
                e.cancel = Some(Arc::clone(&c));
                let t = &mut e.snapshot.tasks[i];
                t.status = "running".into();
                t.message = "正在解析分享链接…".into();
                t.progress = 5;
                t.attempts += 1;
                let t = t.clone();
                self.save_or_warn(&mut e);
                (t, c)
            };
            let result = self.process(&task, &cancel);
            let mut e = self.inner.lock().unwrap();
            let closing = e.closing;
            if let Some(t) = e.snapshot.tasks.iter_mut().find(|t| t.id == task.id) {
                match result {
                    Ok((files, skipped)) => {
                        t.status = if skipped { "skipped" } else { "done" }.into();
                        t.progress = 100;
                        t.message = if skipped {
                            "相同作品已经下载，已跳过"
                        } else {
                            "下载完成"
                        }
                        .into();
                        t.files = files;
                    }
                    Err(error) => {
                        if closing {
                            t.status = "queued".into();
                            t.progress = 0;
                            t.message = "上次中断，等待重新下载".into();
                        } else if cancel.load(Ordering::Relaxed) {
                            t.status = "cancelled".into();
                            t.message = "已取消".into();
                        } else {
                            t.status = "error".into();
                            t.message = format!("{error:#}");
                        }
                    }
                }
            }
            e.cancel = None;
            self.save_or_warn(&mut e);
        }
    }
    fn process(
        self: &Arc<Self>,
        task: &Task,
        cancel: &Arc<AtomicBool>,
    ) -> anyhow::Result<(Vec<String>, bool)> {
        let vid = download::resolve_id(download::parse_link(&task.link)?, cancel)?;
        {
            let mut e = self.inner.lock().unwrap();
            let old = e
                .snapshot
                .tasks
                .iter()
                .find(|t| {
                    t.id != task.id
                        && t.video_id == vid
                        && t.mode == task.mode
                        && t.output == task.output
                        && t.reusable()
                })
                .cloned();
            if let Some(t) = e.snapshot.tasks.iter_mut().find(|t| t.id == task.id) {
                t.video_id = vid.clone();
                if let Some(other) = &old {
                    t.title = other.title.clone();
                }
            }
            if let Some(other) = old {
                return Ok((other.files, true));
            }
        }
        let (tx, rx) = mpsc::channel();
        let shared = Arc::clone(self);
        let id = task.id;
        let observer = thread::spawn(move || {
            for event in rx {
                let mut e = shared.inner.lock().unwrap();
                let cancelling = e.cancel.as_ref().is_some_and(|c| c.load(Ordering::Relaxed));
                if let Some(t) = e.snapshot.tasks.iter_mut().find(|t| t.id == id) {
                    match event {
                        Event::Progress(n, text) => {
                            t.progress = n;
                            if !cancelling {
                                t.message = text;
                            }
                        }
                        Event::Title(title) => t.title = title,
                    }
                }
            }
        });
        let mode = match task.mode.as_str() {
            "audio" => Mode::Audio,
            "video" => Mode::Video,
            _ => Mode::Both,
        };
        let result = download::run(
            &format!("https://www.douyin.com/video/{vid}"),
            mode,
            Path::new(&task.output),
            &tx,
            cancel,
        );
        drop(tx);
        let _ = observer.join();
        Ok((
            result?
                .iter()
                .map(|p| p.to_string_lossy().into_owned())
                .collect(),
            false,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn setup() -> (tempfile::TempDir, Shared) {
        let dir = tempfile::tempdir().unwrap();
        let s = Shared::load(dir.path().join("queue.json"));
        (dir, s)
    }
    #[test]
    fn mixed_batch_canonicalizes_and_counts() {
        let p=parse_batch("文案 https://www.iesdouyin.com/share/video/123/?foo=1 https://www.douyin.com/video/123?bar=2 https://example.com/no https://v.douyin.com/abc/").unwrap();
        assert_eq!(p.links.len(), 2);
        assert_eq!(p.duplicates, 1);
        assert_eq!(p.ignored, 1);
    }
    #[test]
    fn duplicate_is_scoped_to_mode_and_folder() {
        let (_d, s) = setup();
        assert_eq!(s.add("https://v.douyin.com/a/", "both").unwrap().added, 1);
        assert_eq!(
            s.add("https://v.douyin.com/a/?x=y", "both")
                .unwrap()
                .duplicates,
            1
        );
        assert_eq!(s.add("https://v.douyin.com/a/", "audio").unwrap().added, 1);
        s.set_output("D:/other".into()).unwrap();
        assert_eq!(s.add("https://v.douyin.com/a/", "both").unwrap().added, 1);
    }
    #[test]
    fn recovery_pauses_and_restores_running() {
        let (_d, s) = setup();
        s.add("https://v.douyin.com/a/", "both").unwrap();
        {
            let mut e = s.inner.lock().unwrap();
            e.snapshot.tasks[0].status = "running".into();
            s.persist(&e.snapshot).unwrap();
        }
        let recovered = Shared::load(s.path.clone()).snapshot();
        assert!(recovered.paused);
        assert_eq!(recovered.tasks[0].status, "queued");
        assert_eq!(recovered.next_id, 2);
    }
    #[test]
    fn cancel_retry_and_clear_preserve_files() {
        let (d, s) = setup();
        let path = d.path().join("saved.mp4");
        fs::write(&path, b"video").unwrap();
        let ids = s
            .add("https://v.douyin.com/a/ https://v.douyin.com/b/", "both")
            .unwrap()
            .ids;
        s.cancel(Some(ids[0])).unwrap();
        assert_eq!(s.retry(Some(ids[0])).unwrap(), 1);
        s.cancel(None).unwrap();
        s.clear_history().unwrap();
        assert!(s.snapshot().tasks.is_empty());
        assert!(path.exists());
    }
    #[test]
    fn queue_capacity_rejects_atomically() {
        let (_d, s) = setup();
        let text = (0..100)
            .map(|i| format!("https://www.douyin.com/video/{i}"))
            .collect::<Vec<_>>()
            .join("\n");
        s.add(&text, "both").unwrap();
        assert!(s.add("https://www.douyin.com/video/100", "both").is_err());
        assert_eq!(s.snapshot().tasks.len(), 100);
    }
    #[test]
    fn failed_persistence_does_not_enqueue() {
        let (d, s) = setup();
        fs::create_dir(&s.path).unwrap();
        assert!(s.add("https://v.douyin.com/a/", "both").is_err());
        assert!(s.snapshot().tasks.is_empty());
        drop(d);
    }
    #[test]
    fn paused_worker_exits_after_shutdown() {
        let (_d, s) = setup();
        let s = Arc::new(s);
        s.pause(true).unwrap();
        s.add("https://www.douyin.com/video/123", "both").unwrap();
        let (tx, rx) = mpsc::channel();
        s.start(move || {
            tx.send(()).unwrap();
        });
        s.shutdown();
        rx.recv_timeout(std::time::Duration::from_secs(3)).unwrap();
        assert_eq!(s.snapshot().tasks[0].status, "queued");
    }
}
