use anchor_lang::prelude::*;
use anchor_spl::{
    token::Token,
    token_interface::{TokenAccount as InterfaceTokenAccount, TokenInterface},
};

use crate::{
    constants::{
        BPS_DENOMINATOR, MAX_FIXED_WEIGHT_EXECUTION_SLIPPAGE_BPS,
        MAX_FIXED_WEIGHT_POST_REBALANCE_DRIFT_BPS, MAX_FIXED_WEIGHT_QUOTE_DUST_BPS,
        MAX_REBALANCE_SWAPS, USDC_MINT, VAULT_AUTHORITY_SEED,
    },
    errors::BasketError,
    events::FixedWeightRebalanceExecuted,
    state::{IndexComponent, IndexKind, IndexState},
    utils::{
        associated_token_address_with_token_program,
        create_associated_token_account_idempotent_for_token_program, invoke_jupiter_swap,
        load_interface_mint, load_interface_token_account, load_mint, switchboard_feed_price,
        units_per_index_for_amount, validate_buy_execution_price,
        validate_jupiter_route_account_scope, validate_sell_execution_price,
        verified_switchboard_prices, ASSOCIATED_TOKEN_ID, SWITCHBOARD_NAD_SCALE_FACTOR,
        SWITCHBOARD_PRICE_SCALE,
    },
};

use super::mint_index_with_jupiter::JupiterRebalanceSwapPlan as JupiterSwapPlan;

#[derive(AnchorSerialize, AnchorDeserialize, Clone, Debug)]
pub struct RebalanceFixedWeightsWithJupiterArgs {
    pub max_quote_dust: u64,
    pub max_post_rebalance_drift_bps: u16,
    pub switchboard_max_age_slots: u64,
    pub swaps: Vec<JupiterSwapPlan>,
}

#[derive(Accounts)]
pub struct RebalanceFixedWeightsWithJupiter<'info> {
    #[account(mut)]
    pub executor: Signer<'info>,
    #[account(mut, has_one = index_mint @ BasketError::IndexMintMismatch)]
    pub index: Account<'info, IndexState>,
    /// CHECK: Validated as the configured classic SPL Token index mint.
    pub index_mint: UncheckedAccount<'info>,
    /// CHECK: PDA authority over the index mint and component vaults.
    #[account(
        seeds = [VAULT_AUTHORITY_SEED, index.key().as_ref()],
        bump = index.vault_authority_bump
    )]
    pub vault_authority: UncheckedAccount<'info>,
    /// CHECK: Validated as the configured quote mint and parsed manually for Token/Token-2022 decimals.
    pub quote_mint: UncheckedAccount<'info>,
    #[account(mut)]
    /// CHECK: Created and validated as the vault authority quote ATA.
    pub vault_quote_token_account: UncheckedAccount<'info>,
    /// CHECK: Validated against known Jupiter program ids.
    pub jupiter_program: UncheckedAccount<'info>,
    /// CHECK: Verified by Switchboard's quote verifier.
    pub switchboard_queue: UncheckedAccount<'info>,
    /// CHECK: Verified by Switchboard's quote verifier and canonical key check.
    pub switchboard_quote: UncheckedAccount<'info>,
    /// CHECK: Switchboard verifier validates this sysvar id.
    pub slothashes: UncheckedAccount<'info>,
    /// CHECK: Switchboard verifier validates this sysvar id.
    pub instructions_sysvar: UncheckedAccount<'info>,
    /// CHECK: Validated as the Associated Token Program.
    #[account(address = ASSOCIATED_TOKEN_ID @ BasketError::InvalidAssociatedTokenProgram)]
    pub associated_token_program: UncheckedAccount<'info>,
    pub quote_token_program: Interface<'info, TokenInterface>,
    pub system_program: Program<'info, System>,
    pub token_program: Program<'info, Token>,
}

