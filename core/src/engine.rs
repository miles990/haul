//! 下載引擎：佇列、限流、驗證閘門、檔名處理。
//!
//! 刻意不知道 UI 的存在——所有對外溝通都走 `Sink` 回呼。GUI 把事件轉成
//! Tauri event 推給 webview，CLI 把同一批事件印成 NDJSON。兩邊共用同一套
//! 行為，不會各自長出一份。

use crate::direct;
use crate::extract::{self, Mode, Probe};
use crate::tools::{self, Tools};
use crate::verify;

use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::{OnceCell, Semaphore};
use tokio::task::JoinHandle;

/// 進度事件節流。不節流的話一個 45MB 的檔案會送出約 2800 次事件。
const PROGRESS_EVERY: Duration = Duration::from_millis(200);

#[derive(Clone, Serialize, Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
pub struct Item {
    pub id: u64,
    pub input: String,
    pub title: String,
    /// video | audio
    pub kind: String,
    /// queued | resolving | downloading | verifying | done | failed
    pub status: String,
    pub bytes: u64,
    pub total: u64,
    pub secs: Option<f64>,
    pub file: Option<String>,
    /// 完成後的完整路徑
    pub path: Option<String>,
    pub error: Option<String>,
}

#[derive(Clone, Serialize, Debug)]
#[serde(tag = "event", rename_all = "camelCase")]
pub enum Event {
    /// 正在取得 yt-dlp / ffmpeg
    Setup {
        tool: String,
        bytes: u64,
        total: u64,
    },
    SetupDone,
    SetupFailed {
        error: String,
    },
    /// 任何狀態變化都送出完整項目，消費端不需要自己拼狀態
    Item(Item),
}

pub type Sink = Arc<dyn Fn(Event) + Send + Sync>;

pub struct Config {
    pub out_dir: PathBuf,
    pub bin_dir: PathBuf,
    pub max_downloads: usize,
    pub max_verifies: usize,
}

impl Config {
    pub fn new(out_dir: PathBuf, bin_dir: PathBuf) -> Self {
        Self {
            out_dir,
            bin_dir,
            max_downloads: 3,
            max_verifies: 2,
        }
    }
}

/// 一個項目要怎麼抓。yt-dlp 是主力，Direct 是它拒絕或不認識時的後備。
#[derive(Clone, Debug)]
enum Job {
    Ytdlp { url: String },
    Direct { media: String, title: String },
}

enum Resolution {
    Single {
        job: Job,
        title: String,
    },
    Playlist {
        title: String,
        items: Vec<(Job, String)>,
    },
}

/// 歷史最多保留幾筆，避免無限成長
const HISTORY_CAP: usize = 500;

pub struct Engine {
    cfg: Config,
    staging: PathBuf,
    /// append-only 的 NDJSON 歷史。用追加而非覆寫，GUI 與 CLI 同時跑
    /// 也不會互相蓋掉對方的紀錄。
    history: PathBuf,
    sink: Sink,
    items: Mutex<Vec<Item>>,
    seen: Mutex<HashSet<String>>,
    client: reqwest::Client,
    tools: OnceCell<Tools>,
    dl: Semaphore,
    vf: Semaphore,
    next_id: AtomicU64,
}

impl Engine {
    pub fn new(cfg: Config, sink: Sink) -> Result<Arc<Self>> {
        std::fs::create_dir_all(&cfg.out_dir)?;
        let staging = cfg.out_dir.join(".haul-part");
        std::fs::create_dir_all(&staging)?;
        sweep(&staging);

        let history = cfg.out_dir.join(".haul-history.jsonl");
        let past = load_history(&history, HISTORY_CAP);
        // 接在歷史最大的 id 之後，重啟後的新項目才不會跟舊紀錄撞號
        let next = past.iter().map(|i| i.id).max().unwrap_or(0) + 1;

        let client = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(15))
            .build()?;

