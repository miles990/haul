//! 瀏覽器 cookie 的取得與使用。
//!
//! 自己去解 Chrome 在 macOS keychain 裡的加密太脆弱，所以交給 yt-dlp——
//! 它本來就做這件事，而且跨瀏覽器跨平台都維護著。我們只負責把它匯出的
//! Netscape 檔案分給另外兩條路徑用（gallery-dl 吃檔案，直接抓取自己組
//! Cookie 標頭）。
//!
//! 安全上的立場：Haul 不碰帳號密碼。需要登入時開使用者自己的瀏覽器，
//! 在那裡登入（看得到網址列、用得到密碼管理器、走得完 2FA），
//! 我們事後只讀 cookie。

use anyhow::{bail, Result};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use tokio::process::Command;

/// yt-dlp 支援的瀏覽器（實測 yt-dlp --help 得到的清單）
pub const BROWSERS: &[&str] = &[
    "brave", "chrome", "chromium", "edge", "firefox", "opera", "safari", "vivaldi", "whale",
];

pub fn is_supported(name: &str) -> bool {
    let base = name.split([':', '+']).next().unwrap_or(name);
    BROWSERS.contains(&base.to_ascii_lowercase().as_str())
}

/// 把瀏覽器的 cookie 匯出成 Netscape 檔案。
///
/// 需要一個網址是因為 yt-dlp 的匯出綁在一次實際的萃取上；用 --simulate
/// 所以不會下載任何東西。
pub async fn export(ytdlp: &Path, browser: &str, url: &str, dest: &Path) -> Result<PathBuf> {
    if !is_supported(browser) {
        bail!("不認得的瀏覽器：{browser}（支援 {}）", BROWSERS.join("、"));
    }
    if let Some(parent) = dest.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }

    let out = Command::new(ytdlp)
        .args([
            "--ignore-config",
            "--no-warnings",
            "--simulate",
            "--skip-download",
        ])
        .arg("--cookies-from-browser")
        .arg(browser)
        .arg("--cookies")
        .arg(dest)
        .arg(url)
        .stdin(Stdio::null())
        .output()
        .await?;

    // 萃取失敗不代表 cookie 沒匯出成功，所以先看檔案在不在
    if dest.is_file() {
        restrict(dest);
        return Ok(dest.to_path_buf());
    }
    let msg = String::from_utf8_lossy(&out.stderr);
    let last = msg
        .lines()
        .rev()
        .find(|l| !l.trim().is_empty())
        .unwrap_or("yt-dlp 沒有說明原因")
        .trim();
    bail!("讀取 {browser} 的 cookie 失敗：{last}")
}

/// 把 cookie 檔鎖成只有擁有者讀得到。
///
/// 這個檔案等同於使用者的登入憑證。預設權限通常是 644，同一台機器上
/// 任何程序都讀得到；對一個放在固定路徑、長期存在的 session 檔案來說
/// 那太寬鬆了。
fn restrict(path: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
    }
    // Windows 的檔案 ACL 預設就跟著使用者設定檔走，不另外處理
    #[cfg(not(unix))]
    let _ = path;
}

/// Netscape cookie 檔的一列。
#[derive(Clone, Debug)]
pub struct Cookie {
    pub domain: String,
    pub path: String,
    pub secure: bool,
    pub http_only: bool,
    /// Unix 秒；None 代表 session cookie
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
            Some(Cookie {
                domain,
                path,
                secure,
                http_only,
                expires,
                name,
                value,
            })
        })
        .collect()
}

/// 從 Netscape cookie 檔組出某個主機能用的 Cookie 標頭。
///
/// 找不到對應的 cookie 回 None —— 沒有 cookie 跟空的 Cookie 標頭意思不同，
/// 後者有些伺服器會當成「明確表示沒有 session」。
pub fn header_for_host(file: &Path, host: &str) -> Option<String> {
    let host = host.to_ascii_lowercase();
    let pairs: Vec<String> = parse_netscape(file)
        .into_iter()
        .filter(|c| {
            // domain 完全相同，或是它的子網域
            let domain = c.domain.trim_start_matches('.').to_ascii_lowercase();
            host == domain || host.ends_with(&format!(".{domain}"))
        })
        .map(|c| format!("{}={}", c.name, c.value))
        .collect();

    (!pairs.is_empty()).then(|| pairs.join("; "))
}

