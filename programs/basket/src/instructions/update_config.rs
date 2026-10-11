use anchor_lang::prelude::*;

use crate::{
    constants::MAX_REBALANCE_DELAY_SECONDS, errors::BasketError, events::IndexConfigUpdated,
    state::IndexState,
};

// There is no redeem pause: a redeem only takes its own share of the vaults, so stopping
// redeems protects no other holder and would only trap them.
#[derive(AnchorSerialize, AnchorDeserialize, Clone, Debug)]
pub struct UpdateConfigArgs {
    pub fee_recipient: Pubkey,
    pub creator_fee_recipient: Pubkey,
    pub max_supply: u64,
    pub rebalance_delay_seconds: i64,
    pub minting_paused: bool,
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
        // A community basket's creator only chooses where its creator fee goes. The protocol
        // fee recipient, supply cap and pauses are the protocol's (update_community_basket):
        // a creator who could pause mints or rebalances, or cap supply, could hold everyone
        // else back.
        let index = &ctx.accounts.index;
        require!(
            !index.community
                || (args.fee_recipient == index.fee_recipient
                    && args.max_supply == index.max_supply
                    && args.rebalance_delay_seconds == index.rebalance_delay_seconds
                    && args.minting_paused == index.minting_paused
                    && args.rebalancing_paused == index.rebalancing_paused),
            BasketError::CommunityBasketRestricted
        );

        let index = &mut ctx.accounts.index;
        index.fee_recipient = args.fee_recipient;
        index.creator_fee_recipient = args.creator_fee_recipient;
        index.max_supply = args.max_supply;
        index.rebalance_delay_seconds = args.rebalance_delay_seconds;
        index.minting_paused = args.minting_paused;
        index.rebalancing_paused = args.rebalancing_paused;
        emit_config_updated(index, ctx.accounts.authority.key());
        Ok(())
    }
}

pub fn emit_config_updated(index: &Account<IndexState>, authority: Pubkey) {
    emit!(IndexConfigUpdated {
        index: index.key(),
        authority,
        fee_recipient: index.fee_recipient,
        creator_fee_recipient: index.creator_fee_recipient,
        max_supply: index.max_supply,
        rebalance_delay_seconds: index.rebalance_delay_seconds,
        minting_paused: index.minting_paused,
        rebalancing_paused: index.rebalancing_paused,
    });
}
