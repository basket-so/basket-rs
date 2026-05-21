use anchor_lang::prelude::*;
use anchor_spl::{
    token::{self, Burn, Token},
    token_interface::{
        self, TokenAccount as InterfaceTokenAccount, TokenInterface, TransferChecked,
    },
};

use crate::{
    constants::{USDC_MINT, VAULT_AUTHORITY_SEED},
    errors::BasketError,
    events::IndexRedeemed,
    state::{IndexComponent, IndexState},
    utils::{
        associated_token_address_with_token_program, basis_points_amount,
        create_associated_token_account_idempotent_for_token_program,
        invoke_jupiter_swap_with_scratch, load_interface_mint, load_interface_token_account,
        load_mint, redeem_component_backing_amount, route_creator_fee,
        validate_jupiter_route_account_scope, validate_pending_component_targets_integral,
        JupiterInvokeScratch, ASSOCIATED_TOKEN_ID,
    },
};

use super::mint_index_with_jupiter::JupiterSwapPlan;

#[derive(AnchorSerialize, AnchorDeserialize, Clone, Debug)]
pub struct RedeemIndexWithJupiterArgs {
    pub index_amount_in: u64,
    pub min_quote_out: u64,
    pub swaps: Vec<JupiterSwapPlan>,
}

#[derive(Accounts)]
pub struct RedeemIndexWithJupiter<'info> {
    #[account(mut)]
    pub user: Signer<'info>,
    #[account(has_one = index_mint @ BasketError::IndexMintMismatch)]
    pub index: Account<'info, IndexState>,
    #[account(mut)]
    /// CHECK: Validated as the configured classic SPL index mint.
    pub index_mint: UncheckedAccount<'info>,
    /// CHECK: PDA authority over the index mint and component vaults.
    #[account(
        seeds = [VAULT_AUTHORITY_SEED, index.key().as_ref()],
        bump = index.vault_authority_bump
    )]
    pub vault_authority: UncheckedAccount<'info>,
    /// CHECK: Validated as the configured quote mint and parsed manually for Token/Token-2022 decimals.
    pub quote_mint: UncheckedAccount<'info>,
    #[account(mut)]
    pub user_quote_token_account: InterfaceAccount<'info, InterfaceTokenAccount>,
    /// CHECK: Validated as the protocol fee recipient's quote token account when protocol fees apply.
    #[account(mut)]
    pub fee_recipient_quote_token_account: UncheckedAccount<'info>,
    /// CHECK: Validated as the creator fee recipient's quote token account when creator fees apply.
    #[account(mut)]
    pub creator_fee_recipient_quote_token_account: UncheckedAccount<'info>,
    #[account(mut)]
    /// CHECK: Validated as the user's index token account before burning.
    pub user_index_token_account: UncheckedAccount<'info>,
    /// CHECK: Validated against known Jupiter program ids.
    pub jupiter_program: UncheckedAccount<'info>,
    /// CHECK: Validated as the Associated Token Program.
    #[account(address = ASSOCIATED_TOKEN_ID @ BasketError::InvalidAssociatedTokenProgram)]
    pub associated_token_program: UncheckedAccount<'info>,
    pub quote_token_program: Interface<'info, TokenInterface>,
    pub index_token_program: Program<'info, Token>,
    pub system_program: Program<'info, System>,
}

#[derive(Clone)]
struct ComponentAccount<'info> {
    mint_info: AccountInfo<'info>,
    vault_info: AccountInfo<'info>,
    token_program_info: AccountInfo<'info>,
}

