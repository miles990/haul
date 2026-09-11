//! 最小的 Chrome DevTools Protocol 客戶端。
//!
//! 只用到五個 domain 的十來個方法，手寫幾百行比引入 chromiumoxide 划算——
//! 那類 crate 跟 Chrome 版本綁死，Chrome 一季一版，我們不想跟著改。
//! 協定本身很簡單：送 {id, method, params, sessionId}，收 {id, result|error}
//! 或 {method, params, sessionId}。

use anyhow::{anyhow, bail, Result};
use futures_util::{SinkExt, StreamExt};
use serde_json::Value;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use tokio::sync::{broadcast, mpsc, oneshot};

#[derive(Debug, Clone)]
pub struct CdpEvent {
    pub method: String,
    pub params: Value,
    pub session_id: Option<String>,
}

pub struct Cdp {
    out: mpsc::UnboundedSender<String>,
    pending: Arc<Mutex<HashMap<u64, oneshot::Sender<Result<Value>>>>>,
    events: broadcast::Sender<Arc<CdpEvent>>,
    next_id: AtomicU64,
}

/// 事件流的緩衝。一個影音頁面幾秒內幾百個 Network 事件很正常，
/// 消費端慢一點不該直接掉事件。
const EVENT_BUFFER: usize = 4096;

impl Cdp {
    /// 從一對 channel 建立。真正的 WebSocket 由 `connect` 接上；測試直接餵。
    pub fn new(
        out: mpsc::UnboundedSender<String>,
        mut inbox: mpsc::UnboundedReceiver<String>,
    ) -> Arc<Self> {
        let (events, _) = broadcast::channel(EVENT_BUFFER);
        let me = Arc::new(Self {
            out,
            pending: Arc::new(Mutex::new(HashMap::new())),
            events,
            next_id: AtomicU64::new(1),
        });

        let pending = me.pending.clone();
        let events = me.events.clone();
        tokio::spawn(async move {
            while let Some(text) = inbox.recv().await {
                let Ok(msg) = serde_json::from_str::<Value>(&text) else {
                    continue;
                };
                if let Some(id) = msg.get("id").and_then(Value::as_u64) {
                    if let Some(tx) = pending.lock().unwrap().remove(&id) {
                        let r = match msg.get("error") {
                            Some(e) => Err(anyhow!(
                                "{}",
                                e.get("message")
                                    .and_then(Value::as_str)
                                    .unwrap_or("未知錯誤")
                            )),
                            None => Ok(msg.get("result").cloned().unwrap_or(Value::Null)),
                        };
                        let _ = tx.send(r);
                    }
                } else if let Some(method) = msg.get("method").and_then(Value::as_str) {
                    let _ = events.send(Arc::new(CdpEvent {
                        method: method.to_string(),
                        params: msg.get("params").cloned().unwrap_or(Value::Null),
                        session_id: msg
                            .get("sessionId")
                            .and_then(Value::as_str)
                            .map(str::to_string),
                    }));
                }
            }
            // transport 斷了：所有還在等的呼叫都要拿到錯誤，不能永遠掛著
            for (_, tx) in pending.lock().unwrap().drain() {
                let _ = tx.send(Err(anyhow!("瀏覽器連線已關閉")));
            }
        });
        me
    }

    /// 連到 Chrome 的 DevTools WebSocket。
    pub async fn connect(ws_url: &str) -> Result<Arc<Self>> {
        use tokio_tungstenite::tungstenite::Message;

        let (ws, _) = tokio_tungstenite::connect_async(ws_url)
            .await
            .map_err(|e| anyhow!("連不上瀏覽器的 DevTools：{e}"))?;
        let (mut sink, mut stream) = ws.split();
        let (out_tx, mut out_rx) = mpsc::unbounded_channel::<String>();
        let (in_tx, in_rx) = mpsc::unbounded_channel::<String>();

        tokio::spawn(async move {
            while let Some(text) = out_rx.recv().await {
                if sink.send(Message::Text(text)).await.is_err() {
                    break;
                }
            }
        });
        tokio::spawn(async move {
            while let Some(Ok(msg)) = stream.next().await {
                if let Message::Text(t) = msg {
                    if in_tx.send(t).is_err() {
                        break;
                    }
                }
            }
            // in_tx 在這裡 drop，reader 會把 pending 全部失敗
        });
        Ok(Self::new(out_tx, in_rx))
    }

