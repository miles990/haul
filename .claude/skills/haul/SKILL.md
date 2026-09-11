---
name: haul
description: Use when the user wants to download video, audio, or music from a URL — YouTube, Bilibili, SoundCloud, Bandcamp, Suno, archive.org, Twitter/X, or a direct media link. Also use for "download this video", "抓這個影片", "下載這首歌", "把這個存下來", saving a playlist or channel locally, extracting audio from a video, or checking what was downloaded before. Handles ~1800 sites via yt-dlp, verifies every file actually plays before keeping it.
---

# Haul

貼網址下載影片或音訊，每個檔案都通過驗證閘門才留下。

## 核心：離開碼就是答案

`haul` 跟直接呼叫 `yt-dlp` 的差別只有一個——**它會驗證檔案真的能播才回傳成功**。

| 離開碼 | 意思 |
| --- | --- |
| `0` | 每一項都下載完成、通過驗證。檔案存在且真的能播 |
| `1` | 有項目失敗（stderr 寫明原因） |
| `2` | 用法錯誤，或準備 yt-dlp / ffmpeg 失敗 |

**不要自己去檢查檔案存不存在或大小對不對**，那些 haul 已經做過了。直接看離開碼。

每一項還會回報 `verified` 欄位，說明它通過了哪一級檢查：`media`（完整解碼）、
`image`（解出一張）、`json`（真的 parse 過）、`archive`（magic + 結尾簽章）、
`text`（合法 UTF-8）、`integrity`（只比對 Content-Length）。不同型別能做到的
強度差很多，需要判斷保證有多強時看這個欄位。

圖片、PDF、壓縮檔等一般檔案也吃得下。但 **`text/html` 預設不算檔案**——
存下一頁 HTML 卻報成功，比失敗更糟。真的要存網頁本身才加 `--any`。

## 用法

```bash
haul <網址>...                      # 下載影片
haul -a <網址>...                   # 只要聲音（抽原始音軌，不重新編碼）
haul -i <網址>...                   # 只要封面圖／縮圖
haul -q 1080 <網址>                 # 畫質上限，避免一支 4K 吃掉幾 GB
haul -o /path/to/dir <網址>         # 指定輸出資料夾（預設 ~/Downloads/Haul）
haul -c 5 <網址>...                 # 同時下載 5 個（預設 3）
haul --any <網址>                   # 連網頁本身也存（預設拒絕）
haul --cookies chrome <網址>        # 需要登入的內容，借用瀏覽器已有的登入狀態
haul status                         # 列出歷史
haul logs                           # 看執行紀錄（診斷失敗用）
haul update                         # 更新 yt-dlp
```

播放清單、頻道、個人頁直接貼，haul 會自己展開成一項一項。

## 給程式讀的輸出

`--json` 讓每個狀態變化印成一行 NDJSON 到 **stdout**（人類看的進度走 stderr，所以重導向不會混在一起）：

```bash
haul --json <網址> > events.jsonl
```

```jsonl
{"event":"item","id":1,"status":"downloading","title":"…","bytes":8388608,"total":45568216}
{"event":"item","id":1,"status":"done","title":"…","file":"…​.mp4","path":"/Users/…/x.mp4","secs":222.8}
{"event":"item","id":2,"status":"failed","title":"…","error":"…"}
```

拿剛下載好的檔案路徑：

```bash
haul --json <網址> | jq -r 'select(.status=="done") | .path'
```

## 查歷史

`haul status` 讀的是輸出資料夾裡的 `.haul-history.jsonl`。**不需要 GUI 在跑**，也沒有任何 daemon 或 port——狀態檔本身就是介面。

```bash
haul status --json | jq -r 'select(.status=="done") | .path'   # 所有可播的檔案
haul status --json | jq -r 'select(.status=="failed") | .error' # 失敗原因
```

檔案已經被刪掉的項目會自動從歷史裡濾掉，所以列出來的路徑都是真的還在。

## 失敗了怎麼辦

