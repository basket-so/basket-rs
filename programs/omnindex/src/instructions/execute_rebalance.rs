use anchor_lang::prelude::*;
use anchor_spl::token::{Token, TokenAccount};

use crate::{
    constants::{
        MAX_COMPONENTS, MAX_METADATA_URI_LEN, MAX_NAME_LEN, MAX_REBALANCE_SWAPS, MAX_SYMBOL_LEN,
        VAULT_AUTHORITY_SEED,
    },
    errors::OmnindexError,
    events::IndexRebalanced,
    state::{IndexKind, IndexState},
    utils::{
        associated_token_address, create_associated_token_account_idempotent,
        load_futarchy_authority, load_mint, load_pair, load_rate_model, nav_nad,
        omnipair_event_authority_address, omnipair_futarchy_authority_address,
        quote_exact_output_for_pair_input, rebalance_mints, reserve_vault_address,
        resolve_rebalance_prices, swap as omnipair_swap, target_component_amount,
        validate_no_self_component, validate_vault_token_account, within_bps_tolerance_u128,
        RebalancePriceInput, SwapAccounts, SwapArgs, ASSOCIATED_TOKEN_ID, OMNIPAIR_ID,
        TOKEN_2022_ID,
    },
};

#[derive(AnchorSerialize, AnchorDeserialize, Clone, Debug)]
pub struct ExecuteRebalanceArgs {
    pub swaps: Vec<RebalanceSwapInput>,
    pub prices: Vec<RebalancePriceInput>,
}

#[derive(AnchorSerialize, AnchorDeserialize, Clone, Debug)]
pub struct RebalanceSwapInput {
    pub token_in_mint: Pubkey,
    pub token_out_mint: Pubkey,
    pub amount_in: u64,
    pub min_amount_out: u64,
}

