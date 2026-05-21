use anchor_lang::prelude::*;

use crate::{errors::BasketError, events::IndexAuthorityUpdated, state::IndexState};

#[derive(AnchorSerialize, AnchorDeserialize, Clone, Debug)]
pub struct UpdateAuthorityArgs {
    pub new_authority: Pubkey,
}

#[derive(Accounts)]
pub struct UpdateAuthority<'info> {
    pub authority: Signer<'info>,
    #[account(
        mut,
        has_one = authority @ BasketError::UnauthorizedAuthority
    )]
    pub index: Account<'info, IndexState>,
}

impl<'info> UpdateAuthority<'info> {
    pub fn handle(ctx: Context<Self>, args: UpdateAuthorityArgs) -> Result<()> {
        require!(
            args.new_authority != Pubkey::default(),
            BasketError::InvalidAuthority
        );

        let index = &mut ctx.accounts.index;
        let old_authority = index.authority;
        index.authority = args.new_authority;

        emit!(IndexAuthorityUpdated {
            index: index.key(),
            old_authority,
            new_authority: index.authority,
        });

        Ok(())
    }
}
