//! 圖庫萃取器（gallery-dl），選配。
//!
//! 跟 yt-dlp 的分工一樣：gallery-dl 只負責「這個頁面上有哪些圖」，實際下載
//! 與驗證仍由 Haul 做——每張圖變成一個一般的下載項目，共用佇列、限流、
//! 歷史與圖片驗證閘門。
//!
//! 為什麼是選配而不是像 yt-dlp 那樣自動下載：gallery-dl 最近的版本都沒有
//! 附二進位檔（實測確認），只走 PyPI 發布。替使用者自動安裝 Python 套件
//! 太越界，所以改成「有就用，沒有就明確說要裝什麼」。

use anyhow::{anyhow, bail, Result};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use tokio::process::Command;

/// gallery-dl 對不支援的網址回傳這個離開碼。實測確認：
/// 支援回 0、不支援回 64，而 stderr 的訊息格式不保證穩定。
const EXIT_UNSUPPORTED: i32 = 64;

/// 一次最多展開幾張。漫畫一話通常幾十頁，整本可能上千張；
/// 給個上限免得一個網址就把佇列灌爆。
pub const MAX_ITEMS: usize = 500;

#[derive(Debug, Clone)]
pub struct Entry {
    pub url: String,
    /// 不含副檔名 —— 搬移時會依實際下載到的檔案補上
    pub title: String,
    pub ext: String,
}

/// 找 gallery-dl：先看 Haul 自己的 bin 目錄，再看 PATH。
pub fn find(bin_dir: &Path) -> Option<PathBuf> {
    let name = if cfg!(windows) {
        "gallery-dl.exe"
    } else {
        "gallery-dl"
    };

    let own = bin_dir.join(name);
    if own.is_file() {
        return Some(own);
    }

    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|d| d.join(name))
        .find(|p| p.is_file())
}

/// 這個網址 gallery-dl 認不認得。用 --simulate 問，不會下載任何東西。
pub async fn supported(bin: &Path, url: &str) -> bool {
    match Command::new(bin)
        .args(["--simulate", "--range", "1-1", url])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await
    {
        Ok(s) => s.code() != Some(EXIT_UNSUPPORTED),
        Err(_) => false,
    }
}

/// 列出頁面上的圖片網址，不下載。
pub async fn list(
    bin: &Path,
    url: &str,
    limit: usize,
    cookie_file: Option<&Path>,
) -> Result<Vec<Entry>> {
    let limit = limit.clamp(1, MAX_ITEMS);
    let mut cmd = Command::new(bin);
    cmd.args(["--dump-json", "--range", &format!("1-{limit}")]);
    // gallery-dl 要的是檔案不是瀏覽器名稱，所以共用 yt-dlp 匯出的那份
    if let Some(f) = cookie_file {
        cmd.arg("--cookies").arg(f);
    }
    let out = cmd
        .arg(url)
        .stdin(Stdio::null())
        .output()
        .await
        .map_err(|e| anyhow!("執行 gallery-dl 失敗：{e}"))?;

    if out.status.code() == Some(EXIT_UNSUPPORTED) {
        bail!("gallery-dl 不支援這個網址");
    }
    if !out.status.success() {
        let msg = String::from_utf8_lossy(&out.stderr);
        let last = msg
            .lines()
            .rev()
            .find(|l| !l.trim().is_empty())
            .unwrap_or("gallery-dl 沒有給出原因")
            .trim();
        bail!("{last}");
    }

    let v: serde_json::Value = serde_json::from_slice(&out.stdout)
        .map_err(|e| anyhow!("看不懂 gallery-dl 的輸出：{e}"))?;

    let entries = parse_dump(&v);
    if entries.is_empty() {
        // 離開碼 0 不代表有東西：頁面可能是空的分類或需要登入
        bail!("這個頁面上沒有找到可下載的圖片");
    }
    Ok(entries)
}

/// 解析 --dump-json。每筆是 `[depth, url, metadata]`（檔案）
/// 或 `[depth, metadata]`（目錄，沒有網址）。
fn parse_dump(v: &serde_json::Value) -> Vec<Entry> {
    let Some(rows) = v.as_array() else {
        return Vec::new();
    };

    rows.iter()
        .filter_map(|row| {
            let row = row.as_array()?;
            // 只有三元素的那種才帶網址
            let url = row.get(1)?.as_str()?;
            if !url.starts_with("http") {
                return None;
            }
            let meta = row.get(2)?.as_object()?;

            // 子分類那種列也帶網址，但指向的是另一個頁面而不是檔案。
            // 沒有 extension 就不是檔案 —— 少了這道過濾會把分類頁當成
            // 圖片抓下來，而且因為繞過 direct 的 HTML 守衛還會「通過」驗證。
            let ext = meta.get("extension").and_then(|e| e.as_str())?;
            if ext.is_empty() || ext.len() > 5 {
                return None;
            }

            let stem = meta.get("filename").and_then(|f| f.as_str()).unwrap_or("");
            // title 不帶副檔名：搬移時會依實際檔案補上，
            // 這裡再接一次就會變成 a.jpg.jpg
            let title = if stem.is_empty() {
                url.split('?')
                    .next()
                    .unwrap_or(url)
                    .rsplit('/')
                    .next()
                    .and_then(|f| f.rsplit_once('.').map(|(s, _)| s.to_string()))
                    .unwrap_or_else(|| "image".into())
            } else {
                stem.to_string()
            };

            Some(Entry {
                url: url.to_string(),
                title,
                ext: ext.to_string(),
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn picks_only_rows_that_carry_a_url() {
        // 真實輸出：目錄那筆只有 [depth, metadata]，沒有網址
        let v = json!([
            [1, { "subcategory": "category", "title": "Cats" }],
            [2, "https://example.com/a.jpg", { "filename": "a cat", "extension": "jpg" }],
            [2, "https://example.com/b.png", { "filename": "b cat", "extension": "png" }]
        ]);
        let got = parse_dump(&v);
        assert_eq!(got.len(), 2, "目錄那筆不該被當成圖片");
        assert_eq!(
            got[0].title, "a cat",
            "title 不該帶副檔名，否則會變成 a.jpg.jpg"
        );
        assert_eq!(got[0].ext, "jpg");
        assert_eq!(got[1].url, "https://example.com/b.png");
    }

    #[test]
    fn falls_back_to_the_url_tail_when_filename_is_missing() {
        let v = json!([[2, "https://example.com/x/y/img_09.jpg?token=1", { "extension": "jpg" }]]);
        let got = parse_dump(&v);
        assert_eq!(got[0].title, "img_09");
    }

    /// 真實案例：子分類列也帶網址，但指向頁面不是檔案。
    /// 沒過濾掉的話會下載到一個 .bin 的 HTML，而且還「驗證通過」。
    #[test]
    fn subcategory_rows_that_carry_a_page_url_are_rejected() {
        let v = json!([[
            1,
            "https://commons.wikimedia.org/wiki/Category:Turkish_Cat_Research_Centres",
            { "subcategory": "category", "title": "Turkish Cat Research Centres" }
        ]]);
        assert!(parse_dump(&v).is_empty(), "沒有 extension 就不是檔案");
    }

    #[test]
    fn ignores_non_http_and_malformed_rows() {
        let v = json!([
            [2, "data:image/png;base64,AAAA", {}],
            ["not a row"],
            [1],
            [2, 12345, {}]
        ]);
        assert!(parse_dump(&v).is_empty());
    }

    #[test]
    fn empty_input_is_not_a_panic() {
        assert!(parse_dump(&json!([])).is_empty());
        assert!(parse_dump(&json!({})).is_empty());
    }
}
