//! yt-dlp 拒絕或不認識的連結走這裡。
//!
//! 之所以需要這層：yt-dlp 對某些站點是政策性拒絕，不是還沒實作。
//! 例如 suno.com 會直接回「This website is not supported and will not
//! be supported」。這種情況下通用方案幫不上忙。
//!
//! 這裡的站點規則本質上是脆弱的（Suno 就已經把 .mp3 換成 .mp4 過一次），
//! 但它只是後備：壞掉只影響這幾個站，不會拖垮整個工具。

use anyhow::{bail, Result};
use futures_util::StreamExt;
use regex::Regex;
use reqwest::Client;
use std::collections::HashMap;
use std::path::Path;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};
use tokio::io::AsyncWriteExt;

pub const UA: &str = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 \
                      (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36";

/// 對同一個主機兩次請求之間至少隔這麼久。
///
/// 之所以需要：整批圖庫是對同一台主機連發幾百個請求。原本只有「同時 3 個
/// 下載」的限制，對三支大影片夠用，但對 223 張小圖就直接被回 429 —— 實測
/// 223 張只成功 4 張。下載器與爬蟲的差別正在這裡。
const MIN_HOST_INTERVAL: Duration = Duration::from_millis(300);

/// 被限流時重試幾次
const MAX_ATTEMPTS: u32 = 4;

#[derive(Debug, Clone)]
pub struct Found {
    pub media: String,
    pub title: String,
    /// 伺服器回報的 Content-Type。比副檔名可靠，用來決定驗證強度。
    pub content_type: Option<String>,
}

fn uuid_re() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| {
        Regex::new(r"(?i)[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}").unwrap()
    })
}

fn og_title_re() -> &'static [Regex; 2] {
    static R: OnceLock<[Regex; 2]> = OnceLock::new();
    R.get_or_init(|| {
        [
            Regex::new(
                r#"(?is)<meta[^>]+(?:property|name)=["']og:title["'][^>]*content=["']([^"']*)["']"#,
            )
            .unwrap(),
            Regex::new(
                r#"(?is)<meta[^>]+content=["']([^"']*)["'][^>]*(?:property|name)=["']og:title["']"#,
            )
            .unwrap(),
        ]
    })
}

fn decode_entities(s: &str) -> String {
    let mut out = s
        .replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&apos;", "'")
        .replace("&nbsp;", " ");
    if out.contains("&#") {
        let re = Regex::new(r"&#(\d{1,7});").unwrap();
        out = re
            .replace_all(&out, |c: &regex::Captures| {
                c[1].parse::<u32>()
                    .ok()
                    .and_then(char::from_u32)
                    .map(String::from)
                    .unwrap_or_default()
            })
            .into_owned();
    }
    out
}

pub fn looks_like_media_url(url: &str) -> bool {
    let path = url.split('?').next().unwrap_or(url);
    matches!(
        path.rsplit('.')
            .next()
            .map(str::to_ascii_lowercase)
            .as_deref(),
        Some("mp4" | "m4a" | "mp3" | "webm" | "mkv" | "mov" | "wav" | "flac" | "opus" | "ogg")
    )
}

fn host_of(url: &str) -> String {
    url.split("://")
        .nth(1)
        .and_then(|r| r.split('/').next())
        .unwrap_or("")
        .to_ascii_lowercase()
}

/// 排隊等這個主機的下一個空檔。時間戳是先佔的，所以併發的任務會
/// 自動錯開而不是同時衝出去。
async fn wait_turn(host: &str) {
    static GATE: OnceLock<Mutex<HashMap<String, Instant>>> = OnceLock::new();
    let gate = GATE.get_or_init(|| Mutex::new(HashMap::new()));

    let wait = {
        let mut m = gate.lock().unwrap();
        let now = Instant::now();
        let slot = m
            .get(host)
            .map(|t| (*t + MIN_HOST_INTERVAL).max(now))
            .unwrap_or(now);
        m.insert(host.to_string(), slot);
        slot.saturating_duration_since(now)
    };
    if !wait.is_zero() {
        tokio::time::sleep(wait).await;
    }
}

/// 伺服器說要等多久就等多久，沒說就用指數退避
fn retry_delay(res: &reqwest::Response, attempt: u32) -> Duration {
    res.headers()
        .get("retry-after")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.trim().parse::<u64>().ok())
        .map(Duration::from_secs)
        .unwrap_or_else(|| Duration::from_millis(500 * 2u64.pow(attempt.min(5))))
        .min(Duration::from_secs(30))
}

async fn head_ok(client: &Client, url: &str) -> bool {
    match client.head(url).header("user-agent", UA).send().await {
        Ok(r) => r.status().is_success(),
        Err(_) => false,
    }
}

