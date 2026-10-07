use anchor_lang::prelude::*;

use crate::{
    constants::{MAX_PRICES_PER_POST, PRICE_BOARD_SEED, PROTOCOL_CONFIG_SEED},
    errors::BasketError,
    events::PriceOracleSet,
    state::{PriceBoard, ProtocolConfig},
};

#[derive(AnchorSerialize, AnchorDeserialize, Clone, Debug)]
pub struct SetPriceOracleArgs {
    pub oracle: Pubkey,
}

/// Creates the price board on first use and sets the key allowed to post to it.
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
        space = 8 + PriceBoard::SPACE,
        seeds = [PRICE_BOARD_SEED],
        bump
    )]
    pub price_board: Account<'info, PriceBoard>,
    pub system_program: Program<'info, System>,
}

impl<'info> SetPriceOracle<'info> {
    pub fn handle(ctx: Context<Self>, args: SetPriceOracleArgs) -> Result<()> {
        require_keys_neq!(args.oracle, Pubkey::default(), BasketError::InvalidAuthority);
        let board = &mut ctx.accounts.price_board;
        let previous = board.oracle;
        board.oracle = args.oracle;
        board.bump = ctx.bumps.price_board;
        // Nothing the previous key posted stays readable.
        board.prices.clear();
        emit!(PriceOracleSet { previous, oracle: args.oracle });
        Ok(())
    }
}

#[derive(AnchorSerialize, AnchorDeserialize, Clone, Debug)]
pub struct PostedPrice {
    pub mint: Pubkey,
    /// USD per whole token, scaled by `PRICE_SCALE` (1e18).
    pub price: u128,
}

#[derive(AnchorSerialize, AnchorDeserialize, Clone, Debug)]
pub struct PostPricesArgs {
    pub prices: Vec<PostedPrice>,
}

/// The oracle signs; anyone (the keeper) may pay the fee.
#[derive(Accounts)]
pub struct PostPrices<'info> {
    pub oracle: Signer<'info>,
    #[account(
        mut,
        has_one = oracle @ BasketError::UnauthorizedAuthority,
        seeds = [PRICE_BOARD_SEED],
        bump = price_board.bump,
    )]
    pub price_board: Account<'info, PriceBoard>,
}

impl<'info> PostPrices<'info> {
    pub fn handle(ctx: Context<Self>, args: PostPricesArgs) -> Result<()> {
        require!(
            !args.prices.is_empty() && args.prices.len() <= MAX_PRICES_PER_POST,
            BasketError::InvalidOraclePrice
        );
        let slot = Clock::get()?.slot;
        let board = &mut ctx.accounts.price_board;
        for posted in &args.prices {
            board.post(posted.mint, posted.price, slot)?;
        }
        Ok(())
    }
}