/// 把網址裡看起來像機密的查詢參數遮掉。
///
/// 開始處理登入身分之後這變成必要：簽章網址常把 token 放在查詢字串，
/// 而紀錄檔會留在磁碟上、也可能被貼進 issue。
pub fn redact(url: &str) -> String {
    const SECRET_KEYS: &[&str] = &[
        "token",
        "access_token",
        "auth",
        "key",
        "sig",
        "signature",
        "password",
        "passwd",
        "secret",
        "session",
        "sid",
        "api_key",
        "apikey",
        "credential",
        "upsig",
        "hmac",
    ];

    let Some((base, query)) = url.split_once('?') else {
        return url.to_string();
    };

    let cleaned: Vec<String> = query
        .split('&')
        .map(|kv| {
            let (k, _) = kv.split_once('=').unwrap_or((kv, ""));
            let lower = k.to_ascii_lowercase();
            if SECRET_KEYS
                .iter()
                .any(|s| lower == *s || lower.ends_with(s))
            {
                format!("{k}=<已遮蔽>")
            } else {
                kv.to_string()
            }
        })
        .collect();

    format!("{base}?{}", cleaned.join("&"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_the_browsers_yt_dlp_actually_supports() {
        assert!(is_supported("chrome"));
        assert!(is_supported("Firefox"));
        // yt-dlp 允許 browser:profile 的寫法
        assert!(is_supported("chrome:Default"));
        assert!(!is_supported("netscape"));
    }

    fn write(name: &str, body: &str) -> PathBuf {
        let dir = std::env::temp_dir().join("haul-cookie-test");
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join(name);
        std::fs::write(&p, body).unwrap();
        p
    }

    #[test]
    fn builds_a_cookie_header_for_matching_hosts() {
        let f = write(
            "c1.txt",
            "# Netscape HTTP Cookie File\n\
             .example.com\tTRUE\t/\tTRUE\t0\tsess\tabc\n\
             #HttpOnly_.example.com\tTRUE\t/\tTRUE\t0\ttoken\txyz\n\
             other.com\tTRUE\t/\tTRUE\t0\tnope\t1\n",
        );
        let h = header_for_host(&f, "www.example.com").unwrap();
        assert!(h.contains("sess=abc"));
        // #HttpOnly_ 開頭的是真 cookie 不是註解
        assert!(h.contains("token=xyz"));
        assert!(!h.contains("nope"));
    }

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
        // 0 是 session cookie，不是「1970 年就過期」
        assert_eq!(rows[1].expires, None);
        assert_eq!((rows[1].name.as_str(), rows[1].value.as_str()), ("token", "xyz"));
    }

    #[test]
    fn no_matching_cookie_is_none_not_empty() {
        let f = write("c2.txt", "other.com\tTRUE\t/\tTRUE\t0\tx\t1\n");
        // 空的 Cookie 標頭跟沒有標頭意思不同，不能混為一談
        assert_eq!(header_for_host(&f, "example.com"), None);
    }

    #[test]
    fn redacts_secret_looking_query_params() {
        let got = redact("https://x.com/a.mp4?e=123&upsig=deadbeef&token=abc&os=cos");
        assert!(got.contains("e=123"), "無害的參數要保留：{got}");
        assert!(got.contains("os=cos"));
        assert!(!got.contains("deadbeef"), "簽章不該留在紀錄裡：{got}");
        assert!(!got.contains("abc"));
    }

    #[test]
    fn redact_leaves_plain_urls_alone() {
        let u = "https://x.com/a/b.mp4";
        assert_eq!(redact(u), u);
    }
}
