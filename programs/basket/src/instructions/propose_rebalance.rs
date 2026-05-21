use anchor_lang::prelude::*;

use crate::{
    constants::{
        MAX_COMPONENTS, MAX_METADATA_URI_LEN, MAX_NAME_LEN, MAX_NAV_TOLERANCE_BPS,
        MAX_ORACLE_PRICE_TOLERANCE_BPS, MAX_SYMBOL_LEN,
    },
    errors::BasketError,
    events::IndexRebalanceProposed,
    state::{IndexComponentInput, IndexKind, IndexState},
    utils::{
        load_interface_mint, load_mint, nav_nad, rebalance_mints, resolve_rebalance_prices,
        validate_component_inputs, validate_component_targets_integral,
        validate_index_strategy_config, validate_no_self_component, validate_rebalance_quote_mint,
        verified_switchboard_prices, within_bps_tolerance_u128, RebalancePriceInput,
    },
};

#[derive(AnchorSerialize, AnchorDeserialize, Clone, Debug)]
pub struct ProposeRebalanceArgs {
    pub components: Vec<IndexComponentInput>,
    pub quote_mint: Pubkey,
    pub prices: Vec<RebalancePriceInput>,
    pub oracle_price_tolerance_bps: u16,
    pub nav_tolerance_bps: u16,
    pub switchboard_max_age_slots: u64,
}

#[derive(Accounts)]
pub struct ProposeRebalance<'info> {
    #[account(mut)]
    pub authority: Signer<'info>,
    #[account(
        mut,
        has_one = authority @ BasketError::UnauthorizedAuthority,
        has_one = index_mint @ BasketError::IndexMintMismatch,
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
    /// CHECK: Verified by Switchboard's quote verifier.
    pub switchboard_queue: UncheckedAccount<'info>,
    /// CHECK: Verified by Switchboard's quote verifier and canonical key check.
    pub switchboard_quote: UncheckedAccount<'info>,
    /// CHECK: Switchboard verifier validates this sysvar id.
    pub slothashes: UncheckedAccount<'info>,
    /// CHECK: Switchboard verifier validates this sysvar id.
    pub instructions_sysvar: UncheckedAccount<'info>,
    pub system_program: Program<'info, System>,
}

impl<'info> ProposeRebalance<'info> {
    pub fn handle(
        ctx: Context<'_, '_, 'info, 'info, Self>,
        args: ProposeRebalanceArgs,
    ) -> Result<()> {
        require!(
            !ctx.accounts.index.rebalancing_paused,
            BasketError::RebalancingPaused
        );
        require!(
            ctx.accounts.index.kind == IndexKind::FixedUnits,
            BasketError::InvalidIndexKind
        );
        validate_rebalance_quote_mint(&args.quote_mint)?;
        require!(
            args.oracle_price_tolerance_bps <= MAX_ORACLE_PRICE_TOLERANCE_BPS,
            BasketError::InvalidOraclePriceTolerance
        );
        require!(
            args.nav_tolerance_bps <= MAX_NAV_TOLERANCE_BPS,
            BasketError::InvalidNavTolerance
        );

        let components = validate_component_inputs(args.components.clone())?;
        validate_no_self_component(&components, &ctx.accounts.index_mint.key())?;
        validate_index_strategy_config(
            IndexKind::FixedUnits,
            &components,
            Pubkey::default(),
            0,
            0,
            0,
        )?;
        let old_components = ctx.accounts.index.components.clone();
        let index_mint = load_mint(&ctx.accounts.index_mint.to_account_info())?;
        validate_component_targets_integral(
            &components,
            index_mint.supply,
            ctx.accounts.index.index_base_units()?,
        )?;

        let rebalance_mints = rebalance_mints(&old_components, &components);
        let mut remaining = ctx.remaining_accounts.iter();
        let mint_decimals = load_rebalance_mint_decimals(&mut remaining, &rebalance_mints)?;
        require!(
            remaining.next().is_none(),
            BasketError::InvalidRemainingAccounts
        );
        let switchboard_prices = verified_switchboard_prices(
            &ctx.accounts.switchboard_queue.to_account_info(),
            &ctx.accounts.switchboard_quote.to_account_info(),
            &ctx.accounts.slothashes.to_account_info(),
            &ctx.accounts.instructions_sysvar.to_account_info(),
            Clock::get()?.slot,
            args.switchboard_max_age_slots,
        )?;
        let prices = resolve_rebalance_prices(
            &rebalance_mints,
            &old_components,
            &components,
            &mint_decimals,
            &args.prices,
            args.oracle_price_tolerance_bps,
            &switchboard_prices,
        )?;

        let old_nav_nad = nav_nad(&old_components, &prices)?;
        let new_nav_nad = nav_nad(&components, &prices)?;
        require!(
            old_nav_nad > 0 && new_nav_nad > 0,
            BasketError::RebalanceNavMismatch
        );
        require!(
            within_bps_tolerance_u128(old_nav_nad, new_nav_nad, args.nav_tolerance_bps)?,
            BasketError::RebalanceNavMismatch
        );

        let now = Clock::get()?.unix_timestamp;
        let available_at = now
            .checked_add(ctx.accounts.index.rebalance_delay_seconds)
            .ok_or_else(|| error!(BasketError::ArithmeticOverflow))?;
        let nonce = ctx
            .accounts
            .index
            .pending_rebalance_nonce
            .checked_add(1)
            .ok_or_else(|| error!(BasketError::ArithmeticOverflow))?;

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

fn load_rebalance_mint_decimals<'info>(
    remaining: &mut std::slice::Iter<'info, AccountInfo<'info>>,
    rebalance_mints: &[Pubkey],
) -> Result<Vec<u8>> {
    let mut decimals = Vec::with_capacity(rebalance_mints.len());

    for mint in rebalance_mints {
        let mint_info = next_account_info(remaining)?;
        require_keys_eq!(mint_info.key(), *mint, BasketError::InvalidComponentMint);
        decimals.push(load_interface_mint(mint_info)?.decimals);
    }

    Ok(decimals)
}
