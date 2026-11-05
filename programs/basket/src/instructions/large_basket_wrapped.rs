// Mint and redeem legs for a stORE component that go through ORE's staking program instead
// of stORE's thin pools. A mint leg buys ORE with Jupiter and wraps it into stORE; a redeem
// leg unwraps the component's stORE and sells the ORE with Jupiter.
//
// The owner signs every outside call: the Jupiter swap, the wrap and the unwrap all run on
// the owner's own token accounts. The vault authority signs only one transfer, of exactly
// the leg's stORE from the vault to the owner on a redeem, which the redeem was owed anyway.
// On a mint, the vault is credited by its measured balance change, as on the other legs.
use anchor_lang::{
    prelude::*,
    solana_program::{instruction::Instruction, program::invoke},
};
use anchor_spl::{
    token::{Token, TokenAccount},
    token_interface::{self, TokenAccount as InterfaceTokenAccount, TransferChecked},
};

use crate::{
    constants::{
        LARGE_BASKET_INTENT_LOCK_SEED, MAX_UNSOLD_UNWRAPPED_ORE_BPS, ORE_LST_PROGRAM_ID,
        ORE_LST_UNWRAP_DISCRIMINATOR, ORE_LST_WRAP_DISCRIMINATOR, ORE_MINT,
        ORE_STAKE_PROGRAM_ID, STAKING_AUTHORITY_SEED, STAKING_POOL_SEED, STORE_DECIMALS,
        STORE_MINT, USDC_MINT, VAULT_AUTHORITY_SEED,
    },
    errors::BasketError,
    events::{LargeBasketComponentFilled, LargeBasketIntentFinalized},
    instructions::large_basket_intent::{
        add_mint_quote_spent, add_redeem_quote_received, charge_redeem_leg_fees_from,
        clear_intent_lock, component_filled, intent_component_amount, mark_component_filled,
        record_component_received, require_mint_basis, set_component_quote_atoms,
        validate_component_accounts, validate_open_intent, RedeemLegFeeAccounts,
    },
    state::{
        IndexState, LargeBasketComponentPage, LargeBasketIntent, LargeBasketIntentKind,
        LargeBasketIntentLock, LargeBasketIntentStatus, StakingPool,
    },
    utils::{
        associated_token_address, create_associated_token_account_idempotent,
        create_associated_token_account_idempotent_for_token_program,
        invoke_jupiter_swap_with_scratch, load_interface_token_account, unpack_account_metas,
        validate_staking_vault, validate_vault_authority_token_account_scope,
        JupiterInvokeScratch, LargeBasketSwapPlan, ASSOCIATED_TOKEN_ID,
    },
};

#[derive(AnchorSerialize, AnchorDeserialize, Clone, Debug)]
pub struct ExecuteLargeBasketMintWrappedComponentArgs {
    pub component_index: u16,
    pub max_quote_in: u64,
    /// USDC -> ORE, from owner_quote_token_account into owner_ore_token_account. None only
    /// when the leg's amount is zero.
    pub swap: Option<LargeBasketSwapPlan>,
}

#[derive(AnchorSerialize, AnchorDeserialize, Clone, Debug)]
pub struct ExecuteLargeBasketRedeemWrappedComponentArgs {
    pub component_index: u16,
    pub min_quote_out: u64,
    /// ORE -> USDC, from owner_ore_token_account into owner_quote_token_account. None only
    /// when the leg's amount is zero.
    pub swap: Option<LargeBasketSwapPlan>,
}

