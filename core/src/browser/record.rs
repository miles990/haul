//! 錄製：把一個分頁的畫面與聲音錄成檔案。
//!
//! 擷取與錄製都跑在目標分頁裡：注入的腳本呼叫 `getDisplayMedia({preferCurrentTab})`
//! 擷取自己，Chrome 以 `--auto-accept-this-tab-capture` 啟動所以不跳選擇框。
//! 曾經想另開一個面板頁去選目標分頁，但 Chrome 的「依標題自動選」旗標實測選不到
//! 分頁，只有自己擷取自己是零互動的。代價：分頁換頁會殺掉錄製器，所以監聽
//! pagehide 先停下來，已收到的 chunk 照收尾。


use anyhow::{bail, Result};
use std::path::Path;
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
        Self {
            start: now,
            max,
            user: false,
            ever_played: false,
            quiet_since: None,
        }
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

/// 注入目標分頁的腳本：擷取自己、錄製、每秒回報播放狀態，chunk 經 binding 送回。
/// 用 `{{MIME}}`、`{{AUDIO_ONLY}}` 佔位，注入前替換。
///
/// 全部包在一個 IIFE 裡、只掛兩個全域（haulStop、__haulWatch），盡量不碰頁面自己的東西。
const RECORD_JS: &str = r#"(async () => {
  const send = o => { try { window.haulRec(JSON.stringify(o)); } catch (_) {} };
  try {
    const stream = await navigator.mediaDevices.getDisplayMedia({
      video: { frameRate: 30 }, audio: true, preferCurrentTab: true,
    });
    const audioOnly = {{AUDIO_ONLY}};
    const src = audioOnly ? new MediaStream(stream.getAudioTracks()) : stream;
    if (audioOnly && src.getAudioTracks().length === 0) throw new Error('分頁沒有聲音軌');
    const rec = new MediaRecorder(src, { mimeType: '{{MIME}}', videoBitsPerSecond: 6e6, audioBitsPerSecond: 160e3 });
    rec.ondataavailable = async e => {
      if (!e.data || !e.data.size) return;
      const buf = new Uint8Array(await e.data.arrayBuffer());
      let bin = '';
      for (let i = 0; i < buf.length; i += 0x8000) bin += String.fromCharCode.apply(null, buf.subarray(i, i + 0x8000));
      send({ type: 'chunk', data: btoa(bin) });
    };
    rec.onstop = () => { clearInterval(window.__haulWatch); stream.getTracks().forEach(t => t.stop()); send({ type: 'stopped' }); };
    rec.onerror = e => send({ type: 'error', message: String(e.error || e) });
    const stop = () => { if (rec.state !== 'inactive') rec.stop(); };
    // Chrome 藍條上的「停止分享」會結束 track；那也是合法的停止
    stream.getVideoTracks()[0].onended = stop;
    // 換頁會殺掉這裡的一切：先停，讓最後一個 chunk 送出去
    window.addEventListener('pagehide', stop);
    window.haulStop = stop;
    // 每秒回報有沒有媒體在播，給「播完自動停」用。
    // 跨網域 iframe 裡的播放器看不到 —— 那種情況只剩使用者停止與上限
    window.__haulWatch = setInterval(() => {
      const m = [...document.querySelectorAll('video,audio')];
      const playing = m.some(x => !x.paused && !x.ended && x.readyState > 2);
      send({ type: 'playing', playing, count: m.length });
    }, 1000);
    rec.start(1000);
    send({ type: 'started' });
  } catch (e) {
    send({ type: 'error', message: String(e && e.message || e) });
  }
})()"#;

pub fn record_js(mime: &str, audio_only: bool) -> String {
    RECORD_JS
        .replace("{{MIME}}", mime)
        .replace("{{AUDIO_ONLY}}", if audio_only { "true" } else { "false" })
}

/// 依模式與 Chrome 支援挑錄製格式。h264 優先：轉 mp4 時視訊不必重編。
pub fn pick_mime(audio_only: bool, supported: &[String]) -> Option<String> {
    let prefs: &[&str] = if audio_only {
        &["audio/webm;codecs=opus"]
    } else {
        &[
            "video/webm;codecs=h264,opus",
            "video/webm;codecs=vp9,opus",
            "video/webm;codecs=vp8,opus",
        ]
    };
    prefs
        .iter()
        .find(|p| supported.iter().any(|s| s == *p))
        .map(|s| s.to_string())
}

/// 問 Chrome 支援哪些格式的表達式，回 JSON 陣列字串
pub const SUPPORTED_JS: &str = r#"JSON.stringify(['video/webm;codecs=h264,opus','video/webm;codecs=vp9,opus','video/webm;codecs=vp8,opus','audio/webm;codecs=opus'].filter(m => MediaRecorder.isTypeSupported(m)))"#;

