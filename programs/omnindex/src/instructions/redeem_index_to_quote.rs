use anchor_lang::prelude::*;
use anchor_spl::token::{self, Burn, Token, TokenAccount, Transfer};

use crate::{
    constants::{STAKING_AUTHORITY_SEED, STAKING_POOL_SEED, USDC_MINT, VAULT_AUTHORITY_SEED},
    errors::OmnindexError,
    events::{IndexRedeemed, StakingRewardsAccrued},
    state::{IndexState, StakingPool},
    utils::{
        accrue_staking_rewards, associated_token_address, basis_points_amount,
        create_associated_token_account_idempotent, load_futarchy_authority, load_mint, load_pair,
        load_rate_model, load_user_token_account, omnipair_event_authority_address,
        omnipair_futarchy_authority_address, quote_exact_output_for_pair_input,
        redeem_component_backing_amount, reserve_vault_address, swap, swap_fee_to_usdc,
        validate_pending_component_targets_integral, validate_staking_vault,
        validate_user_token_account, validate_vault_token_account, FeeConversionSwap,
        FutarchyAuthority, SwapAccounts, SwapArgs, ASSOCIATED_TOKEN_ID, OMNIPAIR_ID, TOKEN_2022_ID,
    },
};

#[derive(AnchorSerialize, AnchorDeserialize, Clone, Debug)]
pub struct RedeemIndexToQuoteArgs {
    pub index_amount_in: u64,
    pub min_quote_out: u64,
}

#[derive(Accounts)]
pub struct RedeemIndexToQuote<'info> {
    #[account(mut)]
    pub user: Signer<'info>,
    #[account(has_one = index_mint @ OmnindexError::IndexMintMismatch)]
    pub index: Account<'info, IndexState>,
    #[account(mut)]
    /// CHECK: Validated as the configured classic SPL Token mint before burning.
    pub index_mint: UncheckedAccount<'info>,
    /// CHECK: PDA authority over the index mint and component vaults.
    #[account(
        seeds = [VAULT_AUTHORITY_SEED, index.key().as_ref()],
        bump = index.vault_authority_bump
    )]
    pub vault_authority: UncheckedAccount<'info>,
    /// CHECK: Validated as a classic SPL Token mint in the handler.
    pub quote_mint: UncheckedAccount<'info>,
    /// CHECK: Validated as the user's quote token account before use.
    #[account(mut)]
    pub user_quote_token_account: UncheckedAccount<'info>,
    /// CHECK: Created and validated as the vault authority's quote ATA before use.
    #[account(mut)]
    pub vault_quote_token_account: UncheckedAccount<'info>,
    /// CHECK: Validated as the user's index token account before burning.
    #[account(mut)]
    pub user_index_token_account: UncheckedAccount<'info>,
    /// CHECK: Validated against the Omnipair program id.
    #[account(address = OMNIPAIR_ID @ OmnindexError::InvalidOmnipairProgram)]
    pub omnipair_program: UncheckedAccount<'info>,
    /// CHECK: Validated as the Omnipair futarchy authority account.
    pub omnipair_futarchy_authority: UncheckedAccount<'info>,
    /// CHECK: Validated as the Omnipair event authority account.
    pub omnipair_event_authority: UncheckedAccount<'info>,
    /// CHECK: Validated as the Associated Token Program.
    #[account(address = ASSOCIATED_TOKEN_ID @ OmnindexError::InvalidAssociatedTokenProgram)]
    pub associated_token_program: UncheckedAccount<'info>,
    #[account(mut, seeds = [STAKING_POOL_SEED], bump = staking_pool.bump)]
    pub staking_pool: Account<'info, StakingPool>,
    /// CHECK: PDA authority over staking vaults.
    #[account(seeds = [STAKING_AUTHORITY_SEED], bump = staking_pool.staking_authority_bump)]
    pub staking_authority: UncheckedAccount<'info>,
    #[account(mut)]
    pub staking_reward_vault: Account<'info, TokenAccount>,
    pub token_program: Program<'info, Token>,
    /// CHECK: Omnipair's swap ABI requires this program account even when routes use classic SPL Token.
    #[account(address = TOKEN_2022_ID @ OmnindexError::InvalidOmnipairTokenProgram)]
    pub token_2022_program: UncheckedAccount<'info>,
    pub system_program: Program<'info, System>,
}

