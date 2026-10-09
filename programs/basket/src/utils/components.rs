use std::collections::BTreeSet;

use anchor_lang::prelude::*;

use crate::{
    constants::{
        BPS_DENOMINATOR, MAX_COMPONENTS, MAX_REBALANCE_DELAY_SECONDS,
        MIN_FIXED_WEIGHT_POST_REBALANCE_DRIFT_BPS,
    },
    errors::BasketError,
    state::{IndexComponent, IndexComponentInput, IndexKind},
    utils::{pro_rata_mint_amount, pro_rata_redeem_amount, quote_component_amount},
};

pub fn validate_component_inputs(inputs: Vec<IndexComponentInput>) -> Result<Vec<IndexComponent>> {
    require!(!inputs.is_empty(), BasketError::InvalidComponentCount);
    require!(
        inputs.len() <= MAX_COMPONENTS,
        BasketError::TooManyComponents
    );

    let mut seen_mints = BTreeSet::new();
    let mut components = Vec::with_capacity(inputs.len());

    for component in inputs {
        require!(
            component.units_per_index > 0,
            BasketError::ZeroComponentUnits
        );
        require!(
            seen_mints.insert(component.mint),
            BasketError::DuplicateComponentMint
        );

        components.push(IndexComponent {
            mint: component.mint,
            units_per_index: component.units_per_index,
            target_weight_bps: component.target_weight_bps,
            oracle_pair: component.oracle_pair,
        });
    }

    Ok(components)
}

pub fn validate_index_strategy_config(
    kind: IndexKind,
    components: &[IndexComponent],
    fixed_weight_quote_mint: Pubkey,
    fixed_weight_rebalance_interval_seconds: i64,
    fixed_weight_drift_threshold_bps: u16,
    fixed_weight_spot_ema_max_deviation_bps: u16,
) -> Result<()> {
    match kind {
        IndexKind::FixedUnits => validate_fixed_unit_config(
            components,
            fixed_weight_quote_mint,
            fixed_weight_rebalance_interval_seconds,
            fixed_weight_drift_threshold_bps,
            fixed_weight_spot_ema_max_deviation_bps,
        ),
        IndexKind::FixedWeights => validate_fixed_weight_config(
            components,
            fixed_weight_quote_mint,
            fixed_weight_rebalance_interval_seconds,
            fixed_weight_drift_threshold_bps,
            fixed_weight_spot_ema_max_deviation_bps,
        ),
    }
}

fn validate_fixed_unit_config(
    components: &[IndexComponent],
    fixed_weight_quote_mint: Pubkey,
    fixed_weight_rebalance_interval_seconds: i64,
    fixed_weight_drift_threshold_bps: u16,
    fixed_weight_spot_ema_max_deviation_bps: u16,
) -> Result<()> {
    require_keys_eq!(
        fixed_weight_quote_mint,
        Pubkey::default(),
        BasketError::InvalidFixedWeightConfig
    );
    require!(
        fixed_weight_rebalance_interval_seconds == 0,
        BasketError::InvalidFixedWeightConfig
    );
    require!(
        fixed_weight_drift_threshold_bps == 0,
        BasketError::InvalidFixedWeightConfig
    );
    require!(
        fixed_weight_spot_ema_max_deviation_bps == 0,
        BasketError::InvalidFixedWeightConfig
    );

    for component in components {
        require!(
            component.target_weight_bps == 0,
            BasketError::InvalidFixedWeightConfig
        );
    }

    Ok(())
}

