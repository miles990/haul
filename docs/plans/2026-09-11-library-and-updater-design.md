# 列表縮圖、圖片模式、自我更新、公開發布

2026-09-11 定案。起點是「下載完想在 UI 看到縮圖」，討論後收斂成：
**不做媒體庫，把現有列表做到能當瀏覽用**；順手補上 GUI 一直缺的圖片模式、
app 自我更新，並把 repo 公開、README 出中英版。

## 定案（brainstorm 結論）

- **不做管理頁。** 列表重開就還原歷史（上限 500 筆），本來就是「已下載」清單；
  缺的是認不出項目，不是缺一個頁面。搬移、改名、刪除交給 Finder／檔案總管——
  一做管理頁就會跟歷史檔對不上，然後得處理同步。Haul 只管「下載的目標」。
- **縮圖放列左側 48px 方格**，列表維持緊湊。
- **列表分兩組**：進行中在上、已完成在下，同一張列表只是分組，不分頁。
- **完成項目多一個動作「在 Finder 顯示」**（Windows 為檔案總管）——想動檔案時一步
  跳到真正的檔案管理器。
- **圖片模式進 GUI**：引擎早有（CLI `-i`），只是沒放按鈕。
- **自我更新走 Tauri 官方 updater**，來源就是 GitHub Releases 的 `latest.json`。
  updater 的 minisign 簽章是必要的（`pubkey` 必填），但免費、免帳號；
  Apple／Windows 程式碼簽章仍然不做。
- **repo 轉 public**——updater 要能匿名讀 `latest.json`，README 也要有人看得到。
- 不做：搜尋、篩選、刪除、改名、標籤。

## 縮圖

### 怎麼產

驗證通過、搬進正式資料夾之後，引擎用 ffmpeg 產一張 96×96 JPEG（Retina 顯示 48px）：

| 型別 | 做法 |
| --- | --- |
| 影片 | `-ss` 到 2% 處抽一格（跟影片抽樣驗證的第一個點相同） |
| 音樂 | `-map 0:v:0 -frames:v 1` 抽內嵌封面（mp3 APIC / m4a covr 在 ffmpeg 眼裡都是 attached_pic 視訊流）；沒有就不產 |
| 圖片 | 縮圖本身 |
| 其他（PDF / ZIP / 文字） | 不產 |

統一 `scale=96:96:force_original_aspect_ratio=increase,crop=96:96` 裁成正方形。
產不出來不是錯誤——項目照樣完成，只是沒縮圖。

### 存哪

輸出資料夾裡的 `.haul-thumbs/<hash>.jpg`，hash 取自完整路徑。跟 `.haul-history.jsonl`
同一個地方：輸出資料夾就是真相，換資料夾縮圖跟著走。

`Item` 多一個 `thumb: Option<String>`（縮圖完整路徑），跟著歷史一起存；
CLI `--json` 也看得到，agent 可以直接用。

### 舊項目

第一次顯示時原檔還在就補產一張，之後直接讀。原檔不在了就顯示占位圖示、
點了不再嘗試播放。這由 GUI 在 paint 時對「已完成但沒縮圖」的列呼叫一次
`thumb(id)` 完成——引擎負責「確認或補產」，回傳 base64 JPEG。

### 送進 webview

不開 asset protocol 白名單（跟提示音同一個決定）。`thumb(id)` 指令回 base64，
前端用 `data:` URL；CSP 加 `img-src 'self' data:`。前端只傳 id，不傳路徑。

## 列表分組與「在 Finder 顯示」

- `#queue` 裡兩個容器：`#active`（進行中，含 queued / resolving / browser / recording /
  downloading / verifying / failed）與 `#done`（done）。列的狀態變了就把元素搬到
  另一個容器；進行中依加入順序，已完成新的在上。
- 只有進行中非空才顯示「進行中」標題；已完成空時不顯示標題。
- 已完成的列 hover 出現「在 Finder 顯示」；指令 `reveal_file(path)` 沿用
  `validate_playable` 的路徑檢查（只准輸出資料夾裡的檔案），macOS `open -R`，
  Windows `explorer /select,`。

## 圖片模式

模式列變三顆：影片／只要聲音／圖片。`kind` 對應多一個 `"image"`（目前圖片模式
的 kind 記成 video，重試時會變成影片模式——順手修正）。占位圖示：影片 ▶、
音樂 ♪、圖片 ▣。

## 自我更新

### 流程

```
啟動 → 工具就緒後靜靜 check 一次 ──有新版──→ 頂端出現「有新版本 v0.2.0 · 更新並重新啟動」
設定面板「檢查 Haul 更新」 → 同一條路，但沒有新版／失敗會講出來
按下更新 → download_and_install（進度顯示在同一條）→ 重新啟動
```

- check 與安裝都在 Rust 端做（`tauri-plugin-updater` 的 Rust API），前端只 invoke
  `check_update` / `install_update`——不必給 webview 任何 updater 權限，
  capabilities 維持 `core:default`。
- 自動 check 失敗（沒網路、dev 建置沒有 release）只寫 log，不打擾。
- Windows 用 `installMode: passive`：跑安裝檔但不問問題。

### 簽章與發布

- `cargo tauri signer generate` 產一對 minisign 金鑰。公鑰進 `tauri.conf.json`，
  私鑰放 GitHub secret `TAURI_SIGNING_PRIVATE_KEY`（`gh secret set`），本機留
  `~/.tauri/haul.key`。**私鑰要備份**：弄丟了要重產，已裝的 app 得手動下載一次。
- `bundle.createUpdaterArtifacts: true`；release workflow 的 tauri-action 加
  `includeUpdaterJson: true`，會把 `latest.json` 跟 `.tar.gz` / `.sig` 一起附到
  release。
- endpoint：`https://github.com/miles990/haul/releases/latest/download/latest.json`。
  release 是 draft 的期間這個網址是 404，發布後才生效——正好當「按下發布才推給
  使用者」的開關。
- 已裝的 v0.1.0 沒有 updater，仍需手動下載一次；從這版起自動。
- macOS 未簽章的 app 被 updater 覆蓋後，新檔案不帶 quarantine 標記，理論上不會
  再被 Gatekeeper 攔。第一次真的更新時要驗。

## README

`README.md` 英文、`README.zh-TW.md` 中文，開頭互連，各約 100 行：一句話講是什麼、
安裝（含放行說明）、GUI 三句、CLI 用法＋離開碼＋驗證等級表、agent skill、
從原始碼建置。長篇的「為什麼」留在程式註解與 docs/plans。

## 公開

先掃整個 git 歷史（token、cookie 檔、個人路徑、log），乾淨才
`gh repo edit --visibility public`，然後推 main。

## 順序

1. 引擎：縮圖產生 + `Item.thumb` + 歷史
2. GUI：縮圖、分組、在 Finder 顯示、圖片模式
3. 自我更新：plugin、金鑰、workflow、UI
4. README 中英版
5. 掃描、轉 public、推 main

每步一個 commit。
