use anchor_lang::prelude::*;
use anchor_spl::{token::Token, token_interface::TokenAccount as InterfaceTokenAccount};

use crate::{
    constants::{
        MAX_COMPONENTS, MAX_FIXED_WEIGHT_EXECUTION_SLIPPAGE_BPS, MAX_METADATA_URI_LEN,
        MAX_NAME_LEN, MAX_NAV_TOLERANCE_BPS, MAX_ORACLE_PRICE_TOLERANCE_BPS, MAX_REBALANCE_SWAPS,
        MAX_SYMBOL_LEN, VAULT_AUTHORITY_SEED,
    },
    errors::OmnindexError,
    events::IndexRebalanced,
    state::{IndexKind, IndexState},
    utils::{
        associated_token_address_with_token_program,
        create_associated_token_account_idempotent_for_token_program, invoke_jupiter_swap,
        load_interface_mint, load_interface_token_account, load_mint, nav_nad, rebalance_mints,
        rebalance_price_for_mint, resolve_rebalance_prices, target_component_amount,
        validate_jupiter_route_account_scope, validate_no_self_component,
        validate_rebalance_execution_value, validate_rebalance_quote_mint,
        verified_switchboard_prices, within_bps_tolerance_u128, RebalancePrice,
        RebalancePriceInput, ASSOCIATED_TOKEN_ID,
    },
};

use super::mint_index_with_jupiter::JupiterSwapPlan;

const REBALANCE_ACCOUNT_STRIDE: usize = 3;

#[derive(AnchorSerialize, AnchorDeserialize, Clone, Debug)]
pub struct ExecuteRebalanceArgs {
    pub swaps: Vec<JupiterSwapPlan>,
    pub prices: Vec<RebalancePriceInput>,
    pub switchboard_max_age_slots: u64,
}

#[derive(Clone)]
struct RebalanceMintAccount {
    mint: Pubkey,
    token_program: Pubkey,
    vault_account_index: usize,
    target_amount: u64,
    is_new_component: bool,
}

struct RebalanceSwapContext<'a, 'info> {
    jupiter_program: AccountInfo<'info>,
    vault_authority: AccountInfo<'info>,
    rebalance_accounts: &'info [AccountInfo<'info>],
    prices: &'a [RebalancePrice],
    protected_vaults: &'a [Pubkey],
    candidates: &'a [AccountInfo<'info>],
    signer_seeds: &'a [&'a [u8]],
}

#[derive(Accounts)]
pub struct ExecuteRebalance<'info> {
    #[account(mut)]
    pub authority: Signer<'info>,
    #[account(
        mut,
        has_one = authority @ OmnindexError::UnauthorizedAuthority,
        has_one = index_mint @ OmnindexError::IndexMintMismatch,
        realloc = 8 + IndexState::space(
            MAX_NAME_LEN,
            MAX_SYMBOL_LEN,
            MAX_METADATA_URI_LEN,
            MAX_COMPONENTS
        ),
        realloc::payer = authority,
        realloc::zero = true
    )]
    pub index: Account<'info, IndexState>,
    /// CHECK: Validated as the configured classic SPL Token mint in the handler.
    pub index_mint: UncheckedAccount<'info>,
    /// CHECK: PDA authority over the index mint and component vaults.
    #[account(
        seeds = [VAULT_AUTHORITY_SEED, index.key().as_ref()],
        bump = index.vault_authority_bump
    )]
    pub vault_authority: UncheckedAccount<'info>,
    /// CHECK: Validated against known Jupiter program ids.
    pub jupiter_program: UncheckedAccount<'info>,
    /// CHECK: Verified by Switchboard's quote verifier.
    pub switchboard_queue: UncheckedAccount<'info>,
    /// CHECK: Verified by Switchboard's quote verifier and canonical key check.
    pub switchboard_quote: UncheckedAccount<'info>,
    /// CHECK: Switchboard verifier validates this sysvar id.
    pub slothashes: UncheckedAccount<'info>,
    /// CHECK: Switchboard verifier validates this sysvar id.
    pub instructions_sysvar: UncheckedAccount<'info>,
    /// CHECK: Validated as the Associated Token Program.
    #[account(address = ASSOCIATED_TOKEN_ID @ OmnindexError::InvalidAssociatedTokenProgram)]
    pub associated_token_program: UncheckedAccount<'info>,
    pub token_program: Program<'info, Token>,
    pub system_program: Program<'info, System>,
}

