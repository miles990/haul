//! 外部執行檔（yt-dlp / ffmpeg）的取得與管理。
//!
//! 刻意不把它們包進安裝檔：yt-dlp 必須能獨立更新。各站的播放器每隔幾週
//! 就會改，凍在安裝檔裡的版本會在幾週內開始壞掉，而使用者只能等我重新
//! 發布。放在資料夾裡就能隨時抽換。

use anyhow::{anyhow, bail, Context, Result};
use futures_util::StreamExt;
use std::path::{Path, PathBuf};
use tokio::io::AsyncWriteExt;

#[derive(Clone, Debug)]
pub struct Tools {
    pub ytdlp: PathBuf,
    pub ffmpeg: PathBuf,
}

/// yt-dlp 官方 release。macOS 那支是 universal，Intel 與 Apple Silicon 通用。
fn ytdlp_url() -> Result<&'static str> {
    Ok(match std::env::consts::OS {
        "macos" => "https://github.com/yt-dlp/yt-dlp/releases/latest/download/yt-dlp_macos",
        "windows" => "https://github.com/yt-dlp/yt-dlp/releases/latest/download/yt-dlp.exe",
        "linux" => "https://github.com/yt-dlp/yt-dlp/releases/latest/download/yt-dlp_linux",
        os => bail!("不支援的作業系統：{os}"),
    })
}

/// ffmpeg 沒有官方的單一靜態執行檔下載點。yt-dlp 自家的 FFmpeg-Builds
/// 只出 Windows/Linux，macOS 是 404，所以統一走 eugeneware/ffmpeg-static
/// ——它三個平台都有，而且用的是穩定的 latest 網址。
fn ffmpeg_url() -> Result<String> {
    let slug = match (std::env::consts::OS, std::env::consts::ARCH) {
        ("macos", "aarch64") => "darwin-arm64",
        ("macos", "x86_64") => "darwin-x64",
        ("windows", "x86_64") => "win32-x64",
        ("windows", "aarch64") => "win32-x64", // Windows on ARM 走 x64 模擬
        ("linux", "x86_64") => "linux-x64",
        ("linux", "aarch64") => "linux-arm64",
        (os, arch) => bail!("不支援的平台：{os}/{arch}"),
    };
    Ok(format!(
        "https://github.com/eugeneware/ffmpeg-static/releases/latest/download/ffmpeg-{slug}"
    ))
}

fn exe_name(stem: &str) -> String {
    if cfg!(windows) {
        format!("{stem}.exe")
    } else {
        stem.to_string()
    }
}

/// 真的執行一次確認它能跑。只檢查檔案存在會漏掉「下載到一半」和
/// 「架構不對」這兩種情況。
///
/// 版本旗標兩支工具不一樣：yt-dlp 是 `--version`，ffmpeg 是單破折號的
/// `-version`。給 ffmpeg 打 `--version` 會被當成不認識的選項而回非零，
/// 看起來就像「架構不符」。
async fn works(path: &Path, version_flag: &str) -> bool {
    if !path.exists() {
        return false;
    }
    matches!(
        tokio::process::Command::new(path)
            .arg(version_flag)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .await,
        Ok(s) if s.success()
    )
}

#[cfg(unix)]
fn make_executable(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mut p = std::fs::metadata(path)?.permissions();
    p.set_mode(0o755);
    std::fs::set_permissions(path, p)?;
    Ok(())
}

#[cfg(not(unix))]
fn make_executable(_path: &Path) -> Result<()> {
    Ok(())
}

/// 下載到 `.part` 再改名，中途失敗不會留下半個看起來能用的檔案
async fn fetch(
    client: &reqwest::Client,
    url: &str,
    dest: &Path,
    mut on_progress: impl FnMut(u64, u64),
) -> Result<()> {
    let res = client
        .get(url)
        .send()
        .await
        .with_context(|| format!("連不上 {url}"))?;
    if !res.status().is_success() {
        bail!("下載 {url} 失敗：HTTP {}", res.status());
    }
    let total = res.content_length().unwrap_or(0);

    let part = dest.with_extension("part");
    let file = tokio::fs::File::create(&part).await?;
    let mut writer = tokio::io::BufWriter::with_capacity(64 * 1024, file);

    // 節流：一個 45MB 的檔案約 2800 個 chunk，每個都回報會把事件佇列灌爆，
    // 後面的「完成」事件要排很久才輪得到，進度列因此看起來像卡住。
    let tick = std::time::Duration::from_millis(200);
    let mut last = std::time::Instant::now() - tick;

    let mut got: u64 = 0;
    let mut stream = res.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk?;
        got += chunk.len() as u64;
        writer.write_all(&chunk).await?;
        if last.elapsed() >= tick {
            last = std::time::Instant::now();
            on_progress(got, total);
        }
    }
    writer.flush().await?;
    drop(writer);

    if total > 0 && got != total {
        let _ = tokio::fs::remove_file(&part).await;
        bail!("下載不完整（{got}/{total} bytes）");
    }

    tokio::fs::rename(&part, dest).await?;
    make_executable(dest)?;
    Ok(())
}

