use anchor_lang::prelude::*;

use super::composition_change::restart_composition_notice;
use crate::{
    constants::MAX_REBALANCE_DELAY_SECONDS, errors::BasketError, events::IndexConfigUpdated,
    state::{IndexKind, IndexState},
};

#[derive(AnchorSerialize, AnchorDeserialize, Clone, Debug)]
pub struct UpdateConfigArgs {
    pub fee_recipient: Pubkey,
    pub creator_fee_recipient: Pubkey,
    pub max_supply: u64,
    pub rebalance_delay_seconds: i64,
    pub minting_paused: bool,
    pub redeeming_paused: bool,
    pub rebalancing_paused: bool,
}

#[derive(Accounts)]
pub struct UpdateConfig<'info> {
    pub authority: Signer<'info>,
    #[account(
        mut,
        has_one = authority @ BasketError::UnauthorizedAuthority
    )]
    pub index: Account<'info, IndexState>,
}

impl<'info> UpdateConfig<'info> {
    // remaining_accounts: when pausing or unpausing redemptions, the basket's composition
    // change PDA (see restart_composition_notice).
    pub fn handle(ctx: Context<Self>, args: UpdateConfigArgs) -> Result<()> {
        require!(
            args.fee_recipient != Pubkey::default(),
            BasketError::InvalidAuthority
        );
        require!(
            args.rebalance_delay_seconds >= 0
                && args.rebalance_delay_seconds <= MAX_REBALANCE_DELAY_SECONDS,
            BasketError::InvalidRebalanceDelay
        );

        // Only fixed-weight baskets can have composition changes.
        if ctx.accounts.index.kind == IndexKind::FixedWeights
            && args.redeeming_paused != ctx.accounts.index.redeeming_paused
        {
            restart_composition_notice(&ctx.accounts.index.key(), ctx.program_id, ctx.remaining_accounts)?;
        }

        let index = &mut ctx.accounts.index;
        index.fee_recipient = args.fee_recipient;
        index.creator_fee_recipient = args.creator_fee_recipient;
        index.max_supply = args.max_supply;
        index.rebalance_delay_seconds = args.rebalance_delay_seconds;
        index.minting_paused = args.minting_paused;
        index.redeeming_paused = args.redeeming_paused;
        index.rebalancing_paused = args.rebalancing_paused;

        emit!(IndexConfigUpdated {
            index: index.key(),
            authority: ctx.accounts.authority.key(),
            fee_recipient: index.fee_recipient,
            creator_fee_recipient: index.creator_fee_recipient,
            max_supply: index.max_supply,
            rebalance_delay_seconds: index.rebalance_delay_seconds,
            minting_paused: index.minting_paused,
            redeeming_paused: index.redeeming_paused,
            rebalancing_paused: index.rebalancing_paused,
        });

        Ok(())
    }
}
