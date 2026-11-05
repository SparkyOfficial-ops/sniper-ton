use crate::amount::{address_key, format_ton, is_ton_asset, scale_nano, DEDUST_FACTORY, DEDUST_NATIVE_VAULT};
use crate::config::{Mode, Settings};
use crate::copy::{self, ObservedBuy};
use crate::dedust::{DedustClient, NativePool};
use crate::listener::{self, LiveEvent};
use crate::ston::{Pool, StonClient};
use crate::swap::{self, SwapOrder};
use crate::wallet::{Chain, Signer};
use anyhow::{Context, Result};
use num_bigint::BigUint;
use std::collections::{HashMap, HashSet};
use std::str::FromStr;
use std::time::{Duration, Instant};
use tokio::sync::mpsc;
use tonlib_core::types::TonAddress;

const ATTEMPTS: u8 = 3;
const RESERVE_NANO: u64 = 50_000_000;

struct Books {
    pools: HashSet<String>,
    jettons: HashSet<String>,
    attempts: HashMap<String, u8>,
    copied: HashSet<String>,
    cooldown: HashMap<String, Instant>,
    buys: u32,
    ready: bool,
}

pub async fn run(settings: Settings) -> Result<()> {
    let http = reqwest::Client::builder()
        .timeout(Duration::from_secs(40))
        .user_agent("sniper-ton/0.1")
        .build()?;
    let signer = if settings.mnemonic.is_empty() {
        if !settings.dry_run {
            anyhow::bail!("для реальной отправки нужна мнемоника в TON_MNEMONIC или config.toml");
        }
        None
    } else {
        Some(Signer::from_mnemonic(&settings.mnemonic, settings.wallet_version)?)
    };

    let chain = Chain::new(http.clone(), settings.tonapi_key.clone(), settings.toncenter_api_key.clone());
    if let Some(signer) = &signer {
        let state = chain.account(signer.address()).await?;
        tracing::info!(
            "кошелёк {} | баланс {} TON | seqno {} | {}",
            signer.address().to_base64_url(),
            format_ton(&state.balance),
            state.seqno,
            if state.active { "активен" } else { "ещё не развёрнут, первая покупка его создаст" }
        );
    } else {
        tracing::info!("мнемоники нет: бот только пишет сигналы и ничего не подписывает");
    }
    tracing::info!(
        "режим {} | dry_run {} | ставка {} TON | минимум ликвидности {} TON",
        mode_name(settings.mode),
        settings.dry_run,
        format_ton(&settings.buy_amount),
        format_ton(&settings.min_liquidity)
    );

    let ston = StonClient::new(http.clone());
    let dedust = DedustClient::new(http.clone());
    let mut books = Books {
        pools: HashSet::new(),
        jettons: HashSet::new(),
        attempts: HashMap::new(),
        copied: HashSet::new(),
        cooldown: HashMap::new(),
        buys: 0,
        ready: false,
    };

    if settings.mode.snipe() {
        scan_ston(&settings, &ston, &chain, signer.as_ref(), &mut books).await;
        scan_dedust(&settings, &dedust, &chain, signer.as_ref(), &mut books).await;
        books.ready = true;
        tracing::info!(
            "снимок готов: {} пулов, {} жетонов. Дальше покупаются только новые",
            books.pools.len(),
            books.jettons.len()
        );
    }

    let (stonfi, dedust_factories, wallets, subscribe) = watch_sets(&settings, &ston).await?;
    let (tx, mut rx) = mpsc::channel(256);
    let listener = tokio::spawn(listener::listen(
        settings.tonapi_key.clone(),
        stonfi,
        dedust_factories,
        wallets,
        subscribe,
        tx,
    ));

    let mut tick = tokio::time::interval(Duration::from_secs(settings.poll_secs));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut last_ston = Instant::now();
    let mut last_dedust = Instant::now();

    loop {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {
                tracing::info!("остановка");
                break;
            }
            _ = tick.tick(), if settings.mode.snipe() => {
                scan_ston(&settings, &ston, &chain, signer.as_ref(), &mut books).await;
                scan_dedust(&settings, &dedust, &chain, signer.as_ref(), &mut books).await;
                last_ston = Instant::now();
                last_dedust = Instant::now();
            }
            event = rx.recv() => {
                let Some(event) = event else { break };
                match event {
                    LiveEvent::StonfiActivity if settings.mode.snipe() && last_ston.elapsed() > Duration::from_secs(1) => {
                        scan_ston(&settings, &ston, &chain, signer.as_ref(), &mut books).await;
                        last_ston = Instant::now();
                    }
                    LiveEvent::DedustActivity if settings.mode.snipe() && last_dedust.elapsed() > Duration::from_secs(1) => {
                        scan_dedust(&settings, &dedust, &chain, signer.as_ref(), &mut books).await;
                        last_dedust = Instant::now();
                    }
                    LiveEvent::WalletTx { tx_hash, lt } if settings.mode.copy() => {
                        if let Err(err) = on_wallet(&settings, &http, &dedust, &chain, signer.as_ref(), &mut books, &tx_hash, lt).await {
                            tracing::warn!("копирование {tx_hash}: {err}");
                        }
                    }
                    _ => {}
                }
            }
        }
        if settings.max_buys > 0 && books.buys >= settings.max_buys {
            tracing::info!("достигнут max_buys = {}", settings.max_buys);
            break;
        }
    }
    listener.abort();
    Ok(())
}

