use anchor_lang::prelude::*;
use anchor_spl::token::TokenAccount;

use crate::{
    constants::REWARD_PER_TOKEN_SCALE,
    errors::OmnindexError,
    state::{StakePosition, StakingPool},
    utils::{associated_token_address, SplTokenAccount},
};

pub fn accrue_staking_rewards(pool: &mut StakingPool, amount: u64) -> Result<()> {
    if amount == 0 && pool.unallocated_rewards == 0 {
        return Ok(());
    }

    let total_rewards = pool
        .unallocated_rewards
        .checked_add(amount)
        .ok_or_else(|| error!(OmnindexError::ArithmeticOverflow))?;

    if pool.total_staked == 0 {
        pool.unallocated_rewards = total_rewards;
        return Ok(());
    }

    let scaled_rewards = u128::from(total_rewards)
        .checked_mul(REWARD_PER_TOKEN_SCALE)
        .and_then(|value| value.checked_add(pool.reward_remainder_scaled))
        .ok_or_else(|| error!(OmnindexError::ArithmeticOverflow))?;
    let total_staked = u128::from(pool.total_staked);
    let increment = scaled_rewards
        .checked_div(total_staked)
        .ok_or_else(|| error!(OmnindexError::ArithmeticOverflow))?;

    pool.reward_per_token_accumulator = pool
        .reward_per_token_accumulator
        .checked_add(increment)
        .ok_or_else(|| error!(OmnindexError::ArithmeticOverflow))?;
    pool.reward_remainder_scaled = scaled_rewards
        .checked_rem(total_staked)
        .ok_or_else(|| error!(OmnindexError::ArithmeticOverflow))?;
    pool.unallocated_rewards = 0;

    Ok(())
}

pub fn settle_stake_position(pool: &StakingPool, position: &mut StakePosition) -> Result<()> {
    let delta = pool
        .reward_per_token_accumulator
        .checked_sub(position.reward_per_token_checkpoint)
        .ok_or_else(|| error!(OmnindexError::ArithmeticOverflow))?;

    if position.amount_staked > 0 && delta > 0 {
        let earned = u128::from(position.amount_staked)
            .checked_mul(delta)
            .and_then(|value| value.checked_div(REWARD_PER_TOKEN_SCALE))
            .ok_or_else(|| error!(OmnindexError::ArithmeticOverflow))?;
        let earned =
            u64::try_from(earned).map_err(|_| error!(OmnindexError::ArithmeticOverflow))?;
        position.pending_rewards = position
            .pending_rewards
            .checked_add(earned)
            .ok_or_else(|| error!(OmnindexError::ArithmeticOverflow))?;
    }

    position.reward_per_token_checkpoint = pool.reward_per_token_accumulator;
    Ok(())
}

pub fn validate_staking_vault(
    token_account: &TokenAccount,
    actual_key: &Pubkey,
    staking_authority: &Pubkey,
    mint: &Pubkey,
) -> Result<()> {
    require_keys_eq!(
        *actual_key,
        associated_token_address(staking_authority, mint),
        OmnindexError::InvalidStakingVault
    );
    require_keys_eq!(
        token_account.owner,
        *staking_authority,
        OmnindexError::InvalidStakingVault
    );
    require_keys_eq!(
        token_account.mint,
        *mint,
        OmnindexError::InvalidStakingVault
    );
    Ok(())
}

pub fn validate_staking_spl_vault(
    token_account: &SplTokenAccount,
    actual_key: &Pubkey,
    staking_authority: &Pubkey,
    mint: &Pubkey,
) -> Result<()> {
    require_keys_eq!(
        *actual_key,
        associated_token_address(staking_authority, mint),
        OmnindexError::InvalidStakingVault
    );
    require_keys_eq!(
        token_account.owner,
        *staking_authority,
        OmnindexError::InvalidStakingVault
    );
    require_keys_eq!(
        token_account.mint,
        *mint,
        OmnindexError::InvalidStakingVault
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pool(total_staked: u64) -> StakingPool {
        StakingPool {
            authority: Pubkey::new_unique(),
            basket_mint: Pubkey::new_unique(),
            reward_mint: Pubkey::new_unique(),
            bump: 255,
            staking_authority_bump: 254,
            total_staked,
            reward_per_token_accumulator: 0,
            unallocated_rewards: 0,
            reward_remainder_scaled: 0,
            reserved: [0; 48],
        }
    }

    fn position(amount_staked: u64) -> StakePosition {
        StakePosition {
            owner: Pubkey::new_unique(),
            staking_pool: Pubkey::new_unique(),
            amount_staked,
            pending_rewards: 0,
            reward_per_token_checkpoint: 0,
            bump: 253,
            reserved: [0; 31],
        }
    }

    #[test]
    fn accrues_rewards_per_staked_token() {
        let mut pool = pool(100);
        accrue_staking_rewards(&mut pool, 25).unwrap();

        let mut position = position(20);
        settle_stake_position(&pool, &mut position).unwrap();

        assert_eq!(position.pending_rewards, 5);
    }

    #[test]
    fn stores_rewards_when_no_stakers_exist() {
        let mut pool = pool(0);
        accrue_staking_rewards(&mut pool, 25).unwrap();

        assert_eq!(pool.unallocated_rewards, 25);
        assert_eq!(pool.reward_per_token_accumulator, 0);
    }

    #[test]
    fn flushes_unallocated_rewards_when_stake_exists() {
        let mut pool = pool(0);
        accrue_staking_rewards(&mut pool, 25).unwrap();
        pool.total_staked = 100;
        accrue_staking_rewards(&mut pool, 0).unwrap();

        let mut position = position(20);
        settle_stake_position(&pool, &mut position).unwrap();

        assert_eq!(position.pending_rewards, 5);
        assert_eq!(pool.unallocated_rewards, 0);
    }

    #[test]
    fn rounding_remainder_does_not_overallocate_rewards() {
        let mut pool = pool(3);

        accrue_staking_rewards(&mut pool, 1).unwrap();
        accrue_staking_rewards(&mut pool, 1).unwrap();
        accrue_staking_rewards(&mut pool, 1).unwrap();

        let mut position = position(3);
        settle_stake_position(&pool, &mut position).unwrap();

        assert_eq!(position.pending_rewards, 3);
        assert_eq!(pool.unallocated_rewards, 0);
        assert!(pool.reward_remainder_scaled < u128::from(pool.total_staked));
    }
}
