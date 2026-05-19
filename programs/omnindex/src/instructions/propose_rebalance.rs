use anchor_lang::prelude::*;

use crate::{
    constants::{
        MAX_COMPONENTS, MAX_METADATA_URI_LEN, MAX_NAME_LEN, MAX_NAV_TOLERANCE_BPS,
        MAX_ORACLE_PRICE_TOLERANCE_BPS, MAX_SYMBOL_LEN,
    },
    errors::OmnindexError,
    events::IndexRebalanceProposed,
    state::{IndexComponentInput, IndexKind, IndexState},
    utils::{
        load_futarchy_authority, load_mint, nav_nad, omnipair_futarchy_authority_address,
        rebalance_mints, resolve_rebalance_prices, validate_component_inputs,
        validate_component_targets_integral, validate_no_self_component, within_bps_tolerance_u128,
        RebalancePriceInput,
    },
};

#[derive(AnchorSerialize, AnchorDeserialize, Clone, Debug)]
pub struct ProposeRebalanceArgs {
    pub components: Vec<IndexComponentInput>,
    pub quote_mint: Pubkey,
    pub prices: Vec<RebalancePriceInput>,
    pub oracle_price_tolerance_bps: u16,
    pub nav_tolerance_bps: u16,
}

#[derive(Accounts)]
pub struct ProposeRebalance<'info> {
    #[account(mut)]
    pub authority: Signer<'info>,
    #[account(
        mut,
        has_one = authority @ OmnindexError::UnauthorizedAuthority,
        has_one = index_mint @ OmnindexError::IndexMintMismatch,
        realloc = 8 + IndexState::space(
            MAX_NAME_LEN,
            MAX_SYMBOL_LEN,
            MAX_METADATA_URI_LEN,
            MAX_COMPONENTS
        ),
        realloc::payer = authority,
        realloc::zero = true
    )]
    pub index: Account<'info, IndexState>,
    /// CHECK: Validated as the configured classic SPL Token mint in the handler.
    pub index_mint: UncheckedAccount<'info>,
    /// CHECK: Validated as the Omnipair futarchy authority account.
    pub omnipair_futarchy_authority: UncheckedAccount<'info>,
    pub system_program: Program<'info, System>,
}

impl<'info> ProposeRebalance<'info> {
    pub fn handle(
        ctx: Context<'_, '_, 'info, 'info, Self>,
        args: ProposeRebalanceArgs,
    ) -> Result<()> {
        require!(
            !ctx.accounts.index.rebalancing_paused,
            OmnindexError::RebalancingPaused
        );
        require!(
            ctx.accounts.index.kind == IndexKind::FixedUnits,
            OmnindexError::InvalidIndexKind
        );
        require_keys_eq!(
            ctx.accounts.omnipair_futarchy_authority.key(),
            omnipair_futarchy_authority_address(),
            OmnindexError::InvalidOmnipairFutarchyAuthority
        );
        let futarchy_authority =
            load_futarchy_authority(&ctx.accounts.omnipair_futarchy_authority.to_account_info())?;
        require!(
            args.oracle_price_tolerance_bps <= MAX_ORACLE_PRICE_TOLERANCE_BPS,
            OmnindexError::InvalidOraclePriceTolerance
        );
        require!(
            args.nav_tolerance_bps <= MAX_NAV_TOLERANCE_BPS,
            OmnindexError::InvalidNavTolerance
        );

        let components = validate_component_inputs(args.components.clone())?;
        validate_no_self_component(&components, &ctx.accounts.index_mint.key())?;
        let old_components = ctx.accounts.index.components.clone();
        let index_mint = load_mint(&ctx.accounts.index_mint.to_account_info())?;
        validate_component_targets_integral(
            &components,
            index_mint.supply,
            ctx.accounts.index.index_base_units()?,
        )?;

        let rebalance_mints = rebalance_mints(&old_components, &components);

        let mut remaining = ctx.remaining_accounts.iter();
        let prices = resolve_rebalance_prices(
            &mut remaining,
            &rebalance_mints,
            &args.quote_mint,
            &args.prices,
            args.oracle_price_tolerance_bps,
            &futarchy_authority,
        )?;
        require!(
            remaining.next().is_none(),
            OmnindexError::InvalidRemainingAccounts
        );

        let old_nav_nad = nav_nad(&old_components, &rebalance_mints, &prices)?;
        let new_nav_nad = nav_nad(&components, &rebalance_mints, &prices)?;
        require!(
            old_nav_nad > 0 && new_nav_nad > 0,
            OmnindexError::RebalanceNavMismatch
        );
        require!(
            within_bps_tolerance_u128(old_nav_nad, new_nav_nad, args.nav_tolerance_bps)?,
            OmnindexError::RebalanceNavMismatch
        );

        let now = Clock::get()?.unix_timestamp;
        let available_at = now
            .checked_add(ctx.accounts.index.rebalance_delay_seconds)
            .ok_or_else(|| error!(OmnindexError::ArithmeticOverflow))?;
        let nonce = ctx
            .accounts
            .index
            .pending_rebalance_nonce
            .checked_add(1)
            .ok_or_else(|| error!(OmnindexError::ArithmeticOverflow))?;

        let index = &mut ctx.accounts.index;
        index.pending_component_count = components.len() as u8;
        index.pending_components = components;
        index.pending_rebalance_available_at = available_at;
        index.pending_rebalance_nonce = nonce;
        index.pending_rebalance_quote_mint = args.quote_mint;
        index.pending_rebalance_oracle_price_tolerance_bps = args.oracle_price_tolerance_bps;
        index.pending_rebalance_nav_tolerance_bps = args.nav_tolerance_bps;

        emit!(IndexRebalanceProposed {
            index: index.key(),
            authority: ctx.accounts.authority.key(),
            components: index.pending_component_count,
            available_at,
            nonce,
            quote_mint: args.quote_mint,
            old_nav_nad,
            new_nav_nad,
            oracle_price_tolerance_bps: args.oracle_price_tolerance_bps,
            nav_tolerance_bps: args.nav_tolerance_bps,
        });

        Ok(())
    }
}