// The accounts ore-lst's wrap and unwrap take besides the owner's, the mints and the
// programs. ore-lst checks each of them; the basket relies on none of them, since it only
// measures its own vault and the owner's accounts.
#[derive(Accounts)]
pub struct OreLstAccounts<'info> {
    /// CHECK: ore-lst's vault PDA, checked by ore-lst.
    #[account(mut)]
    pub ore_lst_vault: UncheckedAccount<'info>,
    /// CHECK: ore-lst's vault ORE account, checked by ore-lst.
    #[account(mut)]
    pub ore_lst_vault_tokens: UncheckedAccount<'info>,
    /// CHECK: ore-lst's ore-stake position, checked by ore-lst.
    #[account(mut)]
    pub ore_stake: UncheckedAccount<'info>,
    /// CHECK: The stake position's ORE account, checked by ore-lst.
    #[account(mut)]
    pub ore_stake_tokens: UncheckedAccount<'info>,
    /// CHECK: ore-stake's treasury, checked by ore-lst.
    #[account(mut)]
    pub ore_stake_treasury: UncheckedAccount<'info>,
    /// CHECK: The treasury's ORE account, checked by ore-lst.
    #[account(mut)]
    pub ore_stake_treasury_tokens: UncheckedAccount<'info>,
    /// CHECK: ore-stake's vesting account, checked by ore-lst.
    #[account(mut)]
    pub ore_stake_vesting: UncheckedAccount<'info>,
    /// CHECK: ore-lst.
    #[account(address = ORE_LST_PROGRAM_ID @ BasketError::InvalidOreStakingAccount)]
    pub ore_lst_program: UncheckedAccount<'info>,
    /// CHECK: ore-stake.
    #[account(address = ORE_STAKE_PROGRAM_ID @ BasketError::InvalidOreStakingAccount)]
    pub ore_stake_program: UncheckedAccount<'info>,
}

#[derive(Accounts)]
pub struct ExecuteLargeBasketMintWrappedComponent<'info> {
    #[account(mut)]
    pub owner: Signer<'info>,
    #[account(has_one = index_mint @ BasketError::IndexMintMismatch)]
    pub index: Box<Account<'info, IndexState>>,
    /// CHECK: Validated as the configured classic SPL index mint.
    pub index_mint: UncheckedAccount<'info>,
    #[account(mut, has_one = owner @ BasketError::InvalidLargeBasketIntent, has_one = index @ BasketError::InvalidLargeBasketIntent)]
    pub intent: Box<Account<'info, LargeBasketIntent>>,
    /// CHECK: PDA authority over component vaults.
    #[account(
        seeds = [VAULT_AUTHORITY_SEED, index.key().as_ref()],
        bump = index.vault_authority_bump
    )]
    pub vault_authority: UncheckedAccount<'info>,
    /// CHECK: Validated as native Solana USDC.
    #[account(address = USDC_MINT @ BasketError::InvalidQuoteMint)]
    pub quote_mint: UncheckedAccount<'info>,
    #[account(
        mut,
        constraint = owner_quote_token_account.mint == USDC_MINT
            && owner_quote_token_account.owner == owner.key() @ BasketError::InvalidUserTokenAccount
    )]
    pub owner_quote_token_account: Box<InterfaceAccount<'info, InterfaceTokenAccount>>,
    /// CHECK: Validated against known Jupiter program ids.
    pub jupiter_program: UncheckedAccount<'info>,
    pub component_page: Box<Account<'info, LargeBasketComponentPage>>,
    /// CHECK: Validated against the component page and as stORE. ore-lst mints it.
    #[account(mut)]
    pub component_mint: UncheckedAccount<'info>,
    /// CHECK: Validated against the component page.
    #[account(mut)]
    pub component_vault: UncheckedAccount<'info>,
    /// CHECK: Validated as ORE. ore-lst writes it when it claims staking yield.
    #[account(mut, address = ORE_MINT @ BasketError::InvalidOreStakingAccount)]
    pub ore_mint: UncheckedAccount<'info>,
    /// CHECK: Validated as the owner's ORE associated token account.
    #[account(mut)]
    pub owner_ore_token_account: UncheckedAccount<'info>,
    /// CHECK: Validated as the owner's stORE associated token account.
    #[account(mut)]
    pub owner_store_token_account: UncheckedAccount<'info>,
    pub ore_lst: OreLstAccounts<'info>,
    pub token_program: Program<'info, Token>,
    /// CHECK: Validated as the Associated Token Program.
    #[account(address = ASSOCIATED_TOKEN_ID @ BasketError::InvalidAssociatedTokenProgram)]
    pub associated_token_program: UncheckedAccount<'info>,
    pub system_program: Program<'info, System>,
}

