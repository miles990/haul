# 列表縮圖、圖片模式、自我更新、公開發布 — 執行計畫

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** 讓現有列表能當「已下載」瀏覽用（縮圖、分組、在 Finder 顯示）、GUI 補上圖片模式、app 能從 GitHub Releases 自我更新，然後把 repo 公開並出中英 README。

**Architecture:** 縮圖由引擎在驗證通過後用 ffmpeg 產生，存在輸出資料夾的 `.haul-thumbs/`，`Item.thumb` 跟著歷史走；GUI 只傳 id、拿 base64。自我更新用 `tauri-plugin-updater` 的 Rust API，前端不拿任何 plugin 權限。設計：`docs/plans/2026-09-11-library-and-updater-design.md`。

**Tech Stack:** Rust（tokio、anyhow、serde）、Tauri 2、tauri-plugin-updater 2、ffmpeg（已由 Haul 管理）、手寫 `ui/index.html`、GitHub Actions + tauri-action。

**慣例：** 測試跑 `cargo test -p haul-core --lib`；需要 ffmpeg 的測試用 `verify.rs` 裡的 `test_ffmpeg()` 模式（找不到就跳過）。每個 Task 一個 commit，訊息用「引擎：」「GUI：」「發布：」「文件：」開頭。

---

## Task 1: 引擎 — 縮圖模組

**Files:**
- Create: `core/src/thumb.rs`
- Modify: `core/src/lib.rs`（加 `pub mod thumb;`）

**Step 1: 寫會失敗的測試**（`core/src/thumb.rs` 底部）

```rust
#[cfg(test)]
mod tests {
    use super::*;

    fn test_ffmpeg() -> Option<PathBuf> {
        ["/opt/homebrew/bin/ffmpeg", "/usr/local/bin/ffmpeg", "/usr/bin/ffmpeg"]
            .into_iter()
            .map(PathBuf::from)
            .find(|p| p.exists())
    }

    fn dir() -> PathBuf {
        let d = std::env::temp_dir().join("haul-thumb-test");
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// lavfi 合成：影片（可帶音軌）、純音訊（可帶封面）、圖片
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
        let v = synth(&ff, "v.mp4", &["-f", "lavfi", "-i", "testsrc=duration=3:size=320x180:rate=10", "-c:v", "mpeg4"]);
        let dest = dir().join("v.jpg");
        let got = make(&ff, &v, Some(0.5), &dest).await.unwrap();
        assert_eq!(got.as_deref(), Some(dest.as_path()));
        assert!(std::fs::metadata(&dest).unwrap().len() > 0);
    }

    #[tokio::test]
    async fn audio_without_cover_yields_none_not_error() {
        let Some(ff) = test_ffmpeg() else { return };
        let a = synth(&ff, "a.m4a", &["-f", "lavfi", "-i", "sine=frequency=440:duration=2", "-c:a", "aac"]);
        let dest = dir().join("a.jpg");
        assert!(make(&ff, &a, None, &dest).await.unwrap().is_none());
        assert!(!dest.exists(), "失敗時不該留半截檔");
    }

    #[tokio::test]
    async fn audio_with_cover_gets_the_cover() {
        let Some(ff) = test_ffmpeg() else { return };
        let cover = synth(&ff, "cover.png", &["-f", "lavfi", "-i", "color=c=red:size=64x64", "-frames:v", "1"]);
        let mp3 = synth(&ff, "c.mp3", &[
            "-f", "lavfi", "-i", "sine=frequency=440:duration=2",
            "-i", cover.to_str().unwrap(),
            "-map", "0:a", "-map", "1:v", "-c:a", "libmp3lame", "-c:v", "copy",
            "-id3v2_version", "3", "-disposition:v", "attached_pic",
        ]);
        let dest = dir().join("c.jpg");
        assert!(make(&ff, &mp3, None, &dest).await.unwrap().is_some());
    }

    #[tokio::test]
    async fn image_is_downscaled() {
        let Some(ff) = test_ffmpeg() else { return };
        let img = synth(&ff, "i.png", &["-f", "lavfi", "-i", "color=c=blue:size=400x300", "-frames:v", "1"]);
        let dest = dir().join("i.jpg");
        assert!(make(&ff, &img, None, &dest).await.unwrap().is_some());
    }
}
```

