//! Haul 的命令列介面。
//!
//! 設計重點是**離開碼**：0 代表每個項目都下載完成且通過驗證閘門，也就是
//! 檔案確實存在且真的能播。這才是 Haul 相對於直接呼叫 yt-dlp 的價值，
//! 也是 agent 唯一需要判讀的東西。
//!
//! `--json` 會把每個狀態變化印成一行 NDJSON，方便程式解析。

use haul_core::{
    default_bin_dir, default_log_dir, default_out_dir, load_history, log as hlog, Config, Engine,
    Event, Item, Mode, Options,
};
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

const HELP: &str = r#"haul — 萬用媒體下載器

用法
  haul <網址>...            下載並驗證，全部成功才回傳 0
  haul status               列出歷史紀錄
  haul logs                 看執行紀錄（診斷失敗用）
  haul update               更新 yt-dlp（站點改版後用）

選項
  -a, --audio               只要聲音（抽出原始音軌，不重新編碼）
  -i, --image               只要封面圖／縮圖
  -q, --quality <N|best>    影片畫質上限，例如 1080（預設不設限）
      --any                 連網頁本身也存下來（預設拒絕，避免假成功）
      --overwrite           目標檔案已存在時照樣重抓（預設跳過）
      --cookies <瀏覽器>    用該瀏覽器的登入狀態（chrome/firefox/safari/edge…）
                            需要登入的內容用這個。Haul 不碰帳密，只讀 cookie。
  -o, --out <資料夾>        輸出位置（預設 ~/Downloads/Haul）
  -c, --concurrency <N>     同時下載幾個（預設 3）
  -n, --lines <N>           logs 要看幾則（預設 50）
      --json                每個事件一行 NDJSON 到 stdout
  -h, --help                顯示這則說明

離開碼
  0  全部下載完成且通過驗證
  1  有項目失敗
  2  用法錯誤，或準備 yt-dlp / ffmpeg 失敗

範例
  haul https://example.com/watch/abc
  haul -a --json https://example.com/playlist/xyz
  haul status --json | jq 'select(.status == "done") | .path'
  haul logs --json | jq 'select(.level == "error")'
  haul --cookies chrome https://example.com/private/video
"#;

enum Cmd {
    Get,
    Status,
    Logs,
    Update,
    Help,
}

struct Args {
    cmd: Cmd,
    urls: Vec<String>,
    mode: Mode,
    any: bool,
    overwrite: bool,
    cookies_from: Option<String>,
    max_height: Option<u32>,
    out: PathBuf,
    json: bool,
    concurrency: usize,
    lines: usize,
}

fn parse() -> Result<Args, String> {
    let mut a = Args {
        cmd: Cmd::Get,
        urls: Vec::new(),
        mode: Mode::Video,
        any: false,
        overwrite: false,
        cookies_from: None,
        max_height: None,
        out: default_out_dir(),
        json: false,
        concurrency: 3,
        lines: 50,
    };

    let mut it = std::env::args().skip(1).peekable();
    let mut first = true;

    while let Some(arg) = it.next() {
        match arg.as_str() {
            "-h" | "--help" => a.cmd = Cmd::Help,
            "-a" | "--audio" => a.mode = Mode::Audio,
            "-i" | "--image" => a.mode = Mode::Image,
            "--any" => a.any = true,
            "--overwrite" => a.overwrite = true,
            "--cookies" => {
                let b = it.next().ok_or("--cookies 後面要接瀏覽器名稱")?;
                if !haul_core::cookies::is_supported(&b) {
                    return Err(format!(
                        "不認得的瀏覽器：{b}（支援 {}）",
                        haul_core::cookies::BROWSERS.join("、")
                    ));
                }
                a.cookies_from = Some(b);
            }
            "-q" | "--quality" => {
                let v = it.next().ok_or("--quality 後面要接數字或 best")?;
                a.max_height = if v.eq_ignore_ascii_case("best") {
                    None
                } else {
                    Some(
                        v.trim_end_matches('p')
                            .parse()
                            .map_err(|_| "--quality 要接數字（例如 1080）或 best".to_string())?,
                    )
                };
            }
            "--json" => a.json = true,
            "-o" | "--out" => {
                a.out = it
                    .next()
                    .map(PathBuf::from)
                    .ok_or("--out 後面要接資料夾路徑")?;
            }
            "-c" | "--concurrency" => {
                a.concurrency = it
                    .next()
                    .ok_or("--concurrency 後面要接數字")?
                    .parse()
                    .map_err(|_| "--concurrency 要接數字".to_string())?;
                if a.concurrency == 0 {
                    return Err("--concurrency 至少要是 1".into());
                }
            }
            "-n" | "--lines" => {
                a.lines = it
                    .next()
                    .ok_or("--lines 後面要接數字")?
                    .parse()
                    .map_err(|_| "--lines 要接數字".to_string())?;
            }
            "status" if first => a.cmd = Cmd::Status,
            "logs" if first => a.cmd = Cmd::Logs,
            "update" if first => a.cmd = Cmd::Update,
            s if s.starts_with("http") => a.urls.push(s.to_string()),
            s => return Err(format!("看不懂的參數：{s}")),
        }
        first = false;
    }
    Ok(a)
}

