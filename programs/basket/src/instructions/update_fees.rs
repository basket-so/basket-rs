use anchor_lang::prelude::*;

use super::composition_change::restart_composition_notice;
use crate::{
    errors::BasketError,
    events::IndexFeesUpdated,
    state::{IndexFees, IndexKind, IndexState},
    utils::{load_mint, total_redeem_fee_bps},
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

impl From<&UpdateFeesArgs> for IndexFees {
    fn from(args: &UpdateFeesArgs) -> Self {
        IndexFees {
            mint_fee_bps: args.mint_fee_bps,
            redeem_fee_bps: args.redeem_fee_bps,
            creator_mint_fee_bps: args.creator_mint_fee_bps,
            creator_redeem_fee_bps: args.creator_redeem_fee_bps,
            staking_mint_fee_bps: args.staking_mint_fee_bps,
            staking_redeem_fee_bps: args.staking_redeem_fee_bps,
        }
    }
}

#[derive(Accounts)]
pub struct UpdateFees<'info> {
    pub authority: Signer<'info>,
    #[account(
        mut,
        has_one = authority @ BasketError::UnauthorizedAuthority,
        has_one = index_mint @ BasketError::IndexMintMismatch
    )]
    pub index: Account<'info, IndexState>,
    /// CHECK: Matched by has_one; its supply decides whether an increase needs notice.
    pub index_mint: UncheckedAccount<'info>,
}

impl<'info> UpdateFees<'info> {
    /// Lowers fees at once. Raising any fee needs notice (propose_fee_change), unless the
    /// basket has no supply yet and so no holders to give notice to.
    pub fn handle(ctx: Context<Self>, args: UpdateFeesArgs) -> Result<()> {
        let fees = IndexFees::from(&args);
        let current = ctx.accounts.index.fees();
        validate_fee_update(&ctx.accounts.index, &fees)?;
        let supply = load_mint(&ctx.accounts.index_mint.to_account_info())?.supply;
        require!(
            supply == 0 || !fees.raises_any(&current),
            BasketError::FeeIncreaseNeedsNotice
        );

        // remaining_accounts: when the total redeem fee changes, the basket's composition
        // change PDA (see restart_composition_notice).
        // Only fixed-weight baskets can have composition changes.
        if ctx.accounts.index.kind == IndexKind::FixedWeights
            && fees.total_redeem_bps() != total_redeem_fee_bps(&ctx.accounts.index)
        {
            restart_composition_notice(&ctx.accounts.index.key(), ctx.program_id, ctx.remaining_accounts)?;
        }

        let index = &mut ctx.accounts.index;
        index.set_fees(&fees);
        emit_fees_updated(index, ctx.accounts.authority.key());
        Ok(())
    }
}

/// Checks a basket's new fees: each side within the cap, and for a community basket the
/// split it was created with (a whole number of its fee steps, at least one).
pub fn validate_fee_update(index: &IndexState, fees: &IndexFees) -> Result<()> {
    fees.validate()?;
    require!(
        !index.community || fees.same_split_as(&index.fees()),
        BasketError::CommunityFeeSplit
    );
    Ok(())
}

pub fn emit_fees_updated(index: &Account<IndexState>, authority: Pubkey) {
    emit!(IndexFeesUpdated {
        index: index.key(),
        authority,
        mint_fee_bps: index.mint_fee_bps,
        redeem_fee_bps: index.redeem_fee_bps,
        creator_mint_fee_bps: index.creator_mint_fee_bps,
        creator_redeem_fee_bps: index.creator_redeem_fee_bps,
        staking_mint_fee_bps: index.staking_mint_fee_bps,
        staking_redeem_fee_bps: index.staking_redeem_fee_bps,
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fees(protocol: u16, creator: u16, staking: u16) -> IndexFees {
        IndexFees {
            mint_fee_bps: protocol,
            redeem_fee_bps: protocol,
            creator_mint_fee_bps: creator,
            creator_redeem_fee_bps: creator,
            staking_mint_fee_bps: staking,
            staking_redeem_fee_bps: staking,
        }
    }

    #[test]
    fn fee_split_allows_total_at_one_percent() {
        assert!(fees(50, 30, 20).validate().is_ok());
    }

    #[test]
    fn fee_split_rejects_total_above_one_percent() {
        assert!(fees(50, 30, 21).validate().is_err());
        let mut one_side = fees(0, 0, 0);
        one_side.redeem_fee_bps = 101;
        assert!(one_side.validate().is_err());
    }

    #[test]
    fn any_higher_rate_is_an_increase() {
        let current = fees(4, 1, 5);
        assert!(!fees(4, 1, 5).raises_any(&current));
        assert!(!fees(0, 0, 0).raises_any(&current));
        let mut creator_redeem_up = current;
        creator_redeem_up.creator_redeem_fee_bps = 2;
        creator_redeem_up.mint_fee_bps = 0;
        assert!(creator_redeem_up.raises_any(&current));
    }

    #[test]
    fn community_fees_keep_their_split() {
        let current = fees(4, 1, 5);
        assert!(fees(4, 1, 5).same_split_as(&current));
        assert!(fees(8, 2, 10).same_split_as(&current));
        assert!(fees(40, 10, 50).same_split_as(&current));
        assert!(fees(8, 2, 10).same_split_as(&fees(12, 3, 15)));
        assert!(!fees(0, 0, 0).same_split_as(&current), "a fee of zero loses the split");
        assert!(!fees(4, 3, 5).same_split_as(&current), "the creator alone");
        assert!(!fees(5, 1, 4).same_split_as(&current), "shifted toward the treasury");
        let mut one_side = fees(8, 2, 10);
        one_side.creator_redeem_fee_bps = 3;
        assert!(!one_side.same_split_as(&current), "each side keeps it");
    }
}
