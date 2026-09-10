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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_non_audio() {
        let dir = std::env::temp_dir().join("sunodl-test");
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("garbage.mp3");
        std::fs::write(&p, b"this is definitely not an mp3 file, not even close").unwrap();
        assert!(verify(&p).is_err());
        let _ = std::fs::remove_file(&p);
    }

    /// 只有負向測試的閘門，寫成「什麼都拒絕」也會全綠。
    /// 指一個真實音檔給 SUNODL_TEST_MP3 就會跑這關；CI 上沒設就跳過。
    #[test]
    fn accepts_real_audio() {
        let Some(p) = std::env::var_os("SUNODL_TEST_MP3") else {
            return;
        };
        let v = verify(Path::new(&p)).expect("真實音檔應該通過驗證");
        assert!(v.secs > 1.0, "解出來的長度不合理：{}", v.secs);
    }

    #[test]
    fn rejects_empty() {
        let dir = std::env::temp_dir().join("sunodl-test");
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("empty.mp3");
        std::fs::write(&p, b"").unwrap();
        assert!(verify(&p).is_err());
        let _ = std::fs::remove_file(&p);
    }
}