**Step 2: 跑，確認編譯失敗**

Run: `cargo test -p haul-core --lib thumb::`
Expected: `error[E0432]`／找不到 `thumb` 模組。

**Step 3: 實作**

`core/src/thumb.rs`：

```rust
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
    let vf = format!(
        "scale={SIZE}:{SIZE}:force_original_aspect_ratio=increase,crop={SIZE}:{SIZE}"
    );
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
        && tokio::fs::metadata(dest).await.map(|m| m.len() > 0).unwrap_or(false);
    if !ok {
        let _ = tokio::fs::remove_file(dest).await;
        return Ok(None);
    }
    Ok(Some(dest.to_path_buf()))
}
```

`core/src/lib.rs`：在 `pub mod verify;` 附近加 `pub mod thumb;`。

**Step 4: 跑測試**

Run: `cargo test -p haul-core --lib thumb::`
Expected: 5 passed。若 `audio_with_cover` 因系統 ffmpeg 沒有 libmp3lame 而失敗，把編碼改成 `-c:a aac` 容器 `.m4a`（`-disposition:v attached_pic` 對 mp4 同樣有效）。

**Step 5: Commit**

```bash
git add core/src/thumb.rs core/src/lib.rs
git commit -m "引擎：縮圖模組"
```

---

## Task 2: 引擎 — `Item.thumb`、完成時產縮圖、`kind: image`

**Files:**
- Modify: `core/src/engine.rs`（`Item`、`push` 呼叫端的 kind、`finish_staged`、新方法 `ensure_thumb`）

**Step 1: 寫會失敗的測試**（`engine.rs` 的 `mod tests`）

```rust
    #[test]
    fn kind_follows_mode_so_retries_keep_it() {
        assert_eq!(kind_for(Mode::Video), "video");
        assert_eq!(kind_for(Mode::Audio), "audio");
        // 之前圖片模式記成 video，重試時就變成抓影片
        assert_eq!(kind_for(Mode::Image), "image");
        assert_eq!(Mode::parse(kind_for(Mode::Image)), Mode::Image);
    }

    #[test]
    fn thumb_seek_only_for_video_media() {
        use verify::Level;
        assert_eq!(thumb_seek("video", Level::Media, Some(100.0)), Some(2.0));
        assert_eq!(thumb_seek("video", Level::Media, None), Some(0.0));
        assert_eq!(thumb_seek("audio", Level::Media, Some(100.0)), None);
        assert_eq!(thumb_seek("image", Level::Image, None), None);
    }

    #[test]
    fn only_media_and_images_get_thumbs() {
        use verify::Level;
        assert!(wants_thumb(Level::Media));
        assert!(wants_thumb(Level::Image));
        assert!(!wants_thumb(Level::Archive));
        assert!(!wants_thumb(Level::Text));
    }

    #[test]
    fn history_round_trips_thumb() {
        let mut it = Item { /* 用 push 的欄位，status done */ ..sample_item() };
        it.thumb = Some("/out/.haul-thumbs/x.jpg".into());
        let line = serde_json::to_string(&it).unwrap();
        let back: Item = serde_json::from_str(&line).unwrap();
        assert_eq!(back.thumb.as_deref(), Some("/out/.haul-thumbs/x.jpg"));
        // 舊的歷史行沒有 thumb 也要讀得起來
        let old = line.replace(",\"thumb\":\"/out/.haul-thumbs/x.jpg\"", "");
        assert!(serde_json::from_str::<Item>(&old).unwrap().thumb.is_none());
    }
```

`sample_item()` 是測試用的小 helper（照 `push` 的欄位填，status `"done"`）。

**Step 2: 跑，確認失敗**