// Like the batched redeem, the leg pays its USDC to the owner and charges its share of the
// fee in the same instruction, and the leg that completes the intent finalizes it.
#[derive(Accounts)]
pub struct ExecuteLargeBasketRedeemWrappedComponent<'info> {
    #[account(mut)]
    pub owner: Signer<'info>,
    #[account(mut, has_one = index_mint @ BasketError::IndexMintMismatch)]
    pub index: Box<Account<'info, IndexState>>,
    /// CHECK: Validated as the configured classic SPL index mint.
    pub index_mint: UncheckedAccount<'info>,
    #[account(mut, has_one = owner @ BasketError::InvalidLargeBasketIntent, has_one = index @ BasketError::InvalidLargeBasketIntent)]
    pub intent: Box<Account<'info, LargeBasketIntent>>,
    /// CHECK: PDA authority over component vaults.
    #[account(
        seeds = [VAULT_AUTHORITY_SEED, index.key().as_ref()],
        bump = index.vault_authority_bump
    )]
    pub vault_authority: UncheckedAccount<'info>,
    /// CHECK: Validated as native Solana USDC.
    #[account(address = USDC_MINT @ BasketError::InvalidQuoteMint)]
    pub quote_mint: UncheckedAccount<'info>,
    #[account(
        mut,
        constraint = owner_quote_token_account.mint == USDC_MINT
            && owner_quote_token_account.owner == owner.key() @ BasketError::InvalidUserTokenAccount
    )]
    pub owner_quote_token_account: Box<InterfaceAccount<'info, InterfaceTokenAccount>>,
    /// CHECK: Validated against known Jupiter program ids.
    pub jupiter_program: UncheckedAccount<'info>,
    pub component_page: Box<Account<'info, LargeBasketComponentPage>>,
    /// CHECK: Validated against the component page and as stORE. ore-lst burns it.
    #[account(mut)]
    pub component_mint: UncheckedAccount<'info>,
    /// CHECK: Validated against the component page.
    #[account(mut)]
    pub component_vault: UncheckedAccount<'info>,
    /// CHECK: Validated as ORE. ore-lst writes it when it claims staking yield.
    #[account(mut, address = ORE_MINT @ BasketError::InvalidOreStakingAccount)]
    pub ore_mint: UncheckedAccount<'info>,
    /// CHECK: Validated as the owner's ORE associated token account.
    #[account(mut)]
    pub owner_ore_token_account: UncheckedAccount<'info>,
    /// CHECK: Validated as the owner's stORE associated token account.
    #[account(mut)]
    pub owner_store_token_account: UncheckedAccount<'info>,
    pub ore_lst: OreLstAccounts<'info>,
    pub token_program: Program<'info, Token>,
    /// CHECK: Validated as the Associated Token Program.
    #[account(address = ASSOCIATED_TOKEN_ID @ BasketError::InvalidAssociatedTokenProgram)]
    pub associated_token_program: UncheckedAccount<'info>,
    pub system_program: Program<'info, System>,
    #[account(mut)]
    pub fee_recipient_quote_token_account: Box<Account<'info, TokenAccount>>,
    #[account(mut)]
    pub creator_fee_recipient_quote_token_account: Box<Account<'info, TokenAccount>>,
    #[account(mut, seeds = [STAKING_POOL_SEED], bump = staking_pool.bump)]
    pub staking_pool: Box<Account<'info, StakingPool>>,
    /// CHECK: PDA authority over staking vaults.
    #[account(seeds = [STAKING_AUTHORITY_SEED], bump = staking_pool.staking_authority_bump)]
    pub staking_authority: UncheckedAccount<'info>,
    #[account(mut)]
    pub staking_reward_vault: Box<Account<'info, TokenAccount>>,
    #[account(
        mut,
        seeds = [
            LARGE_BASKET_INTENT_LOCK_SEED,
            index.key().as_ref(),
            owner.key().as_ref(),
        ],
        bump = intent_lock.bump
    )]
    pub intent_lock: Box<Account<'info, LargeBasketIntentLock>>,
}