        Ok(Arc::new(Self {
            dl: Semaphore::new(cfg.max_downloads),
            vf: Semaphore::new(cfg.max_verifies),
            cfg,
            staging,
            history,
            sink,
            items: Mutex::new(past),
            // 刻意不用歷史去填 seen：同一個連結想重抓是合理的，
            // 檔名撞號由 unique_path 處理
            seen: Mutex::new(HashSet::new()),
            client,
            tools: OnceCell::new(),
            next_id: AtomicU64::new(next),
        }))
    }

    /// 把一筆完成或失敗的紀錄追加到歷史檔
    fn record(&self, item: &Item) {
        use std::io::Write;
        let Ok(line) = serde_json::to_string(item) else {
            return;
        };
        if let Ok(mut f) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.history)
        {
            // 單一次 writeln 寫一行。O_APPEND 下的短寫入是原子的，
            // 所以多個程序同時追加不會互相插進對方的行裡。
            let _ = writeln!(f, "{line}");
        }
    }

    pub fn out_dir(&self) -> &Path {
        &self.cfg.out_dir
    }

    pub fn snapshot(&self) -> Vec<Item> {
        self.items.lock().unwrap().clone()
    }

    /// 移除已完成與失敗的項目，回傳剩下的
    pub fn clear_finished(&self) -> Vec<Item> {
        let mut g = self.items.lock().unwrap();
        g.retain(|i| i.status != "done" && i.status != "failed");
        g.clone()
    }

    fn emit(&self, e: Event) {
        (self.sink)(e);
    }

    fn push(&self, input: String, title: String, kind: &str) -> u64 {
        let item = Item {
            id: self.next_id.fetch_add(1, Ordering::Relaxed),
            input,
            title,
            kind: kind.to_string(),
            status: "queued".into(),
            bytes: 0,
            total: 0,
            secs: None,
            file: None,
            path: None,
            error: None,
        };
        let id = item.id;
        self.items.lock().unwrap().push(item.clone());
        self.emit(Event::Item(item));
        id
    }

    /// 鎖只在函式內存活，不跨 await
    fn update(&self, id: u64, f: impl FnOnce(&mut Item)) -> Option<Item> {
        let patched = {
            let mut g = self.items.lock().unwrap();
            match g.iter_mut().find(|i| i.id == id) {
                Some(it) => {
                    f(it);
                    Some(it.clone())
                }
                None => None,
            }
        };
        if let Some(item) = &patched {
            self.emit(Event::Item(item.clone()));
        }
        patched
    }

    /// 終局狀態才寫進歷史。done 與 failed 各自只會發生一次，不會重複記錄。
    fn finish(&self, id: u64, f: impl FnOnce(&mut Item)) {
        if let Some(item) = self.update(id, f) {
            self.record(&item);
        }
    }

    fn fail(&self, id: u64, why: impl Into<String>) {
        let why = why.into();
        self.finish(id, |i| {
            i.status = "failed".into();
            i.error = Some(why);
        });
    }

    /// 取得（必要時先下載）yt-dlp 與 ffmpeg。多個呼叫者只會下載一次。
    pub async fn tools(&self) -> Result<Tools, String> {
        self.tools
            .get_or_try_init(|| async {
                let result =
                    tools::ensure(&self.client, &self.cfg.bin_dir, |tool, bytes, total| {
                        self.emit(Event::Setup {
                            tool: tool.to_string(),
                            bytes,
                            total,
                        });
                    })
                    .await;

                match result {
                    Ok(t) => {
                        self.emit(Event::SetupDone);
                        Ok(t)
                    }
                    Err(e) => {
                        let msg = e.to_string();
                        self.emit(Event::SetupFailed { error: msg.clone() });
                        Err(msg)
                    }
                }
            })
            .await
            .cloned()
    }

    /// 讓 yt-dlp 自我更新。站點改版時靠這個跟上。
    pub async fn update_tools(&self) -> Result<String, String> {
        let t = self.tools().await?;
        tools::update_ytdlp(&t).await.map_err(|e| e.to_string())
    }

    /// 加入一筆輸入。回傳每個實際下載任務的 handle，讓 CLI 能等它們跑完。
    pub async fn add(self: &Arc<Self>, input: String, mode: Mode) -> Vec<JoinHandle<()>> {
        let kind = if mode == Mode::Audio {
            "audio"
        } else {
            "video"
        };
        let id = self.push(input.clone(), short(&input), kind);
        self.update(id, |i| i.status = "resolving".into());

        let tools = match self.tools().await {
            Ok(t) => t,
            Err(e) => {
                self.fail(id, format!("準備下載工具失敗：{e}"));
                return Vec::new();
            }
        };

        let resolution = match self.resolve(&tools, &input).await {
            Ok(r) => r,
            Err(e) => {
                self.fail(id, e.to_string());
                return Vec::new();
            }
        };

        let mut jobs: Vec<(u64, Job)> = Vec::new();
        match resolution {
            Resolution::Single { job, title } => {
                if !self.seen.lock().unwrap().insert(input.clone()) {
                    self.update(id, |i| {
                        i.status = "done".into();
                        i.error = Some("這個項目已經在佇列裡了".into());
                    });
                    return Vec::new();
                }
                self.update(id, |i| i.title = title);
                jobs.push((id, job));
            }
            Resolution::Playlist { title, items } => {
                self.update(id, |i| i.title = format!("{title}（清單）"));
                let mut first = true;
                for (job, label) in items {
                    let key = match &job {
                        Job::Ytdlp { url } => url.clone(),
                        Job::Direct { media, .. } => media.clone(),
                    };
                    if !self.seen.lock().unwrap().insert(key.clone()) {
                        continue;
                    }
                    if first {
                        first = false;
                        self.update(id, |i| i.title = label);
                        jobs.push((id, job));
                    } else {
                        let nid = self.push(key, label, kind);
                        jobs.push((nid, job));
                    }
                }
                if jobs.is_empty() {
                    self.update(id, |i| {
                        i.status = "done".into();
                        i.error = Some("這份清單裡的項目都已經排過了".into());
                    });
                    return Vec::new();
                }
            }
        }

        jobs.into_iter()
            .map(|(item_id, job)| {
                let me = self.clone();
                let tools = tools.clone();
                tokio::spawn(async move { me.run(tools, item_id, job, mode).await })
            })
            .collect()
    }

    /// 決定一個輸入該怎麼抓
    async fn resolve(&self, tools: &Tools, input: &str) -> Result<Resolution> {
        match extract::probe(tools, input).await {
            Ok(Probe::Single { title }) => Ok(Resolution::Single {
                job: Job::Ytdlp {
                    url: input.to_string(),
                },
                title,
            }),
            Ok(Probe::Playlist { title, entries }) => Ok(Resolution::Playlist {
                title,
                items: entries
                    .into_iter()
                    .map(|e| {
                        let label = e.title.clone().unwrap_or_else(|| short(&e.url));
                        (Job::Ytdlp { url: e.url }, label)
                    })
                    .collect(),
            }),
            // yt-dlp 對某些站是政策性拒絕（例如 suno.com），不是還沒實作。
            // 這種情況才輪到直接抓取。
            Err(yt_err) => match direct::probe(&self.client, input).await {
                Ok(found) => Ok(Resolution::Single {
                    job: Job::Direct {
                        media: found.media,
                        title: found.title.clone(),
                    },
                    title: found.title,
                }),
                // 沒有直接規則時，該讓使用者看到的是 yt-dlp 的原因
                Err(_) => Err(yt_err),
            },
        }
    }

    async fn run(&self, tools: Tools, id: u64, job: Job, mode: Mode) {
        // 1. 下載（限流）
        let permit = match self.dl.acquire().await {
            Ok(p) => p,
            Err(_) => return,
        };
        self.update(id, |i| i.status = "downloading".into());

        let mut last = Instant::now() - PROGRESS_EVERY;
        let mut progress = |bytes: u64, total: u64| {
            if last.elapsed() >= PROGRESS_EVERY || (total > 0 && bytes >= total) {
                last = Instant::now();
                self.update(id, |i| {
                    i.bytes = bytes;
                    i.total = total;
                });
            }
        };

        let outcome = self.fetch(&tools, id, &job, mode, &mut progress).await;
        drop(permit); // 讓下一個開始下載，驗證走另一條隊

        let (staged, reported, forced_stem) = match outcome {
            Ok(v) => v,
            Err(e) => return self.fail(id, e.to_string()),
        };

        // 2. 驗證（限流）
        let _vp = match self.vf.acquire().await {
            Ok(p) => p,
            Err(_) => return,
        };
        self.update(id, |i| i.status = "verifying".into());

        // 音訊：symphonia 先試，它不認識的編碼交給 ffmpeg 裁決
        let probe_path = staged.clone();
        let decoded = tokio::task::spawn_blocking(move || verify::verify(&probe_path)).await;
        let secs = match decoded {
            Ok(Ok(v)) => Some(v.secs),
            Ok(Err(sym_err)) => {
                match verify::verify_audio_with_ffmpeg(&tools.ffmpeg, &staged).await {
                    Ok(()) => reported,
                    Err(ff_err) => {
                        let _ = tokio::fs::remove_file(&staged).await;
                        return self
                            .fail(id, format!("驗證未通過：{ff_err}（symphonia：{sym_err}）"));
                    }
                }
            }
            Err(e) => return self.fail(id, format!("驗證程序異常：{e}")),
        };

        // 影片：抽樣確認畫面解得出來
        if mode == Mode::Video {
            let dur = reported.or(secs).unwrap_or(0.0);
            if let Err(e) = verify::verify_video(&tools.ffmpeg, &staged, dur).await {
                let _ = tokio::fs::remove_file(&staged).await;
                return self.fail(id, format!("驗證未通過：{e}"));
            }
        }

        // 3. 過關才搬進正式資料夾
        let stem = forced_stem.unwrap_or_else(|| {
            staged
                .file_stem()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_else(|| "untitled".into())
        });
        let ext = staged
            .extension()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_else(|| "bin".into());
        let dest = unique_path(&self.cfg.out_dir, &sanitize(&stem), &ext);

        if let Err(e) = tokio::fs::rename(&staged, &dest).await {
            return self.fail(id, format!("搬移失敗：{e}"));
        }

        let name = dest
            .file_name()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_default();
        let full = dest.to_string_lossy().to_string();
        let size = tokio::fs::metadata(&dest)
            .await
            .map(|m| m.len())
            .unwrap_or(0);

        self.finish(id, |i| {
            i.status = "done".into();
            i.secs = reported.or(secs);
            i.file = Some(name);
            i.path = Some(full);
            i.bytes = size;
            i.total = size;
            i.error = None;
        });
    }

    /// 回傳（待驗證的檔案、時長、指定的檔名主體）
    async fn fetch(
        &self,
        tools: &Tools,
        tag: u64,
        job: &Job,
        mode: Mode,
        progress: &mut impl FnMut(u64, u64),
    ) -> Result<(PathBuf, Option<f64>, Option<String>)> {
        match job {
            // yt-dlp 自己會取好檔名，沿用它的
            Job::Ytdlp { url } => extract::download(tools, url, mode, &self.staging, progress)
                .await
                .map(|d| (d.path, d.secs, None)),

            Job::Direct { media, title } => {
                let ext = ext_of(media);
                let raw = self.staging.join(format!("{tag}-raw.{ext}"));
                direct::download(&self.client, media, &raw, progress).await?;

                if mode == Mode::Audio && is_video_container(&ext) {
                    // 影音混合檔要的只是聲音：-c copy 抽出音軌，不重新編碼
                    let m4a = self.staging.join(format!("{tag}.m4a"));
                    let extracted = direct::extract_audio(&tools.ffmpeg, &raw, &m4a).await;
                    let _ = tokio::fs::remove_file(&raw).await;
                    extracted?;
                    Ok((m4a, None, Some(title.clone())))
                } else {
                    Ok((raw, None, Some(title.clone())))
                }
            }
        }
    }
}