#[derive(Clone)]
struct FixedWeightJupiterComponent<'info> {
    component: IndexComponent,
    mint_info: AccountInfo<'info>,
    vault_info: AccountInfo<'info>,
    token_program_info: AccountInfo<'info>,
    decimals: u8,
    oracle_price: i128,
    current_amount: u64,
    target_amount: u64,
}

impl<'info> RebalanceFixedWeightsWithJupiter<'info> {
    pub fn handle(
        ctx: Context<'_, '_, 'info, 'info, Self>,
        args: RebalanceFixedWeightsWithJupiterArgs,
    ) -> Result<()> {
        require!(
            ctx.accounts.index.kind == IndexKind::FixedWeights,
            BasketError::InvalidIndexKind
        );
        require!(
            !ctx.accounts.index.rebalancing_paused,
            BasketError::RebalancingPaused
        );
        require!(
            args.max_post_rebalance_drift_bps <= MAX_FIXED_WEIGHT_POST_REBALANCE_DRIFT_BPS,
            BasketError::InvalidFixedWeightConfig
        );
        require!(
            args.swaps.len() <= MAX_REBALANCE_SWAPS,
            BasketError::TooManyRebalanceSwaps
        );
        validate_rebalance_swap_controls(&args.swaps)?;
        require_keys_eq!(
            ctx.accounts.quote_mint.key(),
            USDC_MINT,
            BasketError::InvalidQuoteMint
        );
        require_keys_eq!(
            *ctx.accounts.quote_mint.to_account_info().owner,
            ctx.accounts.quote_token_program.key(),
            BasketError::InvalidQuoteMint
        );
        require_keys_eq!(
            ctx.accounts.quote_mint.key(),
            ctx.accounts.index.fixed_weight_quote_mint,
            BasketError::InvalidQuoteMint
        );
        let quote_mint_info = load_interface_mint(&ctx.accounts.quote_mint.to_account_info())?;
        let quote_decimals = quote_mint_info.decimals;

        create_quote_vault_if_needed(&ctx)?;

        let index_mint = load_mint(&ctx.accounts.index_mint.to_account_info())?;
        let supply = index_mint.supply;
        require!(supply > 0, BasketError::InvalidIndexAmount);

        let prices = verified_switchboard_prices(
            &ctx.accounts.switchboard_queue.to_account_info(),
            &ctx.accounts.switchboard_quote.to_account_info(),
            &ctx.accounts.slothashes.to_account_info(),
            &ctx.accounts.instructions_sysvar.to_account_info(),
            Clock::get()?.slot,
            args.switchboard_max_age_slots,
        )?;

        let components = ctx.accounts.index.components.clone();
        let quote_mint = ctx.accounts.quote_mint.key();
        let mut remaining = ctx.remaining_accounts.iter();
        let mut accounts = Vec::with_capacity(components.len());
        let mut total_value = 0u128;
        let mut quote_component_index = None;

        for component in components.iter().cloned() {
            let account =
                load_component_account(&ctx, &mut remaining, component, &quote_mint, &prices)?;
            total_value = total_value
                .checked_add(component_value_scaled(
                    account.current_amount,
                    account.decimals,
                    account.oracle_price,
                )?)
                .ok_or_else(|| error!(BasketError::ArithmeticOverflow))?;
            if account.component.mint == quote_mint {
                quote_component_index = Some(accounts.len());
            }
            accounts.push(account);
        }

        let route_accounts = remaining.as_slice();
        let candidates = account_candidates(&ctx, &accounts, route_accounts);
        require!(total_value > 0, BasketError::InvalidSwitchboardPrice);

        let (max_drift_bps, drift_triggered) = fixed_weight_drift_status(
            &accounts,
            total_value,
            ctx.accounts.index.fixed_weight_drift_threshold_bps,
        )?;
        if quote_component_index.is_none() {
            validate_quote_dust_budget(args.max_quote_dust, quote_decimals, total_value)?;
        }
        let now = Clock::get()?.unix_timestamp;
        let time_triggered = ctx.accounts.index.fixed_weight_rebalance_interval_seconds > 0
            && now
                >= ctx
                    .accounts
                    .index
                    .fixed_weight_last_rebalanced_at
                    .checked_add(ctx.accounts.index.fixed_weight_rebalance_interval_seconds)
                    .ok_or_else(|| error!(BasketError::ArithmeticOverflow))?;
        require!(
            drift_triggered || time_triggered,
            BasketError::RebalanceNotNeeded
        );

        assign_target_amounts(&mut accounts, total_value)?;
        let accounts = execute_jupiter_swaps(&ctx, accounts, &candidates, &args, quote_decimals)?;
        validate_quote_dust(&ctx, quote_component_index, args.max_quote_dust)?;

        let base_units = ctx.accounts.index.index_base_units()?;
        let mut final_total_value = 0u128;
        let mut updated_components = Vec::with_capacity(accounts.len());
        let mut refreshed_accounts = accounts;
        for account in &mut refreshed_accounts {
            let vault = load_interface_token_account(&account.vault_info)?;
            validate_vault_account(
                &vault,
                &ctx.accounts.vault_authority.key(),
                &account.component.mint,
            )?;
            account.current_amount = vault.amount;
            final_total_value = final_total_value
                .checked_add(component_value_scaled(
                    account.current_amount,
                    account.decimals,
                    account.oracle_price,
                )?)
                .ok_or_else(|| error!(BasketError::ArithmeticOverflow))?;

            let mut component = account.component.clone();
            update_fixed_weight_component_units(
                &mut component,
                account.current_amount,
                base_units,
                supply,
            )?;
            updated_components.push(component);
        }

        require!(final_total_value > 0, BasketError::InvalidSwitchboardPrice);
        let (post_rebalance_max_drift_bps, _) =
            fixed_weight_drift_status(&refreshed_accounts, final_total_value, 0)?;
        require!(
            post_rebalance_max_drift_bps <= args.max_post_rebalance_drift_bps,
            BasketError::RebalanceTargetNotMet
        );
        let total_nav_nad = total_value
            .checked_div(SWITCHBOARD_NAD_SCALE_FACTOR)
            .ok_or_else(|| error!(BasketError::ArithmeticOverflow))?;

        let index = &mut ctx.accounts.index;
        index.components = updated_components;
        index.component_count = index.components.len() as u8;
        index.fixed_weight_last_rebalanced_at = now;

        emit!(FixedWeightRebalanceExecuted {
            index: index.key(),
            executor: ctx.accounts.executor.key(),
            supply,
            total_nav_nad,
            max_drift_bps,
            time_triggered,
            drift_triggered,
        });

        Ok(())
    }
}

