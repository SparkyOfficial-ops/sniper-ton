mod amount;
mod config;
mod copy;
mod dedust;
mod engine;
mod listener;
mod ston;
mod swap;
mod wallet;

use anyhow::Result;
use clap::{Parser, Subcommand};
use std::path::PathBuf;
use std::time::Duration;
use tracing_subscriber::EnvFilter;

use crate::amount::{format_ton, parse_address, ton_to_nano};
use crate::config::Settings;
use crate::ston::StonClient;
use crate::wallet::Signer;

#[derive(Parser)]
#[command(
    name = "sniper-ton",
    version,
    about = "Слушает пулы STON.fi и DeDust и покупает жетон на старте или повторяет чужую покупку за TON"
)]
struct Cli {
    #[arg(long, default_value = "config.toml")]
    config: PathBuf,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Слушать сеть и покупать по правилам из config.toml
    Run,
    /// Показать адрес, который получится из мнемоники
    Address,
    /// Посчитать своп STON.fi и не отправлять его
    Quote {
        /// Мастер-адрес жетона
        #[arg(long)]
        jetton: String,
        /// Сколько TON отдать. По умолчанию buy_amount_ton из конфига
        #[arg(long)]
        amount: Option<String>,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")))
        .init();
    let cli = Cli::parse();
    let settings = Settings::load(&cli.config)?;
    match cli.command {
        Command::Run => engine::run(settings).await,
        Command::Address => {
            if settings.mnemonic.is_empty() {
                anyhow::bail!("мнемоника пустая: заполните TON_MNEMONIC или mnemonic в конфиге");
            }
            let signer = Signer::from_mnemonic(&settings.mnemonic, settings.wallet_version)?;
            println!("{}", signer.address().to_base64_url());
            Ok(())
        }
        Command::Quote { jetton, amount } => quote(settings, jetton, amount).await,
    }
}

async fn quote(settings: Settings, jetton: String, amount: Option<String>) -> Result<()> {
    parse_address(&jetton)?;
    let nano = match amount {
        Some(value) => ton_to_nano(&value)?,
        None => settings.buy_amount.clone(),
    };
    let http = reqwest::Client::builder().timeout(Duration::from_secs(20)).build()?;
    let ston = StonClient::new(http);
    let simulation = ston.simulate(&nano, &jetton, &settings.slippage, None, 0).await?;
    println!("площадка: STON.fi v{}", simulation.router.major_version);
    println!("отдаём: {} TON", format_ton(&nano));
    println!("расчётный выход: {}", simulation.ask_units);
    println!("минимум с проскальзыванием: {}", simulation.min_ask_units);
    println!("удар по цене: {}", simulation.price_impact);
    println!("пул: {}", simulation.pool_address);
    println!("кошелёк оффера: {}", simulation.offer_jetton_wallet);
    Ok(())
}
