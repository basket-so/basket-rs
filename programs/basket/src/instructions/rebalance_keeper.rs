use anchor_lang::prelude::*;

use crate::{
    constants::{REBALANCE_REQUEST_COOLDOWN_SECONDS, REBALANCE_REQUEST_WINDOW_SECONDS},
    errors::BasketError,
    events::{RebalanceKeeperUpdated, RebalanceRequestUpdated},
    state::{IndexKind, IndexState},
};

#[derive(AnchorSerialize, AnchorDeserialize, Clone, Debug)]
pub struct SetRebalanceKeeperArgs {
    /// Pubkey::default() removes the keeper, leaving rebalances to the authority.
    pub keeper: Pubkey,
}

#[derive(Accounts)]
pub struct SetRebalanceKeeper<'info> {
    pub authority: Signer<'info>,
    #[account(mut, has_one = authority @ BasketError::UnauthorizedAuthority)]
    pub index: Account<'info, IndexState>,
}

impl<'info> SetRebalanceKeeper<'info> {
    pub fn handle(ctx: Context<Self>, args: SetRebalanceKeeperArgs) -> Result<()> {
        let index = &mut ctx.accounts.index;
        index.rebalance_keeper = args.keeper;
        emit!(RebalanceKeeperUpdated {
            index: index.key(),
            keeper: args.keeper,
        });
        Ok(())
    }
}

#[derive(Accounts)]
pub struct UpdateRebalanceRequest<'info> {
    pub operator: Signer<'info>,
    #[account(mut)]
    pub index: Account<'info, IndexState>,
}

impl<'info> UpdateRebalanceRequest<'info> {
    /// Holds back new mint/redeem intents so the open ones can settle or expire and a
    /// rebalance can open. The hold lapses on its own after REBALANCE_REQUEST_WINDOW_SECONDS.
    pub fn handle_request(ctx: Context<Self>) -> Result<()> {
        let now = Clock::get()?.unix_timestamp;
        let index = &mut ctx.accounts.index;
        require!(
            index.is_rebalance_operator(&ctx.accounts.operator.key()),
            BasketError::NotRebalanceOperator
        );
        require!(index.kind == IndexKind::FixedWeights, BasketError::InvalidIndexKind);
        require!(!index.rebalancing_paused, BasketError::RebalancingPaused);
        require!(
            !index.large_basket_operation_in_progress,
            BasketError::InvalidLargeBasketIntent
        );
        let next_request_at = index
            .rebalance_requested_at
            .saturating_add(REBALANCE_REQUEST_WINDOW_SECONDS)
            .saturating_add(REBALANCE_REQUEST_COOLDOWN_SECONDS);
        require!(now >= next_request_at, BasketError::RebalanceRequestCooldown);
        index.rebalance_requested = true;
        index.rebalance_requested_at = now;
        emit!(RebalanceRequestUpdated {
            index: index.key(),
            operator: ctx.accounts.operator.key(),
            requested: true,
            requested_at: now,
        });
        Ok(())
    }

    pub fn handle_cancel(ctx: Context<Self>) -> Result<()> {
        let index = &mut ctx.accounts.index;
        require!(
            index.is_rebalance_operator(&ctx.accounts.operator.key()),
            BasketError::NotRebalanceOperator
        );
        index.rebalance_requested = false;
        emit!(RebalanceRequestUpdated {
            index: index.key(),
            operator: ctx.accounts.operator.key(),
            requested: false,
            requested_at: index.rebalance_requested_at,
        });
        Ok(())
    }
}
