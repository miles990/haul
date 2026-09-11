//! 從瀏覽器的網路事件裡挑出「這頁真正的媒體」。
//!
//! 一個影音頁面跑起來有幾百個請求：預覽縮圖、廣告、背景音、主片、
//! 同一支片的兩三種畫質。這裡負責分類、去重、挑一個最像的，
//! 以及決定什麼時候可以停止觀察。

use super::cdp::{Cdp, CdpEvent};
use crate::cookies::Cookie;
use anyhow::{anyhow, bail, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
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

#[derive(Clone, Debug, Serialize, Deserialize)]
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
    let mime = mime
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    let ext = ext_of(url);

    if MANIFEST_MIMES.contains(&mime.as_str()) || ext == "m3u8" || ext == "mpd" {
        return Some(Kind::Manifest);
    }
    if mime == "video/mp2t" || mime == "video/iso.segment" || SEGMENT_EXTS.contains(&ext.as_str()) {
        return Some(Kind::Segment);
    }
    if mime.starts_with("video/") || mime.starts_with("audio/") || FILE_EXTS.contains(&ext.as_str())
    {
        return Some(Kind::File);
    }
    if resource_type == "Media" {
        return Some(Kind::File);
    }
    None
}

pub fn is_ad_host(host: &str) -> bool {
    let h = host.to_ascii_lowercase();
    AD_HOSTS
        .iter()
        .any(|a| h == *a || h.ends_with(&format!(".{a}")))
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

    /// `best` 在清單裡的索引，給 UI 標「已選」用
    pub fn best_index(&self) -> Option<usize> {
        let best = self.best()?;
        self.list.iter().position(|c| c.url == best.url)
    }
}

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

pub struct Sniffed {
    pub candidates: Candidates,
    pub title: String,
}

