//! 一般網頁上的圖片。圖片模式對 yt-dlp 與 gallery-dl 都不認得的頁面
//! （GitHub README、部落格文章）的最後一層：抓 HTML，把 `<img>`、`srcset`、
//! `og:image` 裡的網址挑出來，每張變成一個一般的直接下載項目。
//!
//! 不引進 HTML parser——幾個 regex 夠用，錯抓的會被圖片驗證閘門擋掉。
//! 頭像、徽章、追蹤像素這類雜訊靠路徑關鍵字與檔案大小過濾。

use anyhow::{anyhow, bail, Result};
use futures_util::{stream, StreamExt};
use regex::Regex;
use reqwest::{Client, Url};
use std::sync::OnceLock;

use crate::direct::UA;

/// 一頁最多展開幾張，免得一個網址就把佇列灌爆
pub const MAX_ITEMS: usize = 200;

/// 小於這個大小的多半是圖示、徽章、追蹤像素
pub const MIN_BYTES: u64 = 8 * 1024;

/// 同時問幾張的大小
const HEAD_CONCURRENCY: usize = 8;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Image {
    pub url: String,
    /// 不含副檔名 —— 搬移時會依實際下載到的檔案補上
    pub title: String,
    pub content_type: Option<String>,
}

#[derive(Debug)]
pub struct Page {
    pub title: String,
    pub images: Vec<Image>,
}

fn re(cell: &'static OnceLock<Regex>, pat: &str) -> &'static Regex {
    cell.get_or_init(|| Regex::new(pat).unwrap())
}

fn img_tag_re() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    re(&R, r#"(?is)<img\b[^>]*>"#)
}

fn attr_re() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    re(
        &R,
        r#"(?is)\b(src|data-src|srcset|data-srcset)\s*=\s*["']([^"']+)["']"#,
    )
}

fn og_re() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    re(
        &R,
        r#"(?is)<meta\b[^>]*property\s*=\s*["']og:image["'][^>]*content\s*=\s*["']([^"']+)["']"#,
    )
}

fn link_re() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    re(
        &R,
        r#"(?is)<a\b[^>]*href\s*=\s*["']([^"']+\.(?:png|jpe?g|gif|webp|avif)(?:\?[^"']*)?)["']"#,
    )
}

fn title_re() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    re(&R, r#"(?is)<title[^>]*>(.*?)</title>"#)
}

/// 路徑裡有這些字的幾乎都不是內容圖
const JUNK: &[&str] = &[
    "avatar",
    "emoji",
    "badge",
    "favicon",
    "icon",
    "logo",
    "spinner",
    "pixel",
    "shields.io",
    "gravatar",
    "/s/",
    "sprite",
    "tracking",
];

fn unescape(s: &str) -> String {
    s.replace("&amp;", "&")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
}

/// srcset 裡挑描述子最大的那個（"a.jpg 1x, b.jpg 2x" 或 "a.jpg 400w, b.jpg 800w"）
fn largest_in_srcset(srcset: &str) -> Option<&str> {
    srcset
        .split(',')
        .filter_map(|part| {
            let mut it = part.split_whitespace();
            let url = it.next()?;
            let n = it
                .next()
                .and_then(|d| d.trim_end_matches(['x', 'w']).parse::<f64>().ok())
                .unwrap_or(1.0);
            Some((n, url))
        })
        .max_by(|a, b| a.0.total_cmp(&b.0))
        .map(|(_, u)| u)
}

fn is_junk(url: &str) -> bool {
    let lower = url.to_ascii_lowercase();
    lower.starts_with("data:")
        || lower.ends_with(".svg")
        || lower.contains(".svg?")
        || JUNK.iter().any(|k| lower.contains(k))
}

fn stem_of(url: &Url) -> Option<String> {
    let last = url.path_segments()?.filter(|s| !s.is_empty()).next_back()?;
    let stem = last.rsplit_once('.').map(|(s, _)| s).unwrap_or(last);
    let stem = stem.trim();
    (!stem.is_empty()).then(|| stem.to_string())
}

