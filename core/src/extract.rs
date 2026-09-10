//! 驅動 yt-dlp 做萃取與下載。
//!
//! 這裡刻意不放任何站點專用規則。上一版硬編了 Suno 的 CDN 路徑，
//! 結果第一次碰真實連結就 403 —— 站點會改，維護 extractor 是一整個
//! 社群的工作，不是這支程式該扛的。

use anyhow::{anyhow, bail, Result};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;

use crate::tools::Tools;

/// stdout/stderr 上的標記。yt-dlp 在不同情況下會把進度寫到不同的串流，
/// 所以兩邊都掃，不去賭它寫在哪一邊。
const MARK_PROGRESS: &str = "HAULPROG ";
const MARK_FILE: &str = "HAULFILE ";
const MARK_DUR: &str = "HAULDUR ";

/// 保留多少行 stderr 用來回報錯誤
const STDERR_TAIL: usize = 12;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Mode {
    /// 影片：最佳畫質 + 最佳音軌，合併成 mp4
    Video,
    /// 只要聲音：抽出原始音軌，能直接複製就不重新編碼
    Audio,
}

impl Mode {
    pub fn parse(s: &str) -> Self {
        if s == "audio" {
            Mode::Audio
        } else {
            Mode::Video
        }
    }
}

#[derive(Debug)]
pub struct Entry {
    pub url: String,
    pub title: Option<String>,
}

#[derive(Debug)]
pub enum Probe {
    Single { title: String },
    Playlist { title: String, entries: Vec<Entry> },
}

fn base_args(cmd: &mut Command) {
    // --ignore-config：不要讓使用者既有的 yt-dlp 設定檔改變我們的行為
    cmd.args(["--ignore-config", "--no-warnings", "--no-colors"]);
}

fn stderr_tail(lines: &[String]) -> String {
    let msg = lines
        .iter()
        .rev()
        .filter(|l| !l.trim().is_empty())
        .take(3)
        .map(|s| s.trim())
        .collect::<Vec<_>>()
        .join(" / ");
    if msg.is_empty() {
        "yt-dlp 沒有給出原因".into()
    } else {
        msg
    }
}

/// 問 yt-dlp 這個連結是什麼，不下載任何媒體。
pub async fn probe(tools: &Tools, url: &str) -> Result<Probe> {
    let mut cmd = Command::new(&tools.ytdlp);
    base_args(&mut cmd);
    cmd.args(["-J", "--flat-playlist"]).arg(url);

    let out = cmd
        .output()
        .await
        .map_err(|e| anyhow!("執行 yt-dlp 失敗：{e}"))?;

    if !out.status.success() {
        let lines: Vec<String> = String::from_utf8_lossy(&out.stderr)
            .lines()
            .map(str::to_string)
            .collect();
        bail!("{}", stderr_tail(&lines));
    }

    let v: serde_json::Value =
        serde_json::from_slice(&out.stdout).map_err(|e| anyhow!("看不懂 yt-dlp 的輸出：{e}"))?;

    let title = v
        .get("title")
        .and_then(|t| t.as_str())
        .unwrap_or("untitled")
        .to_string();

    if v.get("_type").and_then(|t| t.as_str()) == Some("playlist") {
        let entries = v
            .get("entries")
            .and_then(|e| e.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|e| {
                        let url = e
                            .get("url")
                            .or_else(|| e.get("webpage_url"))
                            .and_then(|u| u.as_str())?;
                        Some(Entry {
                            url: url.to_string(),
                            title: e.get("title").and_then(|t| t.as_str()).map(str::to_string),
                        })
                    })
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();

        if entries.is_empty() {
            bail!("這是一份清單，但裡面沒有可下載的項目");
        }
        return Ok(Probe::Playlist { title, entries });
    }

    Ok(Probe::Single { title })
}

#[derive(Debug)]
pub struct Downloaded {
    pub path: PathBuf,
    /// yt-dlp 回報的時長。用來決定影片抽樣的取樣點。
    pub secs: Option<f64>,
}

