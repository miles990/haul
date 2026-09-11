# 錄製 實作計畫（第 2 期）

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** 根本沒有檔案可抓的頁面（JS 自己組分段的 MSE、WebRTC），用 Haul 的 Chrome 把那個分頁的畫面＋聲音錄成檔案，過 `media` 驗證閘門後存下來，明確標成「錄製」而非「下載」。

**Architecture:** 新模組 `core/src/browser/record.rs`，跟 `sniff.rs` 平行、共用 `chrome.rs` / `cdp.rs`。目標分頁照常載入；另開一個小視窗當面板，在裡面用 `getDisplayMedia` 擷取目標分頁（Chrome 以 `--auto-select-tab-capture-source-by-title` 啟動所以不跳選擇框），`MediaRecorder` 每秒一個 chunk 經 `Runtime.addBinding` 回到 Haul 寫進 staging。停止後 ffmpeg 轉封裝成 mp4 / m4a，走既有的驗證與存檔。引擎多 `Job::Recording` 與 `Item.source`。設計見 `docs/plans/2026-09-11-browser-layer-design.md`。

**Tech Stack:** 既有的 CDP 客戶端；Chrome 的 getDisplayMedia + MediaRecorder；ffmpeg（已隨 Haul 下載）；base64。

**慣例：** 同第 1 期。`cargo test -p haul-core <名稱>`；每個任務結束 commit；註解寫「為什麼」。整合測試一律 `HAUL_TEST_CHROME=1` 閘門。

**先驗證再蓋：** Task 1 是 spike，驗證三個技術假設。任何一個不成立就**停下來回報**，不要繞路硬做——那代表設計要改。

> **Task 1 結果（2026-09-11）**：假設 1 不成立——依標題／名稱自動選分頁的兩個旗標都選不到分頁。
> 改走「目標分頁自己 `getDisplayMedia({preferCurrentTab: true})` + `--auto-accept-this-tab-capture`」，
> 341 ms 零互動成功。**因此 Task 3、5 調整為：沒有面板視窗**，`PANEL_JS` 改名 `RECORD_JS` 注入
> 目標分頁（擷取、MediaRecorder、播放狀態回報、`pagehide` 時停止都在同一段），binding 只在
> 目標 session 上；`TAB_MARK` 與標題替換整個拿掉。Task 3 的 `WATCH_JS` 併進 `RECORD_JS`。

---

### Task 1: Spike — 驗證三個假設

**Files:**
- Modify: `core/src/browser/chrome.rs`（`launch_args` 加一個旗標）
- Create: `core/src/browser/record.rs`（先只放 spike 測試）
- Modify: `core/src/browser/mod.rs`（`pub mod record;`）

**假設：**
1. `--auto-select-tab-capture-source-by-title=<標記>` 會讓 `getDisplayMedia` 不跳選擇框、直接選到標題含標記的分頁
2. `Runtime.evaluate` 帶 `userGesture: true` 能滿足 `getDisplayMedia` 的手勢要求
3. `MediaRecorder.isTypeSupported('video/webm;codecs=h264,opus')` 為真（否則退 vp9，轉封裝要重編）

**Step 1: 啟動旗標**

`chrome.rs` 的 `launch_args` 加（`--autoplay-policy` 之後）：

```rust
        // 錄製用：getDisplayMedia 直接選到標題含這個標記的分頁，不跳選擇框。
        // 啟動時就得給，所以是固定字串；錄製時把目標分頁的標題暫時換成它。
        format!("--auto-select-tab-capture-source-by-title={}", TAB_MARK),
```

並加常數：

```rust
/// 錄製時目標分頁的暫時標題。選一個不會出現在真實網頁上的字串。
pub const TAB_MARK: &str = "⟪Haul⟫";
```

**Step 2: spike 測試**

`record.rs`：

```rust
//! 錄製：把一個分頁的畫面與聲音錄成檔案。

use super::chrome::TAB_MARK;

#[cfg(test)]
mod spike {
    use super::super::{cdp::Cdp, chrome};
    use serde_json::json;

    /// 驗證三個技術假設。任何一個不成立，錄製的設計就要改。
    #[tokio::test]
    async fn assumptions_hold() {
        if std::env::var("HAUL_TEST_CHROME").is_err() {
            eprintln!("略過：未設 HAUL_TEST_CHROME");
            return;
        }
        let exe = chrome::find(None).unwrap();
        let dir = std::env::temp_dir().join("haul-chrome-test");
        let ch = chrome::launch(&exe, &dir).await.unwrap();
        let cdp = Cdp::connect(&ch.ws_url).await.unwrap();

        // 目標分頁：標題設成標記
        let t = cdp.call(None, "Target.createTarget", json!({"url": "about:blank"})).await.unwrap();
        let tid = t["targetId"].as_str().unwrap().to_string();
        let a = cdp.call(None, "Target.attachToTarget", json!({"targetId": tid, "flatten": true})).await.unwrap();
        let ts = a["sessionId"].as_str().unwrap().to_string();
        cdp.call(Some(&ts), "Runtime.evaluate", json!({"expression": format!("document.title = {:?}", super::TAB_MARK)})).await.unwrap();

        // 面板分頁：在這裡呼叫 getDisplayMedia
        let p = cdp.call(None, "Target.createTarget", json!({"url": "about:blank", "newWindow": true, "width": 360, "height": 200})).await.unwrap();
        let pid = p["targetId"].as_str().unwrap().to_string();
        let a = cdp.call(None, "Target.attachToTarget", json!({"targetId": pid, "flatten": true})).await.unwrap();
        let ps = a["sessionId"].as_str().unwrap().to_string();

        let expr = r#"(async () => {
            const h264 = MediaRecorder.isTypeSupported('video/webm;codecs=h264,opus');
            const s = await navigator.mediaDevices.getDisplayMedia({ video: true, audio: true });
            const v = s.getVideoTracks()[0], a = s.getAudioTracks()[0];
            const out = { h264, video: !!v, audio: !!a, label: v ? v.label : '', surface: v ? v.getSettings().displaySurface : '' };
            s.getTracks().forEach(t => t.stop());
            return JSON.stringify(out);
        })()"#;
        let r = cdp.call(Some(&ps), "Runtime.evaluate", json!({
            "expression": expr, "awaitPromise": true, "returnByValue": true, "userGesture": true,
            "timeout": 10000,
        })).await.unwrap();
        let _ = cdp.call(None, "Browser.close", json!({})).await;

        let val = r["result"]["value"].as_str().unwrap_or_else(|| panic!("getDisplayMedia 失敗：{r}"));
        let out: serde_json::Value = serde_json::from_str(val).unwrap();
        eprintln!("spike: {out}");
        assert!(out["video"].as_bool().unwrap(), "沒拿到視訊軌（假設 1/2 不成立）");
        assert!(out["audio"].as_bool().unwrap(), "沒拿到音訊軌 —— 分頁擷取應該有分頁聲音");
        assert_eq!(out["surface"], "browser", "選到的不是分頁");
        assert!(out["h264"].as_bool().unwrap(), "MediaRecorder 不支援 h264（假設 3 不成立，要改轉封裝策略）");
    }
}
```