/// 純解析：從 HTML 裡挑出候選圖片網址（已轉成絕對網址、去重、去雜訊）。
pub fn extract(html: &str, base: &Url) -> Page {
    let title = title_re()
        .captures(html)
        .map(|c| unescape(c[1].trim()))
        .filter(|t| !t.is_empty())
        .unwrap_or_else(|| base.host_str().unwrap_or("page").to_string());

    let mut raw: Vec<String> = Vec::new();
    for tag in img_tag_re().find_iter(html) {
        let mut src = None;
        let mut best = None;
        for a in attr_re().captures_iter(tag.as_str()) {
            let v = unescape(&a[2]);
            match &a[1].to_ascii_lowercase()[..] {
                "srcset" | "data-srcset" => {
                    best = largest_in_srcset(&v).map(str::to_string);
                }
                _ => src = Some(v),
            }
        }
        if let Some(u) = best.or(src) {
            raw.push(u);
        }
    }
    for c in link_re().captures_iter(html) {
        raw.push(unescape(&c[1]));
    }
    // og:image 多半是社群卡片，不是內容；只在頁面上什麼圖都沒有時才拿它
    if raw.iter().all(|u| is_junk(u)) {
        for c in og_re().captures_iter(html) {
            raw.push(unescape(&c[1]));
        }
    }

    let mut seen = std::collections::HashSet::new();
    let mut images = Vec::new();
    for u in raw {
        if is_junk(&u) {
            continue;
        }
        let Ok(abs) = base.join(u.trim()) else {
            continue;
        };
        if !matches!(abs.scheme(), "http" | "https") {
            continue;
        }
        let s = abs.to_string();
        if !seen.insert(s.clone()) {
            continue;
        }
        images.push(Image {
            title: stem_of(&abs).unwrap_or_else(|| format!("image-{}", images.len() + 1)),
            url: s,
            content_type: None,
        });
        if images.len() >= MAX_ITEMS {
            break;
        }
    }
    Page { title, images }
}

