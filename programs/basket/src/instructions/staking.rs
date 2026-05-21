use anchor_lang::prelude::*;
use anchor_spl::token::{self, Token, TokenAccount, Transfer};

use crate::{
    constants::{
        BASKET_MINT, PROTOCOL_CONFIG_SEED, STAKE_POSITION_SEED, STAKING_AUTHORITY_SEED,
        STAKING_POOL_SEED, USDC_MINT,
    },
    errors::BasketError,
    events::{
        BasketStaked, BasketUnstaked, StakingPoolInitialized, StakingRewardsAccrued,
        StakingRewardsClaimed,
    },
    state::{ProtocolConfig, StakePosition, StakingPool},
    utils::{
        accrue_staking_rewards, create_associated_token_account_idempotent, load_mint,
        load_user_token_account, settle_stake_position, validate_staking_spl_vault,
        validate_staking_vault, validate_user_token_account, ASSOCIATED_TOKEN_ID,
    },
};

#[derive(AnchorSerialize, AnchorDeserialize, Clone, Debug)]
pub struct StakeBasketArgs {
    pub amount: u64,
}

#[derive(AnchorSerialize, AnchorDeserialize, Clone, Debug)]
pub struct UnstakeBasketArgs {
    pub amount: u64,
}

#[derive(AnchorSerialize, AnchorDeserialize, Clone, Debug)]
pub struct FundStakingRewardsArgs {
    pub amount: u64,
}

#[derive(Accounts)]
pub struct InitializeStakingPool<'info> {
    #[account(mut)]
    pub payer: Signer<'info>,
    pub authority: Signer<'info>,
    #[account(
        seeds = [PROTOCOL_CONFIG_SEED],
        bump = protocol_config.bump,
        has_one = authority @ BasketError::UnauthorizedAuthority
    )]
    pub protocol_config: Account<'info, ProtocolConfig>,
    #[account(
        init,
        payer = payer,
        seeds = [STAKING_POOL_SEED],
        bump,
        space = 8 + StakingPool::SPACE
    )]
    pub staking_pool: Account<'info, StakingPool>,
    /// CHECK: PDA authority over staking vaults.
    #[account(seeds = [STAKING_AUTHORITY_SEED], bump)]
    pub staking_authority: UncheckedAccount<'info>,
    /// CHECK: Validated as the BASKET mint.
    #[account(address = BASKET_MINT @ BasketError::InvalidBasketMint)]
    pub basket_mint: UncheckedAccount<'info>,
    /// CHECK: Validated as the USDC reward mint.
    #[account(address = USDC_MINT @ BasketError::InvalidRewardMint)]
    pub reward_mint: UncheckedAccount<'info>,
    /// CHECK: Created and validated as staking authority's BASKET ATA.
    #[account(mut)]
    pub stake_vault: UncheckedAccount<'info>,
    /// CHECK: Created and validated as staking authority's USDC ATA.
    #[account(mut)]
    pub reward_vault: UncheckedAccount<'info>,
    /// CHECK: Validated as the Associated Token Program.
    #[account(address = ASSOCIATED_TOKEN_ID @ BasketError::InvalidAssociatedTokenProgram)]
    pub associated_token_program: UncheckedAccount<'info>,
    pub token_program: Program<'info, Token>,
    pub system_program: Program<'info, System>,
}

impl<'info> InitializeStakingPool<'info> {
    pub fn handle(ctx: Context<'_, '_, 'info, 'info, Self>) -> Result<()> {
        load_mint(&ctx.accounts.basket_mint.to_account_info())?;
        load_mint(&ctx.accounts.reward_mint.to_account_info())?;

        create_associated_token_account_idempotent(
            ctx.accounts.associated_token_program.to_account_info(),
            ctx.accounts.payer.to_account_info(),
            ctx.accounts.stake_vault.to_account_info(),
            ctx.accounts.staking_authority.to_account_info(),
            ctx.accounts.basket_mint.to_account_info(),
            ctx.accounts.system_program.to_account_info(),
            ctx.accounts.token_program.to_account_info(),
        )?;
        create_associated_token_account_idempotent(
            ctx.accounts.associated_token_program.to_account_info(),
            ctx.accounts.payer.to_account_info(),
            ctx.accounts.reward_vault.to_account_info(),
            ctx.accounts.staking_authority.to_account_info(),
            ctx.accounts.reward_mint.to_account_info(),
            ctx.accounts.system_program.to_account_info(),
            ctx.accounts.token_program.to_account_info(),
        )?;

        let stake_vault = load_user_token_account(&ctx.accounts.stake_vault.to_account_info())?;
        validate_staking_spl_vault(
            &stake_vault,
            &ctx.accounts.stake_vault.key(),
            &ctx.accounts.staking_authority.key(),
            &BASKET_MINT,
        )?;
        let reward_vault = load_user_token_account(&ctx.accounts.reward_vault.to_account_info())?;
        validate_staking_spl_vault(
            &reward_vault,
            &ctx.accounts.reward_vault.key(),
            &ctx.accounts.staking_authority.key(),
            &USDC_MINT,
        )?;

        let pool = &mut ctx.accounts.staking_pool;
        pool.authority = ctx.accounts.authority.key();
        pool.basket_mint = BASKET_MINT;
        pool.reward_mint = USDC_MINT;
        pool.bump = ctx.bumps.staking_pool;
        pool.staking_authority_bump = ctx.bumps.staking_authority;
        pool.total_staked = 0;
        pool.reward_per_token_accumulator = 0;
        pool.unallocated_rewards = 0;
        pool.reward_remainder_scaled = 0;
        pool.reserved = [0; 48];

        emit!(StakingPoolInitialized {
            authority: pool.authority,
            basket_mint: pool.basket_mint,
            reward_mint: pool.reward_mint,
        });

        Ok(())
    }
}

