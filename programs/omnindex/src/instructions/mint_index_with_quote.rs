use anchor_lang::prelude::*;
use anchor_spl::token::{self, MintTo, Token, TokenAccount, Transfer};

use crate::{
    constants::{STAKING_AUTHORITY_SEED, STAKING_POOL_SEED, USDC_MINT, VAULT_AUTHORITY_SEED},
    errors::OmnindexError,
    events::{IndexMinted, StakingRewardsAccrued},
    state::{IndexComponent, IndexState, StakingPool},
    utils::{
        accrue_staking_rewards, associated_token_address, basis_points_amount,
        create_associated_token_account_idempotent, load_futarchy_authority, load_mint, load_pair,
        load_rate_model, load_user_token_account, mint_component_backing_amount,
        omnipair_event_authority_address, omnipair_futarchy_authority_address,
        quote_exact_input_for_pair_output, reserve_vault_address, swap, swap_fee_to_usdc,
        validate_pending_component_targets_integral, validate_staking_vault,
        validate_user_token_account, validate_vault_token_account, FeeConversionSwap,
        FutarchyAuthority, SwapAccounts, SwapArgs, ASSOCIATED_TOKEN_ID, OMNIPAIR_ID, TOKEN_2022_ID,
    },
};

#[derive(AnchorSerialize, AnchorDeserialize, Clone, Debug)]
pub struct MintIndexWithQuoteArgs {
    pub index_amount_out: u64,
    pub max_quote_in: u64,
}

#[derive(Accounts)]
pub struct MintIndexWithQuote<'info> {
    #[account(mut)]
    pub user: Signer<'info>,
    #[account(mut, has_one = index_mint @ OmnindexError::IndexMintMismatch)]
    pub index: Account<'info, IndexState>,
    #[account(mut)]
    /// CHECK: Validated as the configured classic SPL Token mint in the handler.
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
    /// CHECK: Created and validated as the user's index ATA before minting.
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

#[derive(Clone, Copy)]
struct ComponentDepositPlan {
    quote_mint: Pubkey,
    index_amount_out: u64,
    base_units: u64,
    current_supply: u64,
    total_quote_spent: u64,
    max_quote_in: u64,
}

impl<'info> MintIndexWithQuote<'info> {
    pub fn handle(
        mut ctx: Context<'_, '_, 'info, 'info, Self>,
        args: MintIndexWithQuoteArgs,
    ) -> Result<()> {
        require!(args.index_amount_out > 0, OmnindexError::InvalidIndexAmount);
        require!(
            *ctx.accounts.quote_mint.to_account_info().owner == ctx.accounts.token_program.key(),
            OmnindexError::InvalidQuoteMint
        );
        require_keys_eq!(
            ctx.accounts.omnipair_futarchy_authority.key(),
            omnipair_futarchy_authority_address(),
            OmnindexError::InvalidOmnipairFutarchyAuthority
        );
        require_keys_eq!(
            ctx.accounts.omnipair_event_authority.key(),
            omnipair_event_authority_address(),
            OmnindexError::InvalidOmnipairEventAuthority
        );
        let futarchy_authority =
            load_futarchy_authority(&ctx.accounts.omnipair_futarchy_authority.to_account_info())?;

        let index = &ctx.accounts.index;
        let index_mint = load_mint(&ctx.accounts.index_mint.to_account_info())?;
        require!(!index.minting_paused, OmnindexError::MintingPaused);
        let current_supply = index_mint.supply;
        let post_supply = current_supply
            .checked_add(args.index_amount_out)
            .ok_or_else(|| error!(OmnindexError::ArithmeticOverflow))?;
        if index.max_supply > 0 {
            require!(
                post_supply <= index.max_supply,
                OmnindexError::SupplyCapExceeded
            );
        }
        validate_pending_component_targets_integral(index, post_supply)?;

        let base_units = index.index_base_units()?;
        let quote_mint = ctx.accounts.quote_mint.key();
        validate_staking_vault(
            &ctx.accounts.staking_reward_vault,
            &ctx.accounts.staking_reward_vault.key(),
            &ctx.accounts.staking_authority.key(),
            &USDC_MINT,
        )?;
        let mut remaining = ctx.remaining_accounts.iter();
        let mut total_quote_spent = 0u64;

        for component in &index.components {
            total_quote_spent = self::process_component_deposit(
                &ctx,
                &mut remaining,
                component,
                &futarchy_authority,
                ComponentDepositPlan {
                    quote_mint,
                    index_amount_out: args.index_amount_out,
                    base_units,
                    current_supply,
                    total_quote_spent,
                    max_quote_in: args.max_quote_in,
                },
            )?;
        }

        self::collect_mint_fee(
            &mut ctx,
            &mut remaining,
            &futarchy_authority,
            total_quote_spent,
            args.max_quote_in,
        )?;
        require!(
            remaining.next().is_none(),
            OmnindexError::InvalidRemainingAccounts
        );

        self::mint_index_tokens(&ctx, args.index_amount_out)?;

        emit!(IndexMinted {
            index: ctx.accounts.index.key(),
            depositor: ctx.accounts.user.key(),
            amount: args.index_amount_out,
        });

        Ok(())
    }
}