impl<'info> ExecuteLargeBasketMintWrappedComponent<'info> {
    pub fn handle(
        ctx: Context<'_, '_, 'info, 'info, Self>,
        args: ExecuteLargeBasketMintWrappedComponentArgs,
    ) -> Result<()> {
        validate_open_intent(
            &ctx.accounts.intent,
            LargeBasketIntentKind::Mint,
            args.component_index,
        )?;
        // In-kind mints skip the USDC fee collect, as on the other swap legs.
        require!(
            !ctx.accounts.intent.in_kind,
            BasketError::InvalidLargeBasketIntent
        );
        require_mint_basis(&ctx.accounts.index, &ctx.accounts.index_mint, &ctx.accounts.intent)?;
        require!(
            !component_filled(&ctx.accounts.intent, args.component_index)?,
            BasketError::LargeBasketComponentAlreadyFilled
        );
        let component = validate_component_accounts(
            &ctx.accounts.index.key(),
            &ctx.accounts.component_page,
            &ctx.accounts.component_page.key(),
            args.component_index,
            &ctx.accounts.component_mint.to_account_info(),
            &ctx.accounts.component_vault.to_account_info(),
            &ctx.accounts.token_program.to_account_info(),
        )?;
        require_keys_eq!(
            component.mint,
            STORE_MINT,
            BasketError::WrappedComponentNotStore
        );
        let owner = ctx.accounts.owner.key();
        require_owner_atas(
            &owner,
            &ctx.accounts.owner_ore_token_account,
            &ctx.accounts.owner_store_token_account,
        )?;
        create_associated_token_account_idempotent_for_token_program(
            ctx.accounts.associated_token_program.to_account_info(),
            ctx.accounts.owner.to_account_info(),
            ctx.accounts.component_vault.to_account_info(),
            ctx.accounts.vault_authority.to_account_info(),
            ctx.accounts.component_mint.to_account_info(),
            ctx.accounts.system_program.to_account_info(),
            ctx.accounts.token_program.to_account_info(),
        )?;

        let amount = intent_component_amount(&ctx.accounts.intent, args.component_index)?;
        let (quote_spent, received) = if amount == 0 {
            require!(args.swap.is_none(), BasketError::InvalidJupiterRoute);
            (0, 0)
        } else {
            let swap = args
                .swap
                .as_ref()
                .ok_or_else(|| error!(BasketError::InvalidJupiterRoute))?;
            mint_wrapped_leg(&ctx, swap)?
        };
        require!(
            quote_spent <= args.max_quote_in,
            BasketError::QuoteBudgetExceeded
        );
        let intent = &mut ctx.accounts.intent;
        add_mint_quote_spent(intent, quote_spent)?;
        set_component_quote_atoms(intent, args.component_index, quote_spent)?;
        record_component_received(intent, args.component_index, received)?;
        mark_component_filled(intent, args.component_index)?;

        emit!(LargeBasketComponentFilled {
            intent: intent.key(),
            index: ctx.accounts.index.key(),
            owner,
            component_index: args.component_index,
            amount: received,
            quote_atoms: quote_spent,
        });
        Ok(())
    }
}