impl<'info> RedeemIndexWithJupiter<'info> {
    pub fn handle(
        ctx: Context<'_, '_, 'info, 'info, Self>,
        args: RedeemIndexWithJupiterArgs,
    ) -> Result<()> {
        require!(args.index_amount_in > 0, BasketError::InvalidIndexAmount);
        require!(
            !ctx.accounts.index.redeeming_paused,
            BasketError::RedeemingPaused
        );
        require_keys_eq!(
            ctx.accounts.quote_mint.key(),
            USDC_MINT,
            BasketError::InvalidQuoteMint
        );
        require_keys_eq!(
            *ctx.accounts.quote_mint.to_account_info().owner,
            ctx.accounts.quote_token_program.key(),
            BasketError::InvalidQuoteMint
        );
        let quote_mint = load_interface_mint(&ctx.accounts.quote_mint.to_account_info())?;
        let total_redeem_fee_bps = ctx
            .accounts
            .index
            .redeem_fee_bps
            .checked_add(ctx.accounts.index.creator_redeem_fee_bps)
            .ok_or_else(|| error!(BasketError::InvalidFeeBps))?;
        require!(
            total_redeem_fee_bps <= crate::constants::MAX_TOTAL_INDEX_FEE_BPS,
            BasketError::InvalidFeeBps
        );
        require!(
            args.swaps.len() <= ctx.accounts.index.components.len(),
            BasketError::InvalidJupiterRoute
        );

        let index_mint = load_mint(&ctx.accounts.index_mint.to_account_info())?;
        let current_supply = index_mint.supply;
        let post_supply = current_supply
            .checked_sub(args.index_amount_in)
            .ok_or_else(|| error!(BasketError::InvalidIndexAmount))?;
        validate_pending_component_targets_integral(&ctx.accounts.index, post_supply)?;

        let components = ctx.accounts.index.components.clone();
        let mut remaining = ctx.remaining_accounts.iter();
        let mut component_accounts = Vec::with_capacity(components.len());
        for component in &components {
            component_accounts.push(load_component_account(&ctx, &mut remaining, component)?);
        }
        let route_accounts = remaining.as_slice();
        let protected_vaults = protected_vault_keys(&component_accounts);
        let candidates = account_candidates(&ctx, &component_accounts, route_accounts);
        let signer_seeds: &[&[u8]] = &[
            VAULT_AUTHORITY_SEED,
            ctx.accounts.index.to_account_info().key.as_ref(),
            &[ctx.accounts.index.vault_authority_bump],
        ];

        burn_index_tokens(&ctx, args.index_amount_in)?;

        let quote_decimals = quote_mint.decimals;
        let mut total_quote_out = 0u64;
        let mut swap_index = 0usize;
        let mut jupiter_scratch = JupiterInvokeScratch::new();

        for (component, accounts) in components.iter().zip(component_accounts.into_iter()) {
            let vault_before = load_interface_token_account(&accounts.vault_info)?.amount;
            let backing_amount = redeem_component_backing_amount(
                args.index_amount_in,
                current_supply,
                vault_before,
            )?;
            if backing_amount == 0 {
                continue;
            }

            if component.mint == ctx.accounts.quote_mint.key() {
                transfer_checked_from_vault_quote(
                    &ctx,
                    accounts.vault_info.clone(),
                    signer_seeds,
                    backing_amount,
                    quote_decimals,
                )?;
                ctx.accounts.user_quote_token_account.reload()?;
                total_quote_out = total_quote_out
                    .checked_add(backing_amount)
                    .ok_or_else(|| error!(BasketError::ArithmeticOverflow))?;
                continue;
            }

            let swap = args
                .swaps
                .get(swap_index)
                .ok_or_else(|| error!(BasketError::InvalidJupiterRoute))?;
            swap_index = swap_index
                .checked_add(1)
                .ok_or_else(|| error!(BasketError::ArithmeticOverflow))?;
            validate_jupiter_route_account_scope(
                &candidates,
                &swap.accounts,
                &protected_vaults,
                &[
                    accounts.vault_info.key(),
                    ctx.accounts.user_quote_token_account.key(),
                ],
            )?;

            let quote_before = ctx.accounts.user_quote_token_account.amount;
            invoke_jupiter_swap_with_scratch(
                ctx.accounts.jupiter_program.to_account_info(),
                &candidates,
                &swap.accounts,
                &swap.instruction_data,
                Some(ctx.accounts.vault_authority.key()),
                &[signer_seeds],
                &mut jupiter_scratch,
            )?;

            ctx.accounts.user_quote_token_account.reload()?;
            let vault_after = load_interface_token_account(&accounts.vault_info)?.amount;
            let quote_after = ctx.accounts.user_quote_token_account.amount;
            let component_spent = vault_before
                .checked_sub(vault_after)
                .ok_or_else(|| error!(BasketError::InvalidJupiterRoute))?;
            require!(
                component_spent == backing_amount,
                BasketError::InvalidJupiterRoute
            );
            let quote_received = quote_after
                .checked_sub(quote_before)
                .ok_or_else(|| error!(BasketError::InvalidJupiterRoute))?;
            total_quote_out = total_quote_out
                .checked_add(quote_received)
                .ok_or_else(|| error!(BasketError::ArithmeticOverflow))?;
        }

        require!(
            swap_index == args.swaps.len(),
            BasketError::InvalidJupiterRoute
        );
        let (protocol_fee, creator_fee) = quote_fee_split(
            total_quote_out,
            ctx.accounts.index.redeem_fee_bps,
            ctx.accounts.index.creator_redeem_fee_bps,
        )?;
        let net_quote_out = total_quote_out
            .checked_sub(protocol_fee)
            .and_then(|value| value.checked_sub(creator_fee))
            .ok_or_else(|| error!(BasketError::ArithmeticOverflow))?;
        require!(
            net_quote_out >= args.min_quote_out,
            BasketError::QuoteBudgetExceeded
        );
        collect_quote_fees_from_user(&ctx, protocol_fee, creator_fee, quote_decimals)?;

        emit!(IndexRedeemed {
            index: ctx.accounts.index.key(),
            redeemer: ctx.accounts.user.key(),
            amount: args.index_amount_in,
        });

        Ok(())
    }
}