#[inline(never)]
fn process_component_deposit<'info>(
    ctx: &Context<'_, '_, 'info, 'info, MintIndexWithQuote<'info>>,
    remaining: &mut std::slice::Iter<'info, AccountInfo<'info>>,
    component: &IndexComponent,
    futarchy_authority: &FutarchyAuthority,
    plan: ComponentDepositPlan,
) -> Result<u64> {
    let component_mint_info = next_account_info(remaining)?;
    let user_component_info = next_account_info(remaining)?;
    let vault_info = next_account_info(remaining)?;

    require_keys_eq!(
        component_mint_info.key(),
        component.mint,
        OmnindexError::InvalidComponentMint
    );

    self::create_component_vault_if_needed(ctx, component_mint_info, vault_info)?;

    let vault_token_account = Account::<TokenAccount>::try_from(vault_info)?;
    validate_vault_token_account(
        &vault_token_account,
        &vault_info.key(),
        &ctx.accounts.vault_authority.key(),
        &component.mint,
    )?;
    let component_amount = mint_component_backing_amount(
        component,
        plan.index_amount_out,
        plan.base_units,
        plan.current_supply,
        vault_token_account.amount,
    )?;
    let deposit_amount = component_amount;
    if deposit_amount == 0 {
        return Ok(plan.total_quote_spent);
    }

    if component.mint == plan.quote_mint {
        require_keys_eq!(
            user_component_info.key(),
            ctx.accounts.user_quote_token_account.key(),
            OmnindexError::InvalidUserComponentTokenAccount
        );
        let user_quote_token_account =
            load_user_token_account(&ctx.accounts.user_quote_token_account.to_account_info())?;
        validate_user_token_account(
            &user_quote_token_account,
            &ctx.accounts.user.key(),
            &plan.quote_mint,
        )?;

        let new_total_quote_spent =
            require_quote_budget(plan.total_quote_spent, deposit_amount, plan.max_quote_in)?;

        token::transfer(
            CpiContext::new(
                ctx.accounts.token_program.to_account_info(),
                Transfer {
                    from: ctx.accounts.user_quote_token_account.to_account_info(),
                    to: vault_info.clone(),
                    authority: ctx.accounts.user.to_account_info(),
                },
            ),
            deposit_amount,
        )?;

        return Ok(new_total_quote_spent);
    }

    self::create_user_component_ata_if_needed(ctx, component_mint_info, user_component_info)?;

    let user_component_token_account = load_user_token_account(user_component_info)?;
    validate_user_token_account(
        &user_component_token_account,
        &ctx.accounts.user.key(),
        &component.mint,
    )?;

    let pair_info = next_account_info(remaining)?;
    let rate_model_info = next_account_info(remaining)?;
    let quote_vault_info = next_account_info(remaining)?;
    let component_vault_info = next_account_info(remaining)?;

    let pair = load_pair(pair_info)?;
    let rate_model = load_rate_model(rate_model_info)?;

    require_keys_eq!(
        rate_model_info.key(),
        pair.rate_model,
        OmnindexError::InvalidOmnipairRateModel
    );
    require!(
        (pair.token0 == plan.quote_mint && pair.token1 == component.mint)
            || (pair.token1 == plan.quote_mint && pair.token0 == component.mint),
        OmnindexError::InvalidOmnipairPair
    );

    let expected_quote_vault = reserve_vault_address(pair_info.key, &plan.quote_mint);
    let expected_component_vault = reserve_vault_address(pair_info.key, &component.mint);
    require_keys_eq!(
        quote_vault_info.key(),
        expected_quote_vault,
        OmnindexError::InvalidOmnipairVault
    );
    require_keys_eq!(
        component_vault_info.key(),
        expected_component_vault,
        OmnindexError::InvalidOmnipairVault
    );

    let quote_vault_account = Account::<TokenAccount>::try_from(quote_vault_info)?;
    require_keys_eq!(
        quote_vault_account.owner,
        pair_info.key(),
        OmnindexError::InvalidOmnipairVault
    );
    require_keys_eq!(
        quote_vault_account.mint,
        plan.quote_mint,
        OmnindexError::InvalidOmnipairVault
    );

    let component_vault_account = Account::<TokenAccount>::try_from(component_vault_info)?;
    require_keys_eq!(
        component_vault_account.owner,
        pair_info.key(),
        OmnindexError::InvalidOmnipairVault
    );
    require_keys_eq!(
        component_vault_account.mint,
        component.mint,
        OmnindexError::InvalidOmnipairVault
    );

    let quote_input = quote_exact_input_for_pair_output(
        &pair,
        &rate_model,
        futarchy_authority,
        &plan.quote_mint,
        &component.mint,
        deposit_amount,
    )?;

    let new_total_quote_spent =
        require_quote_budget(plan.total_quote_spent, quote_input, plan.max_quote_in)?;

    swap(
        ctx.accounts.omnipair_program.to_account_info(),
        SwapAccounts {
            pair: pair_info.clone(),
            rate_model: rate_model_info.clone(),
            futarchy_authority: ctx.accounts.omnipair_futarchy_authority.to_account_info(),
            token_in_vault: quote_vault_info.clone(),
            token_out_vault: component_vault_info.clone(),
            user_token_in_account: ctx.accounts.user_quote_token_account.to_account_info(),
            user_token_out_account: user_component_info.clone(),
            token_in_mint: ctx.accounts.quote_mint.to_account_info(),
            token_out_mint: component_mint_info.clone(),
            user: ctx.accounts.user.to_account_info(),
            token_program: ctx.accounts.token_program.to_account_info(),
            token_2022_program: ctx.accounts.token_2022_program.to_account_info(),
            event_authority: ctx.accounts.omnipair_event_authority.to_account_info(),
        },
        SwapArgs {
            amount_in: quote_input,
            min_amount_out: deposit_amount,
        },
        &[],
    )?;

    token::transfer(
        CpiContext::new(
            ctx.accounts.token_program.to_account_info(),
            Transfer {
                from: user_component_info.clone(),
                to: vault_info.clone(),
                authority: ctx.accounts.user.to_account_info(),
            },
        ),
        deposit_amount,
    )?;

    Ok(new_total_quote_spent)
}