// Buys ORE, wraps it and moves every stORE the wrap minted into the vault. Returns the USDC
// spent and the stORE that reached the vault.
#[inline(never)]
fn mint_wrapped_leg<'info>(
    ctx: &Context<'_, '_, 'info, 'info, ExecuteLargeBasketMintWrappedComponent<'info>>,
    swap: &LargeBasketSwapPlan,
) -> Result<(u64, u64)> {
    let a = &ctx.accounts;
    let owner_quote = a.owner_quote_token_account.to_account_info();
    let owner_ore = a.owner_ore_token_account.to_account_info();
    let owner_store = a.owner_store_token_account.to_account_info();
    let vault = a.component_vault.to_account_info();
    create_owner_ata(
        &a.owner,
        &a.owner_ore_token_account,
        &a.ore_mint,
        &a.associated_token_program,
        &a.system_program,
        &a.token_program,
    )?;

    let quote_before = token_amount(&owner_quote)?;
    let ore_before = token_amount(&owner_ore)?;
    swap_as_owner(
        &a.owner,
        &a.vault_authority,
        [&owner_quote, &a.quote_mint, &owner_ore, &a.ore_mint],
        &a.token_program,
        &a.associated_token_program,
        &a.system_program,
        &a.jupiter_program,
        ctx.remaining_accounts,
        swap,
    )?;
    let quote_spent = quote_before
        .checked_sub(token_amount(&owner_quote)?)
        .ok_or_else(|| error!(BasketError::InvalidJupiterRoute))?;
    require!(quote_spent > 0, BasketError::InvalidJupiterRoute);
    let ore_received = token_amount(&owner_ore)?
        .checked_sub(ore_before)
        .ok_or_else(|| error!(BasketError::InvalidJupiterRoute))?;
    require!(ore_received > 0, BasketError::InvalidJupiterRoute);

    let store_before = token_amount_or_zero(&owner_store)?;
    invoke_ore_lst(
        ORE_LST_WRAP_DISCRIMINATOR,
        ore_received,
        &a.owner,
        &owner_ore,
        &owner_store,
        &a.ore_mint,
        &a.component_mint,
        &a.ore_lst,
        &a.token_program,
        &a.associated_token_program,
        &a.system_program,
    )?;
    let store_minted = token_amount(&owner_store)?
        .checked_sub(store_before)
        .ok_or_else(|| error!(BasketError::WrappedLegMismatch))?;
    require!(store_minted > 0, BasketError::WrappedLegMismatch);
    require!(
        token_amount(&owner_ore)? == ore_before,
        BasketError::WrappedLegMismatch
    );

    let vault_before = token_amount(&vault)?;
    token_interface::transfer_checked(
        CpiContext::new(
            a.token_program.to_account_info(),
            TransferChecked {
                from: owner_store,
                mint: a.component_mint.to_account_info(),
                to: vault.clone(),
                authority: a.owner.to_account_info(),
            },
        ),
        store_minted,
        STORE_DECIMALS,
    )?;
    let received = token_amount(&vault)?
        .checked_sub(vault_before)
        .ok_or_else(|| error!(BasketError::WrappedLegMismatch))?;
    Ok((quote_spent, received))
}

impl<'info> ExecuteLargeBasketRedeemWrappedComponent<'info> {
    pub fn handle(
        ctx: Context<'_, '_, 'info, 'info, Self>,
        args: ExecuteLargeBasketRedeemWrappedComponentArgs,
    ) -> Result<()> {
        require_keys_eq!(
            ctx.accounts.intent.staking_pool,
            ctx.accounts.staking_pool.key(),
            BasketError::InvalidStakingVault
        );
        require_keys_eq!(
            ctx.accounts.staking_pool.reward_mint,
            USDC_MINT,
            BasketError::InvalidRewardMint
        );
        validate_staking_vault(
            &ctx.accounts.staking_reward_vault,
            &ctx.accounts.staking_reward_vault.key(),
            &ctx.accounts.staking_authority.key(),
            &USDC_MINT,
        )?;
        validate_open_intent(
            &ctx.accounts.intent,
            LargeBasketIntentKind::Redeem,
            args.component_index,
        )?;
        // An in-kind redeem takes its fee in kind; this leg pays USDC.
        require!(
            !ctx.accounts.intent.in_kind,
            BasketError::InvalidLargeBasketIntent
        );
        require!(
            !component_filled(&ctx.accounts.intent, args.component_index)?,
            BasketError::LargeBasketComponentAlreadyFilled
        );
        let component = validate_component_accounts(
            &ctx.accounts.index.key(),
            &ctx.accounts.component_page,
            &ctx.accounts.component_page.key(),
            args.component_index,
            &ctx.accounts.component_mint.to_account_info(),
            &ctx.accounts.component_vault.to_account_info(),
            &ctx.accounts.token_program.to_account_info(),
        )?;
        require_keys_eq!(
            component.mint,
            STORE_MINT,
            BasketError::WrappedComponentNotStore
        );
        require_owner_atas(
            &ctx.accounts.owner.key(),
            &ctx.accounts.owner_ore_token_account,
            &ctx.accounts.owner_store_token_account,
        )?;

        let amount = intent_component_amount(&ctx.accounts.intent, args.component_index)?;
        let quote_received = if amount == 0 {
            require!(args.swap.is_none(), BasketError::InvalidJupiterRoute);
            0
        } else {
            let swap = args
                .swap
                .as_ref()
                .ok_or_else(|| error!(BasketError::InvalidJupiterRoute))?;
            redeem_wrapped_leg(&ctx, swap, amount)?
        };
        require!(
            quote_received >= args.min_quote_out,
            BasketError::QuoteBudgetExceeded
        );

        let accounts = &mut *ctx.accounts;
        add_redeem_quote_received(&mut accounts.intent, quote_received)?;
        set_component_quote_atoms(&mut accounts.intent, args.component_index, quote_received)?;
        mark_component_filled(&mut accounts.intent, args.component_index)?;
        let fee_accounts = RedeemLegFeeAccounts {
            owner: accounts.owner.to_account_info(),
            owner_quote_token_account: accounts.owner_quote_token_account.to_account_info(),
            quote_mint: accounts.quote_mint.to_account_info(),
            quote_token_program: accounts.token_program.to_account_info(),
            fee_recipient_quote_token_account: &accounts.fee_recipient_quote_token_account,
            creator_fee_recipient_quote_token_account: &accounts
                .creator_fee_recipient_quote_token_account,
            staking_reward_vault: accounts.staking_reward_vault.to_account_info(),
        };
        charge_redeem_leg_fees_from(
            &mut accounts.intent,
            &mut accounts.staking_pool,
            fee_accounts,
        )?;

        let intent_key = accounts.intent.key();
        emit!(LargeBasketComponentFilled {
            intent: intent_key,
            index: accounts.index.key(),
            owner: accounts.owner.key(),
            component_index: args.component_index,
            amount,
            quote_atoms: quote_received,
        });

        // As in the batched redeem, the leg that completes the intent settles it.
        if accounts.intent.completed_components == accounts.intent.component_count {
            accounts.intent.fees_collected = true;
            accounts.intent.status = LargeBasketIntentStatus::Finalized;
            accounts.index.track_closed_intent();
            clear_intent_lock(&mut accounts.intent_lock, intent_key)?;
            emit!(LargeBasketIntentFinalized {
                intent: intent_key,
                index: accounts.index.key(),
                owner: accounts.intent.owner,
                kind: accounts.intent.kind,
                index_amount: accounts.intent.index_amount,
                quote_atoms_executed: accounts.intent.quote_atoms_executed,
            });
        }
        Ok(())
    }
}