impl<'info> ExecuteRebalance<'info> {
    pub fn handle(
        ctx: Context<'_, '_, 'info, 'info, Self>,
        args: ExecuteRebalanceArgs,
    ) -> Result<()> {
        require!(
            !ctx.accounts.index.rebalancing_paused,
            OmnindexError::RebalancingPaused
        );
        require!(
            ctx.accounts.index.kind == IndexKind::FixedUnits,
            OmnindexError::InvalidIndexKind
        );
        require!(
            ctx.accounts.index.pending_component_count > 0,
            OmnindexError::NoPendingRebalance
        );
        require!(
            args.swaps.len() <= MAX_REBALANCE_SWAPS,
            OmnindexError::TooManyRebalanceSwaps
        );
        validate_rebalance_swap_controls(&args.swaps)?;
        require!(
            ctx.accounts
                .index
                .pending_rebalance_oracle_price_tolerance_bps
                <= MAX_ORACLE_PRICE_TOLERANCE_BPS,
            OmnindexError::InvalidOraclePriceTolerance
        );
        require!(
            ctx.accounts.index.pending_rebalance_nav_tolerance_bps <= MAX_NAV_TOLERANCE_BPS,
            OmnindexError::InvalidNavTolerance
        );
        require!(
            Clock::get()?.unix_timestamp >= ctx.accounts.index.pending_rebalance_available_at,
            OmnindexError::RebalanceTimelockActive
        );
        validate_rebalance_quote_mint(&ctx.accounts.index.pending_rebalance_quote_mint)?;

        let new_components = ctx.accounts.index.pending_components.clone();
        validate_no_self_component(&new_components, &ctx.accounts.index_mint.key())?;
        let old_components = ctx.accounts.index.components.clone();
        let rebalance_mints = rebalance_mints(&old_components, &new_components);
        let mint_account_count = rebalance_mints
            .len()
            .checked_mul(REBALANCE_ACCOUNT_STRIDE)
            .ok_or_else(|| error!(OmnindexError::ArithmeticOverflow))?;
        require!(
            ctx.remaining_accounts.len() >= mint_account_count,
            OmnindexError::InvalidRemainingAccounts
        );
        let rebalance_accounts = &ctx.remaining_accounts[..mint_account_count];
        let route_accounts = &ctx.remaining_accounts[mint_account_count..];

        let base_units = ctx.accounts.index.index_base_units()?;
        let index_mint = load_mint(&ctx.accounts.index_mint.to_account_info())?;
        let supply = index_mint.supply;
        let mut mint_accounts = Vec::with_capacity(rebalance_mints.len());
        let mut mint_decimals = Vec::with_capacity(rebalance_mints.len());

        for (index, mint) in rebalance_mints.iter().enumerate() {
            let mint_account_index = index * REBALANCE_ACCOUNT_STRIDE;
            let vault_account_index = mint_account_index + 1;
            let token_program_account_index = mint_account_index + 2;
            let mint_info = &rebalance_accounts[mint_account_index];
            let vault_info = &rebalance_accounts[vault_account_index];
            let token_program_info = &rebalance_accounts[token_program_account_index];

            require_keys_eq!(mint_info.key(), *mint, OmnindexError::InvalidComponentMint);
            require_keys_eq!(
                *mint_info.owner,
                token_program_info.key(),
                OmnindexError::InvalidTokenMint
            );
            mint_decimals.push(load_interface_mint(mint_info)?.decimals);

            create_component_vault_if_needed(&ctx, mint_info, vault_info, token_program_info)?;

            let vault_token_account = load_interface_token_account(vault_info)?;
            validate_rebalance_vault_account(
                &vault_token_account,
                vault_info,
                &ctx.accounts.vault_authority.key(),
                mint,
                token_program_info.key,
            )?;

            mint_accounts.push(RebalanceMintAccount {
                mint: *mint,
                token_program: token_program_info.key(),
                vault_account_index,
                target_amount: target_component_amount(&new_components, mint, supply, base_units)?,
                is_new_component: new_components
                    .iter()
                    .any(|component| component.mint == *mint),
            });
        }

        let switchboard_prices = verified_switchboard_prices(
            &ctx.accounts.switchboard_queue.to_account_info(),
            &ctx.accounts.switchboard_quote.to_account_info(),
            &ctx.accounts.slothashes.to_account_info(),
            &ctx.accounts.instructions_sysvar.to_account_info(),
            Clock::get()?.slot,
            args.switchboard_max_age_slots,
        )?;
        let prices = resolve_rebalance_prices(
            &rebalance_mints,
            &old_components,
            &new_components,
            &mint_decimals,
            &args.prices,
            ctx.accounts
                .index
                .pending_rebalance_oracle_price_tolerance_bps,
            &switchboard_prices,
        )?;
        let old_nav_nad = nav_nad(&old_components, &prices)?;
        let new_nav_nad = nav_nad(&new_components, &prices)?;
        require!(
            old_nav_nad > 0 && new_nav_nad > 0,
            OmnindexError::RebalanceNavMismatch
        );
        require!(
            within_bps_tolerance_u128(
                old_nav_nad,
                new_nav_nad,
                ctx.accounts.index.pending_rebalance_nav_tolerance_bps,
            )?,
            OmnindexError::RebalanceNavMismatch
        );

        let index_key = ctx.accounts.index.key();
        let vault_authority_bump = [ctx.accounts.index.vault_authority_bump];
        let signer_seeds: &[&[u8]] = &[
            VAULT_AUTHORITY_SEED,
            index_key.as_ref(),
            &vault_authority_bump,
        ];
        let candidates = account_candidates(&ctx, rebalance_accounts, route_accounts);
        let protected_vaults = protected_vault_keys(&mint_accounts, rebalance_accounts);
        let swap_context = RebalanceSwapContext {
            jupiter_program: ctx.accounts.jupiter_program.to_account_info(),
            vault_authority: ctx.accounts.vault_authority.to_account_info(),
            rebalance_accounts,
            prices: &prices,
            protected_vaults: &protected_vaults,
            candidates: &candidates,
            signer_seeds,
        };

        for swap in &args.swaps {
            execute_jupiter_rebalance_swap(swap, &mint_accounts, &swap_context)?;
        }

        for account in &mint_accounts {
            let vault_info = &rebalance_accounts[account.vault_account_index];
            let vault_token_account = load_interface_token_account(vault_info)?;
            validate_rebalance_vault_account(
                &vault_token_account,
                vault_info,
                &ctx.accounts.vault_authority.key(),
                &account.mint,
                &account.token_program,
            )?;
            validate_rebalance_final_amount(
                vault_token_account.amount,
                account.target_amount,
                account.is_new_component,
            )?;
        }

        let index = &mut ctx.accounts.index;
        index.component_count = new_components.len() as u8;
        index.components = new_components;
        index.pending_component_count = 0;
        index.pending_components = Vec::new();
        index.pending_rebalance_available_at = 0;
        index.pending_rebalance_quote_mint = Pubkey::default();
        index.pending_rebalance_oracle_price_tolerance_bps = 0;
        index.pending_rebalance_nav_tolerance_bps = 0;

        emit!(IndexRebalanced {
            index: index.key(),
            authority: ctx.accounts.authority.key(),
            components: index.component_count,
            supply,
        });

        Ok(())
    }
}