Run: `cargo test -p haul-core --lib engine::tests`
Expected: `kind_for` / `thumb_seek` / `wants_thumb` / `thumb` 欄位不存在。

**Step 3: 實作**

1. `Item` 加欄位（放在 `source` 後面）：
   ```rust
       /// 縮圖的完整路徑（`.haul-thumbs/` 裡）。沒有就是產不出來（無封面的音樂、PDF）
       #[serde(default, skip_serializing_if = "Option::is_none")]
       pub thumb: Option<String>,
   ```
   `push` 初始化加 `thumb: None`。

2. 自由函式（放 `sanitize` 附近）：
   ```rust
   /// 項目的 kind 直接對應模式，重試時 `Mode::parse(kind)` 才拿得回同一個模式
   fn kind_for(mode: Mode) -> &'static str {
       match mode {
           Mode::Video => "video",
           Mode::Audio => "audio",
           Mode::Image => "image",
       }
   }

   fn wants_thumb(level: verify::Level) -> bool {
       matches!(level, verify::Level::Media | verify::Level::Image)
   }

   /// 影片抽 2% 處那格（跟影片抽樣驗證的第一個點相同）；音樂抽封面、圖片就是圖片，不用 seek
   fn thumb_seek(kind: &str, level: verify::Level, secs: Option<f64>) -> Option<f64> {
       (kind == "video" && level == verify::Level::Media).then(|| secs.unwrap_or(0.0) * 0.02)
   }
   ```
   `add()` 裡 `let kind = if mode == Mode::Audio {...}` 改成 `let kind = kind_for(mode);`。

3. `finish_staged`：在 `let size = ...` 之後、`self.log.info("item.done"...)` 之前：
   ```rust
        // 縮圖：產不出來不影響完成
        let thumb = if wants_thumb(level) {
            let kind = self.items.lock().unwrap().iter().find(|i| i.id == id).map(|i| i.kind.clone()).unwrap_or_default();
            let dest = thumb::path_for(&self.cfg.out_dir, &dest);
            match thumb::make(&tools.ffmpeg, &dest_media, thumb_seek(&kind, level, reported.or(secs)), &dest).await {
                Ok(p) => p.map(|p| p.to_string_lossy().to_string()),
                Err(e) => { self.log.warn("thumb.failed", serde_json::json!({"id": id, "error": e.to_string()})); None }
            }
        } else { None };
   ```
   （變數名依實際程式碼調整：媒體檔是 `dest`，縮圖目的地另取名 `thumb_dest`。）`finish` 閉包加 `i.thumb = thumb;`。log 加 `"thumb": ...` 可有可無。確認 `Logger` 有 `warn`，沒有就用 `info`。

4. 新方法（`snapshot` 附近）：
   ```rust
    /// 確認或補產一個完成項目的縮圖，回傳縮圖路徑。
    ///
    /// 舊項目（更新前下載的）沒有縮圖，第一次顯示時從原檔補一張；
    /// 縮圖檔名由原檔路徑決定，所以不必寫回歷史——下次啟動看檔案在不在就好。
    pub async fn ensure_thumb(&self, id: u64) -> Result<Option<PathBuf>, String> {
        let (path, kind, level, secs, thumb) = {
            let g = self.items.lock().unwrap();
            let it = g.iter().find(|i| i.id == id).ok_or("沒有這個項目")?;
            if it.status != "done" { return Err("還沒完成".into()); }
            (
                it.path.clone().ok_or("沒有檔案")?,
                it.kind.clone(),
                it.verified.clone(),
                it.secs,
                it.thumb.clone(),
            )
        };
        let media = PathBuf::from(&path);
        if !media.is_file() { return Err("檔案已不在".into()); }
        if let Some(t) = thumb.as_deref().map(PathBuf::from).filter(|p| p.is_file()) {
            return Ok(Some(t));
        }
        let level = match level.as_deref() {
            Some("media") => verify::Level::Media,
            Some("image") => verify::Level::Image,
            _ => return Ok(None),
        };
        let dest = thumb::path_for(&self.cfg.out_dir, &media);
        let got = if dest.is_file() {
            Some(dest)
        } else {
            let tools = self.tools().await?;
            thumb::make(&tools.ffmpeg, &media, thumb_seek(&kind, level, secs), &dest)
                .await
                .map_err(|e| e.to_string())?
        };
        if let Some(p) = &got {
            let s = p.to_string_lossy().to_string();
            self.update(id, |i| i.thumb = Some(s));
        }
        Ok(got)
    }
   ```
   `verify::Level` 需要 `PartialEq`（已有）。`load_history` 不動：它只看 `path` 在不在。