// ---------------------------------------------------------------- 輔助

/// 讀回歷史紀錄。壞掉的行直接跳過 —— 一行讀不懂不該讓整份歷史消失。
///
/// 檔案已經不在的項目會被濾掉：清單上留著一個點下去打不開的東西，
/// 比不顯示它更糟。
pub fn load_history(path: &Path, cap: usize) -> Vec<Item> {
    let Ok(text) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    let mut items: Vec<Item> = text
        .lines()
        .filter(|l| !l.trim().is_empty())
        .filter_map(|l| serde_json::from_str::<Item>(l).ok())
        .filter(|i| match &i.path {
            Some(p) => Path::new(p).is_file(),
            // 失敗的項目沒有檔案，但保留下來仍有參考價值
            None => i.status == "failed",
        })
        .collect();

    if items.len() > cap {
        items.drain(0..items.len() - cap);
    }
    items
}

/// 清掉上次沒下載完留下的暫存
fn sweep(staging: &Path) {
    if let Ok(rd) = std::fs::read_dir(staging) {
        for e in rd.flatten() {
            let p = e.path();
            let _ = if p.is_dir() {
                std::fs::remove_dir_all(&p)
            } else {
                std::fs::remove_file(&p)
            };
        }
    }
}

pub fn short(s: &str) -> String {
    let t = s.trim_end_matches('/').rsplit('/').next().unwrap_or(s);
    t.chars().take(28).collect()
}

