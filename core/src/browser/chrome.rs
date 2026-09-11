//! 找到並啟動一個 Haul 專用的 Chrome / Chromium / Edge / Brave 實例。

use anyhow::{anyhow, bail, Result};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};
use tokio::process::{Child, Command};

/// 各平台慣用的安裝位置，依偏好排序。使用者可以在設定裡覆寫。
pub fn candidates() -> Vec<PathBuf> {
    let mut v = Vec::new();
    #[cfg(target_os = "macos")]
    {
        for app in [
            "Google Chrome",
            "Chromium",
            "Microsoft Edge",
            "Brave Browser",
        ] {
            v.push(PathBuf::from(format!(
                "/Applications/{app}.app/Contents/MacOS/{app}"
            )));
        }
    }
    #[cfg(target_os = "windows")]
    {
        let roots = [
            std::env::var("PROGRAMFILES").unwrap_or_else(|_| r"C:\Program Files".into()),
            std::env::var("PROGRAMFILES(X86)")
                .unwrap_or_else(|_| r"C:\Program Files (x86)".into()),
            std::env::var("LOCALAPPDATA").unwrap_or_default(),
        ];
        for root in roots {
            for rel in [
                r"Google\Chrome\Application\chrome.exe",
                r"Chromium\Application\chrome.exe",
                r"Microsoft\Edge\Application\msedge.exe",
                r"BraveSoftware\Brave-Browser\Application\brave.exe",
            ] {
                v.push(Path::new(&root).join(rel));
            }
        }
    }
    #[cfg(all(not(target_os = "macos"), not(target_os = "windows")))]
    {
        for name in [
            "google-chrome",
            "chromium",
            "chromium-browser",
            "microsoft-edge",
            "brave-browser",
        ] {
            v.push(PathBuf::from(format!("/usr/bin/{name}")));
        }
    }
    v
}

/// 找瀏覽器。指定了路徑就只認那個：設了卻沒生效比找不到更糟。
pub fn find(override_path: Option<&Path>) -> Result<PathBuf> {
    if let Some(p) = override_path {
        if p.is_file() {
            return Ok(p.to_path_buf());
        }
        bail!("設定裡指定的瀏覽器不存在：{}", p.display());
    }
    candidates()
        .into_iter()
        .find(|p| p.is_file())
        .ok_or_else(|| {
            anyhow!("找不到 Chrome / Chromium / Edge / Brave。裝一個，或在設定裡指定路徑")
        })
}

/// `DevToolsActivePort` 是兩行：port 與 WebSocket 路徑。
pub fn parse_active_port(text: &str) -> Option<(u16, String)> {
    let mut lines = text.lines();
    let port: u16 = lines.next()?.trim().parse().ok()?;
    let path = lines.next()?.trim();
    if path.is_empty() {
        return None;
    }
    Some((port, path.to_string()))
}

/// 一個活著的瀏覽器實例。Drop 時殺掉——Haul 退出不該留一個孤兒 Chrome。
pub struct Chrome {
    child: Child,
    pub ws_url: String,
}

/// 啟動的參數。`--remote-debugging-port=0` 讓 Chrome 自己挑 port，
/// 從 user-data-dir 的 DevToolsActivePort 讀回來；不用 pipe 是因為 Rust 的
/// std::process 在 Windows 上沒辦法乾淨地傳 fd 3/4。
pub fn launch_args(data_dir: &Path) -> Vec<String> {
    vec![
        format!("--user-data-dir={}", data_dir.display()),
        "--remote-debugging-port=0".into(),
        "--no-first-run".into(),
        "--no-default-browser-check".into(),
        // 很多播放器要按了才載入媒體；允許自動播放讓偵測不必等使用者
        "--autoplay-policy=no-user-gesture-required".into(),
        "about:blank".into(),
    ]
}

const BOOT_TIMEOUT: Duration = Duration::from_secs(10);

/// 啟動並等到 DevTools 可以連。
pub async fn launch(exe: &Path, data_dir: &Path) -> Result<Chrome> {
    tokio::fs::create_dir_all(data_dir).await?;
    let port_file = data_dir.join("DevToolsActivePort");
    // 上一次留下的檔案會讓我們連到一個已經不存在的 port
    let _ = tokio::fs::remove_file(&port_file).await;

    let child = Command::new(exe)
        .args(launch_args(data_dir))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| anyhow!("啟動瀏覽器失敗：{e}"))?;

    let start = Instant::now();
    loop {
        if let Ok(text) = tokio::fs::read_to_string(&port_file).await {
            if let Some((port, path)) = parse_active_port(&text) {
                return Ok(Chrome {
                    child,
                    ws_url: format!("ws://127.0.0.1:{port}{path}"),
                });
            }
        }
        if start.elapsed() > BOOT_TIMEOUT {
            bail!("瀏覽器啟動後 {} 秒內沒有回應", BOOT_TIMEOUT.as_secs());
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

impl Chrome {
    /// 程序還在嗎。瀏覽器被使用者整個關掉時，偵測中的項目要失敗而不是等逾時。
    pub fn alive(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(None))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_the_two_line_devtools_file() {
        let (port, path) = parse_active_port("54321\n/devtools/browser/abc-123\n").unwrap();
        assert_eq!(port, 54321);
        assert_eq!(path, "/devtools/browser/abc-123");
    }

    #[test]
    fn rejects_half_written_devtools_file() {
        // Chrome 先寫 port 再寫路徑，中間讀到會只有一行
        assert!(parse_active_port("54321\n").is_none());
        assert!(parse_active_port("").is_none());
        assert!(parse_active_port("abc\n/x").is_none());
    }

    #[test]
    fn override_path_wins_but_must_exist() {
        let missing = Path::new("/definitely/not/here");
        // 使用者指定了一個不存在的路徑：不能默默退回自動偵測，
        // 那會讓「設了沒生效」無聲無息
        assert!(find(Some(missing)).is_err());
    }

    /// 需要機器上有 Chrome。CI 不設 HAUL_TEST_CHROME，跳過。
    #[tokio::test]
    async fn launches_and_answers_over_cdp() {
        if std::env::var("HAUL_TEST_CHROME").is_err() {
            eprintln!("略過：未設 HAUL_TEST_CHROME");
            return;
        }
        let exe = find(None).unwrap();
        let dir = std::env::temp_dir().join("haul-chrome-test");
        let chrome = launch(&exe, &dir).await.unwrap();
        let cdp = super::super::cdp::Cdp::connect(&chrome.ws_url).await.unwrap();
        let v = cdp
            .call(None, "Browser.getVersion", serde_json::json!({}))
            .await
            .unwrap();
        let product = v["product"].as_str().unwrap();
        assert!(product.contains('/'), "{product}");
        let _ = cdp.call(None, "Browser.close", serde_json::json!({})).await;
    }
}
