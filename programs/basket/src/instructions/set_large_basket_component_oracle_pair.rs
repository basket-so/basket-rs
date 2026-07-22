use anchor_lang::prelude::*;

use crate::{
    constants::LARGE_BASKET_COMPONENT_PAGE_SEED,
    errors::BasketError,
    events::LargeBasketComponentOraclePairUpdated,
    state::{IndexState, LargeBasketComponentPage},
    utils::load_mint,
};

#[derive(AnchorSerialize, AnchorDeserialize, Clone, Debug)]
pub struct SetLargeBasketComponentOraclePairArgs {
    pub page_index: u8,
    pub component_index: u16,
    pub component_mint: Pubkey,
    pub oracle_pair: Pubkey,
}

#[derive(Accounts)]
#[instruction(args: SetLargeBasketComponentOraclePairArgs)]
pub struct SetLargeBasketComponentOraclePair<'info> {
    pub authority: Signer<'info>,
    #[account(
        has_one = authority @ BasketError::UnauthorizedAuthority,
        has_one = index_mint @ BasketError::IndexMintMismatch
    )]
    pub index: Account<'info, IndexState>,
    /// CHECK: Validated as the configured classic SPL index mint; only read to assert zero supply.
    pub index_mint: UncheckedAccount<'info>,
    #[account(
        mut,
        seeds = [
            LARGE_BASKET_COMPONENT_PAGE_SEED,
            index.key().as_ref(),
            &[args.page_index],
        ],
        bump = page.bump
    )]
    pub page: Account<'info, LargeBasketComponentPage>,
}

impl<'info> SetLargeBasketComponentOraclePair<'info> {
    pub fn handle(ctx: Context<Self>, args: SetLargeBasketComponentOraclePairArgs) -> Result<()> {
        require!(
            ctx.accounts.index.large_basket_configured,
            BasketError::LargeBasketNotConfigured
        );
        // Re-pointing a component's oracle feed shifts the price the program
        // verifies mint/redeem execution against. Restrict it to before any
        // index tokens exist and while no intent is mid-flight so it can never
        // change the oracle out from under a holder or an open intent.
        require!(
            !ctx.accounts.index.large_basket_operation_in_progress,
            BasketError::InvalidLargeBasketIntent
        );
        let index_mint = load_mint(&ctx.accounts.index_mint.to_account_info())?;
        require!(
            index_mint.supply == 0,
            BasketError::LargeBasketOracleUpdateRequiresZeroSupply
        );
        require_keys_neq!(
            args.oracle_pair,
            Pubkey::default(),
            BasketError::InvalidLargeBasketComponentPage
        );

        let page = &mut ctx.accounts.page;
        require!(page.finalized, BasketError::InvalidLargeBasketComponentPage);
        let offset = page.component_offset(args.component_index)?;
        let component = &mut page.components[offset];
        require_keys_eq!(
            component.mint,
            args.component_mint,
            BasketError::InvalidComponentMint
        );
        component.oracle_pair = args.oracle_pair;

        emit!(LargeBasketComponentOraclePairUpdated {
            index: ctx.accounts.index.key(),
            authority: ctx.accounts.authority.key(),
            page: ctx.accounts.page.key(),
            component_index: args.component_index,
            component_mint: args.component_mint,
            oracle_pair: args.oracle_pair,
        });

        Ok(())
    }
}
