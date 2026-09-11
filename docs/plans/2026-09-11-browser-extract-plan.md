# 瀏覽器輔助萃取 實作計畫（第 1 期）

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** 前三層抓不到時，讓一個 Haul 啟動的 Chrome 把頁面跑起來、攔截它發出的媒體請求，把網址＋header 交回現有的下載與驗證流程。

**Architecture:** 新模組 `core/src/browser/`（`chrome.rs` 找與啟動、`cdp.rs` 最小 CDP 客戶端、`sniff.rs` 候選與停止規則）是第四條解析路徑，跟 `extract.rs` / `gallery.rs` / `direct.rs` 平行——只回答「這頁上有什麼」。引擎多一個 `Job::Browser`，`extract::download` 與 `direct::download` 學會帶自訂 header。GUI 在萃取失敗的列上出現「用瀏覽器抓」，CLI 用 `--browser` 允許自動落到這層。設計見 `docs/plans/2026-09-11-browser-layer-design.md`。

**Tech Stack:** Rust（tokio、tokio-tungstenite 不開 TLS、serde_json、base64）、Chrome DevTools Protocol（Target / Network / Page / Runtime / Browser）、Tauri 2、單檔 HTML 前端。

**慣例：** 測試用 `cargo test -p haul-core <名稱>`；每個任務結束 commit；註解與訊息用繁體中文，寫「為什麼」不寫「做什麼」，密度跟周圍程式碼一致；不引入 chromiumoxide 這類綁 Chrome 版本的大依賴。

**一個簡化，明講：** 「換一個」在第 1 期是**用另一個候選新增一個項目**，不是取消正在跑的那個——引擎目前沒有取消機制，那是獨立功能。

---

### Task 1: 相依與模組骨架

**Files:**
- Modify: `Cargo.toml`（workspace.dependencies）
- Modify: `core/Cargo.toml`
- Create: `core/src/browser/mod.rs`
- Modify: `core/src/lib.rs`

**Step 1: 加相依**

`Cargo.toml` 的 `[workspace.dependencies]` 加：

```toml
# 只連 127.0.0.1 的 DevTools socket，不需要 TLS
tokio-tungstenite = { version = "0.24", default-features = false, features = ["connect"] }
base64 = "0.22"
```

`core/Cargo.toml` 的 `[dependencies]` 加：

```toml
tokio-tungstenite.workspace = true
base64.workspace = true
```

**Step 2: 模組骨架**

`core/src/browser/mod.rs`：

```rust
//! 第四條解析路徑：讓一個 Haul 自己啟動的 Chrome 把頁面跑起來，攔截它發出的
//! 媒體請求。跟 extract / gallery / direct 一樣只回答「這頁上有什麼」，
//! 下載與驗證仍走引擎。
//!
//! 為什麼不接管使用者正在跑的 Chrome：Chrome 136 起禁止對預設 profile 開
//! remote debugging。所以是獨立實例、獨立 profile（放在 app 資料夾），
//! 使用者在裡面登入過的站會保留。

pub mod cdp;
pub mod chrome;
pub mod sniff;
```

`core/src/lib.rs` 在 `pub mod cookies;` 前加 `pub mod browser;`。

**Step 3: 建置**

Run: `cargo build -p haul-core`
Expected: 成功（三個子模組檔案先各放一行 `//! TODO`，Task 2–8 填內容）。

**Step 4: Commit**

```bash
git add Cargo.toml Cargo.lock core/Cargo.toml core/src/browser core/src/lib.rs
git commit -m "瀏覽器層：模組骨架與相依"
```

---

### Task 2: chrome.rs — 找可執行檔、解析 DevToolsActivePort

**Files:**
- Create: `core/src/browser/chrome.rs`

**Step 1: 寫失敗的測試**

```rust
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
        let missing = std::path::Path::new("/definitely/not/here");
        // 使用者指定了一個不存在的路徑：不能默默退回自動偵測，那會讓「設了沒生效」無聲無息
        assert!(matches!(find(Some(missing)), Err(_)));
    }
}
```

**Step 2: 跑測試確認失敗**

Run: `cargo test -p haul-core browser::chrome`
Expected: 編譯錯誤，`parse_active_port` / `find` 不存在。

**Step 3: 實作**

```rust
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
        for name in ["google-chrome", "chromium", "chromium-browser", "microsoft-edge", "brave-browser"] {
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
```

**Step 4: 跑測試**

Run: `cargo test -p haul-core browser::chrome`
Expected: 3 passed。

**Step 5: Commit**

```bash
git add core/src/browser/chrome.rs
git commit -m "瀏覽器層：找可執行檔、啟動、讀 DevToolsActivePort"
```

---

### Task 3: cdp.rs — 訊息框架（假 transport）

**Files:**
- Create: `core/src/browser/cdp.rs`

**Step 1: 寫失敗的測試**

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// 用兩條 channel 假裝是 WebSocket：測的是 id 對應與事件分派，不是網路
    fn fake() -> (Arc<Cdp>, mpsc::UnboundedReceiver<String>, mpsc::UnboundedSender<String>) {
        let (out_tx, out_rx) = mpsc::unbounded_channel();
        let (in_tx, in_rx) = mpsc::unbounded_channel();
        (Cdp::new(out_tx, in_rx), out_rx, in_tx)
    }

    #[tokio::test]
    async fn call_resolves_the_matching_id_only() {
        let (cdp, mut out, inbox) = fake();
        let c = cdp.clone();
        let h = tokio::spawn(async move { c.call(None, "Browser.getVersion", json!({})).await });

        let sent: serde_json::Value = serde_json::from_str(&out.recv().await.unwrap()).unwrap();
        let id = sent["id"].as_u64().unwrap();
        assert_eq!(sent["method"], "Browser.getVersion");

        // 別人的回應不該被吃掉
        inbox.send(json!({"id": id + 100, "result": {}}).to_string()).unwrap();
        inbox.send(json!({"id": id, "result": {"product": "Chrome/1"}}).to_string()).unwrap();

        let got = h.await.unwrap().unwrap();
        assert_eq!(got["product"], "Chrome/1");
    }

    #[tokio::test]
    async fn protocol_error_becomes_err_with_method_name() {
        let (cdp, mut out, inbox) = fake();
        let c = cdp.clone();
        let h = tokio::spawn(async move { c.call(Some("s1"), "Page.navigate", json!({})).await });
        let sent: serde_json::Value = serde_json::from_str(&out.recv().await.unwrap()).unwrap();
        assert_eq!(sent["sessionId"], "s1");
        inbox
            .send(json!({"id": sent["id"], "error": {"message": "Cannot navigate"}}).to_string())
            .unwrap();
        let err = h.await.unwrap().unwrap_err().to_string();
        assert!(err.contains("Page.navigate"), "{err}");
        assert!(err.contains("Cannot navigate"), "{err}");
    }

    #[tokio::test]
    async fn events_are_broadcast_with_session() {
        let (cdp, _out, inbox) = fake();
        let mut rx = cdp.subscribe();
        inbox
            .send(json!({"method": "Network.responseReceived", "sessionId": "s1", "params": {"x": 1}}).to_string())
            .unwrap();
        let ev = rx.recv().await.unwrap();
        assert_eq!(ev.method, "Network.responseReceived");
        assert_eq!(ev.session_id.as_deref(), Some("s1"));
        assert_eq!(ev.params["x"], 1);
    }

    #[tokio::test]
    async fn closing_the_transport_fails_pending_calls() {
        let (cdp, mut out, inbox) = fake();
        let c = cdp.clone();
        let h = tokio::spawn(async move { c.call(None, "Target.getTargets", json!({})).await });
        out.recv().await.unwrap();
        drop(inbox); // 瀏覽器掛了
        let err = h.await.unwrap().unwrap_err().to_string();
        assert!(err.contains("連線"), "{err}");
    }
}
```

**Step 2: 跑測試確認失敗**

Run: `cargo test -p haul-core browser::cdp`
Expected: 編譯錯誤。

**Step 3: 實作**

```rust
//! 最小的 Chrome DevTools Protocol 客戶端。
//!
//! 只用到五個 domain 的十來個方法，手寫幾百行比引入 chromiumoxide 划算——
//! 那類 crate 跟 Chrome 版本綁死，Chrome 一季一版，我們不想跟著改。
//! 協定本身很簡單：送 {id, method, params, sessionId}，收 {id, result|error}
//! 或 {method, params, sessionId}。