// Pays the leg's stORE to the owner, unwraps exactly that and sells the ORE. Returns the
// USDC the owner received.
#[inline(never)]
fn redeem_wrapped_leg<'info>(
    ctx: &Context<'_, '_, 'info, 'info, ExecuteLargeBasketRedeemWrappedComponent<'info>>,
    swap: &LargeBasketSwapPlan,
    amount: u64,
) -> Result<u64> {
    let a = &ctx.accounts;
    let owner_quote = a.owner_quote_token_account.to_account_info();
    let owner_ore = a.owner_ore_token_account.to_account_info();
    let owner_store = a.owner_store_token_account.to_account_info();
    let vault = a.component_vault.to_account_info();
    for (account, mint) in [
        (&a.owner_store_token_account, &a.component_mint),
        (&a.owner_ore_token_account, &a.ore_mint),
    ] {
        create_owner_ata(
            &a.owner,
            account,
            mint,
            &a.associated_token_program,
            &a.system_program,
            &a.token_program,
        )?;
    }

    let index_key = a.index.key();
    let signer_seeds: &[&[u8]] = &[
        VAULT_AUTHORITY_SEED,
        index_key.as_ref(),
        std::slice::from_ref(&a.index.vault_authority_bump),
    ];
    let vault_before = token_amount(&vault)?;
    token_interface::transfer_checked(
        CpiContext::new_with_signer(
            a.token_program.to_account_info(),
            TransferChecked {
                from: vault.clone(),
                mint: a.component_mint.to_account_info(),
                to: owner_store.clone(),
                authority: a.vault_authority.to_account_info(),
            },
            &[signer_seeds],
        ),
        amount,
        STORE_DECIMALS,
    )?;
    require!(
        vault_before.checked_sub(token_amount(&vault)?) == Some(amount),
        BasketError::WrappedLegMismatch
    );

    let store_before = token_amount(&owner_store)?;
    let ore_before = token_amount(&owner_ore)?;
    let quote_before = token_amount(&owner_quote)?;
    invoke_ore_lst(
        ORE_LST_UNWRAP_DISCRIMINATOR,
        amount,
        &a.owner,
        &owner_ore,
        &owner_store,
        &a.ore_mint,
        &a.component_mint,
        &a.ore_lst,
        &a.token_program,
        &a.associated_token_program,
        &a.system_program,
    )?;
    require!(
        store_before.checked_sub(token_amount(&owner_store)?) == Some(amount),
        BasketError::WrappedLegMismatch
    );
    let ore_unwrapped = token_amount(&owner_ore)?
        .checked_sub(ore_before)
        .ok_or_else(|| error!(BasketError::WrappedLegMismatch))?;
    require!(ore_unwrapped > 0, BasketError::WrappedLegMismatch);

    swap_as_owner(
        &a.owner,
        &a.vault_authority,
        [&owner_quote, &a.quote_mint, &owner_ore, &a.ore_mint],
        &a.token_program,
        &a.associated_token_program,
        &a.system_program,
        &a.jupiter_program,
        ctx.remaining_accounts,
        swap,
    )?;
    // The fee is charged on the USDC this leg pays, so the route has to sell what it unwrapped.
    let unsold = token_amount(&owner_ore)?.saturating_sub(ore_before);
    require!(
        u128::from(unsold) * 10_000
            <= u128::from(ore_unwrapped) * u128::from(MAX_UNSOLD_UNWRAPPED_ORE_BPS),
        BasketError::UnwrappedOreNotSold
    );
    token_amount(&owner_quote)?
        .checked_sub(quote_before)
        .ok_or_else(|| error!(BasketError::InvalidJupiterRoute))
}

