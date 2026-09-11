# 瀏覽器層：輔助萃取、錄製、設定面板

2026-09-11 定案。對應「萬用」的最後兩層——有檔案但網址只在跑起來的頁面裡才出現、
以及根本沒有檔案只能錄——加上把逐漸變多的設定收進一個面板。

## 範圍與不做的事

| 層 | 情況 | 做法 |
| --- | --- | --- |
| 1 | 有網址、伺服器肯給檔案 | 已完成（yt-dlp / gallery-dl / 直接抓取，含登入牆） |
| 2 | 檔案存在，但網址只在跑起來的頁面裡才出現 | **瀏覽器輔助萃取**：真的 Chrome 跑頁面，攔截媒體請求，交回現有引擎 |
| 3 | 根本沒有檔案（WebRTC、DRM） | **錄製**：分頁擷取畫面＋聲音，產出標成「錄製」而非「下載」 |

不做：繞過 DRM（錄出來是黑畫面，這是預期行為）、系統層側錄、headless。

## 定案（brainstorm 結論）

- **驅動哪個瀏覽器**：Haul 自己啟動一個獨立 Chrome / Chromium / Edge 實例，
  user-data-dir 放在 app 資料夾的 `browser/`。Chrome 136 起禁止對預設 profile 開
  remote debugging，接管使用者正在跑的 Chrome 已不可能。瀏覽器是**選配依賴**，
  跟 gallery-dl 一樣：沒有就在錯誤裡說明。
- **觸發**：瀏覽器不是預設後備。GUI 在萃取失敗的項目列上出現「用瀏覽器抓」；
  CLI 用 `--browser` 允許該次執行在前三層失敗後自動落到瀏覽器。401 / 429 /
  驗證失敗不出現這顆按鈕——那不是瀏覽器能救的。
- **候選挑選**：自動挑，候選列出來可改。manifest（m3u8 / mpd）優先於單檔；同類
  取最大；< 10 KB 與廣告網域排除。`--json` 先印一行 `candidates` 事件。
- **錄製方式**：分頁擷取（`getDisplayMedia`），Chrome 用
  `--auto-select-tab-capture-source-by-title` 啟動所以不跳選擇對話框，
  `Runtime.evaluate` 的 `userGesture: true` 滿足手勢要求。只錄那個分頁的聲音，
  不要系統權限、不裝虛擬音訊裝置。
- **錄製的影音選擇**：沿用項目的「影片／只要聲音」，不新增控制項。
- **錄製輸出**：影片 `video/webm;codecs=h264,opus` → ffmpeg 轉封裝 mp4
  （視訊 copy、音訊 Opus→AAC）；聲音 → m4a。理由：macOS 系統播放器不吃 WebM，
  會打破「完成的項目點一下就播」。
- **錄製停止**：使用者按停止（GUI、面板、Chrome 藍條任一）／頁面上所有媒體
  `ended` 且 5 秒內沒有新播放／到上限（預設 3 小時）。先到先贏。
- **歷史與 `--json`**：錄製的項目多 `source: "recording"`，`verified` 照樣誠實報
  `media`——「能不能播」與「是不是原檔」分開講。檔名加「（錄製）」。
- **設定面板**：齒輪開一個面板，不是頁面。規則：每貼一個連結都可能不同的留在輸入
  欄旁（只有影片／只要聲音），一年改不到幾次的進設定。
- **提示音**：佇列「進行中」數量從 >0 變 0 時響一次。內建（Web Audio 合成）或
  自訂檔案；自訂檔案選擇時走 `media` 驗證閘門，找不到時退回內建。

## 架構

新模組 `core/src/browser/`，跟 `extract.rs`、`gallery.rs`、`direct.rs` 平行——
第四條解析路徑，同樣只負責「回答這頁上有什麼」，下載與驗證仍走引擎。

```
browser/
  chrome.rs   找可執行檔（Chrome / Chromium / Edge / Brave，macOS 與 Windows 的慣用路徑，
              設定可覆寫）、啟動、讀 DevToolsActivePort、關閉
  cdp.rs      最小 CDP 客戶端：JSON over WebSocket，id 對應回應、事件廣播。
              只用 Target / Network / Page / Runtime / Browser 五個 domain 的十來個方法，
              手寫幾百行，不引入 chromiumoxide 這種綁版本的大依賴
  sniff.rs    從 Network 事件挑媒體候選、計分、停止規則
  record.rs   錄製：輔助面板頁、MediaRecorder、chunk 回傳、停止條件
```

