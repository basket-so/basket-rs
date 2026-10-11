use anchor_lang::prelude::*;

use super::{
    composition_change::restart_composition_notice,
    update_fees::{emit_fees_updated, validate_fee_update, UpdateFeesArgs},
};
use crate::{
    constants::{FEE_CHANGE_APPLY_WINDOW_SECONDS, FEE_CHANGE_DELAY_SECONDS, FEE_CHANGE_SEED},
    errors::BasketError,
    events::{FeeChangeApplied, FeeChangeCancelled, FeeChangeProposed},
    state::{FeeChange, IndexFees, IndexKind, IndexState},
    utils::total_redeem_fee_bps,
};

// A fee increase takes effect in two steps, like a composition change. The authority
// proposes the new fees; after FEE_CHANGE_DELAY_SECONDS it applies them, within
// FEE_CHANGE_APPLY_WINDOW_SECONDS or the proposal lapses. Holders who don't accept the new
// fees can redeem at the old ones in the meantime. Decreases need no notice (update_fees).

#[derive(Accounts)]
pub struct ProposeFeeChange<'info> {
    #[account(mut)]
    pub authority: Signer<'info>,
    #[account(has_one = authority @ BasketError::UnauthorizedAuthority)]
    pub index: Account<'info, IndexState>,
    #[account(
        init,
        payer = authority,
        space = 8 + FeeChange::SPACE,
        seeds = [FEE_CHANGE_SEED, index.key().as_ref()],
        bump
    )]
    pub fee_change: Account<'info, FeeChange>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct CancelFeeChange<'info> {
    pub authority: Signer<'info>,
    #[account(has_one = authority @ BasketError::UnauthorizedAuthority)]
    pub index: Account<'info, IndexState>,
    #[account(
        mut,
        has_one = index @ BasketError::InvalidFeeChange,
        has_one = proposer @ BasketError::InvalidFeeChange,
        seeds = [FEE_CHANGE_SEED, index.key().as_ref()],
        bump = fee_change.bump,
        close = proposer
    )]
    pub fee_change: Account<'info, FeeChange>,
    /// CHECK: Receives the proposal's rent; matched by has_one.
    #[account(mut)]
    pub proposer: UncheckedAccount<'info>,
}

#[derive(Accounts)]
pub struct ApplyFeeChange<'info> {
    pub authority: Signer<'info>,
    #[account(mut, has_one = authority @ BasketError::UnauthorizedAuthority)]
    pub index: Account<'info, IndexState>,
    #[account(
        mut,
        has_one = index @ BasketError::InvalidFeeChange,
        has_one = proposer @ BasketError::InvalidFeeChange,
        seeds = [FEE_CHANGE_SEED, index.key().as_ref()],
        bump = fee_change.bump,
        close = proposer
    )]
    pub fee_change: Account<'info, FeeChange>,
    /// CHECK: Receives the proposal's rent; matched by has_one.
    #[account(mut)]
    pub proposer: UncheckedAccount<'info>,
}

impl<'info> ProposeFeeChange<'info> {
    pub fn handle(ctx: Context<Self>, args: UpdateFeesArgs) -> Result<()> {
        let fees = IndexFees::from(&args);
        validate_fee_update(&ctx.accounts.index, &fees)?;
        require!(
            fees.raises_any(&ctx.accounts.index.fees()),
            BasketError::InvalidFeeChange
        );

        let now = Clock::get()?.unix_timestamp;
        let effective_at = now
            .checked_add(FEE_CHANGE_DELAY_SECONDS)
            .ok_or_else(|| error!(BasketError::ArithmeticOverflow))?;
        let change = &mut ctx.accounts.fee_change;
        change.index = ctx.accounts.index.key();
        change.proposer = ctx.accounts.authority.key();
        change.proposed_at = now;
        change.effective_at = effective_at;
        change.bump = ctx.bumps.fee_change;
        change.fees = fees;

        emit!(FeeChangeProposed {
            index: change.index,
            proposer: change.proposer,
            effective_at,
            fees,
        });
        Ok(())
    }
}

impl<'info> CancelFeeChange<'info> {
    pub fn handle(ctx: Context<Self>) -> Result<()> {
        emit!(FeeChangeCancelled {
            index: ctx.accounts.index.key(),
            authority: ctx.accounts.authority.key(),
        });
        Ok(())
    }
}

impl<'info> ApplyFeeChange<'info> {
    // remaining_accounts: when the total redeem fee changes, the basket's composition change
    // PDA (see restart_composition_notice).
    pub fn handle(ctx: Context<Self>) -> Result<()> {
        let now = Clock::get()?.unix_timestamp;
        let change = &ctx.accounts.fee_change;
        require!(now >= change.effective_at, BasketError::FeeChangeNotReady);
        let expires_at = change
            .effective_at
            .checked_add(FEE_CHANGE_APPLY_WINDOW_SECONDS)
            .ok_or_else(|| error!(BasketError::ArithmeticOverflow))?;
        require!(now <= expires_at, BasketError::FeeChangeExpired);
        let fees = change.fees;
        validate_fee_update(&ctx.accounts.index, &fees)?;

        // Only fixed-weight baskets can have composition changes.
        if ctx.accounts.index.kind == IndexKind::FixedWeights
            && fees.total_redeem_bps() != total_redeem_fee_bps(&ctx.accounts.index)
        {
            restart_composition_notice(&ctx.accounts.index.key(), ctx.program_id, ctx.remaining_accounts)?;
        }

        let index = &mut ctx.accounts.index;
        index.set_fees(&fees);
        emit_fees_updated(index, ctx.accounts.authority.key());
        emit!(FeeChangeApplied {
            index: index.key(),
            authority: ctx.accounts.authority.key(),
            fees,
        });
        Ok(())
    }
}
