use anchor_lang::prelude::*;

use crate::state::{ComponentAddition, IndexKind, LargeBasketIntentKind};

#[event]
pub struct IndexCreated {
    pub index: Pubkey,
    pub index_mint: Pubkey,
    pub authority: Pubkey,
    pub kind: IndexKind,
    pub components: u8,
}

#[event]
pub struct ProtocolConfigInitialized {
    pub authority: Pubkey,
    pub index_creator: Pubkey,
    pub permissionless_index_creation: bool,
}

#[event]
pub struct ProtocolConfigUpdated {
    pub old_authority: Pubkey,
    pub new_authority: Pubkey,
    pub old_index_creator: Pubkey,
    pub new_index_creator: Pubkey,
    pub old_permissionless_index_creation: bool,
    pub new_permissionless_index_creation: bool,
}

#[event]
pub struct IndexCreatorWhitelistUpdated {
    pub authority: Pubkey,
    pub creator: Pubkey,
    pub whitelisted: bool,
}

#[event]
pub struct IndexMinted {
    pub index: Pubkey,
    pub depositor: Pubkey,
    pub amount: u64,
}

#[event]
pub struct IndexRedeemed {
    pub index: Pubkey,
    pub redeemer: Pubkey,
    pub amount: u64,
}

#[event]
pub struct IndexFeesUpdated {
    pub index: Pubkey,
    pub authority: Pubkey,
    pub mint_fee_bps: u16,
    pub redeem_fee_bps: u16,
    pub creator_mint_fee_bps: u16,
    pub creator_redeem_fee_bps: u16,
    pub staking_mint_fee_bps: u16,
    pub staking_redeem_fee_bps: u16,
}

#[event]
pub struct IndexRebalanced {
    pub index: Pubkey,
    pub authority: Pubkey,
    pub components: u8,
    pub supply: u64,
}

#[derive(AnchorSerialize, AnchorDeserialize, Clone, Debug)]
pub struct ComponentAmountQuote {
    pub mint: Pubkey,
    pub gross_amount: u64,
    pub fee_amount: u64,
    pub net_amount: u64,
}

#[event]
pub struct IndexConfigUpdated {
    pub index: Pubkey,
    pub authority: Pubkey,
    pub fee_recipient: Pubkey,
    pub creator_fee_recipient: Pubkey,
    pub max_supply: u64,
    pub rebalance_delay_seconds: i64,
    pub minting_paused: bool,
    pub redeeming_paused: bool,
    pub rebalancing_paused: bool,
}

#[event]
pub struct IndexAuthorityUpdated {
    pub index: Pubkey,
    pub old_authority: Pubkey,
    pub new_authority: Pubkey,
}

#[event]
pub struct IndexMetadataUpdated {
    pub index: Pubkey,
    pub metadata: Pubkey,
    pub uri: String,
}

#[event]
pub struct IndexMetadataAuthorityMigrated {
    pub index: Pubkey,
    pub metadata: Pubkey,
    pub update_authority: Pubkey,
}

#[event]
pub struct IndexFeesClaimed {
    pub index: Pubkey,
    pub authority: Pubkey,
    pub fee_recipient: Pubkey,
    pub amounts: Vec<ComponentAmountQuote>,
}

#[event]
pub struct IndexRebalanceProposed {
    pub index: Pubkey,
    pub authority: Pubkey,
    pub components: u8,
    pub available_at: i64,
    pub nonce: u64,
    pub quote_mint: Pubkey,
    pub old_nav_nad: u128,
    pub new_nav_nad: u128,
    pub oracle_price_tolerance_bps: u16,
    pub nav_tolerance_bps: u16,
}

#[event]
pub struct IndexRebalanceCancelled {
    pub index: Pubkey,
    pub authority: Pubkey,
    pub nonce: u64,
}

#[event]
pub struct IndexMintQuote {
    pub index: Pubkey,
    pub amount: u64,
    pub mint_fee_bps: u16,
    pub components: Vec<ComponentAmountQuote>,
}

#[event]
pub struct IndexRedeemQuote {
    pub index: Pubkey,
    pub amount: u64,
    pub redeem_fee_bps: u16,
    pub components: Vec<ComponentAmountQuote>,
}

#[event]
pub struct FixedWeightRebalanceExecuted {
    pub index: Pubkey,
    pub executor: Pubkey,
    pub supply: u64,
    pub total_nav_nad: u128,
    pub max_drift_bps: u16,
    pub time_triggered: bool,
    pub drift_triggered: bool,
}

#[event]
pub struct FixedWeightConfigUpdated {
    pub index: Pubkey,
    pub authority: Pubkey,
    pub quote_mint: Pubkey,
    pub components: u8,
    pub rebalance_interval_seconds: i64,
    pub drift_threshold_bps: u16,
    pub spot_ema_max_deviation_bps: u16,
}