#[inline(never)]
fn collect_mint_fee<'info>(
    ctx: &mut Context<'_, '_, 'info, 'info, MintIndexWithQuote<'info>>,
    remaining: &mut std::slice::Iter<'info, AccountInfo<'info>>,
    futarchy_authority: &FutarchyAuthority,
    total_quote_spent: u64,
    max_quote_in: u64,
) -> Result<()> {
    let fee_amount = basis_points_amount(total_quote_spent, ctx.accounts.index.mint_fee_bps)?;
    if fee_amount == 0 {
        return Ok(());
    }

    require_keys_eq!(
        ctx.accounts.staking_pool.reward_mint,
        USDC_MINT,
        OmnindexError::InvalidRewardMint
    );
    let _ = require_quote_budget(total_quote_spent, fee_amount, max_quote_in)?;

    let quote_mint = ctx.accounts.quote_mint.key();
    let usdc_rewards = if quote_mint == USDC_MINT {
        token::transfer(
            CpiContext::new(
                ctx.accounts.token_program.to_account_info(),
                Transfer {
                    from: ctx.accounts.user_quote_token_account.to_account_info(),
                    to: ctx.accounts.staking_reward_vault.to_account_info(),
                    authority: ctx.accounts.user.to_account_info(),
                },
            ),
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
                funding_token_account: ctx.accounts.user_quote_token_account.to_account_info(),
                funding_authority: ctx.accounts.user.to_account_info(),
                staking_authority: ctx.accounts.staking_authority.to_account_info(),
                staking_reward_vault: ctx.accounts.staking_reward_vault.to_account_info(),
                token_program: ctx.accounts.token_program.to_account_info(),
                token_2022_program: ctx.accounts.token_2022_program.to_account_info(),
            },
            &[],
            &[staking_authority_seeds],
        )?
    };

    accrue_staking_rewards(&mut ctx.accounts.staking_pool, usdc_rewards)?;

    emit!(StakingRewardsAccrued {
        source: ctx.accounts.index.key(),
        amount: usdc_rewards,
        total_staked: ctx.accounts.staking_pool.total_staked,
    });

    Ok(())
}