fn create_quote_vault_if_needed<'info>(
    ctx: &Context<'_, '_, 'info, 'info, RebalanceFixedWeightsWithJupiter<'info>>,
) -> Result<()> {
    let expected_ata = associated_token_address_with_token_program(
        &ctx.accounts.vault_authority.key(),
        &ctx.accounts.quote_mint.key(),
        ctx.accounts.quote_token_program.key,
    );
    require_keys_eq!(
        ctx.accounts.vault_quote_token_account.key(),
        expected_ata,
        BasketError::InvalidVaultAccount
    );

    create_associated_token_account_idempotent_for_token_program(
        ctx.accounts.associated_token_program.to_account_info(),
        ctx.accounts.executor.to_account_info(),
        ctx.accounts.vault_quote_token_account.to_account_info(),
        ctx.accounts.vault_authority.to_account_info(),
        ctx.accounts.quote_mint.to_account_info(),
        ctx.accounts.system_program.to_account_info(),
        ctx.accounts.quote_token_program.to_account_info(),
    )?;

    let quote_vault =
        load_interface_token_account(&ctx.accounts.vault_quote_token_account.to_account_info())?;
    validate_vault_account(
        &quote_vault,
        &ctx.accounts.vault_authority.key(),
        &ctx.accounts.quote_mint.key(),
    )
}

