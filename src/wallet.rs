use crate::swap::SwapOrder;
use anyhow::{anyhow, Context, Result};
use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use num_bigint::BigUint;
use num_traits::Zero;
use serde::Deserialize;
use std::str::FromStr;
use std::time::{SystemTime, UNIX_EPOCH};
use tonlib_core::cell::{BagOfCells, Cell};
use tonlib_core::tlb_types::block::coins::{CurrencyCollection, Grams};
use tonlib_core::tlb_types::block::message::{CommonMsgInfo, IntMsgInfo, Message};
use tonlib_core::tlb_types::tlb::TLB;
use tonlib_core::types::TonAddress;
use tonlib_core::wallet::mnemonic::{KeyPair, Mnemonic};
use tonlib_core::wallet::ton_wallet::TonWallet;
use tonlib_core::wallet::wallet_version::WalletVersion;

#[derive(Clone)]
pub struct Signer {
    pub wallet: TonWallet,
}

#[derive(Debug, Clone)]
pub struct AccountState {
    pub balance: BigUint,
    pub seqno: u32,
    pub active: bool,
}

pub struct Chain {
    http: reqwest::Client,
    tonapi_key: String,
    toncenter_key: String,
}

#[derive(Debug, Deserialize)]
struct TonapiWallet {
    balance: String,
    seqno: u32,
    status: String,
}

#[derive(Debug, Deserialize)]
struct TonapiAccount {
    balance: String,
    status: String,
}

impl Signer {
    pub fn from_mnemonic(phrase: &str, version: WalletVersion) -> Result<Self> {
        let words = phrase.split_whitespace().collect::<Vec<_>>();
        if words.len() != 24 && words.len() != 12 {
            anyhow::bail!("мнемоника должна быть из 12 или 24 слов, сейчас {}", words.len());
        }
        let mnemonic = Mnemonic::from_str(&words.join(" "), &None).map_err(|err| anyhow!("мнемоника: {err}"))?;
        let key_pair: KeyPair = mnemonic.to_key_pair().map_err(|err| anyhow!("ключи: {err}"))?;
        let wallet = TonWallet::new(version, key_pair).map_err(|err| anyhow!("кошелёк: {err}"))?;
        Ok(Self { wallet })
    }

    pub fn address(&self) -> &TonAddress {
        &self.wallet.address
    }

    pub fn sign(&self, order: &SwapOrder, seqno: u32, deploy: bool) -> Result<String> {
        let internal = internal_message(&self.wallet.address, &order.destination, &order.value, &order.body)?;
        let expire = u32::try_from(unix_now()?.saturating_add(120)).unwrap_or(u32::MAX);
        let external = self
            .wallet
            .create_external_msg(expire, seqno, deploy, [internal.to_arc()])
            .map_err(|err| anyhow!("подпись: {err}"))?;
        let bytes = BagOfCells::from_root(external)
            .serialize(true)
            .map_err(|err| anyhow!("boc: {err}"))?;
        Ok(STANDARD.encode(bytes))
    }
}

impl Chain {
    pub fn new(http: reqwest::Client, tonapi_key: String, toncenter_key: String) -> Self {
        Self {
            http,
            tonapi_key,
            toncenter_key,
        }
    }

    pub async fn account(&self, address: &TonAddress) -> Result<AccountState> {
        let friendly = address.to_base64_url();
        let wallet_url = format!("https://tonapi.io/v2/wallet/{friendly}");
        let response = self.tonapi_get(&wallet_url).await?;
        if response.status().is_success() {
            let body = response.text().await.unwrap_or_default();
            let parsed: TonapiWallet = serde_json::from_str(&body).with_context(|| format!("кошелёк: {body}"))?;
            return Ok(AccountState {
                balance: BigUint::from_str(&parsed.balance).unwrap_or_else(|_| BigUint::zero()),
                seqno: parsed.seqno,
                active: parsed.status == "active",
            });
        }

        let account_url = format!("https://tonapi.io/v2/accounts/{friendly}");
        let response = self.tonapi_get(&account_url).await?;
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        if !status.is_success() {
            return Err(anyhow!("не прочитан счёт {friendly}: {status} {body}"));
        }
        let parsed: TonapiAccount = serde_json::from_str(&body).with_context(|| body.clone())?;
        Ok(AccountState {
            balance: BigUint::from_str(&parsed.balance).unwrap_or_else(|_| BigUint::zero()),
            seqno: 0,
            active: parsed.status == "active",
        })
    }

