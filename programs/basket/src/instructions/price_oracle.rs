use anchor_lang::prelude::*;

use crate::{
    constants::{PRICE_ORACLE_SEED, PROTOCOL_CONFIG_SEED},
    errors::BasketError,
    events::PriceOracleSet,
    state::{PriceOracle, ProtocolConfig},
};

#[derive(AnchorSerialize, AnchorDeserialize, Clone, Debug)]
pub struct SetPriceOracleArgs {
    pub oracle: Pubkey,
}

/// Creates the price oracle account on first use and sets the key whose signed prices
/// rebalances accept.
#[derive(Accounts)]
pub struct SetPriceOracle<'info> {
    #[account(mut)]
    pub authority: Signer<'info>,
    #[account(
        has_one = authority @ BasketError::UnauthorizedAuthority,
        seeds = [PROTOCOL_CONFIG_SEED],
        bump = protocol_config.bump,
    )]
    pub protocol_config: Account<'info, ProtocolConfig>,
    #[account(
        init_if_needed,
        payer = authority,
        space = 8 + PriceOracle::SPACE,
        seeds = [PRICE_ORACLE_SEED],
        bump
    )]
    pub price_oracle: Account<'info, PriceOracle>,
    pub system_program: Program<'info, System>,
}

impl<'info> SetPriceOracle<'info> {
    pub fn handle(ctx: Context<Self>, args: SetPriceOracleArgs) -> Result<()> {
        require_keys_neq!(args.oracle, Pubkey::default(), BasketError::InvalidAuthority);
        let account = &mut ctx.accounts.price_oracle;
        let previous = account.oracle;
        // Prices the previous key signed stop verifying with this.
        account.oracle = args.oracle;
        account.bump = ctx.bumps.price_oracle;
        emit!(PriceOracleSet { previous, oracle: args.oracle });
        Ok(())
    }
}