/// 影音容器：在「只要聲音」模式下需要多抽一道音軌
fn is_video_container(ext: &str) -> bool {
    matches!(
        ext.to_ascii_lowercase().as_str(),
        "mp4" | "webm" | "mkv" | "mov" | "avi" | "flv" | "ts"
    )
}

fn ext_of(url: &str) -> String {
    url.split('?')
        .next()
        .unwrap_or(url)
        .rsplit('/')
        .next()
        .and_then(|f| f.rsplit_once('.').map(|(_, e)| e.to_ascii_lowercase()))
        .filter(|e| e.len() <= 5 && e.chars().all(|c| c.is_ascii_alphanumeric()))
        .unwrap_or_else(|| "bin".into())
}

/// Windows 保留裝置名。macOS 不管這些，但檔案要能互通就得一起避開。
const WIN_RESERVED: &[&str] = &[
    "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8",
    "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
];

pub fn sanitize(name: &str) -> String {
    let replaced: String = name
        .chars()
        .map(|c| match c {
            '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*' => ' ',
            c if (c as u32) < 0x20 || c as u32 == 0x7f => ' ',
            c => c,
        })
        .collect();

    let mut s = replaced.split_whitespace().collect::<Vec<_>>().join(" ");

    // 以字元為單位截斷，不會切壞 UTF-8（中文與日文標題很重要）
    if s.chars().count() > 110 {
        s = s.chars().take(110).collect();
    }
    while s.ends_with('.') || s.ends_with(' ') {
        s.pop();
    }
    if s.is_empty() {
        return "untitled".into();
    }

    let stem = s.split('.').next().unwrap_or(&s).to_uppercase();
    if WIN_RESERVED.contains(&stem.as_str()) {
        return format!("_{s}");
    }
    s
}