fn validate_fixed_weight_config(
    components: &[IndexComponent],
    fixed_weight_quote_mint: Pubkey,
    fixed_weight_rebalance_interval_seconds: i64,
    fixed_weight_drift_threshold_bps: u16,
    fixed_weight_spot_ema_max_deviation_bps: u16,
) -> Result<()> {
    require_keys_neq!(
        fixed_weight_quote_mint,
        Pubkey::default(),
        BasketError::InvalidFixedWeightConfig
    );
    require!(
        (0..=MAX_REBALANCE_DELAY_SECONDS).contains(&fixed_weight_rebalance_interval_seconds),
        BasketError::InvalidFixedWeightConfig
    );
    // Drift triggering is either disabled (0) or set strictly above the keeper drift-bound
    // floor. The rebalance open clamps a drift-triggered intent's post-rebalance drift
    // bound to [MIN_FIXED_WEIGHT_POST_REBALANCE_DRIFT_BPS, threshold); a threshold at or
    // below the floor would make that range empty and the drift rebalance un-openable, so
    // forbid it at config time rather than silently bricking the drift flow.
    require!(
        fixed_weight_drift_threshold_bps == 0
            || (fixed_weight_drift_threshold_bps > MIN_FIXED_WEIGHT_POST_REBALANCE_DRIFT_BPS
                && fixed_weight_drift_threshold_bps <= BPS_DENOMINATOR),
        BasketError::InvalidFixedWeightConfig
    );
    // Reserved knob: the legacy atomic rebalance enforced a spot-vs-EMA oracle deviation
    // bound; the batched intent flow does not read it yet, so 0 (= disabled) is allowed.
    require!(
        fixed_weight_spot_ema_max_deviation_bps <= BPS_DENOMINATOR,
        BasketError::InvalidFixedWeightConfig
    );
    require!(
        fixed_weight_rebalance_interval_seconds > 0 || fixed_weight_drift_threshold_bps > 0,
        BasketError::InvalidFixedWeightConfig
    );

    let mut total_weight = 0u32;

    for component in components {
        require!(
            component.units_per_index > 0 || (component.mint == fixed_weight_quote_mint && component.target_weight_bps == 0),
            BasketError::ZeroComponentUnits
        );
        require!(
            component.target_weight_bps > 0 || component.mint == fixed_weight_quote_mint,
            BasketError::InvalidFixedWeightConfig
        );
        total_weight = total_weight
            .checked_add(u32::from(component.target_weight_bps))
            .ok_or_else(|| error!(BasketError::ArithmeticOverflow))?;
    }

    require!(
        total_weight == u32::from(BPS_DENOMINATOR),
        BasketError::InvalidFixedWeightConfig
    );

    Ok(())
}

pub fn validate_no_self_component(
    components: &[IndexComponent],
    index_mint: &Pubkey,
) -> Result<()> {
    for component in components {
        require_keys_neq!(
            component.mint,
            *index_mint,
            BasketError::InvalidComponentMint
        );
    }

    Ok(())
}

pub fn rebalance_mints(
    old_components: &[IndexComponent],
    new_components: &[IndexComponent],
) -> Vec<Pubkey> {
    let mut seen = BTreeSet::new();
    let mut mints = Vec::with_capacity(old_components.len() + new_components.len());

    for component in old_components.iter().chain(new_components.iter()) {
        if seen.insert(component.mint) {
            mints.push(component.mint);
        }
    }

    mints
}

pub fn target_component_amount(
    components: &[IndexComponent],
    mint: &Pubkey,
    supply: u64,
    base_units: u64,
) -> Result<u64> {
    let Some(component) = components.iter().find(|component| component.mint == *mint) else {
        return Ok(0);
    };

    quote_component_amount(component.units_per_index, supply, base_units)
}

pub fn mint_component_backing_amount(
    component: &IndexComponent,
    index_amount: u64,
    base_units: u64,
    current_supply: u64,
    vault_amount: u64,
) -> Result<u64> {
    if current_supply == 0 {
        quote_component_amount(component.units_per_index, index_amount, base_units)
    } else {
        pro_rata_mint_amount(index_amount, vault_amount, current_supply)
    }
}

pub fn redeem_component_backing_amount(
    index_amount: u64,
    current_supply: u64,
    vault_amount: u64,
) -> Result<u64> {
    require!(current_supply > 0, BasketError::InvalidIndexAmount);
    pro_rata_redeem_amount(index_amount, vault_amount, current_supply)
}