use anyhow::{anyhow, bail, Result};
use futures_util::{SinkExt, StreamExt};
use serde_json::Value;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use tokio::sync::{broadcast, mpsc, oneshot};

#[derive(Debug, Clone)]
pub struct CdpEvent {
    pub method: String,
    pub params: Value,
    pub session_id: Option<String>,
}

pub struct Cdp {
    out: mpsc::UnboundedSender<String>,
    pending: Arc<Mutex<HashMap<u64, oneshot::Sender<Result<Value>>>>>,
    events: broadcast::Sender<Arc<CdpEvent>>,
    next_id: AtomicU64,
}

/// 事件流的緩衝。一個影音頁面幾秒內幾百個 Network 事件很正常，
/// 消費端慢一點不該直接掉事件。
const EVENT_BUFFER: usize = 4096;

impl Cdp {
    /// 從一對 channel 建立。真正的 WebSocket 由 `connect` 接上；測試直接餵。
    pub fn new(out: mpsc::UnboundedSender<String>, mut inbox: mpsc::UnboundedReceiver<String>) -> Arc<Self> {
        let (events, _) = broadcast::channel(EVENT_BUFFER);
        let me = Arc::new(Self {
            out,
            pending: Arc::new(Mutex::new(HashMap::new())),
            events,
            next_id: AtomicU64::new(1),
        });

        let pending = me.pending.clone();
        let events = me.events.clone();
        tokio::spawn(async move {
            while let Some(text) = inbox.recv().await {
                let Ok(msg) = serde_json::from_str::<Value>(&text) else { continue };
                if let Some(id) = msg.get("id").and_then(Value::as_u64) {
                    if let Some(tx) = pending.lock().unwrap().remove(&id) {
                        let r = match msg.get("error") {
                            Some(e) => Err(anyhow!(
                                "{}",
                                e.get("message").and_then(Value::as_str).unwrap_or("未知錯誤")
                            )),
                            None => Ok(msg.get("result").cloned().unwrap_or(Value::Null)),
                        };
                        let _ = tx.send(r);
                    }
                } else if let Some(method) = msg.get("method").and_then(Value::as_str) {
                    let _ = events.send(Arc::new(CdpEvent {
                        method: method.to_string(),
                        params: msg.get("params").cloned().unwrap_or(Value::Null),
                        session_id: msg
                            .get("sessionId")
                            .and_then(Value::as_str)
                            .map(str::to_string),
                    }));
                }
            }
            // transport 斷了：所有還在等的呼叫都要拿到錯誤，不能永遠掛著
            for (_, tx) in pending.lock().unwrap().drain() {
                let _ = tx.send(Err(anyhow!("瀏覽器連線已關閉")));
            }
        });
        me
    }

    /// 連到 Chrome 的 DevTools WebSocket。
    pub async fn connect(ws_url: &str) -> Result<Arc<Self>> {
        let (ws, _) = tokio_tungstenite::connect_async(ws_url)
            .await
            .map_err(|e| anyhow!("連不上瀏覽器的 DevTools：{e}"))?;
        let (mut sink, mut stream) = ws.split();
        let (out_tx, mut out_rx) = mpsc::unbounded_channel::<String>();
        let (in_tx, in_rx) = mpsc::unbounded_channel::<String>();

        tokio::spawn(async move {
            while let Some(text) = out_rx.recv().await {
                if sink
                    .send(tokio_tungstenite::tungstenite::Message::Text(text))
                    .await
                    .is_err()
                {
                    break;
                }
            }
        });
        tokio::spawn(async move {
            use tokio_tungstenite::tungstenite::Message;
            while let Some(Ok(msg)) = stream.next().await {
                if let Message::Text(t) = msg {
                    if in_tx.send(t).is_err() {
                        break;
                    }
                }
            }
            // in_tx 在這裡 drop，reader 會把 pending 全部失敗
        });
        Ok(Self::new(out_tx, in_rx))
    }

    /// 呼叫一個方法並等回應。`session` 是 Target.attachToTarget 給的 sessionId，
    /// 瀏覽器層級的方法（Target.*、Browser.*）傳 None。
    pub async fn call(&self, session: Option<&str>, method: &str, params: Value) -> Result<Value> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().unwrap().insert(id, tx);

        let mut msg = serde_json::json!({ "id": id, "method": method, "params": params });
        if let Some(s) = session {
            msg["sessionId"] = Value::String(s.to_string());
        }
        if self.out.send(msg.to_string()).is_err() {
            self.pending.lock().unwrap().remove(&id);
            bail!("瀏覽器連線已關閉");
        }
        rx.await
            .map_err(|_| anyhow!("瀏覽器連線已關閉"))?
            .map_err(|e| anyhow!("{method}：{e}"))
    }

    pub fn subscribe(&self) -> broadcast::Receiver<Arc<CdpEvent>> {
        self.events.subscribe()
    }
}
```

**Step 4: 跑測試**

Run: `cargo test -p haul-core browser::cdp`
Expected: 4 passed。

**Step 5: Commit**

```bash
git add core/src/browser/cdp.rs
git commit -m "瀏覽器層：最小 CDP 客戶端"
```

---

### Task 4: 環境變數閘門的整合測試——真的啟動 Chrome、問版本

**Files:**
- Modify: `core/src/browser/chrome.rs`（tests）

**Step 1: 寫測試**

在 `chrome.rs` 的 tests 加：

```rust
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
```

**Step 2: 跑測試**

Run: `HAUL_TEST_CHROME=1 cargo test -p haul-core launches_and_answers -- --nocapture`
Expected: PASS，會閃一個 Chrome 視窗然後關掉。也跑一次不帶環境變數確認會略過。

**Step 3: Commit**

```bash
git add core/src/browser/chrome.rs
git commit -m "瀏覽器層：真實 Chrome 的啟動與 CDP 連線整合測試"
```

---

### Task 5: sniff.rs — 候選分類、去重、計分

**Files:**
- Create: `core/src/browser/sniff.rs`

**Step 1: 寫失敗的測試**

```rust
#[cfg(test)]
mod tests {
    use super::*;

    fn c(url: &str, kind: Kind, size: Option<u64>) -> Candidate {
        Candidate { url: url.into(), kind, size, mime: String::new(), headers: vec![] }
    }

    #[test]
    fn classifies_by_mime_first_then_extension() {
        assert_eq!(classify("https://x/a?e=1", "application/vnd.apple.mpegurl", "Fetch"), Some(Kind::Manifest));
        assert_eq!(classify("https://x/a.m3u8", "text/plain", "Fetch"), Some(Kind::Manifest));
        assert_eq!(classify("https://x/a.mpd", "application/dash+xml", "Fetch"), Some(Kind::Manifest));
        assert_eq!(classify("https://x/v.mp4", "video/mp4", "Media"), Some(Kind::File));
        assert_eq!(classify("https://x/v", "audio/mpeg", "Media"), Some(Kind::File));
        // 分段：不是可以直接抓的東西，但值得記下來給錯誤訊息用
        assert_eq!(classify("https://x/seg-12.ts", "video/mp2t", "Fetch"), Some(Kind::Segment));
        assert_eq!(classify("https://x/seg-12.m4s", "application/octet-stream", "Fetch"), Some(Kind::Segment));
        // resourceType 是 Media 但 MIME 講不清楚：信 Chrome 的判斷
        assert_eq!(classify("https://x/stream", "application/octet-stream", "Media"), Some(Kind::File));
        assert_eq!(classify("https://x/page", "text/html", "Document"), None);
        assert_eq!(classify("https://x/app.js", "application/javascript", "Script"), None);
    }

