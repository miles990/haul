//! 解析輸入的網址、串流下載。全程純 HTTP，不開瀏覽器。

use anyhow::{bail, Result};
use futures_util::StreamExt;
use regex::Regex;
use reqwest::Client;
use std::path::Path;
use std::sync::OnceLock;
use tokio::io::AsyncWriteExt;

pub const UA: &str = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 \
                      (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36";

/// 一次讀 64KiB，記憶體用量與檔案大小無關
const CHUNK_HINT: usize = 64 * 1024;

fn uuid_re() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| {
        Regex::new(r"(?i)[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}").unwrap()
    })
}

fn og_re() -> &'static [Regex; 2] {
    static R: OnceLock<[Regex; 2]> = OnceLock::new();
    R.get_or_init(|| {
        [
            // property 在 content 前面
            Regex::new(
                r#"(?is)<meta[^>]+(?:property|name)=["']og:title["'][^>]*content=["']([^"']*)["']"#,
            )
            .unwrap(),
            // content 在 property 前面
            Regex::new(
                r#"(?is)<meta[^>]+content=["']([^"']*)["'][^>]*(?:property|name)=["']og:title["']"#,
            )
            .unwrap(),
        ]
    })
}

pub fn build_client() -> Result<Client> {
    Ok(Client::builder()
        .user_agent(UA)
        .connect_timeout(std::time::Duration::from_secs(15))
        // 不設整體 timeout：長檔案下載不該被砍
        .pool_max_idle_per_host(4)
        .build()?)
}

/// 從一段文字裡挑出所有看起來像輸入的行（網址或裸 UUID）
pub fn split_inputs(text: &str) -> Vec<String> {
    text.split(|c: char| c.is_whitespace() || c == ',' || c == ';')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .filter(|s| s.starts_with("http") || uuid_re().is_match(s))
        .map(|s| {
            s.trim_end_matches(&['/', ')', ']', '.', ','][..])
                .to_string()
        })
        .collect()
}

/// `https://suno.com/song/<uuid>` 或整串就是一個 uuid
pub fn song_id_from_input(input: &str) -> Option<String> {
    let lower = input.to_lowercase();
    if let Some(pos) = lower.find("/song/") {
        if let Some(m) = uuid_re().find(&lower[pos..]) {
            return Some(m.as_str().to_string());
        }
    }
    let t = input.trim();
    if let Some(m) = uuid_re().find(t) {
        if m.start() == 0 && m.end() == t.len() {
            return Some(t.to_lowercase());
        }
    }
    None
}

pub fn audio_url(id: &str) -> String {
    format!("https://cdn1.suno.ai/{id}.mp3")
}

pub fn page_url(id: &str) -> String {
    format!("https://suno.com/song/{id}")
}

/// HEAD 探一下這個 id 到底有沒有音檔，用來過濾頁面上撈到的雜訊 UUID
async fn looks_like_song(client: &Client, id: &str) -> bool {
    match client.head(audio_url(id)).send().await {
        Ok(r) => {
            if !r.status().is_success() {
                return false;
            }
            match r.content_length() {
                Some(n) => n > 100_000, // 小於 100KB 不可能是一首歌
                None => true,
            }
        }
        Err(_) => false,
    }
}

/// 單首 → 自己；清單／個人頁 → 撈出頁面裡所有歌曲 id（HEAD 驗證過的才留）
pub async fn expand(client: &Client, input: &str) -> Result<Vec<String>> {
    if let Some(id) = song_id_from_input(input) {
        return Ok(vec![id]);
    }
    if !input.starts_with("http") {
        bail!("看不懂這個輸入");
    }

    let html = client
        .get(input)
        .header("accept", "text/html,application/xhtml+xml")
        .send()
        .await?
        .text()
        .await?;

    let mut seen = std::collections::HashSet::new();
    let candidates: Vec<String> = uuid_re()
        .find_iter(&html)
        .map(|m| m.as_str().to_lowercase())
        .filter(|id| seen.insert(id.clone()))
        .take(300)
        .collect();

    if candidates.is_empty() {
        bail!("這個頁面裡找不到歌曲（清單頁可能是純前端渲染，請改貼單曲網址）");
    }

    // 併發 8 條探測，別把 CDN 打爆
    let ids: Vec<String> = futures_util::stream::iter(candidates)
        .map(|id| {
            let c = client.clone();
            async move { looks_like_song(&c, &id).await.then_some(id) }
        })
        .buffer_unordered(8)
        .filter_map(|x| async move { x })
        .collect()
        .await;

    if ids.is_empty() {
        bail!("頁面上的 id 都不是可下載的歌曲");
    }
    Ok(ids)
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
    // 數字實體
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

/// 抓歌名。抓不到不是錯誤 —— 退回用 id 當檔名，下載照跑。
pub async fn fetch_title(client: &Client, id: &str) -> Option<String> {
    let html = client
        .get(page_url(id))
        .send()
        .await
        .ok()?
        .text()
        .await
        .ok()?;
    let raw = og_re()
        .iter()
        .find_map(|re| re.captures(&html).map(|c| c[1].to_string()))
        .or_else(|| {
            Regex::new(r"(?is)<title[^>]*>([^<]*)</title>")
                .ok()?
                .captures(&html)
                .map(|c| c[1].to_string())
        })?;

    let t = decode_entities(&raw);
    // 去掉 "… | Suno" 這種站名後綴
    let t = t
        .rsplit_once(" | ")
        .filter(|(_, tail)| tail.trim().eq_ignore_ascii_case("suno"))
        .map_or(t.as_str(), |(head, _)| head)
        .trim()
        .to_string();

    (!t.is_empty()).then_some(t)
}

/// 串流下載到 `dest`。回傳寫入的位元組數。
/// `on_progress(got, total)` 由呼叫端自己節流。
pub async fn download<F>(client: &Client, url: &str, dest: &Path, mut on_progress: F) -> Result<u64>
where
    F: FnMut(u64, u64),
{
    let res = client.get(url).send().await?;
    let status = res.status();
    if !status.is_success() {
        let hint = match status.as_u16() {
            401 | 403 => "（連結可能是私人的或已失效）",
            404 => "（找不到這首歌）",
            _ => "",
        };
        bail!("HTTP {status}{hint}");
    }

    let total = res.content_length().unwrap_or(0);
    let file = tokio::fs::File::create(dest).await?;
    let mut writer = tokio::io::BufWriter::with_capacity(CHUNK_HINT, file);

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

    // 閘門一：位元組數必須跟 Content-Length 對得上，這是抓截斷最可靠的方式
    if total > 0 && got != total {
        bail!("下載不完整（{got}/{total} bytes）");
    }
    if got < 10_240 {
        bail!("檔案只有 {got} bytes，不像完整音檔");
    }
    Ok(got)
}
