use anchor_lang::prelude::*;

use crate::{errors::OmnindexError, events::IndexRebalanceCancelled, state::IndexState};

#[derive(Accounts)]
pub struct CancelRebalance<'info> {
    pub authority: Signer<'info>,
    #[account(
        mut,
        has_one = authority @ OmnindexError::UnauthorizedAuthority
    )]
    pub index: Account<'info, IndexState>,
}

impl<'info> CancelRebalance<'info> {
    pub fn handle(ctx: Context<Self>) -> Result<()> {
        let index = &mut ctx.accounts.index;
        require!(
            index.pending_component_count > 0,
            OmnindexError::NoPendingRebalance
        );

        let nonce = index.pending_rebalance_nonce;
        index.pending_component_count = 0;
        index.pending_components = Vec::new();
        index.pending_rebalance_available_at = 0;
        index.pending_rebalance_quote_mint = Pubkey::default();
        index.pending_rebalance_oracle_price_tolerance_bps = 0;
        index.pending_rebalance_nav_tolerance_bps = 0;

        emit!(IndexRebalanceCancelled {
            index: index.key(),
            authority: ctx.accounts.authority.key(),
            nonce,
        });

        Ok(())
    }
}
