//! 「抓下來的一定要能播」的閘門。
//!
//! 用 symphonia 在程序內完整解碼一遍，不呼叫外部程式、不需要使用者裝 ffmpeg。
//! 解碼是逐 packet 進行的，記憶體用量與檔案長度無關。

use anyhow::{anyhow, bail, Result};
use std::path::Path;
use symphonia::core::audio::SampleBuffer;
use symphonia::core::codecs::{DecoderOptions, CODEC_TYPE_NULL};
use symphonia::core::errors::Error as SymError;
use symphonia::core::formats::FormatOptions;
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::MetadataOptions;
use symphonia::core::probe::Hint;

pub struct Verified {
    pub secs: f64,
}

/// 無聲判定門檻。正常音樂的 RMS 大約在 -20 ~ -8 dB，
/// -70 dB 以下等於整首都是數位靜音（DRM 空殼或抓錯東西）。
const SILENCE_DB: f64 = -70.0;

/// 允許少量解碼錯誤：有些 mp3 的第一個 frame 是不完整的，
/// 這在正常播放器上也會被跳過，不該判定成壞檔。
const MAX_DECODE_ERRORS: u32 = 8;

/// 這是 CPU 密集的同步函式，呼叫端請丟到 blocking 執行緒。
pub fn verify(path: &Path) -> Result<Verified> {
    let file = std::fs::File::open(path)?;
    let mss = MediaSourceStream::new(Box::new(file), Default::default());

    let mut hint = Hint::new();
    if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
        hint.with_extension(ext);
    }

    let probed = symphonia::default::get_probe()
        .format(
            &hint,
            mss,
            &FormatOptions::default(),
            &MetadataOptions::default(),
        )
        .map_err(|e| anyhow!("認不出音訊格式：{e}"))?;
    let mut format = probed.format;

    // 影音混合的 mp4 裡第 0 軌通常是視訊，default_track() 會挑到它。
    // 這裡明確找第一個帶取樣率的可解碼音訊軌。
    let (track_id, sample_rate, params) = {
        let t = format
            .tracks()
            .iter()
            .find(|t| {
                t.codec_params.codec != CODEC_TYPE_NULL && t.codec_params.sample_rate.is_some()
            })
            .ok_or_else(|| anyhow!("檔案裡沒有可解碼的音訊軌"))?;
        (
            t.id,
            t.codec_params.sample_rate.unwrap_or(0),
            t.codec_params.clone(),
        )
    };
    if sample_rate == 0 {
        bail!("取樣率不明");
    }

    let mut decoder = symphonia::default::get_codecs()
        .make(&params, &DecoderOptions::default())
        .map_err(|e| anyhow!("沒有對應的解碼器：{e}"))?;

    let mut frames: u64 = 0;
    let mut sum_sq: f64 = 0.0;
    let mut samples: u64 = 0;
    let mut decode_errors: u32 = 0;

    // 取樣緩衝配置一次重複使用。一首歌有幾千個 packet，
    // 每個 packet 都重新配置的話光是配置就吃掉大半時間。
    let mut buf: Option<SampleBuffer<f32>> = None;
    let mut buf_cap: u64 = 0;
    let mut buf_spec: Option<symphonia::core::audio::SignalSpec> = None;

    loop {
        let packet = match format.next_packet() {
            Ok(p) => p,
            // 讀到檔尾是正常結束
            Err(SymError::IoError(e)) if e.kind() == std::io::ErrorKind::UnexpectedEof => break,
            Err(SymError::ResetRequired) => break,
            Err(e) => bail!("讀取失敗：{e}"),
        };
        if packet.track_id() != track_id {
            continue;
        }

        match decoder.decode(&packet) {
            Ok(decoded) => {
                let spec = *decoded.spec();
                let channels = spec.channels.count().max(1);
                let cap = decoded.capacity() as u64;

                // 只在第一個 packet、或遇到更大的 packet／換了規格時才重新配置
                if buf.is_none() || cap > buf_cap || buf_spec != Some(spec) {
                    buf = Some(SampleBuffer::<f32>::new(cap, spec));
                    buf_cap = cap;
                    buf_spec = Some(spec);
                }
                let sb = buf.as_mut().expect("剛才才配置過");

                sb.copy_interleaved_ref(decoded);
                let s = sb.samples();
                for &v in s {
                    sum_sq += (v as f64) * (v as f64);
                }
                samples += s.len() as u64;
                frames += (s.len() / channels) as u64;
            }
            Err(SymError::DecodeError(msg)) => {
                decode_errors += 1;
                if decode_errors > MAX_DECODE_ERRORS {
                    bail!("解碼錯誤過多（最後一筆：{msg}）");
                }
            }
            Err(e) => bail!("解碼失敗：{e}"),
        }
    }

    // 閘門二：真的解出東西，而且長度合理
    if samples == 0 {
        bail!("完全解不出音訊資料");
    }
    let secs = frames as f64 / sample_rate as f64;
    if secs < 1.0 {
        bail!("長度只有 {secs:.2} 秒");
    }

    // 閘門三：不是整首無聲
    let rms = (sum_sq / samples as f64).sqrt();
    let rms_db = if rms > 0.0 {
        20.0 * rms.log10()
    } else {
        f64::NEG_INFINITY
    };
    if rms_db < SILENCE_DB {
        bail!("整首無聲（RMS {rms_db:.1} dB）");
    }

    Ok(Verified { secs })
}

