# Haul

[English](README.md) | 繁體中文

貼連結、排隊、下載。影片、音樂、圖片、串流都吃，每個檔案都驗證過能播才留下。

有桌面 app（macOS Intel／Apple Silicon、Windows），也有給腳本與 AI agent 用的
命令列。萃取交給 [yt-dlp](https://github.com/yt-dlp/yt-dlp)（約 1800 個站），
Haul 負責佇列、限流、驗證與檔案管理。

## 安裝

到 [Releases](../../releases) 下載：

| 平台 | App | CLI |
| --- | --- | --- |
| macOS | `Haul_x.y.z_universal.dmg` | `haul-universal-apple-darwin` |
| Windows | `Haul_x.y.z_x64-setup.exe` | `haul-x86_64-pc-windows-msvc.exe` |

沒有做程式碼簽章，首次開啟需要放行一次：

- **macOS** — 系統設定 → 隱私權與安全性 → 「仍要打開」
- **Windows** — SmartScreen → 「更多資訊」→「仍要執行」

首次啟動會自動下載 yt-dlp 與 ffmpeg（約 80 MB）到 app 資料夾，GUI 與 CLI 共用一份。
從 v0.2.0 起 app 會自己到 GitHub Releases 檢查更新並就地安裝（設定 → 檢查 Haul 更新）。

## App

貼連結（或直接拖進視窗），選**影片**、**只要聲音**或**圖片**，按加入佇列。
檔案存到 `~/Downloads/Haul`。

- **佇列**與**已完成**是兩個分頁。每一列都有縮圖（影片抽一格、音樂用封面、圖片
  就是圖片本身）和來源網址——網址點一下就複製。
- 完成的項目**點一下就用系統播放器開啟**；hover 有「在 Finder 顯示」。歷史重開仍在。
- 失敗的項目可以**重試**；萃取失敗的還能**用瀏覽器抓**（真的 Chrome 把頁面跑起來，
  Haul 攔截媒體請求）或**改用錄製**（連檔案都沒有時，錄分頁的畫面與聲音）。
- 按 **×** 移除一列。移除還沒完成的會取消下載，會先問你。
- 清單、圖庫、網頁展開出來的檔案收在來源底下，點來源那列展開收起。
- ⚙ **設定**：介面語言（繁體中文／English）、輸出資料夾、畫質上限、同時下載數、
  登入來源（借用瀏覽器的登入狀態，Haul 不碰帳密）、瀏覽器路徑、錄製上限、完成提示音。

## CLI

```bash
haul <網址>...                # 下載影片
haul -a <網址>...             # 只要聲音（抽原始音軌，不重新編碼）
haul -i <網址>...             # 只要圖片：影片封面、圖片網址、或一般網頁上的所有圖
haul -q 1080 <網址>           # 畫質上限
haul -o <資料夾> <網址>       # 指定輸出位置
haul --any <網址>             # 連網頁本身也存（預設拒絕，見下）
haul --cookies chrome <網址>  # 需要登入的內容
haul --browser <網址>         # 前幾層抓不到時，開真的 Chrome 把頁面跑起來
haul record <網址>            # 錄製分頁的畫面＋聲音（-a 只錄聲音）
haul status                   # 列出歷史
haul logs                     # 執行紀錄（診斷失敗用）
haul update                   # 更新 yt-dlp
```

播放清單、頻道、個人頁會自動展開成一項一項。圖片、PDF、壓縮檔等一般檔案也吃得下。

**離開碼就是答案**——這是 Haul 相對於直接呼叫 yt-dlp 的全部價值：

| 離開碼 | 意思 |
| --- | --- |
| `0` | 每一項都下載完成，且通過該型別能做到的最強檢查 |
| `1` | 有項目失敗 |
| `2` | 用法錯誤，或準備 yt-dlp / ffmpeg 失敗 |

不同型別能做到的驗證強度差很多，所以每一項會回報自己通過了哪一級：

| 等級 | 對象 | 做法 |
| --- | --- | --- |
| `media` | 音訊、影片 | 完整解碼 + 無聲偵測 + 影片抽樣 |
| `image` | 圖片 | ffmpeg 解出一張 |
| `json` | JSON | 真的 parse 一遍，半截的回應會被抓出來 |
| `archive` | PDF、ZIP | magic bytes + 結尾簽章 |
| `text` | 純文字 | 合法 UTF-8 且非空 |
| `integrity` | 其他 | 只比對 Content-Length |

**`text/html` 預設不算檔案**——影片萃取失敗卻悄悄存下一頁 HTML，比失敗更糟。
真的要存網頁才加 `--any`。

`--json` 讓每個狀態變化印成一行 NDJSON 到 stdout（人類看的進度走 stderr）：

```bash
haul --json <網址> | jq -r 'select(.status=="done") | .path'
```

### 給 AI agent 用

`.claude/skills/haul/` 是一份 Claude Code skill，教 agent 何時該用、怎麼讀 NDJSON、
失敗了該做什麼（例如站點改版時先 `haul update` 再重試）。連到全域：

```bash
ln -s "$PWD/.claude/skills/haul" ~/.claude/skills/haul
```

執行紀錄在 `~/Library/Application Support/com.haul.desktop/logs/`（NDJSON，自動輪替）；
`haul status` 讀的是輸出資料夾裡的 `.haul-history.jsonl`——沒有 daemon、沒有 port。

## 運作方式

```
連結 ─→ 解析 ─→ 展開清單 ─→ 逐項下載 ─→ 驗證 ─→ 存檔
          │                                  │
  yt-dlp → gallery-dl → 直接抓取 → 瀏覽器     沒過就刪掉
```

解析分層：yt-dlp 負責影音站；[gallery-dl](https://github.com/mikf/gallery-dl)
（選配，`pipx install gallery-dl`）負責圖庫；直接抓取負責裸檔案連結，圖片模式下
也負責一般網頁上的圖；媒體網址只在跑起來的頁面裡才出現時交給真的 Chrome（選配）；
錄製是最後一條路。

驗證在程序內解碼音訊（symphonia，它不認得的編碼退回 ffmpeg），影片用 ffmpeg 在開頭、
中間、結尾各抽幾格。沒過的檔案會刪掉並寫明原因。設計筆記在 `docs/plans/`。

## 從原始碼建置

只需要 Rust——前端是一支手寫的 `ui/index.html`。

```bash
cargo build --release --workspace          # CLI 在 target/release/haul
cargo install tauri-cli --version "^2" --locked
cd src-tauri && cargo tauri build          # 桌面安裝檔
cargo test --workspace
```

需要真實網路或素材的測試由環境變數開啟（`HAUL_TEST_MEDIA`、`HAUL_TEST_NET`、
`HAUL_TEST_CHROME`），沒設就跳過。

## 授權

[MIT](LICENSE)。yt-dlp、ffmpeg、gallery-dl 是各自獨立的專案、各有自己的授權；
Haul 在首次啟動時下載它們，不打包進安裝檔。