連線用 `--remote-debugging-port=0`，port 從 user-data-dir 的 `DevToolsActivePort`
讀。不用 `--remote-debugging-pipe` 是因為 Rust 的 `std::process` 在 Windows 上沒辦法
乾淨地傳 fd 3 / 4。代價：session 期間本機其他程序理論上連得進這個瀏覽器。
接受，因為 (a) 只綁 127.0.0.1，(b) profile 目錄 700，(c) 工作結束就 `Browser.close`。

一個 Engine 同時最多一個瀏覽器實例，多個項目開多個分頁。錄製同時最多一個
（title 標記是啟動時固定的字串，而且錄製本來就是即時的）。

新依賴：`tokio-tungstenite`（不開 TLS feature，只連 127.0.0.1）、`base64`。

### 資料流：輔助萃取

```
項目失敗（extract 類） ─→ 使用者按「用瀏覽器抓」/ CLI --browser
  ─→ 確保瀏覽器在跑 ─→ Target.createTarget(url) ─→ Network.enable
  ─→ 若該次帶 --cookies：導向前先 Network.setCookies（用 cookies.rs 匯出的檔）
  ─→ 收 requestWillBeSentExtraInfo（完整 request header 含 Cookie）
       與 responseReceived（MIME、Content-Length、狀態）
  ─→ sniff 計分，停止規則到了 ─→ Event::Candidates ─→ 自動挑一個
  ─→ manifest → Job::Ytdlp { url, headers }（extract.rs 加 --add-headers / --referer）
     單檔    → Job::Direct { media, headers }（direct.rs 的 cookie 參數擴成 headers map）
  ─→ 既有的佇列 / 限流 / 驗證 / 歷史，一個都不跳
```

停止規則：第一個 manifest 出現後再等 3 秒收尾；單檔候選 5 秒內沒有新候選；
整體 60 秒逾時；使用者關分頁視為取消。逾時期間 UI 顯示「在瀏覽器裡按播放」。

看到分段（`.ts` / `.m4s`）卻沒有 manifest：這是 JS 自己組 segment 的 MSE，不在
第 2 層範圍。失敗訊息明講「分段串流但沒有 manifest，可改用錄製」。

### 資料流：錄製

```
「改用錄製」/ haul record ─→ 確保瀏覽器在跑 ─→ 目標分頁（沿用或新開）
  ─→ 目標分頁 document.title 前綴固定標記（給 auto-select 用），注入 ended/play 監聽
  ─→ 另開小視窗當面板（Target.createTarget newWindow 360×200）
  ─→ 面板頁 getDisplayMedia({video, audio}) 自動選到目標分頁
  ─→ MediaRecorder timeslice 1s；「只要聲音」時只餵音訊軌
  ─→ 每個 chunk base64 經 Runtime binding 回 Haul（不另開 HTTP server）
  ─→ 寫進 staging/<tag>-rec.webm
  ─→ 停止 ─→ ffmpeg 轉封裝（mp4 或 m4a）─→ media 驗證閘門 ─→ 存檔
```

面板頁同時是使用者看得到的控制：`● 錄製中 00:42 · 38 MB [停止]`。
Chrome 自己的「此分頁正在分享」藍條會出現，這是誠實的訊號，不擋也擋不掉；
藍條上的停止會觸發 track `ended`，我們照收尾。

> **2026-09-11 實作時修正**：spike 實測 `--auto-select-tab-capture-source-by-title`
> 與 `--auto-select-desktop-capture-source` 都選不到分頁（前者選到整個螢幕、後者
> 選擇框開著逾時）。改成**目標分頁自己** `getDisplayMedia({preferCurrentTab: true})`，
> Chrome 帶 `--auto-accept-this-tab-capture`，626 ms 內零互動拿到分頁的影音。
> 所以不再有面板視窗：擷取與 MediaRecorder 跑在目標分頁裡，chunk 經 binding 回
> Haul；狀態顯示交給 Chrome 的藍條與 Haul 的 UI。代價與限制：分頁換頁會殺掉錄製器
>（監聽 `pagehide` 先停，最多掉 1 秒）；`getDisplayMedia` 只在 secure context 可用，
> 純 `http://` 的頁面（localhost 除外）錄不了。