async fn watch_sets(settings: &Settings, ston: &StonClient) -> Result<(HashSet<String>, HashSet<String>, HashSet<String>, Vec<String>)> {
    let mut stonfi = HashSet::new();
    let mut subscribe = Vec::new();
    if settings.mode.snipe() {
        let routers = ston.routers().await.context("список роутеров STON.fi")?;
        for router in routers.into_iter().filter(|router| router.pool_creation_enabled) {
            stonfi.insert(address_key(&router.address));
            subscribe.push(router.address);
        }
        let factory = TonAddress::from_str(DEDUST_FACTORY)?;
        subscribe.push(factory.to_base64_url());
    }
    let mut dedust = HashSet::new();
    dedust.insert(address_key(DEDUST_FACTORY));

    let mut wallets = HashSet::new();
    if settings.mode.copy() {
        for wallet in &settings.copy_wallets {
            wallets.insert(wallet.to_hex());
            subscribe.push(wallet.to_base64_url());
        }
    }
    Ok((stonfi, dedust, wallets, subscribe))
}

async fn scan_ston(settings: &Settings, ston: &StonClient, chain: &Chain, signer: Option<&Signer>, books: &mut Books) {
    let pools = match ston.pools().await {
        Ok(pools) => pools,
        Err(err) => {
            tracing::warn!("список пулов STON.fi: {err}");
            return;
        }
    };
    for pool in pools {
        consider_ston(settings, chain, signer, books, pool).await;
    }
}

async fn consider_ston(settings: &Settings, chain: &Chain, signer: Option<&Signer>, books: &mut Books, pool: Pool) {
    let pool_key = address_key(&pool.address);
    if !books.pools.insert(pool_key) {
        return;
    }
    let Some((jetton, reserve)) = ton_side(&pool) else {
        return;
    };
    let jetton_key = address_key(&jetton);
    if !books.ready {
        books.jettons.insert(jetton_key);
        return;
    }
    if pool.deprecated || reserve < settings.min_liquidity {
        books.jettons.insert(jetton_key);
        return;
    }
    let known = books.jettons.contains(&jetton_key);
    if known && !settings.buy_known_tokens {
        tracing::info!(
            "STON.fi: новый пул {} для уже известного жетона, пропуск",
            short(&jetton)
        );
        return;
    }
    tracing::info!(
        "новый пул STON.fi {} | жетон {} | ликвидность {} TON",
        short(&pool.address),
        short(&jetton),
        format_ton(&reserve)
    );
    let reason = format!("старт пула STON.fi {}", short(&pool.address));
    match execute(settings, chain, signer, &jetton, &settings.buy_amount, None, &reason).await {
        Ok(()) => {
            books.jettons.insert(jetton_key);
            books.buys += 1;
        }
        Err(err) => {
            let tries = books.attempts.entry(jetton_key.clone()).or_insert(0);
            *tries += 1;
            tracing::warn!("покупка {} не собрана ({tries}/{ATTEMPTS}): {err}", short(&jetton));
            if *tries >= ATTEMPTS {
                books.jettons.insert(jetton_key);
            } else {
                books.pools.remove(&address_key(&pool.address));
            }
        }
    }
}

async fn scan_dedust(settings: &Settings, dedust: &DedustClient, chain: &Chain, signer: Option<&Signer>, books: &mut Books) {
    let pools = match dedust.native_pools().await {
        Ok(pools) => pools,
        Err(err) => {
            tracing::warn!("список пулов DeDust: {err}");
            return;
        }
    };
    for pool in pools {
        consider_dedust(settings, chain, signer, books, pool).await;
    }
}