fn load_component_account<'info>(
    ctx: &Context<'_, '_, 'info, 'info, RebalanceFixedWeightsWithJupiter<'info>>,
    remaining: &mut std::slice::Iter<'info, AccountInfo<'info>>,
    component: IndexComponent,
    quote_mint: &Pubkey,
    prices: &[crate::utils::SwitchboardPrice],
) -> Result<FixedWeightJupiterComponent<'info>> {
    let mint_info = next_account_info(remaining)?;
    let vault_info = next_account_info(remaining)?;
    let token_program_info = next_account_info(remaining)?;

    require_keys_eq!(
        mint_info.key(),
        component.mint,
        BasketError::InvalidComponentMint
    );
    require_keys_eq!(
        *mint_info.owner,
        token_program_info.key(),
        BasketError::InvalidTokenMint
    );

    let expected_vault = associated_token_address_with_token_program(
        &ctx.accounts.vault_authority.key(),
        &component.mint,
        token_program_info.key,
    );
    require_keys_eq!(
        vault_info.key(),
        expected_vault,
        BasketError::InvalidVaultAccount
    );
    create_associated_token_account_idempotent_for_token_program(
        ctx.accounts.associated_token_program.to_account_info(),
        ctx.accounts.executor.to_account_info(),
        vault_info.clone(),
        ctx.accounts.vault_authority.to_account_info(),
        mint_info.clone(),
        ctx.accounts.system_program.to_account_info(),
        token_program_info.clone(),
    )?;

    let mint = load_interface_mint(mint_info)?;
    let vault = load_interface_token_account(vault_info)?;
    validate_vault_account(&vault, &ctx.accounts.vault_authority.key(), &component.mint)?;

    let oracle_price = if component.mint == *quote_mint {
        SWITCHBOARD_PRICE_SCALE as i128
    } else {
        require_keys_neq!(
            component.oracle_pair,
            Pubkey::default(),
            BasketError::InvalidFixedWeightConfig
        );
        switchboard_feed_price(prices, &component.oracle_pair)?
    };

    Ok(FixedWeightJupiterComponent {
        component,
        mint_info: mint_info.clone(),
        vault_info: vault_info.clone(),
        token_program_info: token_program_info.clone(),
        decimals: mint.decimals,
        oracle_price,
        current_amount: vault.amount,
        target_amount: 0,
    })
}

