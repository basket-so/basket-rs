use anchor_lang::prelude::*;

use crate::{constants::MAX_PRICE_AGE_SLOTS, errors::BasketError, state::PriceBoard};

/// Scale of every oracle price: USD per whole token × 1e18.
pub const PRICE_SCALE: u128 = 1_000_000_000_000_000_000;
/// Divides a `PRICE_SCALE` value down to the 1e9 ("nad") scale NAVs are recorded in.
pub const PRICE_NAD_SCALE_FACTOR: u128 = 1_000_000_000;

pub fn validate_price_age_slots(max_age_slots: u64) -> Result<()> {
    require!(
        max_age_slots <= MAX_PRICE_AGE_SLOTS,
        BasketError::InvalidOraclePriceAge
    );
    Ok(())
}

/// The board's price for `mint`, posted at most `max_age_slots` before `current_slot`.
pub fn board_price(
    board: &PriceBoard,
    mint: &Pubkey,
    current_slot: u64,
    max_age_slots: u64,
) -> Result<i128> {
    validate_price_age_slots(max_age_slots)?;
    let entry = board
        .prices
        .iter()
        .find(|entry| entry.mint == *mint)
        .ok_or_else(|| error!(BasketError::MissingOraclePrice))?;
    require!(
        current_slot.saturating_sub(entry.posted_slot) <= max_age_slots,
        BasketError::StaleOraclePrice
    );
    let price = i128::try_from(entry.price).map_err(|_| error!(BasketError::InvalidOraclePrice))?;
    require!(price > 0, BasketError::InvalidOraclePrice);
    Ok(price)
}

pub fn execution_price_scaled(
    quote_atoms: u64,
    component_atoms: u64,
    quote_decimals: u8,
    component_decimals: u8,
) -> Result<u128> {
    require!(component_atoms > 0, BasketError::InvalidIndexAmount);
    let quote_factor = pow10_u128(quote_decimals)?;
    let component_factor = pow10_u128(component_decimals)?;
    u128::from(quote_atoms)
        .checked_mul(component_factor)
        .and_then(|value| value.checked_mul(PRICE_SCALE))
        .and_then(|value| value.checked_div(u128::from(component_atoms)))
        .and_then(|value| value.checked_div(quote_factor))
        .ok_or_else(|| error!(BasketError::ArithmeticOverflow))
}

pub fn validate_buy_execution_price(
    quote_atoms: u64,
    component_atoms: u64,
    quote_decimals: u8,
    component_decimals: u8,
    oracle_price: i128,
    max_slippage_bps: u16,
) -> Result<()> {
    let effective = execution_price_scaled(
        quote_atoms,
        component_atoms,
        quote_decimals,
        component_decimals,
    )?;
    let max_price = oracle_price_with_bps(oracle_price, max_slippage_bps, true)?;
    require!(
        effective <= max_price,
        BasketError::ExecutionPriceOutsideOracleTolerance
    );
    Ok(())
}

pub fn validate_sell_execution_price(
    quote_atoms: u64,
    component_atoms: u64,
    quote_decimals: u8,
    component_decimals: u8,
    oracle_price: i128,
    max_slippage_bps: u16,
) -> Result<()> {
    let effective = execution_price_scaled(
        quote_atoms,
        component_atoms,
        quote_decimals,
        component_decimals,
    )?;
    let min_price = oracle_price_with_bps(oracle_price, max_slippage_bps, false)?;
    require!(
        effective >= min_price,
        BasketError::ExecutionPriceOutsideOracleTolerance
    );
    Ok(())
}

fn oracle_price_with_bps(price: i128, bps: u16, add: bool) -> Result<u128> {
    require!(bps <= 10_000, BasketError::InvalidOraclePriceTolerance);
    require!(price > 0, BasketError::InvalidOraclePrice);
    let price = u128::try_from(price).map_err(|_| error!(BasketError::InvalidOraclePrice))?;
    let bps = u128::from(bps);
    let numerator = if add {
        10_000u128
            .checked_add(bps)
            .ok_or_else(|| error!(BasketError::ArithmeticOverflow))?
    } else {
        10_000u128
            .checked_sub(bps)
            .ok_or_else(|| error!(BasketError::ArithmeticOverflow))?
    };
    price
        .checked_mul(numerator)
        .and_then(|value| value.checked_div(10_000))
        .ok_or_else(|| error!(BasketError::ArithmeticOverflow))
}

fn pow10_u128(decimals: u8) -> Result<u128> {
    let mut value = 1u128;
    for _ in 0..decimals {
        value = value
            .checked_mul(10)
            .ok_or_else(|| error!(BasketError::ArithmeticOverflow))?;
    }
    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::BoardPrice;

    #[test]
    fn execution_price_scales_decimals() {
        let price = execution_price_scaled(250_000, 500_000, 6, 6).unwrap();
        assert_eq!(price, PRICE_SCALE / 2);
    }

    #[test]
    fn buy_price_allows_configured_slippage() {
        let oracle = PRICE_SCALE as i128;
        assert!(validate_buy_execution_price(101, 100, 0, 0, oracle, 100).is_ok());
        assert!(validate_buy_execution_price(102, 100, 0, 0, oracle, 100).is_err());
    }

    #[test]
    fn sell_price_allows_configured_slippage() {
        let oracle = PRICE_SCALE as i128;
        assert!(validate_sell_execution_price(99, 100, 0, 0, oracle, 100).is_ok());
        assert!(validate_sell_execution_price(98, 100, 0, 0, oracle, 100).is_err());
    }

    #[test]
    fn rejects_unbounded_price_age() {
        assert!(validate_price_age_slots(MAX_PRICE_AGE_SLOTS).is_ok());
        assert!(validate_price_age_slots(MAX_PRICE_AGE_SLOTS + 1).is_err());
    }

    #[test]
    fn board_prices_must_be_present_and_fresh() {
        let mint = Pubkey::new_unique();
        let board = PriceBoard {
            oracle: Pubkey::new_unique(),
            bump: 255,
            reserved: [0; 32],
            prices: vec![BoardPrice { mint, price: 2 * PRICE_SCALE, posted_slot: 1_000 }],
        };
        assert_eq!(board_price(&board, &mint, 1_100, 150).unwrap(), 2 * PRICE_SCALE as i128);
        assert_eq!(board_price(&board, &mint, 1_150, 150).unwrap(), 2 * PRICE_SCALE as i128);
        assert!(board_price(&board, &mint, 1_151, 150).is_err(), "older than the allowed age");
        assert!(board_price(&board, &mint, 1_100, 50).is_err(), "caller asked for a tighter age");
        assert!(board_price(&board, &Pubkey::new_unique(), 1_100, 150).is_err(), "never posted");
        assert!(board_price(&board, &mint, 1_100, MAX_PRICE_AGE_SLOTS + 1).is_err());
    }
}