pub fn unique_path(dir: &Path, base: &str, ext: &str) -> PathBuf {
    let first = dir.join(format!("{base}.{ext}"));
    if !first.exists() {
        return first;
    }
    for n in 2..1000 {
        let p = dir.join(format!("{base} ({n}).{ext}"));
        if !p.exists() {
            return p;
        }
    }
    dir.join(format!("{base} {}.{ext}", std::process::id()))
}

/// 檔案必須落在輸出資料夾底下才允許開啟。路徑可能來自前端或命令列，
/// 不驗證的話這就變成「叫作業系統開啟任意路徑」的通道。canonicalize
/// 會解掉 .. 與符號連結，比字串比對可靠。
pub fn validate_playable(out_dir: &Path, path: &str) -> Result<PathBuf, String> {
    let target = Path::new(path)
        .canonicalize()
        .map_err(|e| format!("找不到這個檔案：{e}"))?;
    let root = out_dir
        .canonicalize()
        .map_err(|e| format!("輸出資料夾有問題：{e}"))?;

    if !target.starts_with(&root) {
        return Err("這個檔案不在下載資料夾裡".into());
    }
    if !target.is_file() {
        return Err("這不是一個檔案".into());
    }
    Ok(target)
}

/// 用系統預設程式開啟
pub fn open_with_system(target: &Path) -> Result<(), String> {
    #[cfg(target_os = "macos")]
    let mut cmd = std::process::Command::new("open");
    #[cfg(target_os = "windows")]
    let mut cmd = {
        // Windows 沒有等價的單一執行檔，走 cmd 的 start；
        // 第一個空字串是 start 的視窗標題參數，省略會把路徑當標題吃掉
        let mut c = std::process::Command::new("cmd");
        c.args(["/C", "start", ""]);
        c
    };
    #[cfg(all(not(target_os = "macos"), not(target_os = "windows")))]
    let mut cmd = std::process::Command::new("xdg-open");

    cmd.arg(target)
        .spawn()
        .map(|_| ())
        .map_err(|e| format!("開啟失敗：{e}"))
}