fn execute_jupiter_swaps<'info>(
    ctx: &Context<'_, '_, 'info, 'info, RebalanceFixedWeightsWithJupiter<'info>>,
    mut accounts: Vec<FixedWeightJupiterComponent<'info>>,
    candidates: &[AccountInfo<'info>],
    args: &RebalanceFixedWeightsWithJupiterArgs,
    quote_decimals: u8,
) -> Result<Vec<FixedWeightJupiterComponent<'info>>> {
    let index_key = ctx.accounts.index.key();
    let vault_authority_bump = [ctx.accounts.index.vault_authority_bump];
    let signer_seeds: &[&[u8]] = &[
        VAULT_AUTHORITY_SEED,
        index_key.as_ref(),
        &vault_authority_bump,
    ];
    let quote_mint = ctx.accounts.quote_mint.key();
    let quote_vault = ctx.accounts.vault_quote_token_account.key();
    let protected_vaults = protected_vault_keys(&accounts, quote_vault);
    let mut swap_index = 0usize;

    for account in &mut accounts {
        if account.component.mint == quote_mint || account.current_amount <= account.target_amount {
            continue;
        }

        let surplus = account
            .current_amount
            .checked_sub(account.target_amount)
            .ok_or_else(|| error!(BasketError::ArithmeticOverflow))?;
        let swap = args
            .swaps
            .get(swap_index)
            .ok_or_else(|| error!(BasketError::InvalidJupiterRoute))?;
        swap_index = swap_index
            .checked_add(1)
            .ok_or_else(|| error!(BasketError::ArithmeticOverflow))?;
        validate_component_sell_swap(quote_mint, quote_vault, account, swap)?;
        validate_jupiter_route_account_scope(
            candidates,
            &swap.accounts,
            &protected_vaults,
            &[account.vault_info.key(), quote_vault],
        )?;

        let component_before = load_interface_token_account(&account.vault_info)?.amount;
        let quote_before = load_interface_token_account(
            &ctx.accounts.vault_quote_token_account.to_account_info(),
        )?
        .amount;
        invoke_jupiter_swap(
            ctx.accounts.jupiter_program.to_account_info(),
            candidates,
            &swap.accounts,
            &swap.instruction_data,
            Some(ctx.accounts.vault_authority.key()),
            &[signer_seeds],
        )?;
        let component_after = load_interface_token_account(&account.vault_info)?.amount;
        let quote_after = load_interface_token_account(
            &ctx.accounts.vault_quote_token_account.to_account_info(),
        )?
        .amount;
        let component_spent = component_before
            .checked_sub(component_after)
            .ok_or_else(|| error!(BasketError::InvalidJupiterRoute))?;
        let quote_received = quote_after
            .checked_sub(quote_before)
            .ok_or_else(|| error!(BasketError::InvalidJupiterRoute))?;
        require!(component_spent > 0, BasketError::InvalidJupiterRoute);
        require!(component_spent <= surplus, BasketError::InvalidJupiterRoute);
        require!(quote_received > 0, BasketError::InvalidJupiterRoute);
        validate_sell_execution_price(
            quote_received,
            component_spent,
            quote_decimals,
            account.decimals,
            account.oracle_price,
            swap.max_oracle_slippage_bps,
        )?;
        account.current_amount = component_after;
    }

    for index in underweight_buy_order(&accounts)? {
        if accounts[index].component.mint == quote_mint
            || accounts[index].current_amount >= accounts[index].target_amount
        {
            continue;
        }

        let deficit = accounts[index]
            .target_amount
            .checked_sub(accounts[index].current_amount)
            .ok_or_else(|| error!(BasketError::ArithmeticOverflow))?;
        let swap = args
            .swaps
            .get(swap_index)
            .ok_or_else(|| error!(BasketError::InvalidJupiterRoute))?;
        swap_index = swap_index
            .checked_add(1)
            .ok_or_else(|| error!(BasketError::ArithmeticOverflow))?;
        validate_component_buy_swap(quote_mint, quote_vault, &accounts[index], swap)?;
        validate_jupiter_route_account_scope(
            candidates,
            &swap.accounts,
            &protected_vaults,
            &[quote_vault, accounts[index].vault_info.key()],
        )?;

        let quote_before = load_interface_token_account(
            &ctx.accounts.vault_quote_token_account.to_account_info(),
        )?
        .amount;
        let component_before = load_interface_token_account(&accounts[index].vault_info)?.amount;
        invoke_jupiter_swap(
            ctx.accounts.jupiter_program.to_account_info(),
            candidates,
            &swap.accounts,
            &swap.instruction_data,
            Some(ctx.accounts.vault_authority.key()),
            &[signer_seeds],
        )?;
        let quote_after = load_interface_token_account(
            &ctx.accounts.vault_quote_token_account.to_account_info(),
        )?
        .amount;
        let component_after = load_interface_token_account(&accounts[index].vault_info)?.amount;
        let quote_spent = quote_before
            .checked_sub(quote_after)
            .ok_or_else(|| error!(BasketError::InvalidJupiterRoute))?;
        let component_received = component_after
            .checked_sub(component_before)
            .ok_or_else(|| error!(BasketError::InvalidJupiterRoute))?;
        require!(quote_spent > 0, BasketError::InvalidJupiterRoute);
        require!(component_received > 0, BasketError::InvalidJupiterRoute);
        require!(
            component_received <= deficit,
            BasketError::InvalidJupiterRoute
        );
        validate_buy_execution_price(
            quote_spent,
            component_received,
            quote_decimals,
            accounts[index].decimals,
            accounts[index].oracle_price,
            swap.max_oracle_slippage_bps,
        )?;
        accounts[index].current_amount = component_after;
    }

    require!(
        swap_index == args.swaps.len(),
        BasketError::InvalidJupiterRoute
    );

    Ok(accounts)
}

