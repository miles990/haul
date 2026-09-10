#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

//! Haul 的圖形介面外殼。
//!
//! 所有下載邏輯都在 haul-core，這裡只做兩件事：把引擎事件轉成 Tauri event
//! 推給 webview，以及把 webview 的指令轉成引擎呼叫。CLI 是同一個引擎的
//! 另一個外殼，兩邊行為不會分岔。

use haul_core::{
    default_bin_dir, default_out_dir, open_with_system, validate_playable, Config, Engine, Event,
    Item, Mode,
};
use serde::Serialize;
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
fn add(app: AppHandle, text: String, mode: String) -> Result<usize, String> {
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
    let eng = engine(&app);
    let n = inputs.len();
    for input in inputs {
        let e = eng.clone();
        tauri::async_runtime::spawn(async move {
            e.add(input, mode).await;
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

#[tauri::command]
fn clear_done(state: State<'_, Arc<Engine>>) -> Vec<Item> {
    state.clear_finished()
}

/// 讓 yt-dlp 自我更新。站點改版時靠這個跟上，不必等 Haul 重新發布。
#[tauri::command]
async fn update_tools(app: AppHandle) -> Result<String, String> {
    engine(&app).update_tools().await
}

fn main() {
    tauri::Builder::default()
        .setup(|app| {
            let handle = app.handle().clone();

            // 引擎事件 → Tauri event。webview 只需要 item 與 setup 兩種。
            let sink: haul_core::Sink = Arc::new(move |ev: Event| match ev {
                Event::Item(item) => {
                    let _ = handle.emit("item", item);
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

            let eng = Engine::new(Config::new(default_out_dir(), default_bin_dir()), sink)?;
            app.manage(eng.clone());

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
            clear_done,
            update_tools
        ])
        .run(tauri::generate_context!())
        .expect("Tauri 啟動失敗");
}
