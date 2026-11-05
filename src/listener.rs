use anyhow::{anyhow, Context, Result};
use futures_util::{SinkExt, StreamExt};
use serde_json::Value;
use std::collections::HashSet;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::Message;

use crate::amount::address_key;

#[derive(Debug, Clone)]
pub enum LiveEvent {
    StonfiActivity,
    DedustActivity,
    WalletTx { tx_hash: String, lt: u64 },
}

pub async fn listen(
    api_key: String,
    stonfi: HashSet<String>,
    dedust: HashSet<String>,
    wallets: HashSet<String>,
    subscribe: Vec<String>,
    tx: mpsc::Sender<LiveEvent>,
) {
    if subscribe.is_empty() {
        tracing::warn!("WebSocket: нечего слушать");
        return;
    }
    let mut pause = Duration::from_secs(1);
    loop {
        match session(&api_key, &stonfi, &dedust, &wallets, &subscribe, &tx).await {
            Ok(()) => tracing::warn!("WebSocket закрылся, подключаюсь снова"),
            Err(err) => tracing::warn!("WebSocket: {err}. Повтор через {} с", pause.as_secs()),
        }
        tokio::time::sleep(pause).await;
        pause = (pause * 2).min(Duration::from_secs(30));
    }
}

async fn session(
    api_key: &str,
    stonfi: &HashSet<String>,
    dedust: &HashSet<String>,
    wallets: &HashSet<String>,
    subscribe: &[String],
    tx: &mpsc::Sender<LiveEvent>,
) -> Result<()> {
    let mut request = "wss://tonapi.io/v2/websocket"
        .into_client_request()
        .context("адрес WebSocket")?;
    let header = format!("Bearer {api_key}")
        .parse()
        .map_err(|_| anyhow!("ключ TonAPI не помещается в заголовок"))?;
    request.headers_mut().insert("Authorization", header);

    let (socket, _) = connect_async(request).await.context("соединение TonAPI")?;
    tracing::info!("WebSocket TonAPI подключён, подписок: {}", subscribe.len());
    let (mut write, mut read) = socket.split();
    let hello = serde_json::json!({
        "id": 1,
        "jsonrpc": "2.0",
        "method": "subscribe_account",
        "params": subscribe,
    });
    write
        .send(Message::Text(hello.to_string().into()))
        .await
        .context("подписка")?;

    while let Some(message) = read.next().await {
        let message = message.context("чтение WebSocket")?;
        match message {
            Message::Ping(payload) => {
                write.send(Message::Pong(payload)).await.context("pong")?;
            }
            Message::Text(text) => {
                if let Some(event) = decode(&text, stonfi, dedust, wallets) {
                    if tx.send(event).await.is_err() {
                        break;
                    }
                }
            }
            Message::Close(_) => break,
            _ => {}
        }
    }
    Ok(())
}

fn decode(text: &str, stonfi: &HashSet<String>, dedust: &HashSet<String>, wallets: &HashSet<String>) -> Option<LiveEvent> {
    let value: Value = serde_json::from_str(text).ok()?;
    let method = value.get("method").and_then(|item| item.as_str()).unwrap_or("");
    if method == "subscribe_account" {
        let result = value.get("result").cloned().unwrap_or(Value::Null);
        tracing::info!("подписка: {result}");
        return None;
    }
    if method != "account_transaction" {
        return None;
    }
    let params = value.get("params")?;
    let account = params.get("account_id").and_then(|item| item.as_str())?;
    let key = address_key(account);
    if stonfi.contains(&key) {
        return Some(LiveEvent::StonfiActivity);
    }
    if dedust.contains(&key) {
        return Some(LiveEvent::DedustActivity);
    }
    if wallets.contains(&key) {
        let tx_hash = params.get("tx_hash").and_then(|item| item.as_str()).unwrap_or("").to_string();
        let lt = params.get("lt").and_then(|item| item.as_u64()).unwrap_or(0);
        if tx_hash.is_empty() {
            return None;
        }
        return Some(LiveEvent::WalletTx { tx_hash, lt });
    }
    None
}
