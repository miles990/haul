# Haul

貼連結、排隊、下載。影片、音樂、串流都吃，每個檔案都驗證過能播才留下。

有圖形介面，也有給腳本與 AI agent 用的命令列。macOS（Intel / Apple Silicon）與 Windows 皆可執行。

## 安裝

到 [Releases](../../releases) 下載：

| 平台 | GUI | CLI |
| --- | --- | --- |
| macOS | `Haul_x.y.z_universal.dmg`（Intel 與 Apple Silicon 通用） | `haul-universal-apple-darwin` |
| Windows | `Haul_x.y.z_x64-setup.exe` | `haul-x86_64-pc-windows-msvc.exe` |

沒有做程式碼簽章，首次開啟需要放行一次：

- **macOS** — 「系統設定 → 隱私權與安全性」按「仍要打開」
- **Windows** — SmartScreen 出現時選「更多資訊 → 仍要執行」

**首次啟動會自動下載 yt-dlp 與 ffmpeg**（約 80 MB，存在 app 資料夾）。GUI 與 CLI 共用同一份，不會各抓一次。

## GUI

貼連結（或直接拖進視窗），選「影片」或「只要聲音」，按加入佇列。檔案存到 `~/Downloads/Haul`。

完成的項目**點一下就用系統播放器開啟**。歷史會保留，重開 app 仍看得到。

## CLI

```bash
haul <網址>...                # 下載影片
haul -a <網址>...             # 只要聲音（抽原始音軌，不重新編碼）
haul -i <網址>...             # 只要封面圖／縮圖
haul -q 1080 <網址>           # 畫質上限，避免一支 4K 就吃掉幾 GB
haul -o <資料夾> <網址>       # 指定輸出位置
haul --any <網址>             # 連網頁本身也存（預設拒絕，見下）
haul status                   # 列出歷史
haul logs                     # 看執行紀錄（診斷失敗用）
haul update                   # 更新 yt-dlp
```

圖片、PDF、壓縮檔等一般檔案的網址也吃得下。

**離開碼就是答案**——這是 Haul 相對於直接呼叫 yt-dlp 的全部價值：

| 離開碼 | 意思 |
| --- | --- |
| `0` | 每一項都下載完成，且通過該型別能做到的最強檢查 |
| `1` | 有項目失敗 |
| `2` | 用法錯誤，或準備 yt-dlp / ffmpeg 失敗 |

不需要自己檢查檔案存不存在或大小對不對，那些閘門已經跑過了。

不同型別能做到的驗證強度差很多，所以每一項會回報自己通過了哪一級，
而不是一律報成「成功」：

| 等級 | 對象 | 做法 |
| --- | --- | --- |
| `media` | 音訊、影片 | 完整解碼 + 無聲偵測 + 影片抽樣 |
| `image` | 圖片 | ffmpeg 解出一張 |
| `json` | JSON | 真的 parse 一遍，半截的回應會被抓出來 |
| `archive` | PDF、ZIP | magic bytes + 結尾簽章，抓截斷 |
| `text` | 純文字 | 合法 UTF-8 且非空 |
| `integrity` | 其他 | 只比對 Content-Length |

**`text/html` 預設不算檔案**，要加 `--any` 才會存。理由是假成功比失敗更糟：
如果 yt-dlp 對某個影片連結暫時失敗，然後「成功」存下那頁 HTML，
使用者會以為抓到了。

`--json` 讓每個狀態變化印成一行 NDJSON 到 stdout（人類看的進度走 stderr，重導向不會混在一起）：

```bash
haul --json <網址> | jq -r 'select(.status=="done") | .path'
```

### 給 AI agent 用

`.claude/skills/haul/` 是一份 Claude Code skill，教 agent 何時該用、怎麼讀 NDJSON、失敗了該做什麼（例如站點改版時先 `haul update` 再重試，而不是直接放棄）。

要在所有專案都能用就連結到全域：

```bash
ln -s "$PWD/.claude/skills/haul" ~/.claude/skills/haul
```

失敗時先看紀錄，那裡有完整原因（介面上只有一行摘要）：

```bash
haul logs --json | jq 'select(.level == "error")'
```

執行紀錄在 `~/Library/Application Support/com.haul.desktop/logs/`，一行一則 NDJSON，
單檔 4 MB 輪替、保留 3 份。紀錄跟著安裝走而不是跟著 `-o` 走——診斷時不必回想當初
輸出到哪個資料夾。

`haul status` 讀的是輸出資料夾裡的 `.haul-history.jsonl`。**不需要 GUI 在跑，也沒有 daemon 或 port——狀態檔本身就是介面。** 那個檔是 append-only 的，所以 GUI 與 CLI 同時跑也不會互相蓋掉紀錄。

## 運作方式