// The Jupiter route runs on the owner's USDC and ORE accounts with the owner's signature. It
// is resolved against [owner, owner_quote_token_account, quote_mint, owner_ore_token_account,
// ore_mint, token_program, associated_token_program, system_program, jupiter_program] followed
// by the remaining accounts, and may not touch any token account the vault authority owns.
#[inline(never)]
#[allow(clippy::too_many_arguments)]
fn swap_as_owner<'info>(
    owner: &Signer<'info>,
    vault_authority: &UncheckedAccount<'info>,
    [owner_quote, quote_mint, owner_ore, ore_mint]: [&AccountInfo<'info>; 4],
    token_program: &Program<'info, Token>,
    associated_token_program: &UncheckedAccount<'info>,
    system_program: &Program<'info, System>,
    jupiter_program: &UncheckedAccount<'info>,
    route_accounts: &[AccountInfo<'info>],
    swap: &LargeBasketSwapPlan,
) -> Result<()> {
    let mut candidates: Vec<AccountInfo<'info>> = Vec::with_capacity(9 + route_accounts.len());
    candidates.push(owner.to_account_info());
    candidates.push(owner_quote.clone());
    candidates.push(quote_mint.clone());
    candidates.push(owner_ore.clone());
    candidates.push(ore_mint.clone());
    candidates.push(token_program.to_account_info());
    candidates.push(associated_token_program.to_account_info());
    candidates.push(system_program.to_account_info());
    candidates.push(jupiter_program.to_account_info());
    candidates.extend_from_slice(route_accounts);
    let account_metas = unpack_account_metas(&swap.accounts);
    validate_vault_authority_token_account_scope(
        &candidates,
        &account_metas,
        vault_authority.key(),
        &[],
    )?;
    let mut scratch = JupiterInvokeScratch::new();
    invoke_jupiter_swap_with_scratch(
        jupiter_program.to_account_info(),
        &candidates,
        &account_metas,
        &swap.instruction_data,
        Some(owner.key()),
        &[],
        &mut scratch,
    )
}

// ore-lst's wrap and unwrap: [signer, payer, sender ORE, sender stORE, ORE mint, stORE mint,
// stake, stake tokens, treasury, treasury tokens, vault, vault tokens, vesting, system,
// token, associated token, ore-stake], then the u8 discriminator and the u64 amount. The
// owner signs and pays; no basket account takes part.
#[inline(never)]
#[allow(clippy::too_many_arguments)]
fn invoke_ore_lst<'info>(
    discriminator: u8,
    amount: u64,
    owner: &Signer<'info>,
    owner_ore: &AccountInfo<'info>,
    owner_store: &AccountInfo<'info>,
    ore_mint: &UncheckedAccount<'info>,
    store_mint: &UncheckedAccount<'info>,
    ore_lst: &OreLstAccounts<'info>,
    token_program: &Program<'info, Token>,
    associated_token_program: &UncheckedAccount<'info>,
    system_program: &Program<'info, System>,
) -> Result<()> {
    let mut data = Vec::with_capacity(9);
    data.push(discriminator);
    data.extend_from_slice(&amount.to_le_bytes());
    let owner_key = owner.key();
    let instruction = Instruction {
        program_id: ORE_LST_PROGRAM_ID,
        accounts: vec![
            AccountMeta::new(owner_key, true),
            AccountMeta::new(owner_key, true),
            AccountMeta::new(owner_ore.key(), false),
            AccountMeta::new(owner_store.key(), false),
            AccountMeta::new(ore_mint.key(), false),
            AccountMeta::new(store_mint.key(), false),
            AccountMeta::new(ore_lst.ore_stake.key(), false),
            AccountMeta::new(ore_lst.ore_stake_tokens.key(), false),
            AccountMeta::new(ore_lst.ore_stake_treasury.key(), false),
            AccountMeta::new(ore_lst.ore_stake_treasury_tokens.key(), false),
            AccountMeta::new(ore_lst.ore_lst_vault.key(), false),
            AccountMeta::new(ore_lst.ore_lst_vault_tokens.key(), false),
            AccountMeta::new(ore_lst.ore_stake_vesting.key(), false),
            AccountMeta::new_readonly(system_program.key(), false),
            AccountMeta::new_readonly(token_program.key(), false),
            AccountMeta::new_readonly(associated_token_program.key(), false),
            AccountMeta::new_readonly(ore_lst.ore_stake_program.key(), false),
        ],
        data,
    };
    invoke(
        &instruction,
        &[
            owner.to_account_info(),
            owner_ore.clone(),
            owner_store.clone(),
            ore_mint.to_account_info(),
            store_mint.to_account_info(),
            ore_lst.ore_stake.to_account_info(),
            ore_lst.ore_stake_tokens.to_account_info(),
            ore_lst.ore_stake_treasury.to_account_info(),
            ore_lst.ore_stake_treasury_tokens.to_account_info(),
            ore_lst.ore_lst_vault.to_account_info(),
            ore_lst.ore_lst_vault_tokens.to_account_info(),
            ore_lst.ore_stake_vesting.to_account_info(),
            system_program.to_account_info(),
            token_program.to_account_info(),
            associated_token_program.to_account_info(),
            ore_lst.ore_stake_program.to_account_info(),
            ore_lst.ore_lst_program.to_account_info(),
        ],
    )
    .map_err(Into::into)
}

