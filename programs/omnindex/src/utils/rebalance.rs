use anchor_lang::prelude::*;

use crate::{
    constants::{BPS_DENOMINATOR, USDC_DECIMALS, USDC_MINT},
    errors::OmnindexError,
    state::IndexComponent,
    utils::{switchboard_feed_price, switchboard_price_to_nad, SwitchboardPrice},
};

pub const REBALANCE_PRICE_SCALE: u64 = 1_000_000_000;

#[derive(AnchorSerialize, AnchorDeserialize, Clone, Debug)]
pub struct RebalancePriceInput {
    pub mint: Pubkey,
    pub price_nad: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RebalancePrice {
    pub mint: Pubkey,
    pub price_nad: u64,
    pub decimals: u8,
}

pub fn validate_rebalance_quote_mint(quote_mint: &Pubkey) -> Result<()> {
    require_keys_eq!(*quote_mint, USDC_MINT, OmnindexError::InvalidQuoteMint);
    Ok(())
}

pub fn resolve_rebalance_prices(
    rebalance_mints: &[Pubkey],
    old_components: &[IndexComponent],
    new_components: &[IndexComponent],
    mint_decimals: &[u8],
    price_inputs: &[RebalancePriceInput],
    oracle_price_tolerance_bps: u16,
    switchboard_prices: &[SwitchboardPrice],
) -> Result<Vec<RebalancePrice>> {
    require!(
        price_inputs.len() == rebalance_mints.len() && mint_decimals.len() == rebalance_mints.len(),
        OmnindexError::InvalidRebalancePriceInput
    );

    let mut prices = Vec::with_capacity(rebalance_mints.len());

    for ((expected_mint, price_input), decimals) in rebalance_mints
        .iter()
        .zip(price_inputs.iter())
        .zip(mint_decimals.iter().copied())
    {
        require_keys_eq!(
            price_input.mint,
            *expected_mint,
            OmnindexError::InvalidRebalancePriceInput
        );

        let oracle_price_nad = if *expected_mint == USDC_MINT {
            require!(
                decimals == USDC_DECIMALS,
                OmnindexError::InvalidRebalancePriceInput
            );
            REBALANCE_PRICE_SCALE
        } else {
            let feed = oracle_feed_for_mint(old_components, new_components, expected_mint)?;
            let price = switchboard_feed_price(switchboard_prices, &feed)?;
            switchboard_price_to_nad(price)?
        };

        let price_nad = match price_input.price_nad {
            Some(price_nad) => {
                require!(price_nad > 0, OmnindexError::InvalidRebalancePriceInput);
                require!(
                    within_bps_tolerance_u64(
                        oracle_price_nad,
                        price_nad,
                        oracle_price_tolerance_bps,
                    )?,
                    OmnindexError::PriceOutsideOracleTolerance
                );
                price_nad
            }
            None => oracle_price_nad,
        };

        prices.push(RebalancePrice {
            mint: *expected_mint,
            price_nad,
            decimals,
        });
    }

    Ok(prices)
}

pub fn rebalance_price_for_mint<'a>(
    prices: &'a [RebalancePrice],
    mint: &Pubkey,
) -> Result<&'a RebalancePrice> {
    prices
        .iter()
        .find(|price| price.mint == *mint)
        .ok_or_else(|| error!(OmnindexError::InvalidRebalancePriceInput))
}

pub fn nav_nad(components: &[IndexComponent], prices: &[RebalancePrice]) -> Result<u128> {
    let mut nav = 0u128;

    for component in components {
        let price = rebalance_price_for_mint(prices, &component.mint)?;
        let component_value =
            token_value_nad(component.units_per_index, price.decimals, price.price_nad)?;
        nav = nav
            .checked_add(component_value)
            .ok_or_else(|| error!(OmnindexError::ArithmeticOverflow))?;
    }

    Ok(nav)
}

