use anchor_lang::prelude::*;

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
    pub pending_rebalance_available_at: i64,
    pub pending_rebalance_nonce: u64,
    pub pending_rebalance_quote_mint: Pubkey,
    pub fixed_weight_quote_mint: Pubkey,
    pub active_rebalance_intent: Pubkey,
    pub pending_rebalance_oracle_price_tolerance_bps: u16,
    pub pending_rebalance_nav_tolerance_bps: u16,
    pub fixed_weight_drift_threshold_bps: u16,
    pub fixed_weight_spot_ema_max_deviation_bps: u16,
    pub minting_paused: bool,
    pub redeeming_paused: bool,
    pub rebalancing_paused: bool,
    pub pending_rebalance_ready: bool,
    pub reserved: [u8; 1],
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
            + 8 // pending_rebalance_available_at
            + 8 // pending_rebalance_nonce
            + 32 // pending_rebalance_quote_mint
            + 32 // fixed_weight_quote_mint
            + 32 // active_rebalance_intent
            + 2 // pending_rebalance_oracle_price_tolerance_bps
            + 2 // pending_rebalance_nav_tolerance_bps
            + 2 // fixed_weight_drift_threshold_bps
            + 2 // fixed_weight_spot_ema_max_deviation_bps
            + 1 // minting_paused
            + 1 // redeeming_paused
            + 1 // rebalancing_paused
            + 1 // pending_rebalance_ready
            + 1 // reserved
            + 4 + name_len
            + 4 + symbol_len
            + 4 + metadata_uri_len
    }

    pub fn index_base_units(&self) -> Result<u64> {
        index_base_units(self.decimals)
    }
}