**Step 4: 跑全部測試**

Run: `cargo test -p haul-core --lib`
Expected: 全綠。再實測：`cargo build -p haul-cli && ./target/debug/haul --json -o /tmp/haul-thumb-test 'https://www.facebook.com/share/v/1EgcALi45c/' | tail -1` 應含 `"thumb":"/tmp/haul-thumb-test/.haul-thumbs/….jpg"`，且該檔存在。

**Step 5: Commit**

```bash
git add core/src/engine.rs
git commit -m "引擎：完成時產縮圖、ensure_thumb 補舊項目、kind 對應模式"
```

---

## Task 3: GUI 後端 — `thumb`、`reveal_file` 指令，CSP

**Files:**
- Modify: `src-tauri/src/main.rs`
- Modify: `src-tauri/tauri.conf.json`（CSP 加 `img-src 'self' data:`）
- Modify: `core/src/engine.rs`（`reveal_in_folder`，放 `open_with_system` 旁）

**Step 1: 測試**（engine.rs tests；reveal 的路徑檢查沿用 `validate_playable`，已有測試；這裡只確認函式存在且拒絕資料夾外的路徑）

```rust
    #[test]
    fn reveal_refuses_paths_outside_the_download_folder() {
        let root = std::env::temp_dir().join("haul-reveal-test");
        std::fs::create_dir_all(&root).unwrap();
        assert!(reveal_in_folder(&root, "/nope/x.mp4").is_err());
    }
```

**Step 2: 跑，確認失敗**（函式不存在）

**Step 3: 實作**

engine.rs：
```rust
/// 在 Finder／檔案總管裡選取這個檔案。想搬、想刪、想改名時一步跳到真正的檔案管理器。
pub fn reveal_in_folder(out_dir: &Path, path: &str) -> Result<(), String> {
    let target = validate_playable(out_dir, path)?;
    #[cfg(target_os = "macos")]
    let mut cmd = { let mut c = std::process::Command::new("open"); c.arg("-R"); c };
    #[cfg(target_os = "windows")]
    let mut cmd = {
        let mut c = std::process::Command::new("explorer");
        c.arg(format!("/select,{}", target.display()));
        c
    };
    #[cfg(all(not(target_os = "macos"), not(target_os = "windows")))]
    let mut cmd = { let mut c = std::process::Command::new("xdg-open"); c.arg(target.parent().unwrap_or(&target)); c };
    #[cfg(not(target_os = "windows"))]
    cmd.arg(&target);
    cmd.spawn().map(|_| ()).map_err(|e| format!("開啟失敗：{e}"))
}
```
（Windows 分支 `/select,` 已含路徑，所以 `cmd.arg(&target)` 只在非 Windows 加。）

main.rs：
```rust
/// 完成項目的縮圖，base64 JPEG。前端只傳 id，不傳路徑。
/// Ok(None) = 這個項目沒有縮圖（無封面的音樂、PDF）；Err = 原檔已不在等
#[tauri::command]
async fn thumb(state: State<'_, Arc<Engine>>, id: u64) -> Result<Option<String>, String> {
    use base64::Engine as _;
    let Some(p) = state.ensure_thumb(id).await? else { return Ok(None) };
    let bytes = tokio::fs::read(&p).await.map_err(|e| e.to_string())?;
    Ok(Some(base64::engine::general_purpose::STANDARD.encode(bytes)))
}

#[tauri::command]
fn reveal_file(state: State<'_, Arc<Engine>>, path: String) -> Result<(), String> {
    reveal_in_folder(state.out_dir(), &path)
}
```
註冊到 `generate_handler!`；`use haul_core::reveal_in_folder`（看現有 use 怎麼寫）。`src-tauri/Cargo.toml` 要有 `tokio`（若沒有，改用 `std::fs::read` 並把指令改成同步——但 `ensure_thumb` 是 async，所以 tokio 應已在依賴裡，確認）。