    #[test]
    fn ad_hosts_are_dropped() {
        assert!(is_ad_host("pagead2.googlesyndication.com"));
        assert!(is_ad_host("ad.doubleclick.net"));
        assert!(!is_ad_host("cdn.example.com"));
    }

    #[test]
    fn range_requests_to_the_same_url_merge_into_one() {
        let mut set = Candidates::default();
        set.push(c("https://x/v.mp4", Kind::File, Some(1_000)));
        set.push(c("https://x/v.mp4", Kind::File, Some(50_000_000)));
        set.push(c("https://x/v.mp4", Kind::File, None));
        assert_eq!(set.list().len(), 1);
        assert_eq!(set.list()[0].size, Some(50_000_000));
    }

    #[test]
    fn best_prefers_manifest_then_largest_file() {
        let mut set = Candidates::default();
        set.push(c("https://x/small.mp4", Kind::File, Some(20_000)));
        set.push(c("https://x/big.mp4", Kind::File, Some(90_000_000)));
        assert_eq!(set.best().unwrap().url, "https://x/big.mp4");
        set.push(c("https://x/master.m3u8", Kind::Manifest, None));
        assert_eq!(set.best().unwrap().url, "https://x/master.m3u8");
    }

    #[test]
    fn tiny_files_and_segments_never_win() {
        let mut set = Candidates::default();
        set.push(c("https://x/beacon.mp3", Kind::File, Some(2_000)));
        set.push(c("https://x/seg.ts", Kind::Segment, Some(500_000)));
        assert!(set.best().is_none());
        assert!(set.saw_segments());
    }

    #[test]
    fn only_forwardable_headers_are_kept() {
        let kept = keep_headers(&[
            ("Referer".into(), "https://x/".into()),
            ("cookie".into(), "a=b".into()),
            ("Accept-Encoding".into(), "gzip".into()),
            ("Range".into(), "bytes=0-".into()),
            ("User-Agent".into(), "UA".into()),
            ("Authorization".into(), "Bearer t".into()),
        ]);
        let names: Vec<_> = kept.iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(names, ["Referer", "cookie", "User-Agent", "Authorization"]);
    }
}
```

**Step 2: 跑測試確認失敗**

Run: `cargo test -p haul-core browser::sniff`
Expected: 編譯錯誤。

**Step 3: 實作**

```rust
//! 從瀏覽器的網路事件裡挑出「這頁真正的媒體」。
//!
//! 一個影音頁面跑起來有幾百個請求：預覽縮圖、廣告、背景音、主片、
//! 同一支片的兩三種畫質。這裡負責分類、去重、挑一個最像的，
//! 以及決定什麼時候可以停止觀察。

use serde::Serialize;
use std::time::{Duration, Instant};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    /// HLS / DASH 清單。交給 yt-dlp，它會自己抓分段合併
    Manifest,
    /// 單一媒體檔。走直接抓取
    File,
    /// HLS / DASH 的分段。本身不能抓，但看到它而沒看到清單，
    /// 代表清單是 JS 自己組的——這時該建議改用錄製
    Segment,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Candidate {
    pub url: String,
    pub kind: Kind,
    pub size: Option<u64>,
    pub mime: String,
    /// 原始請求裡值得帶去重放的 header（Referer、Cookie…）
    pub headers: Vec<(String, String)>,
}

const MANIFEST_MIMES: &[&str] = &[
    "application/vnd.apple.mpegurl",
    "application/x-mpegurl",
    "audio/mpegurl",
    "audio/x-mpegurl",
    "application/dash+xml",
];
const FILE_EXTS: &[&str] = &[
    "mp4", "m4a", "m4v", "mp3", "webm", "ogg", "oga", "opus", "flac", "wav", "aac", "mov", "mkv",
];
const SEGMENT_EXTS: &[&str] = &["ts", "m4s"];
const AD_HOSTS: &[&str] = &[
    "doubleclick.net",
    "googlesyndication.com",
    "googleadservices.com",
    "adnxs.com",
    "adsystem.com",
    "moatads.com",
];
/// 比這小的「媒體」是 beacon 或探測，不是內容
pub const MIN_FILE_BYTES: u64 = 10_240;

fn ext_of(url: &str) -> String {
    url.split(['?', '#'])
        .next()
        .unwrap_or("")
        .rsplit('/')
        .next()
        .and_then(|f| f.rsplit_once('.'))
        .map(|(_, e)| e.to_ascii_lowercase())
        .unwrap_or_default()
}

/// 依 MIME、副檔名與 Chrome 的 resourceType 判斷。MIME 最可靠，
/// 但很多 CDN 對 m3u8 回 text/plain、對分段回 octet-stream，所以三者都看。
pub fn classify(url: &str, mime: &str, resource_type: &str) -> Option<Kind> {
    let mime = mime.split(';').next().unwrap_or("").trim().to_ascii_lowercase();
    let ext = ext_of(url);

    if MANIFEST_MIMES.contains(&mime.as_str()) || ext == "m3u8" || ext == "mpd" {
        return Some(Kind::Manifest);
    }
    if mime == "video/mp2t" || mime == "video/iso.segment" || SEGMENT_EXTS.contains(&ext.as_str()) {
        return Some(Kind::Segment);
    }
    if mime.starts_with("video/") || mime.starts_with("audio/") || FILE_EXTS.contains(&ext.as_str()) {
        return Some(Kind::File);
    }
    if resource_type == "Media" {
        return Some(Kind::File);
    }
    None
}

pub fn is_ad_host(host: &str) -> bool {
    let h = host.to_ascii_lowercase();
    AD_HOSTS.iter().any(|a| h == *a || h.ends_with(&format!(".{a}")))
}

/// 只帶跟身分與來源有關的 header。Range、Accept-Encoding 這些是那一次請求的
/// 細節，帶去重放反而會拿到半截或壓縮過的東西。
pub fn keep_headers(all: &[(String, String)]) -> Vec<(String, String)> {
    const KEEP: &[&str] = &["referer", "origin", "user-agent", "cookie", "authorization"];
    all.iter()
        .filter(|(k, _)| KEEP.contains(&k.to_ascii_lowercase().as_str()))
        .cloned()
        .collect()
}

#[derive(Default, Debug)]
pub struct Candidates {
    list: Vec<Candidate>,
    segments: usize,
}

impl Candidates {
    /// 同一個網址的 Range 請求會來很多次，合併成一筆、大小取最大。
    pub fn push(&mut self, c: Candidate) {
        if c.kind == Kind::Segment {
            self.segments += 1;
            return;
        }
        if let Some(existing) = self.list.iter_mut().find(|e| e.url == c.url) {
            existing.size = match (existing.size, c.size) {
                (Some(a), Some(b)) => Some(a.max(b)),
                (a, b) => a.or(b),
            };
            if existing.headers.is_empty() {
                existing.headers = c.headers;
            }
            return;
        }
        self.list.push(c);
    }

    pub fn list(&self) -> &[Candidate] {
        &self.list
    }

    pub fn saw_segments(&self) -> bool {
        self.segments > 0
    }

