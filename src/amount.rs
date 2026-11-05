use anyhow::{anyhow, bail, Result};
use num_bigint::BigUint;
use num_traits::Zero;
use std::str::FromStr;
use tonlib_core::types::TonAddress;

pub const TON_ASSET: &str = "EQAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAM9c";
pub const DEDUST_FACTORY: &str = "EQBfBWT7X2BHg9tXAxzhz2aKiNTU1tpt5NsiK0uSDW_YAJ67";
pub const DEDUST_NATIVE_VAULT: &str = "EQDa4VOnTYlLvDJ0gZjNYm5PXfSmmtL6Vs6A_CZEtXCNICq_";

pub fn parse_address(raw: &str) -> Result<TonAddress> {
    TonAddress::from_str(raw.trim()).map_err(|err| anyhow!("адрес {raw}: {err}"))
}

/// Сравнение адресов в виде `workchain:hex`, чтобы EQ и UQ совпали.
pub fn address_key(raw: &str) -> String {
    parse_address(raw)
        .map(|addr| addr.to_hex())
        .unwrap_or_else(|_| raw.trim().to_string())
}

pub fn is_ton_asset(raw: &str) -> bool {
    address_key(raw) == address_key(TON_ASSET)
}

pub fn ton_to_nano(input: &str) -> Result<BigUint> {
    let (num, den) = decimal_ratio(input, 9)?;
    let nano = BigUint::from(1_000_000_000u64);
    if &den > &nano {
        bail!("у {input} больше 9 знаков после запятой");
    }
    Ok(num * (nano / den))
}

pub fn scale_nano(amount: &BigUint, scale: &str) -> Result<BigUint> {
    let (num, den) = decimal_ratio(scale, 8)?;
    if den.is_zero() {
        bail!("нулевой множитель");
    }
    Ok(amount * num / den)
}

/// Оставляет долю `(1 - slippage)`. `0.15` оставляет 85%.
pub fn after_slippage(amount: &BigUint, slippage: &str) -> Result<BigUint> {
    let (num, den) = decimal_ratio(slippage, 8)?;
    if num > den {
        bail!("проскальзывание больше 100%");
    }
    let kept = amount * (&den - &num) / &den;
    if kept.is_zero() {
        Ok(BigUint::from(1u32))
    } else {
        Ok(kept)
    }
}

pub fn format_ton(nano: &BigUint) -> String {
    let base = BigUint::from(1_000_000_000u64);
    let whole = nano / &base;
    let frac = (nano % &base).to_string();
    format!("{whole}.{:0>9}", frac)
}

fn decimal_ratio(input: &str, max_frac: usize) -> Result<(BigUint, BigUint)> {
    let input = input.trim();
    if input.is_empty() {
        bail!("пустое число");
    }
    let (whole, frac) = match input.split_once('.') {
        Some((whole, frac)) => (whole, frac),
        None => (input, ""),
    };
    if frac.len() > max_frac {
        bail!("{input}: слишком много знаков после запятой");
    }
    if !whole.chars().all(|ch| ch.is_ascii_digit()) || !frac.chars().all(|ch| ch.is_ascii_digit()) {
        bail!("{input} не похоже на число");
    }
    let digits = format!("{whole}{frac}");
    let digits = if digits.is_empty() { "0".to_string() } else { digits };
    let den = BigUint::from(10u32).pow(frac.len() as u32);
    Ok((BigUint::from_str(&digits)?, den))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_half_ton() {
        assert_eq!(ton_to_nano("0.5").unwrap(), BigUint::from(500_000_000u64));
        assert_eq!(ton_to_nano("1").unwrap(), BigUint::from(1_000_000_000u64));
        assert_eq!(format_ton(&ton_to_nano("1.25").unwrap()), "1.250000000");
    }

    #[test]
    fn scales_amount() {
        let amount = ton_to_nano("2").unwrap();
        assert_eq!(scale_nano(&amount, "0.25").unwrap(), ton_to_nano("0.5").unwrap());
    }

    #[test]
    fn friendly_and_raw_match() {
        let friendly = "EQCxE6mUtQJKFnGfaROTKOt1lZbDiiX1kCixRv7Nw2Id_sDs";
        let addr = parse_address(friendly).unwrap();
        assert_eq!(address_key(friendly), address_key(&addr.to_hex()));
    }
}
