use crate::amount::TON_ASSET;
use anyhow::{anyhow, Context, Result};
use num_bigint::BigUint;
use serde::Deserialize;

const API: &str = "https://api.ston.fi";

#[derive(Clone)]
pub struct StonClient {
    http: reqwest::Client,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Pool {
    pub address: String,
    pub token0_address: String,
    pub token1_address: String,
    pub reserve0: String,
    pub reserve1: String,
    #[serde(default)]
    pub deprecated: bool,
}

#[derive(Debug, Clone, Deserialize)]
pub struct RouterInfo {
    pub address: String,
    #[serde(default)]
    pub pool_creation_enabled: bool,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Simulation {
    pub offer_jetton_wallet: String,
    pub ask_jetton_wallet: String,
    pub pool_address: String,
    pub router: RouterMeta,
    pub offer_units: String,
    pub ask_units: String,
    pub min_ask_units: String,
    pub price_impact: String,
    #[serde(default)]
    pub gas_params: Option<GasParams>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct RouterMeta {
    pub address: String,
    pub major_version: u32,
}

#[derive(Debug, Clone, Deserialize)]
pub struct GasParams {
    pub forward_gas: String,
}

#[derive(Deserialize)]
struct PoolList {
    pool_list: Vec<Pool>,
}

#[derive(Deserialize)]
struct RouterList {
    router_list: Vec<RouterInfo>,
}

impl StonClient {
    pub fn new(http: reqwest::Client) -> Self {
        Self { http }
    }

    pub async fn pools(&self) -> Result<Vec<Pool>> {
        let list: PoolList = self.get("/v1/pools").await?;
        Ok(list.pool_list)
    }

    pub async fn routers(&self) -> Result<Vec<RouterInfo>> {
        let list: RouterList = self.get("/v1/routers").await?;
        Ok(list.router_list)
    }

    pub async fn simulate(
        &self,
        offer_nano: &BigUint,
        ask_jetton: &str,
        slippage: &str,
        referral: Option<&str>,
        referral_fee_bps: u16,
    ) -> Result<Simulation> {
        let mut url = format!(
            "{API}/v1/swap/simulate?offer_address={TON_ASSET}&ask_address={ask}&units={offer_nano}&slippage_tolerance={slippage}&dex_v2=true",
            ask = urlencoding_address(ask_jetton),
        );
        if let Some(referral) = referral {
            if referral_fee_bps > 0 {
                url.push_str(&format!(
                    "&referral_address={}&referral_fee={referral_fee_bps}",
                    urlencoding_address(referral)
                ));
            }
        }
        let response = self.http.post(url).send().await.context("STON.fi не ответил")?;
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        if !status.is_success() {
            return Err(anyhow!("симуляция STON.fi {status}: {body}"));
        }
        serde_json::from_str(&body).with_context(|| format!("непонятный ответ STON.fi: {body}"))
    }

    async fn get<T: serde::de::DeserializeOwned>(&self, path: &str) -> Result<T> {
        let response = self
            .http
            .get(format!("{API}{path}"))
            .send()
            .await
            .with_context(|| format!("STON.fi {path}"))?;
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        if !status.is_success() {
            return Err(anyhow!("STON.fi {path} -> {status}: {body}"));
        }
        serde_json::from_str(&body).with_context(|| format!("STON.fi {path}: неожиданный JSON"))
    }
}

fn urlencoding_address(addr: &str) -> String {
    addr.replace('+', "%2B").replace('/', "%2F")
}