fn validate_rebalance_swap_controls(swaps: &[JupiterSwapPlan]) -> Result<()> {
    for swap in swaps {
        require!(
            swap.max_oracle_slippage_bps <= MAX_FIXED_WEIGHT_EXECUTION_SLIPPAGE_BPS,
            BasketError::InvalidOraclePriceTolerance
        );
    }

    Ok(())
}

fn protected_vault_keys(
    accounts: &[FixedWeightJupiterComponent<'_>],
    quote_vault: Pubkey,
) -> Vec<Pubkey> {
    let mut keys = vec![quote_vault];
    for account in accounts {
        let vault = account.vault_info.key();
        if !keys.contains(&vault) {
            keys.push(vault);
        }
    }
    keys
}

fn validate_component_sell_swap(
    quote_mint: Pubkey,
    quote_vault: Pubkey,
    account: &FixedWeightJupiterComponent,
    swap: &JupiterSwapPlan,
) -> Result<()> {
    require_keys_eq!(
        swap.input_mint,
        account.component.mint,
        BasketError::InvalidJupiterRoute
    );
    require_keys_eq!(
        swap.output_mint,
        quote_mint,
        BasketError::InvalidJupiterRoute
    );
    require_keys_eq!(
        swap.source_token_account,
        account.vault_info.key(),
        BasketError::InvalidJupiterRoute
    );
    require_keys_eq!(
        swap.destination_token_account,
        quote_vault,
        BasketError::InvalidJupiterRoute
    );
    Ok(())
}

fn validate_component_buy_swap(
    quote_mint: Pubkey,
    quote_vault: Pubkey,
    account: &FixedWeightJupiterComponent,
    swap: &JupiterSwapPlan,
) -> Result<()> {
    require_keys_eq!(
        swap.input_mint,
        quote_mint,
        BasketError::InvalidJupiterRoute
    );
    require_keys_eq!(
        swap.output_mint,
        account.component.mint,
        BasketError::InvalidJupiterRoute
    );
    require_keys_eq!(
        swap.source_token_account,
        quote_vault,
        BasketError::InvalidJupiterRoute
    );
    require_keys_eq!(
        swap.destination_token_account,
        account.vault_info.key(),
        BasketError::InvalidJupiterRoute
    );
    Ok(())
}

fn validate_quote_dust<'info>(
    ctx: &Context<'_, '_, 'info, 'info, RebalanceFixedWeightsWithJupiter<'info>>,
    quote_component_index: Option<usize>,
    max_quote_dust: u64,
) -> Result<()> {
    if quote_component_index.is_some() {
        return Ok(());
    }

    let quote_vault =
        load_interface_token_account(&ctx.accounts.vault_quote_token_account.to_account_info())?;
    require!(
        quote_vault.amount <= max_quote_dust,
        BasketError::RebalanceTargetNotMet
    );
    Ok(())
}

fn validate_quote_dust_budget(
    max_quote_dust: u64,
    quote_decimals: u8,
    total_value: u128,
) -> Result<()> {
    let max_dust_value = component_value_scaled(
        max_quote_dust,
        quote_decimals,
        SWITCHBOARD_PRICE_SCALE as i128,
    )?;
    let allowed_dust_value = total_value
        .checked_mul(u128::from(MAX_FIXED_WEIGHT_QUOTE_DUST_BPS))
        .and_then(|value| value.checked_div(u128::from(BPS_DENOMINATOR)))
        .ok_or_else(|| error!(BasketError::ArithmeticOverflow))?;
    require!(
        max_dust_value <= allowed_dust_value,
        BasketError::InvalidFixedWeightConfig
    );
    Ok(())
}