#[event]
pub struct LargeBasketIntentOpened {
    pub intent: Pubkey,
    pub index: Pubkey,
    pub owner: Pubkey,
    pub nonce: u64,
    pub kind: LargeBasketIntentKind,
    pub index_amount: u64,
    pub fee_basis_usdc_atoms: u64,
    pub protocol_fee_usdc_atoms: u64,
    pub creator_fee_usdc_atoms: u64,
    pub staking_fee_usdc_atoms: u64,
    pub expires_at: i64,
}

#[event]
pub struct LargeBasketComponentPageInitialized {
    pub index: Pubkey,
    pub page: Pubkey,
    pub page_index: u8,
    pub start_component_index: u16,
    pub component_count: u16,
}

#[event]
pub struct LargeBasketConfigFinalized {
    pub index: Pubkey,
    pub component_count: u8,
    pub page_count: u8,
}

#[event]
pub struct LargeBasketComponentOraclePairUpdated {
    pub index: Pubkey,
    pub authority: Pubkey,
    pub page: Pubkey,
    pub component_index: u16,
    pub component_mint: Pubkey,
    pub oracle_pair: Pubkey,
}

#[event]
pub struct LargeBasketComponentFilled {
    pub intent: Pubkey,
    pub index: Pubkey,
    pub owner: Pubkey,
    pub component_index: u16,
    pub amount: u64,
    pub quote_atoms: u64,
}

#[event]
pub struct LargeBasketIntentFinalized {
    pub intent: Pubkey,
    pub index: Pubkey,
    pub owner: Pubkey,
    pub kind: LargeBasketIntentKind,
    pub index_amount: u64,
    pub quote_atoms_executed: u64,
}

#[event]
pub struct StakingPoolInitialized {
    pub authority: Pubkey,
    pub basket_mint: Pubkey,
    pub reward_mint: Pubkey,
}

#[event]
pub struct BasketStaked {
    pub owner: Pubkey,
    pub amount: u64,
    pub total_staked: u64,
}

#[event]
pub struct BasketUnstaked {
    pub owner: Pubkey,
    pub amount: u64,
    pub total_staked: u64,
}

#[event]
pub struct StakingRewardsClaimed {
    pub owner: Pubkey,
    pub amount: u64,
}

#[event]
pub struct StakingRewardsAccrued {
    pub source: Pubkey,
    pub amount: u64,
    pub total_staked: u64,
}

#[event]
pub struct RebalanceIntentOpened {
    pub intent: Pubkey,
    pub index: Pubkey,
    pub initiator: Pubkey,
    pub nonce: u64,
    pub component_count: u16,
    pub sell_legs: u16,
    pub buy_legs: u16,
    pub total_nav_nad: u128,
    pub max_drift_bps: u16,
    pub time_triggered: bool,
    pub drift_triggered: bool,
    pub expires_at: i64,
}

#[event]
pub struct RebalanceComponentSwapped {
    pub intent: Pubkey,
    pub index: Pubkey,
    pub component_index: u16,
    pub is_sell: bool,
    pub component_atoms: u64,
    pub quote_atoms: u64,
}

#[event]
pub struct RebalanceIntentUnwound {
    pub intent: Pubkey,
    pub index: Pubkey,
    pub caller: Pubkey,
    pub nonce: u64,
    pub expired: bool,
    pub completed_sells: u16,
    pub completed_buys: u16,
}

#[event]
pub struct RebalanceKeeperUpdated {
    pub index: Pubkey,
    pub keeper: Pubkey,
}

#[event]
pub struct RebalanceRequestUpdated {
    pub index: Pubkey,
    pub operator: Pubkey,
    pub requested: bool,
    pub requested_at: i64,
}

#[event]
pub struct CompositionChangeProposed {
    pub index: Pubkey,
    pub proposer: Pubkey,
    pub effective_at: i64,
    pub redeem_fee_bps: u16,
    pub target_weights_bps: Vec<u16>,
    pub additions: Vec<ComponentAddition>,
}

/// Redemptions were paused or unpaused, or the redeem fee changed, while a change was
/// pending: its notice restarts from now.
#[event]
pub struct CompositionChangeDelayed {
    pub index: Pubkey,
    pub effective_at: i64,
}

#[event]
pub struct CompositionChangeCancelled {
    pub index: Pubkey,
    pub authority: Pubkey,
}

#[event]
pub struct CompositionChangeApplied {
    pub index: Pubkey,
    pub operator: Pubkey,
    pub component_count: u8,
}

/// The protocol authority set the key that posts rebalance prices; every posted price was
/// cleared.
#[event]
pub struct PriceOracleSet {
    pub previous: Pubkey,
    pub oracle: Pubkey,
}
