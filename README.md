# SunoDL

把 Suno 上的歌抓到本機的桌面小工具。貼連結、排隊、下載，每個檔案都驗證過能播才留下。

macOS（Intel / Apple Silicon）與 Windows 皆可執行。

## 安裝

到 [Releases](../../releases) 下載：

| 平台 | 檔案 |
| --- | --- |
| macOS | `SunoDL_x.y.z_universal.dmg` — 一份通用，Intel 與 Apple Silicon 都是原生執行 |
| Windows | `SunoDL_x.y.z_x64-setup.exe` |

沒有做程式碼簽章，首次開啟需要放行一次：

- **macOS** — 「系統設定 → 隱私權與安全性」按「仍要打開」
- **Windows** — SmartScreen 出現時選「更多資訊 → 仍要執行」

## 用法

1. 開啟後把 Suno 連結貼進上方欄位，一行一個（也可以直接把連結拖進視窗）
2. 按「加入佇列」或 <kbd>⌘</kbd>/<kbd>Ctrl</kbd> + <kbd>Enter</kbd>
3. 檔案存到 `~/Music/SunoDL`，按底部「開啟資料夾」直接跳過去

單曲連結（`suno.com/song/…`）最直接。清單頁或個人頁也可以貼，工具會把頁面裡的歌曲抓出來逐首排隊——但 Suno 若把那一頁做成純前端渲染，就抓不到，這時候改貼單曲連結。

## 運作方式

不開瀏覽器、不錄音、不轉檔。Suno 的音訊在 CDN 上就是一個完整的 mp3，所以直接取原檔——**位元完全相同**，一首 3 分鐘的歌大約 1 到 2 秒，而不是等它播完 3 分鐘。

```
連結 ─→ 解析歌曲 id ─→ 抓歌名 ─→ 串流下載 ─→ 三道驗證 ─→ 存檔
                                     │              │
                              64KiB 一塊       沒過就刪掉
                            記憶體恆定不隨檔案長度成長
```

### 三道驗證

「抓下來的一定要能播」不是靠祈禱，是流程保證的。檔案先落成 `.part`，三關全過才改名進資料夾：

1. **位元組數對得上 `Content-Length`** — 抓截斷最可靠的方式
2. **完整解碼一遍** — 用 [symphonia](https://github.com/pdeljanov/Symphonia) 純 Rust 解碼，不需要裝 ffmpeg。認不出格式或解碼錯誤過多就判定壞檔
3. **不是整首無聲** — RMS 低於 -70 dB 代表拿到的是空殼，不是音樂

任一關沒過就刪檔並在介面上寫明原因，不會默默留下一個放不出聲音的檔案。

### 資源用量

| | |
| --- | --- |
| 記憶體 | 約 100 MB（大半是系統 WebView，非本程式） |
| 下載緩衝 | 64 KiB × 3 條，與檔案大小無關 |
| 同時下載 | 3 首 |
| 同時驗證 | 2 首（解碼吃 CPU，刻意壓低以免跟其他程式搶） |

## 從原始碼建置

只需要 Rust，不需要 Node/npm——前端就是一支手寫的 `ui/index.html`。

```bash
cargo install tauri-cli --version "^2" --locked

cargo tauri dev      # 開發模式
cargo tauri build    # 打包安裝檔
```

macOS 上要出 Universal 2：

```bash
rustup target add aarch64-apple-darwin x86_64-apple-darwin
cargo tauri build --target universal-apple-darwin
```

跑測試：

```bash
cargo test --manifest-path src-tauri/Cargo.toml
```

## 發布

推一個 `v` 開頭的 tag，CI 會在 macOS 與 Windows 上各建一份並開一個草稿 release：

```bash
git tag v0.1.0 && git push origin v0.1.0
```

## 限制

- 只認得 Suno。其他網站的音訊沒有處理
- 私人或已下架的歌會回 403/404，介面上會寫明
- 走 DRM（Widevine EME）保護的串流服務本工具完全不處理，也不打算處理

## 專案結構

```
ui/index.html          前端（單檔，無建置步驟、無外部字體）
src-tauri/src/
  main.rs              視窗、佇列、限流、檔名消毒
  song.rs              連結解析、串流下載
  verify.rs            三道驗證閘門
.github/workflows/     ci（fmt + clippy + test）、release（雙平台打包）
```