**Step 3: 跑**

Run: `HAUL_TEST_CHROME=1 cargo test -p haul-core assumptions_hold -- --nocapture`
Expected: PASS，stderr 印出 `spike: {"h264":true,"video":true,"audio":true,...,"surface":"browser"}`。

**若失敗：** 印出的內容告訴你哪個假設錯了。停下來回報，附上輸出。

**Step 4: Commit**

```bash
git add core/src/browser/chrome.rs core/src/browser/record.rs core/src/browser/mod.rs
git commit -m "錄製：spike 驗證分頁擷取的三個假設"
```

---

### Task 2: record.rs — 停止條件狀態機

**Files:**
- Modify: `core/src/browser/record.rs`

三個條件先到先贏：使用者停止、頁面上所有媒體結束且 5 秒沒有新播放、到上限。

**Step 1: 寫失敗的測試**

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    #[test]
    fn stops_at_max_duration() {
        let t0 = Instant::now();
        let mut s = StopWhen::new(t0, Duration::from_secs(10));
        assert_eq!(s.check(t0 + Duration::from_secs(9)), None);
        assert_eq!(s.check(t0 + Duration::from_secs(10)), Some(Stop::MaxDuration));
    }

    #[test]
    fn user_stop_wins_immediately() {
        let t0 = Instant::now();
        let mut s = StopWhen::new(t0, Duration::from_secs(3600));
        s.user_stopped();
        assert_eq!(s.check(t0 + Duration::from_millis(1)), Some(Stop::User));
    }

    #[test]
    fn media_ended_needs_prior_playback_and_quiet_window() {
        let t0 = Instant::now();
        let mut s = StopWhen::new(t0, Duration::from_secs(3600));
        // 一開始沒在播不算「結束」——使用者可能還沒按播放
        s.playing(false, t0 + Duration::from_secs(1));
        assert_eq!(s.check(t0 + Duration::from_secs(30)), None);
        // 播過了
        s.playing(true, t0 + Duration::from_secs(31));
        s.playing(false, t0 + Duration::from_secs(40));
        assert_eq!(s.check(t0 + Duration::from_secs(44)), None, "安靜不到 5 秒");
        assert_eq!(s.check(t0 + Duration::from_secs(45)), Some(Stop::MediaEnded));
    }

    #[test]
    fn new_playback_resets_quiet_window() {
        let t0 = Instant::now();
        let mut s = StopWhen::new(t0, Duration::from_secs(3600));
        s.playing(true, t0 + Duration::from_secs(1));
        s.playing(false, t0 + Duration::from_secs(10));
        s.playing(true, t0 + Duration::from_secs(13)); // 下一首開始
        s.playing(false, t0 + Duration::from_secs(20));
        assert_eq!(s.check(t0 + Duration::from_secs(24)), None);
        assert_eq!(s.check(t0 + Duration::from_secs(25)), Some(Stop::MediaEnded));
    }

    #[test]
    fn parses_human_durations() {
        assert_eq!(parse_duration("90"), Some(Duration::from_secs(90)));
        assert_eq!(parse_duration("90s"), Some(Duration::from_secs(90)));
        assert_eq!(parse_duration("30m"), Some(Duration::from_secs(1800)));
        assert_eq!(parse_duration("2h"), Some(Duration::from_secs(7200)));
        assert_eq!(parse_duration("1h30m"), Some(Duration::from_secs(5400)));
        assert_eq!(parse_duration("abc"), None);
        assert_eq!(parse_duration("0"), None);
    }
}
```

**Step 2: 跑測試確認失敗**

Run: `cargo test -p haul-core browser::record`
Expected: 編譯錯誤。

**Step 3: 實作**

```rust
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stop {
    User,
    MediaEnded,
    MaxDuration,
}

/// 三個停止條件先到先贏。
///
/// 「媒體結束」要先播過才算：使用者可能還沒按播放，一開始的安靜不是結束。
/// 結束後再等 5 秒是給「下一首」或廣告後正片一個機會。
#[derive(Debug)]
pub struct StopWhen {
    start: Instant,
    max: Duration,
    user: bool,
    ever_played: bool,
    quiet_since: Option<Instant>,
}

impl StopWhen {
    pub const QUIET: Duration = Duration::from_secs(5);

    pub fn new(now: Instant, max: Duration) -> Self {
        Self { start: now, max, user: false, ever_played: false, quiet_since: None }
    }

    pub fn user_stopped(&mut self) {
        self.user = true;
    }

    /// 目標分頁每秒回報一次「有沒有媒體在播」
    pub fn playing(&mut self, playing: bool, now: Instant) {
        if playing {
            self.ever_played = true;
            self.quiet_since = None;
        } else if self.ever_played && self.quiet_since.is_none() {
            self.quiet_since = Some(now);
        }
    }

