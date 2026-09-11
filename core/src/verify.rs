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

/// ffmpeg 對音軌的裁決。
///
/// 「沒有音軌」與「音軌壞了」必須分開：影片可以沒有聲音（Facebook 的
/// 無聲 reel 就是真實案例），但有聲音就一定要解得開。要不要接受
/// `Absent` 由呼叫端依模式決定——只要聲音的模式沒有音軌就是失敗。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Audio {
    /// 有音軌，解得開，而且不是整首無聲
    Decoded,
    /// ffmpeg 在容器裡找不到音軌
    Absent,
}

/// symphonia 不認識的編碼交給 ffmpeg 裁決。
///
/// 只在 symphonia 失敗後才走這裡。symphonia 沒有 opus 解碼器，而 YouTube
/// 的音訊常常是 webm/opus —— 若不做這層退路，完好的檔案會被判成壞檔，
/// 那比不檢查還糟。ffmpeg 過得了就代表檔案沒問題。
pub async fn verify_audio_with_ffmpeg(ffmpeg: &Path, file: &Path) -> Result<Audio> {
    // 刻意用預設的 log 等級：-v error 會把 volumedetect 的統計一起壓掉，
    // 所以這裡改用離開碼判損毀、用 mean_volume 判無聲，不倚賴 stderr 是否為空。
    let out = tokio::process::Command::new(ffmpeg)
        .kill_on_drop(true)
        .args(["-nostdin", "-i"])
        .arg(file)
        .args(["-map", "0:a:0", "-af", "volumedetect", "-f", "null", "-"])
        .stdin(std::process::Stdio::null())
        .output()
        .await
        .map_err(|e| anyhow!("執行 ffmpeg 失敗：{e}"))?;

    let msg = String::from_utf8_lossy(&out.stderr);

    if !out.status.success() {
        // -map 0:a:0 在沒有音軌時也會失敗，離開碼跟解碼失敗長得一樣。
        // 只 copy 不解碼地再問兩次：容器裡有音軌嗎？容器本身打得開嗎？
        // 打得開卻沒音軌才是 Absent，其餘一律回報原本的解碼錯誤。
        if !stream_copies(ffmpeg, file, "0:a:0").await? && stream_copies(ffmpeg, file, "0").await? {
            return Ok(Audio::Absent);
        }
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
        _ => Ok(Audio::Decoded),
    }
}

/// `-map` 指定的流能不能原樣 copy 出來。只 copy 不解碼，所以壞掉的音軌
/// 也會回「能」——這正是要的：它把「沒有」跟「壞了」分開。離開碼就是答案，
/// 不去解析「Stream map '0:a:0' matches no streams」這種人類看的訊息。
async fn stream_copies(ffmpeg: &Path, file: &Path, map: &str) -> Result<bool> {
    let status = tokio::process::Command::new(ffmpeg)
        .kill_on_drop(true)
        .args(["-v", "error", "-nostdin", "-i"])
        .arg(file)
        .args(["-map", map, "-c", "copy", "-t", "0.01", "-f", "null", "-"])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .await
        .map_err(|e| anyhow!("執行 ffmpeg 失敗：{e}"))?;
    Ok(status.success())
}

/// 一個檔案通過了哪一級檢查。
///
/// 存在的理由是誠實：不同型別能做到的驗證強度天差地遠，把它們都
/// 報成「成功」會讓呼叫端無從判斷到底檢查了多少。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Level {
    /// 解碼過的音訊或影片
    Media,
    /// 解碼過的圖片
    Image,
    /// 真的 parse 過的 JSON
    Json,
    /// 容器結構完整（PDF / ZIP 的 magic 與結尾簽章）
    Archive,
    /// 合法 UTF-8 且非空
    Text,
    /// 只確認位元組數與 Content-Length 相符
    Integrity,
}