fn burn_index_tokens<'info>(
    ctx: &Context<'_, '_, 'info, 'info, RedeemIndexWithJupiter<'info>>,
    amount: u64,
) -> Result<()> {
    token::burn(
        CpiContext::new(
            ctx.accounts.index_token_program.to_account_info(),
            Burn {
                mint: ctx.accounts.index_mint.to_account_info(),
                from: ctx.accounts.user_index_token_account.to_account_info(),
                authority: ctx.accounts.user.to_account_info(),
            },
        ),
        amount,
    )
}

fn load_component_account<'info>(
    ctx: &Context<'_, '_, 'info, 'info, RedeemIndexWithJupiter<'info>>,
    remaining: &mut std::slice::Iter<'info, AccountInfo<'info>>,
    component: &IndexComponent,
) -> Result<ComponentAccount<'info>> {
    let mint_info = next_account_info(remaining)?;
    let vault_info = next_account_info(remaining)?;
    let token_program_info = next_account_info(remaining)?;

    require_keys_eq!(
        mint_info.key(),
        component.mint,
        BasketError::InvalidComponentMint
    );
    require_keys_eq!(
        *mint_info.owner,
        token_program_info.key(),
        BasketError::InvalidTokenMint
    );
    let expected_vault = associated_token_address_with_token_program(
        &ctx.accounts.vault_authority.key(),
        &component.mint,
        token_program_info.key,
    );
    require_keys_eq!(
        vault_info.key(),
        expected_vault,
        BasketError::InvalidVaultAccount
    );
    create_associated_token_account_idempotent_for_token_program(
        ctx.accounts.associated_token_program.to_account_info(),
        ctx.accounts.user.to_account_info(),
        vault_info.clone(),
        ctx.accounts.vault_authority.to_account_info(),
        mint_info.clone(),
        ctx.accounts.system_program.to_account_info(),
        token_program_info.clone(),
    )?;

    load_interface_mint(mint_info)?;
    let vault = load_interface_token_account(vault_info)?;
    require_keys_eq!(
        vault.owner,
        ctx.accounts.vault_authority.key(),
        BasketError::InvalidVaultAccount
    );
    require_keys_eq!(vault.mint, component.mint, BasketError::InvalidVaultAccount);

    Ok(ComponentAccount {
        mint_info: mint_info.clone(),
        vault_info: vault_info.clone(),
        token_program_info: token_program_info.clone(),
    })
}

fn account_candidates<'info>(
    ctx: &Context<'_, '_, 'info, 'info, RedeemIndexWithJupiter<'info>>,
    component_accounts: &[ComponentAccount<'info>],
    route_accounts: &[AccountInfo<'info>],
) -> Vec<AccountInfo<'info>> {
    let mut candidates = vec![
        ctx.accounts.user.to_account_info(),
        ctx.accounts.index.to_account_info(),
        ctx.accounts.index_mint.to_account_info(),
        ctx.accounts.vault_authority.to_account_info(),
        ctx.accounts.quote_mint.to_account_info(),
        ctx.accounts.user_quote_token_account.to_account_info(),
        ctx.accounts.user_index_token_account.to_account_info(),
        ctx.accounts.jupiter_program.to_account_info(),
        ctx.accounts.associated_token_program.to_account_info(),
        ctx.accounts.quote_token_program.to_account_info(),
        ctx.accounts.index_token_program.to_account_info(),
        ctx.accounts.system_program.to_account_info(),
    ];
    for account in component_accounts {
        candidates.push(account.mint_info.clone());
        candidates.push(account.vault_info.clone());
        candidates.push(account.token_program_info.clone());
    }
    candidates.extend_from_slice(route_accounts);
    candidates
}

