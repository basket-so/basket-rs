use anchor_lang::prelude::*;

use crate::constants::REBALANCE_REQUEST_WINDOW_SECONDS;
use crate::utils::index_base_units;

#[derive(AnchorSerialize, AnchorDeserialize, Clone, Copy, Debug, PartialEq, Eq)]
pub enum IndexKind {
    FixedUnits,
    FixedWeights,
}

impl IndexKind {
    pub const SPACE: usize = 1;
}

#[derive(AnchorSerialize, AnchorDeserialize, Clone, Debug, PartialEq, Eq)]
pub struct IndexComponent {
    pub mint: Pubkey,
    pub units_per_index: u64,
    pub target_weight_bps: u16,
    pub oracle_pair: Pubkey,
}

impl IndexComponent {
    pub const SPACE: usize = 32 + 8 + 2 + 32;
}

#[derive(AnchorSerialize, AnchorDeserialize, Clone, Debug)]
pub struct IndexComponentInput {
    pub mint: Pubkey,
    pub units_per_index: u64,
    pub target_weight_bps: u16,
    pub oracle_pair: Pubkey,
}

#[account]
pub struct IndexState {
    pub authority: Pubkey,
    pub creator: Pubkey,
    pub fee_recipient: Pubkey,
    pub creator_fee_recipient: Pubkey,
    pub index_mint: Pubkey,
    pub vault_authority_bump: u8,
    pub index_bump: u8,
    pub index_mint_bump: u8,
    pub decimals: u8,
    pub kind: IndexKind,
    pub large_basket_component_count: u8,
    pub large_basket_page_count: u8,
    pub page_generation: u64,
    pub large_basket_configured: bool,
    // Set only while a rebalance intent is open. Mint and redeem intents run concurrently
    // and are tracked by open_intent_count instead, so no single user can hold this.
    pub large_basket_operation_in_progress: bool,
    pub mint_fee_bps: u16,
    pub redeem_fee_bps: u16,
    pub creator_mint_fee_bps: u16,
    pub creator_redeem_fee_bps: u16,
    pub staking_mint_fee_bps: u16,
    pub staking_redeem_fee_bps: u16,
    pub max_supply: u64,
    pub rebalance_delay_seconds: i64,
    pub fixed_weight_rebalance_interval_seconds: i64,
    pub fixed_weight_last_rebalanced_at: i64,
    // When the keeper last requested a rebalance; the request lapses after
    // REBALANCE_REQUEST_WINDOW_SECONDS. (Reuses the retired pending_rebalance_available_at slot.)
    pub rebalance_requested_at: i64,
    // Mint and redeem intents opened but not yet finalized or cancelled. Intents from many
    // users run concurrently; a rebalance only opens when this is zero.
    pub open_intent_count: u32,
    // Counts restarts of the basket's composition: each mint that settles into an empty
    // basket starts a new era. Mints price against the era they opened in (see
    // FinalizeLargeBasketMintIntent). (This and open_intent_count share the retired nonce slot.)
    pub supply_era: u32,
    // Wallet allowed, besides the authority, to request and open rebalances. (Retired slot.)
    pub rebalance_keeper: Pubkey,
    pub fixed_weight_quote_mint: Pubkey,
    pub active_rebalance_intent: Pubkey,
    pub pending_rebalance_oracle_price_tolerance_bps: u16,
    pub pending_rebalance_nav_tolerance_bps: u16,
    pub fixed_weight_drift_threshold_bps: u16,
    pub fixed_weight_spot_ema_max_deviation_bps: u16,
    pub minting_paused: bool,
    pub redeeming_paused: bool,
    pub rebalancing_paused: bool,
    // While set (and within the request window), new mint/redeem intents are refused so
    // open ones can drain before a rebalance. (Retired pending_rebalance_ready slot.)
    pub rebalance_requested: bool,
    // Set when a composition change is applied: holdings no longer match the new targets,
    // so the next rebalance may open without waiting for drift or the interval. Cleared
    // when a rebalance finalizes. (Was the 1-byte reserved slot.)
    pub composition_rebalance_due: bool,
    pub name: String,
    pub symbol: String,
    pub metadata_uri: String,
}