impl Level {
    pub fn as_str(self) -> &'static str {
        match self {
            Level::Media => "media",
            Level::Image => "image",
            Level::Json => "json",
            Level::Archive => "archive",
            Level::Text => "text",
            Level::Integrity => "integrity",
        }
    }
}

/// 依副檔名判斷該用哪一級檢查。呼叫端若有 Content-Type 應優先使用它。
pub fn level_for(ext: &str) -> Level {
    match ext.to_ascii_lowercase().as_str() {
        "mp4" | "m4a" | "mp3" | "webm" | "mkv" | "mov" | "avi" | "flv" | "ts" | "wav" | "flac"
        | "opus" | "ogg" | "aac" | "m4v" | "wma" => Level::Media,
        "jpg" | "jpeg" | "png" | "gif" | "webp" | "bmp" | "tif" | "tiff" | "avif" | "heic" => {
            Level::Image
        }
        "json" | "jsonl" | "ndjson" => Level::Json,
        "pdf" | "zip" | "epub" | "cbz" | "docx" | "xlsx" | "pptx" => Level::Archive,
        "txt" | "md" | "csv" | "srt" | "vtt" | "ass" | "lrc" | "html" | "htm" | "xml" => {
            Level::Text
        }
        _ => Level::Integrity,
    }
}

/// Content-Type 比副檔名可靠，有就用它
pub fn level_for_content_type(ct: &str) -> Option<Level> {
    let ct = ct
        .split(';')
        .next()
        .unwrap_or(ct)
        .trim()
        .to_ascii_lowercase();
    Some(match ct.as_str() {
        t if t.starts_with("audio/") || t.starts_with("video/") => Level::Media,
        t if t.starts_with("image/") => Level::Image,
        "application/json" | "application/x-ndjson" => Level::Json,
        "application/pdf" | "application/zip" | "application/epub+zip" => Level::Archive,
        t if t.starts_with("text/") => Level::Text,
        _ => return None,
    })
}

/// 圖片：交給 ffmpeg 解一張出來。已經有這支工具，不必為此多背一個影像函式庫。
pub async fn verify_image(ffmpeg: &Path, file: &Path) -> Result<()> {
    let out = tokio::process::Command::new(ffmpeg)
        .kill_on_drop(true)
        .args(["-v", "error", "-nostdin", "-i"])
        .arg(file)
        .args(["-frames:v", "1", "-f", "null", "-"])
        .stdin(std::process::Stdio::null())
        .output()
        .await
        .map_err(|e| anyhow!("執行 ffmpeg 失敗：{e}"))?;

    let msg = String::from_utf8_lossy(&out.stderr);
    if !out.status.success() {
        let first = decode_complaints(&msg)
            .first()
            .copied()
            .unwrap_or("ffmpeg 未說明原因");
        bail!("這不是一張解得開的圖片：{first}");
    }
    if let Some(first) = decode_complaints(&msg).first() {
        bail!("圖片有問題：{first}");
    }
    Ok(())
}

/// JSON：真的 parse 一遍。這比「非空」強得多——半截的回應會被抓出來。
pub fn verify_json(file: &Path) -> Result<()> {
    let text = std::fs::read_to_string(file).map_err(|e| anyhow!("讀不到檔案：{e}"))?;
    if text.trim().is_empty() {
        bail!("檔案是空的");
    }
    // NDJSON 每行各自是一份 JSON，整份 parse 會失敗，所以兩種都試
    if serde_json::from_str::<serde_json::Value>(&text).is_ok() {
        return Ok(());
    }
    let mut lines = 0;
    for line in text.lines().filter(|l| !l.trim().is_empty()) {
        serde_json::from_str::<serde_json::Value>(line)
            .map_err(|e| anyhow!("不是合法的 JSON 或 NDJSON：{e}"))?;
        lines += 1;
    }
    if lines == 0 {
        bail!("沒有任何一行是 JSON");
    }
    Ok(())
}