fn account_candidates<'info>(
    ctx: &Context<'_, '_, 'info, 'info, RebalanceFixedWeightsWithJupiter<'info>>,
    component_accounts: &[FixedWeightJupiterComponent<'info>],
    route_accounts: &[AccountInfo<'info>],
) -> Vec<AccountInfo<'info>> {
    let mut candidates = vec![
        ctx.accounts.executor.to_account_info(),
        ctx.accounts.index.to_account_info(),
        ctx.accounts.index_mint.to_account_info(),
        ctx.accounts.vault_authority.to_account_info(),
        ctx.accounts.quote_mint.to_account_info(),
        ctx.accounts.vault_quote_token_account.to_account_info(),
        ctx.accounts.jupiter_program.to_account_info(),
        ctx.accounts.associated_token_program.to_account_info(),
        ctx.accounts.quote_token_program.to_account_info(),
        ctx.accounts.system_program.to_account_info(),
        ctx.accounts.token_program.to_account_info(),
    ];
    for account in component_accounts {
        candidates.push(account.mint_info.clone());
        candidates.push(account.vault_info.clone());
        candidates.push(account.token_program_info.clone());
    }
    candidates.extend_from_slice(route_accounts);
    candidates
}

fn assign_target_amounts(
    accounts: &mut [FixedWeightJupiterComponent],
    total_value: u128,
) -> Result<()> {
    for account in accounts {
        let target_value = total_value
            .checked_mul(u128::from(account.component.target_weight_bps))
            .and_then(|value| value.checked_div(u128::from(BPS_DENOMINATOR)))
            .ok_or_else(|| error!(BasketError::ArithmeticOverflow))?;
        account.target_amount =
            target_amount_for_value_scaled(target_value, account.decimals, account.oracle_price)?;
    }
    Ok(())
}

fn fixed_weight_drift_status(
    accounts: &[FixedWeightJupiterComponent],
    total_value: u128,
    drift_threshold_bps: u16,
) -> Result<(u16, bool)> {
    let mut max_drift_bps = 0u16;
    for account in accounts {
        let value = component_value_scaled(
            account.current_amount,
            account.decimals,
            account.oracle_price,
        )?;
        let actual_weight_bps = value
            .checked_mul(u128::from(BPS_DENOMINATOR))
            .and_then(|value| value.checked_div(total_value))
            .ok_or_else(|| error!(BasketError::ArithmeticOverflow))?;
        let actual_weight_bps = u16::try_from(actual_weight_bps)
            .map_err(|_| error!(BasketError::ArithmeticOverflow))?;
        let drift = actual_weight_bps.abs_diff(account.component.target_weight_bps);
        max_drift_bps = max_drift_bps.max(drift);
    }

    Ok((
        max_drift_bps,
        drift_threshold_bps > 0 && max_drift_bps >= drift_threshold_bps,
    ))
}

fn underweight_buy_order(accounts: &[FixedWeightJupiterComponent]) -> Result<Vec<usize>> {
    let mut deficits = Vec::new();
    for (index, account) in accounts.iter().enumerate() {
        if account.current_amount >= account.target_amount {
            continue;
        }
        deficits.push((
            index,
            component_value_scaled(
                account.target_amount - account.current_amount,
                account.decimals,
                account.oracle_price,
            )?,
        ));
    }
    deficits.sort_by(|(left_index, left_value), (right_index, right_value)| {
        right_value
            .cmp(left_value)
            .then_with(|| left_index.cmp(right_index))
    });
    Ok(deficits.into_iter().map(|(index, _)| index).collect())
}

