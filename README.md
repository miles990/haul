# Haul

貼連結、排隊、下載。影片、音樂、串流都吃，每個檔案都驗證過能播才留下。

有圖形介面，也有給腳本與 AI agent 用的命令列。macOS（Intel / Apple Silicon）與 Windows 皆可執行。

## 安裝

到 [Releases](../../releases) 下載：

| 平台 | GUI | CLI |
| --- | --- | --- |
| macOS | `Haul_x.y.z_universal.dmg`（Intel 與 Apple Silicon 通用） | `haul-universal-apple-darwin` |

v0.1.0 的 CLI 資產是 `haul-aarch64-apple-darwin`（僅 Apple Silicon）。當時的
workflow 只建了主機架構，已修正，下個版本起會是真正的 universal。
| Windows | `Haul_x.y.z_x64-setup.exe` | `haul-x86_64-pc-windows-msvc.exe` |

沒有做程式碼簽章，首次開啟需要放行一次：

- **macOS** — 「系統設定 → 隱私權與安全性」按「仍要打開」
- **Windows** — SmartScreen 出現時選「更多資訊 → 仍要執行」

**首次啟動會自動下載 yt-dlp 與 ffmpeg**（約 80 MB，存在 app 資料夾）。GUI 與 CLI 共用同一份，不會各抓一次。

## GUI

貼連結（或直接拖進視窗），選「影片」或「只要聲音」，按加入佇列。檔案存到 `~/Downloads/Haul`。

完成的項目**點一下就用系統播放器開啟**。歷史會保留，重開 app 仍看得到。

需要登入才看得到的內容，在輸入欄旁邊選「Chrome 的登入」（或 Firefox、Safari…），
Haul 會借用那個瀏覽器已經有的登入狀態。詳見下面的「登入」。

## CLI