pub fn home_dir() -> PathBuf {
    #[cfg(windows)]
    let v = std::env::var_os("USERPROFILE");
    #[cfg(not(windows))]
    let v = std::env::var_os("HOME");
    v.map(PathBuf::from).unwrap_or_else(|| PathBuf::from("."))
}

pub fn default_out_dir() -> PathBuf {
    home_dir().join("Downloads").join("Haul")
}

/// yt-dlp 與 ffmpeg 的存放處。GUI 與 CLI 必須算出同一個路徑，
/// 否則兩邊會各自下載一份 80MB。
pub fn default_bin_dir() -> PathBuf {
    const APP_ID: &str = "com.haul.desktop";

    #[cfg(target_os = "macos")]
    let base = home_dir().join("Library").join("Application Support");

    #[cfg(target_os = "windows")]
    let base = std::env::var_os("APPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(|| home_dir().join("AppData").join("Roaming"));

    #[cfg(all(not(target_os = "macos"), not(target_os = "windows")))]
    let base = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home_dir().join(".local").join("share"));

    base.join(APP_ID).join("bin")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_strips_windows_illegal_chars() {
        assert_eq!(sanitize(r#"a/b\c:d*e?f"g<h>i|j"#), "a b c d e f g h i j");
    }

    #[test]
    fn sanitize_handles_windows_trailing_dot_and_space() {
        assert_eq!(sanitize("song name."), "song name");
        assert_eq!(sanitize("song name   "), "song name");
    }

    #[test]
    fn sanitize_escapes_windows_reserved_names() {
        assert_eq!(sanitize("NUL"), "_NUL");
        assert_eq!(sanitize("con"), "_con");
        assert_eq!(sanitize("COM1.mp4"), "_COM1.mp4");
        assert_eq!(sanitize("CONCERT"), "CONCERT"); // 只有完全相同才算保留字
    }

    #[test]
    fn sanitize_never_splits_multibyte_chars() {
        let long = "宮".repeat(200);
        let out = sanitize(&long);
        assert_eq!(out.chars().count(), 110);
        assert!(out.is_char_boundary(out.len()));
    }

    #[test]
    fn sanitize_falls_back_when_everything_stripped() {
        assert_eq!(sanitize("///"), "untitled");
        assert_eq!(sanitize(""), "untitled");
    }

    #[test]
    fn short_takes_the_tail_of_a_url() {
        assert_eq!(short("https://example.com/watch/abc"), "abc");
        assert_eq!(short("https://example.com/watch/abc/"), "abc");
    }

    #[test]
    fn ext_of_ignores_query_strings_and_junk() {
        assert_eq!(ext_of("https://x.com/a.mp4"), "mp4");
        assert_eq!(ext_of("https://x.com/a.MP4?t=1"), "mp4");
        assert_eq!(ext_of("https://x.com/watch"), "bin");
    }

    #[test]
    fn open_only_accepts_paths_inside_the_download_folder() {
        let root = std::env::temp_dir().join("haul-open-test");
        let inside = root.join("ok.mp4");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(&inside, b"not really a video, but it is a file").unwrap();

        assert!(validate_playable(&root, inside.to_str().unwrap()).is_ok());
        // 資料夾本身不是檔案
        assert!(validate_playable(&root, root.to_str().unwrap()).is_err());

        let outside = std::env::temp_dir().join("haul-open-outside.txt");
        std::fs::write(&outside, b"x").unwrap();
        assert!(validate_playable(&root, outside.to_str().unwrap()).is_err());

        // .. 逃逸：canonicalize 會解開，所以擋得住
        let escape = format!("{}/../haul-open-outside.txt", root.display());
        assert!(validate_playable(&root, &escape).is_err());

        assert!(validate_playable(&root, "/nope/nothing.mp4").is_err());

        let _ = std::fs::remove_file(&inside);
        let _ = std::fs::remove_file(&outside);
    }
}
