use crate::dedust::estimate_out;
use crate::ston::Simulation;
use anyhow::{Context, Result};
use num_bigint::BigUint;
use num_traits::Zero;
use std::str::FromStr;
use std::time::{SystemTime, UNIX_EPOCH};
use tonlib_core::cell::{Cell, CellBuilder};
use tonlib_core::types::TonAddress;

const JETTON_TRANSFER: u32 = 0x0f8a7ea5;
const STON_V1_SWAP: u32 = 0x25938561;
const STON_V2_SWAP: u32 = 0x6664de2a;
const PTON_V2_TRANSFER: u32 = 0x01f3835d;
const DEDUST_NATIVE_SWAP: u32 = 0xea06185d;

const V1_FORWARD_GAS: u64 = 185_000_000;
const V2_FORWARD_GAS: u64 = 300_000_000;
const PTON_V2_GAS: u64 = 10_000_000;

#[derive(Debug)]
pub struct SwapOrder {
    pub destination: TonAddress,
    pub value: BigUint,
    pub body: Cell,
    pub min_out: BigUint,
    pub venue: &'static str,
}

pub fn stonfi_order(
    simulation: &Simulation,
    wallet: &TonAddress,
    referral: &TonAddress,
    referral_fee_bps: u16,
    deadline_secs: u64,
) -> Result<SwapOrder> {
    let offer = BigUint::from_str(&simulation.offer_units)?;
    let min_out = BigUint::from_str(&simulation.min_ask_units)?;
    let ask_wallet = TonAddress::from_str(&simulation.ask_jetton_wallet)?;
    let destination = TonAddress::from_str(&simulation.offer_jetton_wallet)?;
    let forward = simulation
        .gas_params
        .as_ref()
        .map(|gas| BigUint::from_str(&gas.forward_gas))
        .transpose()?
        .unwrap_or_else(|| {
            BigUint::from(if simulation.router.major_version >= 2 {
                V2_FORWARD_GAS
            } else {
                V1_FORWARD_GAS
            })
        });

    let (body, value) = if simulation.router.major_version >= 2 {
        let swap = v2_swap_body(&ask_wallet, wallet, referral, referral_fee_bps, &min_out, deadline_secs)?;
        let body = pton_v2_transfer(&offer, wallet, &swap)?;
        let value = &offer + &forward + BigUint::from(PTON_V2_GAS);
        (body, value)
    } else {
        let swap = v1_swap_body(&ask_wallet, wallet, referral, &min_out)?;
        let body = jetton_transfer(&offer, &TonAddress::from_str(&simulation.router.address)?, wallet, &forward, &swap)?;
        let value = &offer + &forward;
        (body, value)
    };

    Ok(SwapOrder {
        destination,
        value,
        body,
        min_out,
        venue: "STON.fi",
    })
}

pub fn dedust_order(
    pool: &TonAddress,
    vault: &TonAddress,
    wallet: &TonAddress,
    referral: &TonAddress,
    amount_in: &BigUint,
    reserve_ton: &BigUint,
    reserve_jetton: &BigUint,
    fee_bps: u32,
    slippage: &str,
    gas: &BigUint,
    deadline_secs: u64,
) -> Result<SwapOrder> {
    let expected = estimate_out(reserve_ton, reserve_jetton, amount_in, fee_bps);
    let min_out = crate::amount::after_slippage(&expected, slippage)?;
    let step = dedust_step(pool, &min_out)?;
    let params = dedust_params(wallet, referral, deadline_secs)?;
    let mut builder = CellBuilder::new();
    builder
        .store_u32(32, DEDUST_NATIVE_SWAP)
        .and_then(|cell| cell.store_u64(64, query_id()))
        .and_then(|cell| cell.store_coins(amount_in))
        .and_then(|cell| cell.store_cell(&step))
        .and_then(|cell| cell.store_reference(&params.to_arc()))
        .context("тело свопа DeDust")?;
    let body = builder.build().context("сборка свопа DeDust")?;
    Ok(SwapOrder {
        destination: vault.clone(),
        value: amount_in + gas,
        body,
        min_out,
        venue: "DeDust",
    })
}

fn v1_swap_body(ask_wallet: &TonAddress, user: &TonAddress, referral: &TonAddress, min_out: &BigUint) -> Result<Cell> {
    let mut builder = CellBuilder::new();
    builder
        .store_u32(32, STON_V1_SWAP)
        .and_then(|cell| cell.store_address(ask_wallet))
        .and_then(|cell| cell.store_coins(min_out))
        .and_then(|cell| cell.store_address(user))
        .and_then(|cell| cell.store_bit(true))
        .and_then(|cell| cell.store_address(referral))
        .context("payload STON.fi v1")?;
    builder.build().context("сборка payload STON.fi v1")
}

