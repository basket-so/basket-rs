use anchor_lang::prelude::*;
use anchor_spl::token::Token;

use crate::{constants::VAULT_AUTHORITY_SEED, errors::OmnindexError, state::IndexState};

#[derive(Accounts)]
pub struct ClaimFees<'info> {
    pub authority: Signer<'info>,
    #[account(
        has_one = authority @ OmnindexError::UnauthorizedAuthority,
        has_one = index_mint @ OmnindexError::IndexMintMismatch
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
    pub token_program: Program<'info, Token>,
}

impl<'info> ClaimFees<'info> {
    pub fn handle(_ctx: Context<'_, '_, 'info, 'info, Self>) -> Result<()> {
        err!(OmnindexError::VaultSurplusClaimDisabled)
    }
}
