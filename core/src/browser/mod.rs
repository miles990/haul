//! 第四條解析路徑：讓一個 Haul 自己啟動的 Chrome 把頁面跑起來，攔截它發出的
//! 媒體請求。跟 extract / gallery / direct 一樣只回答「這頁上有什麼」，
//! 下載與驗證仍走引擎。
//!
//! 為什麼不接管使用者正在跑的 Chrome：Chrome 136 起禁止對預設 profile 開
//! remote debugging。所以是獨立實例、獨立 profile（放在 app 資料夾），
//! 使用者在裡面登入過的站會保留。

pub mod cdp;
pub mod chrome;
pub mod record;
pub mod sniff;
