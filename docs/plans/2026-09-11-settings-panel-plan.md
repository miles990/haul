# 設定面板 實作計畫（第 3 期）

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** 把逐漸變多的設定收進一個齒輪打開的面板（不是頁面），存進 `settings.json` 跨啟動保留；再加「佇列全部完成時播放提示音」（內建或自訂音檔）。

**Architecture:** 新增 `core/src/settings.rs`（serde 結構 + 讀寫 `<app>/settings.json`，欄位都有預設值，向前相容）。GUI 啟動時載入設定建 `Config`；面板改的設定存回檔案，能即時套用的即時套用（cookie 來源、預設畫質、輸出資料夾、瀏覽器路徑、錄製上限），同時下載數標「重新啟動後生效」（semaphore 不能安全縮小）。提示音是前端 Web Audio（內建）或後端讀檔回傳 base64 前端 `decodeAudioData`（自訂），選檔時走 `media` 驗證閘門。設計見 `docs/plans/2026-09-11-browser-layer-design.md`。

**Tech Stack:** serde / serde_json；Tauri 2 指令；單檔 HTML 前端 + Web Audio；原生檔案選擇器走 osascript（macOS）/ PowerShell（Windows），不引入 dialog plugin，延續專案「shell out 到 open/xdg-open」的作法。

**慣例：** 同前兩期。`cargo test -p haul-core <名稱>`；一任務一 commit；註解寫「為什麼」。

**明講的取捨：**
- **同時下載數**與**輸出資料夾**改了要重新啟動才生效。前者的 semaphore 不能安全縮小；後者的 staging 暫存、歷史檔、以及「同檔案系統才能 rename」都在引擎建構時綁定 out_dir，要真正即時換需要一併搬 staging 與歷史、還要處理跨磁碟 rename——那是比面板大得多的改動，不在這一期。面板上這兩項標「重新啟動後生效」。
- **即時生效**的是：登入來源、預設畫質、錄製上限、瀏覽器路徑、提示音。畫質是每次 add 帶的、cookie 已有 setter，錄製上限與瀏覽器路徑加 setter（在錄製／開瀏覽器時才讀，不碰 staging）。

---

### Task 1: settings.rs — 結構、預設、讀寫

**Files:**
- Create: `core/src/settings.rs`
- Modify: `core/src/lib.rs`

