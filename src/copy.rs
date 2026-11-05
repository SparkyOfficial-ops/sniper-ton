use anyhow::{anyhow, Context, Result};
use num_bigint::BigUint;
use num_traits::Zero;
use serde_json::Value;

#[derive(Debug, Clone)]
pub struct ObservedBuy {
    pub jetton: String,
    pub symbol: String,
    pub dex: String,
    pub ton_in: BigUint,
}

pub async fn find_buy(http: &reqwest::Client, api_key: &str, wallet: &str, tx_hash: &str, lt: u64) -> Result<Option<ObservedBuy>> {
    let url = format!("https://tonapi.io/v2/accounts/{wallet}/events?limit=20");
    let response = http
        .get(url)
        .bearer_auth(api_key)
        .send()
        .await
        .context("лента кошелька")?;
    let status = response.status();
    let body = response.text().await.unwrap_or_default();
    if !status.is_success() {
        return Err(anyhow!("события {wallet}: {status} {body}"));
    }
    let parsed: Value = serde_json::from_str(&body).context("события: не JSON")?;
    let events = parsed.get("events").and_then(|item| item.as_array()).cloned().unwrap_or_default();
    for event in events {
        let event_id = event.get("event_id").and_then(|item| item.as_str()).unwrap_or("");
        let event_lt = event.get("lt").and_then(|item| item.as_u64()).unwrap_or(0);
        let same = event_id.eq_ignore_ascii_case(tx_hash) || (lt != 0 && event_lt == lt);
        if !same {
            continue;
        }
        return Ok(buy_from_event(&event));
    }
    Ok(None)
}

fn buy_from_event(event: &Value) -> Option<ObservedBuy> {
    let actions = event.get("actions")?.as_array()?;
    for action in actions {
        if action.get("type").and_then(|item| item.as_str()) != Some("JettonSwap") {
            continue;
        }
        let status = action.get("status").and_then(|item| item.as_str()).unwrap_or("ok");
        if status != "ok" {
            continue;
        }
        let swap = action.get("JettonSwap")?;
        if swap.get("jetton_master_in").is_some() && !swap.get("jetton_master_in").unwrap().is_null() {
            continue;
        }
        let ton_in = json_amount(swap.get("ton_in"));
        if ton_in.is_zero() {
            continue;
        }
        let out = swap.get("jetton_master_out")?;
        let jetton = out.get("address").and_then(|item| item.as_str())?.to_string();
        let symbol = out.get("symbol").and_then(|item| item.as_str()).unwrap_or("").to_string();
        let dex = swap.get("dex").and_then(|item| item.as_str()).unwrap_or("").to_string();
        return Some(ObservedBuy {
            jetton,
            symbol,
            dex,
            ton_in,
        });
    }
    None
}

fn json_amount(value: Option<&Value>) -> BigUint {
    let Some(value) = value else {
        return BigUint::zero();
    };
    if let Some(number) = value.as_u64() {
        return BigUint::from(number);
    }
    if let Some(text) = value.as_str() {
        return text.parse().unwrap_or_else(|_| BigUint::zero());
    }
    BigUint::zero()
}