fn v2_swap_body(
    ask_wallet: &TonAddress,
    user: &TonAddress,
    referral: &TonAddress,
    referral_fee_bps: u16,
    min_out: &BigUint,
    deadline_secs: u64,
) -> Result<Cell> {
    let deadline = unix_now()?.saturating_add(deadline_secs);
    let mut details = CellBuilder::new();
    details
        .store_coins(min_out)
        .and_then(|cell| cell.store_address(user))
        .and_then(|cell| cell.store_coins(&BigUint::from(0u32)))
        .and_then(|cell| cell.store_ref_cell_optional(None))
        .and_then(|cell| cell.store_coins(&BigUint::from(0u32)))
        .and_then(|cell| cell.store_ref_cell_optional(None))
        .and_then(|cell| cell.store_u32(16, u32::from(referral_fee_bps)))
        .and_then(|cell| cell.store_address(referral))
        .context("доп. данные STON.fi v2")?;
    let details = details.build()?.to_arc();

    let mut builder = CellBuilder::new();
    builder
        .store_u32(32, STON_V2_SWAP)
        .and_then(|cell| cell.store_address(ask_wallet))
        .and_then(|cell| cell.store_address(user))
        .and_then(|cell| cell.store_address(user))
        .and_then(|cell| cell.store_u64(64, deadline))
        .and_then(|cell| cell.store_reference(&details))
        .context("payload STON.fi v2")?;
    builder.build().context("сборка payload STON.fi v2")
}

fn pton_v2_transfer(amount: &BigUint, refund: &TonAddress, forward_payload: &Cell) -> Result<Cell> {
    let mut builder = CellBuilder::new();
    builder
        .store_u32(32, PTON_V2_TRANSFER)
        .and_then(|cell| cell.store_u64(64, query_id()))
        .and_then(|cell| cell.store_coins(amount))
        .and_then(|cell| cell.store_address(refund))
        .and_then(|cell| cell.store_ref_cell_optional(Some(&forward_payload.to_arc())))
        .context("перевод pTON v2")?;
    builder.build().context("сборка перевода pTON v2")
}

fn jetton_transfer(
    amount: &BigUint,
    destination: &TonAddress,
    response: &TonAddress,
    forward_ton: &BigUint,
    forward_payload: &Cell,
) -> Result<Cell> {
    let mut builder = CellBuilder::new();
    builder
        .store_u32(32, JETTON_TRANSFER)
        .and_then(|cell| cell.store_u64(64, query_id()))
        .and_then(|cell| cell.store_coins(amount))
        .and_then(|cell| cell.store_address(destination))
        .and_then(|cell| cell.store_address(response))
        .and_then(|cell| cell.store_ref_cell_optional(None))
        .and_then(|cell| cell.store_coins(forward_ton))
        .and_then(|cell| cell.store_bit(true))
        .and_then(|cell| cell.store_reference(&forward_payload.to_arc()))
        .context("jetton transfer")?;
    builder.build().context("сборка jetton transfer")
}

fn dedust_step(pool: &TonAddress, limit: &BigUint) -> Result<Cell> {
    // step#_ pool_addr:MsgAddressInt kind:SwapKind limit:Coins next:(Maybe ^SwapStep)
    // given_in — один нулевой бит. Следующего шага нет.
    let mut builder = CellBuilder::new();
    builder
        .store_address(pool)
        .and_then(|cell| cell.store_bit(false))
        .and_then(|cell| cell.store_coins(limit))
        .and_then(|cell| cell.store_bit(false))
        .context("шаг DeDust")?;
    builder.build().context("сборка шага DeDust")
}

fn dedust_params(recipient: &TonAddress, referral: &TonAddress, deadline_secs: u64) -> Result<Cell> {
    let deadline = u32::try_from(unix_now()?.saturating_add(deadline_secs)).unwrap_or(u32::MAX);
    let referral = if referral == &TonAddress::NULL {
        recipient
    } else {
        referral
    };
    let mut builder = CellBuilder::new();
    builder
        .store_u32(32, deadline)
        .and_then(|cell| cell.store_address(recipient))
        .and_then(|cell| cell.store_address(referral))
        .and_then(|cell| cell.store_ref_cell_optional(None))
        .and_then(|cell| cell.store_ref_cell_optional(None))
        .context("параметры DeDust")?;
    builder.build().context("сборка параметров DeDust")
}

fn query_id() -> u64 {
    unix_now().unwrap_or(0) & 0x0000_ffff_ffff_ffff
}

fn unix_now() -> Result<u64> {
    Ok(SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs())
}

trait ToArc {
    fn to_arc(&self) -> std::sync::Arc<Cell>;
}

impl ToArc for Cell {
    fn to_arc(&self) -> std::sync::Arc<Cell> {
        std::sync::Arc::new(self.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn addr() -> TonAddress {
        TonAddress::from_str("EQCxE6mUtQJKFnGfaROTKOt1lZbDiiX1kCixRv7Nw2Id_sDs").unwrap()
    }

    #[test]
    fn v1_payload_starts_with_opcode() {
        let cell = v1_swap_body(&addr(), &addr(), &addr(), &BigUint::from(10u32)).unwrap();
        let mut parser = cell.parser();
        assert_eq!(parser.load_u32(32).unwrap(), STON_V1_SWAP);
    }

    #[test]
    fn dedust_payload_fits_one_cell() {
        let order = dedust_order(
            &addr(),
            &addr(),
            &addr(),
            &TonAddress::NULL,
            &BigUint::from(1_000_000_000u64),
            &BigUint::from(50_000_000_000u64),
            &BigUint::from(1_000_000_000_000u64),
            25,
            "0.15",
            &BigUint::from(250_000_000u64),
            90,
        )
        .unwrap();
        let mut parser = order.body.parser();
        assert_eq!(parser.load_u32(32).unwrap(), DEDUST_NATIVE_SWAP);
        assert!(order.value > BigUint::from(1_000_000_000u64));
        assert!(!order.min_out.is_zero());
    }
}