/// PDF / ZIP：檢查開頭的 magic 與結尾的簽章。
///
/// 不為此引進 PDF 或 ZIP 函式庫——這兩個簽章足以抓到截斷，
/// 而截斷正是下載最常見的損壞方式。
pub fn verify_archive(file: &Path) -> Result<()> {
    let bytes = std::fs::read(file).map_err(|e| anyhow!("讀不到檔案：{e}"))?;
    if bytes.len() < 32 {
        bail!("檔案只有 {} bytes，不像完整的檔案", bytes.len());
    }
    let tail_from = bytes.len().saturating_sub(2048);
    let tail = &bytes[tail_from..];

    if bytes.starts_with(b"%PDF-") {
        if !contains(tail, b"%%EOF") {
            bail!("PDF 結尾缺少 %%EOF，檔案應該是截斷的");
        }
        return Ok(());
    }
    if bytes.starts_with(b"PK\x03\x04") || bytes.starts_with(b"PK\x05\x06") {
        // 中央目錄結尾記錄。沒有它就代表 zip 沒寫完。
        if !contains(tail, b"PK\x05\x06") {
            bail!("ZIP 缺少中央目錄結尾記錄，檔案應該是截斷的");
        }
        return Ok(());
    }
    bail!("開頭的位元組不像 PDF 或 ZIP")
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|w| w == needle)
}

/// 純文字：合法 UTF-8 且非空白。這是文字唯一能保證的東西——
/// 「內容有沒有意義」不是下載器判斷得了的。
pub fn verify_text(file: &Path) -> Result<()> {
    let bytes = std::fs::read(file).map_err(|e| anyhow!("讀不到檔案：{e}"))?;
    if bytes.is_empty() {
        bail!("檔案是空的");
    }
    let text = std::str::from_utf8(&bytes).map_err(|e| anyhow!("不是合法的 UTF-8：{e}"))?;
    if text.trim().is_empty() {
        bail!("檔案只有空白字元");
    }
    Ok(())
}