瀏覽器中途關掉：已收到的 chunk 照樣轉封裝、驗證——半支片也比沒有好。

## 設定

`<app 資料夾>/settings.json`，欄位：`out_dir`、`max_height`、`concurrency`、
`cookies_from`、`browser_path`、`record_max_secs`、`chime: { enabled, file }`。

Engine 的對應欄位做成執行期可改（同 `set_cookies_from` 的作法）。同時下載數的
semaphore 沒辦法安全地即時縮小，標成「重新啟動後生效」。CLI 旗標只影響該次執行，
不寫回設定。

面板內容：

| 群組 | 項目 |
| --- | --- |
| 下載 | 下載位置、畫質上限、同時下載數、佇列完成提示音（內建／自訂檔案、試聽） |
| 登入 | 登入狀態來源（從輸入欄旁搬進來） |
| 瀏覽器 | 偵測到的路徑（可改）、錄製時間上限 |
| 工具 | yt-dlp 版本＋更新、gallery-dl 有無（沒有就給安裝指令） |
| 診斷 | 開啟執行紀錄資料夾 |

「更新 yt-dlp」從頁尾搬進工具群組；萃取失敗的項目列上給行內動作「更新 yt-dlp 後
重試」補回可發現性。頁尾剩：開啟資料夾、清掉已完成、⚙ 設定。

提示音：自訂檔案由後端讀位元組回傳、前端 `decodeAudioData` 播，不開 asset
protocol 白名單。

## 錯誤處理

| 情況 | 行為 |
| --- | --- |
| 找不到瀏覽器 | 失敗訊息附安裝提示與「設定裡可指定路徑」 |
| 瀏覽器啟動後 10 秒內沒有 DevToolsActivePort | 失敗「瀏覽器沒有回應」，殺掉程序 |
| 瀏覽器中途退出 | 偵測中的項目失敗「瀏覽器已關閉」；錄製中的項目收尾已有內容 |
| 使用者關掉分頁 | 偵測：失敗「已取消」；錄製：停止並收尾 |
| 逾時沒有候選 | 失敗，訊息提示可改用錄製；GUI 把「改用錄製」變成主要動作 |
| 只看到分段沒 manifest | 同上，訊息更具體 |
| MediaRecorder 不支援 h264 | 退到 vp9 → vp8；轉封裝改成重編 H.264（慢但能出 mp4） |
| 磁碟滿 | 錄製失敗，訊息含路徑 |
| CDP 方法回錯 | 紀錄 error 級含方法名，項目失敗 |
| 提示音檔案不見 | 退回內建，設定列標「找不到檔案」 |

## 測試

單元（不需要瀏覽器）：
- `cdp.rs`：假 transport 下的 id 對應、事件分派、連線斷掉時未完成的呼叫都拿到錯誤
- `sniff.rs`：計分（manifest > 單檔、大小、廣告網域、Range 請求去重）、停止規則的
  狀態機（各種順序的事件與時間）
- `record.rs`：停止條件先到先贏、chunk 順序與寫入
- 設定：serde、缺欄位補預設、檔案不存在
- `chrome.rs`：路徑偵測的候選清單、`DevToolsActivePort` 解析

整合（環境變數閘門，CI 不跑，同現有 `HAUL_TEST_MEDIA` 作法）：
- `HAUL_TEST_CHROME=1`：本機 HTTP server 開一頁有 `<video src>` 的頁面，真的啟動
  Chrome → 偵測到候選 → 下載 → 驗證通過
- 同上，一頁自動播放合成音的 `<audio>`：錄 5 秒 → 轉封裝 → `media` 閘門通過且
  無聲偵測**不**誤報

## 分期

1. **瀏覽器輔助萃取**：chrome.rs、cdp.rs、sniff.rs、引擎整合、extract/direct 吃
   headers、CLI `--browser`、GUI 按鈕與候選清單
2. **錄製**：record.rs、面板頁、轉封裝、停止條件、CLI `record`、GUI 控制
3. **設定面板**：settings.json、面板 UI、搬遷登入來源與更新按鈕、瀏覽器路徑、
   錄製上限、提示音（含自訂檔案）

每期結束可獨立發布。第 1、2 期在第 3 期之前用自動偵測與預設值。