    pub fn check(&mut self, now: Instant) -> Option<Stop> {
        if self.user {
            return Some(Stop::User);
        }
        if now.duration_since(self.start) >= self.max {
            return Some(Stop::MaxDuration);
        }
        match self.quiet_since {
            Some(q) if now.duration_since(q) >= Self::QUIET => Some(Stop::MediaEnded),
            _ => None,
        }
    }
}

/// `90`、`90s`、`30m`、`2h`、`1h30m`。0 或看不懂回 None。
pub fn parse_duration(s: &str) -> Option<Duration> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    if let Ok(n) = s.parse::<u64>() {
        return (n > 0).then(|| Duration::from_secs(n));
    }
    let mut total = 0u64;
    let mut num = String::new();
    for ch in s.chars() {
        if ch.is_ascii_digit() {
            num.push(ch);
            continue;
        }
        let n: u64 = num.parse().ok()?;
        num.clear();
        total += match ch {
            'h' => n * 3600,
            'm' => n * 60,
            's' => n,
            _ => return None,
        };
    }
    if !num.is_empty() {
        return None; // 結尾沒單位
    }
    (total > 0).then(|| Duration::from_secs(total))
}
```

**Step 4: 跑測試**

Run: `cargo test -p haul-core browser::record`
Expected: 5 passed（spike 略過）。

**Step 5: Commit**

```bash
git add core/src/browser/record.rs
git commit -m "錄製：停止條件狀態機與時長解析"
```

---

### Task 3: record.rs — 面板頁與目標分頁的腳本

**Files:**
- Modify: `core/src/browser/record.rs`

兩段 JS 放成 Rust 常數。面板頁負責擷取與錄製、回傳 chunk；目標分頁負責回報播放狀態。

**Step 1: 面板腳本**

```rust
/// 面板頁：擷取目標分頁、錄製、把 chunk 經 binding 送回。
/// 用 `{{MIME}}`、`{{AUDIO_ONLY}}` 佔位，注入前替換。
const PANEL_JS: &str = r#"(async () => {
  document.title = 'Haul 錄製';
  document.body.innerHTML = '<div style="font:14px -apple-system,system-ui;padding:16px;color:#ddd;background:#1b1f24;height:100vh;box-sizing:border-box">'
    + '<div id="s">● 準備中</div>'
    + '<button id="stop" style="margin-top:12px;padding:6px 14px">停止</button></div>';
  document.documentElement.style.background = '#1b1f24';
  const status = document.getElementById('s');
  const say = m => status.textContent = m;
  try {
    const stream = await navigator.mediaDevices.getDisplayMedia({
      video: { frameRate: 30 }, audio: true,
      // 不要把面板自己列進去
      selfBrowserSurface: 'exclude', surfaceSwitching: 'exclude', preferCurrentTab: false,
    });
    const audioOnly = {{AUDIO_ONLY}};
    const src = audioOnly ? new MediaStream(stream.getAudioTracks()) : stream;
    if (audioOnly && src.getAudioTracks().length === 0) throw new Error('分頁沒有聲音軌');
    const rec = new MediaRecorder(src, { mimeType: '{{MIME}}', videoBitsPerSecond: 6e6, audioBitsPerSecond: 160e3 });
    let bytes = 0;
    const t0 = Date.now();
    rec.ondataavailable = async e => {
      if (!e.data || !e.data.size) return;
      bytes += e.data.size;
      const buf = new Uint8Array(await e.data.arrayBuffer());
      let bin = '';
      for (let i = 0; i < buf.length; i += 0x8000) bin += String.fromCharCode.apply(null, buf.subarray(i, i + 0x8000));
      window.haulRec(JSON.stringify({ type: 'chunk', data: btoa(bin) }));
      const s = Math.round((Date.now() - t0) / 1000);
      say('● 錄製中 ' + String(Math.floor(s / 60)).padStart(2, '0') + ':' + String(s % 60).padStart(2, '0') + ' · ' + (bytes / 1048576).toFixed(1) + ' MB');
    };
    rec.onstop = () => { window.haulRec(JSON.stringify({ type: 'stopped' })); say('已停止'); };
    rec.onerror = e => window.haulRec(JSON.stringify({ type: 'error', message: String(e.error || e) }));
    // Chrome 藍條上的「停止分享」會結束 track；那也是合法的停止
    stream.getVideoTracks()[0].onended = () => { if (rec.state !== 'inactive') rec.stop(); };
    document.getElementById('stop').onclick = () => { if (rec.state !== 'inactive') rec.stop(); };
    window.haulStop = () => { if (rec.state !== 'inactive') rec.stop(); };
    rec.start(1000);
    say('● 錄製中');
    window.haulRec(JSON.stringify({ type: 'started' }));
  } catch (e) {
    window.haulRec(JSON.stringify({ type: 'error', message: String(e && e.message || e) }));
  }
})()"#;

/// 目標分頁：每秒回報有沒有媒體在播。
/// 跨網域 iframe 裡的播放器看不到 —— 那種情況只剩使用者停止與上限兩個條件。
const WATCH_JS: &str = r#"(() => {
  if (window.__haulWatch) return;
  window.__haulWatch = setInterval(() => {
    const m = [...document.querySelectorAll('video,audio')];
    const playing = m.some(x => !x.paused && !x.ended && x.readyState > 2);
    try { window.haulMedia(JSON.stringify({ playing, count: m.length })); } catch (_) {}
  }, 1000);
})()"#;

pub fn panel_js(mime: &str, audio_only: bool) -> String {
    PANEL_JS
        .replace("{{MIME}}", mime)
        .replace("{{AUDIO_ONLY}}", if audio_only { "true" } else { "false" })
}