/// 開一個分頁載入 `url`，觀察網路直到停止規則到了。
///
/// `cookies` 非空時在導向前灌進去——瀏覽器 profile 是乾淨的，
/// 使用者這次帶的登入狀態要自己送進去。`progress` 收到目前的候選數，給 UI 顯示。
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
        .call(
            None,
            "Target.attachToTarget",
            json!({ "targetId": target_id, "flatten": true }),
        )
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
        let _ = cdp
            .call(Some(&sid), "Network.setCookies", json!({ "cookies": list }))
            .await;
    }

    cdp.call(Some(&sid), "Page.navigate", json!({ "url": url }))
        .await?;

    let mut rule = StopRule::new(Instant::now());
    let mut found = Candidates::default();
    // requestId -> 該請求的 header；ExtraInfo 與 requestWillBeSent 順序不定，兩邊都收
    let mut req_headers: HashMap<String, Vec<(String, String)>> = HashMap::new();
    let mut req_type: HashMap<String, String> = HashMap::new();

    loop {
        let left = rule.deadline().saturating_duration_since(Instant::now());
        if left.is_zero() {
            break;
        }
        let ev = match tokio::time::timeout(left, events.recv()).await {
            Ok(Ok(ev)) => ev,
            Ok(Err(tokio::sync::broadcast::error::RecvError::Lagged(_))) => continue,
            Ok(Err(_)) => bail!("瀏覽器連線已關閉"),
            Err(_) => break, // 到期
        };
        if ev.session_id.as_deref() != Some(sid.as_str()) {
            // 分頁被使用者關掉：Target.targetDestroyed 是瀏覽器層級事件，沒有 sessionId
            if ev.method == "Target.targetDestroyed"
                && ev.params["targetId"].as_str() == Some(target_id.as_str())
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
        .call(
            Some(&sid),
            "Runtime.evaluate",
            json!({ "expression": "document.title", "returnByValue": true }),
        )
        .await
        .ok()
        .and_then(|v| v["result"]["value"].as_str().map(str::to_string))
        .unwrap_or_default();

    let _ = cdp
        .call(None, "Target.closeTarget", json!({ "targetId": target_id }))
        .await;

    Ok(Sniffed {
        candidates: found,
        title,
    })
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
            let host = url
                .split("://")
                .nth(1)?
                .split('/')
                .next()?
                .split(':')
                .next()?;
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
            Some(Candidate {
                url,
                kind,
                size,
                mime,
                headers,
            })
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn c(url: &str, kind: Kind, size: Option<u64>) -> Candidate {
        Candidate {
            url: url.into(),
            kind,
            size,
            mime: String::new(),
            headers: vec![],
        }
    }

    #[test]
    fn classifies_by_mime_first_then_extension() {
        assert_eq!(
            classify("https://x/a?e=1", "application/vnd.apple.mpegurl", "Fetch"),
            Some(Kind::Manifest)
        );
        assert_eq!(
            classify("https://x/a.m3u8", "text/plain", "Fetch"),
            Some(Kind::Manifest)
        );
        assert_eq!(
            classify("https://x/a.mpd", "application/dash+xml", "Fetch"),
            Some(Kind::Manifest)
        );
        assert_eq!(
            classify("https://x/v.mp4", "video/mp4", "Media"),
            Some(Kind::File)
        );
        assert_eq!(
            classify("https://x/v", "audio/mpeg", "Media"),
            Some(Kind::File)
        );
        // 分段：不是可以直接抓的東西，但值得記下來給錯誤訊息用
        assert_eq!(
            classify("https://x/seg-12.ts", "video/mp2t", "Fetch"),
            Some(Kind::Segment)
        );
        assert_eq!(
            classify("https://x/seg-12.m4s", "application/octet-stream", "Fetch"),
            Some(Kind::Segment)
        );
        // resourceType 是 Media 但 MIME 講不清楚：信 Chrome 的判斷
        assert_eq!(
            classify("https://x/stream", "application/octet-stream", "Media"),
            Some(Kind::File)
        );
        assert_eq!(classify("https://x/page", "text/html", "Document"), None);
        assert_eq!(
            classify("https://x/app.js", "application/javascript", "Script"),
            None
        );
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
        assert_eq!(set.best_index(), Some(1));
        set.push(c("https://x/master.m3u8", Kind::Manifest, None));
        assert_eq!(set.best().unwrap().url, "https://x/master.m3u8");
        assert_eq!(set.best_index(), Some(2));
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

    #[test]
    fn stop_rule_waits_briefly_after_a_manifest() {
        let t0 = Instant::now();
        let mut r = StopRule::new(t0);
        assert_eq!(r.deadline(), t0 + StopRule::OVERALL);
        r.saw(Kind::Manifest, t0 + Duration::from_secs(4));
        assert_eq!(
            r.deadline(),
            t0 + Duration::from_secs(4) + StopRule::AFTER_MANIFEST
        );
    }

    #[test]
    fn stop_rule_extends_quiet_window_per_file() {
        let t0 = Instant::now();
        let mut r = StopRule::new(t0);
        r.saw(Kind::File, t0 + Duration::from_secs(2));
        assert_eq!(
            r.deadline(),
            t0 + Duration::from_secs(2) + StopRule::QUIET_AFTER_FILE
        );
        // 又來一個：安靜期重算
        r.saw(Kind::File, t0 + Duration::from_secs(5));
        assert_eq!(
            r.deadline(),
            t0 + Duration::from_secs(5) + StopRule::QUIET_AFTER_FILE
        );
        // 清單出現後就不再被單檔延長
        r.saw(Kind::Manifest, t0 + Duration::from_secs(6));
        r.saw(Kind::File, t0 + Duration::from_secs(7));
        assert_eq!(
            r.deadline(),
            t0 + Duration::from_secs(6) + StopRule::AFTER_MANIFEST
        );
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

    /// 起一個只回兩個路徑的本機 HTTP server：一頁有 <video>，一支假的 mp4。
    /// 用 std 的 TcpListener 就夠，不需要引入 hyper。
    fn tiny_server() -> (String, std::thread::JoinHandle<()>) {
        use std::io::{Read, Write};
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", l.local_addr().unwrap());
        let page = format!(
            "<!doctype html><title>Tiny Player</title><video autoplay muted src=\"{base}/clip.mp4\"></video>"
        );
        // 假 mp4：內容不重要，這裡測的是偵測不是驗證；夠大才不會被當 beacon
        let clip = vec![0u8; 64 * 1024];
        let h = std::thread::spawn(move || {
            for _ in 0..16 {
                let Ok((mut s, _)) = l.accept() else { break };
                let mut buf = [0u8; 4096];
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
        let cdp = super::super::cdp::Cdp::connect(&chrome.ws_url)
            .await
            .unwrap();

        let mut seen = 0;
        let out = sniff(&cdp, &format!("{base}/"), &[], |n| seen = n)
            .await
            .unwrap();
        let _ = cdp.call(None, "Browser.close", json!({})).await;

        let best = out.candidates.best().expect("該偵測到 clip.mp4");
        assert!(best.url.ends_with("/clip.mp4"), "{}", best.url);
        assert_eq!(best.kind, Kind::File);
        assert_eq!(best.size, Some(64 * 1024));
        assert!(
            best.headers
                .iter()
                .any(|(k, _)| k.eq_ignore_ascii_case("referer")),
            "{:?}",
            best.headers
        );
        assert_eq!(out.title, "Tiny Player");
        assert!(seen >= 1);
    }
}