/// 抓頁面、挑圖、問大小。太小的（圖示、追蹤像素）與明確不是圖片的丟掉；
/// 伺服器不肯講大小的保留，交給下載後的驗證。
pub async fn scrape(client: &Client, url: &str, cookie: Option<&str>) -> Result<Page> {
    Url::parse(url).map_err(|e| anyhow!("不是合法網址：{e}"))?;
    let mut req = client.get(url).header("user-agent", UA);
    if let Some(c) = cookie {
        req = req.header("cookie", c);
    }
    let res = req.send().await.map_err(|e| anyhow!("抓不到頁面：{e}"))?;
    if !res.status().is_success() {
        bail!("HTTP {}", res.status().as_u16());
    }
    let final_url = res.url().clone();
    let html = res.text().await.map_err(|e| anyhow!("讀頁面失敗：{e}"))?;
    let page = extract(&html, &final_url);
    if page.images.is_empty() {
        bail!("這一頁上找不到圖片");
    }
    let final_base = final_url.clone();

    let kept: Vec<Image> = stream::iter(page.images)
        .map(|img| {
            let client = client.clone();
            let cookie = cookie.map(str::to_string);
            let referer = final_base.to_string();
            async move {
                let mut req = client
                    .head(&img.url)
                    .header("user-agent", UA)
                    .header("referer", referer);
                if let Some(c) = cookie {
                    req = req.header("cookie", c);
                }
                let Ok(res) = req.send().await else {
                    return Some(img); // 問不到就留著，讓下載階段裁決
                };
                if !res.status().is_success() {
                    return None;
                }
                let ct = res
                    .headers()
                    .get("content-type")
                    .and_then(|v| v.to_str().ok())
                    .map(|s| s.split(';').next().unwrap_or(s).trim().to_ascii_lowercase());
                if let Some(ct) = &ct {
                    if !ct.starts_with("image/") && ct != "application/octet-stream" {
                        return None;
                    }
                }
                if let Some(len) = res.content_length() {
                    if len < MIN_BYTES {
                        return None;
                    }
                }
                Some(Image {
                    content_type: ct,
                    ..img
                })
            }
        })
        .buffered(HEAD_CONCURRENCY)
        .filter_map(|x| async move { x })
        .collect()
        .await;

    if kept.is_empty() {
        bail!("這一頁上的圖片都太小或不是圖片（圖示、徽章之類）");
    }
    Ok(Page {
        title: page.title,
        images: kept,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base() -> Url {
        Url::parse("https://example.com/user/repo").unwrap()
    }

    #[test]
    fn picks_img_src_and_resolves_relative_urls() {
        let html = r#"<html><head><title>Repo &amp; Stuff</title></head>
            <body><img src="/user/repo/raw/master/images/a.png">
            <img src="https://cdn.example.com/b.jpg"></body></html>"#;
        let p = extract(html, &base());
        assert_eq!(p.title, "Repo & Stuff");
        let urls: Vec<_> = p.images.iter().map(|i| i.url.as_str()).collect();
        assert_eq!(
            urls,
            [
                "https://example.com/user/repo/raw/master/images/a.png",
                "https://cdn.example.com/b.jpg"
            ]
        );
        assert_eq!(p.images[0].title, "a");
    }

    #[test]
    fn srcset_beats_src_and_takes_the_largest() {
        let html = r#"<img src="small.jpg" srcset="m.jpg 800w, l.jpg 1600w, s.jpg 400w">"#;
        let p = extract(html, &base());
        assert_eq!(p.images.len(), 1);
        assert_eq!(p.images[0].url, "https://example.com/user/l.jpg");
    }

    #[test]
    fn drops_avatars_badges_svg_and_data_uris() {
        let html = r#"
            <img src="https://avatars.githubusercontent.com/u/1?v=4">
            <img src="https://img.shields.io/badge/x-y-blue">
            <img src="/logo.svg">
            <img src="data:image/png;base64,AAAA">
            <img src="/photos/real.jpg">"#;
        let p = extract(html, &base());
        assert_eq!(p.images.len(), 1);
        assert!(p.images[0].url.ends_with("/photos/real.jpg"));
    }

    #[test]
    fn dedupes_and_includes_image_links() {
        let html = r#"<img src="https://x.com/a.png">
            <img src="https://x.com/a.png">
            <a href="https://x.com/full.jpeg?size=large">full</a>"#;
        let p = extract(html, &base());
        let urls: Vec<_> = p.images.iter().map(|i| i.url.as_str()).collect();
        assert_eq!(
            urls,
            ["https://x.com/a.png", "https://x.com/full.jpeg?size=large"]
        );
    }

    #[test]
    fn og_image_is_only_a_fallback() {
        // 有內容圖時社群卡片不算
        let html = r#"<meta property="og:image" content="https://x.com/og.png">
            <img src="https://x.com/a.png">"#;
        assert_eq!(extract(html, &base()).images.len(), 1);
        // 什麼圖都沒有（JS 才畫出來的頁面）才拿它
        let html = r#"<meta property="og:image" content="https://x.com/og.png">
            <img src="https://x.com/avatar.png">"#;
        let p = extract(html, &base());
        assert_eq!(p.images.len(), 1);
        assert_eq!(p.images[0].url, "https://x.com/og.png");
    }

    #[test]
    fn caps_the_number_of_images() {
        let html: String = (0..(MAX_ITEMS + 50))
            .map(|i| format!(r#"<img src="/p/{i}.jpg">"#))
            .collect();
        assert_eq!(extract(&html, &base()).images.len(), MAX_ITEMS);
    }

    #[test]
    fn falls_back_to_host_when_there_is_no_title() {
        let p = extract(r#"<img src="/a.png">"#, &base());
        assert_eq!(p.title, "example.com");
    }
}