/// 下載一個項目到 `staging`，回傳 yt-dlp 實際寫出來的檔案。
pub async fn download<F>(
    tools: &Tools,
    url: &str,
    mode: Mode,
    staging: &Path,
    mut on_progress: F,
) -> Result<Downloaded>
where
    F: FnMut(u64, u64),
{
    let mut cmd = Command::new(&tools.ytdlp);
    base_args(&mut cmd);
    cmd.args([
        "--newline",
        "--no-playlist",
        "--no-simulate",
        // 檔名一律照 Windows 規則產生，兩個平台的產出才能互通
        "--windows-filenames",
        "--trim-filenames",
        "150",
        "--progress-template",
        "download:HAULPROG %(progress.downloaded_bytes)s %(progress.total_bytes)s %(progress.total_bytes_estimate)s",
        "--print",
        "after_move:HAULFILE %(filepath)s",
        // 時長由 yt-dlp 給，它本來就知道；影片抽樣驗證需要可靠的取樣點，
        // 而我們沒有 ffprobe 可以問
        "--print",
        "after_move:HAULDUR %(duration)s",
        "-o",
        "%(title)s.%(ext)s",
    ]);
    cmd.arg("--ffmpeg-location").arg(&tools.ffmpeg);
    cmd.arg("--paths")
        .arg(format!("home:{}", staging.display()));

    match mode {
        // bv*+ba 拿分離的最佳視訊與音訊再合併，b 是已經合併好的後備
        Mode::Video => cmd.args(["-f", "bv*+ba/b", "--merge-output-format", "mp4"]),
        // --audio-format best 表示能直接複製就不要重新編碼
        Mode::Audio => cmd.args(["-f", "ba/b", "-x", "--audio-format", "best"]),
    };

    cmd.arg(url)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let mut child = cmd.spawn().map_err(|e| anyhow!("啟動 yt-dlp 失敗：{e}"))?;

    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| anyhow!("接不到 stdout"))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| anyhow!("接不到 stderr"))?;

    // 兩個串流都要同時抽乾，只讀一邊會在管線塞滿時把子程序卡死
    let (tx, mut rx) = tokio::sync::mpsc::channel::<(bool, String)>(256);
    let tx2 = tx.clone();
    tokio::spawn(async move {
        let mut lines = BufReader::new(stdout).lines();
        while let Ok(Some(l)) = lines.next_line().await {
            if tx.send((false, l)).await.is_err() {
                break;
            }
        }
    });
    tokio::spawn(async move {
        let mut lines = BufReader::new(stderr).lines();
        while let Ok(Some(l)) = lines.next_line().await {
            if tx2.send((true, l)).await.is_err() {
                break;
            }
        }
    });

    let mut file: Option<PathBuf> = None;
    let mut secs: Option<f64> = None;
    let mut errs: Vec<String> = Vec::new();

    while let Some((is_err, line)) = rx.recv().await {
        if let Some(rest) = line
            .find(MARK_PROGRESS)
            .map(|i| &line[i + MARK_PROGRESS.len()..])
        {
            let mut it = rest.split_whitespace();
            let got = it.next().and_then(|s| s.parse::<u64>().ok()).unwrap_or(0);
            let total = it
                .next()
                .and_then(|s| s.parse::<u64>().ok())
                // total_bytes 未知時退回估計值
                .or_else(|| it.next().and_then(|s| s.parse::<u64>().ok()))
                .unwrap_or(0);
            on_progress(got, total);
            continue;
        }
        if let Some(rest) = line.find(MARK_FILE).map(|i| &line[i + MARK_FILE.len()..]) {
            let p = PathBuf::from(rest.trim());
            if !rest.trim().is_empty() {
                file = Some(p);
            }
            continue;
        }
        if let Some(rest) = line.find(MARK_DUR).map(|i| &line[i + MARK_DUR.len()..]) {
            // 沒有時長的來源（例如直播片段）yt-dlp 會印 NA
            secs = rest.trim().parse::<f64>().ok().filter(|d| *d > 0.0);
            continue;
        }
        if is_err {
            errs.push(line);
            if errs.len() > STDERR_TAIL {
                errs.remove(0);
            }
        }
    }

    let status = child.wait().await?;
    if !status.success() {
        bail!("{}", stderr_tail(&errs));
    }

    let path = file.ok_or_else(|| anyhow!("yt-dlp 沒有回報輸出檔案：{}", stderr_tail(&errs)))?;
    if !path.exists() {
        bail!("yt-dlp 回報的檔案不存在：{}", path.display());
    }
    Ok(Downloaded { path, secs })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mode_parsing() {
        assert_eq!(Mode::parse("audio"), Mode::Audio);
        assert_eq!(Mode::parse("video"), Mode::Video);
        // 認不得的一律當影片，不要因為前端傳錯字就整個壞掉
        assert_eq!(Mode::parse("nonsense"), Mode::Video);
    }

    #[test]
    fn stderr_tail_picks_last_meaningful_lines() {
        let lines: Vec<String> = vec!["a".into(), "".into(), "b".into(), "c".into()];
        let got = stderr_tail(&lines);
        assert!(got.contains('c') && got.contains('b'));
    }

    #[test]
    fn stderr_tail_handles_silence() {
        assert!(stderr_tail(&[]).contains("沒有給出原因"));
    }
}