tauri.conf.json CSP：`"default-src 'self'; img-src 'self' data:; style-src 'self' 'unsafe-inline'; script-src 'self' 'unsafe-inline'"`。

**Step 4:** `cargo test -p haul-core --lib` 全綠；`cargo build -p haul-gui`（或 `cd src-tauri && cargo build`）過。

**Step 5: Commit**

```bash
git add core/src/engine.rs src-tauri/src/main.rs src-tauri/tauri.conf.json
git commit -m "GUI 後端：縮圖與在 Finder 顯示"
```

---

## Task 4: GUI 前端 — 縮圖、分組、在 Finder 顯示、圖片模式

**Files:**
- Modify: `ui/index.html`

前端沒有測試框架；驗證方式是 `cd src-tauri && cargo tauri dev` 目視，加上 Task 2 的 CLI 產出。

**Step 1: HTML**

1. 模式列加第三顆：
   ```html
   <button type="button" class="mode" data-mode="image" role="radio" aria-checked="false">圖片</button>
   ```
   textarea placeholder 改「貼上影片、音樂或圖片的連結，一行一個」。

2. `#queue` 改成：
   ```html
   <div id="queue">
     <p class="empty" id="empty">佇列是空的。貼上連結就會開始下載。</p>
     <h2 class="group" id="active-h" hidden>進行中</h2>
     <div id="active"></div>
     <h2 class="group" id="done-h" hidden>已完成</h2>
     <div id="done"></div>
   </div>
   ```

**Step 2: CSS**

```css
  .group {
    margin: 8px 18px 2px;
    font: 600 11px/1 var(--sans);
    letter-spacing: 0.06em;
    text-transform: uppercase;
    color: var(--dim);
  }

  .track {
    grid-template-columns: 48px 1fr auto;   /* 原本 1fr auto */
    ...
  }
  .name { grid-column: 2; }
  .stat { grid-column: 3; }
  .meter, .why, .act, .cands { grid-column: 2 / -1; }

  /* 縮圖：認得出誰是誰。沒縮圖時顯示型別圖示 */
  .thumb {
    grid-column: 1;
    grid-row: 1 / span 2;
    align-self: start;
    width: 48px;
    height: 48px;
    border-radius: 3px;
    background: var(--sunk);
    display: grid;
    place-items: center;
    color: var(--dim);
    font-size: 18px;
    overflow: hidden;
  }
  .thumb img { width: 100%; height: 100%; object-fit: cover; display: none; }
  .thumb.has img { display: block; }
  .thumb.has::before { content: none; }
  .thumb[data-kind="video"]::before { content: '▶'; }
  .thumb[data-kind="audio"]::before { content: '♪'; }
  .thumb[data-kind="image"]::before { content: '▣'; }
  /* 原檔已不在：調暗、不可點 */
  .track[data-missing] { opacity: 0.5; cursor: default; }

  .show {
    grid-column: 3;
    grid-row: 2;
    justify-self: end;
    visibility: hidden;
    padding: 2px 8px;
    border: 1px solid var(--edge);
    border-radius: 3px;
    background: transparent;
    color: var(--dim);
    font: 11px/1.4 var(--sans);
  }
  .track[data-status="done"]:not([data-missing]):hover .show { visibility: visible; }
```

**Step 3: JS**

1. 取容器：
   ```js
   const activeBox = document.getElementById('active');
   const doneBox   = document.getElementById('done');
   const activeH   = document.getElementById('active-h');
   const doneH     = document.getElementById('done-h');
   const REVEAL_LABEL = /Win/.test(navigator.platform) ? '在檔案總管顯示' : '在 Finder 顯示';
   ```