pub fn validate_component_targets_integral(
    components: &[IndexComponent],
    supply: u64,
    base_units: u64,
) -> Result<()> {
    for component in components {
        quote_component_amount(component.units_per_index, supply, base_units)?;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn component(mint: Pubkey, units_per_index: u64) -> IndexComponent {
        IndexComponent {
            mint,
            units_per_index,
            target_weight_bps: 0,
            oracle_pair: Pubkey::default(),
        }
    }

    fn weighted_component(
        mint: Pubkey,
        target_weight_bps: u16,
        oracle_pair: Pubkey,
    ) -> IndexComponent {
        IndexComponent {
            mint,
            units_per_index: 1,
            target_weight_bps,
            oracle_pair,
        }
    }

    #[test]
    fn rebalance_mints_preserves_old_order_and_appends_added_mints() {
        let abc = Pubkey::new_unique();
        let xyz = Pubkey::new_unique();
        let dfg = Pubkey::new_unique();
        let lmn = Pubkey::new_unique();
        let old_components = vec![component(abc, 1), component(xyz, 1), component(dfg, 1)];
        let new_components = vec![
            component(abc, 1),
            component(xyz, 1),
            component(dfg, 1),
            component(lmn, 1),
        ];

        assert_eq!(
            rebalance_mints(&old_components, &new_components),
            vec![abc, xyz, dfg, lmn]
        );
    }

    #[test]
    fn rebalance_mints_keeps_removed_mints_for_zero_target_validation() {
        let abc = Pubkey::new_unique();
        let xyz = Pubkey::new_unique();
        let dfg = Pubkey::new_unique();
        let old_components = vec![component(abc, 1), component(xyz, 1), component(dfg, 1)];
        let new_components = vec![component(abc, 1), component(xyz, 1)];

        assert_eq!(
            rebalance_mints(&old_components, &new_components),
            vec![abc, xyz, dfg]
        );
    }

    #[test]
    fn removed_components_have_zero_target_amount() {
        let abc = Pubkey::new_unique();
        let dfg = Pubkey::new_unique();
        let supply = 5_000_000;
        let base_units = 1_000_000;
        let new_components = vec![component(abc, 2_000_000)];

        assert_eq!(
            target_component_amount(&new_components, &dfg, supply, base_units).unwrap(),
            0
        );
    }

    #[test]
    fn added_components_get_supply_scaled_target_amount() {
        let lmn = Pubkey::new_unique();
        let supply = 5_000_000;
        let base_units = 1_000_000;
        let new_components = vec![component(lmn, 250_000)];

        assert_eq!(
            target_component_amount(&new_components, &lmn, supply, base_units).unwrap(),
            1_250_000
        );
    }

    #[test]
    fn component_target_integrality_rejects_fractional_supply() {
        let lmn = Pubkey::new_unique();
        let supply = 1;
        let base_units = 1_000_000;
        let new_components = vec![component(lmn, 750_000)];

        assert!(validate_component_targets_integral(&new_components, supply, base_units).is_err());
    }

    #[test]
    fn component_target_integrality_accepts_compatible_supply() {
        let lmn = Pubkey::new_unique();
        let supply = 4;
        let base_units = 1_000_000;
        let new_components = vec![component(lmn, 750_000)];

        assert!(validate_component_targets_integral(&new_components, supply, base_units).is_ok());
    }

    #[test]
    fn validate_component_inputs_rejects_duplicate_added_mints() {
        let lmn = Pubkey::new_unique();
        let result = validate_component_inputs(vec![
            IndexComponentInput {
                mint: lmn,
                units_per_index: 1,
                target_weight_bps: 0,
                oracle_pair: Pubkey::default(),
            },
            IndexComponentInput {
                mint: lmn,
                units_per_index: 2,
                target_weight_bps: 0,
                oracle_pair: Pubkey::default(),
            },
        ]);

        assert!(result.is_err());
    }

    #[test]
    fn validate_component_inputs_rejects_zero_initial_units() {
        let result = validate_component_inputs(vec![IndexComponentInput {
            mint: Pubkey::new_unique(),
            units_per_index: 0,
            target_weight_bps: 5_000,
            oracle_pair: Pubkey::new_unique(),
        }]);

        assert!(result.is_err());
    }

    #[test]
    fn first_mint_uses_configured_initial_units() {
        let component = IndexComponent {
            mint: Pubkey::new_unique(),
            units_per_index: 2_500_000,
            target_weight_bps: 5_000,
            oracle_pair: Pubkey::new_unique(),
        };

        assert_eq!(
            mint_component_backing_amount(&component, 5_000_000, 1_000_000, 0, 0).unwrap(),
            12_500_000
        );
    }

    #[test]
    fn fixed_weight_config_allows_disabled_spot_ema_deviation() {
        let quote = Pubkey::new_unique();
        let component_mint = Pubkey::new_unique();
        let components = vec![
            weighted_component(quote, 5_000, Pubkey::default()),
            weighted_component(component_mint, 5_000, Pubkey::new_unique()),
        ];

        // 0 = disabled (the batched intent flow does not read the knob); anything
        // above 100% is rejected.
        assert!(validate_index_strategy_config(
            IndexKind::FixedWeights,
            &components,
            quote,
            60,
            0,
            0,
        )
        .is_ok());
        assert!(validate_index_strategy_config(
            IndexKind::FixedWeights,
            &components,
            quote,
            60,
            0,
            500,
        )
        .is_ok());
        assert!(validate_index_strategy_config(
            IndexKind::FixedWeights,
            &components,
            quote,
            60,
            0,
            10_001,
        )
        .is_err());
    }

    #[test]
    fn fixed_weight_config_rejects_drift_threshold_at_or_below_keeper_floor() {
        let quote = Pubkey::new_unique();
        let components = vec![
            weighted_component(Pubkey::new_unique(), 5_000, Pubkey::new_unique()),
            weighted_component(Pubkey::new_unique(), 5_000, Pubkey::new_unique()),
        ];
        let cfg = |threshold| {
            validate_index_strategy_config(
                IndexKind::FixedWeights,
                &components,
                quote,
                0,
                threshold,
                500,
            )
        };
        // A drift threshold at or below the post-rebalance drift floor would leave the
        // open-time [MIN, threshold) clamp empty, so the drift rebalance could never open.
        assert!(cfg(MIN_FIXED_WEIGHT_POST_REBALANCE_DRIFT_BPS).is_err());
        assert!(cfg(MIN_FIXED_WEIGHT_POST_REBALANCE_DRIFT_BPS - 1).is_err());
        // Strictly above the floor is fine.
        assert!(cfg(MIN_FIXED_WEIGHT_POST_REBALANCE_DRIFT_BPS + 1).is_ok());
        assert!(cfg(BPS_DENOMINATOR).is_ok());
        assert!(cfg(BPS_DENOMINATOR + 1).is_err());
    }

    #[test]
    fn fixed_weight_config_accepts_external_quote_with_component_oracle_pairs() {
        let quote = Pubkey::new_unique();
        let components = vec![
            weighted_component(Pubkey::new_unique(), 5_000, Pubkey::new_unique()),
            weighted_component(Pubkey::new_unique(), 5_000, Pubkey::new_unique()),
        ];

        assert!(validate_index_strategy_config(
            IndexKind::FixedWeights,
            &components,
            quote,
            60,
            0,
            500,
        )
        .is_ok());
    }

    #[test]
    fn fixed_weight_config_needs_no_oracle_pairs() {
        // Rebalances read prices the oracle signs for each component.
        let quote = Pubkey::new_unique();
        let components = vec![
            weighted_component(Pubkey::new_unique(), 5_000, Pubkey::default()),
            weighted_component(Pubkey::new_unique(), 5_000, Pubkey::default()),
        ];

        assert!(validate_index_strategy_config(
            IndexKind::FixedWeights,
            &components,
            quote,
            60,
            0,
            500,
        )
        .is_ok());
    }

    #[test]
    fn fixed_unit_config_accepts_oracle_feed_ids() {
        let components = vec![IndexComponent {
            mint: Pubkey::new_unique(),
            units_per_index: 1,
            target_weight_bps: 0,
            oracle_pair: Pubkey::new_unique(),
        }];

        assert!(validate_index_strategy_config(
            IndexKind::FixedUnits,
            &components,
            Pubkey::default(),
            0,
            0,
            0,
        )
        .is_ok());
    }

    #[test]
    fn fixed_unit_config_rejects_target_weights() {
        let components = vec![IndexComponent {
            mint: Pubkey::new_unique(),
            units_per_index: 1,
            target_weight_bps: 1,
            oracle_pair: Pubkey::new_unique(),
        }];

        assert!(validate_index_strategy_config(
            IndexKind::FixedUnits,
            &components,
            Pubkey::default(),
            0,
            0,
            0,
        )
        .is_err());
    }

    #[test]
    fn fixed_weights_allow_empty_zero_weight_cash_reserve_only() {
        let cash = Pubkey::new_unique();
        let mut components = vec![
            IndexComponent { mint: Pubkey::new_unique(), units_per_index: 1,
                target_weight_bps: 10_000, oracle_pair: Pubkey::new_unique() },
            IndexComponent { mint: cash, units_per_index: 0,
                target_weight_bps: 0, oracle_pair: Pubkey::default() },
        ];
        assert!(validate_fixed_weight_config(&components, cash, 60, 500, 0).is_ok());
        components[1].mint = Pubkey::new_unique();
        assert!(validate_fixed_weight_config(&components, cash, 60, 500, 0).is_err());
    }

    #[test]
    fn validate_no_self_component_rejects_index_mint() {
        let index_mint = Pubkey::new_unique();
        let result = validate_no_self_component(&[component(index_mint, 1)], &index_mint);

        assert!(result.is_err());
    }

    #[test]
    fn validate_no_self_component_accepts_distinct_components() {
        let index_mint = Pubkey::new_unique();
        let result = validate_no_self_component(&[component(Pubkey::new_unique(), 1)], &index_mint);

        assert!(result.is_ok());
    }
}