pub fn token_value_nad(amount: u64, decimals: u8, price_nad: u64) -> Result<u128> {
    let denominator = pow10_u128(decimals)?;
    u128::from(amount)
        .checked_mul(u128::from(price_nad))
        .and_then(|value| value.checked_div(denominator))
        .ok_or_else(|| error!(OmnindexError::ArithmeticOverflow))
}

pub fn validate_rebalance_execution_value(
    input_amount: u64,
    input_price: &RebalancePrice,
    output_amount: u64,
    output_price: &RebalancePrice,
    max_slippage_bps: u16,
) -> Result<()> {
    require!(
        max_slippage_bps <= BPS_DENOMINATOR,
        OmnindexError::InvalidOraclePriceTolerance
    );
    let input_value = token_value_nad(input_amount, input_price.decimals, input_price.price_nad)?;
    let output_value =
        token_value_nad(output_amount, output_price.decimals, output_price.price_nad)?;
    let min_output_value = input_value
        .checked_mul(u128::from(BPS_DENOMINATOR - max_slippage_bps))
        .and_then(|value| value.checked_div(u128::from(BPS_DENOMINATOR)))
        .ok_or_else(|| error!(OmnindexError::ArithmeticOverflow))?;
    require!(
        output_value >= min_output_value,
        OmnindexError::ExecutionPriceOutsideOracleTolerance
    );
    Ok(())
}

fn oracle_feed_for_mint(
    old_components: &[IndexComponent],
    new_components: &[IndexComponent],
    mint: &Pubkey,
) -> Result<Pubkey> {
    let mut feed = None;

    for component in old_components.iter().chain(new_components.iter()) {
        if component.mint != *mint || component.oracle_pair == Pubkey::default() {
            continue;
        }

        if let Some(existing) = feed {
            require_keys_eq!(
                existing,
                component.oracle_pair,
                OmnindexError::InvalidRebalancePriceInput
            );
        } else {
            feed = Some(component.oracle_pair);
        }
    }

    feed.ok_or_else(|| error!(OmnindexError::MissingSwitchboardFeed))
}

fn within_bps_tolerance_u64(reference: u64, value: u64, tolerance_bps: u16) -> Result<bool> {
    within_bps_tolerance_u128(u128::from(reference), u128::from(value), tolerance_bps)
}

