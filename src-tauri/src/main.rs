#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod song;
mod verify;

use serde::Serialize;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tauri::{AppHandle, Emitter, Manager, State};
use tokio::sync::Semaphore;

/// 同時下載幾首。3 條夠把頻寬吃滿，又不會把 CDN 惹毛。
const MAX_DOWNLOADS: usize = 3;
/// 同時驗證幾首。解碼吃 CPU，壓在 2 條以免跟使用者搶資源。
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
    /// queued | resolving | downloading | verifying | done | failed
    status: String,
    bytes: u64,
    total: u64,
    secs: Option<f64>,
    file: Option<String>,
    error: Option<String>,
}

struct AppState {
    items: Mutex<Vec<Item>>,
    /// 已經排過的歌曲 id，避免同一首重複下載
    seen: Mutex<HashSet<String>>,
    out_dir: PathBuf,
    client: reqwest::Client,
    dl: Semaphore,
    vf: Semaphore,
    next_id: AtomicU64,
}

impl AppState {
    fn new(out_dir: PathBuf) -> anyhow::Result<Self> {
        Ok(Self {
            items: Mutex::new(Vec::new()),
            seen: Mutex::new(HashSet::new()),
            out_dir,
            client: song::build_client()?,
            dl: Semaphore::new(MAX_DOWNLOADS),
            vf: Semaphore::new(MAX_VERIFIES),
            next_id: AtomicU64::new(1),
        })
    }

    fn push(&self, input: String, title: String) -> Item {
        let item = Item {
            id: self.next_id.fetch_add(1, Ordering::Relaxed),
            input,
            title,
            status: "queued".into(),
            bytes: 0,
            total: 0,
            secs: None,
            file: None,
            error: None,
        };
        self.items.lock().unwrap().push(item.clone());
        item
    }