    /// 清單優先——它包含所有畫質而且 yt-dlp 會處理合併；
    /// 單檔取最大的；太小的不算。
    pub fn best(&self) -> Option<&Candidate> {
        if let Some(m) = self.list.iter().find(|c| c.kind == Kind::Manifest) {
            return Some(m);
        }
        self.list
            .iter()
            .filter(|c| c.kind == Kind::File)
            .filter(|c| c.size.map_or(true, |s| s >= MIN_FILE_BYTES))
            .max_by_key(|c| c.size.unwrap_or(0))
    }
}
```

**Step 4: 跑測試**

Run: `cargo test -p haul-core browser::sniff`
Expected: 6 passed。

**Step 5: Commit**

```bash
git add core/src/browser/sniff.rs
git commit -m "瀏覽器層：媒體候選的分類、去重與計分"
```

---

### Task 6: sniff.rs — 停止規則狀態機

**Files:**
- Modify: `core/src/browser/sniff.rs`

**Step 1: 寫失敗的測試**

加到 tests：

```rust
    #[test]
    fn stop_rule_waits_briefly_after_a_manifest() {
        let t0 = Instant::now();
        let mut r = StopRule::new(t0);
        assert_eq!(r.deadline(), t0 + StopRule::OVERALL);
        r.saw(Kind::Manifest, t0 + Duration::from_secs(4));
        assert_eq!(r.deadline(), t0 + Duration::from_secs(4) + StopRule::AFTER_MANIFEST);
    }

    #[test]
    fn stop_rule_extends_quiet_window_per_file() {
        let t0 = Instant::now();
        let mut r = StopRule::new(t0);
        r.saw(Kind::File, t0 + Duration::from_secs(2));
        assert_eq!(r.deadline(), t0 + Duration::from_secs(2) + StopRule::QUIET_AFTER_FILE);
        // 又來一個：安靜期重算
        r.saw(Kind::File, t0 + Duration::from_secs(5));
        assert_eq!(r.deadline(), t0 + Duration::from_secs(5) + StopRule::QUIET_AFTER_FILE);
        // 清單出現後就不再被單檔延長
        r.saw(Kind::Manifest, t0 + Duration::from_secs(6));
        r.saw(Kind::File, t0 + Duration::from_secs(7));
        assert_eq!(r.deadline(), t0 + Duration::from_secs(6) + StopRule::AFTER_MANIFEST);
    }

    #[test]
    fn stop_rule_never_exceeds_overall_timeout() {
        let t0 = Instant::now();
        let mut r = StopRule::new(t0);
        r.saw(Kind::File, t0 + StopRule::OVERALL - Duration::from_secs(1));
        assert_eq!(r.deadline(), t0 + StopRule::OVERALL);
    }

    #[test]
    fn segments_do_not_affect_the_deadline() {
        let t0 = Instant::now();
        let mut r = StopRule::new(t0);
        r.saw(Kind::Segment, t0 + Duration::from_secs(3));
        assert_eq!(r.deadline(), t0 + StopRule::OVERALL);
    }
```

**Step 2: 跑測試確認失敗**

Run: `cargo test -p haul-core stop_rule`
Expected: 編譯錯誤。

**Step 3: 實作**

加到 `sniff.rs`：

```rust
/// 什麼時候可以停止觀察。
///
/// 清單出現後媒體的形狀就確定了，再等一下收尾就好；單檔則要等它安靜下來，
/// 因為播放器常常先拿一小段探測再拿正片。整體有上限，頁面不播就是不播。
#[derive(Debug)]
pub struct StopRule {
    start: Instant,
    deadline: Instant,
    manifest_seen: bool,
}

impl StopRule {
    pub const OVERALL: Duration = Duration::from_secs(60);
    pub const AFTER_MANIFEST: Duration = Duration::from_secs(3);
    pub const QUIET_AFTER_FILE: Duration = Duration::from_secs(5);

    pub fn new(now: Instant) -> Self {
        Self {
            start: now,
            deadline: now + Self::OVERALL,
            manifest_seen: false,
        }
    }

    pub fn saw(&mut self, kind: Kind, now: Instant) {
        let cap = self.start + Self::OVERALL;
        match kind {
            Kind::Manifest if !self.manifest_seen => {
                self.manifest_seen = true;
                self.deadline = (now + Self::AFTER_MANIFEST).min(cap);
            }
            Kind::File if !self.manifest_seen => {
                self.deadline = (now + Self::QUIET_AFTER_FILE).min(cap);
            }
            _ => {}
        }
    }

    pub fn deadline(&self) -> Instant {
        self.deadline
    }
}
```

**Step 4: 跑測試**

Run: `cargo test -p haul-core browser::sniff`
Expected: 10 passed。

**Step 5: Commit**

```bash
git add core/src/browser/sniff.rs
git commit -m "瀏覽器層：偵測的停止規則"
```

---

### Task 7: cookies.rs — 把 Netscape 檔解析抽成可重用

**Files:**
- Modify: `core/src/cookies.rs`

`Network.setCookies` 需要結構化的 cookie，`header_for_host` 目前把解析寫死在裡面。抽出來，兩邊共用。

**Step 1: 寫失敗的測試**

加到 `cookies.rs` 的 tests：

```rust
    #[test]
    fn parses_netscape_rows_into_structs() {
        let f = write(
            "c3.txt",
            "# Netscape HTTP Cookie File\n\
             .example.com\tTRUE\t/\tTRUE\t1800000000\tsess\tabc\n\
             #HttpOnly_.example.com\tTRUE\t/api\tFALSE\t0\ttoken\txyz\n",
        );
        let rows = parse_netscape(&f);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].domain, ".example.com");
        assert!(rows[0].secure);
        assert_eq!(rows[0].expires, Some(1_800_000_000));
        assert_eq!(rows[1].path, "/api");
        assert!(rows[1].http_only);
        assert_eq!(rows[1].expires, None);
        assert_eq!((rows[1].name.as_str(), rows[1].value.as_str()), ("token", "xyz"));
    }
```

**Step 2: 跑測試確認失敗**

Run: `cargo test -p haul-core parses_netscape`
Expected: 編譯錯誤。

**Step 3: 實作**

在 `cookies.rs` 加：

```rust
/// Netscape cookie 檔的一列。
#[derive(Clone, Debug)]
pub struct Cookie {
    pub domain: String,
    pub path: String,
    pub secure: bool,
    pub http_only: bool,
    /// Unix 秒；0 代表 session cookie
    pub expires: Option<u64>,
    pub name: String,
    pub value: String,
}

