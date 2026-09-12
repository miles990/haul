//! 錄製：把一個分頁的畫面與聲音錄成檔案。
//!
//! 擷取與錄製都跑在目標分頁裡：注入的腳本呼叫 `getDisplayMedia({preferCurrentTab})`
//! 擷取自己，Chrome 以 `--auto-accept-this-tab-capture` 啟動所以不跳選擇框。
//! 曾經想另開一個面板頁去選目標分頁，但 Chrome 的「依標題自動選」旗標實測選不到
//! 分頁，只有自己擷取自己是零互動的。代價：分頁換頁會殺掉錄製器，所以監聽
//! pagehide 先停下來，已收到的 chunk 照收尾。

use super::cdp::{Cdp, CdpEvent};
use anyhow::{anyhow, bail, Result};
use base64::Engine as _;
use serde_json::{json, Value};
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::io::AsyncWriteExt;
use tokio::sync::watch;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stop {
    User,
    MediaEnded,
    MaxDuration,
}

/// 三個停止條件先到先贏。
///
/// 「媒體結束」有兩條路：頁面自己說播完了（媒體元素的 `ended`）就立刻停；
/// 沒有事件的頁面則靠「先播過、然後安靜 5 秒」。前者是為了自動接播下一首的
/// 站（實測 Suno 換歌無縫、沒有靜音，只靠安靜窗會一路錄到上限）；後者的
/// 5 秒是給廣告後正片一個機會。要先播過才算：使用者可能還沒按播放。
#[derive(Debug)]
pub struct StopWhen {
    start: Instant,
    max: Duration,
    user: bool,
    ever_played: bool,
    ended: bool,
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
            ended: false,
            quiet_since: None,
        }
    }

    pub fn user_stopped(&mut self) {
        self.user = true;
    }

    /// 頁面回報媒體元素播完了（或被換成下一首）
    pub fn media_ended(&mut self) {
        self.ended = true;
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
        if self.ended {
            return Some(Stop::MediaEnded);
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
    if (!navigator.mediaDevices) {
      throw new Error('這個頁面不是安全內容（' + location.href + '），瀏覽器不開放擷取');
    }
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
    // 頁面自己說播完了，比等安靜窗早也準：自動接播下一首的站換歌時沒有靜音。
    // 換下一首時元素會被重設（emptied），也算這段結束。只認播超過 2 秒的元素，
    // 音效與解鎖自動播放用的 0.1 秒空白聲不算。
    const hooked = new WeakSet();
    const hook = x => {
      if (hooked.has(x)) return;
      hooked.add(x);
      const done = why => { if (x.__haulPlayed) send({ type: 'ended', why }); };
      x.addEventListener('ended', () => done('ended'));
      x.addEventListener('emptied', () => done('emptied'));
    };
    // 每秒回報有沒有媒體在播，給「播完自動停」用。
    // 跨網域 iframe 裡的播放器看不到 —— 那種情況只剩使用者停止與上限
    window.__haulWatch = setInterval(() => {
      const m = [...document.querySelectorAll('video,audio')];
      m.forEach(hook);
      m.forEach(x => { if (!x.paused && x.currentTime > 2) x.__haulPlayed = true; });
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
        .kill_on_drop(true)
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

pub struct Recorded {
    pub stop: Stop,
    pub bytes: u64,
    pub secs: u64,
    pub mime: String,
    pub title: String,
}

/// 主框架導航 commit 後等多久再注入。不等完全載完 —— 直播頁永遠載不完。
const SETTLE: Duration = Duration::from_millis(1500);

/// 等導航 commit 的上限。實測 Suno 的 SSR 頁在冷啟動的 Chrome 裡要兩秒以上
/// 才 commit；在那之前分頁還是 about:blank，不是 secure context，注入的腳本
/// 會因為沒有 `navigator.mediaDevices` 而死得莫名其妙。
const NAV_TIMEOUT: Duration = Duration::from_secs(20);

/// 這個事件是不是「這個分頁的主框架已經導航到真正的網址」。
/// 子框架（parentId）與 about:blank 都不算。
fn main_frame_committed(ev: &CdpEvent, sid: &str) -> Option<String> {
    if ev.method != "Page.frameNavigated" || ev.session_id.as_deref() != Some(sid) {
        return None;
    }
    let frame = &ev.params["frame"];
    if frame.get("parentId").and_then(|v| v.as_str()).is_some() {
        return None;
    }
    frame["url"]
        .as_str()
        .filter(|u| !u.is_empty() && *u != "about:blank")
        .map(str::to_string)
}

/// 開目標分頁、在裡面錄到某個停止條件成立。chunk 邊收邊寫進 `dest`。
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

    // 先開空白頁、訂好事件，再自己導航：否則快的頁面會在訂閱前就 commit，
    // 慢的頁面會在 commit 前就被注入（見 NAV_TIMEOUT）。
    let (target_id, sid) = open(cdp).await?;
    cdp.call(Some(&sid), "Page.enable", json!({})).await?;
    cdp.call(Some(&sid), "Runtime.enable", json!({})).await?;
    cdp.call(
        Some(&sid),
        "Runtime.addBinding",
        json!({ "name": "haulRec" }),
    )
    .await?;
    cdp.call(Some(&sid), "Page.navigate", json!({ "url": url }))
        .await?;

    let deadline = Instant::now() + NAV_TIMEOUT;
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            bail!("頁面 {} 秒內沒有載入（{url}）", NAV_TIMEOUT.as_secs());
        }
        tokio::select! {
            r = tokio::time::timeout(left, events.recv()) => match r {
                Ok(Ok(ev)) => {
                    if main_frame_committed(&ev, &sid).is_some() {
                        break;
                    }
                }
                Ok(Err(tokio::sync::broadcast::error::RecvError::Lagged(_))) => continue,
                Ok(Err(_)) => bail!("瀏覽器連線已關閉"),
                Err(_) => continue,
            },
            _ = stop.changed() => {
                if *stop.borrow() { bail!("錄製在頁面載入前就停止了"); }
            }
        }
    }
    tokio::time::sleep(SETTLE).await;

    let title = eval_str(cdp, &sid, "document.title")
        .await
        .unwrap_or_default();
    let supported: Vec<String> = eval_str(cdp, &sid, SUPPORTED_JS)
        .await
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();
    let mime = pick_mime(audio_only, &supported)
        .ok_or_else(|| anyhow!("這個瀏覽器的 MediaRecorder 不支援任何可用格式（{supported:?}）"))?;

    cdp.call(
        Some(&sid),
        "Runtime.evaluate",
        json!({ "expression": record_js(&mime, audio_only), "userGesture": true }),
    )
    .await?;

    let mut file = tokio::fs::File::create(dest).await?;
    let start = Instant::now();
    let mut when = StopWhen::new(start, max);
    let mut bytes = 0u64;
    let mut started = false;
    let mut outcome: Option<Stop> = None;
    let mut told_page = false;

    // 注入後多久內沒有 started 就當失敗——getDisplayMedia 跳了選擇框或被拒
    const START_TIMEOUT: Duration = Duration::from_secs(15);

    loop {
        if outcome.is_none() {
            if let Some(s) = when.check(Instant::now()) {
                outcome = Some(s);
            }
        }
        if !started && start.elapsed() > START_TIMEOUT {
            bail!(
                "錄製沒有開始（{}秒內沒收到擷取成功的回報）",
                START_TIMEOUT.as_secs()
            );
        }
        // 叫頁面停；最後一個 chunk 與 stopped 事件會跟著來
        if outcome.is_some() && !told_page {
            told_page = true;
            let _ = cdp
                .call(
                    Some(&sid),
                    "Runtime.evaluate",
                    json!({ "expression": "window.haulStop && window.haulStop()" }),
                )
                .await;
        }

        let ev = tokio::select! {
            r = events.recv() => match r {
                Ok(ev) => ev,
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(_) => bail!("瀏覽器連線已關閉"),
            },
            _ = stop.changed() => {
                if *stop.borrow() { when.user_stopped(); }
                continue;
            }
            _ = tokio::time::sleep(Duration::from_millis(500)) => continue,
        };

        match ev.method.as_str() {
            "Runtime.bindingCalled" if ev.session_id.as_deref() == Some(sid.as_str()) => {
                let Ok(msg) =
                    serde_json::from_str::<Value>(ev.params["payload"].as_str().unwrap_or(""))
                else {
                    continue;
                };
                match msg["type"].as_str() {
                    Some("started") => started = true,
                    Some("chunk") => {
                        let data = base64::engine::general_purpose::STANDARD
                            .decode(msg["data"].as_str().unwrap_or(""))
                            .unwrap_or_default();
                        bytes += data.len() as u64;
                        file.write_all(&data).await?;
                        progress(bytes, start.elapsed().as_secs());
                    }
                    Some("playing") => {
                        when.playing(msg["playing"].as_bool().unwrap_or(false), Instant::now());
                    }
                    Some("ended") => when.media_ended(),
                    Some("stopped") => {
                        // 我們沒下令就停了：藍條的「停止分享」或換頁，算使用者停的
                        outcome.get_or_insert(Stop::User);
                        break;
                    }
                    Some("error") => {
                        bail!("錄製失敗：{}", msg["message"].as_str().unwrap_or("?"))
                    }
                    _ => {}
                }
            }
            "Target.targetDestroyed"
                if ev.params["targetId"].as_str() == Some(target_id.as_str()) =>
            {
                // 分頁被關：已錄到的照收尾
                outcome.get_or_insert(Stop::User);
                break;
            }
            _ => {}
        }
    }
    file.flush().await?;
    drop(file);

    let _ = cdp
        .call(None, "Target.closeTarget", json!({ "targetId": target_id }))
        .await;

    if bytes == 0 {
        bail!("什麼都沒錄到");
    }
    Ok(Recorded {
        stop: outcome.unwrap_or(Stop::User),
        bytes,
        secs: start.elapsed().as_secs(),
        mime,
        title,
    })
}

async fn open(cdp: &Cdp) -> Result<(String, String)> {
    let t = cdp
        .call(None, "Target.createTarget", json!({ "url": "about:blank" }))
        .await?;
    let tid = t["targetId"]
        .as_str()
        .ok_or_else(|| anyhow!("沒有 targetId"))?
        .to_string();
    let a = cdp
        .call(
            None,
            "Target.attachToTarget",
            json!({ "targetId": tid, "flatten": true }),
        )
        .await?;
    let sid = a["sessionId"]
        .as_str()
        .ok_or_else(|| anyhow!("沒有 sessionId"))?
        .to_string();
    Ok((tid, sid))
}

async fn eval_str(cdp: &Cdp, sid: &str, expr: &str) -> Option<String> {
    let r = cdp
        .call(
            Some(sid),
            "Runtime.evaluate",
            json!({ "expression": expr, "returnByValue": true }),
        )
        .await
        .ok()?;
    r["result"]["value"].as_str().map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stops_at_max_duration() {
        let t0 = Instant::now();
        let mut s = StopWhen::new(t0, Duration::from_secs(10));
        assert_eq!(s.check(t0 + Duration::from_secs(9)), None);
        assert_eq!(
            s.check(t0 + Duration::from_secs(10)),
            Some(Stop::MaxDuration)
        );
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
        assert_eq!(
            s.check(t0 + Duration::from_secs(45)),
            Some(Stop::MediaEnded)
        );
    }

    #[test]
    fn page_reported_end_stops_without_waiting_for_quiet() {
        let t0 = Instant::now();
        let mut s = StopWhen::new(t0, Duration::from_secs(3600));
        s.playing(true, t0 + Duration::from_secs(1));
        // 換下一首：一直有東西在播，安靜窗永遠不會到
        s.media_ended();
        s.playing(true, t0 + Duration::from_secs(2));
        assert_eq!(s.check(t0 + Duration::from_secs(2)), Some(Stop::MediaEnded));
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
        assert_eq!(
            s.check(t0 + Duration::from_secs(25)),
            Some(Stop::MediaEnded)
        );
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
        assert_eq!(
            pick_mime(false, &all).unwrap(),
            "video/webm;codecs=h264,opus"
        );
        let vp9 = vec!["video/webm;codecs=vp9,opus".to_string()];
        assert_eq!(
            pick_mime(false, &vp9).unwrap(),
            "video/webm;codecs=vp9,opus"
        );
        assert!(pick_mime(false, &[]).is_none());
        assert_eq!(
            pick_mime(true, &["audio/webm;codecs=opus".to_string()]).unwrap(),
            "audio/webm;codecs=opus"
        );
    }

    #[test]
    fn only_main_frame_real_navigation_counts_as_committed() {
        let ev = |method: &str, sid: Option<&str>, params: Value| CdpEvent {
            method: method.into(),
            params,
            session_id: sid.map(String::from),
        };
        let real = json!({ "frame": { "id": "F1", "url": "https://suno.com/song/x" } });
        assert_eq!(
            main_frame_committed(&ev("Page.frameNavigated", Some("s1"), real.clone()), "s1")
                .as_deref(),
            Some("https://suno.com/song/x")
        );
        // 別的分頁
        assert!(
            main_frame_committed(&ev("Page.frameNavigated", Some("s2"), real.clone()), "s1")
                .is_none()
        );
        // 還在 about:blank 不算——那正是這個等待要避開的狀態
        let blank = json!({ "frame": { "id": "F1", "url": "about:blank" } });
        assert!(
            main_frame_committed(&ev("Page.frameNavigated", Some("s1"), blank), "s1").is_none()
        );
        // 子框架（廣告、播放器 iframe）不算
        let sub =
            json!({ "frame": { "id": "F2", "parentId": "F1", "url": "https://ads.example/" } });
        assert!(main_frame_committed(&ev("Page.frameNavigated", Some("s1"), sub), "s1").is_none());
        // 其他事件不算
        assert!(main_frame_committed(&ev("Page.loadEventFired", Some("s1"), real), "s1").is_none());
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

    /// 用 Haul 自己下載的 ffmpeg 產一支 3 秒的真影片。找不到就略過。
    fn make_clip(dir: &Path) -> Option<std::path::PathBuf> {
        make_clip_secs(dir, 3)
    }

    fn make_clip_secs(dir: &Path, secs: u32) -> Option<std::path::PathBuf> {
        let ffmpeg = crate::engine::default_bin_dir().join("ffmpeg");
        if !ffmpeg.is_file() {
            return None;
        }
        let out = dir.join(format!("clip{secs}.mp4"));
        let secs = secs.to_string();
        let ok = std::process::Command::new(&ffmpeg)
            .args([
                "-y",
                "-loglevel",
                "error",
                "-f",
                "lavfi",
                "-i",
                "testsrc=size=320x240:rate=25",
                "-f",
                "lavfi",
                "-i",
                "sine=frequency=440:sample_rate=44100",
                "-t",
                &secs,
                "-c:v",
                "libx264",
                "-pix_fmt",
                "yuv420p",
                "-c:a",
                "aac",
                "-shortest",
            ])
            .arg(&out)
            .status()
            .ok()?
            .success();
        ok.then_some(out)
    }

    /// 一頁自動播放的 <video>。127.0.0.1 是 secure context，getDisplayMedia 可用。
    /// 頁面延遲 `delay` 才回：重現「導航還沒 commit 就注入」的失敗。
    /// 本機伺服器是瞬間回的，沒有這個延遲測不到那個 bug。
    fn clip_server(clip: Vec<u8>, delay: Duration) -> (String, std::thread::JoinHandle<()>) {
        use std::io::{Read, Write};
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", l.local_addr().unwrap());
        let page = format!(
            "<!doctype html><title>Clip</title><video autoplay src=\"{base}/clip.mp4\"></video>"
        );
        let h = std::thread::spawn(move || {
            for _ in 0..32 {
                let Ok((mut s, _)) = l.accept() else { break };
                let mut buf = [0u8; 4096];
                let n = s.read(&mut buf).unwrap_or(0);
                let req = String::from_utf8_lossy(&buf[..n]);
                let (ct, body): (&str, &[u8]) = if req.contains("/clip.mp4") {
                    ("video/mp4", &clip)
                } else {
                    std::thread::sleep(delay);
                    ("text/html", page.as_bytes())
                };
                let _ = write!(
                    s,
                    "HTTP/1.1 200 OK\r\nContent-Type: {ct}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
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
        let Some(clip) = make_clip(&dir) else {
            eprintln!("略過：沒有 ffmpeg");
            return;
        };
        // 頁面比 SETTLE 慢才回：舊版在 about:blank 上注入就會死在 getDisplayMedia
        let (base, _srv) = clip_server(std::fs::read(&clip).unwrap(), Duration::from_millis(2500));

        let exe = super::super::chrome::find(None).unwrap();
        let ch =
            super::super::chrome::launch(&exe, &std::env::temp_dir().join("haul-chrome-test-ends"))
                .await
                .unwrap();
        let cdp = Cdp::connect(&ch.ws_url).await.unwrap();

        let webm = dir.join("rec.webm");
        let (_stop_tx, stop_rx) = watch::channel(false);
        let mut last = (0u64, 0u64);
        let out = record(
            &cdp,
            &format!("{base}/"),
            false,
            &webm,
            Duration::from_secs(60),
            stop_rx,
            |bytes, secs| last = (bytes, secs),
        )
        .await
        .unwrap();
        let _ = cdp.call(None, "Browser.close", json!({})).await;

        assert_eq!(out.stop, Stop::MediaEnded, "3 秒的片播完應該自動停");
        assert!(out.bytes > 10_240, "只有 {} bytes", out.bytes);
        assert!(webm.is_file());
        assert!(last.0 > 0 && last.1 >= 3, "進度回呼：{last:?}");
        assert_eq!(out.title, "Clip");
        assert!(out.mime.contains("h264"), "{}", out.mime);

        // 轉封裝也真的跑一次，確認 ffmpeg 吃得下 MediaRecorder 的 WebM
        let ffmpeg = crate::engine::default_bin_dir().join("ffmpeg");
        let mp4 = dir.join("rec.mp4");
        remux(&ffmpeg, &webm, &mp4, false, &out.mime).await.unwrap();
        assert!(std::fs::metadata(&mp4).unwrap().len() > 10_240);
    }

    /// 停止訊號真的會讓 record() 停下來（這是 CLI 的 Ctrl-C 與 GUI 的停止鈕
    /// 背後的機制）。用一頁循環播放、不會自己停的片，只靠訊號停。
    #[tokio::test]
    async fn stop_signal_ends_a_recording_of_looping_media() {
        if std::env::var("HAUL_TEST_CHROME").is_err() {
            eprintln!("略過：未設 HAUL_TEST_CHROME");
            return;
        }
        let dir = std::env::temp_dir().join("haul-record-test2");
        std::fs::create_dir_all(&dir).unwrap();
        let Some(clip) = make_clip(&dir) else {
            eprintln!("略過：沒有 ffmpeg");
            return;
        };
        // loop 播放，MediaEnded 永遠不會觸發，只有停止訊號能結束
        let (base, _srv) = loop_server(std::fs::read(&clip).unwrap());

        let exe = super::super::chrome::find(None).unwrap();
        let ch =
            super::super::chrome::launch(&exe, &std::env::temp_dir().join("haul-chrome-test-stop"))
                .await
                .unwrap();
        let cdp = Cdp::connect(&ch.ws_url).await.unwrap();

        let webm = dir.join("rec.webm");
        let (stop_tx, stop_rx) = watch::channel(false);
        // 4 秒後按停止
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(4)).await;
            let _ = stop_tx.send(true);
        });
        let t0 = Instant::now();
        let out = record(
            &cdp,
            &format!("{base}/"),
            false,
            &webm,
            Duration::from_secs(3600),
            stop_rx,
            |_, _| {},
        )
        .await
        .unwrap();
        let elapsed = t0.elapsed();
        let _ = cdp.call(None, "Browser.close", json!({})).await;

        assert_eq!(out.stop, Stop::User, "停止訊號應該讓它以 User 結束");
        assert!(
            elapsed < Duration::from_secs(20),
            "上限 1 小時，卻等了 {elapsed:?}，訊號沒生效"
        );
        assert!(out.bytes > 10_240);
    }

    /// 自動接播下一首：第一支播完立刻換成循環播放的第二支，中間沒有靜音。
    /// 這正是 Suno 換歌的樣子——只靠「安靜 5 秒」的話會一路錄到上限。
    #[tokio::test]
    async fn stops_when_the_first_media_ends_even_if_the_page_plays_on() {
        if std::env::var("HAUL_TEST_CHROME").is_err() {
            eprintln!("略過：未設 HAUL_TEST_CHROME");
            return;
        }
        let dir = std::env::temp_dir().join("haul-record-next-test");
        std::fs::create_dir_all(&dir).unwrap();
        let Some(clip) = make_clip_secs(&dir, 6) else {
            eprintln!("略過：沒有 ffmpeg");
            return;
        };
        let (base, _srv) = next_server(std::fs::read(&clip).unwrap());

        let exe = super::super::chrome::find(None).unwrap();
        let ch =
            super::super::chrome::launch(&exe, &std::env::temp_dir().join("haul-chrome-test-next"))
                .await
                .unwrap();
        let cdp = Cdp::connect(&ch.ws_url).await.unwrap();

        let webm = dir.join("rec.webm");
        let (_stop_tx, stop_rx) = watch::channel(false);
        let t0 = Instant::now();
        let out = record(
            &cdp,
            &format!("{base}/"),
            false,
            &webm,
            Duration::from_secs(60),
            stop_rx,
            |_, _| {},
        )
        .await
        .unwrap();
        let elapsed = t0.elapsed();
        let _ = cdp.call(None, "Browser.close", json!({})).await;

        assert_eq!(out.stop, Stop::MediaEnded, "第一支播完就該停");
        assert!(
            elapsed < Duration::from_secs(20),
            "第二支在循環播放，卻等了 {elapsed:?}：沒有在第一支結束時停"
        );
        assert!(out.bytes > 10_240);
    }

    fn next_server(clip: Vec<u8>) -> (String, std::thread::JoinHandle<()>) {
        use std::io::{Read, Write};
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", l.local_addr().unwrap());
        let page = format!(
            "<!doctype html><title>Next</title><video autoplay src=\"{base}/clip.mp4\"></video>\
             <script>const v=document.querySelector('video');\
             v.addEventListener('ended',()=>{{v.src='{base}/clip.mp4?next';v.loop=true;v.play();}});</script>"
        );
        let h = std::thread::spawn(move || {
            for _ in 0..64 {
                let Ok((mut s, _)) = l.accept() else { break };
                let mut buf = [0u8; 4096];
                let n = s.read(&mut buf).unwrap_or(0);
                let req = String::from_utf8_lossy(&buf[..n]);
                let (ct, body): (&str, &[u8]) = if req.contains("/clip.mp4") {
                    ("video/mp4", &clip)
                } else {
                    ("text/html", page.as_bytes())
                };
                let _ = write!(
                    s,
                    "HTTP/1.1 200 OK\r\nContent-Type: {ct}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                let _ = s.write_all(body);
            }
        });
        (base, h)
    }

    fn loop_server(clip: Vec<u8>) -> (String, std::thread::JoinHandle<()>) {
        use std::io::{Read, Write};
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", l.local_addr().unwrap());
        let page = format!(
            "<!doctype html><title>Loop</title><video autoplay loop muted src=\"{base}/clip.mp4\"></video>"
        );
        let h = std::thread::spawn(move || {
            for _ in 0..64 {
                let Ok((mut s, _)) = l.accept() else { break };
                let mut buf = [0u8; 4096];
                let n = s.read(&mut buf).unwrap_or(0);
                let req = String::from_utf8_lossy(&buf[..n]);
                let (ct, body): (&str, &[u8]) = if req.contains("/clip.mp4") {
                    ("video/mp4", &clip)
                } else {
                    ("text/html", page.as_bytes())
                };
                let _ = write!(
                    s,
                    "HTTP/1.1 200 OK\r\nContent-Type: {ct}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                let _ = s.write_all(body);
            }
        });
        (base, h)
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
            .call(
                None,
                "Target.createTarget",
                json!({"url": format!("file://{}", page.display())}),
            )
            .await
            .unwrap();
        let tid = t["targetId"].as_str().unwrap().to_string();
        let a = cdp
            .call(
                None,
                "Target.attachToTarget",
                json!({"targetId": tid, "flatten": true}),
            )
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
        assert!(
            out["video"].as_bool().unwrap(),
            "沒拿到視訊軌（假設 1/2 不成立）：{out}"
        );
        assert!(out["audio"].as_bool().unwrap(), "沒拿到音訊軌：{out}");
        assert_eq!(out["surface"], "browser", "選到的不是分頁：{out}");
        assert!(
            out["h264"].as_bool().unwrap(),
            "MediaRecorder 不支援 h264（假設 3 不成立）"
        );
        assert!(
            t0.elapsed().as_secs() < 5,
            "花了 {:?}，八成是跳了選擇框",
            t0.elapsed()
        );
    }
}