**第一步永遠是看紀錄**，介面與 stdout 上只有一行摘要，完整原因在這裡：

```bash
haul logs --json | jq 'select(.level == "error")'
haul logs -n 30                                    # 人看的版本
```

紀錄裡的 `input.resolved` 會告訴你走的是 `ytdlp` 還是 `direct` 後備，
`item.failed` 帶完整錯誤字串。看過之後再按下面判斷，**不要直接放棄或改用別的工具**：

1. **錯誤訊息提到 extractor、格式解析、`Unable to extract`** → 站點改版了。跑 `haul update` 讓 yt-dlp 自我更新，然後重試一次。這是最常見的失敗原因。

2. **`[Liability] This website is not supported`** → yt-dlp 對該站是政策性拒絕。haul 有後備路徑（直接抓取，涵蓋 suno.com 與裸媒體連結）；如果後備也沒有對應規則，這個站就是抓不到，據實回報即可。

3. **圖庫或漫畫頁抓不到** → 錯誤訊息會提示安裝 `gallery-dl`（`pipx install gallery-dl`）。裝好後 haul 會自動用它萃取網址，下載與驗證仍由 haul 做。需要特殊 Referer 或高度 JS 渲染的站仍然不行，別硬試。

   圖庫**部分失敗是常態**（實測 222 張成功 173 張，失敗全是對方節流），離開碼會是 1。**要補齊就重跑同一個網址**——已下載好的會標成 `existing` 跳過，不會重複下載。真的要重抓才加 `--overwrite`。

4. **HTTP 401 / 403** → 內容是私人的，或需要登入。加 `--cookies <瀏覽器>`
   （chrome / firefox / safari / edge / brave…）借用使用者瀏覽器已有的登入狀態重試。
   haul 遇到 401 / 403 時會自動在系統瀏覽器開一次該網址的登入頁；**請使用者在那裡登入**
   （不要向使用者索取帳密、也不要代替使用者輸入），登入完成後再帶 `--cookies` 重跑。
   帶了 cookie 仍 401 / 403 就是該帳號本來就看不到，據實回報。

5. **HTTP 429** → 被限流。haul 已經會自動退讓重試（同主機間隔 300ms、依 `Retry-After` 退避），還是失敗就是對方限得很緊，過一陣子再試，不要調高 `-c`。

6. **「驗證未通過」** → 檔案抓下來了但沒通過該型別的檢查。haul 已經把壞檔刪掉了。重試一次；若持續失敗，來源本身可能就有問題。

7. **首次執行卡在準備工具** → haul 第一次會下載 yt-dlp 與 ffmpeg 約 80MB 到 app 資料夾。需要網路。

## 做不到的事

- **DRM 保護的串流**（Netflix、Spotify、Apple Music 這類走 Widevine 的）完全不處理
- **帳號密碼**：haul 只讀瀏覽器已有的 cookie，不做登入。需要登入就請使用者在瀏覽器登入
- 不要嘗試繞過上面兩項

## 沒安裝的話

```bash
cd <haul repo> && cargo build --release --workspace
# 二進位在 target/release/haul
```

同一份引擎另外有 GUI（`haul-gui`），兩者共用 yt-dlp / ffmpeg、歷史檔與執行紀錄，可以混著用。

## 檔案在哪

| 用途 | 位置 |
| --- | --- |
| 下載的檔案 | `~/Downloads/Haul`（可用 `-o` 改） |
| 歷史 | 輸出資料夾裡的 `.haul-history.jsonl`（append-only） |
| 執行紀錄 | `~/Library/Application Support/com.haul.desktop/logs/haul.log`（4MB 輪替、保留 3 份） |
| yt-dlp / ffmpeg | 同上目錄的 `bin/` |
| 匯出的 cookie | 同上目錄的 `bin/cookies.txt`（權限 600，等同登入憑證，不要貼出來） |

紀錄跟著安裝走而不是跟著 `-o` 走——診斷時不必回想當初輸出到哪個資料夾。
