use anchor_lang::prelude::*;

use super::composition_change::restart_composition_notice;
use crate::{
    errors::BasketError, events::IndexFeesUpdated, state::{IndexKind, IndexState},
    utils::{total_redeem_fee_bps, validate_total_index_fee_bps},
};

#[derive(AnchorSerialize, AnchorDeserialize, Clone, Debug)]
pub struct UpdateFeesArgs {
    pub mint_fee_bps: u16,
    pub redeem_fee_bps: u16,
    pub creator_mint_fee_bps: u16,
    pub creator_redeem_fee_bps: u16,
    pub staking_mint_fee_bps: u16,
    pub staking_redeem_fee_bps: u16,
}

#[derive(Accounts)]
pub struct UpdateFees<'info> {
    pub authority: Signer<'info>,
    #[account(
        mut,
        has_one = authority @ BasketError::UnauthorizedAuthority
    )]
    pub index: Account<'info, IndexState>,
}

impl<'info> UpdateFees<'info> {
    pub fn handle(ctx: Context<Self>, args: UpdateFeesArgs) -> Result<()> {
        validate_fee_split(
            args.mint_fee_bps,
            args.creator_mint_fee_bps,
            args.staking_mint_fee_bps,
        )?;
        validate_fee_split(
            args.redeem_fee_bps,
            args.creator_redeem_fee_bps,
            args.staking_redeem_fee_bps,
        )?;

        // remaining_accounts: when the total redeem fee changes, the basket's composition
        // change PDA (see restart_composition_notice).
        let redeem_fee_bps = args
            .redeem_fee_bps
            .saturating_add(args.creator_redeem_fee_bps)
            .saturating_add(args.staking_redeem_fee_bps);
        // Only fixed-weight baskets can have composition changes.
        if ctx.accounts.index.kind == IndexKind::FixedWeights
            && redeem_fee_bps != total_redeem_fee_bps(&ctx.accounts.index)
        {
            restart_composition_notice(&ctx.accounts.index.key(), ctx.program_id, ctx.remaining_accounts)?;
        }

        let index = &mut ctx.accounts.index;
        index.mint_fee_bps = args.mint_fee_bps;
        index.redeem_fee_bps = args.redeem_fee_bps;
        index.creator_mint_fee_bps = args.creator_mint_fee_bps;
        index.creator_redeem_fee_bps = args.creator_redeem_fee_bps;
        index.staking_mint_fee_bps = args.staking_mint_fee_bps;
        index.staking_redeem_fee_bps = args.staking_redeem_fee_bps;

        emit!(IndexFeesUpdated {
            index: index.key(),
            authority: ctx.accounts.authority.key(),
            mint_fee_bps: index.mint_fee_bps,
            redeem_fee_bps: index.redeem_fee_bps,
            creator_mint_fee_bps: index.creator_mint_fee_bps,
            creator_redeem_fee_bps: index.creator_redeem_fee_bps,
            staking_mint_fee_bps: index.staking_mint_fee_bps,
            staking_redeem_fee_bps: index.staking_redeem_fee_bps,
        });

        Ok(())
    }
}

fn validate_fee_split(
    protocol_fee_bps: u16,
    creator_fee_bps: u16,
    staking_fee_bps: u16,
) -> Result<()> {
    validate_total_index_fee_bps(protocol_fee_bps, creator_fee_bps, staking_fee_bps)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fee_split_allows_total_at_cap() {
        assert!(validate_fee_split(500, 300, 200).is_ok());
    }

    #[test]
    fn fee_split_rejects_total_above_cap() {
        assert!(validate_fee_split(500, 300, 201).is_err());
    }
}