/// 依模式與 Chrome 支援挑錄製格式。h264 優先：轉 mp4 時視訊不必重編。
pub fn pick_mime(audio_only: bool, supported: &[String]) -> Option<String> {
    let prefs: &[&str] = if audio_only {
        &["audio/webm;codecs=opus"]
    } else {
        &["video/webm;codecs=h264,opus", "video/webm;codecs=vp9,opus", "video/webm;codecs=vp8,opus"]
    };
    prefs.iter().find(|p| supported.iter().any(|s| s == *p)).map(|s| s.to_string())
}
```

測試：

```rust
    #[test]
    fn picks_h264_when_available_else_falls_back() {
        let all = vec!["video/webm;codecs=vp9,opus".to_string(), "video/webm;codecs=h264,opus".to_string()];
        assert_eq!(pick_mime(false, &all).unwrap(), "video/webm;codecs=h264,opus");
        let vp9 = vec!["video/webm;codecs=vp9,opus".to_string()];
        assert_eq!(pick_mime(false, &vp9).unwrap(), "video/webm;codecs=vp9,opus");
        assert!(pick_mime(false, &[]).is_none());
        assert_eq!(pick_mime(true, &["audio/webm;codecs=opus".to_string()]).unwrap(), "audio/webm;codecs=opus");
    }

    #[test]
    fn panel_js_substitutes_placeholders() {
        let js = panel_js("video/webm;codecs=h264,opus", true);
        assert!(js.contains("mimeType: 'video/webm;codecs=h264,opus'"));
        assert!(js.contains("const audioOnly = true;"));
        assert!(!js.contains("{{"));
    }
```

**Step 2: 跑測試、Commit**

Run: `cargo test -p haul-core browser::record`
Expected: 7 passed。

```bash
git add core/src/browser/record.rs
git commit -m "錄製：面板頁與目標分頁的腳本、錄製格式挑選"
```

---

### Task 4: record.rs — 轉封裝

**Files:**
- Modify: `core/src/browser/record.rs`

**Step 1: 寫失敗的測試**

```rust
    #[test]
    fn remux_copies_h264_and_reencodes_everything_else() {
        let a = remux_args(false, "video/webm;codecs=h264,opus");
        assert!(a.windows(2).any(|w| w == ["-c:v", "copy"]), "{a:?}");
        assert!(a.windows(2).any(|w| w == ["-c:a", "aac"]));
        assert!(a.contains(&"+faststart".to_string()));
        let b = remux_args(false, "video/webm;codecs=vp9,opus");
        assert!(b.windows(2).any(|w| w == ["-c:v", "libx264"]), "{b:?}");
        let c = remux_args(true, "audio/webm;codecs=opus");
        assert!(c.contains(&"-vn".to_string()));
        assert!(c.windows(2).any(|w| w == ["-c:a", "aac"]));
    }

    #[test]
    fn output_extension_follows_mode() {
        assert_eq!(output_ext(false), "mp4");
        assert_eq!(output_ext(true), "m4a");
    }
```

**Step 2: 實作**

```rust
use anyhow::{bail, Result};
use std::path::Path;

pub fn output_ext(audio_only: bool) -> &'static str {
    if audio_only { "m4a" } else { "mp4" }
}

/// 轉封裝參數。h264 直接 copy；vp8/vp9 得重編成 H.264，因為 macOS 的系統播放器
/// 不吃 WebM 也不吃 mp4 裡的 VP9，會打破「完成的項目點一下就播」。
/// 音訊一律 Opus→AAC，這是唯一一段固定要轉的。
pub fn remux_args(audio_only: bool, mime: &str) -> Vec<String> {
    let mut a: Vec<String> = Vec::new();
    if audio_only {
        a.extend(["-vn", "-c:a", "aac", "-b:a", "192k"].map(String::from));
    } else {
        if mime.contains("h264") {
            a.extend(["-c:v", "copy"].map(String::from));
        } else {
            a.extend(["-c:v", "libx264", "-preset", "veryfast", "-crf", "23", "-pix_fmt", "yuv420p"].map(String::from));
        }
        a.extend(["-c:a", "aac", "-b:a", "160k", "-movflags", "+faststart"].map(String::from));
    }
    a
}