#[derive(Accounts)]
pub struct FundStakingRewards<'info> {
    #[account(mut)]
    pub funder: Signer<'info>,
    #[account(mut, seeds = [STAKING_POOL_SEED], bump = staking_pool.bump)]
    pub staking_pool: Account<'info, StakingPool>,
    /// CHECK: PDA authority over staking vaults.
    #[account(seeds = [STAKING_AUTHORITY_SEED], bump = staking_pool.staking_authority_bump)]
    pub staking_authority: UncheckedAccount<'info>,
    /// CHECK: Validated as funder's USDC token account.
    #[account(mut)]
    pub funder_reward_token_account: UncheckedAccount<'info>,
    #[account(mut)]
    pub reward_vault: Account<'info, TokenAccount>,
    pub token_program: Program<'info, Token>,
}

impl<'info> FundStakingRewards<'info> {
    pub fn handle(ctx: Context<Self>, args: FundStakingRewardsArgs) -> Result<()> {
        require!(args.amount > 0, BasketError::InvalidRewardAmount);
        require_keys_eq!(
            ctx.accounts.staking_pool.reward_mint,
            USDC_MINT,
            BasketError::InvalidRewardMint
        );
        require!(
            ctx.accounts.staking_pool.total_staked > 0,
            BasketError::NoStakedTokens
        );
        validate_staking_vault(
            &ctx.accounts.reward_vault,
            &ctx.accounts.reward_vault.key(),
            &ctx.accounts.staking_authority.key(),
            &USDC_MINT,
        )?;

        let funder_reward_account =
            load_user_token_account(&ctx.accounts.funder_reward_token_account.to_account_info())?;
        validate_user_token_account(
            &funder_reward_account,
            &ctx.accounts.funder.key(),
            &USDC_MINT,
        )?;

        token::transfer(
            CpiContext::new(
                ctx.accounts.token_program.to_account_info(),
                Transfer {
                    from: ctx.accounts.funder_reward_token_account.to_account_info(),
                    to: ctx.accounts.reward_vault.to_account_info(),
                    authority: ctx.accounts.funder.to_account_info(),
                },
            ),
            args.amount,
        )?;

        accrue_staking_rewards(&mut ctx.accounts.staking_pool, args.amount)?;

        emit!(StakingRewardsAccrued {
            source: ctx.accounts.funder.key(),
            amount: args.amount,
            total_staked: ctx.accounts.staking_pool.total_staked,
        });

        Ok(())
    }
}

#[derive(Accounts)]
pub struct StakeBasket<'info> {
    #[account(mut)]
    pub owner: Signer<'info>,
    #[account(mut, seeds = [STAKING_POOL_SEED], bump = staking_pool.bump)]
    pub staking_pool: Account<'info, StakingPool>,
    /// CHECK: PDA authority over staking vaults.
    #[account(seeds = [STAKING_AUTHORITY_SEED], bump = staking_pool.staking_authority_bump)]
    pub staking_authority: UncheckedAccount<'info>,
    #[account(
        init_if_needed,
        payer = owner,
        seeds = [STAKE_POSITION_SEED, staking_pool.key().as_ref(), owner.key().as_ref()],
        bump,
        space = 8 + StakePosition::SPACE
    )]
    pub stake_position: Account<'info, StakePosition>,
    /// CHECK: Validated as owner's BASKET token account.
    #[account(mut)]
    pub owner_basket_token_account: UncheckedAccount<'info>,
    #[account(mut)]
    pub stake_vault: Account<'info, TokenAccount>,
    pub token_program: Program<'info, Token>,
    pub system_program: Program<'info, System>,
}

