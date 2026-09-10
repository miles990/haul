//! 執行紀錄。
//!
//! 一行一則 NDJSON，跟歷史檔同一個思路：**檔案就是介面**。人要看就
//! `haul logs`，程式要查就 `haul logs --json | jq`，不需要另外發明查詢協定。
//!
//! 輪替是依大小而非時間——不需要排程器，而且對「檔案別無限長大」這個
//! 唯一目的來說已經足夠。

use serde::{Deserialize, Serialize};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

/// 單檔上限，超過就輪替
pub const MAX_BYTES: u64 = 4 * 1024 * 1024;
/// 保留幾個舊檔（haul.log.1 … haul.log.N）
pub const KEEP: usize = 3;

#[derive(Clone, Serialize, Deserialize, Debug)]
pub struct Entry {
    /// ISO-8601 UTC，人看的
    pub time: String,
    /// 毫秒 epoch，程式排序用的
    pub ts: u64,
    /// info | warn | error
    pub level: String,
    /// 事件名稱，例如 download.start
    pub event: String,
    #[serde(default, skip_serializing_if = "serde_json::Value::is_null")]
    pub data: serde_json::Value,
}

pub struct Logger {
    path: PathBuf,
    max_bytes: u64,
    keep: usize,
    /// 同一個程序內的寫入序列化。跨程序靠 O_APPEND 的原子性。
    lock: Mutex<()>,
}

