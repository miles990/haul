#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod direct;
mod extract;
mod tools;
mod verify;

use extract::{Mode, Probe};
use serde::Serialize;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tauri::{AppHandle, Emitter, Manager, State};
use tokio::sync::{OnceCell, Semaphore};

/// 同時下載幾個項目
const MAX_DOWNLOADS: usize = 3;
/// 同時驗證幾個。解碼吃 CPU，壓低以免跟其他程式搶。
const MAX_VERIFIES: usize = 2;
/// 進度事件節流，避免把 webview 洗爆
const PROGRESS_EVERY: Duration = Duration::from_millis(200);

// ---------------------------------------------------------------- 狀態

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct Item {
    id: u64,
    input: String,
    title: String,
    /// video | audio
    kind: String,
    /// queued | resolving | downloading | verifying | done | failed
    status: String,
    bytes: u64,
    total: u64,
    secs: Option<f64>,
    file: Option<String>,
    /// 完成後的完整路徑，點一下就用系統播放器開啟
    path: Option<String>,
    error: Option<String>,
}

#[derive(Clone, Serialize, Default)]
#[serde(rename_all = "camelCase")]
struct SetupEvent {
    /// start | progress | done | failed
    stage: String,
    tool: String,
    bytes: u64,
    total: u64,
    error: Option<String>,
}

/// 一個項目要怎麼抓。yt-dlp 是主力，Direct 是它拒絕或不認識時的後備。
#[derive(Clone, Debug)]
enum Job {
    Ytdlp { url: String },
    Direct { media: String, title: String },
}

struct AppState {
    items: Mutex<Vec<Item>>,
    seen: Mutex<HashSet<String>>,
    out_dir: PathBuf,
    staging: PathBuf,
    bin_dir: PathBuf,
    client: reqwest::Client,
    /// yt-dlp 與 ffmpeg。第一次要用到時才下載。
    tools: OnceCell<tools::Tools>,
    dl: Semaphore,
    vf: Semaphore,
    next_id: AtomicU64,
}

impl AppState {
    fn new(out_dir: PathBuf, bin_dir: PathBuf) -> anyhow::Result<Self> {
        let staging = out_dir.join(".haul-part");
        Ok(Self {
            items: Mutex::new(Vec::new()),
            seen: Mutex::new(HashSet::new()),
            out_dir,
            staging,
            bin_dir,
            client: reqwest::Client::builder()
                .connect_timeout(Duration::from_secs(15))
                .build()?,
            tools: OnceCell::new(),
            dl: Semaphore::new(MAX_DOWNLOADS),
            vf: Semaphore::new(MAX_VERIFIES),
            next_id: AtomicU64::new(1),
        })
    }

    fn push(&self, input: String, title: String, kind: &str) -> Item {
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
        self.items.lock().unwrap().push(item.clone());
        item
    }

    /// 鎖只在函式內存活，不跨 await
    fn patch(&self, id: u64, f: impl FnOnce(&mut Item)) -> Option<Item> {
        let mut guard = self.items.lock().unwrap();
        let it = guard.iter_mut().find(|i| i.id == id)?;
        f(it);
        Some(it.clone())
    }
}

fn state_of(app: &AppHandle) -> Arc<AppState> {
    app.state::<Arc<AppState>>().inner().clone()
}

fn update(app: &AppHandle, st: &AppState, id: u64, f: impl FnOnce(&mut Item)) {
    if let Some(item) = st.patch(id, f) {
        let _ = app.emit("item", item);
    }
}

fn fail(app: &AppHandle, st: &AppState, id: u64, why: impl Into<String>) {
    let why = why.into();
    update(app, st, id, |i| {
        i.status = "failed".into();
        i.error = Some(why);
    });
}

// ---------------------------------------------------------------- 外部工具

/// 取得（必要時先下載）yt-dlp 與 ffmpeg。多個任務同時呼叫只會下載一次。
async fn tools_ready(app: &AppHandle, st: &Arc<AppState>) -> Result<tools::Tools, String> {
    let app2 = app.clone();
    let client = st.client.clone();
    let bin = st.bin_dir.clone();

    st.tools
        .get_or_try_init(|| async move {
            let notify = app2.clone();
            let result = tools::ensure(&client, &bin, move |tool, bytes, total| {
                let _ = notify.emit(
                    "setup",
                    SetupEvent {
                        stage: "progress".into(),
                        tool: tool.to_string(),
                        bytes,
                        total,
                        error: None,
                    },
                );
            })
            .await;

            match result {
                Ok(t) => {
                    let _ = app2.emit(
                        "setup",
                        SetupEvent {
                            stage: "done".into(),
                            ..Default::default()
                        },
                    );
                    Ok(t)
                }
                Err(e) => {
                    let msg = e.to_string();
                    let _ = app2.emit(
                        "setup",
                        SetupEvent {
                            stage: "failed".into(),
                            error: Some(msg.clone()),
                            ..Default::default()
                        },
                    );
                    Err(msg)
                }
            }
        })
        .await
        .cloned()
}