/// MediaRecorder 吐出的 WebM 沒有時長與 cue（它是邊錄邊寫的串流），
/// ffmpeg 讀得懂但會抱怨；轉封裝順便把這些補齊。
pub async fn remux(ffmpeg: &Path, src: &Path, dest: &Path, audio_only: bool, mime: &str) -> Result<()> {
    let out = tokio::process::Command::new(ffmpeg)
        .args(["-v", "error", "-nostdin", "-y", "-i"])
        .arg(src)
        .args(remux_args(audio_only, mime))
        .arg(dest)
        .stdin(std::process::Stdio::null())
        .output()
        .await?;
    if !out.status.success() {
        let msg = String::from_utf8_lossy(&out.stderr);
        let first = msg.lines().find(|l| !l.trim().is_empty()).unwrap_or("");
        bail!("錄製轉檔失敗：{first}");
    }
    Ok(())
}
```

**Step 3: 跑測試、Commit**

```bash
git add core/src/browser/record.rs
git commit -m "錄製：轉封裝成 mp4 / m4a"
```

---

### Task 5: record.rs — 錄製流程（含整合測試）

**Files:**
- Modify: `core/src/browser/record.rs`

**Step 1: 整合測試**

本機 server 一頁自動播放 3 秒的 mp4（`testsrc` + 440 Hz，Task 5 的測試自己用 ffmpeg 產）。錄到媒體結束自動停 → 轉封裝 → 檔案存在且 > 10 KB。驗證閘門的部分在引擎整合（Task 6）再測。

```rust
    /// 用 Haul 自己下載的 ffmpeg 產一支 3 秒的真影片。找不到就略過。
    fn make_clip(dir: &std::path::Path) -> Option<std::path::PathBuf> {
        let ffmpeg = crate::engine::default_bin_dir().join("ffmpeg");
        if !ffmpeg.is_file() { return None; }
        let out = dir.join("clip.mp4");
        let ok = std::process::Command::new(&ffmpeg)
            .args(["-y", "-loglevel", "error", "-f", "lavfi", "-i", "testsrc=size=320x240:rate=25",
                   "-f", "lavfi", "-i", "sine=frequency=440:sample_rate=44100", "-t", "3",
                   "-c:v", "libx264", "-pix_fmt", "yuv420p", "-c:a", "aac", "-shortest"])
            .arg(&out).status().ok()?.success();
        ok.then_some(out)
    }

    fn clip_server(clip: Vec<u8>) -> (String, std::thread::JoinHandle<()>) {
        use std::io::{Read, Write};
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", l.local_addr().unwrap());
        let page = format!("<!doctype html><title>Clip</title><video autoplay src=\"{base}/clip.mp4\"></video>");
        let h = std::thread::spawn(move || {
            for _ in 0..32 {
                let Ok((mut s, _)) = l.accept() else { break };
                let mut buf = [0u8; 4096];
                let n = s.read(&mut buf).unwrap_or(0);
                let req = String::from_utf8_lossy(&buf[..n]);
                let (ct, body): (&str, &[u8]) = if req.contains("/clip.mp4") { ("video/mp4", &clip) } else { ("text/html", page.as_bytes()) };
                // Range：Chrome 播 mp4 會要 Range，不給 206 它可能整支重抓，但也能播
                let _ = write!(s, "HTTP/1.1 200 OK\r\nContent-Type: {ct}\r\nContent-Length: {}\r\nAccept-Ranges: none\r\nConnection: close\r\n\r\n", body.len());
                let _ = s.write_all(body);
            }
        });
        (base, h)
    }

    #[tokio::test]
    async fn records_a_tab_until_its_media_ends() {
        if std::env::var("HAUL_TEST_CHROME").is_err() {
            eprintln!("略過：未設 HAUL_TEST_CHROME");
            return;
        }
        let dir = std::env::temp_dir().join("haul-record-test");
        std::fs::create_dir_all(&dir).unwrap();
        let Some(clip) = make_clip(&dir) else { eprintln!("略過：沒有 ffmpeg"); return; };
        let (base, _srv) = clip_server(std::fs::read(&clip).unwrap());

        let exe = chrome::find(None).unwrap();
        let ch = chrome::launch(&exe, &std::env::temp_dir().join("haul-chrome-test")).await.unwrap();
        let cdp = Cdp::connect(&ch.ws_url).await.unwrap();

        let webm = dir.join("rec.webm");
        let (stop_tx, stop_rx) = tokio::sync::watch::channel(false);
        let mut last = (0u64, 0u64);
        let out = record(&cdp, &format!("{base}/"), false, &webm, Duration::from_secs(60), stop_rx, |bytes, secs| last = (bytes, secs))
            .await
            .unwrap();
        drop(stop_tx);
        let _ = cdp.call(None, "Browser.close", json!({})).await;

        assert_eq!(out.stop, Stop::MediaEnded, "3 秒的片播完應該自動停");
        assert!(out.bytes > 10_240, "只有 {} bytes", out.bytes);
        assert!(webm.is_file());
        assert!(last.0 > 0 && last.1 >= 3, "進度回呼：{last:?}");
        assert_eq!(out.title, "Clip");
    }
```

**Step 2: 實作**

```rust
use super::cdp::Cdp;
use super::chrome::TAB_MARK;
use base64::Engine as _;
use serde_json::{json, Value};
use std::sync::Arc;
use tokio::io::AsyncWriteExt;
use tokio::sync::watch;

pub struct Recorded {
    pub stop: Stop,
    pub bytes: u64,
    pub secs: u64,
    pub mime: String,
    pub title: String,
}