**Step 1: 寫失敗的測試**

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_file_gives_defaults() {
        let s = Settings::load(std::path::Path::new("/nope/settings.json"));
        assert_eq!(s.concurrency, 3);
        assert!(s.cookies_from.is_none());
        assert!(!s.chime.enabled);
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
        assert_eq!(back.chime.file.as_deref(), Some("/tmp/ding.mp3"));
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
```

**Step 2: 跑測試確認失敗**

Run: `cargo test -p haul-core settings`
Expected: 編譯錯誤。

**Step 3: 實作**

```rust
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
        Self { enabled: true, file: None }
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
```

`lib.rs` 加 `pub mod settings;`。

**Step 4: 跑測試、Commit**

Run: `cargo test -p haul-core settings`
Expected: 4 passed。

```bash
git add core/src/settings.rs core/src/lib.rs
git commit -m "設定：settings.json 的結構、預設值與讀寫"
```

---

### Task 2: engine — record_max / browser_path 執行期可改

**Files:**
- Modify: `core/src/engine.rs`

`cookies_from` 已經是 runtime setter 的樣板。`record_max` 與 `browser_path` 在錄製／開瀏覽器時才讀，加同樣的 Mutex setter；`out_dir` **不**動（見上面的取捨，改了重新啟動生效）。

**Step 1: 寫失敗的測試**

```rust
    #[test]
    fn record_max_and_browser_path_are_runtime_settable() {
        let dir = std::env::temp_dir().join("haul-engine-rt");
        let eng = Engine::new(
            Config::new(dir, default_bin_dir()),
            std::sync::Arc::new(|_| {}),
        )
        .unwrap();
        eng.set_record_max(600);
        assert_eq!(eng.record_max().as_secs(), 600);
        eng.set_browser_path(Some(std::path::PathBuf::from("/x/chrome")));
        assert_eq!(eng.browser_path(), Some(std::path::PathBuf::from("/x/chrome")));
    }
```

**Step 2: 跑測試確認失敗**

Run: `cargo test -p haul-core record_max_and_browser`
Expected: 編譯錯誤。

**Step 3: 實作**

`Engine` 加欄位：

```rust
    /// 執行期可改的錄製上限與瀏覽器路徑（同 cookies_from 的作法）。
    /// 在錄製／開瀏覽器時才讀，不碰 staging，所以能安全即時換。
    record_max: Mutex<Duration>,
    browser_path: Mutex<Option<PathBuf>>,
```

`new()` 初始化 `record_max: Mutex::new(cfg.record_max)`、`browser_path: Mutex::new(cfg.browser_path.clone())`。加：

```rust
    pub fn record_max(&self) -> Duration {
        *self.record_max.lock().unwrap()
    }
    pub fn set_record_max(&self, secs: u64) {
        *self.record_max.lock().unwrap() = Duration::from_secs(secs.max(1));
    }
    pub fn browser_path(&self) -> Option<PathBuf> {
        self.browser_path.lock().unwrap().clone()
    }
    pub fn set_browser_path(&self, path: Option<PathBuf>) {
        *self.browser_path.lock().unwrap() = path;
    }
```

把 `browser_session()` 裡的 `browser::chrome::find(self.cfg.browser_path.as_deref())` 改成先 `let bp = self.browser_path();` 再 `find(bp.as_deref())`。`run_recording` 裡的 `self.cfg.record_max` 改成 `self.record_max()`。

**Step 4: 建置與測試**

Run: `cargo build --workspace && cargo test -p haul-core`
Expected: 全過。

**Step 5: Commit**

```bash
git add core/src/engine.rs
git commit -m "引擎：record_max / browser_path 執行期可改"
```

---

### Task 3: engine — 從 Settings 建 Config、套用設定

**Files:**
- Modify: `core/src/engine.rs`

**Step 1: 寫失敗的測試**

```rust
    #[test]
    fn config_from_settings_maps_every_field() {
        use crate::settings::Settings;
        let mut s = Settings::default();
        s.concurrency = 5;
        s.max_height = Some(1080);
        s.cookies_from = Some("firefox".into());
        s.record_max_secs = 600;
        let cfg = Config::from_settings(&s, std::path::PathBuf::from("/out"), default_bin_dir());
        assert_eq!(cfg.max_downloads, 5);
        assert_eq!(cfg.cookies_from.as_deref(), Some("firefox"));
        assert_eq!(cfg.record_max.as_secs(), 600);
    }
```

**Step 2: 跑確認失敗**

Run: `cargo test -p haul-core config_from_settings`

**Step 3: 實作**

```rust
impl Config {
    /// 從使用者設定建 Config。out_dir 與 bin_dir 由外殼決定（設定可覆寫 out_dir）。
    pub fn from_settings(s: &crate::settings::Settings, default_out: PathBuf, bin_dir: PathBuf) -> Self {
        let mut cfg = Self::new(s.out_dir.clone().unwrap_or(default_out), bin_dir);
        cfg.max_downloads = s.concurrency.clamp(1, 8);
        cfg.cookies_from = s.cookies_from.clone();
        cfg.browser_path = s.browser_path.clone();
        cfg.record_max = Duration::from_secs(s.record_max_secs.max(1));
        cfg
    }
}
```

（`max_height` 是每次 add 帶的，不進 Config；GUI 從設定讀預設值填入 add payload。）

**Step 4: 測試、Commit**

```bash
git add core/src/engine.rs
git commit -m "引擎：從 Settings 建 Config"
```

---

### Task 4: GUI 後端 — 載入設定、設定的讀寫指令、提示音、選檔

**Files:**
- Modify: `src-tauri/src/main.rs`

**Step 1: 啟動時用設定建引擎**

`setup` 裡把 `Config::new(...)` 換成：

```rust
let app_dir = default_bin_dir()
    .parent()
    .map(|p| p.to_path_buf())
    .unwrap_or_else(default_bin_dir);
let settings_path = haul_core::settings::Settings::path_in(&app_dir);
let settings = haul_core::settings::Settings::load(&settings_path);
let cfg = Config::from_settings(&settings, default_out_dir(), default_bin_dir());
```

把 `settings_path` 存進 Tauri 的 state（`app.manage(settings_path)`），指令要用。

**Step 2: 指令**

```rust
/// 讀目前設定（給面板初始化）
#[tauri::command]
fn get_settings(path: State<'_, PathBuf>) -> Settings {
    Settings::load(&path)
}

/// 存設定並套用能即時套用的部分。同時下載數要重新啟動才生效。
#[tauri::command]
fn save_settings(app: AppHandle, path: State<'_, PathBuf>, settings: Settings) -> Result<(), String> {
    settings.save(&path).map_err(|e| e.to_string())?;
    let eng = engine(&app);
    eng.set_cookies_from(settings.cookies_from.clone());
    eng.set_record_max(settings.record_max_secs);
    eng.set_browser_path(settings.browser_path.clone());
    // out_dir 與 concurrency 存了但不即時套用：見計畫的取捨，重新啟動才生效
    Ok(())
}

/// 讓使用者用原生對話框挑一個音檔，回傳路徑（取消回 None）。
/// 走系統的選擇器而不是引入 dialog plugin，延續 open/xdg-open 的作法。
#[tauri::command]
fn pick_audio_file() -> Option<String> {
    native_pick_audio()
}

/// 驗證一個音檔真的能解碼（選到壞檔當場知道，不必等佇列跑完）
#[tauri::command]
fn verify_audio(path: String) -> Result<(), String> {
    let p = std::path::PathBuf::from(&path);
    if !p.is_file() {
        return Err("找不到這個檔案".into());
    }
    tauri::async_runtime::block_on(async move {
        tokio::task::spawn_blocking(move || haul_core::verify::verify(&p))
            .await
            .map_err(|e| e.to_string())?
            .map(|_| ())
            .map_err(|e| format!("這個檔案不能當提示音：{e}"))
    })
}

/// 讀音檔的位元組回前端播放（base64）。不開 asset protocol 白名單。
#[tauri::command]
fn read_audio(path: String) -> Result<String, String> {
    use base64::Engine as _;
    let bytes = std::fs::read(&path).map_err(|e| e.to_string())?;
    // 提示音檔案應該很小；設個上限免得有人選了一部電影
    if bytes.len() > 8 * 1024 * 1024 {
        return Err("音檔太大（上限 8 MB）".into());
    }
    Ok(base64::engine::general_purpose::STANDARD.encode(bytes))
}
```

`verify` 要在 `haul_core` 公開（`pub mod verify;` 應已是；確認 `verify::verify` 是 pub）。`base64` 加進 `src-tauri/Cargo.toml`（workspace 已有）。

原生選擇器：

```rust
#[cfg(target_os = "macos")]
fn native_pick_audio() -> Option<String> {
    // AppleScript 的 choose file，限音訊類型
    let script = r#"try
        set f to choose file with prompt "選一個提示音" of type {"mp3","m4a","wav","aiff","aac","ogg"}
        POSIX path of f
    on error
        return ""
    end try"#;
    let out = std::process::Command::new("osascript").args(["-e", script]).output().ok()?;
    let path = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (!path.is_empty()).then_some(path)
}

#[cfg(target_os = "windows")]
fn native_pick_audio() -> Option<String> {
    let ps = r#"Add-Type -AssemblyName System.Windows.Forms
$d = New-Object System.Windows.Forms.OpenFileDialog
$d.Filter = 'Audio|*.mp3;*.m4a;*.wav;*.aac;*.ogg'
if ($d.ShowDialog() -eq 'OK') { Write-Output $d.FileName }"#;
    let out = std::process::Command::new("powershell")
        .args(["-NoProfile", "-Command", ps]).output().ok()?;
    let path = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (!path.is_empty()).then_some(path)
}

#[cfg(all(not(target_os = "macos"), not(target_os = "windows")))]
fn native_pick_audio() -> Option<String> {
    None // Linux 桌面環境太雜，先讓使用者自己貼路徑
}
```

註冊全部新指令到 `invoke_handler`；`use haul_core::settings::Settings;`。

**Step 3: 建置**

Run: `cargo build --workspace`
Expected: 成功（`out_dir()` 仍回 `&Path`，Tauri 指令不變）。

**Step 4: Commit**

```bash
git add src-tauri/src/main.rs src-tauri/Cargo.toml
git commit -m "GUI 後端：載入設定、讀寫指令、提示音驗證與選檔"
```

---

### Task 5: GUI 前端 — 設定面板

**Files:**
- Modify: `ui/index.html`

**Step 1: 面板 HTML 與樣式**

- 頁尾：移除「更新 yt-dlp」按鈕（搬進面板），加「⚙」按鈕。頁尾剩：dest、tally、⚙、開啟資料夾、清掉已完成。
- 輸入欄旁的 `#cookies` 下拉**移除**（搬進面板的登入群組）；`add` 呼叫改成從設定讀 `cookies` 與 `quality`。
- 面板用 `<dialog>` 或一個 `position:fixed` 的側滑層（用 `<dialog>` 最省事，原生 backdrop 與 esc 關閉）。內容分群組：

```html
<dialog id="settings">
  <form method="dialog" class="sheet">
    <h2>設定</h2>

    <fieldset><legend>下載</legend>
      <label>輸出資料夾 <span id="set-out" class="path"></span>
        <button type="button" id="set-out-pick">選擇…</button></label>
      <label>畫質上限
        <select id="set-quality">
          <option value="">不設限</option><option value="2160">2160</option>
          <option value="1440">1440</option><option value="1080">1080</option>
          <option value="720">720</option>
        </select></label>
      <label>同時下載數
        <input type="number" id="set-concurrency" min="1" max="8">
        <span class="note">重新啟動後生效</span></label>
      <label><input type="checkbox" id="set-chime-on"> 佇列完成時播放提示音</label>
      <div class="chime-file" id="chime-file-row">
        <span id="set-chime-file" class="path">內建提示音</span>
        <button type="button" id="chime-pick">選檔…</button>
        <button type="button" id="chime-clear">改回內建</button>
        <button type="button" id="chime-test">試聽</button>
      </div>
    </fieldset>

    <fieldset><legend>登入</legend>
      <label>登入狀態來源
        <select id="set-cookies">
          <option value="">不用登入</option><option value="chrome">Chrome</option>
          <option value="firefox">Firefox</option><option value="safari">Safari</option>
          <option value="edge">Edge</option><option value="brave">Brave</option>
        </select></label>
      <p class="note">Haul 不碰帳密，只借用瀏覽器已有的登入狀態。</p>
    </fieldset>

    <fieldset><legend>瀏覽器 / 錄製</legend>
      <label>瀏覽器路徑 <span id="set-browser" class="path">自動偵測</span>
        <button type="button" id="browser-pick">選擇…</button>
        <button type="button" id="browser-clear">自動</button></label>
      <label>錄製上限（分鐘）<input type="number" id="set-record-max" min="1"></label>
    </fieldset>

    <fieldset><legend>工具</legend>
      <button type="button" id="update">更新 yt-dlp</button>
      <span id="update-status" class="note"></span>
    </fieldset>

    <div class="actions">
      <button type="button" id="set-cancel">取消</button>
      <button type="button" id="set-save" class="primary">儲存</button>
    </div>
  </form>
</dialog>
```

樣式跟現有暗色系一致（`var(--panel)`、`var(--edge)`、`var(--ink)`…）；面板寬度 min(520px, 92vw)，欄位垂直排列。

**Step 2: 前端邏輯**

- 開啟：`⚙` → `dialog.showModal()`，先 `invoke('get_settings')` 填入各欄位。
- 選檔：`chime-pick` / `browser-pick` → `invoke('pick_audio_file')`（瀏覽器路徑另一個指令或共用，Linux 回 null 就提示手動貼）；chime 選到後 `invoke('verify_audio', {path})`，過了才顯示路徑、不過 `say` 錯誤。
- 試聽：讀 `read_audio` → `decodeAudioData` → 播；或內建合成音（見下）。
- 儲存：組 `Settings` 物件 `invoke('save_settings', {settings})`，關面板，更新頁尾 `dest`（out_dir 可能變了 → `invoke('out_dir')` 重讀）。
- `add` 呼叫改用面板存的值：進 app 時 `invoke('get_settings')` 存成 `state`，`go` 送出時帶 `state.cookiesFrom`、`state.maxHeight`。設定存檔後同步更新這份 `state`。

**Step 3: 提示音**

```js
let audioCtx;
function beep() {
  try {
    audioCtx = audioCtx || new (window.AudioContext || window.webkitAudioContext)();
    const now = audioCtx.currentTime;
    for (const [i, f] of [880, 1320].entries()) {   // 兩音，簡短
      const o = audioCtx.createOscillator(), g = audioCtx.createGain();
      o.frequency.value = f; o.connect(g); g.connect(audioCtx.destination);
      const t = now + i * 0.12;
      g.gain.setValueAtTime(0.0001, t);
      g.gain.exponentialRampToValueAtTime(0.2, t + 0.02);
      g.gain.exponentialRampToValueAtTime(0.0001, t + 0.11);
      o.start(t); o.stop(t + 0.12);
    }
  } catch (_) {}
}
async function playChime() {
  if (!settings.chime.enabled) return;
  if (!settings.chime.file) return beep();
  try {
    const b64 = await invoke('read_audio', { path: settings.chime.file });
    const bytes = Uint8Array.from(atob(b64), c => c.charCodeAt(0));
    audioCtx = audioCtx || new (window.AudioContext || window.webkitAudioContext)();
    const buf = await audioCtx.decodeAudioData(bytes.buffer);
    const src = audioCtx.createBufferSource();
    src.buffer = buf; src.connect(audioCtx.destination); src.start();
  } catch (_) { beep(); }   // 檔案不見或解不動就退回內建
}
```

**觸發**：`updateTally` 已算 running/done/failed。加一個模組級 `prevRunning`；當 `prevRunning > 0 && running === 0` 時 `playChime()`。App 剛載入 snapshot 時不要響（初始化時把 `prevRunning` 設成當下值再開始比較）。

**Step 4: jsdom 測試**

`scratchpad/uitest` 新增 `settings.js`：假 `__TAURI__.invoke` 記錄呼叫、`get_settings` 回一組值。驗證：

- 開面板 → 各欄位填入設定值
- 改畫質、同時下載數、勾提示音 → 按儲存 → `save_settings` 帶正確的 `Settings`
- `go` 送出時 `add` 帶面板設定的 `cookies` 與 `quality`
- running 從 1→0 觸發 chime（stub `AudioContext`，斷言被呼叫）；1→0 但 chime.enabled=false 不觸發；初次載入不觸發
- chime 選檔 verify 失敗 → 不採用、顯示錯誤

**Step 5: 建置驗證**

Run: `cargo build --workspace` 後 `node scratchpad/uitest/settings.js ui/index.html`，再跑前兩期的 `run.js`、`rec.js` 確認沒回歸。GUI 實際啟動一次不崩。

**Step 6: Commit**

```bash
git add ui/index.html
git commit -m "GUI：設定面板、佇列完成提示音"
```

---

### Task 6: 文件

**Files:**
- Modify: `README.md`
- Modify: `.claude/skills/haul/SKILL.md`

**README：**
- 「GUI」段：說明齒輪面板收了哪些設定、提示音（內建／自訂）。
- 移除「更新 yt-dlp 在頁尾」的描述若有；「運作方式」的登入段提到來源改在設定面板選（CLI 仍是 `--cookies`）。
- 檔案位置表加 `settings.json`（`<app>/settings.json`）。
- 專案結構加 `settings.rs`。

**SKILL.md：**
- CLI 不受影響（面板是 GUI 專屬）；只在「檔案在哪」加 `settings.json` 一行，並說明 CLI 的旗標只影響單次執行、不讀寫 GUI 的設定。

**Commit：**

```bash
git add README.md .claude/skills/haul/SKILL.md
git commit -m "文件：設定面板"
```

---

## 完成標準

- `cargo test --workspace` 全過；`cargo clippy --workspace` 無警告
- GUI：齒輪開面板、改設定存檔、重開 app 設定還在；登入來源／畫質／錄製上限／瀏覽器路徑即時生效，輸出資料夾與同時下載數標「重新啟動後生效」
- 提示音：佇列全部完成響一次；內建與自訂音檔都可；自訂選到壞檔當場擋下
- 登入來源從輸入欄旁搬進面板後，`add` 仍正確帶 cookie
- jsdom 前端測試（設定、前兩期）全過；GUI 啟動不崩