pub fn output_ext(audio_only: bool) -> &'static str {
    if audio_only {
        "m4a"
    } else {
        "mp4"
    }
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
            a.extend(
                [
                    "-c:v", "libx264", "-preset", "veryfast", "-crf", "23", "-pix_fmt", "yuv420p",
                ]
                .map(String::from),
            );
        }
        a.extend(["-c:a", "aac", "-b:a", "160k", "-movflags", "+faststart"].map(String::from));
    }
    a
}

/// MediaRecorder 吐出的 WebM 沒有時長與 cue（它是邊錄邊寫的串流），
/// ffmpeg 讀得懂但會抱怨；轉封裝順便把這些補齊。
pub async fn remux(
    ffmpeg: &Path,
    src: &Path,
    dest: &Path,
    audio_only: bool,
    mime: &str,
) -> Result<()> {
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

#[cfg(test)]
mod tests {
    use super::*;

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
        assert_eq!(parse_duration("5x"), None);
    }

    #[test]
    fn picks_h264_when_available_else_falls_back() {
        let all = vec![
            "video/webm;codecs=vp9,opus".to_string(),
            "video/webm;codecs=h264,opus".to_string(),
        ];
        assert_eq!(pick_mime(false, &all).unwrap(), "video/webm;codecs=h264,opus");
        let vp9 = vec!["video/webm;codecs=vp9,opus".to_string()];
        assert_eq!(pick_mime(false, &vp9).unwrap(), "video/webm;codecs=vp9,opus");
        assert!(pick_mime(false, &[]).is_none());
        assert_eq!(
            pick_mime(true, &["audio/webm;codecs=opus".to_string()]).unwrap(),
            "audio/webm;codecs=opus"
        );
    }

    #[test]
    fn record_js_substitutes_placeholders() {
        let js = record_js("video/webm;codecs=h264,opus", true);
        assert!(js.contains("mimeType: 'video/webm;codecs=h264,opus'"));
        assert!(js.contains("const audioOnly = true;"));
        assert!(!js.contains("{{"));
    }

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
}

#[cfg(test)]
mod spike {
    use super::super::{cdp::Cdp, chrome};
    use serde_json::json;

    /// 驗證三個技術假設。任何一個不成立，錄製的設計就要改：
    /// 1. --auto-accept-this-tab-capture 讓分頁自己 getDisplayMedia 不跳選擇框
    /// 2. Runtime.evaluate 的 userGesture 滿足手勢要求
    /// 3. MediaRecorder 支援 h264（否則轉 mp4 要重編）
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

        // 目標分頁要是 secure context 才有 navigator.mediaDevices；
        // about:blank 不是，file:// 是。真實網站是 https，同樣成立。
        let page = dir.join("target.html");
        std::fs::write(&page, "<!doctype html><title>Target</title><body>target").unwrap();
        let t = cdp
            .call(None, "Target.createTarget", json!({"url": format!("file://{}", page.display())}))
            .await
            .unwrap();
        let tid = t["targetId"].as_str().unwrap().to_string();
        let a = cdp
            .call(None, "Target.attachToTarget", json!({"targetId": tid, "flatten": true}))
            .await
            .unwrap();
        let ts = a["sessionId"].as_str().unwrap().to_string();
        tokio::time::sleep(std::time::Duration::from_millis(800)).await;

        let expr = r#"(async () => {
            const h264 = MediaRecorder.isTypeSupported('video/webm;codecs=h264,opus');
            const t = new Promise((_, rej) => setTimeout(() => rej(new Error('timeout 8s')), 8000));
            const s = await Promise.race([
                navigator.mediaDevices.getDisplayMedia({ video: true, audio: true, preferCurrentTab: true }), t]);
            const v = s.getVideoTracks()[0], a = s.getAudioTracks()[0];
            const out = { h264, video: !!v, audio: !!a, surface: v ? v.getSettings().displaySurface : '' };
            s.getTracks().forEach(x => x.stop());
            return JSON.stringify(out);
        })()"#;
        let t0 = std::time::Instant::now();
        let r = cdp
            .call(
                Some(&ts),
                "Runtime.evaluate",
                json!({ "expression": expr, "awaitPromise": true, "returnByValue": true, "userGesture": true }),
            )
            .await
            .unwrap();
        let _ = cdp.call(None, "Browser.close", json!({})).await;

        let val = r["result"]["value"]
            .as_str()
            .unwrap_or_else(|| panic!("getDisplayMedia 失敗：{r}"));
        let out: serde_json::Value = serde_json::from_str(val).unwrap();
        eprintln!("spike（{} ms）: {out}", t0.elapsed().as_millis());
        assert!(out["video"].as_bool().unwrap(), "沒拿到視訊軌（假設 1/2 不成立）：{out}");
        assert!(out["audio"].as_bool().unwrap(), "沒拿到音訊軌：{out}");
        assert_eq!(out["surface"], "browser", "選到的不是分頁：{out}");
        assert!(out["h264"].as_bool().unwrap(), "MediaRecorder 不支援 h264（假設 3 不成立）");
        assert!(t0.elapsed().as_secs() < 5, "花了 {:?}，八成是跳了選擇框", t0.elapsed());
    }
}
