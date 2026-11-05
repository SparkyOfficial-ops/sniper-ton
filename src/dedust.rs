use crate::amount::is_ton_asset;
use anyhow::{anyhow, Context, Result};
use num_bigint::BigUint;
use serde::Deserialize;
use std::str::FromStr;

const API: &str = "https://api.dedust.io/v2/pools";

#[derive(Clone)]
pub struct DedustClient {
    http: reqwest::Client,
}

#[derive(Debug, Clone)]
pub struct NativePool {
    pub address: String,
    pub jetton: String,
    pub reserve_ton: BigUint,
    pub reserve_jetton: BigUint,
    pub fee_bps: u32,
}

#[derive(Debug, Deserialize)]
struct RawPool {
    address: String,
    #[serde(rename = "type")]
    kind: String,
    #[serde(rename = "tradeFee")]
    trade_fee: String,
    assets: Vec<Asset>,
    reserves: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct Asset {
    #[serde(rename = "type")]
    kind: String,
    address: Option<String>,
}

impl DedustClient {
    pub fn new(http: reqwest::Client) -> Self {
        Self { http }
    }

    pub async fn native_pools(&self) -> Result<Vec<NativePool>> {
        let response = self.http.get(API).send().await.context("DeDust не ответил")?;
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        if !status.is_success() {
            return Err(anyhow!("DeDust {status}: {body}"));
        }
        let pools: Vec<RawPool> = serde_json::from_str(&body).context("DeDust вернул неожиданный JSON")?;
        let mut native = Vec::new();
        for pool in pools {
            if pool.kind != "volatile" || pool.assets.len() != 2 || pool.reserves.len() != 2 {
                continue;
            }
            let Some(parsed) = native_side(&pool) else {
                continue;
            };
            native.push(parsed);
        }
        Ok(native)
    }
}

fn native_side(pool: &RawPool) -> Option<NativePool> {
    let native_at = pool.assets.iter().position(|asset| asset.kind == "native")?;
    let jetton_at = 1 - native_at;
    let jetton = pool.assets[jetton_at].address.clone()?;
    if is_ton_asset(&jetton) {
        return None;
    }
    let reserve_ton = BigUint::from_str(&pool.reserves[native_at]).ok()?;
    let reserve_jetton = BigUint::from_str(&pool.reserves[jetton_at]).ok()?;
    Some(NativePool {
        address: pool.address.clone(),
        jetton,
        reserve_ton,
        reserve_jetton,
        fee_bps: fee_to_bps(&pool.trade_fee),
    })
}

fn fee_to_bps(fee: &str) -> u32 {
    // "0.25" в API DeDust — это 0.25%, то есть 25 базисных пунктов.
    let Ok((num, den)) = decimal_hundredths(fee) else {
        return 30;
    };
    num.saturating_mul(100) / den.max(1)
}

fn decimal_hundredths(input: &str) -> Result<(u32, u32), ()> {
    let (whole, frac) = match input.split_once('.') {
        Some(pair) => pair,
        None => (input, ""),
    };
    if !whole.chars().all(|ch| ch.is_ascii_digit()) || !frac.chars().all(|ch| ch.is_ascii_digit()) {
        return Err(());
    }
    let frac: String = frac.chars().take(4).collect();
    let den = 10u32.pow(frac.len() as u32);
    let digits: u32 = format!("{whole}{frac}").parse().map_err(|_| ())?;
    Ok((digits, den.max(1)))
}

/// Оценка выхода AMM: out = reserve_out * in * (1-fee) / (reserve_in + in * (1-fee)).
pub fn estimate_out(reserve_in: &BigUint, reserve_out: &BigUint, amount_in: &BigUint, fee_bps: u32) -> BigUint {
    let fee_bps = fee_bps.min(10_000);
    let kept = BigUint::from(10_000u32 - fee_bps);
    let amount_after_fee = amount_in * &kept;
    let numerator = &amount_after_fee * reserve_out;
    let denominator = reserve_in * BigUint::from(10_000u32) + amount_after_fee;
    if denominator == BigUint::from(0u32) {
        return BigUint::from(0u32);
    }
    numerator / denominator
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quarter_percent_is_25_bps() {
        assert_eq!(fee_to_bps("0.25"), 25);
    }

    #[test]
    fn amm_returns_less_than_spot() {
        let out = estimate_out(
            &BigUint::from(1_000_000_000u64),
            &BigUint::from(1_000_000_000u64),
            &BigUint::from(100_000_000u64),
            25,
        );
        assert!(out < BigUint::from(100_000_000u64));
        assert!(out > BigUint::from(80_000_000u64));
    }
}
