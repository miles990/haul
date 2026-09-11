//! 錄製：把一個分頁的畫面與聲音錄成檔案。
//!
//! 擷取與錄製都跑在目標分頁裡：注入的腳本呼叫 `getDisplayMedia({preferCurrentTab})`
//! 擷取自己，Chrome 以 `--auto-accept-this-tab-capture` 啟動所以不跳選擇框。
//! 曾經想另開一個面板頁去選目標分頁，但 Chrome 的「依標題自動選」旗標實測選不到
//! 分頁，只有自己擷取自己是零互動的。代價：分頁換頁會殺掉錄製器，所以監聽
//! pagehide 先停下來，已收到的 chunk 照收尾。

#[cfg(test)]
mod spike {
    use super::super::{cdp::Cdp, chrome};
    use serde_json::json;

    /// 驗證三個技術假設。任何一個不成立，錄製的設計就要改：
    /// 1. --auto-accept-this-tab-capture 讓分頁自己 getDisplayMedia 不跳選擇框
    /// 2. Runtime.evaluate 的 userGesture 滿足手勢要求
    /// 3. MediaRecorder 支援 h264（否則轉 mp4 要重編）
    #[tokio::test]
    async fn assumptions_hold() {
        if std::env::var("HAUL_TEST_CHROME").is_err() {
            eprintln!("略過：未設 HAUL_TEST_CHROME");
            return;
        }
        let exe = chrome::find(None).unwrap();
        let dir = std::env::temp_dir().join("haul-chrome-test");
        let ch = chrome::launch(&exe, &dir).await.unwrap();
        let cdp = Cdp::connect(&ch.ws_url).await.unwrap();

        // 目標分頁要是 secure context 才有 navigator.mediaDevices；
        // about:blank 不是，file:// 是。真實網站是 https，同樣成立。
        let page = dir.join("target.html");
        std::fs::write(&page, "<!doctype html><title>Target</title><body>target").unwrap();
        let t = cdp
            .call(None, "Target.createTarget", json!({"url": format!("file://{}", page.display())}))
            .await
            .unwrap();
        let tid = t["targetId"].as_str().unwrap().to_string();
        let a = cdp
            .call(None, "Target.attachToTarget", json!({"targetId": tid, "flatten": true}))
            .await
            .unwrap();
        let ts = a["sessionId"].as_str().unwrap().to_string();
        tokio::time::sleep(std::time::Duration::from_millis(800)).await;

        let expr = r#"(async () => {
            const h264 = MediaRecorder.isTypeSupported('video/webm;codecs=h264,opus');
            const t = new Promise((_, rej) => setTimeout(() => rej(new Error('timeout 8s')), 8000));
            const s = await Promise.race([
                navigator.mediaDevices.getDisplayMedia({ video: true, audio: true, preferCurrentTab: true }), t]);
            const v = s.getVideoTracks()[0], a = s.getAudioTracks()[0];
            const out = { h264, video: !!v, audio: !!a, surface: v ? v.getSettings().displaySurface : '' };
            s.getTracks().forEach(x => x.stop());
            return JSON.stringify(out);
        })()"#;
        let t0 = std::time::Instant::now();
        let r = cdp
            .call(
                Some(&ts),
                "Runtime.evaluate",
                json!({ "expression": expr, "awaitPromise": true, "returnByValue": true, "userGesture": true }),
            )
            .await
            .unwrap();
        let _ = cdp.call(None, "Browser.close", json!({})).await;

        let val = r["result"]["value"]
            .as_str()
            .unwrap_or_else(|| panic!("getDisplayMedia 失敗：{r}"));
        let out: serde_json::Value = serde_json::from_str(val).unwrap();
        eprintln!("spike（{} ms）: {out}", t0.elapsed().as_millis());
        assert!(out["video"].as_bool().unwrap(), "沒拿到視訊軌（假設 1/2 不成立）：{out}");
        assert!(out["audio"].as_bool().unwrap(), "沒拿到音訊軌：{out}");
        assert_eq!(out["surface"], "browser", "選到的不是分頁：{out}");
        assert!(out["h264"].as_bool().unwrap(), "MediaRecorder 不支援 h264（假設 3 不成立）");
        assert!(t0.elapsed().as_secs() < 5, "花了 {:?}，八成是跳了選擇框", t0.elapsed());
    }
}