/// 從 ffmpeg 的 volumedetect 輸出裡取出 mean_volume（dB）。
fn parse_mean_volume(stderr: &str) -> Option<f64> {
    let at = stderr.find("mean_volume:")? + "mean_volume:".len();
    let rest = stderr[at..].trim_start();
    let num: String = rest
        .chars()
        .take_while(|c| {
            c.is_ascii_digit() || *c == '-' || *c == '.' || *c == 'i' || *c == 'n' || *c == 'f'
        })
        .collect();
    if num.contains("inf") {
        return Some(f64::NEG_INFINITY);
    }
    num.parse::<f64>().ok()
}

/// symphonia 不認識的編碼交給 ffmpeg 裁決。
///
/// 只在 symphonia 失敗後才走這裡。symphonia 沒有 opus 解碼器，而 YouTube
/// 的音訊常常是 webm/opus —— 若不做這層退路，完好的檔案會被判成壞檔，
/// 那比不檢查還糟。ffmpeg 過得了就代表檔案沒問題。
pub async fn verify_audio_with_ffmpeg(ffmpeg: &Path, file: &Path) -> Result<()> {
    // 刻意用預設的 log 等級：-v error 會把 volumedetect 的統計一起壓掉，
    // 所以這裡改用離開碼判損毀、用 mean_volume 判無聲，不倚賴 stderr 是否為空。
    let out = tokio::process::Command::new(ffmpeg)
        .args(["-nostdin", "-i"])
        .arg(file)
        .args(["-map", "0:a:0", "-af", "volumedetect", "-f", "null", "-"])
        .stdin(std::process::Stdio::null())
        .output()
        .await
        .map_err(|e| anyhow!("執行 ffmpeg 失敗：{e}"))?;

    let msg = String::from_utf8_lossy(&out.stderr);

    if !out.status.success() {
        let last = msg
            .lines()
            .rev()
            .find(|l| !l.trim().is_empty())
            .unwrap_or("")
            .trim();
        bail!("ffmpeg 解不開這個音訊：{last}");
    }

    match parse_mean_volume(&msg) {
        Some(db) if db < SILENCE_DB => bail!("整首無聲（mean_volume {db:.1} dB）"),
        // 取不到就不判定 —— 寧可漏一個無聲檔，也不要誤殺好檔
        _ => Ok(()),
    }
}

/// 影片要抽樣的時間點。開頭附近、中間、接近結尾——
/// 截斷的檔案一定會在最後那個點爆掉。
fn sample_points(secs: f64) -> Vec<f64> {
    if !secs.is_finite() || secs <= 0.0 {
        return vec![0.0];
    }
    if secs < 5.0 {
        return vec![0.0];
    }
    vec![secs * 0.02, secs * 0.5, secs * 0.92]
}