#[inline(never)]
fn require_quote_budget(
    total_quote_spent: u64,
    quote_input: u64,
    max_quote_in: u64,
) -> Result<u64> {
    let new_total_quote_spent = total_quote_spent
        .checked_add(quote_input)
        .ok_or_else(|| error!(OmnindexError::ArithmeticOverflow))?;
    require!(
        new_total_quote_spent <= max_quote_in,
        OmnindexError::QuoteBudgetExceeded
    );
    Ok(new_total_quote_spent)
}

#[inline(never)]
fn mint_index_tokens<'info>(
    ctx: &Context<'_, '_, 'info, 'info, MintIndexWithQuote<'info>>,
    amount: u64,
) -> Result<()> {
    create_user_index_ata_if_needed(ctx)?;

    let signer_seeds: &[&[u8]] = &[
        VAULT_AUTHORITY_SEED,
        ctx.accounts.index.to_account_info().key.as_ref(),
        &[ctx.accounts.index.vault_authority_bump],
    ];

    token::mint_to(
        CpiContext::new_with_signer(
            ctx.accounts.token_program.to_account_info(),
            MintTo {
                mint: ctx.accounts.index_mint.to_account_info(),
                to: ctx.accounts.user_index_token_account.to_account_info(),
                authority: ctx.accounts.vault_authority.to_account_info(),
            },
            &[signer_seeds],
        ),
        amount,
    )
}

fn create_component_vault_if_needed<'info>(
    ctx: &Context<'_, '_, 'info, 'info, MintIndexWithQuote<'info>>,
    component_mint_info: &AccountInfo<'info>,
    vault_info: &AccountInfo<'info>,
) -> Result<()> {
    let expected_vault = associated_token_address(
        &ctx.accounts.vault_authority.key(),
        &component_mint_info.key(),
    );
    require_keys_eq!(
        vault_info.key(),
        expected_vault,
        OmnindexError::InvalidVaultAccount
    );

    create_associated_token_account_idempotent(
        ctx.accounts.associated_token_program.to_account_info(),
        ctx.accounts.user.to_account_info(),
        vault_info.clone(),
        ctx.accounts.vault_authority.to_account_info(),
        component_mint_info.clone(),
        ctx.accounts.system_program.to_account_info(),
        ctx.accounts.token_program.to_account_info(),
    )?;

    Ok(())
}

fn create_user_component_ata_if_needed<'info>(
    ctx: &Context<'_, '_, 'info, 'info, MintIndexWithQuote<'info>>,
    component_mint_info: &AccountInfo<'info>,
    user_component_info: &AccountInfo<'info>,
) -> Result<()> {
    let expected_ata =
        associated_token_address(&ctx.accounts.user.key(), &component_mint_info.key());
    require_keys_eq!(
        user_component_info.key(),
        expected_ata,
        OmnindexError::InvalidUserComponentTokenAccount
    );

    create_associated_token_account_idempotent(
        ctx.accounts.associated_token_program.to_account_info(),
        ctx.accounts.user.to_account_info(),
        user_component_info.clone(),
        ctx.accounts.user.to_account_info(),
        component_mint_info.clone(),
        ctx.accounts.system_program.to_account_info(),
        ctx.accounts.token_program.to_account_info(),
    )?;

    Ok(())
}

fn create_user_index_ata_if_needed<'info>(
    ctx: &Context<'_, '_, 'info, 'info, MintIndexWithQuote<'info>>,
) -> Result<()> {
    let expected_ata =
        associated_token_address(&ctx.accounts.user.key(), &ctx.accounts.index_mint.key());
    require_keys_eq!(
        ctx.accounts.user_index_token_account.key(),
        expected_ata,
        OmnindexError::InvalidUserTokenAccount
    );

    create_associated_token_account_idempotent(
        ctx.accounts.associated_token_program.to_account_info(),
        ctx.accounts.user.to_account_info(),
        ctx.accounts.user_index_token_account.to_account_info(),
        ctx.accounts.user.to_account_info(),
        ctx.accounts.index_mint.to_account_info(),
        ctx.accounts.system_program.to_account_info(),
        ctx.accounts.token_program.to_account_info(),
    )
}