pub fn parse_netscape(file: &Path) -> Vec<Cookie> {
    let Ok(text) = std::fs::read_to_string(file) else {
        return Vec::new();
    };
    text.lines()
        .filter_map(|line| {
            // #HttpOnly_ 開頭的是真的 cookie，其餘 # 開頭才是註解
            let (http_only, line) = match line.strip_prefix("#HttpOnly_") {
                Some(rest) => (true, rest),
                None => (false, line),
            };
            if line.starts_with('#') || line.trim().is_empty() {
                return None;
            }
            let mut f = line.split('\t');
            let domain = f.next()?.to_string();
            let _flag = f.next()?;
            let path = f.next()?.to_string();
            let secure = f.next()?.eq_ignore_ascii_case("TRUE");
            let expires = f.next()?.parse::<u64>().ok().filter(|&e| e > 0);
            let name = f.next()?.to_string();
            let value = f.next().unwrap_or("").to_string();
            Some(Cookie { domain, path, secure, http_only, expires, name, value })
        })
        .collect()
}
```

然後把 `header_for_host` 改成用它：

```rust
pub fn header_for_host(file: &Path, host: &str) -> Option<String> {
    let host = host.to_ascii_lowercase();
    let pairs: Vec<String> = parse_netscape(file)
        .into_iter()
        .filter(|c| {
            let domain = c.domain.trim_start_matches('.').to_ascii_lowercase();
            host == domain || host.ends_with(&format!(".{domain}"))
        })
        .map(|c| format!("{}={}", c.name, c.value))
        .collect();
    (!pairs.is_empty()).then(|| pairs.join("; "))
}
```

**Step 4: 跑測試**

Run: `cargo test -p haul-core cookies`
Expected: 全部通過（既有 5 個 + 新的 1 個）。

**Step 5: Commit**

```bash
git add core/src/cookies.rs
git commit -m "cookies：Netscape 解析抽成結構，給 CDP 灌 cookie 用"
```

---

### Task 8: sniff.rs — 從 CDP 事件收集候選（含整合測試）

**Files:**
- Modify: `core/src/browser/sniff.rs`
- Create: `core/tests/fixtures/`（不需要，測試用內嵌字串起本機 server）

**Step 1: 寫整合測試（環境變數閘門）**

加到 `sniff.rs` tests：

```rust
    /// 起一個只回兩個路徑的本機 HTTP server：一頁有 <video>，一支假的 mp4。
    /// 用 std 的 TcpListener 就夠，不需要引入 hyper。
    fn tiny_server() -> (String, std::thread::JoinHandle<()>) {
        use std::io::{Read, Write};
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", l.local_addr().unwrap());
        let page = format!(
            "<!doctype html><video autoplay muted src=\"{base}/clip.mp4\"></video>"
        );
        // 假 mp4：內容不重要，這裡測的是偵測不是驗證；夠大才不會被當 beacon
        let clip = vec![0u8; 64 * 1024];
        let h = std::thread::spawn(move || {
            for _ in 0..8 {
                let Ok((mut s, _)) = l.accept() else { break };
                let mut buf = [0u8; 2048];
                let n = s.read(&mut buf).unwrap_or(0);
                let req = String::from_utf8_lossy(&buf[..n]);
                let (ct, body): (&str, &[u8]) = if req.starts_with("GET /clip.mp4") {
                    ("video/mp4", &clip)
                } else {
                    ("text/html", page.as_bytes())
                };
                let _ = write!(
                    s,
                    "HTTP/1.1 200 OK\r\nContent-Type: {ct}\r\nContent-Length: {}\r\nAccept-Ranges: bytes\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                let _ = s.write_all(body);
            }
        });
        (base, h)
    }

    #[tokio::test]
    async fn real_chrome_reports_the_video_on_the_page() {
        if std::env::var("HAUL_TEST_CHROME").is_err() {
            eprintln!("略過：未設 HAUL_TEST_CHROME");
            return;
        }
        let (base, _srv) = tiny_server();
        let exe = super::super::chrome::find(None).unwrap();
        let dir = std::env::temp_dir().join("haul-chrome-test");
        let chrome = super::super::chrome::launch(&exe, &dir).await.unwrap();
        let cdp = super::super::cdp::Cdp::connect(&chrome.ws_url).await.unwrap();

        let out = sniff(&cdp, &format!("{base}/"), &[], |_| {}).await.unwrap();
        let _ = cdp.call(None, "Browser.close", serde_json::json!({})).await;

        let best = out.candidates.best().expect("該偵測到 clip.mp4");
        assert!(best.url.ends_with("/clip.mp4"), "{}", best.url);
        assert_eq!(best.kind, Kind::File);
        assert_eq!(best.size, Some(64 * 1024));
        assert!(best.headers.iter().any(|(k, _)| k.eq_ignore_ascii_case("referer")));
        assert!(!out.title.is_empty() || out.title.is_empty()); // 標題可能是空的，只確認欄位存在
    }
```

**Step 2: 跑測試確認失敗**

Run: `cargo test -p haul-core real_chrome_reports`
Expected: 編譯錯誤，`sniff` / `Sniffed` 不存在。

**Step 3: 實作**

加到 `sniff.rs`：

```rust
use super::cdp::{Cdp, CdpEvent};
use crate::cookies::Cookie;
use anyhow::{anyhow, bail, Result};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::Arc;

pub struct Sniffed {
    pub candidates: Candidates,
    pub title: String,
}

/// 偵測進度，給 UI 顯示「偵測到 N 個媒體」
pub type Progress<'a> = &'a mut dyn FnMut(usize);

/// 開一個分頁載入 `url`，觀察網路直到停止規則到了。
///
/// `cookies` 非空時在導向前灌進去——瀏覽器 profile 是乾淨的，
/// 使用者這次帶的登入狀態要自己送進去。
pub async fn sniff(
    cdp: &Arc<Cdp>,
    url: &str,
    cookies: &[Cookie],
    mut progress: impl FnMut(usize),
) -> Result<Sniffed> {
    // 先訂閱再開分頁，否則載入初期的事件會漏掉
    let mut events = cdp.subscribe();

    let target = cdp
        .call(None, "Target.createTarget", json!({ "url": "about:blank" }))
        .await?;
    let target_id = target["targetId"]
        .as_str()
        .ok_or_else(|| anyhow!("Target.createTarget 沒有回 targetId"))?
        .to_string();
    let attached = cdp
        .call(None, "Target.attachToTarget", json!({ "targetId": target_id, "flatten": true }))
        .await?;
    let sid = attached["sessionId"]
        .as_str()
        .ok_or_else(|| anyhow!("attachToTarget 沒有回 sessionId"))?
        .to_string();

    cdp.call(Some(&sid), "Network.enable", json!({})).await?;
    cdp.call(Some(&sid), "Page.enable", json!({})).await?;

    if !cookies.is_empty() {
        let list: Vec<Value> = cookies
            .iter()
            .map(|c| {
                let mut v = json!({
                    "name": c.name, "value": c.value, "domain": c.domain,
                    "path": c.path, "secure": c.secure, "httpOnly": c.http_only,
                });
                if let Some(e) = c.expires {
                    v["expires"] = json!(e);
                }
                v
            })
            .collect();
        // 一顆壞 cookie 不該讓整批失敗，所以錯誤只記不擋
        let _ = cdp.call(Some(&sid), "Network.setCookies", json!({ "cookies": list })).await;
    }

    cdp.call(Some(&sid), "Page.navigate", json!({ "url": url })).await?;

    let mut rule = StopRule::new(Instant::now());
    let mut found = Candidates::default();
    // requestId -> 該請求的 header；ExtraInfo 與 requestWillBeSent 順序不定，兩邊都收
    let mut req_headers: HashMap<String, Vec<(String, String)>> = HashMap::new();
    let mut req_type: HashMap<String, String> = HashMap::new();

    loop {
        let now = Instant::now();
        let left = rule.deadline().saturating_duration_since(now);
        if left.is_zero() {
            break;
        }
        let ev = match tokio::time::timeout(left, events.recv()).await {
            Ok(Ok(ev)) => ev,
            Ok(Err(tokio::sync::broadcast::error::RecvError::Lagged(_))) => continue,
            Ok(Err(_)) => bail!("瀏覽器連線已關閉"),
            Err(_) => break, // 到期
        };
        if ev.session_id.as_deref() != Some(&sid) {
            // 分頁被使用者關掉：Target.targetDestroyed 是瀏覽器層級事件，沒有 sessionId
            if ev.method == "Target.targetDestroyed"
                && ev.params["targetId"].as_str() == Some(&target_id)
            {
                bail!("已取消（分頁被關閉）");
            }
            continue;
        }
        if let Some(c) = on_event(&ev, &mut req_headers, &mut req_type) {
            rule.saw(c.kind, Instant::now());
            found.push(c);
            progress(found.list().len());
        }
    }

    let title = cdp
        .call(Some(&sid), "Runtime.evaluate", json!({ "expression": "document.title", "returnByValue": true }))
        .await
        .ok()
        .and_then(|v| v["result"]["value"].as_str().map(str::to_string))
        .unwrap_or_default();

    let _ = cdp.call(None, "Target.closeTarget", json!({ "targetId": target_id })).await;

    Ok(Sniffed { candidates: found, title })
}

fn headers_of(v: &Value) -> Vec<(String, String)> {
    v.as_object()
        .map(|m| {
            m.iter()
                .map(|(k, v)| (k.clone(), v.as_str().unwrap_or("").to_string()))
                .collect()
        })
        .unwrap_or_default()
}

fn header<'a>(hs: &'a [(String, String)], name: &str) -> Option<&'a str> {
    hs.iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(name))
        .map(|(_, v)| v.as_str())
}