/// 開目標分頁、開面板、錄到某個停止條件成立。chunk 邊收邊寫進 `dest`。
///
/// `stop` 是外部的停止訊號（GUI 按鈕、CLI Ctrl-C）。`progress(bytes, secs)` 每個
/// chunk 叫一次。
pub async fn record(
    cdp: &Arc<Cdp>,
    url: &str,
    audio_only: bool,
    dest: &Path,
    max: Duration,
    mut stop: watch::Receiver<bool>,
    mut progress: impl FnMut(u64, u64),
) -> Result<Recorded> {
    let mut events = cdp.subscribe();

    // 目標分頁
    let (target_id, ts) = open(cdp, url, false).await?;
    cdp.call(Some(&ts), "Page.enable", json!({})).await?;
    cdp.call(Some(&ts), "Runtime.enable", json!({})).await?;
    cdp.call(Some(&ts), "Runtime.addBinding", json!({ "name": "haulMedia" })).await?;
    // 等頁面載到可以拿標題的程度；不等完全載完 —— 直播頁永遠載不完
    tokio::time::sleep(Duration::from_millis(1500)).await;
    let title = eval_str(cdp, &ts, "document.title").await.unwrap_or_default();
    cdp.call(Some(&ts), "Runtime.evaluate", json!({ "expression": WATCH_JS })).await?;

    // 面板：先問 Chrome 支援什麼格式
    let (panel_id, ps) = open(cdp, "about:blank", true).await?;
    cdp.call(Some(&ps), "Runtime.enable", json!({})).await?;
    cdp.call(Some(&ps), "Runtime.addBinding", json!({ "name": "haulRec" })).await?;
    let supported: Vec<String> = eval_json(cdp, &ps, r#"JSON.stringify(['video/webm;codecs=h264,opus','video/webm;codecs=vp9,opus','video/webm;codecs=vp8,opus','audio/webm;codecs=opus'].filter(m => MediaRecorder.isTypeSupported(m)))"#)
        .await
        .and_then(|v| serde_json::from_value(v).ok())
        .unwrap_or_default();
    let mime = pick_mime(audio_only, &supported)
        .ok_or_else(|| anyhow!("這個瀏覽器的 MediaRecorder 不支援任何可用格式"))?;

    // 目標分頁的標題暫時換成標記，讓 getDisplayMedia 自動選到它
    let _ = cdp.call(Some(&ts), "Runtime.evaluate", json!({ "expression": format!("document.title = {:?}", TAB_MARK) })).await;
    cdp.call(Some(&ps), "Runtime.evaluate", json!({
        "expression": panel_js(&mime, audio_only), "userGesture": true,
    })).await?;

    let mut file = tokio::fs::File::create(dest).await?;
    let mut when = StopWhen::new(Instant::now(), max);
    let start = Instant::now();
    let mut bytes = 0u64;
    let mut started = false;
    let mut stopped = false;
    let mut outcome: Option<Stop> = None;

    loop {
        // 停止條件
        if outcome.is_none() {
            if let Some(s) = when.check(Instant::now()) {
                outcome = Some(s);
                // 叫面板停；最後一個 chunk 與 stopped 事件會跟著來
                let _ = cdp.call(Some(&ps), "Runtime.evaluate", json!({ "expression": "window.haulStop && window.haulStop()" })).await;
            }
        }
        if stopped {
            break;
        }
        let ev = tokio::select! {
            r = events.recv() => match r {
                Ok(ev) => ev,
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(_) => bail!("瀏覽器連線已關閉"),
            },
            _ = stop.changed() => { if *stop.borrow() { when.user_stopped(); } continue; }
            _ = tokio::time::sleep(Duration::from_millis(500)) => continue,
        };
        match ev.method.as_str() {
            "Runtime.bindingCalled" if ev.session_id.as_deref() == Some(&ps) => {
                let Ok(msg) = serde_json::from_str::<Value>(ev.params["payload"].as_str().unwrap_or("")) else { continue };
                match msg["type"].as_str() {
                    Some("started") => {
                        started = true;
                        // 標題復原，使用者不該一直看到標記
                        let _ = cdp.call(Some(&ts), "Runtime.evaluate", json!({ "expression": format!("document.title = {:?}", title) })).await;
                    }
                    Some("chunk") => {
                        let data = base64::engine::general_purpose::STANDARD
                            .decode(msg["data"].as_str().unwrap_or(""))
                            .unwrap_or_default();
                        bytes += data.len() as u64;
                        file.write_all(&data).await?;
                        progress(bytes, start.elapsed().as_secs());
                    }
                    Some("stopped") => {
                        stopped = true;
                        // 藍條的「停止分享」：我們沒下令，那就是使用者停的
                        outcome.get_or_insert(Stop::User);
                    }
                    Some("error") => bail!("錄製失敗：{}", msg["message"].as_str().unwrap_or("?")),
                    _ => {}
                }
            }
            "Runtime.bindingCalled" if ev.session_id.as_deref() == Some(&ts) => {
                if let Ok(m) = serde_json::from_str::<Value>(ev.params["payload"].as_str().unwrap_or("")) {
                    when.playing(m["playing"].as_bool().unwrap_or(false), Instant::now());
                }
            }
            "Target.targetDestroyed" => {
                let gone = ev.params["targetId"].as_str();
                if gone == Some(&target_id) {
                    // 分頁被關：已錄到的照收尾
                    outcome.get_or_insert(Stop::User);
                    stopped = true;
                } else if gone == Some(&panel_id) {
                    if !started { bail!("錄製面板被關閉"); }
                    outcome.get_or_insert(Stop::User);
                    stopped = true;
                }
            }
            _ => {}
        }
    }
    file.flush().await?;
    drop(file);

    let _ = cdp.call(None, "Target.closeTarget", json!({ "targetId": panel_id })).await;
    let _ = cdp.call(None, "Target.closeTarget", json!({ "targetId": target_id })).await;

    if bytes == 0 {
        bail!("什麼都沒錄到");
    }
    Ok(Recorded { stop: outcome.unwrap_or(Stop::User), bytes, secs: start.elapsed().as_secs(), mime, title })
}

async fn open(cdp: &Cdp, url: &str, new_window: bool) -> Result<(String, String)> {
    let mut p = json!({ "url": url });
    if new_window {
        p["newWindow"] = json!(true);
        p["width"] = json!(360);
        p["height"] = json!(200);
    }
    let t = cdp.call(None, "Target.createTarget", p).await?;
    let tid = t["targetId"].as_str().ok_or_else(|| anyhow!("沒有 targetId"))?.to_string();
    let a = cdp.call(None, "Target.attachToTarget", json!({ "targetId": tid, "flatten": true })).await?;
    let sid = a["sessionId"].as_str().ok_or_else(|| anyhow!("沒有 sessionId"))?.to_string();
    Ok((tid, sid))
}

async fn eval_json(cdp: &Cdp, sid: &str, expr: &str) -> Option<Value> {
    let r = cdp.call(Some(sid), "Runtime.evaluate", json!({ "expression": expr, "returnByValue": true })).await.ok()?;
    let s = r["result"]["value"].as_str()?;
    serde_json::from_str(s).ok()
}

async fn eval_str(cdp: &Cdp, sid: &str, expr: &str) -> Option<String> {
    let r = cdp.call(Some(sid), "Runtime.evaluate", json!({ "expression": expr, "returnByValue": true })).await.ok()?;
    r["result"]["value"].as_str().map(str::to_string)
}
```

**Step 3: 跑**

Run: `HAUL_TEST_CHROME=1 cargo test -p haul-core records_a_tab -- --nocapture`
Expected: PASS，約 10 秒（1.5 秒載入 + 3 秒片 + 5 秒安靜）。

**Step 4: Commit**

```bash
git add core/src/browser/record.rs
git commit -m "錄製：分頁擷取的完整流程，含真實 Chrome 整合測試"
```

---

### Task 6: 引擎整合 — Job::Recording、Item.source、停止

**Files:**
- Modify: `core/src/engine.rs`

**Step 1: 型別**

`Item` 加：

```rust
    /// 產出是原檔還是錄製。錄製「能不能播」跟「是不是原檔」是兩個問題，
    /// verified 講前者，這裡講後者。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    /// 這個項目可以改用錄製（萃取失敗、或瀏覽器偵測不到可下載的媒體）
    #[serde(default)]
    pub can_record: bool,
```

`push()` 補 `source: None, can_record: false`。`status` 註解加 `recording`。

`Job` 加：

```rust
    /// 錄製。不經 fetch —— 檔案是邊錄邊寫出來的，run_recording 直接接到驗證
    Recording {
        url: String,
        title: String,
    },
```

`Config` 加 `pub record_max: Duration`（預設 3 小時）。

`Engine` 加：

```rust
    /// 進行中的錄製，id → 停止訊號
    recordings: Mutex<HashMap<u64, tokio::sync::watch::Sender<bool>>>,
```

**Step 2: 失敗時標 can_record**

`fail_ex` 改成 `fail_ex(id, why, can_browser, can_record)`；`add()` 的萃取失敗兩個都 true；`retry_with_browser` 的偵測失敗 `fail_ex(id, e, false, true)`。所有其他 `fail` 呼叫維持 `(false, false)`。

**Step 3: 錄製流程**

```rust
    /// 從 CLI 進來：新項目直接錄
    pub async fn add_recording(self: &Arc<Self>, url: String, mode: Mode) -> (u64, JoinHandle<()>) {
        let kind = if mode == Mode::Audio { "audio" } else { "video" };
        let id = self.push(url.clone(), short(&url), kind);
        let me = self.clone();
        (id, tokio::spawn(async move { me.run_recording(id, url, mode).await }))
    }

    /// 從 GUI 進來：既有的失敗項目改用錄製
    pub async fn record_item(self: &Arc<Self>, id: u64) {
        let Some(item) = self.items.lock().unwrap().iter().find(|i| i.id == id).cloned() else { return };
        self.run_recording(id, item.input, Mode::parse(&item.kind)).await;
    }

    pub fn stop_recording(&self, id: u64) -> bool {
        match self.recordings.lock().unwrap().get(&id) {
            Some(tx) => { let _ = tx.send(true); true }
            None => false,
        }
    }

    async fn run_recording(self: &Arc<Self>, id: u64, url: String, mode: Mode) {
        let audio_only = mode == Mode::Audio;
        self.update(id, |i| {
            i.status = "recording".into();
            i.error = None;
            i.can_browser = false;
            i.can_record = false;
            i.source = Some("recording".into());
            i.bytes = 0;
            i.total = 0;
            i.secs = None;
        });
        let (tx, rx) = tokio::sync::watch::channel(false);
        self.recordings.lock().unwrap().insert(id, tx);
        self.browser_users.fetch_add(1, Ordering::SeqCst);

        let webm = self.staging.join(format!("{id}-rec.webm"));
        let outcome = async {
            let cdp = self.browser_session().await?;
            browser::record::record(&cdp, &url, audio_only, &webm, self.cfg.record_max, rx, |bytes, secs| {
                self.update(id, |i| { i.bytes = bytes; i.secs = Some(secs as f64); });
            })
            .await
            .map_err(|e| e.to_string())
        }
        .await;

        self.recordings.lock().unwrap().remove(&id);
        self.browser_release().await;

        let rec = match outcome {
            Ok(r) => r,
            Err(e) => { let _ = tokio::fs::remove_file(&webm).await; return self.fail(id, e); }
        };
        self.log.info("record.stopped", serde_json::json!({
            "id": id, "why": format!("{:?}", rec.stop), "bytes": rec.bytes, "secs": rec.secs, "mime": rec.mime,
        }));

        let tools = match self.tools().await { Ok(t) => t, Err(e) => return self.fail(id, e) };

        // 轉封裝
        let ext = browser::record::output_ext(audio_only);
        let staged = self.staging.join(format!("{id}-rec.{ext}"));
        self.update(id, |i| i.status = "verifying".into());
        if let Err(e) = browser::record::remux(&tools.ffmpeg, &webm, &staged, audio_only, &rec.mime).await {
            let _ = tokio::fs::remove_file(&webm).await;
            return self.fail(id, e.to_string());
        }
        let _ = tokio::fs::remove_file(&webm).await;

        // 驗證與存檔：跟其他 job 一樣的閘門與搬移，但 job 是 Recording
        let title = if rec.title.trim().is_empty() { short(&url) } else { rec.title.clone() };
        let job = Job::Recording { url: url.clone(), title: format!("{title}（錄製）") };
        self.update(id, |i| i.title = title.clone());
        self.finish_staged(&tools, id, &job, mode, staged, Some(rec.secs as f64)).await;
    }
```

`finish_staged` 是把 `run()` 的「2. 驗證」「3. 搬移」兩段抽出來的共用函式——`run()` 改成呼叫它。簽章：

```rust
    /// 一個已經在 staging 的檔案：驗證（限流）→ 過關才搬進正式資料夾。
    /// run() 與錄製共用；錄製沒有下載階段所以直接從這裡進來。
    async fn finish_staged(&self, tools: &Tools, id: u64, job: &Job, mode: Mode, staged: PathBuf, reported: Option<f64>)
```

`forced_stem`：`Job::Recording { title, .. }` 用 `title`。`gate()` 的 `level` match 加 `Job::Recording { .. } => Level::Media`。`add()` 的 `key` match 與 `input.resolved` 的 `source` match 補 `Recording` 分支（`source: "recording"`）。`direct_dest()` 的 `let else` 已涵蓋。

**Step 4: 測試**

`engine.rs` 沒有直接測 run 的單元測試；靠建置與下一個任務的 CLI 端對端。

Run: `cargo build --workspace && cargo test --workspace`
Expected: 全過。

**Step 5: Commit**

```bash
git add core/src/engine.rs
git commit -m "引擎：錄製的項目、停止訊號、與驗證存檔共用的收尾"
```

---

### Task 7: CLI `haul record`

**Files:**
- Modify: `cli/src/main.rs`

**Step 1: 子命令**

`Cmd` 加 `Record`。parse：第一個非旗標參數是 `record` 時 `cmd = Cmd::Record`（照 `status` / `logs` / `update` 的寫法）。旗標 `--max <時長>`（`browser::record::parse_duration`，預設 3h）。HELP 加：

```
  haul record <網址>            # 錄製：分頁的畫面＋聲音（沒有檔案可抓時用）
  haul record -a <網址>         # 只錄聲音
      --max <時長>              # 錄製上限，例如 30m、2h（預設 3h）
```

說明段加一句：`按 Ctrl-C 停止錄製，會等轉檔與驗證做完才結束。`

**Step 2: 執行**

```rust
async fn record(args: Args, eng: Arc<Engine>) -> ExitCode {
    let Some(url) = args.urls.first().cloned() else {
        eprintln!("錯誤：record 需要一個網址");
        return ExitCode::from(2);
    };
    let (id, handle) = eng.add_recording(url, args.mode).await;

    // Ctrl-C 是停止錄製，不是砍程序：要等轉檔與驗證做完
    let e2 = eng.clone();
    tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            eprintln!("\n停止錄製，轉檔與驗證中…");
            e2.stop_recording(id);
        }
    });
    let _ = handle.await;
    // 離開碼照 main 的規則：看項目最終狀態
    ...
}
```

`describe()` 加：

```rust
        "recording" => format!(
            "[{}] ● 錄製中 {}  {}  {:.1} MB",
            i.id, i.title, clock(i.secs.unwrap_or(0.0)), i.bytes as f64 / 1048576.0
        ),
