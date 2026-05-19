use std::slice::Iter;

use anchor_lang::prelude::*;

use crate::{
    constants::BPS_DENOMINATOR,
    errors::OmnindexError,
    state::IndexComponent,
    utils::{load_pair, load_rate_model, oracle_price_for_component, FutarchyAuthority, NAD},
};

#[derive(AnchorSerialize, AnchorDeserialize, Clone, Debug)]
pub struct RebalancePriceInput {
    pub mint: Pubkey,
    pub price_nad: Option<u64>,
}

pub fn resolve_rebalance_prices<'info>(
    remaining: &mut Iter<'info, AccountInfo<'info>>,
    rebalance_mints: &[Pubkey],
    quote_mint: &Pubkey,
    price_inputs: &[RebalancePriceInput],
    oracle_price_tolerance_bps: u16,
    futarchy_authority: &FutarchyAuthority,
) -> Result<Vec<u64>> {
    require!(
        price_inputs.len() == rebalance_mints.len(),
        OmnindexError::InvalidRebalancePriceInput
    );

    let mut prices = Vec::with_capacity(rebalance_mints.len());

    for (expected_mint, price_input) in rebalance_mints.iter().zip(price_inputs.iter()) {
        require_keys_eq!(
            price_input.mint,
            *expected_mint,
            OmnindexError::InvalidRebalancePriceInput
        );

        let oracle_price_nad = if expected_mint == quote_mint {
            NAD
        } else {
            let pair_info = next_account_info(remaining)?;
            let rate_model_info = next_account_info(remaining)?;
            let pair = load_pair(pair_info)?;
            let rate_model = load_rate_model(rate_model_info)?;
            require_keys_eq!(
                rate_model_info.key(),
                pair.rate_model,
                OmnindexError::InvalidOmnipairRateModel
            );
            oracle_price_for_component(
                &pair,
                &rate_model,
                futarchy_authority,
                expected_mint,
                quote_mint,
            )?
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

        prices.push(price_nad);
    }

    Ok(prices)
}

pub fn nav_nad(components: &[IndexComponent], mints: &[Pubkey], prices: &[u64]) -> Result<u128> {
    let mut nav = 0u128;

    for component in components {
        let price_index = mints
            .iter()
            .position(|mint| *mint == component.mint)
            .ok_or_else(|| error!(OmnindexError::InvalidRebalancePriceInput))?;
        let component_value = u128::from(component.units_per_index)
            .checked_mul(u128::from(prices[price_index]))
            .ok_or_else(|| error!(OmnindexError::ArithmeticOverflow))?;
        nav = nav
            .checked_add(component_value)
            .ok_or_else(|| error!(OmnindexError::ArithmeticOverflow))?;
    }

    Ok(nav)
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

#[cfg(test)]
mod tests {
    use super::*;

    const UNITS: u64 = 1_000_000;

    fn component(mint: Pubkey, units_per_index: u64) -> IndexComponent {
        IndexComponent {
            mint,
            units_per_index,
            target_weight_bps: 0,
            oracle_pair: Pubkey::default(),
        }
    }

    fn price(value: u64) -> u64 {
        value * NAD
    }

    #[test]
    fn adding_component_requires_reduced_existing_weights_to_preserve_nav() {
        let abc = Pubkey::new_unique();
        let xyz = Pubkey::new_unique();
        let dfg = Pubkey::new_unique();
        let lmn = Pubkey::new_unique();
        let mints = vec![abc, xyz, dfg, lmn];
        let prices = vec![price(8), price(5), price(3), price(16)];
        let old_components = vec![
            component(abc, UNITS),
            component(xyz, UNITS),
            component(dfg, UNITS),
        ];
        let new_components = vec![
            component(abc, 750_000),
            component(xyz, 750_000),
            component(dfg, 750_000),
            component(lmn, 250_000),
        ];

        let old_nav = nav_nad(&old_components, &mints, &prices).unwrap();
        let new_nav = nav_nad(&new_components, &mints, &prices).unwrap();

        assert_eq!(old_nav, 16 * u128::from(UNITS) * u128::from(NAD));
        assert_eq!(new_nav, old_nav);
        assert!(within_bps_tolerance_u128(old_nav, new_nav, 0).unwrap());
    }

    #[test]
    fn adding_full_component_without_reducing_weights_is_rejected_by_nav_check() {
        let abc = Pubkey::new_unique();
        let xyz = Pubkey::new_unique();
        let dfg = Pubkey::new_unique();
        let lmn = Pubkey::new_unique();
        let mints = vec![abc, xyz, dfg, lmn];
        let prices = vec![price(8), price(5), price(3), price(16)];
        let old_components = vec![
            component(abc, UNITS),
            component(xyz, UNITS),
            component(dfg, UNITS),
        ];
        let inflated_components = vec![
            component(abc, UNITS),
            component(xyz, UNITS),
            component(dfg, UNITS),
            component(lmn, UNITS),
        ];

        let old_nav = nav_nad(&old_components, &mints, &prices).unwrap();
        let inflated_nav = nav_nad(&inflated_components, &mints, &prices).unwrap();

        assert_eq!(inflated_nav, old_nav * 2);
        assert!(!within_bps_tolerance_u128(old_nav, inflated_nav, 10).unwrap());
    }

    #[test]
    fn removing_component_requires_remaining_weights_to_preserve_nav() {
        let abc = Pubkey::new_unique();
        let xyz = Pubkey::new_unique();
        let dfg = Pubkey::new_unique();
        let mints = vec![abc, xyz, dfg];
        let prices = vec![price(8), price(5), price(3)];
        let old_components = vec![
            component(abc, UNITS),
            component(xyz, UNITS),
            component(dfg, UNITS),
        ];
        let new_components = vec![component(abc, UNITS), component(xyz, 1_600_000)];

        let old_nav = nav_nad(&old_components, &mints, &prices).unwrap();
        let new_nav = nav_nad(&new_components, &mints, &prices).unwrap();

        assert_eq!(old_nav, 16 * u128::from(UNITS) * u128::from(NAD));
        assert_eq!(new_nav, old_nav);
        assert!(within_bps_tolerance_u128(old_nav, new_nav, 0).unwrap());
    }

    #[test]
    fn removing_component_without_reallocating_value_is_rejected_by_nav_check() {
        let abc = Pubkey::new_unique();
        let xyz = Pubkey::new_unique();
        let dfg = Pubkey::new_unique();
        let mints = vec![abc, xyz, dfg];
        let prices = vec![price(8), price(5), price(3)];
        let old_components = vec![
            component(abc, UNITS),
            component(xyz, UNITS),
            component(dfg, UNITS),
        ];
        let underweight_components = vec![component(abc, UNITS), component(xyz, UNITS)];

        let old_nav = nav_nad(&old_components, &mints, &prices).unwrap();
        let underweight_nav = nav_nad(&underweight_components, &mints, &prices).unwrap();

        assert_eq!(underweight_nav, 13 * u128::from(UNITS) * u128::from(NAD));
        assert!(!within_bps_tolerance_u128(old_nav, underweight_nav, 10).unwrap());
    }
}
