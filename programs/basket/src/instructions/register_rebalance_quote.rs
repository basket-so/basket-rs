use anchor_lang::prelude::*;
use anchor_spl::token::Token;
use crate::{
    constants::{LARGE_BASKET_COMPONENT_PAGE_SEED, MAX_LARGE_BASKET_COMPONENTS,
        MAX_LARGE_BASKET_COMPONENTS_PER_PAGE, USDC_MINT, VAULT_AUTHORITY_SEED},
    errors::BasketError,
    state::{IndexKind, IndexState, LargeBasketComponent, LargeBasketComponentPage},
    utils::{associated_token_address, load_mint, load_interface_token_account, units_per_index_for_amount_saturating,
        create_associated_token_account_idempotent, ASSOCIATED_TOKEN_ID},
};

// Append the existing scratch ATA as a zero-weight component. This is a
// permissionless accounting migration: neither strategy weights nor funds move.
#[derive(Accounts)]
pub struct RegisterRebalanceQuote<'info> {
    #[account(mut)]
    pub payer: Signer<'info>,
    #[account(mut, has_one = index_mint @ BasketError::IndexMintMismatch)]
    pub index: Account<'info, IndexState>,
    /// CHECK: Checked by load_mint and has_one.
    pub index_mint: UncheckedAccount<'info>,
    /// CHECK: Vault PDA.
    #[account(seeds = [VAULT_AUTHORITY_SEED, index.key().as_ref()], bump = index.vault_authority_bump)]
    pub vault_authority: UncheckedAccount<'info>,
    /// CHECK: Native USDC.
    #[account(address = USDC_MINT)]
    pub quote_mint: UncheckedAccount<'info>,
    /// CHECK: Created and checked as the vault's classic SPL USDC ATA.
    #[account(mut)]
    pub vault_quote: UncheckedAccount<'info>,
    #[account(init_if_needed, payer = payer, space = 8 + LargeBasketComponentPage::SPACE,
        seeds = [LARGE_BASKET_COMPONENT_PAGE_SEED, index.key().as_ref(),
            &[index.large_basket_component_count / MAX_LARGE_BASKET_COMPONENTS_PER_PAGE as u8]], bump)]
    pub quote_page: Account<'info, LargeBasketComponentPage>,
    /// CHECK: Associated token program.
    #[account(address = ASSOCIATED_TOKEN_ID)]
    pub associated_token_program: UncheckedAccount<'info>,
    pub token_program: Program<'info, Token>,
    pub system_program: Program<'info, System>,
}

impl<'info> RegisterRebalanceQuote<'info> {
    // remaining_accounts: every existing page, in page-index order (read-only).
    pub fn handle(ctx: Context<'_, '_, 'info, 'info, Self>) -> Result<()> {
        let index = &ctx.accounts.index;
        require!(index.kind == IndexKind::FixedWeights && index.large_basket_configured,
            BasketError::InvalidFixedWeightConfig);
        require!(!index.large_basket_operation_in_progress, BasketError::InvalidLargeBasketIntent);
        // Appending a component changes the layout open intents settle against.
        require!(index.open_intent_count == 0, BasketError::IntentsStillOpen);
        require!(usize::from(index.large_basket_component_count) < MAX_LARGE_BASKET_COMPONENTS,
            BasketError::InvalidFixedWeightConfig);
        require!(ctx.remaining_accounts.len() == usize::from(index.large_basket_page_count),
            BasketError::InvalidRemainingAccounts);
        let mut count = 0usize;
        for (i, info) in ctx.remaining_accounts.iter().enumerate() {
            let page = Account::<LargeBasketComponentPage>::try_from(info)?;
            let expected = Pubkey::find_program_address(&[LARGE_BASKET_COMPONENT_PAGE_SEED,
                index.key().as_ref(), &[i as u8]], ctx.program_id).0;
            require_keys_eq!(info.key(), expected, BasketError::InvalidLargeBasketComponentPage);
            require_keys_eq!(page.index, index.key(), BasketError::InvalidLargeBasketComponentPage);
            require!(page.finalized && usize::from(page.page_index) == i
                && usize::from(page.start_component_index) == count
                && usize::from(page.component_count) == page.components.len()
                && count == i * MAX_LARGE_BASKET_COMPONENTS_PER_PAGE,
                BasketError::InvalidLargeBasketComponentPage);
            require!(!page.components.iter().any(|c| c.mint == USDC_MINT),
                BasketError::DuplicateComponentMint);
            count += page.components.len();
        }
        require!(count == usize::from(index.large_basket_component_count), BasketError::InvalidRemainingAccounts);
        require_keys_eq!(ctx.accounts.vault_quote.key(), associated_token_address(
            &ctx.accounts.vault_authority.key(), &USDC_MINT), BasketError::InvalidVaultAccount);
        create_associated_token_account_idempotent(
            ctx.accounts.associated_token_program.to_account_info(),
            ctx.accounts.payer.to_account_info(), ctx.accounts.vault_quote.to_account_info(),
            ctx.accounts.vault_authority.to_account_info(), ctx.accounts.quote_mint.to_account_info(),
            ctx.accounts.system_program.to_account_info(), ctx.accounts.token_program.to_account_info())?;
        let quote_info = ctx.accounts.vault_quote.to_account_info();
        let quote = load_interface_token_account(&quote_info)?;
        let supply = load_mint(&ctx.accounts.index_mint.to_account_info())?.supply;
        let units = if supply == 0 { 0 } else {
            units_per_index_for_amount_saturating(quote.amount, index.index_base_units()?, supply)
        };
        let page = &mut ctx.accounts.quote_page;
        let page_index = count / MAX_LARGE_BASKET_COMPONENTS_PER_PAGE;
        if count % MAX_LARGE_BASKET_COMPONENTS_PER_PAGE == 0 {
            require!(page.components.is_empty(), BasketError::InvalidLargeBasketComponentPage);
            page.index = index.key();
            page.page_index = page_index as u8;
            page.start_component_index = count as u16;
            page.bump = ctx.bumps.quote_page;
            page.finalized = true;
            page.reserved = [0; 32];
        } else {
            require_keys_eq!(page.index, index.key(), BasketError::InvalidLargeBasketComponentPage);
            require!(page.finalized && usize::from(page.page_index) == page_index
                && page.components.len() == count % MAX_LARGE_BASKET_COMPONENTS_PER_PAGE,
                BasketError::InvalidLargeBasketComponentPage);
        }
        page.components.push(LargeBasketComponent {
            mint: USDC_MINT, units_per_index: units, target_weight_bps: 0,
            oracle_pair: Pubkey::default(), token_program: anchor_spl::token::ID,
            vault: ctx.accounts.vault_quote.key(), accounted_reserve: quote.amount, decimals: 6,
        });
        page.component_count = page.components.len() as u16;
        ctx.accounts.index.large_basket_component_count += 1;
        ctx.accounts.index.large_basket_page_count = (page_index + 1) as u8;
        Ok(())
    }
}