/// 影片抽樣驗證：確認有視訊軌，且在幾個時間點都解得出畫面。
///
/// 完整解一部 1080p 影片要花掉幾十秒 CPU，抽樣把成本壓到一秒內，
/// 又足以抓到截斷與中段損毀。判斷只看離開碼與 stderr 是否為空，
/// 不去解析 ffmpeg 的人類可讀輸出（那個格式會隨版本改）。
pub async fn verify_video(ffmpeg: &Path, file: &Path, secs: f64) -> Result<()> {
    for t in sample_points(secs) {
        let out = tokio::process::Command::new(ffmpeg)
            .args(["-v", "error", "-nostdin", "-ss", &format!("{t:.2}"), "-i"])
            .arg(file)
            // -map 0:v:0 在沒有視訊軌時會直接失敗，正好也當成「有沒有畫面」的檢查
            .args(["-map", "0:v:0", "-frames:v", "8", "-an", "-f", "null", "-"])
            .stdin(std::process::Stdio::null())
            .output()
            .await
            .map_err(|e| anyhow!("執行 ffmpeg 失敗：{e}"))?;

        if !out.status.success() {
            let msg = String::from_utf8_lossy(&out.stderr);
            let first = msg.lines().find(|l| !l.trim().is_empty()).unwrap_or("");
            bail!("第 {t:.0} 秒解不出畫面：{first}");
        }
        let msg = String::from_utf8_lossy(&out.stderr);
        if !msg.trim().is_empty() {
            let first = msg.lines().find(|l| !l.trim().is_empty()).unwrap_or("");
            bail!("第 {t:.0} 秒畫面有問題：{first}");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_mean_volume_from_ffmpeg_output() {
        let s = "[Parsed_volumedetect_0 @ 0x7f] n_samples: 123\n\
                 [Parsed_volumedetect_0 @ 0x7f] mean_volume: -18.4 dB\n\
                 [Parsed_volumedetect_0 @ 0x7f] max_volume: -0.9 dB";
        assert_eq!(parse_mean_volume(s), Some(-18.4));
    }

    #[test]
    fn parses_digital_silence_as_negative_infinity() {
        let s = "[Parsed_volumedetect_0 @ 0x7f] mean_volume: -inf dB";
        assert_eq!(parse_mean_volume(s), Some(f64::NEG_INFINITY));
    }

    #[test]
    fn missing_mean_volume_is_none_not_zero() {
        // 取不到要回 None（不判定），回 Some(0.0) 會變成「很大聲」的誤判
        assert_eq!(parse_mean_volume("nothing useful here"), None);
    }

    #[test]
    fn sample_points_cover_start_middle_end() {
        let p = sample_points(200.0);
        assert_eq!(p.len(), 3);
        assert!(p[0] < 10.0, "第一個點要靠近開頭");
        assert!(p[2] > 180.0, "最後一個點要夠接近結尾才抓得到截斷");
    }

    #[test]
    fn sample_points_handle_short_and_bogus_durations() {
        assert_eq!(sample_points(2.0), vec![0.0]);
        assert_eq!(sample_points(0.0), vec![0.0]);
        assert_eq!(sample_points(f64::NAN), vec![0.0]);
    }

    #[test]
    fn rejects_non_audio() {
        let dir = std::env::temp_dir().join("haul-test");
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("garbage.mp3");
        std::fs::write(&p, b"this is definitely not an mp3 file, not even close").unwrap();
        assert!(verify(&p).is_err());
        let _ = std::fs::remove_file(&p);
    }

    /// 只有負向測試的閘門，寫成「什麼都拒絕」也會全綠。
    /// 指一個真實音檔給 HAUL_TEST_MEDIA 就會跑這關；CI 上沒設就跳過。
    #[test]
    fn accepts_real_audio() {
        let Some(p) = std::env::var_os("HAUL_TEST_MEDIA") else {
            return;
        };
        let v = verify(Path::new(&p)).expect("真實音檔應該通過驗證");
        assert!(v.secs > 1.0, "解出來的長度不合理：{}", v.secs);
    }

    #[test]
    fn rejects_empty() {
        let dir = std::env::temp_dir().join("haul-test");
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("empty.mp3");
        std::fs::write(&p, b"").unwrap();
        assert!(verify(&p).is_err());
        let _ = std::fs::remove_file(&p);
    }
}