```bash
haul <網址>...                # 下載影片
haul -a <網址>...             # 只要聲音（抽原始音軌，不重新編碼）
haul -i <網址>...             # 只要封面圖／縮圖
haul -q 1080 <網址>           # 畫質上限，避免一支 4K 就吃掉幾 GB
haul -o <資料夾> <網址>       # 指定輸出位置
haul --any <網址>             # 連網頁本身也存（預設拒絕，見下）
haul --cookies chrome <網址>  # 需要登入的內容，借用瀏覽器已有的登入狀態
haul --browser <網址>         # 前三層抓不到時，開 Chrome 把頁面跑起來攔截媒體請求
haul record <網址>            # 錄製分頁的畫面＋聲音（連檔案都沒有時用；-a 只錄聲音）
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
| 瀏覽器 | 媒體網址只在跑起來的頁面裡才出現的站 | **選配**，需要 Chrome / Chromium / Edge / Brave，而且要使用者明確要求 |

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

補齊剩下的很簡單：**重跑同一個網址即可**，已經下載好的會標成 `existing` 跳過，
不會重複下載。真的想重抓就加 `--overwrite`。

### 瀏覽器（選配）

有些頁面的影片網址是 JS 在執行期才組出來的——`<video>` 標籤是空的、播放器
按了才去要清單、或前面擋著一道 JS 挑戰。前三層都不執行 JS，所以看不到。
第四層讓一個**真的 Chrome** 把頁面跑起來，攔截它發出的請求，把媒體的網址
連同原始 header（Referer、Cookie、User-Agent）交回引擎，下載與驗證一個都不跳。

```bash
haul --browser https://example.com/player/123
```

GUI 上是萃取失敗的項目列出現「用瀏覽器抓」。兩邊語意一樣：**瀏覽器是使用者
明確要求的後備，不是預設**——貼錯網址不該彈一個 Chrome 視窗出來。也只有萃取類的
失敗才會提供；401 是身分問題、429 是限流、驗證失敗是內容問題，開瀏覽器救不了。

Haul 啟動的是**獨立的 Chrome 實例**，profile 放在 app 資料夾的 `browser/`，
跟你平常用的 Chrome 無關。原因是 Chrome 136 起禁止對預設 profile 開 remote
debugging，接管使用者正在跑的 Chrome 已不可能。這個 profile 會保留，在裡面登入過
的站下次直接有；這次帶了 `--cookies` 的話，導向前會先把匯出的 cookie 灌進去。

偵測到的候選會自動挑一個：串流清單（m3u8 / mpd）優先——它包含所有畫質而且
yt-dlp 會處理合併；否則單檔取最大的；小於 10 KB 的是探測不是內容。其餘候選在
GUI 上列出來，「抓這個」會新增一個項目去抓它。`--json` 會先印一行
`{"event":"candidates", …}` 讓 agent 看得到全部。

停止觀察的規則：第一個清單出現後再等 3 秒收尾；單檔候選 5 秒內沒有新的就停；
整體 60 秒。頁面要按播放才會載入的，就在視窗裡按。

看到分段（`.ts` / `.m4s`）卻沒有清單，代表清單是 JS 自己組的，這種抓不到原檔；
錯誤訊息會明講，那是錄製的範圍。

DevTools 只綁 127.0.0.1、port 由 Chrome 隨機挑、工作結束就關掉。session 期間
本機其他程序理論上連得進這個瀏覽器；接受這個代價是因為 Rust 在 Windows 上沒辦法
乾淨地用 pipe 取代 port。

### 錄製

有些內容根本不是檔案：JS 自己組 segment 的串流、WebRTC 通話、DRM 播放器。
沒有網址可抓，只能**錄**。`haul record` 用 Haul 的 Chrome 把頁面跑起來，
錄下那個分頁的畫面與聲音：

```bash
haul record https://example.com/live/room     # 畫面＋聲音
haul record -a https://example.com/live/room   # 只要聲音
haul record --max 30m <網址>                    # 上限（預設 3h）
```

分頁自己擷取自己（`getDisplayMedia({preferCurrentTab})`，Chrome 帶
`--auto-accept-this-tab-capture` 所以不跳選擇框），**只錄那一個分頁的聲音**，
不會混到系統通知或你另一邊放的音樂，也不需要 macOS 的螢幕錄影權限或虛擬音訊裝置。
Chrome 自己的「此分頁正在分享」藍條會出現——那是誠實的訊號，按它也能停。

**要錄的內容得在那個瀏覽器視窗裡播放**。錄製是即時的：3 分鐘的片要錄 3 分鐘。
停止有三種，先到先贏：按 Ctrl-C（CLI）或停止鈕（GUI）；頁面上的媒體全部播完且
5 秒內沒有新的開始；到達上限。停止後 ffmpeg 轉封裝——影片出 **mp4**（h264 直接
copy、其餘重編）、只要聲音出 **m4a**，因為 macOS 的系統播放器不吃 WebM。

錄出來的檔案照樣過**完整的 `media` 驗證閘門**（解碼 + 無聲偵測），錄到一片靜音會被
抓出來。歷史與 `--json` 上這種項目帶 `source: "recording"`，檔名加「（錄製）」——
「能不能播」與「是不是原檔」是兩個問題，分開講。`verified` 仍誠實報 `media`。

限制：跨網域 iframe 裡的播放器 Haul 看不到，那種「播完自動停」失效，只能手動停或等
上限。**DRM 內容錄出來是黑畫面**（Chrome 的保護管線不給擷取），這是預期行為，
Haul 不會試著繞。

### 登入

需要登入才看得到的內容，加 `--cookies <瀏覽器>`（GUI 是輸入欄旁的下拉選單）：

```bash
haul --cookies chrome https://example.com/private/video
```

**Haul 不碰帳號密碼。** 需要登入時，在你自己的瀏覽器裡登入——那裡看得到網址列、
用得到密碼管理器、走得完 2FA——Haul 事後只讀 cookie。刻意不在 app 裡做登入畫面，
帳密不該經過我們的視窗。

三條解析路徑都吃得到同一份登入狀態：

| 路徑 | 做法 |
| --- | --- |
| yt-dlp | 直接傳 `--cookies-from-browser`，解密瀏覽器 cookie 庫由它負責（跨瀏覽器跨平台都維護著） |
| gallery-dl | 吃 yt-dlp 匯出的 Netscape cookie 檔 |
| 直接抓取 | 從同一份檔案挑出該主機的 cookie 組成 `Cookie` 標頭 |

自己去解 Chrome 在 macOS keychain 裡的加密太脆弱，所以三條路徑的來源都是 yt-dlp。
匯出的檔案放在 app 資料夾的 `bin/cookies.txt`，權限鎖成 600，換瀏覽器就重新匯出。

支援 brave、chrome、chromium、edge、firefox、opera、safari、vivaldi、whale；
yt-dlp 的 `chrome:Profile 1`（指定 profile）或 `chrome:/path/to/profile`（指定 profile
目錄）寫法也可以，字串原樣交給 yt-dlp。

遇到 401 / 403 時，Haul 會在系統瀏覽器**開一次**該網址的登入頁
（整個 session 只開一次——一個 200 張的圖庫全部 401 時不該彈 200 個分頁），
登入後帶 `--cookies` 重跑同一個指令即可。

開始處理登入身分之後，紀錄裡的網址會把 `token`、`sig`、`session` 這類看起來像
機密的查詢參數遮掉，貼進 issue 不會外洩簽章。

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
- 分段串流沒有清單（JS 自己組 segment 的 MSE）抓不到原檔；WebRTC 通話根本沒有檔案。
  這兩種下不了，只能用 `haul record` 錄畫面與聲音（見下）
- 需要登入的內容要靠 `--cookies` 借用瀏覽器的登入狀態；沒開過該瀏覽器、或瀏覽器
  沒登入過該站，一樣抓不到
- 首次啟動需要網路取得 yt-dlp 與 ffmpeg

## 專案結構

```
core/          haul-core：下載引擎。不知道 UI 的存在，透過 Sink 回呼送事件
  engine.rs      佇列、限流、驗證流程、檔名處理、歷史
  extract.rs     驅動 yt-dlp
  direct.rs      yt-dlp 拒絕時的後備（裸媒體連結、Suno）
  cookies.rs     借用瀏覽器登入狀態：由 yt-dlp 匯出，分給另外兩條路徑
  browser/       第四條路徑：Haul 自己的 Chrome
    chrome.rs      找可執行檔、啟動、讀 DevToolsActivePort
    cdp.rs         最小 CDP 客戶端（JSON over WebSocket）
    sniff.rs       從網路事件挑媒體候選、計分、停止規則
    record.rs      錄製：分頁擷取、停止條件、轉封裝成 mp4 / m4a
  tools.rs       取得與更新 yt-dlp / ffmpeg
  verify.rs      驗證閘門
  log.rs         執行紀錄與輪替
cli/           haul：命令列外殼，把事件印成 NDJSON
src-tauri/     haul-gui：圖形外殼，把事件轉成 Tauri event
ui/index.html  前端（單檔，無建置步驟、無外部字體）
.claude/skills/haul/   給 AI agent 的使用說明
```

GUI 與 CLI 都只是同一個引擎的外殼，行為不會分岔。