2. `makeRow`：innerHTML 最前面加 `'<span class="thumb"><img alt=""></span>'`，最後加 `'<button type="button" class="show"></button>'`；refs 加 `thumb`、`img`、`show`。`refs.show.textContent = REVEAL_LABEL;` click：
   ```js
   refs.show.addEventListener('click', ev => {
     ev.stopPropagation();
     if (el.dataset.path) invoke('reveal_file', { path: el.dataset.path }).catch(err => say(String(err), true));
   });
   ```
   `queue.appendChild(el)` 改成不 append（由 `place` 決定）。

3. 分組：
   ```js
   function place(el, status) {
     const box = status === 'done' ? doneBox : activeBox;
     if (el.parentNode !== box) {
       if (status === 'done') box.prepend(el); else box.appendChild(el);
     }
     activeH.hidden = activeBox.childElementCount === 0;
     doneH.hidden = doneBox.childElementCount === 0;
   }
   ```
   `paint` 裡 `el.dataset.status = it.status;` 之後呼叫 `place(el, it.status);`。

4. 縮圖：
   ```js
   const thumbAsked = new Set();
   function loadThumb(it, el, refs) {
     if (thumbAsked.has(it.id)) return;
     thumbAsked.add(it.id);
     invoke('thumb', { id: it.id }).then(b64 => {
       if (!b64) return;
       refs.img.src = 'data:image/jpeg;base64,' + b64;
       refs.thumb.classList.add('has');
     }).catch(() => {
       // 原檔已不在（或讀不到）：留著紀錄但不再假裝點得開
       el.dataset.missing = '1';
       el.title = '檔案已不在';
     });
   }
   ```
   `paint` 裡：`refs.thumb.dataset.kind = it.kind;`；`if (it.status === 'done') loadThumb(it, el, refs);`

5. 點擊播放：條件加 `|| row.dataset.missing`。

6. `reset`：`rows.clear()` 後清空兩個容器 `activeBox.replaceChildren(); doneBox.replaceChildren();`，並在 `updateTally` 之前呼叫一次標題更新（把 `place` 的兩行抽成 `updateHeads()`）。`thumbAsked.clear()` 也一起。

7. `submit` 的 mode 已由按鈕帶入，不用改。

**Step 4: 目視驗證**

Run: `cd src-tauri && cargo tauri dev`
檢查：重開後歷史項目顯示縮圖（影片抽格、貓咪 reel）；無聲 Facebook 影片有縮圖；新貼一個連結出現在「進行中」，完成後跳到「已完成」最上面；hover 出現「在 Finder 顯示」且點了 Finder 選取檔案；模式「圖片」對 YouTube 連結抓到封面（kind image、▣ 占位→縮圖）；把某個檔案移走再重開，該列調暗、點不動。

**Step 5: Commit**

```bash
git add ui/index.html
git commit -m "GUI：列表縮圖、進行中／已完成分組、在 Finder 顯示、圖片模式"
```

---

## Task 5: 自我更新 — plugin 與指令

**Files:**
- Modify: `src-tauri/Cargo.toml`（`tauri-plugin-updater = "2"`）
- Modify: `src-tauri/tauri.conf.json`（`plugins.updater`、`bundle.createUpdaterArtifacts`）
- Modify: `src-tauri/src/main.rs`

**Step 1: 產金鑰**

```bash
mkdir -p ~/.tauri
cargo tauri signer generate -w ~/.tauri/haul.key --ci
cat ~/.tauri/haul.key.pub
```
（`--ci` 免密碼；若版本不同請 `cargo tauri signer generate --help`。）記下公鑰。

**Step 2: 設定**

`src-tauri/Cargo.toml` `[dependencies]` 加 `tauri-plugin-updater = "2"`。