impl<'info> RedeemIndexToQuote<'info> {
    pub fn handle(
        mut ctx: Context<'_, '_, 'info, 'info, Self>,
        args: RedeemIndexToQuoteArgs,
    ) -> Result<()> {
        require!(args.index_amount_in > 0, OmnindexError::InvalidIndexAmount);
        require!(
            !ctx.accounts.index.redeeming_paused,
            OmnindexError::RedeemingPaused
        );
        require!(
            *ctx.accounts.quote_mint.to_account_info().owner == ctx.accounts.token_program.key(),
            OmnindexError::InvalidQuoteMint
        );
        let index_mint = load_mint(&ctx.accounts.index_mint.to_account_info())?;
        let _quote_mint = load_mint(&ctx.accounts.quote_mint.to_account_info())?;
        let current_supply = index_mint.supply;
        let post_supply = current_supply
            .checked_sub(args.index_amount_in)
            .ok_or_else(|| error!(OmnindexError::InvalidIndexAmount))?;
        validate_pending_component_targets_integral(&ctx.accounts.index, post_supply)?;
        require_keys_eq!(
            ctx.accounts.omnipair_event_authority.key(),
            omnipair_event_authority_address(),
            OmnindexError::InvalidOmnipairEventAuthority
        );
        let user_index_token_account =
            load_user_token_account(&ctx.accounts.user_index_token_account.to_account_info())?;
        validate_user_token_account(
            &user_index_token_account,
            &ctx.accounts.user.key(),
            &ctx.accounts.index_mint.key(),
        )?;
        let user_quote_token_account =
            load_user_token_account(&ctx.accounts.user_quote_token_account.to_account_info())?;
        validate_user_token_account(
            &user_quote_token_account,
            &ctx.accounts.user.key(),
            &ctx.accounts.quote_mint.key(),
        )?;
        require_keys_eq!(
            ctx.accounts.omnipair_futarchy_authority.key(),
            omnipair_futarchy_authority_address(),
            OmnindexError::InvalidOmnipairFutarchyAuthority
        );
        let futarchy_authority =
            load_futarchy_authority(&ctx.accounts.omnipair_futarchy_authority.to_account_info())?;

        let index = &ctx.accounts.index;
        let quote_mint = ctx.accounts.quote_mint.key();
        validate_staking_vault(
            &ctx.accounts.staking_reward_vault,
            &ctx.accounts.staking_reward_vault.key(),
            &ctx.accounts.staking_authority.key(),
            &USDC_MINT,
        )?;
        create_vault_quote_ata_if_needed(&ctx)?;
        let signer_seeds: &[&[u8]] = &[
            VAULT_AUTHORITY_SEED,
            ctx.accounts.index.to_account_info().key.as_ref(),
            &[ctx.accounts.index.vault_authority_bump],
        ];
        let mut remaining = ctx.remaining_accounts.iter();
        let mut total_quote_out = 0u64;

        token::burn(
            CpiContext::new(
                ctx.accounts.token_program.to_account_info(),
                Burn {
                    mint: ctx.accounts.index_mint.to_account_info(),
                    from: ctx.accounts.user_index_token_account.to_account_info(),
                    authority: ctx.accounts.user.to_account_info(),
                },
            ),
            args.index_amount_in,
        )?;

        for component in &index.components {
            let component_mint_info = next_account_info(&mut remaining)?;
            let component_vault_info = next_account_info(&mut remaining)?;

            require_keys_eq!(
                component_mint_info.key(),
                component.mint,
                OmnindexError::InvalidComponentMint
            );

            let component_vault_account = Account::<TokenAccount>::try_from(component_vault_info)?;
            validate_vault_token_account(
                &component_vault_account,
                &component_vault_info.key(),
                &ctx.accounts.vault_authority.key(),
                &component.mint,
            )?;

            let backing_amount = redeem_component_backing_amount(
                args.index_amount_in,
                current_supply,
                component_vault_account.amount,
            )?;
            if backing_amount == 0 {
                continue;
            }

            if component.mint == quote_mint {
                require_keys_eq!(
                    component_vault_info.key(),
                    ctx.accounts.vault_quote_token_account.key(),
                    OmnindexError::InvalidVaultAccount
                );
                total_quote_out = total_quote_out
                    .checked_add(backing_amount)
                    .ok_or_else(|| error!(OmnindexError::ArithmeticOverflow))?;
                continue;
            }

            let pair_info = next_account_info(&mut remaining)?;
            let rate_model_info = next_account_info(&mut remaining)?;
            let component_reserve_vault_info = next_account_info(&mut remaining)?;
            let quote_reserve_vault_info = next_account_info(&mut remaining)?;

            let pair = load_pair(pair_info)?;
            let rate_model = load_rate_model(rate_model_info)?;

            require_keys_eq!(
                rate_model_info.key(),
                pair.rate_model,
                OmnindexError::InvalidOmnipairRateModel
            );
            require!(
                (pair.token0 == quote_mint && pair.token1 == component.mint)
                    || (pair.token1 == quote_mint && pair.token0 == component.mint),
                OmnindexError::InvalidOmnipairPair
            );

            let expected_component_reserve_vault =
                reserve_vault_address(pair_info.key, &component.mint);
            let expected_quote_reserve_vault = reserve_vault_address(pair_info.key, &quote_mint);
            require_keys_eq!(
                component_reserve_vault_info.key(),
                expected_component_reserve_vault,
                OmnindexError::InvalidOmnipairVault
            );
            require_keys_eq!(
                quote_reserve_vault_info.key(),
                expected_quote_reserve_vault,
                OmnindexError::InvalidOmnipairVault
            );

            let component_reserve_vault_account =
                Account::<TokenAccount>::try_from(component_reserve_vault_info)?;
            require_keys_eq!(
                component_reserve_vault_account.owner,
                pair_info.key(),
                OmnindexError::InvalidOmnipairVault
            );
            require_keys_eq!(
                component_reserve_vault_account.mint,
                component.mint,
                OmnindexError::InvalidOmnipairVault
            );

            let quote_reserve_vault_account =
                Account::<TokenAccount>::try_from(quote_reserve_vault_info)?;
            require_keys_eq!(
                quote_reserve_vault_account.owner,
                pair_info.key(),
                OmnindexError::InvalidOmnipairVault
            );
            require_keys_eq!(
                quote_reserve_vault_account.mint,
                quote_mint,
                OmnindexError::InvalidOmnipairVault
            );

            let quote_output = quote_exact_output_for_pair_input(
                &pair,
                &rate_model,
                &futarchy_authority,
                &component.mint,
                &quote_mint,
                backing_amount,
            )?;

            swap(
                ctx.accounts.omnipair_program.to_account_info(),
                SwapAccounts {
                    pair: pair_info.clone(),
                    rate_model: rate_model_info.clone(),
                    futarchy_authority: ctx.accounts.omnipair_futarchy_authority.to_account_info(),
                    token_in_vault: component_reserve_vault_info.clone(),
                    token_out_vault: quote_reserve_vault_info.clone(),
                    user_token_in_account: component_vault_info.clone(),
                    user_token_out_account: ctx
                        .accounts
                        .vault_quote_token_account
                        .to_account_info(),
                    token_in_mint: component_mint_info.clone(),
                    token_out_mint: ctx.accounts.quote_mint.to_account_info(),
                    user: ctx.accounts.vault_authority.to_account_info(),
                    token_program: ctx.accounts.token_program.to_account_info(),
                    token_2022_program: ctx.accounts.token_2022_program.to_account_info(),
                    event_authority: ctx.accounts.omnipair_event_authority.to_account_info(),
                },
                SwapArgs {
                    amount_in: backing_amount,
                    min_amount_out: quote_output,
                },
                &[signer_seeds],
            )?;

            total_quote_out = total_quote_out
                .checked_add(quote_output)
                .ok_or_else(|| error!(OmnindexError::ArithmeticOverflow))?;
        }

        let fee_amount = self::collect_redeem_fee(
            &mut ctx,
            &mut remaining,
            &futarchy_authority,
            total_quote_out,
            signer_seeds,
        )?;
        require!(
            remaining.next().is_none(),
            OmnindexError::InvalidRemainingAccounts
        );
        let net_quote_out = total_quote_out
            .checked_sub(fee_amount)
            .ok_or_else(|| error!(OmnindexError::ArithmeticOverflow))?;
        require!(
            net_quote_out >= args.min_quote_out,
            OmnindexError::QuoteBudgetExceeded
        );
        transfer_from_vault_authority(
            &ctx,
            &ctx.accounts.vault_quote_token_account.to_account_info(),
            &ctx.accounts.user_quote_token_account.to_account_info(),
            signer_seeds,
            net_quote_out,
        )?;

        emit!(IndexRedeemed {
            index: ctx.accounts.index.key(),
            redeemer: ctx.accounts.user.key(),
            amount: args.index_amount_in,
        });

        Ok(())
    }
}

