use anchor_lang::prelude::*;

/// A component appended to a fixed-weight basket by a composition change.
#[derive(AnchorSerialize, AnchorDeserialize, Clone, Debug, PartialEq, Eq)]
pub struct ComponentAddition {
    pub mint: Pubkey,
    pub oracle_pair: Pubkey,
    pub target_weight_bps: u16,
}

impl ComponentAddition {
    pub const SPACE: usize = 32 + 32 + 2;
}

/// An announced change to a fixed-weight basket's target weights and/or components.
/// It can only be applied COMPOSITION_CHANGE_DELAY_SECONDS after it was proposed, so
/// holders see it coming and can redeem first. Setting a weight to zero removes a
/// component: the next rebalance sells it and it then sits empty in its slot.
#[account]
pub struct CompositionChange {
    pub index: Pubkey,
    pub proposer: Pubkey,
    pub proposed_at: i64,
    pub effective_at: i64,
    pub bump: u8,
    /// Total redeem fee when proposed. Applying requires redemptions to be open at no more
    /// than this, so the notice period is a real chance to exit.
    pub redeem_fee_bps: u16,
    /// New target weight for each existing component, in global component order.
    pub target_weights_bps: Vec<u16>,
    /// Components to append after the existing ones, in this order.
    pub additions: Vec<ComponentAddition>,
}

impl CompositionChange {
    pub fn space(weights: usize, additions: usize) -> usize {
        32 + 32 + 8 + 8 + 1 + 2 + 4 + weights * 2 + 4 + additions * ComponentAddition::SPACE
    }
}
