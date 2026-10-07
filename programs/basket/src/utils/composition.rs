use std::collections::BTreeSet;

use anchor_lang::prelude::*;

use crate::{
    constants::{
        BPS_DENOMINATOR, MAX_COMPOSITION_ADDITIONS, MAX_COMPOSITION_COMPONENTS,
        MAX_COMPOSITION_PRICED_COMPONENTS, MAX_LARGE_BASKET_COMPONENTS_PER_PAGE,
        MIN_COMPOSITION_WEIGHT_BPS, USDC_MINT,
    },
    errors::BasketError,
    state::{ComponentAddition, IndexState, LargeBasketComponent},
};

/// Everything a holder pays to redeem: protocol, creator and staking shares.
pub fn total_redeem_fee_bps(index: &IndexState) -> u16 {
    index
        .redeem_fee_bps
        .saturating_add(index.creator_redeem_fee_bps)
        .saturating_add(index.staking_redeem_fee_bps)
}

/// Checks a composition change against the basket's current components: one weight per
/// existing component, valid new components, weights summing to 100%, and some change.
/// The USDC cash slot keeps its weight; it backs rebalances rather than the strategy.
/// The result must stay rebalanceable: within the slot and priced-component limits, and
/// with no weight so small it rounds to zero units.
pub fn validate_composition_change(
    existing: &[LargeBasketComponent],
    index_mint: &Pubkey,
    target_weights_bps: &[u16],
    additions: &[ComponentAddition],
) -> Result<()> {
    require!(!existing.is_empty(), BasketError::InvalidCompositionChange);
    require!(
        target_weights_bps.len() == existing.len(),
        BasketError::InvalidCompositionChange
    );
    // Rebalances need the USDC cash slot; requiring it here also means no one can register
    // it later (register_rebalance_quote is permissionless) to make the proposal stale.
    require!(
        existing.iter().any(|c| c.mint == USDC_MINT),
        BasketError::InvalidFixedWeightConfig
    );
    require!(
        additions.len() <= MAX_COMPOSITION_ADDITIONS
            && existing.len() + additions.len() <= MAX_COMPOSITION_COMPONENTS,
        BasketError::InvalidComponentCount
    );

    let mut total = 0u32;
    let mut changed = !additions.is_empty();
    // Priced by the switchover rebalance: everything weighted, plus removed components the
    // basket still holds (they are sold in it).
    let mut priced = additions.len();
    for (component, &weight) in existing.iter().zip(target_weights_bps) {
        if component.mint != USDC_MINT && (weight > 0 || component.accounted_reserve > 0) {
            priced += 1;
        }
        if component.mint == USDC_MINT {
            require!(
                weight == component.target_weight_bps,
                BasketError::InvalidCompositionChange
            );
        } else if weight > 0 {
            require!(weight >= MIN_COMPOSITION_WEIGHT_BPS, BasketError::InvalidCompositionChange);
            // A weighted component must be priceable by rebalances.
            require_keys_neq!(
                component.oracle_pair,
                Pubkey::default(),
                BasketError::InvalidCompositionChange
            );
        }
        changed |= weight != component.target_weight_bps;
        total += u32::from(weight);
    }

    let mut mints: BTreeSet<Pubkey> = existing.iter().map(|c| c.mint).collect();
    require!(priced <= MAX_COMPOSITION_PRICED_COMPONENTS, BasketError::InvalidComponentCount);
    for addition in additions {
        require!(
            addition.target_weight_bps >= MIN_COMPOSITION_WEIGHT_BPS,
            BasketError::InvalidCompositionChange
        );
        require_keys_neq!(
            addition.oracle_pair,
            Pubkey::default(),
            BasketError::InvalidCompositionChange
        );
        // USDC joins only as the cash slot (register_rebalance_quote), and a basket
        // cannot hold its own token.
        require_keys_neq!(addition.mint, USDC_MINT, BasketError::InvalidComponentMint);
        require_keys_neq!(addition.mint, *index_mint, BasketError::InvalidComponentMint);
        // A removed component comes back by weight, not as a second slot.
        require!(mints.insert(addition.mint), BasketError::DuplicateComponentMint);
        total += u32::from(addition.target_weight_bps);
    }

    require!(
        total == u32::from(BPS_DENOMINATOR),
        BasketError::InvalidCompositionChange
    );
    require!(changed, BasketError::InvalidCompositionChange);
    Ok(())
}