fn execute_jupiter_rebalance_swap<'info>(
    swap: &JupiterSwapPlan,
    mint_accounts: &[RebalanceMintAccount],
    context: &RebalanceSwapContext<'_, 'info>,
) -> Result<()> {
    require_keys_neq!(
        swap.input_mint,
        swap.output_mint,
        OmnindexError::InvalidRebalanceSwap
    );
    let input_account = mint_accounts
        .iter()
        .find(|account| account.mint == swap.input_mint)
        .ok_or_else(|| error!(OmnindexError::InvalidRebalanceSwap))?;
    let output_account = mint_accounts
        .iter()
        .find(|account| account.mint == swap.output_mint)
        .ok_or_else(|| error!(OmnindexError::InvalidRebalanceSwap))?;
    let input_vault_info = &context.rebalance_accounts[input_account.vault_account_index];
    let output_vault_info = &context.rebalance_accounts[output_account.vault_account_index];

    require_keys_eq!(
        swap.source_token_account,
        input_vault_info.key(),
        OmnindexError::InvalidJupiterRoute
    );
    require_keys_eq!(
        swap.destination_token_account,
        output_vault_info.key(),
        OmnindexError::InvalidJupiterRoute
    );
    validate_jupiter_route_account_scope(
        &swap.accounts,
        context.protected_vaults,
        &[input_vault_info.key(), output_vault_info.key()],
    )?;

    let input_before = load_interface_token_account(input_vault_info)?.amount;
    let output_before = load_interface_token_account(output_vault_info)?.amount;
    let max_sell_amount = input_before
        .checked_sub(input_account.target_amount)
        .ok_or_else(|| error!(OmnindexError::RebalanceWouldSellTargetBacking))?;
    require!(
        max_sell_amount > 0,
        OmnindexError::RebalanceWouldSellTargetBacking
    );

    invoke_jupiter_swap(
        context.jupiter_program.clone(),
        context.candidates,
        &swap.accounts,
        &swap.instruction_data,
        Some(context.vault_authority.key()),
        &[context.signer_seeds],
    )?;

    let input_after = load_interface_token_account(input_vault_info)?.amount;
    let output_after = load_interface_token_account(output_vault_info)?.amount;
    let input_spent = input_before
        .checked_sub(input_after)
        .ok_or_else(|| error!(OmnindexError::InvalidJupiterRoute))?;
    let output_received = output_after
        .checked_sub(output_before)
        .ok_or_else(|| error!(OmnindexError::InvalidJupiterRoute))?;
    require!(input_spent > 0, OmnindexError::InvalidJupiterRoute);
    require!(output_received > 0, OmnindexError::InvalidJupiterRoute);
    require!(
        input_spent <= max_sell_amount,
        OmnindexError::RebalanceWouldSellTargetBacking
    );

    let input_price = rebalance_price_for_mint(context.prices, &swap.input_mint)?;
    let output_price = rebalance_price_for_mint(context.prices, &swap.output_mint)?;
    validate_rebalance_execution_value(
        input_spent,
        input_price,
        output_received,
        output_price,
        swap.max_oracle_slippage_bps,
    )
}

