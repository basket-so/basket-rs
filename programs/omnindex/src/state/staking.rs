use anchor_lang::prelude::*;

#[account]
pub struct StakingPool {
    pub authority: Pubkey,
    pub basket_mint: Pubkey,
    pub reward_mint: Pubkey,
    pub bump: u8,
    pub staking_authority_bump: u8,
    pub total_staked: u64,
    pub reward_per_token_accumulator: u128,
    pub unallocated_rewards: u64,
    pub reward_remainder_scaled: u128,
    pub reserved: [u8; 48],
}

impl StakingPool {
    pub const SPACE: usize = 32 + 32 + 32 + 1 + 1 + 8 + 16 + 8 + 16 + 48;
}

#[account]
pub struct StakePosition {
    pub owner: Pubkey,
    pub staking_pool: Pubkey,
    pub amount_staked: u64,
    pub pending_rewards: u64,
    pub reward_per_token_checkpoint: u128,
    pub bump: u8,
    pub pending_rewards_scaled: u128,
    pub reserved: [u8; 15],
}

impl StakePosition {
    pub const SPACE: usize = 32 + 32 + 8 + 8 + 16 + 1 + 16 + 15;
}
