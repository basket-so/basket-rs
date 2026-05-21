use anchor_lang::prelude::*;

use crate::{
    constants::{MAX_INDEX_CREATOR_WHITELIST, PROTOCOL_CONFIG_SEED},
    errors::OmnindexError,
    events::IndexCreatorWhitelistUpdated,
    state::ProtocolConfig,
};

#[derive(AnchorSerialize, AnchorDeserialize, Clone, Debug)]
pub struct UpdateIndexCreatorWhitelistArgs {
    pub creator: Pubkey,
    pub whitelisted: bool,
}

#[derive(Accounts)]
pub struct UpdateIndexCreatorWhitelist<'info> {
    pub authority: Signer<'info>,
    #[account(
        mut,
        has_one = authority @ OmnindexError::UnauthorizedAuthority,
        seeds = [PROTOCOL_CONFIG_SEED],
        bump = protocol_config.bump,
    )]
    pub protocol_config: Account<'info, ProtocolConfig>,
}

impl<'info> UpdateIndexCreatorWhitelist<'info> {
    pub fn handle(ctx: Context<Self>, args: UpdateIndexCreatorWhitelistArgs) -> Result<()> {
        require!(
            args.creator != Pubkey::default(),
            OmnindexError::InvalidAuthority
        );

        let whitelist = &mut ctx.accounts.protocol_config.index_creator_whitelist;
        let existing_index = whitelist
            .iter()
            .position(|creator| *creator == args.creator);

        if args.whitelisted {
            if existing_index.is_none() {
                require!(
                    whitelist.len() < MAX_INDEX_CREATOR_WHITELIST,
                    OmnindexError::IndexCreatorWhitelistFull
                );
                whitelist.push(args.creator);
            }
        } else if let Some(existing_index) = existing_index {
            whitelist.remove(existing_index);
        }

        emit!(IndexCreatorWhitelistUpdated {
            authority: ctx.accounts.authority.key(),
            creator: args.creator,
            whitelisted: args.whitelisted,
        });

        Ok(())
    }
}