萃取交給 [yt-dlp](https://github.com/yt-dlp/yt-dlp)，Haul 負責佇列、限流、驗證與檔案管理。

```
連結 ─→ 解析 ─→ 展開清單 ─→ 逐項下載 ─→ 驗證 ─→ 存檔
         │                                  │
    yt-dlp 為主                        沒過就刪掉
    拒絕時走直接抓取
```

### 為什麼不自己寫萃取

這支工具原本鎖定單一站點，硬編了 CDN 的網址規則。第一次拿真實連結測試就 403 ——
該站已經改成投遞影音混合的 mp4，舊規則當場失效。

yt-dlp 有約 1800 個站點的 extractor，靠一整個社群追各站改版。與其自己維護一份注定
落後的規則，不如驅動它，並且**讓它能獨立更新**——所以 yt-dlp 不包進安裝檔，而是放在
資料夾裡，隨時可以 `haul update`。

但 yt-dlp 對某些站是**政策性拒絕**（例如 suno.com 會回 `[Liability] This website is not
supported and will not be supported`），所以另有一層直接抓取的後備。後備的站點規則
本質上脆弱，但它壞掉只影響那幾個站，不會拖垮整個工具。

解析鏈一共四層：

| 順序 | 負責 | 缺席時 |
| --- | --- | --- |
| yt-dlp | 約 1800 個影音站 | 必備，首次啟動自動下載 |
| gallery-dl | 圖庫、漫畫、booru | **選配**，沒裝就在錯誤訊息裡說明怎麼裝 |
| 直接抓取 | 裸媒體連結、Suno、一般檔案 | 內建 |

### 圖庫（選配）

裝了 [gallery-dl](https://github.com/mikf/gallery-dl) 之後，圖庫與漫畫頁也能整批抓：

```bash
pipx install gallery-dl
```

它只當**網址萃取器**——負責回答「這頁上有哪些圖」，實際下載仍走 Haul 的佇列、
限流與圖片驗證閘門，跟 yt-dlp 的 `-J` 完全對稱。每個圖庫收進自己的子資料夾。

之所以是選配而非像 yt-dlp 那樣自動下載：gallery-dl 最近的版本都沒有附二進位檔，
只走 PyPI。替使用者自動安裝 Python 套件太越界，在 Windows 上還會變成「要先裝
Python」——正好是自動下載想避開的坑。

需要特殊 Referer 或高度 JS 渲染的站仍然抓不到，那類直接用 gallery-dl 本身。

實測（Wikimedia Commons，222 張）成功 173 張、失敗 49 張，失敗全是對方的節流。
限流嚴格的站點會部分失敗是正常的，離開碼會是 1。

**已知粗糙處**：重跑同一個圖庫不會跳過已下載的檔案，而是產生 `xxx (2).jpg`。
`seen` 刻意不從歷史載入（同一個連結想重抓是合理的），代價就是補齊剩下的
項目時會重複已完成的。目前得自己刪掉重複檔，或換個輸出資料夾。

### 對同一個主機會限速

整批圖庫是對同一台主機連發幾百個請求。實測 223 張圖在沒有限速時只成功 4 張，
其餘全被回 429。所以同主機兩次請求之間至少隔 300ms，遇到 429 或 503 會依
`Retry-After` 退讓重試——這是下載器與爬蟲的差別所在。

### 驗證閘門

「抓下來的一定要能播」是流程保證的。檔案先落在暫存區，過關才搬進正式資料夾：

| 對象 | 做法 |
| --- | --- |
| 位元組數 | 必須與 `Content-Length` 相符——抓截斷最可靠的方式 |
| 音訊 | [symphonia](https://github.com/pdeljanov/Symphonia) 在程序內完整解碼，並確認 RMS 高於 -70 dB |
| 音訊（symphonia 不認得的編碼） | 退回 ffmpeg 裁決。symphonia 沒有 opus 解碼器而 YouTube 常用 webm/opus——沒有這層退路，好檔案會被誤判成壞檔，那比不檢查還糟 |
| 影片 | 在開頭、中間、接近結尾三個時間點各解幾個 frame。完整解一部 1080p 要幾十秒 CPU，抽樣壓到一秒內又足以抓到截斷 |

沒過就刪檔並寫明原因。判斷只看 ffmpeg 的離開碼，不解析它的人類可讀輸出——那個格式會隨版本改。

## 從原始碼建置

只需要 Rust，不需要 Node/npm——前端就是一支手寫的 `ui/index.html`。

```bash
cargo build --release --workspace     # CLI 在 target/release/haul

cargo install tauri-cli --version "^2" --locked
cd src-tauri && cargo tauri build     # GUI 安裝檔
```

macOS 上要出 Universal 2：

```bash
rustup target add aarch64-apple-darwin x86_64-apple-darwin
cd src-tauri && cargo tauri build --target universal-apple-darwin
```

### 測試

```bash
cargo test --workspace
```

有兩個需要真實素材的測試，設了環境變數才會跑（CI 上不設，所以會跳過）：

```bash
# 驗證閘門會接受真實音檔，而不是「什麼都拒絕」還一路綠燈
HAUL_TEST_MEDIA=/path/to/real.mp3 cargo test --workspace

# 端對端：取得工具 → 解析 → 下載 → 驗證。單元測試證明不了這條鏈路。
HAUL_E2E_URL=https://... cargo test --workspace end_to_end -- --nocapture
```

## 發布

推一個 `v` 開頭的 tag，CI 會在 macOS 與 Windows 上各建一份 GUI 安裝檔與 CLI 二進位，
並開一個草稿 release：

```bash
git tag v0.1.0 && git push origin v0.1.0
```

## 限制

- 走 DRM（Widevine EME）保護的串流服務不處理，也不打算處理
- 需要登入才看得到的內容目前沒有帶 cookie，會失敗
- 首次啟動需要網路取得 yt-dlp 與 ffmpeg

## 專案結構

```
core/          haul-core：下載引擎。不知道 UI 的存在，透過 Sink 回呼送事件
  engine.rs      佇列、限流、驗證流程、檔名處理、歷史
  extract.rs     驅動 yt-dlp
  direct.rs      yt-dlp 拒絕時的後備（裸媒體連結、Suno）
  tools.rs       取得與更新 yt-dlp / ffmpeg
  verify.rs      驗證閘門
  log.rs         執行紀錄與輪替
cli/           haul：命令列外殼，把事件印成 NDJSON
src-tauri/     haul-gui：圖形外殼，把事件轉成 Tauri event
ui/index.html  前端（單檔，無建置步驟、無外部字體）
.claude/skills/haul/   給 AI agent 的使用說明
```

GUI 與 CLI 都只是同一個引擎的外殼，行為不會分岔。
