use anchor_lang::prelude::*;

use crate::constants::{LARGE_BASKET_COMPONENT_BITMAP_BYTES, MAX_LARGE_BASKET_COMPONENTS};

#[derive(AnchorSerialize, AnchorDeserialize, Clone, Copy, Debug, PartialEq, Eq)]
pub enum RebalanceMode {
    /// Authority-proposed target change (new units or weights; add/remove allowed) gated
    /// by a timelock. Builds a pending page set (`target_generation`) that is activated
    /// atomically at finalize.
    Reconfigure,
    /// Permissionless FixedWeights keeper rebalance triggered by oracle drift / time.
    /// Edits the active page set in place (`target_generation` == active generation).
    KeeperDrift,
}

impl RebalanceMode {
    pub const SPACE: usize = 1;
}

#[derive(AnchorSerialize, AnchorDeserialize, Clone, Copy, Debug, PartialEq, Eq)]
pub enum RebalanceStatus {
    Open,
    Finalized,
    Cancelled,
}

impl RebalanceStatus {
    pub const SPACE: usize = 1;
}

/// Index-scoped, in-flight rebalance. Only one can be active per index at a time
/// (tracked by `IndexState.active_rebalance_intent` + `operation_in_progress`, which
/// also mutually-excludes mint/redeem). Mirrors the large-basket mint/redeem intent
/// machinery but rebalance-shaped: targets + sell/buy progress instead of one fill set.
#[account]
pub struct RebalanceIntent {
    pub index: Pubkey,
    pub initiator: Pubkey,
    pub nonce: u64,
    pub mode: RebalanceMode,
    pub status: RebalanceStatus,
    pub supply_snapshot: u64,
    pub opened_at: i64,
    pub expires_at: i64,
    pub component_count: u16,
    // Pending page-set generation holding the target config (Reconfigure). Equal to the
    // active generation for KeeperDrift, which edits the live pages in place.
    pub target_generation: u64,
    // Per-component swap LEG amount (atoms): for a sell leg, the exact component atoms to
    // sell to USDC; for a buy leg, the minimum component atoms to acquire with USDC. 0 for
    // on-target / quote components. Computed at open from the oracle-priced NAV and the
    // component's target weight; mirrors LargeBasketIntent.component_amounts so the same
    // deferred price-verify machinery applies.
    pub component_target_amounts: Vec<u64>,
    // Per-component swap progress. Sells move overweight components -> USDC; buys move
    // USDC -> underweight components. `*_done` track completion; `*_leg` (below) record the
    // immutable direction each component was assigned at open.
    pub sell_done_bitmap: [u8; LARGE_BASKET_COMPONENT_BITMAP_BYTES],
    pub buy_done_bitmap: [u8; LARGE_BASKET_COMPONENT_BITMAP_BYTES],
    pub completed_sells: u16,
    pub completed_buys: u16,
    // Deferred oracle verify: USDC received (sell) / spent (buy) per component, recorded
    // at execute time and bounded against a fresh oracle quote by the verify instruction.
    pub component_quote_atoms: Vec<u64>,
    pub component_verified_bitmap: [u8; LARGE_BASKET_COMPONENT_BITMAP_BYTES],
    // NAV-preservation accumulators (Reconfigure). Each page is priced against a fresh
    // oracle quote and summed here; finalize requires |old - new| within nav_tolerance_bps.
    pub old_nav_nad: u128,
    pub new_nav_nad: u128,
    pub nav_priced_bitmap: [u8; LARGE_BASKET_COMPONENT_BITMAP_BYTES],
    // Priced NAV at open (KeeperDrift), kept as open-time telemetry only. The weight
    // targets and drift trigger are computed from the transient open NAV in the handler;
    // finalize's NAV-preservation gate re-marks `component_open_amounts` at fresh prices
    // rather than reading this stale field (see open_rebalance_intent / finalize_rebalance).
    pub total_value_snapshot: u128,
    pub nav_tolerance_bps: u16,
    pub max_post_rebalance_drift_bps: u16,
    pub bump: u8,
    // Immutable per-component leg direction, set at open: exactly one of these (or neither,
    // for an on-target / quote component) is set for each component < component_count.
    pub sell_leg_bitmap: [u8; LARGE_BASKET_COMPONENT_BITMAP_BYTES],
    pub buy_leg_bitmap: [u8; LARGE_BASKET_COMPONENT_BITMAP_BYTES],
    // Actual component atoms moved by each executed leg (sell: atoms sold == the target
    // amount exactly; buy: atoms received, which may exceed the target on over-delivery).
    // Recorded at execute so the deferred price verify bounds the REAL effective price
    // (quote / fill) instead of the open-time target.
    pub component_fill_atoms: Vec<u64>,
    // Per-component vault balance at open. Finalize re-prices THESE original holdings at
    // the fresh finalize oracle to get the "held still" value, and gates the post-swap NAV
    // against it (one-sided loss). Comparing against this re-mark instead of the open-time
    // priced snapshot prevents a keeper from passing finalize by leaking value into
    // market appreciation that occurred during the intent's lifetime.
    pub component_open_amounts: Vec<u64>,
    // Scratch USDC parked in the vault-authority quote ATA at open, used in the finalize
    // re-mark only when USDC is NOT a component (when it is, its open balance is already
    // captured in component_open_amounts, since the vaults alias).
    pub open_scratch_quote_atoms: u64,
    pub reserved: [u8; 18],
}