/// Suno 的歌曲頁。曾經是 `<uuid>.mp3`，現在是 `<uuid>.mp4`（h264 + aac），
/// 所以兩個都試，不押寶在單一副檔名上。
async fn suno(client: &Client, page_url: &str) -> Result<Found> {
    let id = uuid_re()
        .find(page_url)
        .map(|m| m.as_str().to_lowercase())
        .ok_or_else(|| anyhow::anyhow!("這個 Suno 連結裡沒有歌曲 id"))?;

    let mut media = None;
    for ext in ["mp4", "mp3"] {
        let candidate = format!("https://cdn1.suno.ai/{id}.{ext}");
        if head_ok(client, &candidate).await {
            media = Some(candidate);
            break;
        }
    }
    let media = media.ok_or_else(|| {
        anyhow::anyhow!("Suno 的 CDN 上找不到這首（可能是私人的，或路徑規則又改了）")
    })?;

    // 標題抓不到不是錯誤，退回用 id 當檔名
    let title = match client.get(page_url).header("user-agent", UA).send().await {
        Ok(r) => match r.text().await {
            Ok(html) => og_title_re()
                .iter()
                .find_map(|re| re.captures(&html).map(|c| decode_entities(&c[1])))
                .filter(|t| !t.trim().is_empty())
                .unwrap_or_else(|| id.clone()),
            Err(_) => id.clone(),
        },
        Err(_) => id.clone(),
    };

    Ok(Found {
        media,
        title,
        content_type: None,
    })
}

/// 試著在不靠 yt-dlp 的情況下找出媒體檔。找不到就回 Err，呼叫端據此
/// 決定要回報哪個錯誤。
pub async fn probe(client: &Client, url: &str, allow_html: bool) -> Result<Found> {
    if looks_like_media_url(url) {
        let title = url
            .split('?')
            .next()
            .unwrap_or(url)
            .rsplit('/')
            .next()
            .and_then(|f| f.rsplit_once('.').map(|(stem, _)| stem.to_string()))
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "untitled".into());
        return Ok(Found {
            media: url.to_string(),
            title,
            content_type: None,
        });
    }

    let host = url
        .split("://")
        .nth(1)
        .and_then(|rest| rest.split('/').next())
        .unwrap_or("")
        .to_ascii_lowercase();

    if host.ends_with("suno.com") || host.ends_with("suno.ai") {
        return suno(client, url).await;
    }

    any_file(client, url, allow_html).await
}

/// 通用檔案：問伺服器這是什麼，再決定要不要當成可下載的檔案。
///
/// text/html 預設不算檔案。理由是假成功比失敗更糟：如果 yt-dlp 對某個
/// 影片連結暫時失敗，然後我們「成功」存下那頁 HTML，使用者會以為抓到了。
/// 真的想存網頁本身就明確加 --any。
async fn any_file(client: &Client, url: &str, allow_html: bool) -> Result<Found> {
    let res = client
        .head(url)
        .header("user-agent", UA)
        .send()
        .await
        .map_err(|e| anyhow::anyhow!("連不上：{e}"))?;

    if !res.status().is_success() {
        bail!("HTTP {}", res.status());
    }

    let headers = res.headers();
    let ct = headers
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());

    let is_html = ct
        .as_deref()
        .map(|t| {
            t.split(';')
                .next()
                .unwrap_or(t)
                .trim()
                .eq_ignore_ascii_case("text/html")
        })
        .unwrap_or(false);

    if is_html && !allow_html {
        bail!("這是一個網頁不是檔案。要存下網頁本身請加 --any");
    }

    // 檔名優先順序：Content-Disposition > 網址最後一段 > 主機名
    let title = headers
        .get("content-disposition")
        .and_then(|v| v.to_str().ok())
        .and_then(filename_from_disposition)
        .or_else(|| {
            url.split('?')
                .next()
                .unwrap_or(url)
                .trim_end_matches('/')
                .rsplit('/')
                .next()
                .map(str::to_string)
                .filter(|s| !s.is_empty() && s.contains('.'))
        })
        .unwrap_or_else(|| {
            url.split("://")
                .nth(1)
                .and_then(|r| r.split('/').next())
                .unwrap_or("download")
                .to_string()
        });

    Ok(Found {
        media: url.to_string(),
        title,
        content_type: ct,
    })
}

/// 從 Content-Disposition 取檔名。優先 filename*（RFC 5987，帶編碼），
/// 沒有才用 filename。
fn filename_from_disposition(v: &str) -> Option<String> {
    if let Some(i) = v.to_ascii_lowercase().find("filename*=") {
        let rest = &v[i + "filename*=".len()..];
        let val = rest.split(';').next()?.trim();
        // UTF-8''%E4%B8%AD.pdf 這種形式
        if let Some(enc) = val.split("''").nth(1) {
            return Some(percent_decode(enc));
        }
    }
    let i = v.to_ascii_lowercase().find("filename=")?;
    let rest = &v[i + "filename=".len()..];
    let val = rest.split(';').next()?.trim().trim_matches('"');
    (!val.is_empty()).then(|| val.to_string())
}

fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            if let Ok(v) = u8::from_str_radix(&s[i + 1..i + 3], 16) {
                out.push(v);
                i += 3;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// 串流下載到 `dest`，記憶體用量與檔案大小無關。
pub async fn download<F>(
    client: &Client,
    media_url: &str,
    dest: &Path,
    allow_html: bool,
    mut on_progress: F,
) -> Result<u64>
where
    F: FnMut(u64, u64),
{
    let host = host_of(media_url);
    let mut res = None;

    for attempt in 1..=MAX_ATTEMPTS {
        wait_turn(&host).await;
        let r = client
            .get(media_url)
            .header("user-agent", UA)
            .send()
            .await?;
        let status = r.status();

        // 被限流時退讓再試，而不是直接判失敗
        if status.as_u16() == 429 || status.as_u16() == 503 {
            if attempt == MAX_ATTEMPTS {
                bail!("HTTP {status}（退讓重試 {MAX_ATTEMPTS} 次後仍被限流）");
            }
            tokio::time::sleep(retry_delay(&r, attempt)).await;
            continue;
        }

        if !status.is_success() {
            let hint = match status.as_u16() {
                401 | 403 => "（可能是私人的或連結已失效）",
                404 => "（找不到這個檔案）",
                _ => "",
            };
            bail!("HTTP {status}{hint}");
        }
        res = Some(r);
        break;
    }

    let res = res.ok_or_else(|| anyhow::anyhow!("重試後仍拿不到回應"))?;

    // HEAD 與 GET 的 Content-Type 可能不同，所以這裡再擋一次。
    // 存下一個 HTML 錯誤頁卻報成功，比失敗更糟。
    if !allow_html {
        let is_html = res
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .map(|t| {
                t.split(';')
                    .next()
                    .unwrap_or(t)
                    .trim()
                    .eq_ignore_ascii_case("text/html")
            })
            .unwrap_or(false);
        if is_html {
            bail!("伺服器回的是網頁不是檔案（要存網頁請加 --any）");
        }
    }

    let total = res.content_length().unwrap_or(0);
    let file = tokio::fs::File::create(dest).await?;
    let mut writer = tokio::io::BufWriter::with_capacity(64 * 1024, file);

    let mut got: u64 = 0;
    let mut stream = res.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk?;
        got += chunk.len() as u64;
        writer.write_all(&chunk).await?;
        on_progress(got, total);
    }
    writer.flush().await?;
    writer.into_inner().sync_all().await?;

    // 位元組數對不上 Content-Length 是抓截斷最可靠的方式
    if total > 0 && got != total {
        bail!("下載不完整（{got}/{total} bytes）");
    }
    if got < 10_240 {
        bail!("檔案只有 {got} bytes，不像完整媒體檔");
    }
    Ok(got)
}

/// 從影音混合檔裡抽出音軌。`-c copy` 不重新編碼，一首歌大約 0.2 秒。
pub async fn extract_audio(ffmpeg: &Path, src: &Path, dest: &Path) -> Result<()> {
    let out = tokio::process::Command::new(ffmpeg)
        .args(["-v", "error", "-nostdin", "-y", "-i"])
        .arg(src)
        .args(["-vn", "-c:a", "copy"])
        .arg(dest)
        .stdin(std::process::Stdio::null())
        .output()
        .await?;
    if !out.status.success() {
        let msg = String::from_utf8_lossy(&out.stderr);
        let first = msg.lines().find(|l| !l.trim().is_empty()).unwrap_or("");
        bail!("抽出音軌失敗：{first}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_host_for_rate_limiting() {
        assert_eq!(
            host_of("https://Upload.Wikimedia.org/a/b.jpg"),
            "upload.wikimedia.org"
        );
        assert_eq!(host_of("http://x.com"), "x.com");
        assert_eq!(host_of("not a url"), "");
    }

    #[test]
    fn recognises_bare_media_urls() {
        assert!(looks_like_media_url("https://x.com/a/b.mp4"));
        assert!(looks_like_media_url("https://x.com/a/b.MP3?token=1"));
        assert!(!looks_like_media_url("https://x.com/watch/abc"));
        assert!(!looks_like_media_url("https://suno.com/song/abc"));
    }

    #[test]
    fn decodes_html_entities_in_titles() {
        assert_eq!(decode_entities("A &amp; B &#39;C&#39;"), "A & B 'C'");
    }

    #[test]
    fn reads_filename_from_content_disposition() {
        assert_eq!(
            filename_from_disposition(r#"attachment; filename="report.pdf""#).as_deref(),
            Some("report.pdf")
        );
        // filename* 帶編碼，且優先於 filename
        assert_eq!(
            filename_from_disposition(
                "attachment; filename=\"fallback.pdf\"; filename*=UTF-8''%E5%A0%B1%E5%91%8A.pdf"
            )
            .as_deref(),
            Some("報告.pdf")
        );
        assert_eq!(filename_from_disposition("inline").as_deref(), None);
    }

    #[test]
    fn percent_decoding_survives_malformed_input() {
        assert_eq!(percent_decode("a%20b"), "a b");
        // 壞掉的跳脫序列原樣保留，不該 panic
        assert_eq!(percent_decode("a%zzb"), "a%zzb");
        assert_eq!(percent_decode("a%"), "a%");
    }
}
