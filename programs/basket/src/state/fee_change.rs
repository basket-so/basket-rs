use anchor_lang::prelude::*;

use super::IndexFees;

/// An announced fee increase. It can only be applied FEE_CHANGE_DELAY_SECONDS after it was
/// proposed, so holders see it coming and can redeem at the old rates first.
#[account]
pub struct FeeChange {
    pub index: Pubkey,
    pub proposer: Pubkey,
    pub proposed_at: i64,
    pub effective_at: i64,
    pub bump: u8,
    pub fees: IndexFees,
}

impl FeeChange {
    pub const SPACE: usize = 32 + 32 + 8 + 8 + 1 + IndexFees::SPACE;
}