#[derive(Clone)]
struct RebalanceMintAccount {
    mint: Pubkey,
    mint_account_index: usize,
    vault_account_index: usize,
    target_amount: u64,
    is_new_component: bool,
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
    pub token_program: Program<'info, Token>,
    /// CHECK: Omnipair's swap ABI requires this program account even when routes use classic SPL Token.
    #[account(address = TOKEN_2022_ID @ OmnindexError::InvalidOmnipairTokenProgram)]
    pub token_2022_program: UncheckedAccount<'info>,
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
        require!(
            Clock::get()?.unix_timestamp >= ctx.accounts.index.pending_rebalance_available_at,
            OmnindexError::RebalanceTimelockActive
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

        let new_components = ctx.accounts.index.pending_components.clone();
        validate_no_self_component(&new_components, &ctx.accounts.index_mint.key())?;
        let old_components = ctx.accounts.index.components.clone();
        let rebalance_mints = rebalance_mints(&old_components, &new_components);

        let mut remaining = ctx.remaining_accounts.iter();
        let prices = resolve_rebalance_prices(
            &mut remaining,
            &rebalance_mints,
            &ctx.accounts.index.pending_rebalance_quote_mint,
            &args.prices,
            ctx.accounts
                .index
                .pending_rebalance_oracle_price_tolerance_bps,
            &futarchy_authority,
        )?;
        let old_nav_nad = nav_nad(&old_components, &rebalance_mints, &prices)?;
        let new_nav_nad = nav_nad(&new_components, &rebalance_mints, &prices)?;
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

        let rebalance_accounts = remaining.as_slice();
        let mint_account_count = rebalance_mints.len() * 2;
        let swap_account_count = args.swaps.len() * 4;

        require!(
            rebalance_accounts.len() == mint_account_count + swap_account_count,
            OmnindexError::InvalidRemainingAccounts
        );

        let base_units = ctx.accounts.index.index_base_units()?;
        let index_mint = load_mint(&ctx.accounts.index_mint.to_account_info())?;
        let supply = index_mint.supply;
        let signer_seeds: &[&[u8]] = &[
            VAULT_AUTHORITY_SEED,
            ctx.accounts.index.to_account_info().key.as_ref(),
            &[ctx.accounts.index.vault_authority_bump],
        ];

        let mut mint_accounts = Vec::with_capacity(rebalance_mints.len());

        for (index, mint) in rebalance_mints.iter().enumerate() {
            let mint_account_index = index * 2;
            let vault_account_index = mint_account_index + 1;
            let mint_info = &rebalance_accounts[mint_account_index];
            let vault_info = &rebalance_accounts[vault_account_index];

            require_keys_eq!(mint_info.key(), *mint, OmnindexError::InvalidComponentMint);

            create_component_vault_if_needed(&ctx, mint_info, vault_info)?;

            let vault_token_account = Account::<TokenAccount>::try_from(vault_info)?;
            validate_vault_token_account(
                &vault_token_account,
                &vault_info.key(),
                &ctx.accounts.vault_authority.key(),
                mint,
            )?;

            mint_accounts.push(RebalanceMintAccount {
                mint: *mint,
                mint_account_index,
                vault_account_index,
                target_amount: target_component_amount(&new_components, mint, supply, base_units)?,
                is_new_component: new_components
                    .iter()
                    .any(|component| component.mint == *mint),
            });
        }

        for (swap_index, swap) in args.swaps.iter().enumerate() {
            require!(swap.amount_in > 0, OmnindexError::InvalidRebalanceSwap);
            require!(swap.min_amount_out > 0, OmnindexError::InvalidRebalanceSwap);
            require_keys_neq!(
                swap.token_in_mint,
                swap.token_out_mint,
                OmnindexError::InvalidRebalanceSwap
            );

            let input_index = mint_accounts
                .iter()
                .position(|account| account.mint == swap.token_in_mint)
                .ok_or_else(|| error!(OmnindexError::InvalidRebalanceSwap))?;
            let output_index = mint_accounts
                .iter()
                .position(|account| account.mint == swap.token_out_mint)
                .ok_or_else(|| error!(OmnindexError::InvalidRebalanceSwap))?;

            let input_account = mint_accounts[input_index].clone();
            let output_account = mint_accounts[output_index].clone();
            let input_mint_info = &rebalance_accounts[input_account.mint_account_index];
            let input_vault_info = &rebalance_accounts[input_account.vault_account_index];
            let output_mint_info = &rebalance_accounts[output_account.mint_account_index];
            let output_vault_info = &rebalance_accounts[output_account.vault_account_index];
            let input_vault_account = Account::<TokenAccount>::try_from(input_vault_info)?;
            let max_sell_amount = input_vault_account
                .amount
                .checked_sub(input_account.target_amount)
                .ok_or_else(|| error!(OmnindexError::RebalanceWouldSellTargetBacking))?;
            require!(
                swap.amount_in <= max_sell_amount,
                OmnindexError::RebalanceWouldSellTargetBacking
            );

            let swap_account_offset = mint_account_count + (swap_index * 4);
            let pair_info = &rebalance_accounts[swap_account_offset];
            let rate_model_info = &rebalance_accounts[swap_account_offset + 1];
            let token_in_reserve_vault_info = &rebalance_accounts[swap_account_offset + 2];
            let token_out_reserve_vault_info = &rebalance_accounts[swap_account_offset + 3];

            let pair = load_pair(pair_info)?;
            let rate_model = load_rate_model(rate_model_info)?;
            require_keys_eq!(
                rate_model_info.key(),
                pair.rate_model,
                OmnindexError::InvalidOmnipairRateModel
            );
            require!(
                (pair.token0 == swap.token_in_mint && pair.token1 == swap.token_out_mint)
                    || (pair.token1 == swap.token_in_mint && pair.token0 == swap.token_out_mint),
                OmnindexError::InvalidOmnipairPair
            );

            validate_omnipair_reserve(
                *pair_info.key,
                &swap.token_in_mint,
                token_in_reserve_vault_info,
            )?;
            validate_omnipair_reserve(
                *pair_info.key,
                &swap.token_out_mint,
                token_out_reserve_vault_info,
            )?;

            let expected_amount_out = quote_exact_output_for_pair_input(
                &pair,
                &rate_model,
                &futarchy_authority,
                &swap.token_in_mint,
                &swap.token_out_mint,
                swap.amount_in,
            )?;
            require!(
                expected_amount_out >= swap.min_amount_out,
                OmnindexError::InvalidRebalanceSwap
            );

            omnipair_swap(
                ctx.accounts.omnipair_program.to_account_info(),
                SwapAccounts {
                    pair: pair_info.clone(),
                    rate_model: rate_model_info.clone(),
                    futarchy_authority: ctx.accounts.omnipair_futarchy_authority.to_account_info(),
                    token_in_vault: token_in_reserve_vault_info.clone(),
                    token_out_vault: token_out_reserve_vault_info.clone(),
                    user_token_in_account: input_vault_info.clone(),
                    user_token_out_account: output_vault_info.clone(),
                    token_in_mint: input_mint_info.clone(),
                    token_out_mint: output_mint_info.clone(),
                    user: ctx.accounts.vault_authority.to_account_info(),
                    token_program: ctx.accounts.token_program.to_account_info(),
                    token_2022_program: ctx.accounts.token_2022_program.to_account_info(),
                    event_authority: ctx.accounts.omnipair_event_authority.to_account_info(),
                },
                SwapArgs {
                    amount_in: swap.amount_in,
                    min_amount_out: swap.min_amount_out,
                },
                &[signer_seeds],
            )?;
        }

        for account in &mint_accounts {
            let vault_info = &rebalance_accounts[account.vault_account_index];
            let vault_token_account = Account::<TokenAccount>::try_from(vault_info)?;
            validate_vault_token_account(
                &vault_token_account,
                &vault_info.key(),
                &ctx.accounts.vault_authority.key(),
                &account.mint,
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

fn validate_omnipair_reserve<'info>(
    pair: Pubkey,
    mint: &Pubkey,
    reserve_vault_info: &'info AccountInfo<'info>,
) -> Result<()> {
    require_keys_eq!(
        reserve_vault_info.key(),
        reserve_vault_address(&pair, mint),
        OmnindexError::InvalidOmnipairVault
    );
    let reserve_vault_account = Account::<TokenAccount>::try_from(reserve_vault_info)?;
    require_keys_eq!(
        reserve_vault_account.owner,
        pair,
        OmnindexError::InvalidOmnipairVault
    );
    require_keys_eq!(
        reserve_vault_account.mint,
        *mint,
        OmnindexError::InvalidOmnipairVault
    );
    Ok(())
}

fn create_component_vault_if_needed<'info>(
    ctx: &Context<'_, '_, 'info, 'info, ExecuteRebalance<'info>>,
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
        ctx.accounts.authority.to_account_info(),
        vault_info.clone(),
        ctx.accounts.vault_authority.to_account_info(),
        component_mint_info.clone(),
        ctx.accounts.system_program.to_account_info(),
        ctx.accounts.token_program.to_account_info(),
    )?;

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
}
