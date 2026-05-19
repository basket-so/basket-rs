use anchor_lang::prelude::*;
use anchor_spl::token::{self, Burn, Token, TokenAccount, Transfer};

use crate::{
    constants::VAULT_AUTHORITY_SEED,
    errors::OmnindexError,
    events::IndexRedeemed,
    state::IndexState,
    utils::{
        load_mint, load_user_token_account, redeem_component_backing_amount,
        require_remaining_account_pairs, subtract_fee, validate_pending_component_targets_integral,
        validate_user_token_account, validate_vault_token_account,
    },
};

#[derive(AnchorSerialize, AnchorDeserialize, Clone, Debug)]
pub struct RedeemIndexArgs {
    pub amount: u64,
}

#[derive(Accounts)]
pub struct RedeemIndex<'info> {
    #[account(mut)]
    pub redeemer: Signer<'info>,
    #[account(mut, has_one = index_mint @ OmnindexError::IndexMintMismatch)]
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
    /// CHECK: Validated as the redeemer's index token account before burning.
    #[account(mut)]
    pub redeemer_index_token_account: UncheckedAccount<'info>,
    pub token_program: Program<'info, Token>,
}

impl<'info> RedeemIndex<'info> {
    pub fn handle(ctx: Context<'_, '_, 'info, 'info, Self>, args: RedeemIndexArgs) -> Result<()> {
        require!(args.amount > 0, OmnindexError::InvalidIndexAmount);

        let index = &ctx.accounts.index;
        require!(!index.redeeming_paused, OmnindexError::RedeemingPaused);
        require!(
            index.redeem_fee_bps == 0,
            OmnindexError::FeesRequireUsdcQuote
        );
        let index_mint = load_mint(&ctx.accounts.index_mint.to_account_info())?;
        let current_supply = index_mint.supply;
        let post_supply = current_supply
            .checked_sub(args.amount)
            .ok_or_else(|| error!(OmnindexError::InvalidIndexAmount))?;
        validate_pending_component_targets_integral(index, post_supply)?;
        let redeemer_index_token_account =
            load_user_token_account(&ctx.accounts.redeemer_index_token_account.to_account_info())?;
        validate_user_token_account(
            &redeemer_index_token_account,
            &ctx.accounts.redeemer.key(),
            &ctx.accounts.index_mint.key(),
        )?;
        require_remaining_account_pairs(ctx.remaining_accounts.len(), index.components.len())?;

        token::burn(
            CpiContext::new(
                ctx.accounts.token_program.to_account_info(),
                Burn {
                    mint: ctx.accounts.index_mint.to_account_info(),
                    from: ctx.accounts.redeemer_index_token_account.to_account_info(),
                    authority: ctx.accounts.redeemer.to_account_info(),
                },
            ),
            args.amount,
        )?;

        let signer_seeds: &[&[u8]] = &[
            VAULT_AUTHORITY_SEED,
            ctx.accounts.index.to_account_info().key.as_ref(),
            &[ctx.accounts.index.vault_authority_bump],
        ];

        for (component, accounts) in index
            .components
            .iter()
            .zip(ctx.remaining_accounts.chunks_exact(2))
        {
            let vault_token_info = &accounts[0];
            let user_token_info = &accounts[1];

            let vault_token_account = Account::<TokenAccount>::try_from(vault_token_info)?;
            let user_token_account = load_user_token_account(user_token_info)?;

            validate_vault_token_account(
                &vault_token_account,
                &vault_token_info.key(),
                &ctx.accounts.vault_authority.key(),
                &component.mint,
            )?;
            validate_user_token_account(
                &user_token_account,
                &ctx.accounts.redeemer.key(),
                &component.mint,
            )?;

            let component_amount = redeem_component_backing_amount(
                args.amount,
                current_supply,
                vault_token_account.amount,
            )?;
            let (redeem_amount, _) = subtract_fee(component_amount, index.redeem_fee_bps)?;

            if redeem_amount > 0 {
                token::transfer(
                    CpiContext::new_with_signer(
                        ctx.accounts.token_program.to_account_info(),
                        Transfer {
                            from: vault_token_info.clone(),
                            to: user_token_info.clone(),
                            authority: ctx.accounts.vault_authority.to_account_info(),
                        },
                        &[signer_seeds],
                    ),
                    redeem_amount,
                )?;
            }
        }

        emit!(IndexRedeemed {
            index: ctx.accounts.index.key(),
            redeemer: ctx.accounts.redeemer.key(),
            amount: args.amount,
        });

        Ok(())
    }
}