`tauri.conf.json`：
```json
  "bundle": { ..., "createUpdaterArtifacts": true },
  "plugins": {
    "updater": {
      "pubkey": "<公鑰內容>",
      "endpoints": ["https://github.com/miles990/haul/releases/latest/download/latest.json"],
      "windows": { "installMode": "passive" }
    }
  }
```

**Step 3: 指令**

```rust
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct UpdateInfo { version: String, body: Option<String> }

/// 問 GitHub Releases 有沒有新版。沒網路、dev 建置沒有 release 都會 Err，
/// 啟動時的自動檢查把它吞掉，設定面板手動按的才顯示。
#[tauri::command]
async fn check_update(app: AppHandle) -> Result<Option<UpdateInfo>, String> {
    use tauri_plugin_updater::UpdaterExt;
    let u = app.updater().map_err(|e| e.to_string())?.check().await.map_err(|e| e.to_string())?;
    Ok(u.map(|u| UpdateInfo { version: u.version.clone(), body: u.body.clone() }))
}

/// 下載、覆蓋安裝、重新啟動。進度用 `update` 事件送回前端。
#[tauri::command]
async fn install_update(app: AppHandle) -> Result<(), String> {
    use tauri_plugin_updater::UpdaterExt;
    let Some(u) = app.updater().map_err(|e| e.to_string())?.check().await.map_err(|e| e.to_string())? else {
        return Err("已是最新版".into());
    };
    let h = app.clone();
    let mut got: u64 = 0;
    u.download_and_install(
        move |chunk, total| {
            got += chunk as u64;
            let _ = h.emit("update", serde_json::json!({ "bytes": got, "total": total }));
        },
        || {},
    )
    .await
    .map_err(|e| e.to_string())?;
    app.restart()
}
```
`.plugin(tauri_plugin_updater::Builder::new().build())` 加在 `tauri::Builder::default()` 後；兩個指令註冊。`Serialize` 已從 serde 引入（看檔頭）。

**Step 4: 建置**

Run: `cd src-tauri && cargo build`
Expected: 過。`cargo tauri build --no-bundle` 也過（不 bundle 不需要私鑰）。

**Step 5: Commit**

```bash
git add src-tauri/Cargo.toml src-tauri/tauri.conf.json src-tauri/src/main.rs Cargo.lock
git commit -m "GUI 後端：Tauri updater，從 GitHub Releases 自我更新"
```

---

## Task 6: 自我更新 — UI

**Files:**
- Modify: `ui/index.html`

**Step 1: HTML**（`#setup` 下面）

```html
<div id="update-bar" hidden>
  <span id="update-text"></span>
  <button type="button" id="update-go" class="primary">更新並重新啟動</button>
</div>
```
設定面板「工具」fieldset 加：`<button type="button" id="update-app">檢查 Haul 更新</button>`。

**Step 2: CSS**

`#update-bar` 沿用 `#setup` 的樣式（選擇器寫成 `#setup, #update-bar { ... }`），`#update-bar { justify-content: space-between; }`。

**Step 3: JS**

```js
  const updateBar = $('update-bar'), updateText = $('update-text'), updateGo = $('update-go');
  async function checkUpdate(manual) {
    try {
      const u = await invoke('check_update');
      if (u) {
        updateText.textContent = `有新版本 v${u.version}`;
        updateBar.hidden = false;
        if (manual) dlg.close();
      } else if (manual) {
        say('Haul 已是最新版', false);
      }
    } catch (e) {
      if (manual) say('檢查更新失敗：' + String(e), true);
    }
  }
  updateGo.addEventListener('click', async () => {
    updateGo.disabled = true;
    updateText.textContent = '正在下載更新…';
    try { await invoke('install_update'); }   // 成功就重啟，不會回來
    catch (e) { updateText.textContent = '更新失敗：' + String(e); updateGo.disabled = false; }
  });
  listen('update', e => {
    const { bytes, total } = e.payload;
    updateText.textContent = total ? `正在下載更新 ${mb(bytes)} / ${mb(total)}` : `正在下載更新 ${mb(bytes)}`;
  });
  $('update-app').addEventListener('click', () => checkUpdate(true));
```
啟動 IIFE 最後加 `checkUpdate(false);`（`$` 要在這段之前定義；目前 `$` 在設定面板段落定義，把這段放在它後面）。