fn validate_rebalance_swap_controls(swaps: &[JupiterSwapPlan]) -> Result<()> {
    for swap in swaps {
        require!(
            swap.max_oracle_slippage_bps <= MAX_FIXED_WEIGHT_EXECUTION_SLIPPAGE_BPS,
            OmnindexError::InvalidOraclePriceTolerance
        );
    }

    Ok(())
}

fn protected_vault_keys(
    mint_accounts: &[RebalanceMintAccount],
    rebalance_accounts: &[AccountInfo<'_>],
) -> Vec<Pubkey> {
    mint_accounts
        .iter()
        .map(|account| rebalance_accounts[account.vault_account_index].key())
        .collect()
}

fn account_candidates<'info>(
    ctx: &Context<'_, '_, 'info, 'info, ExecuteRebalance<'info>>,
    rebalance_accounts: &[AccountInfo<'info>],
    route_accounts: &[AccountInfo<'info>],
) -> Vec<AccountInfo<'info>> {
    let mut candidates = vec![
        ctx.accounts.authority.to_account_info(),
        ctx.accounts.index.to_account_info(),
        ctx.accounts.index_mint.to_account_info(),
        ctx.accounts.vault_authority.to_account_info(),
        ctx.accounts.jupiter_program.to_account_info(),
        ctx.accounts.associated_token_program.to_account_info(),
        ctx.accounts.token_program.to_account_info(),
        ctx.accounts.system_program.to_account_info(),
    ];
    candidates.extend_from_slice(rebalance_accounts);
    candidates.extend_from_slice(route_accounts);
    candidates
}

fn create_component_vault_if_needed<'info>(
    ctx: &Context<'_, '_, 'info, 'info, ExecuteRebalance<'info>>,
    component_mint_info: &AccountInfo<'info>,
    vault_info: &AccountInfo<'info>,
    token_program_info: &AccountInfo<'info>,
) -> Result<()> {
    let expected_vault = associated_token_address_with_token_program(
        &ctx.accounts.vault_authority.key(),
        &component_mint_info.key(),
        token_program_info.key,
    );
    require_keys_eq!(
        vault_info.key(),
        expected_vault,
        OmnindexError::InvalidVaultAccount
    );

    create_associated_token_account_idempotent_for_token_program(
        ctx.accounts.associated_token_program.to_account_info(),
        ctx.accounts.authority.to_account_info(),
        vault_info.clone(),
        ctx.accounts.vault_authority.to_account_info(),
        component_mint_info.clone(),
        ctx.accounts.system_program.to_account_info(),
        token_program_info.clone(),
    )?;

    Ok(())
}