/// The page index a change must create, if its additions overflow the last page.
/// Pages before the last are always full, so components fill the last page first.
pub fn new_page_for_additions(component_count: usize, additions: usize) -> Result<Option<u8>> {
    require!(component_count > 0, BasketError::InvalidCompositionChange);
    require!(additions <= MAX_COMPOSITION_ADDITIONS, BasketError::InvalidComponentCount);
    let used_in_last = component_count % MAX_LARGE_BASKET_COMPONENTS_PER_PAGE;
    let free_in_last = if used_in_last == 0 {
        0
    } else {
        MAX_LARGE_BASKET_COMPONENTS_PER_PAGE - used_in_last
    };
    if additions <= free_in_last {
        return Ok(None);
    }
    let page_index = component_count.div_ceil(MAX_LARGE_BASKET_COMPONENTS_PER_PAGE);
    Ok(Some(
        u8::try_from(page_index).map_err(|_| error!(BasketError::ArithmeticOverflow))?,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn component(mint: Pubkey, weight: u16) -> LargeBasketComponent {
        LargeBasketComponent {
            mint,
            units_per_index: 1,
            target_weight_bps: weight,
            oracle_pair: if mint == USDC_MINT { Pubkey::default() } else { Pubkey::new_unique() },
            token_program: anchor_spl::token::ID,
            vault: Pubkey::new_unique(),
            accounted_reserve: if weight > 0 { 1_000 } else { 0 },
            decimals: 6,
        }
    }

    fn addition(weight: u16) -> ComponentAddition {
        ComponentAddition {
            mint: Pubkey::new_unique(),
            oracle_pair: Pubkey::new_unique(),
            target_weight_bps: weight,
        }
    }

    fn basket() -> Vec<LargeBasketComponent> {
        vec![
            component(Pubkey::new_unique(), 6_000),
            component(Pubkey::new_unique(), 4_000),
            component(USDC_MINT, 0),
        ]
    }

    #[test]
    fn reweighting_and_removal_are_valid() {
        let existing = basket();
        let index_mint = Pubkey::new_unique();
        assert!(validate_composition_change(&existing, &index_mint, &[5_000, 5_000, 0], &[]).is_ok());
        assert!(validate_composition_change(&existing, &index_mint, &[10_000, 0, 0], &[]).is_ok());
    }

    #[test]
    fn additions_take_weight_from_existing_components() {
        let existing = basket();
        let index_mint = Pubkey::new_unique();
        assert!(validate_composition_change(&existing, &index_mint, &[5_000, 4_000, 0], &[addition(1_000)]).is_ok());
        // An unchanged composition with no additions is not a change.
        assert!(validate_composition_change(&existing, &index_mint, &[6_000, 4_000, 0], &[]).is_err());
    }

    #[test]
    fn weights_must_sum_to_one_hundred_percent() {
        let existing = basket();
        let index_mint = Pubkey::new_unique();
        assert!(validate_composition_change(&existing, &index_mint, &[6_000, 4_000, 0], &[addition(1_000)]).is_err());
        assert!(validate_composition_change(&existing, &index_mint, &[5_000, 4_000, 0], &[]).is_err());
    }

    #[test]
    fn rejects_a_weight_list_that_does_not_cover_every_component() {
        let existing = basket();
        assert!(validate_composition_change(&existing, &Pubkey::new_unique(), &[5_000, 5_000], &[]).is_err());
    }

    #[test]
    fn usdc_cash_slot_keeps_its_weight() {
        let existing = basket();
        assert!(validate_composition_change(&existing, &Pubkey::new_unique(), &[5_000, 4_000, 1_000], &[]).is_err());
    }

    #[test]
    fn rejects_invalid_additions() {
        let existing = basket();
        let index_mint = Pubkey::new_unique();
        let weights = [5_000, 4_000, 0];
        let mut zero_weight = addition(0);
        zero_weight.target_weight_bps = 0;
        assert!(validate_composition_change(&existing, &index_mint, &[6_000, 4_000, 0], &[zero_weight]).is_err());
        let mut no_oracle = addition(1_000);
        no_oracle.oracle_pair = Pubkey::default();
        assert!(validate_composition_change(&existing, &index_mint, &weights, &[no_oracle]).is_err());
        let mut usdc = addition(1_000);
        usdc.mint = USDC_MINT;
        assert!(validate_composition_change(&existing, &index_mint, &weights, &[usdc]).is_err());
        let mut own_token = addition(1_000);
        own_token.mint = index_mint;
        assert!(validate_composition_change(&existing, &index_mint, &weights, &[own_token]).is_err());
        let mut existing_mint = addition(1_000);
        existing_mint.mint = existing[0].mint;
        assert!(validate_composition_change(&existing, &index_mint, &weights, &[existing_mint]).is_err());
        let twice = addition(500);
        assert!(validate_composition_change(&existing, &index_mint, &weights, &[twice.clone(), twice]).is_err());
    }

    #[test]
    fn caps_the_number_of_additions() {
        let existing = basket();
        let additions: Vec<_> = (0..=MAX_COMPOSITION_ADDITIONS).map(|_| addition(100)).collect();
        let weights = [10_000 - 100 * additions.len() as u16, 0, 0];
        assert!(validate_composition_change(&existing, &Pubkey::new_unique(), &weights, &additions).is_err());
    }

    #[test]
    fn needs_the_usdc_cash_slot() {
        let existing = vec![component(Pubkey::new_unique(), 5_000), component(Pubkey::new_unique(), 5_000)];
        assert!(validate_composition_change(&existing, &Pubkey::new_unique(), &[6_000, 4_000], &[]).is_err());
    }

    #[test]
    fn rejects_weights_too_small_to_hold_a_unit() {
        let existing = basket();
        let index_mint = Pubkey::new_unique();
        assert!(validate_composition_change(&existing, &index_mint, &[9_990, 10, 0], &[]).is_err());
        assert!(validate_composition_change(&existing, &index_mint, &[6_000, 3_990, 0], &[addition(10)]).is_err());
        assert!(validate_composition_change(&existing, &index_mint, &[9_950, 50, 0], &[]).is_ok());
    }

    #[test]
    fn caps_components_priced_during_the_switchover() {
        // 2 held + 8 new = 10 priced: allowed.
        let existing = basket();
        let index_mint = Pubkey::new_unique();
        let eight: Vec<_> = (0..8).map(|_| addition(500)).collect();
        assert!(validate_composition_change(&existing, &index_mint, &[3_000, 3_000, 0], &eight).is_ok());
        // Removing a held component still prices it while it is sold: 11.
        let nine: Vec<_> = (0..9).map(|_| addition(500)).collect();
        assert!(validate_composition_change(&existing, &index_mint, &[5_500, 0, 0], &nine).is_err());
        // An already-emptied removed component is not priced.
        let mut emptied = basket();
        emptied[1].target_weight_bps = 0;
        emptied[1].accounted_reserve = 0;
        assert!(validate_composition_change(&emptied, &index_mint, &[5_500, 0, 0], &nine).is_ok());
    }

    #[test]
    fn caps_total_slots() {
        // Removed components keep their slots: 14 empty ones, one weighted, and USDC.
        let mut existing: Vec<_> = (0..15).map(|_| component(Pubkey::new_unique(), 0)).collect();
        existing[0].target_weight_bps = 10_000;
        existing[0].accounted_reserve = 1_000;
        existing.push(component(USDC_MINT, 0));
        let index_mint = Pubkey::new_unique();
        let additions: Vec<_> = (0..5).map(|_| addition(500)).collect();
        let mut weights = vec![0u16; existing.len()];
        weights[0] = 8_000;
        assert!(validate_composition_change(&existing, &index_mint, &weights, &additions[..4]).is_ok(), "20 slots");
        weights[0] = 7_500;
        assert!(validate_composition_change(&existing, &index_mint, &weights, &additions).is_err(), "21 slots");
    }

    #[test]
    fn new_page_only_when_the_last_page_overflows() {
        assert_eq!(new_page_for_additions(6, 4).unwrap(), None);
        assert_eq!(new_page_for_additions(6, 5).unwrap(), Some(1));
        assert_eq!(new_page_for_additions(10, 1).unwrap(), Some(1));
        assert_eq!(new_page_for_additions(10, 10).unwrap(), Some(1));
        assert_eq!(new_page_for_additions(11, 9).unwrap(), None);
        assert_eq!(new_page_for_additions(11, 10).unwrap(), Some(2));
        assert_eq!(new_page_for_additions(3, 0).unwrap(), None);
        assert!(new_page_for_additions(3, MAX_COMPOSITION_ADDITIONS + 1).is_err());
    }
}
