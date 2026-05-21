use anchor_lang::prelude::*;
use anchor_spl::token::TokenAccount;

use crate::{
    constants::VAULT_AUTHORITY_SEED,
    errors::OmnindexError,
    events::{ComponentAmountQuote, IndexMintQuote, IndexRedeemQuote},
    state::IndexState,
    utils::{
        add_fee, load_mint, mint_component_backing_amount, redeem_component_backing_amount,
        subtract_fee, validate_pending_component_targets_integral, validate_vault_token_account,
    },
};

#[derive(AnchorSerialize, AnchorDeserialize, Clone, Debug)]
pub struct QuoteIndexArgs {
    pub amount: u64,
}

#[derive(Accounts)]
pub struct QuoteIndex<'info> {
    #[account(has_one = index_mint @ OmnindexError::IndexMintMismatch)]
    pub index: Account<'info, IndexState>,
    /// CHECK: Validated as the configured classic SPL Token mint before quoting.
    pub index_mint: UncheckedAccount<'info>,
    /// CHECK: PDA authority over the component vaults.
    #[account(
        seeds = [VAULT_AUTHORITY_SEED, index.key().as_ref()],
        bump = index.vault_authority_bump
    )]
    pub vault_authority: UncheckedAccount<'info>,
}

impl<'info> QuoteIndex<'info> {
    pub fn quote_mint(
        ctx: Context<'_, '_, 'info, 'info, Self>,
        args: QuoteIndexArgs,
    ) -> Result<()> {
        require!(args.amount > 0, OmnindexError::InvalidIndexAmount);

        let index = &ctx.accounts.index;
        let index_mint = load_mint(&ctx.accounts.index_mint.to_account_info())?;
        require!(!index.minting_paused, OmnindexError::MintingPaused);
        require!(
            index.mint_fee_bps == 0 && index.creator_mint_fee_bps == 0,
            OmnindexError::FeesRequireUsdcQuote
        );
        let current_supply = index_mint.supply;
        let post_supply = current_supply
            .checked_add(args.amount)
            .ok_or_else(|| error!(OmnindexError::ArithmeticOverflow))?;
        if index.max_supply > 0 {
            require!(
                post_supply <= index.max_supply,
                OmnindexError::SupplyCapExceeded
            );
        }
        validate_pending_component_targets_integral(index, post_supply)?;
        let components = mint_component_quotes(
            index,
            &ctx.accounts.vault_authority.key(),
            ctx.remaining_accounts,
            args.amount,
            current_supply,
        )?;

        emit!(IndexMintQuote {
            index: index.key(),
            amount: args.amount,
            mint_fee_bps: index.mint_fee_bps,
            components,
        });

        Ok(())
    }

    pub fn quote_redeem(
        ctx: Context<'_, '_, 'info, 'info, Self>,
        args: QuoteIndexArgs,
    ) -> Result<()> {
        require!(args.amount > 0, OmnindexError::InvalidIndexAmount);

        let index = &ctx.accounts.index;
        let index_mint = load_mint(&ctx.accounts.index_mint.to_account_info())?;
        require!(!index.redeeming_paused, OmnindexError::RedeemingPaused);
        require!(
            index.redeem_fee_bps == 0 && index.creator_redeem_fee_bps == 0,
            OmnindexError::FeesRequireUsdcQuote
        );
        let current_supply = index_mint.supply;
        let post_supply = current_supply
            .checked_sub(args.amount)
            .ok_or_else(|| error!(OmnindexError::InvalidIndexAmount))?;
        validate_pending_component_targets_integral(index, post_supply)?;
        let components = redeem_component_quotes(
            index,
            &ctx.accounts.vault_authority.key(),
            ctx.remaining_accounts,
            args.amount,
            current_supply,
        )?;

        emit!(IndexRedeemQuote {
            index: index.key(),
            amount: args.amount,
            redeem_fee_bps: index.redeem_fee_bps,
            components,
        });

        Ok(())
    }
}

fn mint_component_quotes<'info>(
    index: &IndexState,
    vault_authority: &Pubkey,
    remaining_accounts: &'info [AccountInfo<'info>],
    amount: u64,
    current_supply: u64,
) -> Result<Vec<ComponentAmountQuote>> {
    let base_units = index.index_base_units()?;
    let mut quotes = Vec::with_capacity(index.components.len());
    if current_supply > 0 {
        require!(
            remaining_accounts.len() == index.components.len(),
            OmnindexError::InvalidRemainingAccounts
        );
    } else {
        require!(
            remaining_accounts.is_empty(),
            OmnindexError::InvalidRemainingAccounts
        );
    }

    for (component_index, component) in index.components.iter().enumerate() {
        let vault_amount = if current_supply == 0 {
            0
        } else {
            let vault_info = &remaining_accounts[component_index];
            let vault_account = Account::<TokenAccount>::try_from(vault_info)?;
            validate_vault_token_account(
                &vault_account,
                &vault_info.key(),
                vault_authority,
                &component.mint,
            )?;
            vault_account.amount
        };
        let backing_amount = mint_component_backing_amount(
            component,
            amount,
            base_units,
            current_supply,
            vault_amount,
        )?;
        let (total_amount, fee_amount) = add_fee(backing_amount, index.mint_fee_bps)?;
        quotes.push(ComponentAmountQuote {
            mint: component.mint,
            gross_amount: total_amount,
            fee_amount,
            net_amount: backing_amount,
        });
    }

    Ok(quotes)
}

fn redeem_component_quotes<'info>(
    index: &IndexState,
    vault_authority: &Pubkey,
    remaining_accounts: &'info [AccountInfo<'info>],
    amount: u64,
    current_supply: u64,
) -> Result<Vec<ComponentAmountQuote>> {
    let mut quotes = Vec::with_capacity(index.components.len());
    require!(
        remaining_accounts.len() == index.components.len(),
        OmnindexError::InvalidRemainingAccounts
    );

    for (component, vault_info) in index.components.iter().zip(remaining_accounts.iter()) {
        let vault_account = Account::<TokenAccount>::try_from(vault_info)?;
        validate_vault_token_account(
            &vault_account,
            &vault_info.key(),
            vault_authority,
            &component.mint,
        )?;
        let backing_amount =
            redeem_component_backing_amount(amount, current_supply, vault_account.amount)?;
        let (net_amount, fee_amount) = subtract_fee(backing_amount, index.redeem_fee_bps)?;
        quotes.push(ComponentAmountQuote {
            mint: component.mint,
            gross_amount: backing_amount,
            fee_amount,
            net_amount,
        });
    }

    Ok(quotes)
}