/// 確保兩支工具都在 `dir` 裡且能執行，缺的才下載。
///
/// `on_step(工具名, 已下載, 總計)`；總計為 0 表示還在確認階段。
pub async fn ensure(
    client: &reqwest::Client,
    dir: &Path,
    mut on_step: impl FnMut(&str, u64, u64),
) -> Result<Tools> {
    tokio::fs::create_dir_all(dir).await?;

    let ytdlp = dir.join(exe_name("yt-dlp"));
    let ffmpeg = dir.join(exe_name("ffmpeg"));

    for (name, path, url, flag) in [
        ("yt-dlp", &ytdlp, ytdlp_url()?.to_string(), "--version"),
        ("ffmpeg", &ffmpeg, ffmpeg_url()?, "-version"),
    ] {
        if works(path, flag).await {
            continue;
        }
        on_step(name, 0, 0);
        fetch(client, &url, path, |got, total| on_step(name, got, total)).await?;
        if !works(path, flag).await {
            bail!("{name} 下載完成但無法執行，可能是架構不符");
        }
    }

    Ok(Tools { ytdlp, ffmpeg })
}

/// 讓 yt-dlp 自己更新。各站一改版就靠這個跟上，不必等 Haul 重新發布。
pub async fn update_ytdlp(tools: &Tools) -> Result<String> {
    let out = tokio::process::Command::new(&tools.ytdlp)
        .arg("--update")
        .output()
        .await
        .map_err(|e| anyhow!("執行 yt-dlp --update 失敗：{e}"))?;
    let text = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if out.status.success() {
        Ok(text)
    } else {
        bail!("{}", String::from_utf8_lossy(&out.stderr).trim())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_urls_for_this_platform() {
        // 在支援的平台上兩者都要能組出網址，不會 panic
        assert!(ytdlp_url().unwrap().starts_with("https://"));
        assert!(ffmpeg_url().unwrap().starts_with("https://"));
    }

    #[test]
    fn exe_name_matches_platform() {
        let n = exe_name("yt-dlp");
        if cfg!(windows) {
            assert_eq!(n, "yt-dlp.exe");
        } else {
            assert_eq!(n, "yt-dlp");
        }
    }

    #[tokio::test]
    async fn missing_binary_is_not_usable() {
        assert!(!works(Path::new("/definitely/not/here/yt-dlp"), "--version").await);
    }

    /// ffmpeg 只認單破折號的 -version。給它 --version 會回非零，
    /// 於是好好的執行檔會被誤判成「架構不符」。
    #[tokio::test]
    async fn ffmpeg_rejects_double_dash_version() {
        let Ok(sys_ffmpeg) = which_ffmpeg() else {
            return; // 這台機器上沒有 ffmpeg 可測，跳過
        };
        assert!(works(&sys_ffmpeg, "-version").await, "-version 應該要能跑");
        assert!(
            !works(&sys_ffmpeg, "--version").await,
            "--version 會失敗，這正是當初誤判的原因"
        );
    }

    /// 找一支系統上的 ffmpeg 來當測試素材，找不到就讓呼叫端跳過
    #[cfg(test)]
    fn which_ffmpeg() -> Result<PathBuf> {
        for p in [
            "/opt/homebrew/bin/ffmpeg",
            "/usr/local/bin/ffmpeg",
            "/usr/bin/ffmpeg",
        ] {
            let path = PathBuf::from(p);
            if path.exists() {
                return Ok(path);
            }
        }
        bail!("找不到系統 ffmpeg")
    }
}