pub fn within_bps_tolerance_u128(reference: u128, value: u128, tolerance_bps: u16) -> Result<bool> {
    let diff = reference.abs_diff(value);
    let max_diff = reference
        .checked_mul(u128::from(tolerance_bps))
        .ok_or_else(|| error!(OmnindexError::ArithmeticOverflow))?
        .checked_div(u128::from(BPS_DENOMINATOR))
        .ok_or_else(|| error!(OmnindexError::ArithmeticOverflow))?;

    Ok(diff <= max_diff)
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

    fn component(mint: Pubkey, units_per_index: u64) -> IndexComponent {
        IndexComponent {
            mint,
            units_per_index,
            target_weight_bps: 0,
            oracle_pair: Pubkey::new_unique(),
        }
    }

    fn price(mint: Pubkey, value: u64) -> RebalancePrice {
        RebalancePrice {
            mint,
            price_nad: value * REBALANCE_PRICE_SCALE,
            decimals: 6,
        }
    }

    #[test]
    fn adding_component_requires_reduced_existing_weights_to_preserve_nav() {
        let abc = Pubkey::new_unique();
        let xyz = Pubkey::new_unique();
        let dfg = Pubkey::new_unique();
        let lmn = Pubkey::new_unique();
        let prices = vec![price(abc, 8), price(xyz, 5), price(dfg, 3), price(lmn, 16)];
        let old_components = vec![
            component(abc, 1_000_000),
            component(xyz, 1_000_000),
            component(dfg, 1_000_000),
        ];
        let new_components = vec![
            component(abc, 750_000),
            component(xyz, 750_000),
            component(dfg, 750_000),
            component(lmn, 250_000),
        ];

        let old_nav = nav_nad(&old_components, &prices).unwrap();
        let new_nav = nav_nad(&new_components, &prices).unwrap();

        assert_eq!(old_nav, 16 * u128::from(REBALANCE_PRICE_SCALE));
        assert_eq!(new_nav, old_nav);
        assert!(within_bps_tolerance_u128(old_nav, new_nav, 0).unwrap());
    }

    #[test]
    fn adding_full_component_without_reducing_weights_is_rejected_by_nav_check() {
        let abc = Pubkey::new_unique();
        let xyz = Pubkey::new_unique();
        let dfg = Pubkey::new_unique();
        let lmn = Pubkey::new_unique();
        let prices = vec![price(abc, 8), price(xyz, 5), price(dfg, 3), price(lmn, 16)];
        let old_components = vec![
            component(abc, 1_000_000),
            component(xyz, 1_000_000),
            component(dfg, 1_000_000),
        ];
        let inflated_components = vec![
            component(abc, 1_000_000),
            component(xyz, 1_000_000),
            component(dfg, 1_000_000),
            component(lmn, 1_000_000),
        ];

        let old_nav = nav_nad(&old_components, &prices).unwrap();
        let inflated_nav = nav_nad(&inflated_components, &prices).unwrap();

        assert_eq!(inflated_nav, old_nav * 2);
        assert!(!within_bps_tolerance_u128(old_nav, inflated_nav, 10).unwrap());
    }

    #[test]
    fn removing_component_requires_remaining_weights_to_preserve_nav() {
        let abc = Pubkey::new_unique();
        let xyz = Pubkey::new_unique();
        let dfg = Pubkey::new_unique();
        let prices = vec![price(abc, 8), price(xyz, 5), price(dfg, 3)];
        let old_components = vec![
            component(abc, 1_000_000),
            component(xyz, 1_000_000),
            component(dfg, 1_000_000),
        ];
        let new_components = vec![component(abc, 1_000_000), component(xyz, 1_600_000)];

        let old_nav = nav_nad(&old_components, &prices).unwrap();
        let new_nav = nav_nad(&new_components, &prices).unwrap();

        assert_eq!(old_nav, 16 * u128::from(REBALANCE_PRICE_SCALE));
        assert_eq!(new_nav, old_nav);
        assert!(within_bps_tolerance_u128(old_nav, new_nav, 0).unwrap());
    }

    #[test]
    fn removing_component_without_reallocating_value_is_rejected_by_nav_check() {
        let abc = Pubkey::new_unique();
        let xyz = Pubkey::new_unique();
        let dfg = Pubkey::new_unique();
        let prices = vec![price(abc, 8), price(xyz, 5), price(dfg, 3)];
        let old_components = vec![
            component(abc, 1_000_000),
            component(xyz, 1_000_000),
            component(dfg, 1_000_000),
        ];
        let underweight_components = vec![component(abc, 1_000_000), component(xyz, 1_000_000)];

        let old_nav = nav_nad(&old_components, &prices).unwrap();
        let underweight_nav = nav_nad(&underweight_components, &prices).unwrap();

        assert_eq!(underweight_nav, 13 * u128::from(REBALANCE_PRICE_SCALE));
        assert!(!within_bps_tolerance_u128(old_nav, underweight_nav, 10).unwrap());
    }

    #[test]
    fn execution_value_rejects_output_below_slippage_floor() {
        let input = RebalancePrice {
            mint: Pubkey::new_unique(),
            price_nad: REBALANCE_PRICE_SCALE,
            decimals: 6,
        };
        let output = RebalancePrice {
            mint: Pubkey::new_unique(),
            price_nad: REBALANCE_PRICE_SCALE,
            decimals: 6,
        };

        assert!(validate_rebalance_execution_value(100, &input, 95, &output, 500).is_ok());
        assert!(validate_rebalance_execution_value(100, &input, 94, &output, 500).is_err());
    }
}