    /// 改一個項目並回傳改完的副本。鎖只在函式內存活，不跨 await。
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

/// 改狀態 + 推事件給前端，一步完成
fn update(app: &AppHandle, st: &AppState, id: u64, f: impl FnOnce(&mut Item)) {
    if let Some(item) = st.patch(id, f) {
        let _ = app.emit("item", item);
    }
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

    // 壓掉連續空白
    let mut s = replaced.split_whitespace().collect::<Vec<_>>().join(" ");

    // 以字元為單位截斷，不會切壞 UTF-8（中文歌名很重要）
    if s.chars().count() > 110 {
        s = s.chars().take(110).collect();
    }

    // Windows: 檔名結尾不能是點或空白
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
    home_dir().join("Music").join("SunoDL")
}

/// 掃掉上次沒下載完留下的暫存檔
fn sweep_parts(dir: &Path) {
    if let Ok(rd) = std::fs::read_dir(dir) {
        for e in rd.flatten() {
            if e.path().extension().is_some_and(|x| x == "part") {
                let _ = std::fs::remove_file(e.path());
            }
        }
    }
}

// ---------------------------------------------------------------- 主流程

/// 一筆輸入 → 展開成 N 首 → 每首各自排隊
async fn handle_input(app: AppHandle, st: Arc<AppState>, input: String) {
    let probe = st.push(input.clone(), format!("解析中… {}", short(&input)));
    let _ = app.emit("item", probe.clone());
    update(&app, &st, probe.id, |i| i.status = "resolving".into());

    let ids = match song::expand(&st.client, &input).await {
        Ok(ids) => ids,
        Err(e) => {
            update(&app, &st, probe.id, |i| {
                i.status = "failed".into();
                i.title = short(&input);
                i.error = Some(e.to_string());
            });
            return;
        }
    };

    // 第一首沿用這張卡，其餘各開一張，避免清單頁多出一張空卡
    let mut assigned: Vec<(u64, String)> = Vec::new();
    for (n, id) in ids.into_iter().enumerate() {
        if !st.seen.lock().unwrap().insert(id.clone()) {
            continue; // 這首排過了
        }
        if n == 0 {
            update(&app, &st, probe.id, |i| i.title = short(&id));
            assigned.push((probe.id, id));
        } else {
            let it = st.push(song::page_url(&id), short(&id));
            let _ = app.emit("item", it.clone());
            assigned.push((it.id, id));
        }
    }

    if assigned.is_empty() {
        update(&app, &st, probe.id, |i| {
            i.status = "done".into();
            i.title = short(&input);
            i.error = Some("這些歌都已經在佇列裡了".into());
        });
        return;
    }

    for (item_id, song_id) in assigned {
        let app2 = app.clone();
        let st2 = st.clone();
        tauri::async_runtime::spawn(async move { run_song(app2, st2, item_id, song_id).await });
    }
}

fn short(s: &str) -> String {
    let t = s.rsplit('/').next().unwrap_or(s);
    t.chars().take(20).collect()
}

async fn run_song(app: AppHandle, st: Arc<AppState>, id: u64, song_id: String) {
    // 1. 抓歌名（抓不到就用 id，不算失敗）
    update(&app, &st, id, |i| i.status = "resolving".into());
    let title = song::fetch_title(&st.client, &song_id)
        .await
        .unwrap_or_else(|| song_id.clone());
    update(&app, &st, id, |i| i.title = title.clone());

    // 2. 下載（限流）
    let permit = match st.dl.acquire().await {
        Ok(p) => p,
        Err(_) => return,
    };
    update(&app, &st, id, |i| i.status = "downloading".into());

    let part = st.out_dir.join(format!(".{song_id}.mp3.part"));
    let url = song::audio_url(&song_id);

    let mut last = Instant::now() - PROGRESS_EVERY;
    let dl = song::download(&st.client, &url, &part, |got, total| {
        if last.elapsed() >= PROGRESS_EVERY || (total > 0 && got >= total) {
            last = Instant::now();
            update(&app, &st, id, |i| {
                i.bytes = got;
                i.total = total;
            });
        }
    })
    .await;

    drop(permit); // 讓下一首開始下載，驗證走另一條隊

    if let Err(e) = dl {
        let _ = tokio::fs::remove_file(&part).await;
        update(&app, &st, id, |i| {
            i.status = "failed".into();
            i.error = Some(e.to_string());
        });
        return;
    }

    // 3. 驗證能播（限流，CPU 密集所以丟到 blocking 執行緒）
    let _vp = match st.vf.acquire().await {
        Ok(p) => p,
        Err(_) => return,
    };
    update(&app, &st, id, |i| i.status = "verifying".into());

    let probe_path = part.clone();
    let verdict = tokio::task::spawn_blocking(move || verify::verify(&probe_path)).await;

    let ok = match verdict {
        Ok(Ok(v)) => v,
        Ok(Err(e)) => {
            let _ = tokio::fs::remove_file(&part).await;
            update(&app, &st, id, |i| {
                i.status = "failed".into();
                i.error = Some(format!("驗證未通過：{e}"));
            });
            return;
        }
        Err(e) => {
            update(&app, &st, id, |i| {
                i.status = "failed".into();
                i.error = Some(format!("驗證程序異常：{e}"));
            });
            return;
        }
    };

    // 4. 過關才搬進正式資料夾
    let dest = unique_path(&st.out_dir, &sanitize(&title), "mp3");
    if let Err(e) = tokio::fs::rename(&part, &dest).await {
        update(&app, &st, id, |i| {
            i.status = "failed".into();
            i.error = Some(format!("搬移失敗：{e}"));
        });
        return;
    }

    let name = dest
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_default();
    update(&app, &st, id, |i| {
        i.status = "done".into();
        i.secs = Some(ok.secs);
        i.file = Some(name);
        i.error = None;
    });
}

// ---------------------------------------------------------------- 指令

#[tauri::command]
fn add(app: AppHandle, text: String) -> Result<usize, String> {
    let inputs = song::split_inputs(&text);
    if inputs.is_empty() {
        return Err("沒看到網址。貼 suno.com/song/… 的連結，一行一個。".into());
    }
    let st = state_of(&app);
    let n = inputs.len();
    for input in inputs {
        let app2 = app.clone();
        let st2 = st.clone();
        tauri::async_runtime::spawn(async move { handle_input(app2, st2, input).await });
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

#[tauri::command]
fn clear_done(state: State<'_, Arc<AppState>>) -> Vec<Item> {
    let mut guard = state.items.lock().unwrap();
    guard.retain(|i| i.status != "done" && i.status != "failed");
    guard.clone()
}

// ---------------------------------------------------------------- 進入點

fn main() {
    tauri::Builder::default()
        .setup(|app| {
            let out = default_out_dir();
            std::fs::create_dir_all(&out)?;
            sweep_parts(&out);
            app.manage(Arc::new(AppState::new(out)?));
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            add,
            snapshot,
            out_dir,
            open_out_dir,
            clear_done
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
        assert_eq!(sanitize("COM1.mp3"), "_COM1.mp3");
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
    fn parses_song_id_from_url() {
        let id = "0b5ca1de-1234-4abc-89ef-0123456789ab";
        assert_eq!(
            song::song_id_from_input(&format!("https://suno.com/song/{id}")).as_deref(),
            Some(id)
        );
        assert_eq!(song::song_id_from_input(id).as_deref(), Some(id));
        assert_eq!(song::song_id_from_input("https://suno.com/explore"), None);
    }

    #[test]
    fn splits_pasted_text_into_inputs() {
        let text = "https://suno.com/song/aaaaaaaa-1111-4222-8333-444444444444\n\
                    垃圾字串\n\
                    https://suno.com/song/bbbbbbbb-1111-4222-8333-444444444444/";
        let got = song::split_inputs(text);
        assert_eq!(got.len(), 2);
        assert!(got[1].ends_with("444444444444"));
    }
}
