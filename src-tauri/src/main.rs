#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

//! Haul 的圖形介面外殼。
//!
//! 所有下載邏輯都在 haul-core，這裡只做兩件事：把引擎事件轉成 Tauri event
//! 推給 webview，以及把 webview 的指令轉成引擎呼叫。CLI 是同一個引擎的
//! 另一個外殼，兩邊行為不會分岔。

use haul_core::settings::Settings;
use haul_core::{
    default_bin_dir, default_out_dir, open_with_system, reveal_in_folder, validate_playable,
    Config, Engine, Event, Item, Mode, Options,
};
use serde::Serialize;
use std::path::PathBuf;
use std::sync::Arc;
use tauri::{AppHandle, Emitter, Manager, State};

/// 前端 setup 事件的形狀。核心的事件是三個分開的變體，這裡壓成
/// 一個帶 stage 的物件，前端只要一個 listener。
#[derive(Clone, Serialize, Default)]
#[serde(rename_all = "camelCase")]
struct SetupEvent {
    /// progress | done | failed
    stage: String,
    tool: String,
    bytes: u64,
    total: u64,
    error: Option<String>,
}

fn engine(app: &AppHandle) -> Arc<Engine> {
    app.state::<Arc<Engine>>().inner().clone()
}

#[tauri::command]
fn add(
    app: AppHandle,
    text: String,
    mode: String,
    quality: Option<u32>,
    cookies: Option<String>,
) -> Result<usize, String> {
    let inputs: Vec<String> = text
        .split(|c: char| c.is_whitespace())
        .map(str::trim)
        .filter(|s| s.starts_with("http"))
        .map(|s| s.trim_end_matches([')', ']', ',', '。']).to_string())
        .collect();

    if inputs.is_empty() {
        return Err("沒看到網址。貼上影片或音樂的連結，一行一個。".into());
    }

    let mode = Mode::parse(&mode);
    let opts = Options {
        max_height: quality,
        ..Default::default()
    };
    let eng = engine(&app);

    if let Some(b) = cookies.as_deref() {
        if !haul_core::cookies::is_supported(b) {
            return Err(format!("不認得的瀏覽器：{b}"));
        }
    }
    eng.set_cookies_from(cookies);
    let n = inputs.len();
    for input in inputs {
        let e = eng.clone();
        let opts = opts.clone();
        tauri::async_runtime::spawn(async move {
            e.add(input, mode, opts).await;
        });
    }
    Ok(n)
}

#[tauri::command]
fn snapshot(state: State<'_, Arc<Engine>>) -> Vec<Item> {
    state.snapshot()
}

#[tauri::command]
fn out_dir(state: State<'_, Arc<Engine>>) -> String {
    state.out_dir().to_string_lossy().to_string()
}

#[tauri::command]
fn open_out_dir(state: State<'_, Arc<Engine>>) -> Result<(), String> {
    let dir = state.out_dir();
    std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    open_with_system(dir)
}

/// 用系統預設播放器開啟一個下載好的檔案。路徑來自前端，所以要先驗證
/// 它確實落在輸出資料夾底下。
#[tauri::command]
fn open_file(state: State<'_, Arc<Engine>>, path: String) -> Result<(), String> {
    let target = validate_playable(state.out_dir(), &path)?;
    open_with_system(&target)
}

/// 在 Finder／檔案總管裡選取一個下載好的檔案。路徑同樣要先驗證。
#[tauri::command]
fn reveal_file(state: State<'_, Arc<Engine>>, path: String) -> Result<(), String> {
    reveal_in_folder(state.out_dir(), &path)
}