/// 把一個 Network 事件變成候選（如果它是媒體的話）。
fn on_event(
    ev: &CdpEvent,
    req_headers: &mut HashMap<String, Vec<(String, String)>>,
    req_type: &mut HashMap<String, String>,
) -> Option<Candidate> {
    let p = &ev.params;
    let rid = p["requestId"].as_str()?.to_string();
    match ev.method.as_str() {
        "Network.requestWillBeSent" => {
            req_headers
                .entry(rid.clone())
                .or_default()
                .extend(headers_of(&p["request"]["headers"]));
            if let Some(t) = p["type"].as_str() {
                req_type.insert(rid, t.to_string());
            }
            None
        }
        // 這個事件才有完整的 header，包括瀏覽器自己加的 Cookie
        "Network.requestWillBeSentExtraInfo" => {
            let mut hs = headers_of(&p["headers"]);
            let entry = req_headers.entry(rid).or_default();
            // ExtraInfo 的比較完整，蓋掉同名的
            entry.retain(|(k, _)| !hs.iter().any(|(k2, _)| k2.eq_ignore_ascii_case(k)));
            entry.append(&mut hs);
            None
        }
        "Network.responseReceived" => {
            let r = &p["response"];
            let status = r["status"].as_u64().unwrap_or(0);
            if status != 200 && status != 206 {
                return None;
            }
            let url = r["url"].as_str()?.to_string();
            let host = url.split("://").nth(1)?.split('/').next()?.split(':').next()?;
            if is_ad_host(host) {
                return None;
            }
            let mime = r["mimeType"].as_str().unwrap_or("").to_string();
            let rtype = p["type"]
                .as_str()
                .map(str::to_string)
                .or_else(|| req_type.get(&rid).cloned())
                .unwrap_or_default();
            let kind = classify(&url, &mime, &rtype)?;

            let resp_headers = headers_of(&r["headers"]);
            // 206 的 Content-Length 是那一段的長度，總長在 Content-Range 的斜線後面
            let size = header(&resp_headers, "content-range")
                .and_then(|cr| cr.rsplit('/').next())
                .and_then(|t| t.parse::<u64>().ok())
                .or_else(|| header(&resp_headers, "content-length").and_then(|l| l.parse().ok()));

            let headers = keep_headers(req_headers.get(&rid).map(Vec::as_slice).unwrap_or(&[]));
            Some(Candidate { url, kind, size, mime, headers })
        }
        _ => None,
    }
}
```

**Step 4: 跑測試**

Run: `HAUL_TEST_CHROME=1 cargo test -p haul-core real_chrome_reports -- --nocapture`
Expected: PASS。再跑 `cargo test -p haul-core browser` 不帶環境變數確認其他都過、整合測試略過。

**Step 5: Commit**

```bash
git add core/src/browser/sniff.rs
git commit -m "瀏覽器層：從 CDP 網路事件收集媒體候選"
```

---

### Task 9: extract.rs 與 direct.rs 吃自訂 header

**Files:**
- Modify: `core/src/extract.rs:74-82`（`base_args` 附近）與 `download` 簽章
- Modify: `core/src/direct.rs:358-380`
- Modify: `core/src/engine.rs:880-925`（呼叫端）

**Step 1: 寫失敗的測試**

`extract.rs` tests 加：

```rust
    #[test]
    fn header_args_are_one_add_headers_per_pair() {
        let got = header_args(&[("Referer".into(), "https://x/".into()), ("Cookie".into(), "a=b".into())]);
        assert_eq!(got, ["--add-headers", "Referer:https://x/", "--add-headers", "Cookie:a=b"]);
        assert!(header_args(&[]).is_empty());
    }
```

**Step 2: 跑測試確認失敗**

Run: `cargo test -p haul-core header_args`
Expected: 編譯錯誤。

**Step 3: 實作**

`extract.rs`：

```rust
/// 瀏覽器攔到的請求 header 要原樣帶去重放，否則 CDN 認不得這個請求。
pub fn header_args(headers: &[(String, String)]) -> Vec<String> {
    headers
        .iter()
        .flat_map(|(k, v)| ["--add-headers".to_string(), format!("{k}:{v}")])
        .collect()
}
```

`download` 簽章加 `headers: &[(String, String)]`（放在 `browser` 之後），在 `cookie_args(&mut cmd, browser);` 後加 `cmd.args(header_args(headers));`。

`direct.rs` 的 `download`：把 `cookie: Option<&str>` 改成 `headers: &[(String, String)]`，迴圈裡改成：

```rust
        let mut req = client.get(media_url).header("user-agent", ua);
        for (k, v) in headers {
            // 攔到的 User-Agent 要蓋掉我們的，否則 CDN 看到兩個不同身分
            req = req.header(k.as_str(), v.as_str());
        }
```

`engine.rs` 的 `fetch()`：`Job::Ytdlp` 那支呼叫傳 `&[]`；`Job::Direct` 那支把 `cookie` 包成

```rust
                let headers: Vec<(String, String)> = cookie
                    .into_iter()
                    .map(|c| ("cookie".to_string(), c))
                    .collect();
```

再傳 `&headers`。

**Step 4: 跑測試**

Run: `cargo test --workspace`
Expected: 全部通過。

**Step 5: Commit**

```bash
git add core/src/extract.rs core/src/direct.rs core/src/engine.rs
git commit -m "extract / direct：接受自訂 header，給瀏覽器層重放請求用"
```

---

### Task 10: engine.rs — Job::Browser、can_browser、瀏覽器 session

**Files:**
- Modify: `core/src/engine.rs`

**Step 1: 寫失敗的測試**

`engine.rs` tests 加：

```rust
    #[test]
    fn only_extract_failures_are_browser_eligible() {
        assert!(browser_eligible("Unsupported URL: https://x"));
        assert!(browser_eligible("[Liability] This website is not supported"));
        assert!(browser_eligible("不像檔案：text/html"));
        // 這些瀏覽器救不了
        assert!(!browser_eligible("HTTP 401 Unauthorized"));
        assert!(!browser_eligible("HTTP 429（換過 UA…）"));
        assert!(!browser_eligible("驗證未通過：ffmpeg 解不開"));
    }
```

**Step 2: 跑測試確認失敗**

Run: `cargo test -p haul-core browser_eligible`
Expected: 編譯錯誤。

**Step 3: 實作**

(a) `Item` 加欄位（`error` 之後）：

```rust
    /// 這個失敗是萃取層面的、瀏覽器可能救得了。GUI 據此決定要不要出現「用瀏覽器抓」。
    #[serde(default)]
    pub can_browser: bool,
```

`push()` 建 Item 時補 `can_browser: false`。

(b) `Event` 加變體：

```rust
    /// 瀏覽器偵測到的候選。自動挑一個開始抓，其餘讓使用者可以另外加
    Candidates {
        id: u64,
        candidates: Vec<browser::sniff::Candidate>,
    },
```

`Event` 目前 derive `Serialize`，`Candidate` 已經 `Serialize`。

(c) `Job` 加變體：

```rust
    /// 瀏覽器攔到的請求。清單交給 yt-dlp、單檔走直接抓取，都帶原始 header
    Browser {
        media: String,
        title: String,
        headers: Vec<(String, String)>,
        manifest: bool,
        content_type: Option<String>,
    },
```

(d) `Config` 加 `pub browser_fallback: bool`（預設 false）與 `pub browser_path: Option<PathBuf>`（預設 None）。

(e) `Engine` 加欄位：

```rust
    /// Haul 自己的瀏覽器實例，用到才啟動。整個 Engine 共用一個，多個項目開多個分頁。
    browser: tokio::sync::Mutex<Option<BrowserSession>>,
    /// 還有幾個項目在用瀏覽器。歸零就關掉——不留一個 Chrome 在背景。
    browser_users: AtomicUsize,
