use anchor_lang::prelude::*;

use crate::{
    constants::{BPS_DENOMINATOR, USDC_MINT},
    errors::BasketError,
    state::IndexComponent,
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
    require_keys_eq!(*quote_mint, USDC_MINT, BasketError::InvalidQuoteMint);
    Ok(())
}

pub fn rebalance_price_for_mint<'a>(
    prices: &'a [RebalancePrice],
    mint: &Pubkey,
) -> Result<&'a RebalancePrice> {
    prices
        .iter()
        .find(|price| price.mint == *mint)
        .ok_or_else(|| error!(BasketError::InvalidRebalancePriceInput))
}

pub fn nav_nad(components: &[IndexComponent], prices: &[RebalancePrice]) -> Result<u128> {
    let mut nav = 0u128;

    for component in components {
        let price = rebalance_price_for_mint(prices, &component.mint)?;
        let component_value =
            token_value_nad(component.units_per_index, price.decimals, price.price_nad)?;
        nav = nav
            .checked_add(component_value)
            .ok_or_else(|| error!(BasketError::ArithmeticOverflow))?;
    }

    Ok(nav)
}

pub fn token_value_nad(amount: u64, decimals: u8, price_nad: u64) -> Result<u128> {
    let denominator = pow10_u128(decimals)?;
    u128::from(amount)
        .checked_mul(u128::from(price_nad))
        .and_then(|value| value.checked_div(denominator))
        .ok_or_else(|| error!(BasketError::ArithmeticOverflow))
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
        BasketError::InvalidOraclePriceTolerance
    );
    let input_value = token_value_nad(input_amount, input_price.decimals, input_price.price_nad)?;
    let output_value =
        token_value_nad(output_amount, output_price.decimals, output_price.price_nad)?;
    let min_output_value = input_value
        .checked_mul(u128::from(BPS_DENOMINATOR - max_slippage_bps))
        .and_then(|value| value.checked_div(u128::from(BPS_DENOMINATOR)))
        .ok_or_else(|| error!(BasketError::ArithmeticOverflow))?;
    require!(
        output_value >= min_output_value,
        BasketError::ExecutionPriceOutsideOracleTolerance
    );
    Ok(())
}

pub fn within_bps_tolerance_u128(reference: u128, value: u128, tolerance_bps: u16) -> Result<bool> {
    let diff = reference.abs_diff(value);
    let max_diff = reference
        .checked_mul(u128::from(tolerance_bps))
        .ok_or_else(|| error!(BasketError::ArithmeticOverflow))?
        .checked_div(u128::from(BPS_DENOMINATOR))
        .ok_or_else(|| error!(BasketError::ArithmeticOverflow))?;

    Ok(diff <= max_diff)
}