impl<'info> StakeBasket<'info> {
    pub fn handle(mut ctx: Context<Self>, args: StakeBasketArgs) -> Result<()> {
        require!(args.amount > 0, BasketError::InvalidStakeAmount);
        require_keys_eq!(
            ctx.accounts.staking_pool.basket_mint,
            BASKET_MINT,
            BasketError::InvalidBasketMint
        );
        validate_staking_vault(
            &ctx.accounts.stake_vault,
            &ctx.accounts.stake_vault.key(),
            &ctx.accounts.staking_authority.key(),
            &BASKET_MINT,
        )?;

        let owner_token_account =
            load_user_token_account(&ctx.accounts.owner_basket_token_account.to_account_info())?;
        validate_user_token_account(
            &owner_token_account,
            &ctx.accounts.owner.key(),
            &BASKET_MINT,
        )?;

        initialize_or_validate_position(&mut ctx)?;
        settle_stake_position(&ctx.accounts.staking_pool, &mut ctx.accounts.stake_position)?;

        token::transfer(
            CpiContext::new(
                ctx.accounts.token_program.to_account_info(),
                Transfer {
                    from: ctx.accounts.owner_basket_token_account.to_account_info(),
                    to: ctx.accounts.stake_vault.to_account_info(),
                    authority: ctx.accounts.owner.to_account_info(),
                },
            ),
            args.amount,
        )?;

        ctx.accounts.stake_position.amount_staked = ctx
            .accounts
            .stake_position
            .amount_staked
            .checked_add(args.amount)
            .ok_or_else(|| error!(BasketError::ArithmeticOverflow))?;
        ctx.accounts.staking_pool.total_staked = ctx
            .accounts
            .staking_pool
            .total_staked
            .checked_add(args.amount)
            .ok_or_else(|| error!(BasketError::ArithmeticOverflow))?;

        emit!(BasketStaked {
            owner: ctx.accounts.owner.key(),
            amount: args.amount,
            total_staked: ctx.accounts.staking_pool.total_staked,
        });

        Ok(())
    }
}

#[derive(Accounts)]
pub struct UnstakeBasket<'info> {
    #[account(mut)]
    pub owner: Signer<'info>,
    #[account(mut, seeds = [STAKING_POOL_SEED], bump = staking_pool.bump)]
    pub staking_pool: Account<'info, StakingPool>,
    /// CHECK: PDA authority over staking vaults.
    #[account(seeds = [STAKING_AUTHORITY_SEED], bump = staking_pool.staking_authority_bump)]
    pub staking_authority: UncheckedAccount<'info>,
    #[account(
        mut,
        seeds = [STAKE_POSITION_SEED, staking_pool.key().as_ref(), owner.key().as_ref()],
        bump = stake_position.bump
    )]
    pub stake_position: Account<'info, StakePosition>,
    /// CHECK: Validated as owner's BASKET token account.
    #[account(mut)]
    pub owner_basket_token_account: UncheckedAccount<'info>,
    #[account(mut)]
    pub stake_vault: Account<'info, TokenAccount>,
    pub token_program: Program<'info, Token>,
}

impl<'info> UnstakeBasket<'info> {
    pub fn handle(ctx: Context<Self>, args: UnstakeBasketArgs) -> Result<()> {
        require!(args.amount > 0, BasketError::InvalidStakeAmount);
        validate_position(
            &ctx.accounts.stake_position,
            &ctx.accounts.staking_pool.key(),
            &ctx.accounts.owner.key(),
        )?;
        require!(
            ctx.accounts.stake_position.amount_staked >= args.amount,
            BasketError::InsufficientStakedAmount
        );
        validate_staking_vault(
            &ctx.accounts.stake_vault,
            &ctx.accounts.stake_vault.key(),
            &ctx.accounts.staking_authority.key(),
            &BASKET_MINT,
        )?;
        let owner_token_account =
            load_user_token_account(&ctx.accounts.owner_basket_token_account.to_account_info())?;
        validate_user_token_account(
            &owner_token_account,
            &ctx.accounts.owner.key(),
            &BASKET_MINT,
        )?;

        settle_stake_position(&ctx.accounts.staking_pool, &mut ctx.accounts.stake_position)?;

        ctx.accounts.stake_position.amount_staked = ctx
            .accounts
            .stake_position
            .amount_staked
            .checked_sub(args.amount)
            .ok_or_else(|| error!(BasketError::InsufficientStakedAmount))?;
        ctx.accounts.staking_pool.total_staked = ctx
            .accounts
            .staking_pool
            .total_staked
            .checked_sub(args.amount)
            .ok_or_else(|| error!(BasketError::ArithmeticOverflow))?;

        let signer_seeds: &[&[u8]] = &[
            STAKING_AUTHORITY_SEED,
            &[ctx.accounts.staking_pool.staking_authority_bump],
        ];
        token::transfer(
            CpiContext::new_with_signer(
                ctx.accounts.token_program.to_account_info(),
                Transfer {
                    from: ctx.accounts.stake_vault.to_account_info(),
                    to: ctx.accounts.owner_basket_token_account.to_account_info(),
                    authority: ctx.accounts.staking_authority.to_account_info(),
                },
                &[signer_seeds],
            ),
            args.amount,
        )?;

        emit!(BasketUnstaked {
            owner: ctx.accounts.owner.key(),
            amount: args.amount,
            total_staked: ctx.accounts.staking_pool.total_staked,
        });

        Ok(())
    }
}

