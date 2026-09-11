//! 使用者設定，存成 `<app 資料夾>/settings.json`。
//!
//! 每個欄位都有 serde 預設值：舊版寫的檔缺新欄位照樣讀得出來，壞掉的檔
//! 退回全預設而不是讓 app 開不起來——設定不該是啟動的單點故障。

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Settings {
    /// 輸出資料夾。None 表示用預設（~/Downloads/Haul）
    pub out_dir: Option<PathBuf>,
    /// 預設畫質上限（像素高度）。None 不設限
    pub max_height: Option<u32>,
    /// 同時下載幾個。改了要重新啟動
    pub concurrency: usize,
    /// 登入來源瀏覽器（chrome / firefox…）
    pub cookies_from: Option<String>,
    /// 瀏覽器可執行檔路徑。None 自動找
    pub browser_path: Option<PathBuf>,
    /// 錄製上限（秒）
    pub record_max_secs: u64,
    /// 佇列完成提示音
    pub chime: Chime,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Chime {
    pub enabled: bool,
    /// 自訂音檔路徑；None 用內建合成音
    pub file: Option<PathBuf>,
}

impl Default for Chime {
    fn default() -> Self {
        Self {
            enabled: true,
            file: None,
        }
    }
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            out_dir: None,
            max_height: None,
            concurrency: 3,
            cookies_from: None,
            browser_path: None,
            record_max_secs: 3 * 3600,
            chime: Chime::default(),
        }
    }
}

impl Settings {
    /// 讀不到或讀不懂都退回預設——設定不該是啟動的單點故障。
    pub fn load(path: &Path) -> Self {
        std::fs::read_to_string(path)
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or_default()
    }

    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let json = serde_json::to_string_pretty(self).unwrap_or_default();
        std::fs::write(path, json)
    }

    /// 設定檔的慣用位置：app 資料夾（跟 bin/ 平行）
    pub fn path_in(app_dir: &Path) -> PathBuf {
        app_dir.join("settings.json")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_file_gives_defaults() {
        let s = Settings::load(Path::new("/nope/settings.json"));
        assert_eq!(s.concurrency, 3);
        assert!(s.cookies_from.is_none());
        assert!(s.chime.enabled);
        assert!(s.chime.file.is_none());
        assert_eq!(s.record_max_secs, 3 * 3600);
    }

    #[test]
    fn partial_json_fills_the_rest_with_defaults() {
        let dir = std::env::temp_dir().join("haul-settings-test");
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("partial.json");
        // 只寫一個欄位；舊版寫的檔缺新欄位不該讀不出來
        std::fs::write(&f, r#"{"concurrency": 5}"#).unwrap();
        let s = Settings::load(&f);
        assert_eq!(s.concurrency, 5);
        assert_eq!(s.record_max_secs, 3 * 3600); // 預設補上
    }

    #[test]
    fn round_trips_through_disk() {
        let dir = std::env::temp_dir().join("haul-settings-test");
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("rt.json");
        let mut s = Settings::default();
        s.cookies_from = Some("chrome".into());
        s.chime.enabled = true;
        s.chime.file = Some("/tmp/ding.mp3".into());
        s.save(&f).unwrap();
        let back = Settings::load(&f);
        assert_eq!(back.cookies_from.as_deref(), Some("chrome"));
        assert!(back.chime.enabled);
        assert_eq!(back.chime.file.as_deref(), Some(Path::new("/tmp/ding.mp3")));
    }

    #[test]
    fn bad_json_falls_back_to_defaults_not_panic() {
        let dir = std::env::temp_dir().join("haul-settings-test");
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("bad.json");
        std::fs::write(&f, "not json at all").unwrap();
        // 壞掉的設定檔不該讓 app 開不起來
        assert_eq!(Settings::load(&f).concurrency, 3);
    }
}
