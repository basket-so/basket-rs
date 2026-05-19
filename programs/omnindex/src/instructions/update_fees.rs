use anchor_lang::prelude::*;

use crate::{
    constants::MAX_FEE_BPS, errors::OmnindexError, events::IndexFeesUpdated, state::IndexState,
};

#[derive(AnchorSerialize, AnchorDeserialize, Clone, Debug)]
pub struct UpdateFeesArgs {
    pub mint_fee_bps: u16,
    pub redeem_fee_bps: u16,
}

#[derive(Accounts)]
pub struct UpdateFees<'info> {
    pub authority: Signer<'info>,
    #[account(
        mut,
        has_one = authority @ OmnindexError::UnauthorizedAuthority
    )]
    pub index: Account<'info, IndexState>,
}

impl<'info> UpdateFees<'info> {
    pub fn handle(ctx: Context<Self>, args: UpdateFeesArgs) -> Result<()> {
        require!(
            args.mint_fee_bps <= MAX_FEE_BPS,
            OmnindexError::InvalidFeeBps
        );
        require!(
            args.redeem_fee_bps <= MAX_FEE_BPS,
            OmnindexError::InvalidFeeBps
        );

        let index = &mut ctx.accounts.index;
        index.mint_fee_bps = args.mint_fee_bps;
        index.redeem_fee_bps = args.redeem_fee_bps;

        emit!(IndexFeesUpdated {
            index: index.key(),
            authority: ctx.accounts.authority.key(),
            mint_fee_bps: index.mint_fee_bps,
            redeem_fee_bps: index.redeem_fee_bps,
        });

        Ok(())
    }
}