    /// 呼叫一個方法並等回應。`session` 是 Target.attachToTarget 給的 sessionId，
    /// 瀏覽器層級的方法（Target.*、Browser.*）傳 None。
    pub async fn call(&self, session: Option<&str>, method: &str, params: Value) -> Result<Value> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().unwrap().insert(id, tx);

        let mut msg = serde_json::json!({ "id": id, "method": method, "params": params });
        if let Some(s) = session {
            msg["sessionId"] = Value::String(s.to_string());
        }
        if self.out.send(msg.to_string()).is_err() {
            self.pending.lock().unwrap().remove(&id);
            bail!("瀏覽器連線已關閉");
        }
        rx.await
            .map_err(|_| anyhow!("瀏覽器連線已關閉"))?
            .map_err(|e| anyhow!("{method}：{e}"))
    }

    pub fn subscribe(&self) -> broadcast::Receiver<Arc<CdpEvent>> {
        self.events.subscribe()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// 用兩條 channel 假裝是 WebSocket：測的是 id 對應與事件分派，不是網路
    fn fake() -> (
        Arc<Cdp>,
        mpsc::UnboundedReceiver<String>,
        mpsc::UnboundedSender<String>,
    ) {
        let (out_tx, out_rx) = mpsc::unbounded_channel();
        let (in_tx, in_rx) = mpsc::unbounded_channel();
        (Cdp::new(out_tx, in_rx), out_rx, in_tx)
    }

    #[tokio::test]
    async fn call_resolves_the_matching_id_only() {
        let (cdp, mut out, inbox) = fake();
        let c = cdp.clone();
        let h = tokio::spawn(async move { c.call(None, "Browser.getVersion", json!({})).await });

        let sent: Value = serde_json::from_str(&out.recv().await.unwrap()).unwrap();
        let id = sent["id"].as_u64().unwrap();
        assert_eq!(sent["method"], "Browser.getVersion");

        // 別人的回應不該被吃掉
        inbox
            .send(json!({"id": id + 100, "result": {}}).to_string())
            .unwrap();
        inbox
            .send(json!({"id": id, "result": {"product": "Chrome/1"}}).to_string())
            .unwrap();

        let got = h.await.unwrap().unwrap();
        assert_eq!(got["product"], "Chrome/1");
    }

    #[tokio::test]
    async fn protocol_error_becomes_err_with_method_name() {
        let (cdp, mut out, inbox) = fake();
        let c = cdp.clone();
        let h = tokio::spawn(async move { c.call(Some("s1"), "Page.navigate", json!({})).await });
        let sent: Value = serde_json::from_str(&out.recv().await.unwrap()).unwrap();
        assert_eq!(sent["sessionId"], "s1");
        inbox
            .send(json!({"id": sent["id"], "error": {"message": "Cannot navigate"}}).to_string())
            .unwrap();
        let err = h.await.unwrap().unwrap_err().to_string();
        assert!(err.contains("Page.navigate"), "{err}");
        assert!(err.contains("Cannot navigate"), "{err}");
    }

    #[tokio::test]
    async fn events_are_broadcast_with_session() {
        let (cdp, _out, inbox) = fake();
        let mut rx = cdp.subscribe();
        inbox
            .send(
                json!({"method": "Network.responseReceived", "sessionId": "s1", "params": {"x": 1}})
                    .to_string(),
            )
            .unwrap();
        let ev = rx.recv().await.unwrap();
        assert_eq!(ev.method, "Network.responseReceived");
        assert_eq!(ev.session_id.as_deref(), Some("s1"));
        assert_eq!(ev.params["x"], 1);
    }

    #[tokio::test]
    async fn closing_the_transport_fails_pending_calls() {
        let (cdp, mut out, inbox) = fake();
        let c = cdp.clone();
        let h = tokio::spawn(async move { c.call(None, "Target.getTargets", json!({})).await });
        out.recv().await.unwrap();
        drop(inbox); // 瀏覽器掛了
        let err = h.await.unwrap().unwrap_err().to_string();
        assert!(err.contains("連線"), "{err}");
    }
}
