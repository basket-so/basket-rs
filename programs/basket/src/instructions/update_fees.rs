use anchor_lang::prelude::*;

use crate::{
    constants::MAX_TOTAL_INDEX_FEE_BPS, errors::BasketError, events::IndexFeesUpdated,
    state::IndexState,
};

#[derive(AnchorSerialize, AnchorDeserialize, Clone, Debug)]
pub struct UpdateFeesArgs {
    pub mint_fee_bps: u16,
    pub redeem_fee_bps: u16,
    pub creator_mint_fee_bps: u16,
    pub creator_redeem_fee_bps: u16,
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
        validate_fee_split(args.mint_fee_bps, args.creator_mint_fee_bps)?;
        validate_fee_split(args.redeem_fee_bps, args.creator_redeem_fee_bps)?;

        let index = &mut ctx.accounts.index;
        index.mint_fee_bps = args.mint_fee_bps;
        index.redeem_fee_bps = args.redeem_fee_bps;
        index.creator_mint_fee_bps = args.creator_mint_fee_bps;
        index.creator_redeem_fee_bps = args.creator_redeem_fee_bps;

        emit!(IndexFeesUpdated {
            index: index.key(),
            authority: ctx.accounts.authority.key(),
            mint_fee_bps: index.mint_fee_bps,
            redeem_fee_bps: index.redeem_fee_bps,
            creator_mint_fee_bps: index.creator_mint_fee_bps,
            creator_redeem_fee_bps: index.creator_redeem_fee_bps,
        });

        Ok(())
    }
}

fn validate_fee_split(protocol_fee_bps: u16, creator_fee_bps: u16) -> Result<()> {
    let total = protocol_fee_bps
        .checked_add(creator_fee_bps)
        .ok_or_else(|| error!(BasketError::InvalidFeeBps))?;
    require!(total <= MAX_TOTAL_INDEX_FEE_BPS, BasketError::InvalidFeeBps);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fee_split_allows_total_at_cap() {
        assert!(validate_fee_split(600, 400).is_ok());
    }

    #[test]
    fn fee_split_rejects_total_above_cap() {
        assert!(validate_fee_split(600, 401).is_err());
    }
}