```

```rust
struct BrowserSession {
    chrome: browser::chrome::Chrome,
    cdp: Arc<browser::cdp::Cdp>,
}
```

`new()` 裡初始化 `browser: tokio::sync::Mutex::new(None), browser_users: AtomicUsize::new(0)`。

(f) 判斷函式：

```rust
/// 瀏覽器能救的只有「萃取器不認得這頁」這一類。401 是身分問題、429 是限流、
/// 驗證失敗是內容問題——開瀏覽器只會多花時間得到同樣的結果。
fn browser_eligible(error: &str) -> bool {
    let e = error.to_ascii_lowercase();
    if e.contains("http 4") || e.contains("http 5") || e.contains("驗證未通過") {
        return false;
    }
    e.contains("unsupported url")
        || e.contains("not supported")
        || e.contains("unable to extract")
        || e.contains("不像檔案")
        || e.contains("no video formats")
        || e.contains("nothing to download")
}
```

(g) `add()` 裡 `resolve` 失敗那支：

```rust
        let resolution = match self.resolve(&tools, &input).await {
            Ok(r) => r,
            Err(e) => {
                let msg = e.to_string();
                let eligible = browser_eligible(&msg);
                self.fail(id, msg);
                if eligible {
                    self.update(id, |i| i.can_browser = true);
                    if self.cfg.browser_fallback {
                        let me = self.clone();
                        return vec![tokio::spawn(async move { me.retry_with_browser(id).await })];
                    }
                }
                return Vec::new();
            }
        };
```

`Resolution::Playlist` 的 `key` match 加 `Job::Browser { media, .. } => media.clone()`；`input.resolved` 的 `source` match 加 `Some(Job::Browser { .. }) => "browser"`。

(h) 瀏覽器 session 與重試：

```rust
    async fn browser_session(&self) -> Result<Arc<browser::cdp::Cdp>, String> {
        let mut guard = self.browser.lock().await;
        if let Some(s) = guard.as_mut() {
            if s.chrome.alive() {
                return Ok(s.cdp.clone());
            }
            // 使用者把整個瀏覽器關了：丟掉舊的重開
            *guard = None;
        }
        let exe = browser::chrome::find(self.cfg.browser_path.as_deref()).map_err(|e| e.to_string())?;
        let dir = self.cfg.bin_dir.parent().unwrap_or(&self.cfg.bin_dir).join("browser");
        let chrome = browser::chrome::launch(&exe, &dir).await.map_err(|e| e.to_string())?;
        let cdp = browser::cdp::Cdp::connect(&chrome.ws_url).await.map_err(|e| e.to_string())?;
        self.log.info("browser.launched", serde_json::json!({ "exe": exe.display().to_string() }));
        *guard = Some(BrowserSession { chrome, cdp: cdp.clone() });
        Ok(cdp)
    }

    async fn browser_release(&self) {
        if self.browser_users.fetch_sub(1, Ordering::SeqCst) == 1 {
            if let Some(s) = self.browser.lock().await.take() {
                let _ = s.cdp.call(None, "Browser.close", serde_json::json!({})).await;
                drop(s); // kill_on_drop 兜底
                self.log.info("browser.closed", serde_json::json!({}));
            }
        }
    }

    /// 用瀏覽器重試一個萃取失敗的項目。
    pub async fn retry_with_browser(self: &Arc<Self>, id: u64) {
        let Some(item) = self.items.lock().unwrap().iter().find(|i| i.id == id).cloned() else {
            return;
        };
        let mode = Mode::parse(&item.kind);
        self.update(id, |i| {
            i.status = "browser".into();
            i.error = None;
            i.can_browser = false;
        });
        self.browser_users.fetch_add(1, Ordering::SeqCst);

        let outcome = self.sniff_for(id, &item.input).await;
        self.browser_release().await;

        let (job, title) = match outcome {
            Ok(v) => v,
            Err(e) => return self.fail(id, e),
        };
        let tools = match self.tools().await {
            Ok(t) => t,
            Err(e) => return self.fail(id, e),
        };
        self.update(id, |i| i.title = title);
        self.run(tools, id, job, mode, extract::Options::default()).await;
    }

    async fn sniff_for(&self, id: u64, url: &str) -> Result<(Job, String), String> {
        let cdp = self.browser_session().await?;
        let cookies = self
            .cookie_file
            .lock()
            .unwrap()
            .as_ref()
            .map(|(_, f)| cookies::parse_netscape(f))
            .unwrap_or_default();

        let me_items = &self.items;
        let sniffed = browser::sniff::sniff(&cdp, url, &cookies, |n| {
            // 借 total 欄位顯示偵測到幾個，UI 會依 status 決定怎麼呈現
            if let Ok(mut items) = me_items.lock() {
                if let Some(i) = items.iter_mut().find(|i| i.id == id) {
                    i.total = n as u64;
                }
            }
        })
        .await
        .map_err(|e| e.to_string())?;

        let list = sniffed.candidates.list().to_vec();
        self.log.info(
            "browser.sniffed",
            serde_json::json!({ "id": id, "candidates": list.len(), "segments": sniffed.candidates.saw_segments() }),
        );
        self.emit(Event::Candidates { id, candidates: list });

        let best = sniffed.candidates.best().ok_or_else(|| {
            if sniffed.candidates.saw_segments() {
                "頁面在播分段串流但沒有清單（JS 自己組的），這種抓不到原檔，可改用錄製".to_string()
            } else {
                "沒偵測到可下載的媒體。若頁面要按播放才會載入，開著瀏覽器再試一次；否則可改用錄製".to_string()
            }
        })?;

        let title = if sniffed.title.trim().is_empty() { short(url) } else { sniffed.title.clone() };
        Ok((
            Job::Browser {
                media: best.url.clone(),
                title: title.clone(),
                headers: best.headers.clone(),
                manifest: best.kind == browser::sniff::Kind::Manifest,
                content_type: Some(best.mime.clone()),
            },
            title,
        ))
    }

    /// 使用者從候選清單挑了另一個：新增一個項目去抓它。
    /// 不取消原本那個——引擎沒有取消機制，那是獨立功能。
    pub async fn add_candidate(self: &Arc<Self>, from_id: u64, c: browser::sniff::Candidate) -> Option<JoinHandle<()>> {
        let (kind, title) = {
            let items = self.items.lock().unwrap();
            let it = items.iter().find(|i| i.id == from_id)?;
            (it.kind.clone(), it.title.clone())
        };
        let mode = Mode::parse(&kind);
        // 同一頁的不同候選要分得出來，標題帶上網址尾巴
        let label = format!("{title}（{}）", short(&c.url));
        let id = self.push(c.url.clone(), label.clone(), &kind);
        let job = Job::Browser {
            media: c.url,
            title: label,
            headers: c.headers,
            manifest: c.kind == browser::sniff::Kind::Manifest,
            content_type: Some(c.mime),
        };
        let tools = self.tools().await.ok()?;
        let me = self.clone();
        Some(tokio::spawn(async move { me.run(tools, id, job, mode, extract::Options::default()).await }))
    }