fn protected_vault_keys(component_accounts: &[ComponentAccount<'_>]) -> Vec<Pubkey> {
    component_accounts
        .iter()
        .map(|account| account.vault_info.key())
        .collect()
}

fn transfer_checked_from_vault_quote<'info>(
    ctx: &Context<'_, '_, 'info, 'info, RedeemIndexWithJupiter<'info>>,
    source: AccountInfo<'info>,
    signer_seeds: &[&[u8]],
    amount: u64,
    quote_decimals: u8,
) -> Result<()> {
    token_interface::transfer_checked(
        CpiContext::new_with_signer(
            ctx.accounts.quote_token_program.to_account_info(),
            TransferChecked {
                from: source,
                mint: ctx.accounts.quote_mint.to_account_info(),
                to: ctx.accounts.user_quote_token_account.to_account_info(),
                authority: ctx.accounts.vault_authority.to_account_info(),
            },
            &[signer_seeds],
        ),
        amount,
        quote_decimals,
    )
}

fn quote_fee_split(
    quote_amount: u64,
    protocol_fee_bps: u16,
    creator_fee_bps: u16,
) -> Result<(u64, u64)> {
    Ok((
        basis_points_amount(quote_amount, protocol_fee_bps)?,
        basis_points_amount(quote_amount, creator_fee_bps)?,
    ))
}

fn collect_quote_fees_from_user<'info>(
    ctx: &Context<'_, '_, 'info, 'info, RedeemIndexWithJupiter<'info>>,
    protocol_fee: u64,
    creator_fee: u64,
    quote_decimals: u8,
) -> Result<()> {
    let (protocol_fee, creator_fee) = route_creator_fee(
        protocol_fee,
        creator_fee,
        &ctx.accounts.index.creator_fee_recipient,
    )?;
    transfer_quote_fee_from_user(
        ctx,
        ctx.accounts
            .fee_recipient_quote_token_account
            .to_account_info(),
        ctx.accounts.index.fee_recipient,
        protocol_fee,
        false,
        quote_decimals,
    )?;
    transfer_quote_fee_from_user(
        ctx,
        ctx.accounts
            .creator_fee_recipient_quote_token_account
            .to_account_info(),
        ctx.accounts.index.creator_fee_recipient,
        creator_fee,
        true,
        quote_decimals,
    )
}

fn transfer_quote_fee_from_user<'info>(
    ctx: &Context<'_, '_, 'info, 'info, RedeemIndexWithJupiter<'info>>,
    destination: AccountInfo<'info>,
    expected_owner: Pubkey,
    amount: u64,
    creator_fee: bool,
    quote_decimals: u8,
) -> Result<()> {
    if amount == 0 {
        return Ok(());
    }

    let destination_account = load_interface_token_account(&destination)?;
    if creator_fee {
        require_keys_eq!(
            destination_account.owner,
            expected_owner,
            BasketError::InvalidCreatorFeeRecipientTokenAccount
        );
        require_keys_eq!(
            destination_account.mint,
            ctx.accounts.quote_mint.key(),
            BasketError::InvalidCreatorFeeRecipientTokenAccount
        );
    } else {
        require_keys_eq!(
            destination_account.owner,
            expected_owner,
            BasketError::InvalidFeeRecipientTokenAccount
        );
        require_keys_eq!(
            destination_account.mint,
            ctx.accounts.quote_mint.key(),
            BasketError::InvalidFeeRecipientTokenAccount
        );
    }

    if destination.key() == ctx.accounts.user_quote_token_account.key() {
        return Ok(());
    }

    token_interface::transfer_checked(
        CpiContext::new(
            ctx.accounts.quote_token_program.to_account_info(),
            TransferChecked {
                from: ctx.accounts.user_quote_token_account.to_account_info(),
                mint: ctx.accounts.quote_mint.to_account_info(),
                to: destination,
                authority: ctx.accounts.user.to_account_info(),
            },
        ),
        amount,
        quote_decimals,
    )
}