/// 人類看的簡短敘述
fn describe(i: &Item) -> Option<String> {
    let mb = |n: u64| format!("{:.1} MB", n as f64 / 1_048_576.0);
    Some(match i.status.as_str() {
        "resolving" => format!("[{}] 解析中 {}", i.id, i.title),
        "downloading" if i.total > 0 => format!(
            "[{}] 下載中 {} — {} / {}",
            i.id,
            i.title,
            mb(i.bytes),
            mb(i.total)
        ),
        "verifying" => format!("[{}] 驗證中 {}", i.id, i.title),
        // 帶上驗證等級 —— 不同型別能做到的檢查強度差很多，攤開來講
        // 才不會讓人以為每個「成功」都代表同樣的保證
        "done" => format!(
            "[{}] ✓ {}{}  [{}]",
            i.id,
            i.file.clone().unwrap_or_else(|| i.title.clone()),
            i.secs.map(|s| format!("  {:.0}s", s)).unwrap_or_default(),
            i.verified.as_deref().unwrap_or("?")
        ),
        "failed" => format!(
            "[{}] ✗ {} — {}",
            i.id,
            i.title,
            i.error.clone().unwrap_or_default()
        ),
        // queued 與沒有總長度的 downloading 不值得單獨一行
        _ => return None,
    })
}

#[tokio::main]
async fn main() -> ExitCode {
    let args = match parse() {
        Ok(a) => a,
        Err(e) => {
            eprintln!("錯誤：{e}\n\n{HELP}");
            return ExitCode::from(2);
        }
    };

    match args.cmd {
        Cmd::Help => {
            println!("{HELP}");
            return ExitCode::SUCCESS;
        }
        Cmd::Status => return status(&args),
        Cmd::Logs => return logs(&args),
        Cmd::Update => return update(&args).await,
        Cmd::Get => {}
    }

    if args.urls.is_empty() {
        eprintln!("錯誤：沒有給任何網址\n\n{HELP}");
        return ExitCode::from(2);
    }

    // 事件輸出。JSON 走 stdout（給程式讀），人類敘述走 stderr，
    // 這樣 `haul --json ... > out.jsonl` 仍看得到進度。
    let json = args.json;
    let failed = Arc::new(AtomicUsize::new(0));
    let done = Arc::new(AtomicUsize::new(0));
    let (f2, d2) = (failed.clone(), done.clone());

    let sink: haul_core::Sink = Arc::new(move |ev: Event| {
        // 先計數再決定怎麼印。曾經把這段放在 --json 的提早 return 後面，
        // 結果 --json 模式下失敗永遠不算數、離開碼永遠是 0 —— 正好是
        // 給 agent 用的那條路徑，離開碼是它唯一相信的東西。
        if let Event::Item(i) = &ev {
            match i.status.as_str() {
                "done" => {
                    d2.fetch_add(1, Ordering::Relaxed);
                }
                "failed" => {
                    f2.fetch_add(1, Ordering::Relaxed);
                }
                _ => {}
            }
        }
        if json {
            if let Ok(line) = serde_json::to_string(&ev) {
                println!("{line}");
            }
            return;
        }
        match &ev {
            Event::Setup { tool, bytes, total } if *total > 0 => {
                eprint!(
                    "\r首次啟動，正在取得 {tool} {:.0}%   ",
                    *bytes as f64 / *total as f64 * 100.0
                );
            }
            Event::SetupDone => eprintln!("\r工具已就緒                    "),
            Event::SetupFailed { error } => eprintln!("\r準備工具失敗：{error}"),
            Event::Item(i) => {
                if let Some(line) = describe(i) {
                    eprintln!("{line}");
                }
            }
            _ => {}
        }
    });

    let mut cfg = Config::new(args.out.clone(), default_bin_dir());
    cfg.max_downloads = args.concurrency;
    cfg.allow_html = args.any;
    cfg.overwrite = args.overwrite;
    cfg.cookies_from = args.cookies_from.clone();

    let eng = match Engine::new(cfg, sink) {
        Ok(e) => e,
        Err(e) => {
            eprintln!("錯誤：{e}");
            return ExitCode::from(2);
        }
    };

    if let Err(e) = eng.tools().await {
        eprintln!("錯誤：準備 yt-dlp / ffmpeg 失敗：{e}");
        return ExitCode::from(2);
    }

    let mode = args.mode;
    let opts = Options {
        max_height: args.max_height,
        ..Default::default()
    };

    // 每個輸入各自並行解析，否則清單頁的解析會把後面的輸入卡住
    let mut outer = Vec::new();
    for url in args.urls {
        let e = eng.clone();
        let opts = opts.clone();
        outer.push(tokio::spawn(async move {
            for h in e.add(url, mode, opts).await {
                let _ = h.await;
            }
        }));
    }
    for h in outer {
        let _ = h.await;
    }

    let (ok, bad) = (done.load(Ordering::Relaxed), failed.load(Ordering::Relaxed));
    if !json {
        eprintln!("\n完成 {ok}，失敗 {bad}  →  {}", args.out.display());
    }

    if bad > 0 {
        ExitCode::from(1)
    } else {
        ExitCode::SUCCESS
    }
}

