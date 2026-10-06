use anchor_lang::prelude::*;

use crate::constants::LARGE_BASKET_COMPONENT_BITMAP_BYTES;

#[derive(AnchorSerialize, AnchorDeserialize, Clone, Copy, Debug, PartialEq, Eq)]
pub enum LargeBasketIntentKind {
    Mint,
    Redeem,
}

impl LargeBasketIntentKind {
    pub const SPACE: usize = 1;
}

#[derive(AnchorSerialize, AnchorDeserialize, Clone, Copy, Debug, PartialEq, Eq)]
pub enum LargeBasketIntentStatus {
    Open,
    Finalized,
    Cancelled,
    // Expired and partly returned to its owner (see cancel_expired_large_basket_intent);
    // still counted as open until every owed component is back.
    Refunding,
}

impl LargeBasketIntentStatus {
    pub const SPACE: usize = 1;
}

#[account]
pub struct LargeBasketIntent {
    pub index: Pubkey,
    pub owner: Pubkey,
    pub nonce: u64,
    pub kind: LargeBasketIntentKind,
    pub status: LargeBasketIntentStatus,
    pub index_amount: u64,
    pub supply_snapshot: u64,
    pub post_supply: u64,
    pub quote_mint: Pubkey,
    // Design B: fees are computed at finalize from the actual quote executed, so
    // the intent snapshots the fee RATES (bps) at open instead of pre-computed
    // atom amounts. The *_usdc_atoms fields below are populated at finalize and
    // hold the realized fee amounts (0 until then).
    pub fee_basis_usdc_atoms: u64,
    pub protocol_fee_usdc_atoms: u64,
    pub creator_fee_usdc_atoms: u64,
    pub staking_fee_usdc_atoms: u64,
    pub protocol_fee_recipient: Pubkey,
    pub creator_fee_recipient: Pubkey,
    pub staking_pool: Pubkey,
    pub opened_at: i64,
    pub expires_at: i64,
    pub component_count: u16,
    pub completed_components: u16,
    pub component_fill_bitmap: [u8; LARGE_BASKET_COMPONENT_BITMAP_BYTES],
    pub component_amounts: Vec<u64>,
    pub quote_atoms_executed: u64,
    pub max_quote_in: u64,
    pub min_quote_out: u64,
    pub fees_collected: bool,
    pub protocol_fee_bps: u16,
    pub creator_fee_bps: u16,
    pub staking_fee_bps: u16,
    pub bump: u8,
    // Deferred price-check: the per-component quote actually spent (mint) / received
    // (redeem) is recorded at execute time, and the oracle price-bound is checked
    // later by verify_*_component_price (keeps the oracle quote out of the swap tx,
    // which would otherwise exceed the 1232-byte limit). component_verified_bitmap
    // mirrors component_fill_bitmap; finalize requires every filled component verified.
    pub component_quote_atoms: Vec<u64>,
    pub component_verified_bitmap: [u8; LARGE_BASKET_COMPONENT_BITMAP_BYTES],
    // When true the components are deposited/withdrawn in-kind (the exact component
    // tokens move to/from the owner) instead of being swapped via Jupiter. Fees are
    // skimmed in-kind at execute time, so the USDC collect/budget leg is skipped.
    pub in_kind: bool,
    // The basket's supply_era when this intent opened.
    pub supply_era: u32,
    // Components already returned by cancel_expired_large_basket_intent, to the owner or
    // to the refund escrow.
    pub refunded_bitmap: [u8; LARGE_BASKET_COMPONENT_BITMAP_BYTES],
    // Components sitting in the refund escrow until the owner claims them
    // (claim_large_basket_refund).
    pub escrowed_bitmap: [u8; LARGE_BASKET_COMPONENT_BITMAP_BYTES],
    pub reserved: [u8; 6],
}

impl LargeBasketIntent {
    pub const SPACE: usize = 32
        + 32
        + 8
        + LargeBasketIntentKind::SPACE
        + LargeBasketIntentStatus::SPACE
        + 8
        + 8
        + 8
        + 32
        + 8
        + 8
        + 8
        + 8
        + 32
        + 32
        + 32
        + 8
        + 8
        + 2
        + 2
        + LARGE_BASKET_COMPONENT_BITMAP_BYTES
        + 4
        + (crate::constants::MAX_LARGE_BASKET_COMPONENTS * 8)
        + 8
        + 8
        + 8
        + 1
        + 2
        + 2
        + 2
        + 1
        // component_quote_atoms: Vec<u64>
        + 4
        + (crate::constants::MAX_LARGE_BASKET_COMPONENTS * 8)
        // component_verified_bitmap
        + LARGE_BASKET_COMPONENT_BITMAP_BYTES
        // in_kind
        + 1
        // supply_era
        + 4
        // refunded_bitmap
        + LARGE_BASKET_COMPONENT_BITMAP_BYTES
        // escrowed_bitmap
        + LARGE_BASKET_COMPONENT_BITMAP_BYTES
        + 6;
}

#[account]
pub struct LargeBasketIntentLock {
    pub index: Pubkey,
    pub owner: Pubkey,
    pub active_intent: Pubkey,
    pub bump: u8,
    pub reserved: [u8; 31],
}

impl LargeBasketIntentLock {
    pub const SPACE: usize = 32 + 32 + 32 + 1 + 31;
}
