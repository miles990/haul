//! Haul 的下載引擎。
//!
//! GUI 與 CLI 都只是這個 crate 的外殼：引擎透過 `Sink` 回呼把事件送出去，
//! 兩邊各自決定要轉成 Tauri event 還是 NDJSON。行為只有一份，不會分岔。

pub mod direct;
pub mod engine;
pub mod extract;
pub mod tools;
pub mod verify;

pub use engine::{
    default_bin_dir, default_out_dir, home_dir, load_history, open_with_system, sanitize,
    validate_playable, Config, Engine, Event, Item, Sink,
};
pub use extract::Mode;
pub use tools::Tools;