/// 讀歷史檔。不需要 GUI 在跑，也不需要任何 IPC —— 狀態檔本身就是介面。
fn status(args: &Args) -> ExitCode {
    let path = args.out.join(".haul-history.jsonl");
    let items = load_history(&path, usize::MAX);

    if args.json {
        for i in &items {
            if let Ok(line) = serde_json::to_string(i) {
                println!("{line}");
            }
        }
        return ExitCode::SUCCESS;
    }

    if items.is_empty() {
        println!("還沒有任何紀錄（{}）", path.display());
        return ExitCode::SUCCESS;
    }
    for i in &items {
        if let Some(line) = describe(i) {
            println!("{line}");
        }
    }
    let ok = items.iter().filter(|i| i.status == "done").count();
    println!("\n共 {} 筆，其中 {ok} 筆可播", items.len());
    ExitCode::SUCCESS
}

/// 看執行紀錄。跟 status 一樣，讀的就是檔案本身，不需要任何 daemon。
fn logs(args: &Args) -> ExitCode {
    let path = default_log_dir().join("haul.log");
    let entries = hlog::tail(&path, args.lines);

    if args.json {
        for e in &entries {
            if let Ok(line) = serde_json::to_string(e) {
                println!("{line}");
            }
        }
        return ExitCode::SUCCESS;
    }

    if entries.is_empty() {
        println!("還沒有任何紀錄（{}）", path.display());
        return ExitCode::SUCCESS;
    }
    for e in &entries {
        let detail = if e.data.is_null() {
            String::new()
        } else {
            format!("  {}", e.data)
        };
        println!("{}  {:<5} {}{}", e.time, e.level, e.event, detail);
    }
    ExitCode::SUCCESS
}

async fn update(args: &Args) -> ExitCode {
    let sink: haul_core::Sink = Arc::new(|_| {});
    let cfg = Config::new(args.out.clone(), default_bin_dir());
    let eng = match Engine::new(cfg, sink) {
        Ok(e) => e,
        Err(e) => {
            eprintln!("錯誤：{e}");
            return ExitCode::from(2);
        }
    };
    match eng.update_tools().await {
        Ok(msg) => {
            println!(
                "{}",
                if msg.is_empty() {
                    "yt-dlp 已是最新版"
                } else {
                    &msg
                }
            );
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("更新失敗：{e}");
            ExitCode::from(2)
        }
    }
}