impl IndexState {
    pub fn space(name_len: usize, symbol_len: usize, metadata_uri_len: usize) -> usize {
        32 + 32 + 32 + 32 + 32 // authority, creator, fee_recipient, creator_fee_recipient, index_mint
            + 1 // vault_authority_bump
            + 1 // index_bump
            + 1 // index_mint_bump
            + 1 // decimals
            + IndexKind::SPACE // kind
            + 1 // large_basket_component_count
            + 1 // large_basket_page_count
            + 8 // page_generation
            + 1 // large_basket_configured
            + 1 // large_basket_operation_in_progress
            + 2 + 2 + 2 + 2 + 2 + 2 // mint/redeem/creator_mint/creator_redeem/staking_mint/staking_redeem fee bps
            + 8 // max_supply
            + 8 // rebalance_delay_seconds
            + 8 // fixed_weight_rebalance_interval_seconds
            + 8 // fixed_weight_last_rebalanced_at
            + 8 // rebalance_requested_at
            + 4 // open_intent_count
            + 4 // supply_era
            + 32 // rebalance_keeper
            + 32 // fixed_weight_quote_mint
            + 32 // active_rebalance_intent
            + 2 // pending_rebalance_oracle_price_tolerance_bps
            + 2 // pending_rebalance_nav_tolerance_bps
            + 2 // fixed_weight_drift_threshold_bps
            + 2 // fixed_weight_spot_ema_max_deviation_bps
            + 1 // minting_paused
            + 1 // redeeming_paused
            + 1 // rebalancing_paused
            + 1 // rebalance_requested
            + 1 // composition_rebalance_due
            + 4 + name_len
            + 4 + symbol_len
            + 4 + metadata_uri_len
    }

    pub fn index_base_units(&self) -> Result<u64> {
        index_base_units(self.decimals)
    }

    /// A keeper's rebalance request holds back new intents until it is used or lapses.
    pub fn rebalance_request_active(&self, now: i64) -> bool {
        self.rebalance_requested
            && now < self.rebalance_requested_at.saturating_add(REBALANCE_REQUEST_WINDOW_SECONDS)
    }

    /// The authority, or the keeper it designated, may request and open rebalances.
    pub fn is_rebalance_operator(&self, key: &Pubkey) -> bool {
        *key == self.authority
            || (self.rebalance_keeper != Pubkey::default() && *key == self.rebalance_keeper)
    }

    /// Whether a new mint or redeem intent may open: not during a rebalance or a request.
    pub fn accepts_new_intents(&self, now: i64) -> bool {
        !self.large_basket_operation_in_progress && !self.rebalance_request_active(now)
    }

    pub fn track_opened_intent(&mut self) -> Result<()> {
        self.open_intent_count = self
            .open_intent_count
            .checked_add(1)
            .ok_or_else(|| error!(crate::errors::BasketError::ArithmeticOverflow))?;
        Ok(())
    }

    /// Saturating so a terminal step (finalize/cancel) can never be blocked by the counter.
    pub fn track_closed_intent(&mut self) {
        self.open_intent_count = self.open_intent_count.saturating_sub(1);
    }

    /// Whether a mint priced in `opened_era` can settle at `supply`. A mint opened into an
    /// empty basket is priced by units_per_index and every other mint by the reserves-per-token
    /// ratio, so each may only join a composition built on the same basis: units-priced mints
    /// join the restart they belong to (or start one), ratio-priced mints the era they opened in.
    pub fn mint_basis_holds(&self, opened_era: u32, opened_empty: bool, supply: u64) -> bool {
        if opened_empty {
            supply == 0 || self.supply_era == opened_era.wrapping_add(1)
        } else {
            supply > 0 && self.supply_era == opened_era
        }
    }

    /// Checks mint_basis_holds and starts a new era when the mint settles into an empty basket.
    pub fn settle_mint_era(&mut self, opened_era: u32, opened_empty: bool, supply: u64) -> Result<()> {
        require!(
            self.mint_basis_holds(opened_era, opened_empty, supply),
            crate::errors::BasketError::MintBasisChanged
        );
        if supply == 0 {
            self.supply_era = self.supply_era.wrapping_add(1);
        }
        Ok(())
    }
}