async fn consider_dedust(settings: &Settings, chain: &Chain, signer: Option<&Signer>, books: &mut Books, pool: NativePool) {
    let pool_key = address_key(&pool.address);
    if !books.pools.insert(pool_key) {
        return;
    }
    let jetton_key = address_key(&pool.jetton);
    if !books.ready {
        books.jettons.insert(jetton_key);
        return;
    }
    if pool.reserve_ton < settings.min_liquidity {
        books.jettons.insert(jetton_key);
        return;
    }
    if books.jettons.contains(&jetton_key) && !settings.buy_known_tokens {
        tracing::info!("DeDust: новый пул {} для уже известного жетона, пропуск", short(&pool.jetton));
        return;
    }
    tracing::info!(
        "новый пул DeDust {} | жетон {} | ликвидность {} TON",
        short(&pool.address),
        short(&pool.jetton),
        format_ton(&pool.reserve_ton)
    );
    let reason = format!("старт пула DeDust {}", short(&pool.address));
    match execute(settings, chain, signer, &pool.jetton, &settings.buy_amount, Some(&pool), &reason).await {
        Ok(()) => {
            books.jettons.insert(jetton_key);
            books.buys += 1;
        }
        Err(err) => {
            let tries = books.attempts.entry(jetton_key.clone()).or_insert(0);
            *tries += 1;
            tracing::warn!("покупка {} не собрана ({tries}/{ATTEMPTS}): {err}", short(&pool.jetton));
            if *tries >= ATTEMPTS {
                books.jettons.insert(jetton_key);
            } else {
                books.pools.remove(&address_key(&pool.address));
            }
        }
    }
}

async fn on_wallet(
    settings: &Settings,
    http: &reqwest::Client,
    dedust: &DedustClient,
    chain: &Chain,
    signer: Option<&Signer>,
    books: &mut Books,
    tx_hash: &str,
    lt: u64,
) -> Result<()> {
    if !books.copied.insert(tx_hash.to_string()) {
        return Ok(());
    }
    let mut found: Option<ObservedBuy> = None;
    for wallet in &settings.copy_wallets {
        match copy::find_buy(http, &settings.tonapi_key, &wallet.to_base64_url(), tx_hash, lt).await {
            Ok(Some(buy)) => {
                found = Some(buy);
                break;
            }
            Ok(None) => {}
            Err(err) => tracing::debug!("лента {}: {err}", wallet.to_base64_url()),
        }
    }
    let Some(buy) = found else {
        tracing::info!("транзакция {tx_hash} без покупки жетона за TON");
        return Ok(());
    };
    if buy.ton_in < settings.copy_min {
        tracing::info!(
            "копия {}: вход {} TON меньше порога",
            label(&buy),
            format_ton(&buy.ton_in)
        );
        return Ok(());
    }
    let jetton_key = address_key(&buy.jetton);
    if let Some(at) = books.cooldown.get(&jetton_key) {
        if at.elapsed() < Duration::from_secs(settings.copy_cooldown_secs) {
            tracing::info!("копия {}: этот жетон уже брали недавно", label(&buy));
            return Ok(());
        }
    }
    let scaled = scale_nano(&buy.ton_in, &settings.copy_scale)?;
    let amount = if scaled > settings.buy_amount {
        settings.buy_amount.clone()
    } else {
        scaled
    };
    tracing::info!(
        "копия {} | dex {} | их вход {} TON | наша ставка {} TON",
        label(&buy),
        if buy.dex.is_empty() { "неизвестен" } else { buy.dex.as_str() },
        format_ton(&buy.ton_in),
        format_ton(&amount)
    );
    let pool = if buy.dex.to_ascii_lowercase().contains("dedust") {
        dedust
            .native_pools()
            .await
            .ok()
            .and_then(|pools| pools.into_iter().find(|pool| address_key(&pool.jetton) == jetton_key))
    } else {
        None
    };
    let reason = format!("копия {tx_hash}");
    execute(settings, chain, signer, &buy.jetton, &amount, pool.as_ref(), &reason).await?;
    books.cooldown.insert(jetton_key, Instant::now());
    books.buys += 1;
    Ok(())
}

