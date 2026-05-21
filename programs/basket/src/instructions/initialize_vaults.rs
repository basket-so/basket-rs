use anchor_lang::prelude::*;
use anchor_spl::token::Token;

use crate::{
    constants::VAULT_AUTHORITY_SEED,
    errors::BasketError,
    state::IndexState,
    utils::{
        associated_token_address, create_associated_token_account_idempotent,
        require_remaining_account_pairs, ASSOCIATED_TOKEN_ID,
    },
};

#[derive(Accounts)]
pub struct InitializeVaults<'info> {
    #[account(mut)]
    pub payer: Signer<'info>,
    pub index: Account<'info, IndexState>,
    /// CHECK: PDA authority over all component vault ATAs.
    #[account(
        seeds = [VAULT_AUTHORITY_SEED, index.key().as_ref()],
        bump = index.vault_authority_bump
    )]
    pub vault_authority: UncheckedAccount<'info>,
    /// CHECK: Validated as the Associated Token Program.
    #[account(address = ASSOCIATED_TOKEN_ID @ BasketError::InvalidAssociatedTokenProgram)]
    pub associated_token_program: UncheckedAccount<'info>,
    pub token_program: Program<'info, Token>,
    pub system_program: Program<'info, System>,
}

impl<'info> InitializeVaults<'info> {
    pub fn handle(ctx: Context<'_, '_, 'info, 'info, Self>) -> Result<()> {
        let components = &ctx.accounts.index.components;
        require_remaining_account_pairs(ctx.remaining_accounts.len(), components.len())?;

        for (component, accounts) in components
            .iter()
            .zip(ctx.remaining_accounts.chunks_exact(2))
        {
            let component_mint_info = &accounts[0];
            let vault_info = &accounts[1];

            require_keys_eq!(
                component_mint_info.key(),
                component.mint,
                BasketError::InvalidComponentMint
            );

            let expected_vault =
                associated_token_address(&ctx.accounts.vault_authority.key(), &component.mint);
            require_keys_eq!(
                vault_info.key(),
                expected_vault,
                BasketError::InvalidVaultAccount
            );

            create_associated_token_account_idempotent(
                ctx.accounts.associated_token_program.to_account_info(),
                ctx.accounts.payer.to_account_info(),
                vault_info.clone(),
                ctx.accounts.vault_authority.to_account_info(),
                component_mint_info.clone(),
                ctx.accounts.system_program.to_account_info(),
                ctx.accounts.token_program.to_account_info(),
            )?;
        }

        Ok(())
    }
}
