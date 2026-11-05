use crate::amount::{parse_address, ton_to_nano};
use anyhow::{anyhow, bail, Context, Result};
use num_bigint::BigUint;
use serde::Deserialize;
use std::path::Path;
use tonlib_core::types::TonAddress;
use tonlib_core::wallet::wallet_version::WalletVersion;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Snipe,
    Copy,
    Both,
}

impl Mode {
    pub fn snipe(self) -> bool {
        matches!(self, Self::Snipe | Self::Both)
    }

    pub fn copy(self) -> bool {
        matches!(self, Self::Copy | Self::Both)
    }
}

#[derive(Debug, Clone)]
pub struct Settings {
    pub mode: Mode,
    pub dry_run: bool,
    pub buy_amount: BigUint,
    pub slippage: String,
    pub min_liquidity: BigUint,
    pub max_price_impact: f64,
    pub deadline_secs: u64,
    pub poll_secs: u64,
    pub max_buys: u32,
    pub buy_known_tokens: bool,
    pub copy_wallets: Vec<TonAddress>,
    pub copy_scale: String,
    pub copy_min: BigUint,
    pub copy_cooldown_secs: u64,
    pub dedust_gas: BigUint,
    pub referral: Option<TonAddress>,
    pub referral_fee_bps: u16,
    pub tonapi_key: String,
    pub toncenter_api_key: String,
    pub mnemonic: String,
    pub wallet_version: WalletVersion,
}

#[derive(Debug, Deserialize)]
struct FileSettings {
    mode: String,
    dry_run: bool,
    buy_amount_ton: String,
    slippage: String,
    min_liquidity_ton: String,
    max_price_impact: String,
    deadline_secs: u64,
    poll_secs: u64,
    max_buys: u32,
    #[serde(default)]
    buy_known_tokens: bool,
    #[serde(default)]
    copy_wallets: Vec<String>,
    copy_scale: String,
    copy_min_ton: String,
    #[serde(default = "default_cooldown")]
    copy_cooldown_secs: u64,
    dedust_gas_ton: String,
    #[serde(default)]
    referral_address: String,
    #[serde(default)]
    referral_fee_bps: u16,
    #[serde(default)]
    tonapi_key: String,
    #[serde(default)]
    toncenter_api_key: String,
    #[serde(default)]
    mnemonic: String,
    #[serde(default = "default_wallet")]
    wallet_version: String,
}

fn default_cooldown() -> u64 {
    90
}

fn default_wallet() -> String {
    "v4r2".to_string()
}

impl Settings {
    pub fn load(path: &Path) -> Result<Self> {
        let _ = dotenvy::dotenv();
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("нет файла {}. Скопируйте config.example.toml", path.display()))?;
        let file: FileSettings = toml::from_str(&text).context("config.toml разобран с ошибкой")?;

        let mode = match file.mode.trim().to_ascii_lowercase().as_str() {
            "snipe" => Mode::Snipe,
            "copy" => Mode::Copy,
            "both" => Mode::Both,
            other => bail!("mode должен быть snipe, copy или both, сейчас {other}"),
        };
        let wallet_version = match file.wallet_version.trim().to_ascii_lowercase().as_str() {
            "v4r2" | "v4" => WalletVersion::V4R2,
            "v3r2" | "v3" => WalletVersion::V3R2,
            "v5r1" | "v5" => WalletVersion::V5R1,
            other => bail!("wallet_version {other} не поддерживается"),
        };

        let mut dry_run = file.dry_run;
        if let Ok(flag) = std::env::var("DRY_RUN") {
            dry_run = matches!(flag.trim().to_ascii_lowercase().as_str(), "1" | "true" | "yes");
        }

        let tonapi_key = env_or("TONAPI_KEY", file.tonapi_key);
        let toncenter_api_key = env_or("TONCENTER_API_KEY", file.toncenter_api_key);
        let mnemonic = env_or("TON_MNEMONIC", file.mnemonic);
        if tonapi_key.is_empty() {
            bail!("нужен TONAPI_KEY: ключ берётся бесплатно на https://tonconsole.com");
        }
        if file.referral_fee_bps > 100 {
            bail!("referral_fee_bps не больше 100 (это 1%)");
        }
        if file.deadline_secs == 0 || file.poll_secs == 0 {
            bail!("deadline_secs и poll_secs должны быть больше нуля");
        }
        let slippage: f64 = file
            .slippage
            .parse()
            .map_err(|_| anyhow!("slippage должен быть числом, например 0.15"))?;
        if !(0.0..1.0).contains(&slippage) {
            bail!("slippage задаётся долей от 0 до 1, например 0.15");
        }
        let max_price_impact: f64 = file
            .max_price_impact
            .parse()
            .map_err(|_| anyhow!("max_price_impact должен быть числом"))?;

        let referral = if file.referral_address.trim().is_empty() {
            None
        } else {
            Some(parse_address(&file.referral_address)?)
        };
        let copy_wallets = file
            .copy_wallets
            .iter()
            .map(|addr| parse_address(addr))
            .collect::<Result<Vec<_>>>()?;
        if mode.copy() && copy_wallets.is_empty() {
            bail!("режим copy включён, а copy_wallets пуст");
        }

        Ok(Self {
            mode,
            dry_run,
            buy_amount: ton_to_nano(&file.buy_amount_ton)?,
            slippage: file.slippage,
            min_liquidity: ton_to_nano(&file.min_liquidity_ton)?,
            max_price_impact,
            deadline_secs: file.deadline_secs,
            poll_secs: file.poll_secs,
            max_buys: file.max_buys,
            buy_known_tokens: file.buy_known_tokens,
            copy_wallets,
            copy_scale: file.copy_scale,
            copy_min: ton_to_nano(&file.copy_min_ton)?,
            copy_cooldown_secs: file.copy_cooldown_secs,
            dedust_gas: ton_to_nano(&file.dedust_gas_ton)?,
            referral,
            referral_fee_bps: file.referral_fee_bps,
            tonapi_key,
            toncenter_api_key,
            mnemonic: mnemonic.trim().to_string(),
            wallet_version,
        })
    }
}

fn env_or(key: &str, fallback: String) -> String {
    match std::env::var(key) {
        Ok(value) if !value.trim().is_empty() => value.trim().to_string(),
        _ => fallback.trim().to_string(),
    }
}