/// 完成項目的縮圖，base64 JPEG。前端只傳 id，不傳路徑，也不開 asset protocol。
/// Ok(None) = 這個項目沒有縮圖（無封面的音樂、PDF）；Err = 原檔已不在等。
#[tauri::command]
async fn thumb(state: State<'_, Arc<Engine>>, id: u64) -> Result<Option<String>, String> {
    use base64::Engine as _;
    let Some(p) = state.ensure_thumb(id).await? else {
        return Ok(None);
    };
    let bytes = std::fs::read(&p).map_err(|e| e.to_string())?;
    Ok(Some(base64::engine::general_purpose::STANDARD.encode(bytes)))
}

#[tauri::command]
fn clear_done(state: State<'_, Arc<Engine>>) -> Vec<Item> {
    state.clear_finished()
}

/// 把一個項目從列表拿掉；還沒完成的會先取消。只動列表不動檔案。
#[tauri::command]
fn remove_item(state: State<'_, Arc<Engine>>, id: u64) -> Result<(), String> {
    state.remove(id)
}

/// 失敗的項目重來一次，同一列同一個 id。async 的理由同 retry_with_browser。
#[tauri::command]
#[allow(clippy::unused_async)]
async fn retry_item(app: AppHandle, id: u64, quality: Option<u32>) -> Result<(), String> {
    engine(&app).start_retry(
        id,
        Options {
            max_height: quality,
            ..Default::default()
        },
    )
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct UpdateInfo {
    version: String,
    body: Option<String>,
}

/// 問 GitHub Releases 有沒有新版。沒網路、dev 建置沒有 release 都會 Err，
/// 啟動時的自動檢查把它吞掉，設定面板手動按的才顯示。
#[tauri::command]
async fn check_update(app: AppHandle) -> Result<Option<UpdateInfo>, String> {
    use tauri_plugin_updater::UpdaterExt;
    let u = app
        .updater()
        .map_err(|e| e.to_string())?
        .check()
        .await
        .map_err(|e| e.to_string())?;
    Ok(u.map(|u| UpdateInfo {
        version: u.version.clone(),
        body: u.body.clone(),
    }))
}

/// 下載、覆蓋安裝、重新啟動。進度用 `update` 事件送回前端。
/// 檢查與安裝都在這一端做，webview 不需要任何 updater 權限。
#[tauri::command]
async fn install_update(app: AppHandle) -> Result<(), String> {
    use tauri_plugin_updater::UpdaterExt;
    let Some(u) = app
        .updater()
        .map_err(|e| e.to_string())?
        .check()
        .await
        .map_err(|e| e.to_string())?
    else {
        return Err("已是最新版".into());
    };
    let h = app.clone();
    let mut got: u64 = 0;
    u.download_and_install(
        move |chunk, total| {
            got += chunk as u64;
            let _ = h.emit("update", serde_json::json!({ "bytes": got, "total": total }));
        },
        || {},
    )
    .await
    .map_err(|e| e.to_string())?;
    app.restart()
}

/// 讓 yt-dlp 自我更新。站點改版時靠這個跟上，不必等 Haul 重新發布。
#[tauri::command]
async fn update_tools(app: AppHandle) -> Result<String, String> {
    engine(&app).update_tools().await
}

/// 萃取失敗的項目改用瀏覽器抓。偵測與下載都很久，不等它，狀態走事件回來。
///
/// async：引擎要 spawn 任務，得在 tokio runtime 上。同步 command 跑在主執行緒，
/// tokio::spawn 會 panic 然後整個 app abort（實測過）。
#[tauri::command]
#[allow(clippy::unused_async)]
async fn retry_with_browser(app: AppHandle, id: u64) -> Result<(), String> {
    // 由引擎 spawn 並記下把手，使用者移除這個項目時才取消得掉
    engine(&app).start_retry_with_browser(id)
}

/// 讀目前設定（給面板初始化）
#[tauri::command]
fn get_settings(path: State<'_, PathBuf>) -> Settings {
    Settings::load(&path)
}

/// 存設定並套用能即時套用的部分。輸出資料夾與同時下載數要重新啟動才生效。
#[tauri::command]
fn save_settings(app: AppHandle, path: State<'_, PathBuf>, settings: Settings) -> Result<(), String> {
    settings.save(&path).map_err(|e| e.to_string())?;
    let eng = engine(&app);
    eng.set_cookies_from(settings.cookies_from.clone());
    eng.set_record_max(settings.record_max_secs);
    eng.set_browser_path(settings.browser_path.clone());
    Ok(())
}

/// 讓使用者用原生對話框挑一個音檔，回傳路徑（取消回 None）。
/// 走系統的選擇器而不是引入 dialog plugin，延續 open / xdg-open 的作法。
#[tauri::command]
fn pick_audio_file() -> Option<String> {
    native_pick_audio()
}

/// 讓使用者用原生對話框挑一個資料夾（輸出位置用），取消回 None。
#[tauri::command]
fn pick_folder() -> Option<String> {
    native_pick_folder()
}

/// 驗證一個音檔真的能解碼（選到壞檔當場知道，不必等佇列跑完）
#[tauri::command]
async fn verify_audio(path: String) -> Result<(), String> {
    let p = PathBuf::from(&path);
    if !p.is_file() {
        return Err("找不到這個檔案".into());
    }
    tauri::async_runtime::spawn_blocking(move || haul_core::verify::verify(&p))
        .await
        .map_err(|e| e.to_string())?
        .map(|_| ())
        .map_err(|e| format!("這個檔案不能當提示音：{e}"))
}

/// 讀音檔的位元組回前端播放（base64）。不開 asset protocol 白名單。
#[tauri::command]
fn read_audio(path: String) -> Result<String, String> {
    use base64::Engine as _;
    let bytes = std::fs::read(&path).map_err(|e| e.to_string())?;
    // 提示音應該很小；設個上限免得有人選了一部電影
    if bytes.len() > 8 * 1024 * 1024 {
        return Err("音檔太大（上限 8 MB）".into());
    }
    Ok(base64::engine::general_purpose::STANDARD.encode(bytes))
}

#[cfg(target_os = "macos")]
fn native_pick_audio() -> Option<String> {
    // AppleScript 的 choose file，限音訊類型
    let script = r#"try
        set f to choose file with prompt "選一個提示音" of type {"mp3","m4a","wav","aiff","aac","ogg"}
        POSIX path of f
    on error
        return ""
    end try"#;
    let out = std::process::Command::new("osascript")
        .args(["-e", script])
        .output()
        .ok()?;
    let path = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (!path.is_empty()).then_some(path)
}

#[cfg(target_os = "windows")]
fn native_pick_audio() -> Option<String> {
    let ps = r#"Add-Type -AssemblyName System.Windows.Forms
$d = New-Object System.Windows.Forms.OpenFileDialog
$d.Filter = 'Audio|*.mp3;*.m4a;*.wav;*.aac;*.ogg'
if ($d.ShowDialog() -eq 'OK') { Write-Output $d.FileName }"#;
    let out = std::process::Command::new("powershell")
        .args(["-NoProfile", "-Command", ps])
        .output()
        .ok()?;
    let path = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (!path.is_empty()).then_some(path)
}

#[cfg(all(not(target_os = "macos"), not(target_os = "windows")))]
fn native_pick_audio() -> Option<String> {
    None // Linux 桌面環境太雜，先讓使用者自己貼路徑
}

#[cfg(target_os = "macos")]
fn native_pick_folder() -> Option<String> {
    let script = r#"try
        set f to choose folder with prompt "選一個輸出資料夾"
        POSIX path of f
    on error
        return ""
    end try"#;
    let out = std::process::Command::new("osascript")
        .args(["-e", script])
        .output()
        .ok()?;
    let path = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (!path.is_empty()).then_some(path)
}

#[cfg(target_os = "windows")]
fn native_pick_folder() -> Option<String> {
    let ps = r#"Add-Type -AssemblyName System.Windows.Forms
$d = New-Object System.Windows.Forms.FolderBrowserDialog
if ($d.ShowDialog() -eq 'OK') { Write-Output $d.SelectedPath }"#;
    let out = std::process::Command::new("powershell")
        .args(["-NoProfile", "-Command", ps])
        .output()
        .ok()?;
    let path = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (!path.is_empty()).then_some(path)
}

#[cfg(all(not(target_os = "macos"), not(target_os = "windows")))]
fn native_pick_folder() -> Option<String> {
    None
}

/// 萃取失敗或偵測不到媒體時，改用錄製。錄製很久，不等它，狀態走事件回來。
#[tauri::command]
#[allow(clippy::unused_async)]
async fn record_item(app: AppHandle, id: u64) -> Result<(), String> {
    engine(&app).start_record_item(id)
}

/// 停止錄製。回 false 表示這個項目沒在錄。
#[tauri::command]
fn stop_recording(app: AppHandle, id: u64) -> bool {
    engine(&app).stop_recording(id)
}

/// 使用者從候選清單挑了另一個：新增一個項目抓它
#[tauri::command]
async fn add_candidate(
    app: AppHandle,
    id: u64,
    candidate: haul_core::browser::sniff::Candidate,
) -> Result<(), String> {
    engine(&app)
        .add_candidate(id, candidate)
        .await
        .map(|_| ())
        .ok_or_else(|| "找不到原始項目，或工具尚未就緒".to_string())
}

fn main() {
    tauri::Builder::default()
        .plugin(tauri_plugin_updater::Builder::new().build())
        .setup(|app| {
            let handle = app.handle().clone();

            // 引擎事件 → Tauri event。webview 需要 item、setup 與 candidates 三種。
            let sink: haul_core::Sink = Arc::new(move |ev: Event| match ev {
                Event::Item(item) => {
                    let _ = handle.emit("item", item);
                }
                Event::Candidates {
                    id,
                    candidates,
                    chosen,
                } => {
                    let _ = handle.emit(
                        "candidates",
                        serde_json::json!({ "id": id, "candidates": candidates, "chosen": chosen }),
                    );
                }
                Event::Setup { tool, bytes, total } => {
                    let _ = handle.emit(
                        "setup",
                        SetupEvent {
                            stage: "progress".into(),
                            tool,
                            bytes,
                            total,
                            error: None,
                        },
                    );
                }
                Event::SetupDone => {
                    let _ = handle.emit(
                        "setup",
                        SetupEvent {
                            stage: "done".into(),
                            ..Default::default()
                        },
                    );
                }
                Event::SetupFailed { error } => {
                    let _ = handle.emit(
                        "setup",
                        SetupEvent {
                            stage: "failed".into(),
                            error: Some(error),
                            ..Default::default()
                        },
                    );
                }
            });

            // app 資料夾（跟 bin/ 平行）放 settings.json
            let app_dir = default_bin_dir()
                .parent()
                .map(|p| p.to_path_buf())
                .unwrap_or_else(default_bin_dir);
            let settings_path = Settings::path_in(&app_dir);
            let settings = Settings::load(&settings_path);
            let cfg = Config::from_settings(&settings, default_out_dir(), default_bin_dir());
            let eng = Engine::new(cfg, sink)?;
            app.manage(eng.clone());
            app.manage(settings_path);

            // 先把工具備好，使用者貼連結時就不用等
            tauri::async_runtime::spawn(async move {
                let _ = eng.tools().await;
            });

            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            add,
            snapshot,
            out_dir,
            open_out_dir,
            open_file,
            reveal_file,
            thumb,
            clear_done,
            remove_item,
            retry_item,
            update_tools,
            check_update,
            install_update,
            retry_with_browser,
            add_candidate,
            record_item,
            stop_recording,
            get_settings,
            save_settings,
            pick_audio_file,
            pick_folder,
            verify_audio,
            read_audio
        ])
        .run(tauri::generate_context!())
        .expect("Tauri 啟動失敗");
}