**Step 4: 目視**

`cargo tauri dev`：設定→「檢查 Haul 更新」應顯示「檢查更新失敗：…」（repo 還是 private／沒有 latest.json，屬預期）；啟動時不出現任何東西。

**Step 5: Commit**

```bash
git add ui/index.html
git commit -m "GUI：檢查更新與一鍵更新"
```

---

## Task 7: 發布流程 — workflow 與 secrets

**Files:**
- Modify: `.github/workflows/release.yml`

**Step 1:** tauri-action 那步：
```yaml
        env:
          GITHUB_TOKEN: ${{ secrets.GITHUB_TOKEN }}
          TAURI_SIGNING_PRIVATE_KEY: ${{ secrets.TAURI_SIGNING_PRIVATE_KEY }}
          TAURI_SIGNING_PRIVATE_KEY_PASSWORD: ${{ secrets.TAURI_SIGNING_PRIVATE_KEY_PASSWORD }}
        with:
          includeUpdaterJson: true
          ...
```
releaseBody 加一行：「從這版起 app 會自己檢查更新；v0.1.0 請手動下載一次。」

**Step 2: secrets**

```bash
gh secret set TAURI_SIGNING_PRIVATE_KEY < ~/.tauri/haul.key
gh secret set TAURI_SIGNING_PRIVATE_KEY_PASSWORD --body ""
gh secret list
```

**Step 3: Commit**

```bash
git add .github/workflows/release.yml
git commit -m "發布：release 附 updater 簽章與 latest.json"
```

---

## Task 8: README 中英版

**Files:**
- Rewrite: `README.md`（英文）
- Create: `README.zh-TW.md`（中文）
- Modify: `.claude/skills/haul/SKILL.md`（若有列 `--json` 欄位，加 `thumb`）

各約 100 行，結構相同：

1. 標題 + 一句話 + 語言連結（`English | [繁體中文](README.zh-TW.md)`）
2. Install：Releases 表格（macOS universal dmg / Windows exe / CLI）、首次放行、首次啟動下載 yt-dlp+ffmpeg
3. GUI：三句（貼連結、模式、完成點一下播放；縮圖；設定；自我更新）
4. CLI：現有指令區塊（含 `-i`、`--browser`、`record`）、離開碼表、驗證等級表、`--json` 一行範例
5. For AI agents：skill 位置、`haul logs --json`
6. How it works：那張小流程圖 + 一段
7. Build from source：三個指令 + 測試
8. License（看 repo 有沒有 LICENSE；沒有就不寫這節）

**Commit:** `文件：README 中英版`

---

## Task 9: 掃描、轉 public、推 main

**Step 1: 掃歷史**

```bash
git log -p --all | grep -nE 'gh[po]_[A-Za-z0-9]{20,}|sk-[A-Za-z0-9]{20,}|AKIA[0-9A-Z]{16}|BEGIN (RSA|OPENSSH|EC) PRIVATE|password\s*[:=]' | head
git log --all --name-only --pretty=format: | sort -u | grep -iE 'cookie|\.key|\.pem|\.env|settings\.json|\.log|history' 
git grep -n '/Users/user' -- ':!docs/plans' | head
```
Expected: 沒有 token／金鑰／cookie 檔；`/Users/user` 只出現在 docs/plans 的實測紀錄（可接受，但若有就順手改成 `~`）。

**Step 2: 轉 public**

```bash
gh repo edit miles990/haul --visibility public --accept-visibility-change-consequences
gh repo view --json visibility
```

**Step 3: 推**

```bash
git push origin main
```

**Step 4:** 回報：release 尚未發（version 仍 0.1.0）；發版時 `tauri.conf.json` 與 `Cargo.toml` 的 version 改 0.2.0、打 tag `v0.2.0`、把 draft release 發布，`latest.json` 才會生效。
