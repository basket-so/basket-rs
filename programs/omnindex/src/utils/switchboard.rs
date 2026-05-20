use anchor_lang::prelude::*;
use switchboard_on_demand::{
    QuoteVerifier, ON_DEMAND_DEVNET_PID, ON_DEMAND_MAINNET_PID, QUOTE_PROGRAM_ID,
};

use crate::{constants::MAX_SWITCHBOARD_QUOTE_AGE_SLOTS, errors::OmnindexError};

pub const SWITCHBOARD_PRICE_SCALE: u128 = 1_000_000_000_000_000_000;
pub const SWITCHBOARD_NAD_SCALE_FACTOR: u128 = 1_000_000_000;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SwitchboardPrice {
    pub feed_id: [u8; 32],
    pub price: i128,
}

pub fn verified_switchboard_prices<'info>(
    queue: &AccountInfo<'info>,
    quote_account: &AccountInfo<'info>,
    slothashes: &AccountInfo<'info>,
    instructions: &AccountInfo<'info>,
    current_slot: u64,
    max_age_slots: u64,
) -> Result<Vec<SwitchboardPrice>> {
    validate_switchboard_max_age_slots(max_age_slots)?;
    require!(
        *queue.owner == ON_DEMAND_MAINNET_PID || *queue.owner == ON_DEMAND_DEVNET_PID,
        OmnindexError::SwitchboardVerificationFailed
    );
    require_keys_eq!(
        *quote_account.owner,
        QUOTE_PROGRAM_ID,
        OmnindexError::SwitchboardVerificationFailed
    );

    let quote_account_data = quote_account.try_borrow_data()?;
    require!(
        quote_account_data.len() >= 40,
        OmnindexError::SwitchboardVerificationFailed
    );
    require!(
        &quote_account_data[..8] == b"SBOracle",
        OmnindexError::SwitchboardVerificationFailed
    );
    require!(
        &quote_account_data[8..40] == queue.key.as_ref(),
        OmnindexError::SwitchboardVerificationFailed
    );

    let quote = QuoteVerifier::new()
        .queue(queue)
        .slothash_sysvar(slothashes)
        .ix_sysvar(instructions)
        .clock_slot(current_slot)
        .max_age(max_age_slots)
        .verify_delimited(&quote_account_data[40..])
        .map_err(|_| error!(OmnindexError::SwitchboardVerificationFailed))?;

    let canonical_key = quote.canonical_key(queue.key, quote_account.owner);
    require_keys_eq!(
        canonical_key,
        quote_account.key(),
        OmnindexError::SwitchboardVerificationFailed
    );

    Ok(quote
        .feeds()
        .iter()
        .map(|feed| SwitchboardPrice {
            feed_id: *feed.feed_id(),
            price: feed.feed_value(),
        })
        .collect())
}

pub fn validate_switchboard_max_age_slots(max_age_slots: u64) -> Result<()> {
    require!(
        max_age_slots <= MAX_SWITCHBOARD_QUOTE_AGE_SLOTS,
        OmnindexError::InvalidSwitchboardMaxAge
    );
    Ok(())
}

pub fn switchboard_feed_price(prices: &[SwitchboardPrice], feed_id: &Pubkey) -> Result<i128> {
    let feed_id = feed_id.to_bytes();
    let feed = prices
        .iter()
        .find(|feed| feed.feed_id == feed_id)
        .ok_or_else(|| error!(OmnindexError::MissingSwitchboardFeed))?;
    let value = feed.price;
    require!(value > 0, OmnindexError::InvalidSwitchboardPrice);
    Ok(value)
}

pub fn switchboard_price_to_nad(price: i128) -> Result<u64> {
    require!(price > 0, OmnindexError::InvalidSwitchboardPrice);
    let price =
        u128::try_from(price).map_err(|_| error!(OmnindexError::InvalidSwitchboardPrice))?;
    let nad = price
        .checked_div(SWITCHBOARD_NAD_SCALE_FACTOR)
        .ok_or_else(|| error!(OmnindexError::ArithmeticOverflow))?;
    require!(nad > 0, OmnindexError::InvalidSwitchboardPrice);
    u64::try_from(nad).map_err(|_| error!(OmnindexError::ArithmeticOverflow))
}

pub fn execution_price_scaled(
    quote_atoms: u64,
    component_atoms: u64,
    quote_decimals: u8,
    component_decimals: u8,
) -> Result<u128> {
    require!(component_atoms > 0, OmnindexError::InvalidIndexAmount);
    let quote_factor = pow10_u128(quote_decimals)?;
    let component_factor = pow10_u128(component_decimals)?;
    u128::from(quote_atoms)
        .checked_mul(component_factor)
        .and_then(|value| value.checked_mul(SWITCHBOARD_PRICE_SCALE))
        .and_then(|value| value.checked_div(u128::from(component_atoms)))
        .and_then(|value| value.checked_div(quote_factor))
        .ok_or_else(|| error!(OmnindexError::ArithmeticOverflow))
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
        OmnindexError::ExecutionPriceOutsideOracleTolerance
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
        OmnindexError::ExecutionPriceOutsideOracleTolerance
    );
    Ok(())
}

fn oracle_price_with_bps(price: i128, bps: u16, add: bool) -> Result<u128> {
    require!(bps <= 10_000, OmnindexError::InvalidOraclePriceTolerance);
    require!(price > 0, OmnindexError::InvalidSwitchboardPrice);
    let price =
        u128::try_from(price).map_err(|_| error!(OmnindexError::InvalidSwitchboardPrice))?;
    let bps = u128::from(bps);
    let numerator = if add {
        10_000u128
            .checked_add(bps)
            .ok_or_else(|| error!(OmnindexError::ArithmeticOverflow))?
    } else {
        10_000u128
            .checked_sub(bps)
            .ok_or_else(|| error!(OmnindexError::ArithmeticOverflow))?
    };
    price
        .checked_mul(numerator)
        .and_then(|value| value.checked_div(10_000))
        .ok_or_else(|| error!(OmnindexError::ArithmeticOverflow))
}

fn pow10_u128(decimals: u8) -> Result<u128> {
    let mut value = 1u128;
    for _ in 0..decimals {
        value = value
            .checked_mul(10)
            .ok_or_else(|| error!(OmnindexError::ArithmeticOverflow))?;
    }
    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn execution_price_scales_decimals() {
        let price = execution_price_scaled(250_000, 500_000, 6, 6).unwrap();
        assert_eq!(price, SWITCHBOARD_PRICE_SCALE / 2);
    }

    #[test]
    fn buy_price_allows_configured_slippage() {
        let oracle = SWITCHBOARD_PRICE_SCALE as i128;
        assert!(validate_buy_execution_price(101, 100, 0, 0, oracle, 100).is_ok());
        assert!(validate_buy_execution_price(102, 100, 0, 0, oracle, 100).is_err());
    }

    #[test]
    fn sell_price_allows_configured_slippage() {
        let oracle = SWITCHBOARD_PRICE_SCALE as i128;
        assert!(validate_sell_execution_price(99, 100, 0, 0, oracle, 100).is_ok());
        assert!(validate_sell_execution_price(98, 100, 0, 0, oracle, 100).is_err());
    }

    #[test]
    fn rejects_unbounded_switchboard_quote_age() {
        assert!(validate_switchboard_max_age_slots(MAX_SWITCHBOARD_QUOTE_AGE_SLOTS).is_ok());
        assert!(validate_switchboard_max_age_slots(MAX_SWITCHBOARD_QUOTE_AGE_SLOTS + 1).is_err());
    }
}
