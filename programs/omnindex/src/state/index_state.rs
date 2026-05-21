use anchor_lang::prelude::*;

use crate::{errors::OmnindexError, utils::index_base_units};

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
    pub component_count: u8,
    pub pending_component_count: u8,
    pub mint_fee_bps: u16,
    pub redeem_fee_bps: u16,
    pub creator_mint_fee_bps: u16,
    pub creator_redeem_fee_bps: u16,
    pub max_supply: u64,
    pub rebalance_delay_seconds: i64,
    pub fixed_weight_rebalance_interval_seconds: i64,
    pub fixed_weight_last_rebalanced_at: i64,
    pub pending_rebalance_available_at: i64,
    pub pending_rebalance_nonce: u64,
    pub pending_rebalance_quote_mint: Pubkey,
    pub fixed_weight_quote_mint: Pubkey,
    pub pending_rebalance_oracle_price_tolerance_bps: u16,
    pub pending_rebalance_nav_tolerance_bps: u16,
    pub fixed_weight_drift_threshold_bps: u16,
    pub fixed_weight_spot_ema_max_deviation_bps: u16,
    pub minting_paused: bool,
    pub redeeming_paused: bool,
    pub rebalancing_paused: bool,
    pub reserved: [u8; 1],
    pub name: String,
    pub symbol: String,
    pub metadata_uri: String,
    pub components: Vec<IndexComponent>,
    pub pending_components: Vec<IndexComponent>,
}

impl IndexState {
    pub fn space(
        name_len: usize,
        symbol_len: usize,
        metadata_uri_len: usize,
        component_capacity: usize,
    ) -> usize {
        32 + 32
            + 32
            + 32
            + 32
            + 1
            + 1
            + 1
            + 1
            + IndexKind::SPACE
            + 1
            + 1
            + 2
            + 2
            + 2
            + 2
            + 8
            + 8
            + 8
            + 8
            + 8
            + 8
            + 32
            + 32
            + 2
            + 2
            + 2
            + 2
            + 1
            + 1
            + 1
            + 1
            + 4
            + name_len
            + 4
            + symbol_len
            + 4
            + metadata_uri_len
            + 4
            + (component_capacity * IndexComponent::SPACE)
            + 4
            + (component_capacity * IndexComponent::SPACE)
    }

    pub fn index_base_units(&self) -> Result<u64> {
        require!(
            self.component_count as usize == self.components.len(),
            OmnindexError::InvalidComponentCount
        );
        index_base_units(self.decimals)
    }
}