impl Logger {
    pub fn new(dir: &Path) -> std::io::Result<Self> {
        std::fs::create_dir_all(dir)?;
        Ok(Self {
            path: dir.join("haul.log"),
            max_bytes: MAX_BYTES,
            keep: KEEP,
            lock: Mutex::new(()),
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn info(&self, event: &str, data: serde_json::Value) {
        self.write("info", event, data);
    }

    pub fn warn(&self, event: &str, data: serde_json::Value) {
        self.write("warn", event, data);
    }

    pub fn error(&self, event: &str, data: serde_json::Value) {
        self.write("error", event, data);
    }

    fn write(&self, level: &str, event: &str, data: serde_json::Value) {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default();
        let entry = Entry {
            time: iso8601(now.as_secs()),
            ts: now.as_millis() as u64,
            level: level.to_string(),
            event: event.to_string(),
            data,
        };
        let Ok(line) = serde_json::to_string(&entry) else {
            return;
        };

        let _g = self.lock.lock();
        self.rotate_if_needed();

        // 寫紀錄失敗不該影響主要工作，所以全部忽略錯誤
        if let Ok(mut f) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
        {
            let _ = writeln!(f, "{line}");
        }
    }

    /// 超過上限就把 haul.log 推成 .1，舊的往後遞補，最舊的丟掉。
    ///
    /// 跨程序同時輪替可能互相踩到，最壞情況是少數幾行紀錄遺失。
    /// 為了診斷用的紀錄去引進檔案鎖不划算。
    fn rotate_if_needed(&self) {
        let Ok(meta) = std::fs::metadata(&self.path) else {
            return; // 檔案還不存在
        };
        if meta.len() < self.max_bytes {
            return;
        }
        // 由舊往新搬，才不會覆蓋掉還沒搬的
        for n in (1..self.keep).rev() {
            let from = self.numbered(n);
            let to = self.numbered(n + 1);
            if from.exists() {
                let _ = std::fs::rename(&from, &to);
            }
        }
        let _ = std::fs::rename(&self.path, self.numbered(1));
    }

    fn numbered(&self, n: usize) -> PathBuf {
        let mut s = self.path.clone().into_os_string();
        s.push(format!(".{n}"));
        PathBuf::from(s)
    }
}

/// 讀回最後 `limit` 則。會一併讀輪替出去的舊檔，
/// 不然剛輪替完就查會看起來像什麼都沒發生過。
pub fn tail(log_path: &Path, limit: usize) -> Vec<Entry> {
    let mut files: Vec<PathBuf> = Vec::new();
    for n in (1..=KEEP).rev() {
        let mut s = log_path.to_path_buf().into_os_string();
        s.push(format!(".{n}"));
        let p = PathBuf::from(s);
        if p.exists() {
            files.push(p);
        }
    }
    files.push(log_path.to_path_buf());

    let mut all: Vec<Entry> = Vec::new();
    for f in files {
        let Ok(text) = std::fs::read_to_string(&f) else {
            continue;
        };
        all.extend(
            text.lines()
                .filter(|l| !l.trim().is_empty())
                // 壞掉的一行不該讓整份紀錄消失
                .filter_map(|l| serde_json::from_str::<Entry>(l).ok()),
        );
    }
    all.sort_by_key(|e| e.ts);
    if all.len() > limit {
        all.drain(0..all.len() - limit);
    }
    all
}

/// epoch 秒 → `YYYY-MM-DDTHH:MM:SSZ`。
///
/// 為了一個時間戳去多背一個日期函式庫不划算，而這段算式是固定的
/// （Howard Hinnant 的 civil-from-days），有測試釘著就夠可靠。
pub fn iso8601(secs: u64) -> String {
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    let (y, m, d) = civil_from_days(days);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z",
        rem / 3600,
        (rem % 3600) / 60,
        rem % 60
    )
}

fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32; // [1, 12]
    (if m <= 2 { y + 1 } else { y }, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn formats_known_epochs() {
        assert_eq!(iso8601(0), "1970-01-01T00:00:00Z");
        assert_eq!(iso8601(1_000_000_000), "2001-09-09T01:46:40Z");
        assert_eq!(iso8601(1_700_000_000), "2023-11-14T22:13:20Z");
    }

    #[test]
    fn handles_leap_days() {
        // 2000 是閏年（能被 400 整除），2100 不是
        assert_eq!(iso8601(951_782_400), "2000-02-29T00:00:00Z");
        assert_eq!(iso8601(1_709_164_800), "2024-02-29T00:00:00Z");
    }

    #[test]
    fn rotates_when_over_the_limit() {
        let dir = std::env::temp_dir().join("haul-log-rotate-test");
        let _ = std::fs::remove_dir_all(&dir);
        let mut lg = Logger::new(&dir).unwrap();
        lg.max_bytes = 512; // 讓它很快就滿

        for i in 0..200 {
            lg.info("test.event", json!({ "i": i, "pad": "x".repeat(40) }));
        }

        assert!(lg.path().exists(), "現用的紀錄檔要在");
        assert!(lg.numbered(1).exists(), "應該已經輪替出 .1");

        let entries = tail(lg.path(), usize::MAX);

        // 最重要的不變量：留下來的必須是最新的。
        // 一個保留舊紀錄卻丟掉新紀錄的輪替毫無用處。
        assert_eq!(
            entries.last().unwrap().data["i"],
            199,
            "最後寫入的那則必須還在"
        );

        // 輪替出去的舊檔也要讀得回來，否則剛輪替完就查會像什麼都沒發生。
        // 拿現用檔自己的行數當基準，比寫死一個數字可靠。
        let in_current = std::fs::read_to_string(lg.path()).unwrap().lines().count();
        assert!(
            entries.len() > in_current,
            "tail 應該跨檔讀取：現用檔有 {in_current} 行，但只讀到 {} 則",
            entries.len()
        );

        // 輪替的目的就是設上限，所以舊紀錄本來就該被丟掉
        assert!(
            entries.len() < 200,
            "超過上限的舊紀錄應該被丟棄，卻讀到 {} 則",
            entries.len()
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn tail_returns_the_most_recent() {
        let dir = std::env::temp_dir().join("haul-log-tail-test");
        let _ = std::fs::remove_dir_all(&dir);
        let lg = Logger::new(&dir).unwrap();

        for i in 0..20 {
            lg.info("n", json!({ "i": i }));
        }
        let last = tail(lg.path(), 5);
        assert_eq!(last.len(), 5);
        assert_eq!(last[4].data["i"], 19);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn broken_lines_do_not_destroy_the_rest() {
        let dir = std::env::temp_dir().join("haul-log-broken-test");
        let _ = std::fs::remove_dir_all(&dir);
        let lg = Logger::new(&dir).unwrap();
        lg.info("ok", json!({}));

        // 塞一行壞的進去
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(lg.path())
            .unwrap();
        writeln!(f, "{{ this is not json").unwrap();
        drop(f);
        lg.info("ok2", json!({}));

        let entries = tail(lg.path(), usize::MAX);
        assert_eq!(entries.len(), 2, "壞掉的一行該被跳過，其餘照常讀回");

        let _ = std::fs::remove_dir_all(&dir);
    }
}