fn validate_rebalance_vault_account(
    token_account: &InterfaceTokenAccount,
    vault_info: &AccountInfo<'_>,
    vault_authority: &Pubkey,
    component_mint: &Pubkey,
    token_program: &Pubkey,
) -> Result<()> {
    let expected_vault =
        associated_token_address_with_token_program(vault_authority, component_mint, token_program);
    require_keys_eq!(
        vault_info.key(),
        expected_vault,
        OmnindexError::InvalidVaultAccount
    );
    require_keys_eq!(
        *vault_info.owner,
        *token_program,
        OmnindexError::InvalidVaultAccount
    );
    require_keys_eq!(
        token_account.owner,
        *vault_authority,
        OmnindexError::InvalidVaultAccount
    );
    require_keys_eq!(
        token_account.mint,
        *component_mint,
        OmnindexError::InvalidVaultAccount
    );
    Ok(())
}

fn validate_rebalance_final_amount(
    actual_amount: u64,
    target_amount: u64,
    is_target_component: bool,
) -> Result<()> {
    require!(
        actual_amount >= target_amount,
        OmnindexError::RebalanceTargetNotMet
    );
    require!(
        is_target_component || actual_amount == 0,
        OmnindexError::RebalanceTargetNotMet
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn final_validation_accepts_target_component_at_target() {
        assert!(validate_rebalance_final_amount(1_250_000, 1_250_000, true).is_ok());
    }

    #[test]
    fn final_validation_accepts_target_component_with_surplus() {
        assert!(validate_rebalance_final_amount(1_250_001, 1_250_000, true).is_ok());
    }

    #[test]
    fn final_validation_rejects_target_component_below_target() {
        assert!(validate_rebalance_final_amount(1_249_999, 1_250_000, true).is_err());
    }

    #[test]
    fn final_validation_rejects_removed_component_with_stranded_balance() {
        assert!(validate_rebalance_final_amount(1, 0, false).is_err());
    }

    #[test]
    fn final_validation_accepts_removed_component_sold_to_zero() {
        assert!(validate_rebalance_final_amount(0, 0, false).is_ok());
    }

    #[test]
    fn rebalance_controls_reject_wide_jupiter_slippage() {
        let swaps = vec![JupiterSwapPlan {
            input_mint: Pubkey::new_unique(),
            output_mint: Pubkey::new_unique(),
            source_token_account: Pubkey::new_unique(),
            destination_token_account: Pubkey::new_unique(),
            max_oracle_slippage_bps: MAX_FIXED_WEIGHT_EXECUTION_SLIPPAGE_BPS + 1,
            instruction_data: vec![1],
            accounts: Vec::new(),
        }];

        assert!(validate_rebalance_swap_controls(&swaps).is_err());
    }
}