    pub async fn send(&self, boc: &str) -> Result<()> {
        let tonapi = self
            .http
            .post("https://tonapi.io/v2/blockchain/message")
            .bearer_auth(&self.tonapi_key)
            .json(&serde_json::json!({ "boc": boc }))
            .send()
            .await;
        match tonapi {
            Ok(response) if response.status().is_success() => return Ok(()),
            Ok(response) => {
                let status = response.status();
                let body = response.text().await.unwrap_or_default();
                tracing::warn!("TonAPI не принял сообщение: {status} {body}");
            }
            Err(err) => tracing::warn!("TonAPI недоступен: {err}"),
        }

        if self.toncenter_key.is_empty() {
            return Err(anyhow!("отправка через TonAPI не прошла, TONCENTER_API_KEY не задан"));
        }
        let response = self
            .http
            .post("https://toncenter.com/api/v2/jsonRPC")
            .header("X-API-Key", &self.toncenter_key)
            .json(&serde_json::json!({
                "id": 1,
                "jsonrpc": "2.0",
                "method": "sendBoc",
                "params": { "boc": boc }
            }))
            .send()
            .await
            .context("TonCenter не ответил")?;
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        if !status.is_success() || body.contains("\"ok\":false") {
            return Err(anyhow!("TonCenter {status}: {body}"));
        }
        Ok(())
    }

    async fn tonapi_get(&self, url: &str) -> Result<reqwest::Response> {
        self.http
            .get(url)
            .bearer_auth(&self.tonapi_key)
            .send()
            .await
            .with_context(|| format!("запрос {url}"))
    }
}

fn internal_message(src: &TonAddress, dest: &TonAddress, value: &BigUint, body: &Cell) -> Result<Cell> {
    let info = CommonMsgInfo::Int(IntMsgInfo {
        ihr_disabled: true,
        bounce: true,
        bounced: false,
        src: src.to_msg_address(),
        dest: dest.to_msg_address(),
        value: CurrencyCollection::new(value.clone()),
        ihr_fee: Grams::new(BigUint::zero()),
        fwd_fee: Grams::new(BigUint::zero()),
        created_lt: 0,
        created_at: 0,
    });
    let message = Message::new(info, body.to_arc());
    message.to_cell().map_err(|err| anyhow!("внутреннее сообщение: {err}"))
}

fn unix_now() -> Result<u64> {
    Ok(SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs())
}

trait CellArc {
    fn to_arc(&self) -> std::sync::Arc<Cell>;
}

impl CellArc for Cell {
    fn to_arc(&self) -> std::sync::Arc<Cell> {
        std::sync::Arc::new(self.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::amount::DEDUST_NATIVE_VAULT;
    use crate::swap::dedust_order;

    const PHRASE: &str = "fancy carpet hello mandate penalty trial consider property top vicious exit rebuild tragic profit urban major total month holiday sudden rib gather media vicious";

    #[test]
    fn signs_external_message() {
        let signer = Signer::from_mnemonic(PHRASE, WalletVersion::V4R2).unwrap();
        assert_eq!(
            signer.address().to_base64_url(),
            "EQCDM_QGggZ3qMa_f3lRPk4_qLDnLTqdi6OkMAV2NB9r5TG3"
        );
        let pool = TonAddress::from_str("EQCxE6mUtQJKFnGfaROTKOt1lZbDiiX1kCixRv7Nw2Id_sDs").unwrap();
        let vault = TonAddress::from_str(DEDUST_NATIVE_VAULT).unwrap();
        let order = dedust_order(
            &pool,
            &vault,
            signer.address(),
            signer.address(),
            &BigUint::from(500_000_000u64),
            &BigUint::from(20_000_000_000u64),
            &BigUint::from(9_000_000_000_000u64),
            25,
            "0.2",
            &BigUint::from(250_000_000u64),
            60,
        )
        .unwrap();
        let boc = signer.sign(&order, 1, false).unwrap();
        assert!(boc.starts_with("te6cc"));
    }
}