/// 從 ffmpeg 的 stderr 裡挑出「真正跟解碼有關」的抱怨。
///
/// 用 `-ss` 跳進串流中段時，輸出端的 null muxer 會抗議時間戳重疊
/// （`non monotonically increasing dts to muxer`），但畫面本身完全正常。
/// 我們是在解碼不是在封裝，muxer 的意見按定義就與這件事無關，所以整路濾掉。
///
/// 這不是良性訊息白名單——那種清單會一直漏。這是依訊息來源分類：
/// `[null @ …]` 一定是輸出 muxer，其餘（demuxer、解碼器）都保留。
fn decode_complaints(stderr: &str) -> Vec<&str> {
    stderr
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .filter(|l| !l.starts_with("[null @"))
        .collect()
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
            .kill_on_drop(true)
            .args(["-v", "error", "-nostdin", "-ss", &format!("{t:.2}"), "-i"])
            .arg(file)
            // -map 0:v:0 在沒有視訊軌時會直接失敗，正好也當成「有沒有畫面」的檢查
            .args(["-map", "0:v:0", "-frames:v", "8", "-an", "-f", "null", "-"])
            .stdin(std::process::Stdio::null())
            .output()
            .await
            .map_err(|e| anyhow!("執行 ffmpeg 失敗：{e}"))?;

        let msg = String::from_utf8_lossy(&out.stderr);

        if !out.status.success() {
            let first = decode_complaints(&msg)
                .first()
                .copied()
                .unwrap_or("ffmpeg 未說明原因");
            bail!("第 {t:.0} 秒解不出畫面：{first}");
        }

        // 離開碼是主要訊號，但解碼器有時會記錯誤卻仍以 0 結束，
        // 所以額外看有沒有真正的解碼抱怨
        if let Some(first) = decode_complaints(&msg).first() {
            bail!("第 {t:.0} 秒畫面有問題：{first}");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str, bytes: &[u8]) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join("haul-verify-test");
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join(name);
        std::fs::write(&p, bytes).unwrap();
        p
    }

    #[test]
    fn content_type_wins_over_extension_when_available() {
        assert_eq!(level_for_content_type("image/png"), Some(Level::Image));
        assert_eq!(
            level_for_content_type("application/json; charset=utf-8"),
            Some(Level::Json)
        );
        assert_eq!(level_for_content_type("video/mp4"), Some(Level::Media));
        // 認不得就交還給呼叫端，由副檔名決定
        assert_eq!(level_for_content_type("application/octet-stream"), None);
    }

    #[test]
    fn extension_maps_to_the_strongest_available_check() {
        assert_eq!(level_for("mp4"), Level::Media);
        assert_eq!(level_for("JPG"), Level::Image);
        assert_eq!(level_for("json"), Level::Json);
        assert_eq!(level_for("pdf"), Level::Archive);
        assert_eq!(level_for("txt"), Level::Text);
        // 不認得的一律只驗完整性，而不是假裝驗過內容
        assert_eq!(level_for("bin"), Level::Integrity);
    }

    #[test]
    fn json_gate_accepts_both_json_and_ndjson() {
        assert!(verify_json(&tmp("a.json", br#"{"a":1}"#)).is_ok());
        assert!(verify_json(&tmp("b.jsonl", b"{\"a\":1}\n{\"b\":2}\n")).is_ok());
    }

    #[test]
    fn json_gate_catches_truncation() {
        // 半截的回應是下載最常見的損壞方式，光看「非空」抓不到
        assert!(verify_json(&tmp("c.json", br#"{"a":1"#)).is_err());
        assert!(verify_json(&tmp("d.json", b"")).is_err());
    }

    #[test]
    fn archive_gate_catches_truncated_pdf_and_zip() {
        let mut pdf = b"%PDF-1.7\n".to_vec();
        pdf.extend(std::iter::repeat_n(b'x', 100));
        assert!(
            verify_archive(&tmp("truncated.pdf", &pdf)).is_err(),
            "缺少 %%EOF 應該判定為截斷"
        );

        pdf.extend(b"\n%%EOF\n");
        assert!(verify_archive(&tmp("whole.pdf", &pdf)).is_ok());

        let mut zip = b"PK\x03\x04".to_vec();
        zip.extend(std::iter::repeat_n(0u8, 100));
        assert!(
            verify_archive(&tmp("truncated.zip", &zip)).is_err(),
            "缺少中央目錄結尾記錄應該判定為截斷"
        );

        zip.extend(b"PK\x05\x06");
        zip.extend(std::iter::repeat_n(0u8, 18));
        assert!(verify_archive(&tmp("whole.zip", &zip)).is_ok());
    }

    #[test]
    fn archive_gate_rejects_things_that_are_not_archives() {
        let junk = vec![b'z'; 64];
        assert!(verify_archive(&tmp("not.pdf", &junk)).is_err());
    }

    #[test]
    fn text_gate_rejects_empty_blank_and_invalid_utf8() {
        assert!(verify_text(&tmp("ok.txt", "內容".as_bytes())).is_ok());
        assert!(verify_text(&tmp("empty.txt", b"")).is_err());
        assert!(verify_text(&tmp("blank.txt", b"   \n\t ")).is_err());
        // 0xFF 在 UTF-8 裡永遠不合法
        assert!(verify_text(&tmp("bad.txt", &[0xFF, 0xFE, 0x00])).is_err());
    }

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

    /// 真實案例：一支 AV1 1080p 的 B 站影片完全正常，但用 -ss 跳到中段抽樣時
    /// null muxer 會抱怨時間戳重疊。舊版把「stderr 非空」當成失敗，於是好檔案
    /// 被判定成壞檔 —— 這種假陰性比不檢查還糟。
    #[test]
    fn muxer_timestamp_complaints_are_not_decode_errors() {
        let s = "[null @ 0x12c805720] Application provided invalid, non monotonically \
                 increasing dts to muxer in stream 0: 2 >= 2";
        assert!(decode_complaints(s).is_empty());
    }

    #[test]
    fn real_decoder_and_demuxer_errors_survive_the_filter() {
        let s = "[mov,mp4,m4a,3gp,3g2,mj2 @ 0x133904080] stream 1, contradictionary STSC and STCO";
        assert_eq!(decode_complaints(s).len(), 1);

        let s = "[av1 @ 0x600002] Failed to decode frame";
        assert_eq!(decode_complaints(s).len(), 1);
    }

    #[test]
    fn mixed_output_keeps_only_the_real_problem() {
        let s = "[null @ 0x1] non monotonically increasing dts to muxer in stream 0: 2 >= 2\n\
                 \n\
                 [h264 @ 0x2] error while decoding MB 12 4\n\
                 [null @ 0x1] non monotonically increasing dts to muxer in stream 0: 5 >= 5";
        let got = decode_complaints(s);
        assert_eq!(got.len(), 1);
        assert!(got[0].contains("error while decoding"));
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

    /// 找一支系統上的 ffmpeg 來當測試素材，找不到就讓呼叫端跳過
    fn test_ffmpeg() -> Option<std::path::PathBuf> {
        [
            "/opt/homebrew/bin/ffmpeg",
            "/usr/local/bin/ffmpeg",
            "/usr/bin/ffmpeg",
        ]
        .into_iter()
        .map(std::path::PathBuf::from)
        .find(|p| p.exists())
    }

    /// 用 lavfi 合成一支兩秒的小影片，可選要不要帶音軌
    fn synth_video(ffmpeg: &Path, name: &str, with_audio: bool) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join("haul-verify-test");
        std::fs::create_dir_all(&dir).unwrap();
        let out = dir.join(name);
        let mut cmd = std::process::Command::new(ffmpeg);
        cmd.args(["-v", "error", "-y", "-f", "lavfi", "-i"])
            .arg("testsrc=duration=2:size=64x64:rate=10");
        if with_audio {
            cmd.args(["-f", "lavfi", "-i", "sine=frequency=440:duration=2"]);
        }
        // 內建編碼器，不倚賴 ffmpeg 的編譯選項
        cmd.args(["-c:v", "mpeg4"]);
        if with_audio {
            cmd.args(["-c:a", "aac"]);
        }
        let status = cmd.arg(&out).status().unwrap();
        assert!(status.success(), "合成測試影片失敗");
        out
    }

    /// 真實案例：一支 Facebook reel 本來就沒有聲音（Facebook 回報
    /// audio_availability: UNAVAILABLE，DASH manifest 只有視訊）。
    /// 「沒有音軌」與「音軌壞了」必須分開——前者對影片是正常的。
    #[tokio::test]
    async fn video_without_audio_track_is_absent_not_broken() {
        let Some(ff) = test_ffmpeg() else {
            return;
        };
        let f = synth_video(&ff, "silent.mp4", false);
        assert_eq!(
            verify_audio_with_ffmpeg(&ff, &f).await.unwrap(),
            Audio::Absent
        );
    }

    #[tokio::test]
    async fn video_with_audio_track_is_decoded() {
        let Some(ff) = test_ffmpeg() else {
            return;
        };
        let f = synth_video(&ff, "with-audio.mp4", true);
        assert_eq!(
            verify_audio_with_ffmpeg(&ff, &f).await.unwrap(),
            Audio::Decoded
        );
    }

    #[tokio::test]
    async fn garbage_is_still_an_error() {
        let Some(ff) = test_ffmpeg() else {
            return;
        };
        let f = tmp(
            "garbage.mp4",
            b"this is not an mp4 file at all, not even close",
        );
        assert!(verify_audio_with_ffmpeg(&ff, &f).await.is_err());
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