/// True iff `value` has not LOST more than `tolerance_bps` of `reference`. One-sided:
/// any gain passes. The rebalance NAV-preservation gate bounds value leaked to bad
/// execution; an upside move (market drift, recycled quote dust landing in components)
/// never harms holders and must not block finalize.
pub fn nav_loss_within_tolerance_u128(
    reference: u128,
    value: u128,
    tolerance_bps: u16,
) -> Result<bool> {
    if value >= reference {
        return Ok(true);
    }
    let max_loss = reference
        .checked_mul(u128::from(tolerance_bps))
        .ok_or_else(|| error!(BasketError::ArithmeticOverflow))?
        .checked_div(u128::from(BPS_DENOMINATOR))
        .ok_or_else(|| error!(BasketError::ArithmeticOverflow))?;
    Ok(reference - value <= max_loss)
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
    fn nav_remark_is_neutral_to_uniform_market_moves() {
        // The finalize NAV gate re-marks both the original holdings and the post-rebalance
        // holdings at the SAME fresh price vector. A value-conserving rebalance must pass
        // regardless of how the whole market moved between open and finalize — model that
        // by scaling every price by the same factor and confirming the loss ratio (and
        // thus the gate verdict) is unchanged.
        //
        // Pre: 200 A + 50 B. Post (value-conserving swap, A and B same price): 125 A + 125 B.
        let remark = |price: u128| -> (u128, u128) {
            let expected = 200 * price + 50 * price; // original holdings at this price
            let final_value = 125 * price + 125 * price; // post-rebalance holdings
            (expected, final_value)
        };
        for price in [50u128, 100, 137, 1_000] {
            let (expected, final_value) = remark(price);
            // Conserved value: equal, so it passes even at zero tolerance.
            assert!(nav_loss_within_tolerance_u128(expected, final_value, 0).unwrap());
        }
        // A genuine 2% shortfall fails a 1% tolerance at any uniform price level.
        for price in [50u128, 100, 1_000] {
            let expected = 200 * price + 50 * price;
            let leaked_final = expected * 98 / 100;
            assert!(!nav_loss_within_tolerance_u128(expected, leaked_final, 100).unwrap());
        }
    }

    #[test]
    fn nav_loss_tolerance_is_one_sided() {
        // 1% tolerance on a reference of 10_000: exactly -1% passes, one atom more fails.
        assert!(nav_loss_within_tolerance_u128(10_000, 9_900, 100).unwrap());
        assert!(!nav_loss_within_tolerance_u128(10_000, 9_899, 100).unwrap());
        // Gains always pass, even at zero tolerance; losses never pass at zero tolerance.
        assert!(nav_loss_within_tolerance_u128(10_000, 20_000, 0).unwrap());
        assert!(nav_loss_within_tolerance_u128(10_000, 10_000, 0).unwrap());
        assert!(!nav_loss_within_tolerance_u128(10_000, 9_999, 0).unwrap());
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

/// Transaction-size budget tests for the rebalance swap path.
///
/// These pin the byte math behind the central Solana-tx-limit question for rebalance.
/// Two encodings exist in this repo's history:
///   * "fat" — the OLD, now-deleted `execute_rebalance` / `rebalance_fixed_weights_with_jupiter`
///     packed every swap into ONE instruction as a `Vec<JupiterSwapPlan>`. Each fat swap
///     carries 4 full 32-byte pubkeys (128 B) + a `Vec<JupiterAccountMetaInput>` where every
///     route account is a full 34-byte (pubkey + 2 bools) meta — all incompressible
///     instruction DATA (mirrored below from target/idl/omnindex.json).
///   * "compact" — the large-basket mint/redeem rewrite's `LargeBasketSwapPlan`: 1 byte per
///     route account (bit-packed index), with the route pubkeys riding in remaining_accounts
///     so they compress against Address Lookup Tables. Executed in BATCHES across many txs.
///
/// The old fat handlers were deleted and the rebalance was re-built on the compact/batched
/// model (instructions/rebalance_intent.rs — open/execute-batch/verify/finalize). These
/// tests measure both encodings to pin *why* the old path could not fit and what the
/// compact batch budget is.
/// Compiled-message sizing (the account list + ALT compression, which is the real binding
/// constraint) is covered by basket-ui/scripts/rebalance-tx-size.mjs.
#[cfg(test)]
mod tx_budget {
    use super::RebalancePriceInput;
    use crate::constants::{MAX_REBALANCE_SWAPS, MAX_REBALANCE_SWAPS_PER_BATCH};
    use crate::instructions::{ExecuteLargeBasketMintBatchArgs, LargeBasketMintBatchEntry};
    use crate::utils::LargeBasketSwapPlan;
    use anchor_lang::prelude::*;

    /// Hard cap on a Solana transaction (packet MTU).
    const TX_LIMIT: usize = 1232;
    /// Anchor sighash prefixed to every instruction's data.
    const ANCHOR_DISCRIMINATOR: usize = 8;
    /// Representative single/double-hop Jupiter route. DESIGN-batched-swaps.md measured
    /// ~20 route accounts compressing to ~210 B and a single compact swap compiling to
    /// 461 B; 16 accounts / 80 B of route ix data is a conservative mid-point.
    const ROUTE_ACCOUNTS: usize = 16;
    const ROUTE_IX_DATA: usize = 80;

    // --- Old "fat" plan, mirrored from the stale IDL so we can size it without
    //     resurrecting the deleted instruction (JupiterSwapPlan + 34-byte
    //     JupiterAccountMetaInput in target/idl/omnindex.json). ---
    #[derive(AnchorSerialize)]
    struct FatAccountMeta {
        pubkey: Pubkey,
        is_signer: bool,
        is_writable: bool,
    }

    #[derive(AnchorSerialize)]
    struct FatSwapPlan {
        input_mint: Pubkey,
        output_mint: Pubkey,
        source_token_account: Pubkey,
        destination_token_account: Pubkey,
        max_oracle_slippage_bps: u16,
        instruction_data: Vec<u8>,
        accounts: Vec<FatAccountMeta>,
    }

    /// Mirror of the deleted `ExecuteRebalanceArgs` (all swaps + prices in one ix).
    #[derive(AnchorSerialize)]
    struct ExecuteRebalanceArgsModel {
        swaps: Vec<FatSwapPlan>,
        prices: Vec<RebalancePriceInput>,
        switchboard_max_age_slots: u64,
    }

    fn fat_swap() -> FatSwapPlan {
        FatSwapPlan {
            input_mint: Pubkey::new_unique(),
            output_mint: Pubkey::new_unique(),
            source_token_account: Pubkey::new_unique(),
            destination_token_account: Pubkey::new_unique(),
            max_oracle_slippage_bps: 50,
            instruction_data: vec![7u8; ROUTE_IX_DATA],
            accounts: (0..ROUTE_ACCOUNTS)
                .map(|_| FatAccountMeta {
                    pubkey: Pubkey::new_unique(),
                    is_signer: false,
                    is_writable: true,
                })
                .collect(),
        }
    }

    fn compact_swap() -> LargeBasketSwapPlan {
        LargeBasketSwapPlan {
            instruction_data: vec![7u8; ROUTE_IX_DATA],
            accounts: vec![0u8; ROUTE_ACCOUNTS],
        }
    }

    fn borsh_len<T: AnchorSerialize>(value: &T) -> usize {
        value.try_to_vec().unwrap().len()
    }

    #[test]
    fn compact_swap_is_far_smaller_than_fat_swap() {
        let fat = borsh_len(&fat_swap());
        let compact = borsh_len(&compact_swap());
        // Fat carries 128 B of header pubkeys + 34 B/route-meta as DATA; compact carries
        // 1 B/route-account and moves the pubkeys to ALT-compressible accounts.
        assert!(
            fat >= 700 && compact <= 130,
            "unexpected encoding sizes (fat={fat}, compact={compact})",
        );
        assert!(
            fat - compact >= 600,
            "compact encoding should save >=600 B/swap of instruction data vs fat \
             (fat={fat}, compact={compact})",
        );
    }

    #[test]
    fn fat_atomic_rebalance_cannot_fit_two_swaps_in_one_tx() {
        // The deleted execute_rebalance put ALL swaps + prices in a single instruction.
        let args = ExecuteRebalanceArgsModel {
            swaps: vec![fat_swap(), fat_swap()],
            prices: vec![
                RebalancePriceInput { mint: Pubkey::new_unique(), price_nad: Some(1) },
                RebalancePriceInput { mint: Pubkey::new_unique(), price_nad: Some(1) },
            ],
            switchboard_max_age_slots: 150,
        };
        let data = ANCHOR_DISCRIMINATOR + borsh_len(&args);
        assert!(
            data > TX_LIMIT,
            "two fat swaps' instruction DATA alone ({data} B) already exceeds the {TX_LIMIT} B \
             tx limit, before any accounts or message header. DESIGN-batched-swaps.md measured \
             ONE fat swap compiling to 1062 B — i.e. the old atomic rebalance fits ~1 swap/tx.",
        );
    }

    #[test]
    fn fat_atomic_rebalance_at_its_own_cap_is_an_order_of_magnitude_over_limit() {
        // MAX_REBALANCE_SWAPS (=32) is the old design's nominal per-rebalance swap cap, all
        // of which it tried to execute in ONE atomic instruction.
        let one = borsh_len(&fat_swap());
        let cap_data = MAX_REBALANCE_SWAPS * one;
        assert!(
            cap_data > TX_LIMIT * 10,
            "{MAX_REBALANCE_SWAPS} fat swaps = {cap_data} B of instruction data, >10x the \
             {TX_LIMIT} B tx limit: the old atomic rebalance could never process its own \
             MAX_REBALANCE_SWAPS cap.",
        );
    }

    #[test]
    fn compact_batch_instruction_data_fits_with_headroom() {
        // The planned rebalance batch mirrors ExecuteLargeBasketMintBatchArgs (the working
        // large-basket model). Fill a full MAX_REBALANCE_SWAPS_PER_BATCH batch.
        let entries: Vec<LargeBasketMintBatchEntry> = (0..MAX_REBALANCE_SWAPS_PER_BATCH)
            .map(|i| LargeBasketMintBatchEntry {
                component_index: i as u16,
                max_quote_in: u64::MAX,
                route_account_count: ROUTE_ACCOUNTS as u8,
                swap: Some(compact_swap()),
            })
            .collect();
        let args = ExecuteLargeBasketMintBatchArgs { entries };
        let data = ANCHOR_DISCRIMINATOR + borsh_len(&args);
        // Instruction DATA for a full compact batch is a small fraction of the limit. The
        // real binding constraint is the compiled ACCOUNT list (route accounts + per-entry
        // component accounts) and the per-tx account-lock cap — NOT the data. Measurements
        // (DESIGN-batched-swaps.md) put the practical batch at ~3 swaps/tx even though
        // MAX_REBALANCE_SWAPS_PER_BATCH is 4. See scripts/rebalance-tx-size.mjs.
        assert!(
            data < TX_LIMIT,
            "compact batch data ({data} B) must fit the {TX_LIMIT} B tx limit",
        );
        assert!(
            data < TX_LIMIT / 2,
            "compact batch data ({data} B) should leave most of the tx for the \
             ALT-compressed account list",
        );
    }
}