impl RebalanceIntent {
    pub const SPACE: usize = 32 // index
        + 32 // initiator
        + 8 // nonce
        + RebalanceMode::SPACE
        + RebalanceStatus::SPACE
        + 8 // supply_snapshot
        + 8 // opened_at
        + 8 // expires_at
        + 2 // component_count
        + 8 // target_generation
        + 4 + (MAX_LARGE_BASKET_COMPONENTS * 8) // component_target_amounts
        + LARGE_BASKET_COMPONENT_BITMAP_BYTES // sell_done_bitmap
        + LARGE_BASKET_COMPONENT_BITMAP_BYTES // buy_done_bitmap
        + 2 // completed_sells
        + 2 // completed_buys
        + 4 + (MAX_LARGE_BASKET_COMPONENTS * 8) // component_quote_atoms
        + LARGE_BASKET_COMPONENT_BITMAP_BYTES // component_verified_bitmap
        + 16 // old_nav_nad
        + 16 // new_nav_nad
        + LARGE_BASKET_COMPONENT_BITMAP_BYTES // nav_priced_bitmap
        + 16 // total_value_snapshot
        + 2 // nav_tolerance_bps
        + 2 // max_post_rebalance_drift_bps
        + 1 // bump
        + LARGE_BASKET_COMPONENT_BITMAP_BYTES // sell_leg_bitmap
        + LARGE_BASKET_COMPONENT_BITMAP_BYTES // buy_leg_bitmap
        + 4 + (MAX_LARGE_BASKET_COMPONENTS * 8) // component_fill_atoms
        + 4 + (MAX_LARGE_BASKET_COMPONENTS * 8) // component_open_amounts
        + 8 // open_scratch_quote_atoms
        + 18; // reserved
}

#[cfg(test)]
mod tests {
    use super::{RebalanceIntent, RebalanceMode, RebalanceStatus};
    use crate::constants::{LARGE_BASKET_COMPONENT_BITMAP_BYTES, MAX_LARGE_BASKET_COMPONENTS};
    use anchor_lang::prelude::*;

    /// A fully-populated intent: every `Vec` is at `MAX_LARGE_BASKET_COMPONENTS`, which is
    /// exactly what `RebalanceIntent::SPACE` budgets for. The account would be created with
    /// `space = 8 + SPACE` and is never `realloc`'d, so if the manual `SPACE` accounting
    /// ever drifts from the real borsh layout the `init` allocation is wrong (too small =>
    /// the keeper's first write panics; too large => wasted rent). This is the guard.
    fn max_intent() -> RebalanceIntent {
        RebalanceIntent {
            index: Pubkey::new_unique(),
            initiator: Pubkey::new_unique(),
            nonce: u64::MAX,
            mode: RebalanceMode::KeeperDrift,
            status: RebalanceStatus::Open,
            supply_snapshot: u64::MAX,
            opened_at: i64::MAX,
            expires_at: i64::MAX,
            component_count: MAX_LARGE_BASKET_COMPONENTS as u16,
            target_generation: u64::MAX,
            component_target_amounts: vec![u64::MAX; MAX_LARGE_BASKET_COMPONENTS],
            sell_done_bitmap: [0xFF; LARGE_BASKET_COMPONENT_BITMAP_BYTES],
            buy_done_bitmap: [0xFF; LARGE_BASKET_COMPONENT_BITMAP_BYTES],
            completed_sells: u16::MAX,
            completed_buys: u16::MAX,
            component_quote_atoms: vec![u64::MAX; MAX_LARGE_BASKET_COMPONENTS],
            component_verified_bitmap: [0xFF; LARGE_BASKET_COMPONENT_BITMAP_BYTES],
            old_nav_nad: u128::MAX,
            new_nav_nad: u128::MAX,
            nav_priced_bitmap: [0xFF; LARGE_BASKET_COMPONENT_BITMAP_BYTES],
            total_value_snapshot: u128::MAX,
            nav_tolerance_bps: u16::MAX,
            max_post_rebalance_drift_bps: u16::MAX,
            bump: u8::MAX,
            sell_leg_bitmap: [0xFF; LARGE_BASKET_COMPONENT_BITMAP_BYTES],
            buy_leg_bitmap: [0xFF; LARGE_BASKET_COMPONENT_BITMAP_BYTES],
            component_fill_atoms: vec![u64::MAX; MAX_LARGE_BASKET_COMPONENTS],
            component_open_amounts: vec![u64::MAX; MAX_LARGE_BASKET_COMPONENTS],
            open_scratch_quote_atoms: u64::MAX,
            reserved: [0xFF; 18],
        }
    }

    #[test]
    fn space_matches_serialized_max_instance() {
        let serialized = max_intent().try_to_vec().unwrap();
        assert_eq!(
            serialized.len(),
            RebalanceIntent::SPACE,
            "RebalanceIntent::SPACE ({}) must equal the borsh size of a max-populated \
             intent ({}); a mismatch mis-sizes the `init` allocation",
            RebalanceIntent::SPACE,
            serialized.len(),
        );
    }

    #[test]
    fn account_fits_a_single_init_allocation() {
        // Anchor prepends an 8-byte discriminator at init.
        let account_size = 8 + RebalanceIntent::SPACE;
        // A brand-new account is capped at 10 KiB without explicit `realloc` steps (the
        // 10 MiB ceiling is the absolute max). The intent must fit one plain `init`.
        assert!(
            account_size <= 10 * 1024,
            "RebalanceIntent account is {account_size} B; must fit a single 10 KiB init \
             (a 50-component intent is ~1 KiB, so there is ample headroom)",
        );
    }
}
