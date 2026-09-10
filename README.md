# Haul

貼連結、排隊、下載。影片、音樂、串流都吃，每個檔案都驗證過能播才留下。

macOS（Intel / Apple Silicon）與 Windows 皆可執行。

## 安裝

到 [Releases](../../releases) 下載：

| 平台 | 檔案 |
| --- | --- |
| macOS | `Haul_x.y.z_universal.dmg` — 一份通用，Intel 與 Apple Silicon 都是原生執行 |
| Windows | `Haul_x.y.z_x64-setup.exe` |

沒有做程式碼簽章，首次開啟需要放行一次：

- **macOS** — 「系統設定 → 隱私權與安全性」按「仍要打開」
- **Windows** — SmartScreen 出現時選「更多資訊 → 仍要執行」

**首次啟動會自動下載 yt-dlp 與 ffmpeg**（約 80 MB，存在 app 的資料夾裡），介面上會顯示進度。之後就不需要網路以外的任何準備。

## 用法

1. 把連結貼進上方欄位，一行一個（也可以直接把連結拖進視窗）
2. 選「影片」或「只要聲音」
3. 按「加入佇列」或 <kbd>⌘</kbd>/<kbd>Ctrl</kbd> + <kbd>Enter</kbd>

檔案存到 `~/Downloads/Haul`，按底部「開啟資料夾」直接跳過去。

單支連結最直接。播放清單、頻道、個人頁也可以貼，Haul 會展開成一項一項排隊。

**「只要聲音」不會重新編碼**：直接取出原始音軌，能複製就複製。從一支影片抽出音訊通常不到一秒，而且位元完全相同。

## 運作方式

萃取交給 [yt-dlp](https://github.com/yt-dlp/yt-dlp)，Haul 負責佇列、限流、驗證與檔案管理。

```
連結 ─→ yt-dlp 解析 ─→ 展開清單 ─→ 逐項下載 ─→ 驗證 ─→ 存檔
                                       │          │
                                  進度回報    沒過就刪掉
```

### 為什麼不自己寫萃取

這支工具原本鎖定單一站點，直接硬編 CDN 的網址規則。第一次拿真實連結測試就 403 ——
該站已經改成投遞影音混合的 mp4，舊規則當場失效。

yt-dlp 有約 1800 個站點的 extractor，靠一整個社群在追各站的改版。與其自己維護一份注定
落後的規則，不如驅動它，並且**讓它能獨立更新**——所以 yt-dlp 不包進安裝檔，而是放在資料
夾裡，介面上有「更新 yt-dlp」按鈕。站點一改版，按一下就跟上，不必等 Haul 重新發布。

### 驗證閘門

「抓下來的一定要能播」是流程保證的，不是靠祈禱。檔案先落在暫存區，過關才搬進正式資料夾：

| 對象 | 做法 |
| --- | --- |
| 音訊 | [symphonia](https://github.com/pdeljanov/Symphonia) 在程序內完整解碼，並確認 RMS 高於 -70 dB（無聲代表拿到空殼） |
| 音訊（symphonia 不認得的編碼） | 退回用 ffmpeg 裁決。symphonia 沒有 opus 解碼器，而 YouTube 常用 webm/opus——沒有這層退路，好檔案會被誤判成壞檔，那比不檢查還糟 |
| 影片 | 在開頭、中間、接近結尾三個時間點各解幾個 frame。完整解一部 1080p 影片要幾十秒 CPU，抽樣壓到一秒內又足以抓到截斷 |

沒過就刪檔，並在介面上寫明原因，不會默默留下一個放不出來的檔案。

判斷只看 ffmpeg 的離開碼，不去解析它的人類可讀輸出——那個格式會隨版本改。

### 資源用量

| | |
| --- | --- |
| 記憶體 | 約 100 MB（大半是系統 WebView，非本程式） |
| 同時下載 | 3 項 |
| 同時驗證 | 2 項（解碼吃 CPU，刻意壓低以免跟其他程式搶） |

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

### 測試

```bash
cargo test --manifest-path src-tauri/Cargo.toml
```

有兩個需要真實素材的測試，設了環境變數才會跑（CI 上不設，所以會跳過）：

```bash
# 驗證閘門會接受真實音檔，而不是「什麼都拒絕」還一路綠燈
HAUL_TEST_MEDIA=/path/to/real.mp3 cargo test --manifest-path src-tauri/Cargo.toml

# 端對端：取得工具 → 解析 → 下載 → 驗證。單元測試證明不了這條鏈路。
HAUL_E2E_URL=https://... cargo test --manifest-path src-tauri/Cargo.toml end_to_end -- --nocapture
HAUL_E2E_AUDIO=1 HAUL_E2E_URL=https://... cargo test --manifest-path src-tauri/Cargo.toml end_to_end -- --nocapture
```

## 發布

推一個 `v` 開頭的 tag，CI 會在 macOS 與 Windows 上各建一份並開一個草稿 release：

```bash
git tag v0.1.0 && git push origin v0.1.0
```

## 限制

- 走 DRM（Widevine EME）保護的串流服務不處理，也不打算處理
- 需要登入才看得到的內容目前沒有帶 cookie，會失敗
- 首次啟動需要網路取得 yt-dlp 與 ffmpeg

## 專案結構

```
ui/index.html          前端（單檔，無建置步驟、無外部字體）
src-tauri/src/
  main.rs              視窗、佇列、限流、檔名消毒
  tools.rs             yt-dlp / ffmpeg 的取得與更新
  extract.rs           驅動 yt-dlp 解析與下載
  verify.rs            驗證閘門
.github/workflows/     ci（fmt + clippy + test）、release（雙平台打包）
```