async fn execute(
    settings: &Settings,
    chain: &Chain,
    signer: Option<&Signer>,
    jetton: &str,
    amount: &BigUint,
    dedust: Option<&NativePool>,
    reason: &str,
) -> Result<()> {
    let Some(signer) = signer else {
        tracing::info!("{reason}: сигнал по {} на {} TON, подпись пропущена", short(jetton), format_ton(amount));
        return Ok(());
    };
    let referral = settings.referral.as_ref().unwrap_or_else(|| signer.address());
    let order = if let Some(pool) = dedust {
        let pool_addr = TonAddress::from_str(&pool.address)?;
        let vault = TonAddress::from_str(DEDUST_NATIVE_VAULT)?;
        swap::dedust_order(
            &pool_addr,
            &vault,
            signer.address(),
            referral,
            amount,
            &pool.reserve_ton,
            &pool.reserve_jetton,
            pool.fee_bps,
            &settings.slippage,
            &settings.dedust_gas,
            settings.deadline_secs,
        )?
    } else {
        ston_order(settings, signer, referral, jetton, amount).await?
    };

    tracing::info!(
        "{reason}: {} {} TON -> {} | минимум выхода {} | получатель {}",
        order.venue,
        format_ton(amount),
        short(jetton),
        order.min_out,
        order.destination.to_base64_url()
    );
    if settings.dry_run {
        tracing::info!("dry_run: транзакция собрана и не отправлена");
        return Ok(());
    }

    let state = chain.account(signer.address()).await?;
    let need = &order.value + BigUint::from(RESERVE_NANO);
    if state.balance < need {
        anyhow::bail!(
            "на кошельке {} TON, для сделки нужно около {} TON",
            format_ton(&state.balance),
            format_ton(&need)
        );
    }
    let boc = signer.sign(&order, state.seqno, !state.active)?;
    chain.send(&boc).await?;
    tracing::info!("отправлено, seqno {}", state.seqno);
    Ok(())
}

async fn ston_order(settings: &Settings, signer: &Signer, referral: &TonAddress, jetton: &str, amount: &BigUint) -> Result<SwapOrder> {
    let mut last_error = None;
    for attempt in 1..=4 {
        match settings_simulate(settings, signer, referral, jetton, amount).await {
            Ok(order) => return Ok(order),
            Err(err) => {
                last_error = Some(err);
                tokio::time::sleep(Duration::from_millis(350)).await;
                tracing::debug!("симуляция {jetton}, попытка {attempt}");
            }
        }
    }
    Err(last_error.unwrap_or_else(|| anyhow::anyhow!("симуляция не удалась")))
}

async fn settings_simulate(
    settings: &Settings,
    signer: &Signer,
    referral: &TonAddress,
    jetton: &str,
    amount: &BigUint,
) -> Result<SwapOrder> {
    let ston = StonClient::new(
        reqwest::Client::builder()
            .timeout(Duration::from_secs(15))
            .build()?,
    );
    let fee = if settings.referral.is_some() {
        settings.referral_fee_bps
    } else {
        0
    };
    let referral_arg = if fee > 0 { Some(referral.to_base64_url()) } else { None };
    let simulation = ston
        .simulate(amount, jetton, &settings.slippage, referral_arg.as_deref(), fee)
        .await?;
    let impact: f64 = simulation.price_impact.parse().unwrap_or(0.0);
    if impact > settings.max_price_impact {
        anyhow::bail!("удар по цене {impact:.2} выше порога {}", settings.max_price_impact);
    }
    swap::stonfi_order(&simulation, signer.address(), referral, fee, settings.deadline_secs)
}

fn ton_side(pool: &Pool) -> Option<(String, BigUint)> {
    let reserve0 = BigUint::from_str(&pool.reserve0).ok()?;
    let reserve1 = BigUint::from_str(&pool.reserve1).ok()?;
    if is_ton_asset(&pool.token0_address) {
        Some((pool.token1_address.clone(), reserve0))
    } else if is_ton_asset(&pool.token1_address) {
        Some((pool.token0_address.clone(), reserve1))
    } else {
        None
    }
}

fn short(addr: &str) -> String {
    let addr = addr.trim();
    if addr.len() <= 12 {
        addr.to_string()
    } else {
        format!("{}…{}", &addr[..6], &addr[addr.len() - 4..])
    }
}

fn label(buy: &ObservedBuy) -> String {
    if buy.symbol.is_empty() {
        short(&buy.jetton)
    } else {
        format!("{} ({})", buy.symbol, short(&buy.jetton))
    }
}

fn mode_name(mode: Mode) -> &'static str {
    match mode {
        Mode::Snipe => "snipe",
        Mode::Copy => "copy",
        Mode::Both => "snipe+copy",
    }
}
