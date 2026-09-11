//! 完成項目的縮圖。列表要認得出誰是誰，靠的就是這張 96×96。
//!
//! 由引擎在驗證通過後產生，存在輸出資料夾的 `.haul-thumbs/`——跟歷史檔
//! 同一個地方，換資料夾縮圖跟著走。產不出來不是錯誤：項目照樣完成，
//! 只是列上顯示占位圖示。

use anyhow::{anyhow, Result};
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};

/// 邊長。Retina 顯示成 48px。
pub const SIZE: u32 = 96;

pub const DIR: &str = ".haul-thumbs";

/// 縮圖的存放處：檔名取自完整路徑的 hash，同一個檔永遠對到同一張，
/// 所以「有沒有產過」直接看檔案在不在，不必另外記。
pub fn path_for(out_dir: &Path, media: &Path) -> PathBuf {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    media.hash(&mut h);
    out_dir.join(DIR).join(format!("{:016x}.jpg", h.finish()))
}

/// 產一張正方形 JPEG。`seek` 是影片要抽的秒數；音樂與圖片給 None。
///
/// 三種型別走同一條 ffmpeg 命令：`-map 0:v:0 -frames:v 1` 對影片是抽一格，
/// 對音樂是抽內嵌封面（ffmpeg 把 APIC / covr 當 attached_pic 視訊流），
/// 對圖片就是圖片本身。沒有視訊流（無封面的音樂）ffmpeg 會失敗，回 None。
pub async fn make(
    ffmpeg: &Path,
    media: &Path,
    seek: Option<f64>,
    dest: &Path,
) -> Result<Option<PathBuf>> {
    if let Some(parent) = dest.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    let mut cmd = tokio::process::Command::new(ffmpeg);
    cmd.args(["-v", "error", "-nostdin", "-y"]);
    if let Some(t) = seek {
        cmd.args(["-ss", &format!("{t:.2}")]);
    }
    let vf = format!("scale={SIZE}:{SIZE}:force_original_aspect_ratio=increase,crop={SIZE}:{SIZE}");
    cmd.arg("-i")
        .arg(media)
        .args(["-map", "0:v:0", "-frames:v", "1", "-vf", &vf, "-q:v", "4"])
        .arg(dest)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    let status = cmd
        .status()
        .await
        .map_err(|e| anyhow!("執行 ffmpeg 失敗：{e}"))?;

    let ok = status.success()
        && tokio::fs::metadata(dest)
            .await
            .map(|m| m.len() > 0)
            .unwrap_or(false);
    if !ok {
        let _ = tokio::fs::remove_file(dest).await;
        return Ok(None);
    }
    Ok(Some(dest.to_path_buf()))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 找一支系統上的 ffmpeg 來當測試素材，找不到就讓呼叫端跳過
    fn test_ffmpeg() -> Option<PathBuf> {
        [
            "/opt/homebrew/bin/ffmpeg",
            "/usr/local/bin/ffmpeg",
            "/usr/bin/ffmpeg",
        ]
        .into_iter()
        .map(PathBuf::from)
        .find(|p| p.exists())
    }

    fn dir() -> PathBuf {
        let d = std::env::temp_dir().join("haul-thumb-test");
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// lavfi 合成：影片、純音訊（可帶封面）、圖片
    fn synth(ffmpeg: &Path, name: &str, args: &[&str]) -> PathBuf {
        let out = dir().join(name);
        let ok = std::process::Command::new(ffmpeg)
            .args(["-v", "error", "-y"])
            .args(args)
            .arg(&out)
            .status()
            .unwrap()
            .success();
        assert!(ok, "合成 {name} 失敗");
        out
    }

    #[test]
    fn thumb_path_is_stable_and_lives_in_the_hidden_folder() {
        let out = Path::new("/out");
        let a = path_for(out, Path::new("/out/a.mp4"));
        assert_eq!(a, path_for(out, Path::new("/out/a.mp4")));
        assert!(a.starts_with("/out/.haul-thumbs"));
        assert_eq!(a.extension().unwrap(), "jpg");
        assert_ne!(a, path_for(out, Path::new("/out/b.mp4")));
    }

    #[tokio::test]
    async fn video_gets_a_square_frame() {
        let Some(ff) = test_ffmpeg() else { return };
        let v = synth(
            &ff,
            "v.mp4",
            &[
                "-f",
                "lavfi",
                "-i",
                "testsrc=duration=3:size=320x180:rate=10",
                "-c:v",
                "mpeg4",
            ],
        );
        let dest = dir().join("v.jpg");
        let got = make(&ff, &v, Some(0.5), &dest).await.unwrap();
        assert_eq!(got.as_deref(), Some(dest.as_path()));
        assert!(std::fs::metadata(&dest).unwrap().len() > 0);
    }

    #[tokio::test]
    async fn audio_without_cover_yields_none_not_error() {
        let Some(ff) = test_ffmpeg() else { return };
        let a = synth(
            &ff,
            "a.m4a",
            &[
                "-f",
                "lavfi",
                "-i",
                "sine=frequency=440:duration=2",
                "-c:a",
                "aac",
            ],
        );
        let dest = dir().join("a.jpg");
        let _ = std::fs::remove_file(&dest);
        assert!(make(&ff, &a, None, &dest).await.unwrap().is_none());
        assert!(!dest.exists(), "失敗時不該留半截檔");
    }

    #[tokio::test]
    async fn audio_with_cover_gets_the_cover() {
        let Some(ff) = test_ffmpeg() else { return };
        let cover = synth(
            &ff,
            "cover.png",
            &[
                "-f",
                "lavfi",
                "-i",
                "color=c=red:size=64x64",
                "-frames:v",
                "1",
            ],
        );
        // 封面在 ffmpeg 眼裡是 attached_pic 視訊流，mp4 與 mp3 都一樣
        let m4a = synth(
            &ff,
            "c.m4a",
            &[
                "-f",
                "lavfi",
                "-i",
                "sine=frequency=440:duration=2",
                "-i",
                cover.to_str().unwrap(),
                "-map",
                "0:a",
                "-map",
                "1:v",
                "-c:a",
                "aac",
                "-c:v",
                "copy",
                "-disposition:v",
                "attached_pic",
            ],
        );
        let dest = dir().join("c.jpg");
        assert!(make(&ff, &m4a, None, &dest).await.unwrap().is_some());
    }

    #[tokio::test]
    async fn image_is_downscaled() {
        let Some(ff) = test_ffmpeg() else { return };
        let img = synth(
            &ff,
            "i.png",
            &[
                "-f",
                "lavfi",
                "-i",
                "color=c=blue:size=400x300",
                "-frames:v",
                "1",
            ],
        );
        let dest = dir().join("i.jpg");
        assert!(make(&ff, &img, None, &dest).await.unwrap().is_some());
    }
}