```

(i) `fetch()` 加一支：

```rust
            Job::Browser { media, title, headers, manifest, .. } => {
                self.log.info(
                    "browser.fetch",
                    serde_json::json!({ "url": cookies::redact(media), "manifest": manifest, "headers": headers.len() }),
                );
                if *manifest {
                    extract::download(tools, media, mode, opts, None, headers, &self.staging, progress)
                        .await
                        .map(|d| (d.path, d.secs, Some(title.clone())))
                } else {
                    let ext = ext_of(media);
                    let ext = if ext.is_empty() { "mp4".to_string() } else { ext };
                    let raw = self.staging.join(format!("{tag}-raw.{ext}"));
                    direct::download(&self.client, media, &raw, self.cfg.allow_html, headers, progress).await?;
                    if mode == Mode::Audio && is_video_container(&ext) {
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
```

(j) `gate()` 的 `level` match 加：

```rust
            // 清單經 yt-dlp 合併出來一定是媒體；單檔看 Chrome 回報的 MIME
            Job::Browser { manifest: true, .. } => Level::Media,
            Job::Browser { content_type, .. } => content_type
                .as_deref()
                .and_then(verify::level_for_content_type)
                .unwrap_or(Level::Media),
```

(k) `direct_dest()`：`Job::Browser` 回 `None`（跟 Ytdlp 一樣，下載前不預測檔名）——`let Job::Direct {..} = job else { return None }` 已經涵蓋。

(l) `Item.status` 的註解補 `browser`；`Mode::parse` 若沒有 `pub`，補上。

**Step 4: 建置與測試**

Run: `cargo build --workspace && cargo test --workspace`
Expected: 全過。CLI 與 GUI 會因為 `Event::Candidates` 新變體而需要處理——CLI 的 sink 用 `match &ev` 有 `_ => {}`，GUI 的 `main.rs` 看下一個任務。

**Step 5: Commit**

```bash
git add core/src/engine.rs
git commit -m "引擎：Job::Browser、can_browser、瀏覽器 session 的生命週期"
```

---

### Task 11: CLI `--browser`

**Files:**
- Modify: `cli/src/main.rs`

**Step 1: 加旗標**

HELP 加（`--cookies` 那行後面）：

```
      --browser             前三層抓不到時，開一個 Chrome 把頁面跑起來攔截媒體請求
                            （需要機器上有 Chrome / Chromium / Edge / Brave）
```

範例加 `haul --browser https://example.com/player/123`。

`Args` 加 `browser: bool`，parse 加 `"--browser" => a.browser = true,`，`cfg.browser_fallback = args.browser;`。

**Step 2: `--json` 印候選**

sink 的 json 分支不用改——`Event::Candidates` 會自動序列化成 `{"event":"candidates","id":..,"candidates":[..]}`。人類版加一支：

```rust
            Event::Candidates { id, candidates } => {
                eprintln!("[{id}] 瀏覽器偵測到 {} 個媒體，自動挑最像的", candidates.len());
            }
```

`describe()` 的狀態對照加 `"browser" => "瀏覽器偵測中（在視窗裡按播放）"`（看該函式怎麼寫，照既有格式）。

**Step 3: 手動驗證**

Run: `cargo build --release -p haul-cli && ./target/release/haul --browser --json -o /tmp/haul-test <一個 yt-dlp 不認識、但頁面有 <video> 的網址>`
Expected: 先看到 `failed`、接著 `browser`、一行 `candidates`、然後 `downloading` → `done`，離開碼 0。沒有 Chrome 的機器會看到「找不到 Chrome…」。

**Step 4: Commit**

```bash
git add cli/src/main.rs
git commit -m "CLI：--browser 允許落到瀏覽器層"
```

---

### Task 12: GUI — 「用瀏覽器抓」與候選清單

**Files:**
- Modify: `src-tauri/src/main.rs`
- Modify: `ui/index.html`

**Step 1: Tauri 指令與事件**

`main.rs` 加：

```rust
#[tauri::command]
async fn retry_with_browser(app: AppHandle, id: u64) {
    let eng = engine(&app);
    tokio::spawn(async move { eng.retry_with_browser(id).await });
}

#[tauri::command]
async fn add_candidate(app: AppHandle, id: u64, candidate: haul_core::browser::sniff::Candidate) -> Result<(), String> {
    let eng = engine(&app);
    eng.add_candidate(id, candidate).await.map(|_| ()).ok_or_else(|| "找不到原始項目".to_string())
}
```

`Candidate` 需要 `Deserialize`：在 `sniff.rs` 的 `Candidate` 與 `Kind` derive 加 `serde::Deserialize`。

事件轉發的 match 加：

```rust
                Event::Candidates { id, candidates } => {
                    let _ = handle.emit("candidates", serde_json::json!({ "id": id, "candidates": candidates }));
                }
```

`invoke_handler` 註冊兩個新指令。

**Step 2: 前端**

`LABEL` 加 `browser: '瀏覽器偵測中 · 在視窗裡按播放'`。`statLine` 的 `browser` 狀態：`it.total ? \`偵測到 ${it.total} 個媒體\` : LABEL.browser`。

`makeRow` 的 innerHTML 多兩個節點：

```html
<button type="button" class="act" hidden>用瀏覽器抓</button>
<div class="cands" hidden></div>
```

refs 加 `act` 與 `cands`。`paint()`：

```js
    refs.act.hidden = !(it.status === 'failed' && it.canBrowser);
    if (it.status !== 'failed') refs.cands.hidden = true;
```

`act` 按下：`invoke('retry_with_browser', { id: it.id })`（在 makeRow 綁一次，用 `row.item.id`）。

`listen('candidates', e => …)`：找到該列，渲染清單，每筆一行 `型別 · 大小 · 網址尾巴 [抓這個]`，按了 `invoke('add_candidate', { id, candidate })`。自動挑的那筆標「已選」（跟 best 同一規則：第一個 manifest，否則最大的 file——前端照抄，不要另一套邏輯，或者後端在事件裡多帶 `chosen` 索引；**選後端帶索引**，`Event::Candidates` 加 `chosen: Option<usize>`）。

CSS：`.act` 沿用 `.mode` 的樣式（小、安靜）；`.cands` 是縮排的清單，`font: 12px var(--mono)`。

**Step 3: 手動驗證**

Run: `cd src-tauri && cargo tauri dev`
Expected: 貼一個萃取失敗的網址 → 列上出現「用瀏覽器抓」→ 按了 Chrome 視窗開 → 列變「偵測到 N 個媒體」→ 候選清單出現、已選的在下載 → done。再按另一個候選的「抓這個」→ 新的一列出現。

**Step 4: Commit**

```bash
git add src-tauri/src/main.rs ui/index.html core/src/browser/sniff.rs core/src/engine.rs
git commit -m "GUI：用瀏覽器抓、候選清單"
```

---

### Task 13: 文件

**Files:**
- Modify: `README.md`
- Modify: `.claude/skills/haul/SKILL.md`

**Step 1: README**

- CLI 用法表加 `haul --browser <網址>`。
- 「運作方式」的解析鏈表加第四列：`瀏覽器 | 網址只在跑起來的頁面裡才出現的站 | 選配，需要 Chrome / Chromium / Edge / Brave`。
- 新增「### 瀏覽器（選配）」一節，內容從設計文件的「資料流：輔助萃取」濃縮：為什麼是獨立實例、怎麼觸發、候選怎麼挑、看到分段沒清單時的建議、`browser/` profile 在哪、port 只綁 127.0.0.1。
- 限制段落改寫「需要 JS 才出現的媒體」那類說明。
- 專案結構加 `browser/` 三個檔案。

**Step 2: SKILL.md**

- 用法加 `--browser`。
- 「失敗了怎麼辦」加一條：**錯誤說 Unsupported URL / 不像檔案，而且 `haul update` 後還是一樣** → 加 `--browser` 重試；會開一個 Chrome 視窗，若頁面要按播放，請使用者按。看到「分段串流但沒有清單」就是抓不到原檔，據實回報（錄製是第 2 期）。

**Step 3: Commit**

```bash
git add README.md .claude/skills/haul/SKILL.md
git commit -m "文件：瀏覽器輔助萃取"
```

---

## 完成標準

- `cargo test --workspace` 全過；`HAUL_TEST_CHROME=1 cargo test -p haul-core browser` 兩個整合測試通過
- `cargo clippy --workspace` 無警告
- CLI：一個 yt-dlp 不認識的 `<video>` 頁面用 `--browser` 抓得到、驗證通過、離開碼 0；不帶 `--browser` 時失敗訊息不變
- GUI：失敗列上的按鈕、候選清單、「抓這個」都能用；瀏覽器在最後一個項目結束後自動關閉
- 沒有 Chrome 的機器：錯誤訊息說明要裝什麼