fn require_owner_atas(
    owner: &Pubkey,
    owner_ore: &UncheckedAccount<'_>,
    owner_store: &UncheckedAccount<'_>,
) -> Result<()> {
    require_keys_eq!(
        owner_ore.key(),
        associated_token_address(owner, &ORE_MINT),
        BasketError::InvalidUserTokenAccount
    );
    require_keys_eq!(
        owner_store.key(),
        associated_token_address(owner, &STORE_MINT),
        BasketError::InvalidUserTokenAccount
    );
    Ok(())
}

fn create_owner_ata<'info>(
    owner: &Signer<'info>,
    account: &UncheckedAccount<'info>,
    mint: &UncheckedAccount<'info>,
    associated_token_program: &UncheckedAccount<'info>,
    system_program: &Program<'info, System>,
    token_program: &Program<'info, Token>,
) -> Result<()> {
    create_associated_token_account_idempotent(
        associated_token_program.to_account_info(),
        owner.to_account_info(),
        account.to_account_info(),
        owner.to_account_info(),
        mint.to_account_info(),
        system_program.to_account_info(),
        token_program.to_account_info(),
    )
}

fn token_amount(info: &AccountInfo<'_>) -> Result<u64> {
    Ok(load_interface_token_account(info)?.amount)
}

// The owner's stORE account may not exist until ore-lst's wrap creates it.
fn token_amount_or_zero(info: &AccountInfo<'_>) -> Result<u64> {
    if info.data_is_empty() {
        return Ok(0);
    }
    token_amount(info)
}
