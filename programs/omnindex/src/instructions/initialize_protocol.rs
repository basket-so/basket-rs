use anchor_lang::prelude::*;

use crate::{
    constants::PROTOCOL_CONFIG_SEED, errors::OmnindexError, events::ProtocolConfigInitialized,
    program::Omnindex, state::ProtocolConfig,
};

#[derive(AnchorSerialize, AnchorDeserialize, Clone, Debug)]
pub struct InitializeProtocolArgs {
    pub index_creator: Pubkey,
}

#[derive(Accounts)]
pub struct InitializeProtocol<'info> {
    #[account(mut)]
    pub payer: Signer<'info>,
    pub authority: Signer<'info>,
    #[account(
        constraint = program.programdata_address()? == Some(program_data.key())
            @ OmnindexError::InvalidProgramData
    )]
    pub program: Program<'info, Omnindex>,
    #[account(
        constraint = program_data.upgrade_authority_address == Some(authority.key())
            @ OmnindexError::UnauthorizedAuthority
    )]
    pub program_data: Account<'info, ProgramData>,
    #[account(
        init,
        payer = payer,
        seeds = [PROTOCOL_CONFIG_SEED],
        bump,
        space = 8 + ProtocolConfig::SPACE,
    )]
    pub protocol_config: Account<'info, ProtocolConfig>,
    pub system_program: Program<'info, System>,
}

impl<'info> InitializeProtocol<'info> {
    pub fn handle(ctx: Context<Self>, args: InitializeProtocolArgs) -> Result<()> {
        require!(
            ctx.accounts.authority.key() != Pubkey::default(),
            OmnindexError::InvalidAuthority
        );

        let index_creator = if args.index_creator == Pubkey::default() {
            ctx.accounts.authority.key()
        } else {
            args.index_creator
        };

        require!(
            index_creator != Pubkey::default(),
            OmnindexError::InvalidAuthority
        );

        let protocol_config = &mut ctx.accounts.protocol_config;
        protocol_config.authority = ctx.accounts.authority.key();
        protocol_config.index_creator = index_creator;
        protocol_config.permissionless_index_creation = false;
        protocol_config.bump = ctx.bumps.protocol_config;
        protocol_config.reserved = [0; 30];
        protocol_config.index_creator_whitelist = Vec::new();

        emit!(ProtocolConfigInitialized {
            authority: protocol_config.authority,
            index_creator: protocol_config.index_creator,
            permissionless_index_creation: protocol_config.permissionless_index_creation,
        });

        Ok(())
    }
}