```

（`clock` 若不存在就寫一個 `mm:ss`。）人類版的進度用 `\r` 蓋同一行，不要一秒一行。

**Step 3: 端對端**

本機 server 一頁自動播放 3 秒片（沿用 Task 5 的 server 邏輯，寫成 scratchpad 的 python）：

Run: `haul record --json -o /tmp/haul-rec <url>`
Expected: `recording` 狀態的 item 事件 bytes/secs 遞增 → `verifying` → `done`，`verified: media`、`source: recording`、檔名 `Clip（錄製）.mp4`，離開碼 0。再跑 `haul record -a` 得到 `.m4a`。Ctrl-C 一次：正常收尾、離開碼 0。

**Step 4: Commit**

```bash
git add cli/src/main.rs
git commit -m "CLI：haul record"
```

---

### Task 8: GUI — 改用錄製、錄製中、停止

**Files:**
- Modify: `src-tauri/src/main.rs`
- Modify: `ui/index.html`

**Step 1: 指令**

```rust
#[tauri::command]
fn record_item(app: AppHandle, id: u64) {
    let eng = engine(&app);
    tauri::async_runtime::spawn(async move { eng.record_item(id).await });
}

#[tauri::command]
fn stop_recording(app: AppHandle, id: u64) -> bool {
    engine(&app).stop_recording(id)
}
```

註冊到 `invoke_handler`。

**Step 2: 前端**

- `LABEL.recording = '錄製中'`；`statLine` 的 `recording`：`● ${clock(it.secs||0)} · ${mb(it.bytes)}`。
- 列上第二顆按鈕 `.act2`（跟 `.act` 同樣式，排在旁邊）：
  - `failed && canRecord` → 顯示 `改用錄製` / `改用錄製（只要聲音）`（依 `it.kind`）；`canBrowser` 為 false 時（瀏覽器偵測失敗）這顆是唯一的動作，加 class `primary` 讓它明顯
  - `recording` → 顯示 `停止`，按了 `invoke('stop_recording', { id })`
- `done` 且 `it.source === 'recording'`：stat 加「錄製」字樣：`已驗證 · 錄製 · 3:00`。
- `NEEDS_TOOLS` 加 `recording`。

**Step 3: jsdom 測試**

沿用第 1 期的 `scratchpad/uitest/run.js` 手法，加：

- failed + canRecord + canBrowser → 兩顆都在；failed + canRecord + !canBrowser → 只有錄製且 primary
- 按「改用錄製」→ invoke `record_item`
- recording 狀態 → 顯示時間與大小、按鈕變「停止」→ invoke `stop_recording`
- done + source=recording → stat 含「錄製」

**Step 4: Commit**

```bash
git add src-tauri/src/main.rs ui/index.html
git commit -m "GUI：改用錄製、錄製中的進度與停止"
```

---

### Task 9: 文件

**Files:**
- Modify: `README.md`
- Modify: `.claude/skills/haul/SKILL.md`

**README：**
- CLI 用法表加 `haul record` 三行
- 「運作方式」加「### 錄製」一節：什麼時候用（分段沒清單、WebRTC）、怎麼錄（分頁擷取、只錄那個分頁、不要系統權限）、三個停止條件、輸出 mp4/m4a 與為什麼、`source: "recording"` 與 `verified` 的區別、Chrome 藍條是誠實訊號、DRM 錄出來是黑畫面（預期）、跨網域 iframe 的播放器看不到所以只能手動停或等上限
- 限制段：把「規劃中」拿掉
- 專案結構加 `record.rs`

**SKILL.md：**
- 用法加 `haul record`
- 「失敗了怎麼辦」第 1 條的「錄製功能規劃中」改成：看到「分段串流但沒有清單」→ `haul record <網址>`，跟使用者說會開 Chrome、錄製是即時的（3 分鐘的片要錄 3 分鐘）、按 Ctrl-C 停
- 「做不到的事」加：錄製通話類內容前要提醒使用者取得參與者同意；DRM 內容錄出來是黑畫面，不要嘗試
- `--json` 的欄位說明加 `source`

**Commit：**

```bash
git add README.md .claude/skills/haul/SKILL.md
git commit -m "文件：錄製"
```

---

## 完成標準

- `cargo test --workspace` 全過；`HAUL_TEST_CHROME=1 cargo test -p haul-core browser` 四個整合測試（含 spike 與錄製）通過
- `cargo clippy --workspace` 無警告
- CLI：`haul record` 對一頁自動播放的片，錄到自動停、`verified: media`、`source: recording`、離開碼 0；`-a` 出 m4a；Ctrl-C 正常收尾
- GUI：jsdom 測試通過；app 啟動不崩
- 瀏覽器在錄製結束後自動關閉，沒有孤兒程序
