use anchor_lang::prelude::*;

use crate::{constants::BPS_DENOMINATOR, errors::OmnindexError};

pub fn index_base_units(decimals: u8) -> Result<u64> {
    10u64
        .checked_pow(decimals as u32)
        .ok_or_else(|| error!(OmnindexError::ArithmeticOverflow))
}

pub fn quote_component_amount(
    units_per_index: u64,
    index_amount: u64,
    index_base_units: u64,
) -> Result<u64> {
    let numerator = u128::from(units_per_index)
        .checked_mul(u128::from(index_amount))
        .ok_or_else(|| error!(OmnindexError::ArithmeticOverflow))?;

    require!(index_base_units > 0, OmnindexError::ArithmeticOverflow);
    require!(
        numerator % u128::from(index_base_units) == 0,
        OmnindexError::NonIntegralBasketAmount
    );

    u64::try_from(numerator / u128::from(index_base_units))
        .map_err(|_| error!(OmnindexError::ArithmeticOverflow))
}

pub fn mul_div_floor_u64(a: u64, b: u64, denominator: u64) -> Result<u64> {
    require!(denominator > 0, OmnindexError::ArithmeticOverflow);
    let numerator = u128::from(a)
        .checked_mul(u128::from(b))
        .ok_or_else(|| error!(OmnindexError::ArithmeticOverflow))?;
    u64::try_from(numerator / u128::from(denominator))
        .map_err(|_| error!(OmnindexError::ArithmeticOverflow))
}

pub fn mul_div_ceil_u64(a: u64, b: u64, denominator: u64) -> Result<u64> {
    require!(denominator > 0, OmnindexError::ArithmeticOverflow);
    let numerator = u128::from(a)
        .checked_mul(u128::from(b))
        .ok_or_else(|| error!(OmnindexError::ArithmeticOverflow))?;
    let denominator = u128::from(denominator);
    let value = numerator
        .checked_add(denominator - 1)
        .and_then(|value| value.checked_div(denominator))
        .ok_or_else(|| error!(OmnindexError::ArithmeticOverflow))?;
    u64::try_from(value).map_err(|_| error!(OmnindexError::ArithmeticOverflow))
}

pub fn pro_rata_mint_amount(index_amount: u64, vault_amount: u64, supply: u64) -> Result<u64> {
    mul_div_ceil_u64(index_amount, vault_amount, supply)
}

pub fn pro_rata_redeem_amount(index_amount: u64, vault_amount: u64, supply: u64) -> Result<u64> {
    mul_div_floor_u64(index_amount, vault_amount, supply)
}

pub fn units_per_index_for_amount(
    vault_amount: u64,
    index_base_units: u64,
    supply: u64,
) -> Result<u64> {
    mul_div_floor_u64(vault_amount, index_base_units, supply)
}

pub fn basis_points_amount(amount: u64, bps: u16) -> Result<u64> {
    require!(bps <= BPS_DENOMINATOR, OmnindexError::InvalidFeeBps);
    if amount == 0 || bps == 0 {
        return Ok(0);
    }

    let numerator = u128::from(amount)
        .checked_mul(u128::from(bps))
        .ok_or_else(|| error!(OmnindexError::ArithmeticOverflow))?;
    let denominator = u128::from(BPS_DENOMINATOR);
    let fee = numerator
        .checked_add(denominator - 1)
        .and_then(|value| value.checked_div(denominator))
        .ok_or_else(|| error!(OmnindexError::ArithmeticOverflow))?;

    u64::try_from(fee).map_err(|_| error!(OmnindexError::ArithmeticOverflow))
}

pub fn add_fee(amount: u64, fee_bps: u16) -> Result<(u64, u64)> {
    let fee = basis_points_amount(amount, fee_bps)?;
    let total = amount
        .checked_add(fee)
        .ok_or_else(|| error!(OmnindexError::ArithmeticOverflow))?;
    Ok((total, fee))
}

pub fn subtract_fee(amount: u64, fee_bps: u16) -> Result<(u64, u64)> {
    let fee = basis_points_amount(amount, fee_bps)?;
    let net = amount
        .checked_sub(fee)
        .ok_or_else(|| error!(OmnindexError::ArithmeticOverflow))?;
    Ok((net, fee))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quotes_fractional_index_amounts_when_underlying_units_support_it() {
        let amount = quote_component_amount(1_000_000, 2_500_000, 1_000_000).unwrap();
        assert_eq!(amount, 2_500_000);
    }

    #[test]
    fn rejects_non_integral_underlying_amounts() {
        let result = quote_component_amount(1, 1, 2);
        assert!(result.is_err());
    }

    #[test]
    fn computes_base_units_from_decimals() {
        let base_units = index_base_units(6).unwrap();
        assert_eq!(base_units, 1_000_000);
    }

    #[test]
    fn pro_rata_mint_rounds_up_to_protect_existing_holders() {
        assert_eq!(pro_rata_mint_amount(1, 10, 3).unwrap(), 4);
    }

    #[test]
    fn pro_rata_redeem_rounds_down_to_protect_remaining_holders() {
        assert_eq!(pro_rata_redeem_amount(1, 10, 3).unwrap(), 3);
    }

    #[test]
    fn computes_basis_point_amounts() {
        let fee = basis_points_amount(1_000_000, 25).unwrap();
        assert_eq!(fee, 2_500);
    }

    #[test]
    fn rounds_nonzero_basis_point_fees_up() {
        assert_eq!(basis_points_amount(199, 50).unwrap(), 1);
        assert_eq!(basis_points_amount(1, 1).unwrap(), 1);
        assert_eq!(basis_points_amount(1, 0).unwrap(), 0);
    }

    #[test]
    fn adds_and_subtracts_fees() {
        assert_eq!(add_fee(1_000, 50).unwrap(), (1_005, 5));
        assert_eq!(subtract_fee(1_000, 50).unwrap(), (995, 5));
    }
}