// ---------------------------------------------------------------- 檔名

/// Windows 保留裝置名。macOS 不管這些，但檔案要能互通就得一起避開。
const WIN_RESERVED: &[&str] = &[
    "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8",
    "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
];

fn sanitize(name: &str) -> String {
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

fn unique_path(dir: &Path, base: &str, ext: &str) -> PathBuf {
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

fn home_dir() -> PathBuf {
    #[cfg(windows)]
    let v = std::env::var_os("USERPROFILE");
    #[cfg(not(windows))]
    let v = std::env::var_os("HOME");
    v.map(PathBuf::from).unwrap_or_else(|| PathBuf::from("."))
}

fn default_out_dir() -> PathBuf {
    home_dir().join("Downloads").join("Haul")
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

// ---------------------------------------------------------------- 主流程

fn short(s: &str) -> String {
    let t = s.trim_end_matches('/').rsplit('/').next().unwrap_or(s);
    t.chars().take(28).collect()
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

/// 決定一個輸入該怎麼抓。刻意不碰 UI 狀態，這樣端對端測試才能直接跑
/// 這條真正的路徑，而不是在測試裡另外抄一份邏輯。
async fn resolve(
    tools: &tools::Tools,
    client: &reqwest::Client,
    input: &str,
) -> anyhow::Result<Resolution> {
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
        Err(yt_err) => match direct::probe(client, input).await {
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

/// 實際把一個 job 抓下來。回傳（待驗證的檔案、時長、指定的檔名主體）。
async fn fetch_job(
    st: &AppState,
    tools: &tools::Tools,
    tag: u64,
    job: &Job,
    mode: Mode,
    progress: &mut impl FnMut(u64, u64),
) -> anyhow::Result<(PathBuf, Option<f64>, Option<String>)> {
    match job {
        // yt-dlp 自己會取好檔名，沿用它的
        Job::Ytdlp { url } => extract::download(tools, url, mode, &st.staging, progress)
            .await
            .map(|d| (d.path, d.secs, None)),

        Job::Direct { media, title } => {
            let ext = ext_of(media);
            let raw = st.staging.join(format!("{tag}-raw.{ext}"));
            direct::download(&st.client, media, &raw, progress).await?;

            if mode == Mode::Audio && is_video_container(&ext) {
                // 影音混合檔要的只是聲音：-c copy 抽出音軌，不重新編碼
                let m4a = st.staging.join(format!("{tag}.m4a"));
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

/// 一筆輸入 → 決定怎麼抓 → 單項或整份清單各自排隊
async fn handle_input(app: AppHandle, st: Arc<AppState>, input: String, mode: Mode) {
    let kind = if mode == Mode::Audio {
        "audio"
    } else {
        "video"
    };
    let probe_item = st.push(input.clone(), short(&input), kind);
    let _ = app.emit("item", probe_item.clone());
    let id = probe_item.id;

    update(&app, &st, id, |i| i.status = "resolving".into());

    let tools = match tools_ready(&app, &st).await {
        Ok(t) => t,
        Err(e) => return fail(&app, &st, id, format!("準備下載工具失敗：{e}")),
    };

    let resolution = match resolve(&tools, &st.client, &input).await {
        Ok(r) => r,
        Err(e) => return fail(&app, &st, id, e.to_string()),
    };

    let already_queued = |app: &AppHandle, st: &AppState, id: u64| {
        update(app, st, id, |i| {
            i.status = "done".into();
            i.error = Some("這個項目已經在佇列裡了".into());
        });
    };

    let mut jobs: Vec<(u64, Job)> = Vec::new();
    match resolution {
        Resolution::Single { job, title } => {
            if !st.seen.lock().unwrap().insert(input.clone()) {
                return already_queued(&app, &st, id);
            }
            update(&app, &st, id, |i| i.title = title);
            jobs.push((id, job));
        }
        Resolution::Playlist { title, items } => {
            update(&app, &st, id, |i| i.title = format!("{title}（清單）"));
            let mut first = true;
            for (job, label) in items {
                let key = match &job {
                    Job::Ytdlp { url } => url.clone(),
                    Job::Direct { media, .. } => media.clone(),
                };
                if !st.seen.lock().unwrap().insert(key.clone()) {
                    continue;
                }
                if first {
                    first = false;
                    update(&app, &st, id, |i| i.title = label);
                    jobs.push((id, job));
                } else {
                    let it = st.push(key, label, kind);
                    let _ = app.emit("item", it.clone());
                    jobs.push((it.id, job));
                }
            }
            if jobs.is_empty() {
                return update(&app, &st, id, |i| {
                    i.status = "done".into();
                    i.error = Some("這份清單裡的項目都已經排過了".into());
                });
            }
        }
    }

    for (item_id, job) in jobs {
        let app2 = app.clone();
        let st2 = st.clone();
        let tools2 = tools.clone();
        tauri::async_runtime::spawn(async move {
            run_item(app2, st2, tools2, item_id, job, mode).await;
        });
    }
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

async fn run_item(
    app: AppHandle,
    st: Arc<AppState>,
    tools: tools::Tools,
    id: u64,
    job: Job,
    mode: Mode,
) {
    // 1. 下載（限流）
    let permit = match st.dl.acquire().await {
        Ok(p) => p,
        Err(_) => return,
    };
    update(&app, &st, id, |i| i.status = "downloading".into());

    let mut last = Instant::now() - PROGRESS_EVERY;
    let mut progress = |bytes: u64, total: u64| {
        if last.elapsed() >= PROGRESS_EVERY || (total > 0 && bytes >= total) {
            last = Instant::now();
            update(&app, &st, id, |i| {
                i.bytes = bytes;
                i.total = total;
            });
        }
    };

    let outcome = fetch_job(&st, &tools, id, &job, mode, &mut progress).await;

    drop(permit); // 讓下一個開始下載，驗證走另一條隊

    let (staged_path, reported_secs, forced_stem) = match outcome {
        Ok(v) => v,
        Err(e) => return fail(&app, &st, id, e.to_string()),
    };
    let downloaded = extract::Downloaded {
        path: staged_path,
        secs: reported_secs,
    };

    // 2. 驗證（限流）
    let _vp = match st.vf.acquire().await {
        Ok(p) => p,
        Err(_) => return,
    };
    update(&app, &st, id, |i| i.status = "verifying".into());

    let staged = downloaded.path.clone();

    // 音訊：symphonia 先試，它不認識的編碼（例如 opus）交給 ffmpeg 裁決
    let probe_path = staged.clone();
    let audio = tokio::task::spawn_blocking(move || verify::verify(&probe_path)).await;
    let secs = match audio {
        Ok(Ok(v)) => Some(v.secs),
        Ok(Err(sym_err)) => match verify::verify_audio_with_ffmpeg(&tools.ffmpeg, &staged).await {
            Ok(()) => downloaded.secs,
            Err(ff_err) => {
                let _ = tokio::fs::remove_file(&staged).await;
                return fail(
                    &app,
                    &st,
                    id,
                    format!("驗證未通過：{ff_err}（symphonia：{sym_err}）"),
                );
            }
        },
        Err(e) => return fail(&app, &st, id, format!("驗證程序異常：{e}")),
    };

    // 影片：抽樣確認畫面解得出來
    if mode == Mode::Video {
        let dur = downloaded.secs.or(secs).unwrap_or(0.0);
        if let Err(e) = verify::verify_video(&tools.ffmpeg, &staged, dur).await {
            let _ = tokio::fs::remove_file(&staged).await;
            return fail(&app, &st, id, format!("驗證未通過：{e}"));
        }
    }

    // 3. 過關才搬進正式資料夾
    // 走 yt-dlp 時沿用它取好的檔名；直接抓取沒有檔名，用頁面標題
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
    let dest = unique_path(&st.out_dir, &sanitize(&stem), &ext);

    if let Err(e) = tokio::fs::rename(&staged, &dest).await {
        return fail(&app, &st, id, format!("搬移失敗：{e}"));
    }

    let name = dest
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_default();
    let full = dest.to_string_lossy().to_string();
    update(&app, &st, id, |i| {
        i.status = "done".into();
        i.secs = downloaded.secs.or(secs);
        i.file = Some(name);
        i.path = Some(full);
        i.error = None;
    });
}

// ---------------------------------------------------------------- 指令

#[tauri::command]
fn add(app: AppHandle, text: String, mode: String) -> Result<usize, String> {
    let inputs: Vec<String> = text
        .split(|c: char| c.is_whitespace())
        .map(str::trim)
        .filter(|s| s.starts_with("http"))
        .map(|s| s.trim_end_matches(&[')', ']', ',', '。'][..]).to_string())
        .collect();

    if inputs.is_empty() {
        return Err("沒看到網址。貼上影片或音樂的連結，一行一個。".into());
    }

    let mode = Mode::from_str(&mode);
    let st = state_of(&app);
    let n = inputs.len();
    for input in inputs {
        let app2 = app.clone();
        let st2 = st.clone();
        tauri::async_runtime::spawn(async move { handle_input(app2, st2, input, mode).await });
    }
    Ok(n)
}

#[tauri::command]
fn snapshot(state: State<'_, Arc<AppState>>) -> Vec<Item> {
    state.items.lock().unwrap().clone()
}

#[tauri::command]
fn out_dir(state: State<'_, Arc<AppState>>) -> String {
    state.out_dir.to_string_lossy().to_string()
}

#[tauri::command]
fn open_out_dir(state: State<'_, Arc<AppState>>) -> Result<(), String> {
    let dir = &state.out_dir;
    std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;

    #[cfg(target_os = "macos")]
    let mut cmd = std::process::Command::new("open");
    #[cfg(target_os = "windows")]
    let mut cmd = std::process::Command::new("explorer");
    #[cfg(all(not(target_os = "macos"), not(target_os = "windows")))]
    let mut cmd = std::process::Command::new("xdg-open");

    // explorer.exe 成功時也會回非零，所以只看能不能啟動
    cmd.arg(dir).spawn().map(|_| ()).map_err(|e| e.to_string())
}

/// 用系統預設播放器開啟一個下載好的檔案。
///
/// 路徑來自前端，所以要驗證它確實落在輸出資料夾底下——否則這個指令
/// 就變成「叫作業系統開啟任意路徑」的通道。canonicalize 會把 .. 解掉，
/// 符號連結也一併解析，所以比字串比對可靠。
fn validate_playable(out_dir: &Path, path: &str) -> Result<PathBuf, String> {
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

#[tauri::command]
fn open_file(state: State<'_, Arc<AppState>>, path: String) -> Result<(), String> {
    let target = validate_playable(&state.out_dir, &path)?;

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

    cmd.arg(&target)
        .spawn()
        .map(|_| ())
        .map_err(|e| format!("開啟失敗：{e}"))
}

#[tauri::command]
fn clear_done(state: State<'_, Arc<AppState>>) -> Vec<Item> {
    let mut guard = state.items.lock().unwrap();
    guard.retain(|i| i.status != "done" && i.status != "failed");
    guard.clone()
}

/// 讓 yt-dlp 自我更新。各站改版時靠這個跟上，不必等 Haul 重新發布。
#[tauri::command]
async fn update_tools(app: AppHandle) -> Result<String, String> {
    let st = state_of(&app);
    let tools = tools_ready(&app, &st).await?;
    tools::update_ytdlp(&tools).await.map_err(|e| e.to_string())
}

// ---------------------------------------------------------------- 進入點

fn main() {
    tauri::Builder::default()
        .setup(|app| {
            let out = default_out_dir();
            std::fs::create_dir_all(&out)?;

            let bin = app
                .path()
                .app_data_dir()
                .unwrap_or_else(|_| out.clone())
                .join("bin");

            let state = Arc::new(AppState::new(out, bin)?);
            std::fs::create_dir_all(&state.staging)?;
            sweep(&state.staging);
            app.manage(state);

            // 先把工具備好，使用者貼連結時就不用等
            let handle = app.handle().clone();
            tauri::async_runtime::spawn(async move {
                let st = state_of(&handle);
                let _ = tools_ready(&handle, &st).await;
            });

            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            add,
            snapshot,
            out_dir,
            open_out_dir,
            open_file,
            clear_done,
            update_tools
        ])
        .run(tauri::generate_context!())
        .expect("Tauri 啟動失敗");
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

    /// 端對端：取得工具 → 解析 → 下載 → 驗證。
    ///
    /// 需要網路，設 HAUL_E2E_URL 才會跑（CI 上不設，所以會跳過）。
    /// 加設 HAUL_E2E_AUDIO 可改測只要聲音的路徑。
    ///
    /// 單元測試證明不了這條鏈路 —— 站點的實際行為只有真的打過才知道。
    #[tokio::test]
    async fn end_to_end_download() {
        let Ok(url) = std::env::var("HAUL_E2E_URL") else {
            return;
        };
        let audio_only = std::env::var("HAUL_E2E_AUDIO").is_ok();
        let mode = if audio_only { Mode::Audio } else { Mode::Video };

        let bin = std::env::temp_dir().join("haul-e2e-bin");
        let out = std::env::temp_dir().join("haul-e2e-out");
        std::fs::create_dir_all(&out).unwrap();

        let st = AppState::new(out, bin.clone()).unwrap();
        std::fs::create_dir_all(&st.staging).unwrap();

        let tools = tools::ensure(&st.client, &bin, |t, got, total| {
            if total > 0 && got == total {
                eprintln!("  取得 {t}：{} MB", total / 1_048_576);
            }
        })
        .await
        .expect("取得 yt-dlp / ffmpeg 失敗");

        // 走的是 app 真正用的那條解析路徑，含 yt-dlp 失敗時的直接抓取後備
        let job = match resolve(&tools, &st.client, &url)
            .await
            .expect("解析連結失敗")
        {
            Resolution::Single { job, title } => {
                eprintln!("  單項：{title}");
                job
            }
            Resolution::Playlist { title, mut items } => {
                eprintln!("  清單：{title}，共 {} 項，只測第一項", items.len());
                assert!(!items.is_empty(), "清單展開後不該是空的");
                items.remove(0).0
            }
        };
        eprintln!("  來源：{job:?}");

        let (path, reported, stem) = fetch_job(&st, &tools, 1, &job, mode, &mut |_, _| {})
            .await
            .expect("下載失敗");
        eprintln!(
            "  檔案：{}\n  時長：{reported:?}  檔名主體：{stem:?}",
            path.display()
        );
        assert!(path.exists(), "回報的檔案不存在");

        // 音訊閘門：symphonia 先試，它不認識的編碼退回 ffmpeg
        let p = path.clone();
        let decoded = tokio::task::spawn_blocking(move || verify::verify(&p))
            .await
            .unwrap();
        let secs = match decoded {
            Ok(v) => {
                eprintln!("  symphonia 通過，長度 {:.1}s", v.secs);
                Some(v.secs)
            }
            Err(e) => {
                eprintln!("  symphonia 不認得（{e}），改用 ffmpeg");
                verify::verify_audio_with_ffmpeg(&tools.ffmpeg, &path)
                    .await
                    .expect("ffmpeg 音訊驗證也沒過");
                None
            }
        };

        if mode == Mode::Video {
            // 直接抓取沒有 yt-dlp 回報的時長，退回用 symphonia 解出來的，
            // 否則取樣點會全部落在第 0 秒，抓不到截斷
            let dur = reported.or(secs).unwrap_or(0.0);
            assert!(dur > 0.0, "拿不到時長，影片抽樣會失去意義");
            verify::verify_video(&tools.ffmpeg, &path, dur)
                .await
                .expect("影片抽樣驗證沒過");
            eprintln!("  影片抽樣通過（時長 {dur:.1}s）");
        }

        let _ = std::fs::remove_file(&path);
    }

    /// open_file 收的是前端給的路徑，若不驗證就等於「叫作業系統開啟任意檔案」。
    #[test]
    fn open_file_only_accepts_paths_inside_the_download_folder() {
        let root = std::env::temp_dir().join("haul-open-test");
        let inside = root.join("ok.mp4");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(&inside, b"not really a video, but it is a file").unwrap();

        // 資料夾內的真實檔案
        assert!(validate_playable(&root, inside.to_str().unwrap()).is_ok());

        // 資料夾本身不是檔案
        assert!(validate_playable(&root, root.to_str().unwrap()).is_err());

        // 資料夾外的檔案
        let outside = std::env::temp_dir().join("haul-open-outside.txt");
        std::fs::write(&outside, b"x").unwrap();
        assert!(validate_playable(&root, outside.to_str().unwrap()).is_err());

        // .. 逃逸：canonicalize 會把它解開，所以擋得住
        let escape = format!("{}/../haul-open-outside.txt", root.display());
        assert!(validate_playable(&root, &escape).is_err());

        // 不存在的路徑
        assert!(validate_playable(&root, "/nope/nothing.mp4").is_err());

        let _ = std::fs::remove_file(&inside);
        let _ = std::fs::remove_file(&outside);
    }

    #[test]
    fn short_takes_the_tail_of_a_url() {
        assert_eq!(short("https://example.com/watch/abc"), "abc");
        assert_eq!(short("https://example.com/watch/abc/"), "abc");
    }
}
