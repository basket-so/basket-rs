use anchor_lang::prelude::*;

use crate::{
    constants::PROTOCOL_CONFIG_SEED, errors::OmnindexError, events::ProtocolConfigUpdated,
    state::ProtocolConfig,
};

#[derive(AnchorSerialize, AnchorDeserialize, Clone, Debug)]
pub struct UpdateProtocolConfigArgs {
    pub authority: Pubkey,
    pub index_creator: Pubkey,
    pub permissionless_index_creation: bool,
}

#[derive(Accounts)]
pub struct UpdateProtocolConfig<'info> {
    pub authority: Signer<'info>,
    #[account(
        mut,
        has_one = authority @ OmnindexError::UnauthorizedAuthority,
        seeds = [PROTOCOL_CONFIG_SEED],
        bump = protocol_config.bump,
    )]
    pub protocol_config: Account<'info, ProtocolConfig>,
}

impl<'info> UpdateProtocolConfig<'info> {
    pub fn handle(ctx: Context<Self>, args: UpdateProtocolConfigArgs) -> Result<()> {
        require!(
            args.authority != Pubkey::default(),
            OmnindexError::InvalidAuthority
        );
        require!(
            args.index_creator != Pubkey::default(),
            OmnindexError::InvalidAuthority
        );

        let protocol_config = &mut ctx.accounts.protocol_config;
        let old_authority = protocol_config.authority;
        let old_index_creator = protocol_config.index_creator;
        let old_permissionless_index_creation = protocol_config.permissionless_index_creation;

        protocol_config.authority = args.authority;
        protocol_config.index_creator = args.index_creator;
        protocol_config.permissionless_index_creation = args.permissionless_index_creation;

        emit!(ProtocolConfigUpdated {
            old_authority,
            new_authority: protocol_config.authority,
            old_index_creator,
            new_index_creator: protocol_config.index_creator,
            old_permissionless_index_creation,
            new_permissionless_index_creation: protocol_config.permissionless_index_creation,
        });

        Ok(())
    }
}