#[derive(Accounts)]
pub struct ClaimStakingRewards<'info> {
    #[account(mut)]
    pub owner: Signer<'info>,
    #[account(mut, seeds = [STAKING_POOL_SEED], bump = staking_pool.bump)]
    pub staking_pool: Account<'info, StakingPool>,
    /// CHECK: PDA authority over staking vaults.
    #[account(seeds = [STAKING_AUTHORITY_SEED], bump = staking_pool.staking_authority_bump)]
    pub staking_authority: UncheckedAccount<'info>,
    #[account(
        mut,
        seeds = [STAKE_POSITION_SEED, staking_pool.key().as_ref(), owner.key().as_ref()],
        bump = stake_position.bump
    )]
    pub stake_position: Account<'info, StakePosition>,
    #[account(mut)]
    pub reward_vault: Account<'info, TokenAccount>,
    /// CHECK: Validated as owner's USDC token account.
    #[account(mut)]
    pub owner_reward_token_account: UncheckedAccount<'info>,
    pub token_program: Program<'info, Token>,
}

impl<'info> ClaimStakingRewards<'info> {
    pub fn handle(ctx: Context<Self>) -> Result<()> {
        validate_position(
            &ctx.accounts.stake_position,
            &ctx.accounts.staking_pool.key(),
            &ctx.accounts.owner.key(),
        )?;
        validate_staking_vault(
            &ctx.accounts.reward_vault,
            &ctx.accounts.reward_vault.key(),
            &ctx.accounts.staking_authority.key(),
            &USDC_MINT,
        )?;
        let owner_reward_account =
            load_user_token_account(&ctx.accounts.owner_reward_token_account.to_account_info())?;
        validate_user_token_account(&owner_reward_account, &ctx.accounts.owner.key(), &USDC_MINT)?;

        settle_stake_position(&ctx.accounts.staking_pool, &mut ctx.accounts.stake_position)?;
        let amount = ctx.accounts.stake_position.pending_rewards;
        require!(amount > 0, BasketError::NoRewardsToClaim);
        ctx.accounts.stake_position.pending_rewards = 0;

        let signer_seeds: &[&[u8]] = &[
            STAKING_AUTHORITY_SEED,
            &[ctx.accounts.staking_pool.staking_authority_bump],
        ];
        token::transfer(
            CpiContext::new_with_signer(
                ctx.accounts.token_program.to_account_info(),
                Transfer {
                    from: ctx.accounts.reward_vault.to_account_info(),
                    to: ctx.accounts.owner_reward_token_account.to_account_info(),
                    authority: ctx.accounts.staking_authority.to_account_info(),
                },
                &[signer_seeds],
            ),
            amount,
        )?;

        emit!(StakingRewardsClaimed {
            owner: ctx.accounts.owner.key(),
            amount,
        });

        Ok(())
    }
}

fn initialize_or_validate_position(ctx: &mut Context<StakeBasket>) -> Result<()> {
    let position = &mut ctx.accounts.stake_position;
    if position.owner == Pubkey::default() {
        position.owner = ctx.accounts.owner.key();
        position.staking_pool = ctx.accounts.staking_pool.key();
        position.amount_staked = 0;
        position.pending_rewards = 0;
        position.reward_per_token_checkpoint =
            ctx.accounts.staking_pool.reward_per_token_accumulator;
        position.bump = ctx.bumps.stake_position;
        position.pending_rewards_scaled = 0;
        position.reserved = [0; 15];
        return Ok(());
    }

    validate_position(
        position,
        &ctx.accounts.staking_pool.key(),
        &ctx.accounts.owner.key(),
    )
}

fn validate_position(position: &StakePosition, pool: &Pubkey, owner: &Pubkey) -> Result<()> {
    require_keys_eq!(position.owner, *owner, BasketError::InvalidStakePosition);
    require_keys_eq!(
        position.staking_pool,
        *pool,
        BasketError::InvalidStakePosition
    );
    Ok(())
}
