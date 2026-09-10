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
use std::path::Path;
use std::sync::OnceLock;
use tokio::io::AsyncWriteExt;

pub const UA: &str = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 \
                      (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36";

#[derive(Debug, Clone)]
pub struct Found {
    pub media: String,
    pub title: String,
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

    Ok(Found { media, title })
}

/// 試著在不靠 yt-dlp 的情況下找出媒體檔。找不到就回 Err，呼叫端據此
/// 決定要回報哪個錯誤。
pub async fn probe(client: &Client, url: &str) -> Result<Found> {
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

    bail!("沒有對應的直接抓取規則")
}

/// 串流下載到 `dest`，記憶體用量與檔案大小無關。
pub async fn download<F>(
    client: &Client,
    media_url: &str,
    dest: &Path,
    mut on_progress: F,
) -> Result<u64>
where
    F: FnMut(u64, u64),
{
    let res = client
        .get(media_url)
        .header("user-agent", UA)
        .send()
        .await?;
    let status = res.status();
    if !status.is_success() {
        let hint = match status.as_u16() {
            401 | 403 => "（可能是私人的或連結已失效）",
            404 => "（找不到這個檔案）",
            _ => "",
        };
        bail!("HTTP {status}{hint}");
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

    #[tokio::test]
    async fn refuses_hosts_without_a_rule() {
        let c = Client::new();
        assert!(probe(&c, "https://example.com/watch/abc").await.is_err());
    }
}