fn component_value_scaled(amount: u64, decimals: u8, oracle_price: i128) -> Result<u128> {
    require!(oracle_price > 0, BasketError::InvalidSwitchboardPrice);
    let oracle_price =
        u128::try_from(oracle_price).map_err(|_| error!(BasketError::InvalidSwitchboardPrice))?;
    let denominator = pow10_u128(decimals)?;
    u128::from(amount)
        .checked_mul(oracle_price)
        .and_then(|value| value.checked_div(denominator))
        .ok_or_else(|| error!(BasketError::ArithmeticOverflow))
}

fn target_amount_for_value_scaled(value: u128, decimals: u8, oracle_price: i128) -> Result<u64> {
    require!(oracle_price > 0, BasketError::InvalidSwitchboardPrice);
    let oracle_price =
        u128::try_from(oracle_price).map_err(|_| error!(BasketError::InvalidSwitchboardPrice))?;
    let amount = value
        .checked_mul(pow10_u128(decimals)?)
        .and_then(|value| value.checked_div(oracle_price))
        .ok_or_else(|| error!(BasketError::ArithmeticOverflow))?;
    u64::try_from(amount).map_err(|_| error!(BasketError::ArithmeticOverflow))
}

fn update_fixed_weight_component_units(
    component: &mut IndexComponent,
    vault_amount: u64,
    base_units: u64,
    supply: u64,
) -> Result<()> {
    let units_per_index = units_per_index_for_amount(vault_amount, base_units, supply)?;
    require!(
        component.target_weight_bps == 0 || units_per_index > 0,
        BasketError::ZeroComponentUnits
    );
    component.units_per_index = units_per_index;
    Ok(())
}

fn validate_vault_account(
    token_account: &InterfaceTokenAccount,
    vault_authority: &Pubkey,
    component_mint: &Pubkey,
) -> Result<()> {
    require_keys_eq!(
        token_account.owner,
        *vault_authority,
        BasketError::InvalidVaultAccount
    );
    require_keys_eq!(
        token_account.mint,
        *component_mint,
        BasketError::InvalidVaultAccount
    );
    Ok(())
}

fn pow10_u128(decimals: u8) -> Result<u128> {
    let mut value = 1u128;
    for _ in 0..decimals {
        value = value
            .checked_mul(10)
            .ok_or_else(|| error!(BasketError::ArithmeticOverflow))?;
    }
    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn component_value_normalizes_token_decimals() {
        let price = 2_i128 * SWITCHBOARD_PRICE_SCALE as i128;
        let value = component_value_scaled(1_500_000, 6, price).unwrap();

        assert_eq!(value, 3 * SWITCHBOARD_PRICE_SCALE);
    }

    #[test]
    fn target_amount_normalizes_token_decimals() {
        let price = 2_i128 * SWITCHBOARD_PRICE_SCALE as i128;
        let amount = target_amount_for_value_scaled(3 * SWITCHBOARD_PRICE_SCALE, 6, price).unwrap();

        assert_eq!(amount, 1_500_000);
    }

    #[test]
    fn rebalance_controls_reject_wide_jupiter_slippage() {
        let swaps = vec![JupiterSwapPlan {
            input_mint: Pubkey::new_unique(),
            output_mint: Pubkey::new_unique(),
            source_token_account: Pubkey::new_unique(),
            destination_token_account: Pubkey::new_unique(),
            max_oracle_slippage_bps: MAX_FIXED_WEIGHT_EXECUTION_SLIPPAGE_BPS + 1,
            instruction_data: vec![1],
            accounts: Vec::new(),
        }];

        assert!(validate_rebalance_swap_controls(&swaps).is_err());
    }

    #[test]
    fn quote_dust_budget_is_capped_to_nav_share() {
        let total_value = 100 * SWITCHBOARD_PRICE_SCALE;
        let allowed_dust = 5_000_000;
        let too_much_dust = 5_000_001;

        assert!(validate_quote_dust_budget(allowed_dust, 6, total_value).is_ok());
        assert!(validate_quote_dust_budget(too_much_dust, 6, total_value).is_err());
    }
}