fn create_vault_quote_ata_if_needed<'info>(
    ctx: &Context<'_, '_, 'info, 'info, RedeemIndexToQuote<'info>>,
) -> Result<()> {
    let expected_ata = associated_token_address(
        &ctx.accounts.vault_authority.key(),
        &ctx.accounts.quote_mint.key(),
    );
    require_keys_eq!(
        ctx.accounts.vault_quote_token_account.key(),
        expected_ata,
        OmnindexError::InvalidVaultAccount
    );

    create_associated_token_account_idempotent(
        ctx.accounts.associated_token_program.to_account_info(),
        ctx.accounts.user.to_account_info(),
        ctx.accounts.vault_quote_token_account.to_account_info(),
        ctx.accounts.vault_authority.to_account_info(),
        ctx.accounts.quote_mint.to_account_info(),
        ctx.accounts.system_program.to_account_info(),
        ctx.accounts.token_program.to_account_info(),
    )
}

fn collect_redeem_fee<'info>(
    ctx: &mut Context<'_, '_, 'info, 'info, RedeemIndexToQuote<'info>>,
    remaining: &mut std::slice::Iter<'info, AccountInfo<'info>>,
    futarchy_authority: &FutarchyAuthority,
    total_quote_out: u64,
    signer_seeds: &[&[u8]],
) -> Result<u64> {
    let fee_amount = basis_points_amount(total_quote_out, ctx.accounts.index.redeem_fee_bps)?;
    if fee_amount == 0 {
        return Ok(0);
    }

    require_keys_eq!(
        ctx.accounts.staking_pool.reward_mint,
        USDC_MINT,
        OmnindexError::InvalidRewardMint
    );

    let quote_mint = ctx.accounts.quote_mint.key();
    let usdc_rewards = if quote_mint == USDC_MINT {
        transfer_from_vault_authority(
            ctx,
            &ctx.accounts.vault_quote_token_account.to_account_info(),
            &ctx.accounts.staking_reward_vault.to_account_info(),
            signer_seeds,
            fee_amount,
        )?;
        fee_amount
    } else {
        let staking_authority_bump = [ctx.accounts.staking_pool.staking_authority_bump];
        let staking_authority_seeds: &[&[u8]] = &[STAKING_AUTHORITY_SEED, &staking_authority_bump];
        swap_fee_to_usdc(
            remaining,
            futarchy_authority,
            &quote_mint,
            fee_amount,
            FeeConversionSwap {
                payer: ctx.accounts.user.to_account_info(),
                associated_token_program: ctx.accounts.associated_token_program.to_account_info(),
                system_program: ctx.accounts.system_program.to_account_info(),
                omnipair_program: ctx.accounts.omnipair_program.to_account_info(),
                omnipair_futarchy_authority: ctx
                    .accounts
                    .omnipair_futarchy_authority
                    .to_account_info(),
                omnipair_event_authority: ctx.accounts.omnipair_event_authority.to_account_info(),
                source_mint_info: ctx.accounts.quote_mint.to_account_info(),
                funding_token_account: ctx.accounts.vault_quote_token_account.to_account_info(),
                funding_authority: ctx.accounts.vault_authority.to_account_info(),
                staking_authority: ctx.accounts.staking_authority.to_account_info(),
                staking_reward_vault: ctx.accounts.staking_reward_vault.to_account_info(),
                token_program: ctx.accounts.token_program.to_account_info(),
                token_2022_program: ctx.accounts.token_2022_program.to_account_info(),
            },
            &[signer_seeds],
            &[staking_authority_seeds],
        )?
    };
    accrue_staking_rewards(&mut ctx.accounts.staking_pool, usdc_rewards)?;

    emit!(StakingRewardsAccrued {
        source: ctx.accounts.index.key(),
        amount: usdc_rewards,
        total_staked: ctx.accounts.staking_pool.total_staked,
    });

    Ok(fee_amount)
}

fn transfer_from_vault_authority<'info>(
    ctx: &Context<'_, '_, 'info, 'info, RedeemIndexToQuote<'info>>,
    from: &AccountInfo<'info>,
    to: &AccountInfo<'info>,
    signer_seeds: &[&[u8]],
    amount: u64,
) -> Result<()> {
    token::transfer(
        CpiContext::new_with_signer(
            ctx.accounts.token_program.to_account_info(),
            Transfer {
                from: from.clone(),
                to: to.clone(),
                authority: ctx.accounts.vault_authority.to_account_info(),
            },
            &[signer_seeds],
        ),
        amount,
    )
}
