use anchor_lang::prelude::*;
use anchor_spl::token::{Token, TokenAccount};

use crate::{
    constants::{BPS_DENOMINATOR, VAULT_AUTHORITY_SEED},
    errors::OmnindexError,
    events::FixedWeightRebalanceExecuted,
    state::{IndexComponent, IndexKind, IndexState},
    utils::{
        associated_token_address, create_associated_token_account_idempotent,
        load_futarchy_authority, load_mint, load_pair, load_rate_model, load_user_token_account,
        omnipair_event_authority_address, omnipair_futarchy_authority_address,
        oracle_spot_and_ema_price_for_component, quote_exact_input_for_pair_output,
        quote_exact_output_for_pair_input, reserve_vault_address, swap as omnipair_swap,
        units_per_index_for_amount, validate_spot_ema_deviation, validate_vault_spl_token_account,
        validate_vault_token_account, FutarchyAuthority, Pair, RateModel, SwapAccounts, SwapArgs,
        ASSOCIATED_TOKEN_ID, NAD, OMNIPAIR_ID, TOKEN_2022_ID,
    },
};

#[derive(Clone)]
struct FixedWeightComponentAccount<'info> {
    component: IndexComponent,
    mint_info: AccountInfo<'info>,
    vault_info: AccountInfo<'info>,
    pair_info: Option<AccountInfo<'info>>,
    rate_model_info: Option<AccountInfo<'info>>,
    component_reserve_vault_info: Option<AccountInfo<'info>>,
    quote_reserve_vault_info: Option<AccountInfo<'info>>,
    price_nad: u64,
    current_amount: u64,
    target_amount: u64,
}

struct FixedWeightSwapContext<'info> {
    omnipair_program: AccountInfo<'info>,
    futarchy_authority: AccountInfo<'info>,
    event_authority: AccountInfo<'info>,
    vault_authority: AccountInfo<'info>,
    quote_mint: AccountInfo<'info>,
    quote_vault_info: AccountInfo<'info>,
    token_program: AccountInfo<'info>,
    token_2022_program: AccountInfo<'info>,
    index_key: Pubkey,
    vault_authority_bump: u8,
}

#[derive(AnchorSerialize, AnchorDeserialize, Clone, Debug)]
pub struct RebalanceFixedWeightsArgs {
    pub max_quote_dust: u64,
    pub max_post_rebalance_drift_bps: u16,
}

#[derive(Accounts)]
pub struct RebalanceFixedWeights<'info> {
    #[account(mut)]
    pub executor: Signer<'info>,
    #[account(mut, has_one = index_mint @ OmnindexError::IndexMintMismatch)]
    pub index: Account<'info, IndexState>,
    /// CHECK: Validated as the configured classic SPL Token mint in the handler.
    pub index_mint: UncheckedAccount<'info>,
    /// CHECK: PDA authority over the index mint and component vaults.
    #[account(
        seeds = [VAULT_AUTHORITY_SEED, index.key().as_ref()],
        bump = index.vault_authority_bump
    )]
    pub vault_authority: UncheckedAccount<'info>,
    /// CHECK: Validated against the fixed-weight quote mint stored on the index.
    pub quote_mint: UncheckedAccount<'info>,
    /// CHECK: Created and validated as the vault authority's quote ATA before use.
    #[account(mut)]
    pub vault_quote_token_account: UncheckedAccount<'info>,
    /// CHECK: Validated against the Omnipair program id.
    #[account(address = OMNIPAIR_ID @ OmnindexError::InvalidOmnipairProgram)]
    pub omnipair_program: UncheckedAccount<'info>,
    /// CHECK: Validated as the Omnipair futarchy authority account.
    pub omnipair_futarchy_authority: UncheckedAccount<'info>,
    /// CHECK: Validated as the Omnipair event authority account.
    pub omnipair_event_authority: UncheckedAccount<'info>,
    /// CHECK: Omnipair's swap ABI requires this program account even when routes use classic SPL Token.
    #[account(address = TOKEN_2022_ID @ OmnindexError::InvalidOmnipairTokenProgram)]
    pub token_2022_program: UncheckedAccount<'info>,
    /// CHECK: Validated as the Associated Token Program.
    #[account(address = ASSOCIATED_TOKEN_ID @ OmnindexError::InvalidAssociatedTokenProgram)]
    pub associated_token_program: UncheckedAccount<'info>,
    pub token_program: Program<'info, Token>,
    pub system_program: Program<'info, System>,
}

impl<'info> RebalanceFixedWeights<'info> {
    pub fn handle(
        ctx: Context<'_, '_, 'info, 'info, Self>,
        args: RebalanceFixedWeightsArgs,
    ) -> Result<()> {
        let index = &ctx.accounts.index;
        require!(
            index.kind == IndexKind::FixedWeights,
            OmnindexError::InvalidIndexKind
        );
        require!(
            args.max_post_rebalance_drift_bps <= BPS_DENOMINATOR,
            OmnindexError::InvalidFixedWeightConfig
        );
        require!(!index.rebalancing_paused, OmnindexError::RebalancingPaused);
        require!(
            index.fixed_weight_spot_ema_max_deviation_bps > 0,
            OmnindexError::InvalidFixedWeightConfig
        );
        require_keys_eq!(
            ctx.accounts.quote_mint.key(),
            index.fixed_weight_quote_mint,
            OmnindexError::InvalidQuoteMint
        );
        require_keys_eq!(
            ctx.accounts.omnipair_event_authority.key(),
            omnipair_event_authority_address(),
            OmnindexError::InvalidOmnipairEventAuthority
        );
        require_keys_eq!(
            ctx.accounts.omnipair_futarchy_authority.key(),
            omnipair_futarchy_authority_address(),
            OmnindexError::InvalidOmnipairFutarchyAuthority
        );
        require!(
            *ctx.accounts.quote_mint.to_account_info().owner == ctx.accounts.token_program.key(),
            OmnindexError::InvalidQuoteMint
        );
        create_quote_vault_if_needed(&ctx)?;

        let index_mint = load_mint(&ctx.accounts.index_mint.to_account_info())?;
        let supply = index_mint.supply;
        require!(supply > 0, OmnindexError::InvalidIndexAmount);

        let futarchy_authority =
            load_futarchy_authority(&ctx.accounts.omnipair_futarchy_authority.to_account_info())?;
        let components = index.components.clone();
        let quote_mint = ctx.accounts.quote_mint.key();
        let mut remaining = ctx.remaining_accounts.iter();
        let mut accounts = Vec::with_capacity(components.len());
        let mut total_nav_nad = 0u128;
        let mut quote_component_index = None;

        for component in components.iter().cloned() {
            let account = load_component_account(
                &ctx,
                &mut remaining,
                component,
                &quote_mint,
                &futarchy_authority,
                index.fixed_weight_spot_ema_max_deviation_bps,
            )?;
            total_nav_nad = total_nav_nad
                .checked_add(component_value_nad(
                    account.current_amount,
                    account.price_nad,
                )?)
                .ok_or_else(|| error!(OmnindexError::ArithmeticOverflow))?;
            if account.component.mint == quote_mint {
                quote_component_index = Some(accounts.len());
            }
            accounts.push(account);
        }

        require!(
            remaining.next().is_none(),
            OmnindexError::InvalidRemainingAccounts
        );
        require!(total_nav_nad > 0, OmnindexError::InvalidOmnipairOraclePrice);

        let (max_drift_bps, drift_triggered) = fixed_weight_drift_status(
            &accounts,
            total_nav_nad,
            index.fixed_weight_drift_threshold_bps,
        )?;
        let now = Clock::get()?.unix_timestamp;
        let time_triggered = index.fixed_weight_rebalance_interval_seconds > 0
            && now
                >= index
                    .fixed_weight_last_rebalanced_at
                    .checked_add(index.fixed_weight_rebalance_interval_seconds)
                    .ok_or_else(|| error!(OmnindexError::ArithmeticOverflow))?;
        require!(
            drift_triggered || time_triggered,
            OmnindexError::RebalanceNotNeeded
        );

        assign_target_amounts(&mut accounts, total_nav_nad, quote_component_index)?;
        let swap_ctx = FixedWeightSwapContext {
            omnipair_program: ctx.accounts.omnipair_program.to_account_info(),
            futarchy_authority: ctx.accounts.omnipair_futarchy_authority.to_account_info(),
            event_authority: ctx.accounts.omnipair_event_authority.to_account_info(),
            vault_authority: ctx.accounts.vault_authority.to_account_info(),
            quote_mint: ctx.accounts.quote_mint.to_account_info(),
            quote_vault_info: ctx.accounts.vault_quote_token_account.to_account_info(),
            token_program: ctx.accounts.token_program.to_account_info(),
            token_2022_program: ctx.accounts.token_2022_program.to_account_info(),
            index_key: ctx.accounts.index.key(),
            vault_authority_bump: ctx.accounts.index.vault_authority_bump,
        };
        let accounts = execute_target_swaps(
            &swap_ctx,
            accounts,
            quote_component_index,
            &futarchy_authority,
            args.max_quote_dust,
        )?;
        if quote_component_index.is_none() {
            let quote_vault =
                load_user_token_account(&ctx.accounts.vault_quote_token_account.to_account_info())?;
            validate_vault_spl_token_account(
                &quote_vault,
                &ctx.accounts.vault_quote_token_account.key(),
                &ctx.accounts.vault_authority.key(),
                &ctx.accounts.quote_mint.key(),
            )?;
            require!(
                quote_vault.amount <= args.max_quote_dust,
                OmnindexError::RebalanceTargetNotMet
            );
        }

        let base_units = ctx.accounts.index.index_base_units()?;
        let mut updated_components = Vec::with_capacity(accounts.len());
        let mut final_total_nav_nad = 0u128;
        let mut accounts = accounts;
        for account in &mut accounts {
            let vault_account = load_user_token_account(&account.vault_info)?;
            validate_vault_spl_token_account(
                &vault_account,
                &account.vault_info.key(),
                &ctx.accounts.vault_authority.key(),
                &account.component.mint,
            )?;
            account.current_amount = vault_account.amount;
            final_total_nav_nad = final_total_nav_nad
                .checked_add(component_value_nad(
                    account.current_amount,
                    account.price_nad,
                )?)
                .ok_or_else(|| error!(OmnindexError::ArithmeticOverflow))?;

            let mut component = account.component.clone();
            update_fixed_weight_component_units(
                &mut component,
                vault_account.amount,
                base_units,
                supply,
            )?;
            updated_components.push(component);
        }
        require!(
            final_total_nav_nad > 0,
            OmnindexError::InvalidOmnipairOraclePrice
        );
        let (post_rebalance_max_drift_bps, _) =
            fixed_weight_drift_status(&accounts, final_total_nav_nad, 0)?;
        require!(
            post_rebalance_max_drift_bps <= args.max_post_rebalance_drift_bps,
            OmnindexError::RebalanceTargetNotMet
        );

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
    ctx: &Context<'_, '_, 'info, 'info, RebalanceFixedWeights<'info>>,
) -> Result<()> {
    let expected_ata = associated_token_address(
        &ctx.accounts.vault_authority.key(),
        &ctx.accounts.quote_mint.key(),
    );
    require_keys_eq!(
        ctx.accounts.vault_quote_token_account.key(),
        expected_ata,
        OmnindexError::InvalidVaultAccount
    );

    create_associated_token_account_idempotent(
        ctx.accounts.associated_token_program.to_account_info(),
        ctx.accounts.executor.to_account_info(),
        ctx.accounts.vault_quote_token_account.to_account_info(),
        ctx.accounts.vault_authority.to_account_info(),
        ctx.accounts.quote_mint.to_account_info(),
        ctx.accounts.system_program.to_account_info(),
        ctx.accounts.token_program.to_account_info(),
    )?;

    let quote_vault =
        load_user_token_account(&ctx.accounts.vault_quote_token_account.to_account_info())?;
    validate_vault_spl_token_account(
        &quote_vault,
        &ctx.accounts.vault_quote_token_account.key(),
        &ctx.accounts.vault_authority.key(),
        &ctx.accounts.quote_mint.key(),
    )
}

fn load_component_account<'info>(
    ctx: &Context<'_, '_, 'info, 'info, RebalanceFixedWeights<'info>>,
    remaining: &mut std::slice::Iter<'info, AccountInfo<'info>>,
    component: IndexComponent,
    quote_mint: &Pubkey,
    futarchy_authority: &FutarchyAuthority,
    max_spot_ema_deviation_bps: u16,
) -> Result<FixedWeightComponentAccount<'info>> {
    let mint_info = next_account_info(remaining)?;
    let vault_info = next_account_info(remaining)?;

    require_keys_eq!(
        mint_info.key(),
        component.mint,
        OmnindexError::InvalidComponentMint
    );
    let vault_account = Account::<TokenAccount>::try_from(vault_info)?;
    validate_vault_token_account(
        &vault_account,
        &vault_info.key(),
        &ctx.accounts.vault_authority.key(),
        &component.mint,
    )?;

    if component.mint == *quote_mint {
        return Ok(FixedWeightComponentAccount {
            component,
            mint_info: mint_info.clone(),
            vault_info: vault_info.clone(),
            pair_info: None,
            rate_model_info: None,
            component_reserve_vault_info: None,
            quote_reserve_vault_info: None,
            price_nad: NAD,
            current_amount: vault_account.amount,
            target_amount: 0,
        });
    }

    require_keys_neq!(
        component.oracle_pair,
        Pubkey::default(),
        OmnindexError::InvalidFixedWeightConfig
    );

    let pair_info = next_account_info(remaining)?;
    let rate_model_info = next_account_info(remaining)?;
    let component_reserve_vault_info = next_account_info(remaining)?;
    let quote_reserve_vault_info = next_account_info(remaining)?;

    require_keys_eq!(
        pair_info.key(),
        component.oracle_pair,
        OmnindexError::InvalidOmnipairPair
    );
    let pair = load_pair(pair_info)?;
    let rate_model = load_rate_model(rate_model_info)?;
    require_keys_eq!(
        rate_model_info.key(),
        pair.rate_model,
        OmnindexError::InvalidOmnipairRateModel
    );
    require!(
        (pair.token0 == component.mint && pair.token1 == *quote_mint)
            || (pair.token1 == component.mint && pair.token0 == *quote_mint),
        OmnindexError::InvalidOmnipairPair
    );
    validate_omnipair_reserve(
        pair_info.key(),
        &component.mint,
        component_reserve_vault_info,
    )?;
    validate_omnipair_reserve(pair_info.key(), quote_mint, quote_reserve_vault_info)?;
    let (price_nad, spot_price_nad) = oracle_spot_and_ema_price_for_component(
        &pair,
        &rate_model,
        futarchy_authority,
        &component.mint,
        quote_mint,
    )?;
    validate_spot_ema_deviation(spot_price_nad, price_nad, max_spot_ema_deviation_bps)?;

    Ok(FixedWeightComponentAccount {
        component,
        mint_info: mint_info.clone(),
        vault_info: vault_info.clone(),
        pair_info: Some(pair_info.clone()),
        rate_model_info: Some(rate_model_info.clone()),
        component_reserve_vault_info: Some(component_reserve_vault_info.clone()),
        quote_reserve_vault_info: Some(quote_reserve_vault_info.clone()),
        price_nad,
        current_amount: vault_account.amount,
        target_amount: 0,
    })
}

fn fixed_weight_drift_status(
    accounts: &[FixedWeightComponentAccount],
    total_nav_nad: u128,
    drift_threshold_bps: u16,
) -> Result<(u16, bool)> {
    let mut max_drift_bps = 0u16;

    for account in accounts {
        let value = component_value_nad(account.current_amount, account.price_nad)?;
        let actual_weight_bps = value
            .checked_mul(u128::from(BPS_DENOMINATOR))
            .and_then(|value| value.checked_div(total_nav_nad))
            .ok_or_else(|| error!(OmnindexError::ArithmeticOverflow))?;
        let actual_weight_bps = u16::try_from(actual_weight_bps)
            .map_err(|_| error!(OmnindexError::ArithmeticOverflow))?;
        let drift = actual_weight_bps.abs_diff(account.component.target_weight_bps);
        max_drift_bps = max_drift_bps.max(drift);
    }

    Ok((
        max_drift_bps,
        drift_threshold_bps > 0 && max_drift_bps >= drift_threshold_bps,
    ))
}

fn assign_target_amounts(
    accounts: &mut [FixedWeightComponentAccount],
    total_nav_nad: u128,
    quote_component_index: Option<usize>,
) -> Result<()> {
    let mut assigned_value_nad = 0u128;

    for (index, account) in accounts.iter_mut().enumerate() {
        if Some(index) == quote_component_index {
            continue;
        }
        account.target_amount = target_amount_for_weight(
            total_nav_nad,
            account.component.target_weight_bps,
            account.price_nad,
        )?;
        assigned_value_nad = assigned_value_nad
            .checked_add(component_value_nad(
                account.target_amount,
                account.price_nad,
            )?)
            .ok_or_else(|| error!(OmnindexError::ArithmeticOverflow))?;
    }

    if let Some(quote_component_index) = quote_component_index {
        let quote_value_nad = total_nav_nad.saturating_sub(assigned_value_nad);
        accounts[quote_component_index].target_amount =
            u64::try_from(quote_value_nad / u128::from(NAD))
                .map_err(|_| error!(OmnindexError::ArithmeticOverflow))?;
    }

    Ok(())
}

fn target_amount_for_weight(
    total_nav_nad: u128,
    target_weight_bps: u16,
    price_nad: u64,
) -> Result<u64> {
    let target_value = total_nav_nad
        .checked_mul(u128::from(target_weight_bps))
        .and_then(|value| value.checked_div(u128::from(BPS_DENOMINATOR)))
        .ok_or_else(|| error!(OmnindexError::ArithmeticOverflow))?;
    let target_amount = target_value
        .checked_div(u128::from(price_nad))
        .ok_or_else(|| error!(OmnindexError::ArithmeticOverflow))?;
    u64::try_from(target_amount).map_err(|_| error!(OmnindexError::ArithmeticOverflow))
}

fn execute_target_swaps<'info>(
    ctx: &FixedWeightSwapContext<'info>,
    mut accounts: Vec<FixedWeightComponentAccount<'info>>,
    quote_component_index: Option<usize>,
    futarchy_authority: &FutarchyAuthority,
    max_quote_dust: u64,
) -> Result<Vec<FixedWeightComponentAccount<'info>>> {
    let signer_seeds: &[&[u8]] = &[
        VAULT_AUTHORITY_SEED,
        ctx.index_key.as_ref(),
        &[ctx.vault_authority_bump],
    ];
    let quote_mint = ctx.quote_mint.key();
    let quote_vault_info = ctx.quote_vault_info.clone();
    let mut quote_available = if let Some(quote_component_index) = quote_component_index {
        accounts[quote_component_index].current_amount
    } else {
        let quote_vault_account = load_user_token_account(&quote_vault_info)?;
        quote_vault_account.amount
    };

    for index in 0..accounts.len() {
        if accounts[index].component.mint == quote_mint
            || accounts[index].current_amount <= accounts[index].target_amount
        {
            continue;
        }

        let amount_in = accounts[index].current_amount - accounts[index].target_amount;
        let (pair, rate_model) = load_current_pair_and_rate_model(&accounts[index])?;
        let quote_output = quote_exact_output_for_pair_input(
            &pair,
            &rate_model,
            futarchy_authority,
            &accounts[index].component.mint,
            &quote_mint,
            amount_in,
        )?;
        require!(quote_output > 0, OmnindexError::InvalidRebalanceSwap);

        call_component_quote_swap(
            ctx,
            account_swap_infos(&accounts[index])?,
            quote_vault_info.clone(),
            signer_seeds,
            amount_in,
            quote_output,
            true,
        )?;
        quote_available = quote_available
            .checked_add(quote_output)
            .ok_or_else(|| error!(OmnindexError::ArithmeticOverflow))?;
        accounts[index].current_amount = accounts[index].target_amount;
    }

    for index in underweight_buy_order(&accounts)? {
        if accounts[index].component.mint == quote_mint
            || accounts[index].current_amount >= accounts[index].target_amount
        {
            continue;
        }
        if quote_available == 0 {
            break;
        }

        let amount_out = accounts[index].target_amount - accounts[index].current_amount;
        let (pair, rate_model) = load_current_pair_and_rate_model(&accounts[index])?;
        let quote_input = quote_exact_input_for_pair_output(
            &pair,
            &rate_model,
            futarchy_authority,
            &quote_mint,
            &accounts[index].component.mint,
            amount_out,
        )?;
        let (quote_spent, component_output) = if quote_available >= quote_input {
            (quote_input, amount_out)
        } else {
            if quote_available <= max_quote_dust {
                break;
            }
            let component_output = quote_exact_output_for_pair_input(
                &pair,
                &rate_model,
                futarchy_authority,
                &quote_mint,
                &accounts[index].component.mint,
                quote_available,
            )?;
            require!(component_output > 0, OmnindexError::InvalidRebalanceSwap);
            (quote_available, component_output)
        };

        call_component_quote_swap(
            ctx,
            account_swap_infos(&accounts[index])?,
            quote_vault_info.clone(),
            signer_seeds,
            quote_spent,
            component_output,
            false,
        )?;
        quote_available = quote_available
            .checked_sub(quote_spent)
            .ok_or_else(|| error!(OmnindexError::ArithmeticOverflow))?;
        accounts[index].current_amount = accounts[index]
            .current_amount
            .checked_add(component_output)
            .ok_or_else(|| error!(OmnindexError::ArithmeticOverflow))?;
    }

    if quote_component_index.is_none() && quote_available > max_quote_dust {
        let index = surplus_sink_component_index(&accounts)?;
        let (pair, rate_model) = load_current_pair_and_rate_model(&accounts[index])?;
        let component_output = quote_exact_output_for_pair_input(
            &pair,
            &rate_model,
            futarchy_authority,
            &quote_mint,
            &accounts[index].component.mint,
            quote_available,
        )?;
        require!(component_output > 0, OmnindexError::InvalidRebalanceSwap);

        call_component_quote_swap(
            ctx,
            account_swap_infos(&accounts[index])?,
            quote_vault_info.clone(),
            signer_seeds,
            quote_available,
            component_output,
            false,
        )?;
        accounts[index].current_amount = accounts[index]
            .current_amount
            .checked_add(component_output)
            .ok_or_else(|| error!(OmnindexError::ArithmeticOverflow))?;
    }

    Ok(accounts)
}

fn load_current_pair_and_rate_model(
    account: &FixedWeightComponentAccount,
) -> Result<(Pair, RateModel)> {
    let pair_info = account
        .pair_info
        .as_ref()
        .ok_or_else(|| error!(OmnindexError::InvalidOmnipairPair))?;
    let rate_model_info = account
        .rate_model_info
        .as_ref()
        .ok_or_else(|| error!(OmnindexError::InvalidOmnipairRateModel))?;
    let pair = load_pair(pair_info)?;
    let rate_model = load_rate_model(rate_model_info)?;
    require_keys_eq!(
        rate_model_info.key(),
        pair.rate_model,
        OmnindexError::InvalidOmnipairRateModel
    );
    Ok((pair, rate_model))
}

fn underweight_buy_order(accounts: &[FixedWeightComponentAccount]) -> Result<Vec<usize>> {
    let mut deficits = Vec::new();
    for (index, account) in accounts.iter().enumerate() {
        if account.current_amount >= account.target_amount {
            continue;
        }
        deficits.push((
            index,
            component_value_nad(
                account.target_amount - account.current_amount,
                account.price_nad,
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

fn surplus_sink_component_index(accounts: &[FixedWeightComponentAccount]) -> Result<usize> {
    accounts
        .iter()
        .enumerate()
        .filter(|(_, account)| account.pair_info.is_some())
        .max_by(|(left_index, left), (right_index, right)| {
            left.component
                .target_weight_bps
                .cmp(&right.component.target_weight_bps)
                .then_with(|| right_index.cmp(left_index))
        })
        .map(|(index, _)| index)
        .ok_or_else(|| error!(OmnindexError::InvalidOmnipairPair))
}

struct ComponentSwapInfos<'info> {
    pair_info: AccountInfo<'info>,
    rate_model_info: AccountInfo<'info>,
    component_reserve_vault_info: AccountInfo<'info>,
    quote_reserve_vault_info: AccountInfo<'info>,
    component_vault_info: AccountInfo<'info>,
    component_mint_info: AccountInfo<'info>,
}

fn account_swap_infos<'info>(
    account: &FixedWeightComponentAccount<'info>,
) -> Result<ComponentSwapInfos<'info>> {
    Ok(ComponentSwapInfos {
        pair_info: (*account
            .pair_info
            .as_ref()
            .ok_or_else(|| error!(OmnindexError::InvalidOmnipairPair))?)
        .clone(),
        rate_model_info: (*account
            .rate_model_info
            .as_ref()
            .ok_or_else(|| error!(OmnindexError::InvalidOmnipairRateModel))?)
        .clone(),
        component_reserve_vault_info: (*account
            .component_reserve_vault_info
            .as_ref()
            .ok_or_else(|| error!(OmnindexError::InvalidOmnipairVault))?)
        .clone(),
        quote_reserve_vault_info: (*account
            .quote_reserve_vault_info
            .as_ref()
            .ok_or_else(|| error!(OmnindexError::InvalidOmnipairVault))?)
        .clone(),
        component_vault_info: account.vault_info.clone(),
        component_mint_info: account.mint_info.clone(),
    })
}

fn call_component_quote_swap<'info>(
    ctx: &FixedWeightSwapContext<'info>,
    infos: ComponentSwapInfos<'info>,
    quote_vault_info: AccountInfo<'info>,
    signer_seeds: &[&[u8]],
    amount_in: u64,
    min_amount_out: u64,
    component_to_quote: bool,
) -> Result<()> {
    let (
        token_in_vault,
        token_out_vault,
        user_token_in_account,
        user_token_out_account,
        token_in_mint,
        token_out_mint,
    ) = if component_to_quote {
        (
            infos.component_reserve_vault_info.clone(),
            infos.quote_reserve_vault_info.clone(),
            infos.component_vault_info.clone(),
            quote_vault_info.clone(),
            infos.component_mint_info.clone(),
            ctx.quote_mint.clone(),
        )
    } else {
        (
            infos.quote_reserve_vault_info.clone(),
            infos.component_reserve_vault_info.clone(),
            quote_vault_info.clone(),
            infos.component_vault_info.clone(),
            ctx.quote_mint.clone(),
            infos.component_mint_info.clone(),
        )
    };

    omnipair_swap(
        ctx.omnipair_program.clone(),
        SwapAccounts {
            pair: infos.pair_info,
            rate_model: infos.rate_model_info,
            futarchy_authority: ctx.futarchy_authority.clone(),
            token_in_vault,
            token_out_vault,
            user_token_in_account,
            user_token_out_account,
            token_in_mint,
            token_out_mint,
            user: ctx.vault_authority.clone(),
            token_program: ctx.token_program.clone(),
            token_2022_program: ctx.token_2022_program.clone(),
            event_authority: ctx.event_authority.clone(),
        },
        SwapArgs {
            amount_in,
            min_amount_out,
        },
        &[signer_seeds],
    )
}

fn validate_omnipair_reserve<'info>(
    pair: Pubkey,
    mint: &Pubkey,
    reserve_vault_info: &'info AccountInfo<'info>,
) -> Result<()> {
    require_keys_eq!(
        reserve_vault_info.key(),
        reserve_vault_address(&pair, mint),
        OmnindexError::InvalidOmnipairVault
    );
    let reserve_vault_account = Account::<TokenAccount>::try_from(reserve_vault_info)?;
    require_keys_eq!(
        reserve_vault_account.owner,
        pair,
        OmnindexError::InvalidOmnipairVault
    );
    require_keys_eq!(
        reserve_vault_account.mint,
        *mint,
        OmnindexError::InvalidOmnipairVault
    );
    Ok(())
}

fn component_value_nad(amount: u64, price_nad: u64) -> Result<u128> {
    u128::from(amount)
        .checked_mul(u128::from(price_nad))
        .ok_or_else(|| error!(OmnindexError::ArithmeticOverflow))
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
        OmnindexError::ZeroComponentUnits
    );
    component.units_per_index = units_per_index;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fixed_weight_units_reject_zero_for_positive_weight_component() {
        let mut component = IndexComponent {
            mint: Pubkey::new_unique(),
            units_per_index: 1,
            target_weight_bps: 1,
            oracle_pair: Pubkey::new_unique(),
        };

        let result = update_fixed_weight_component_units(&mut component, 0, 1_000_000, 1_000_000);

        assert!(result.is_err());
        assert_eq!(component.units_per_index, 1);
    }

    #[test]
    fn fixed_weight_units_accept_nonzero_recomputed_units() {
        let mut component = IndexComponent {
            mint: Pubkey::new_unique(),
            units_per_index: 1,
            target_weight_bps: 1,
            oracle_pair: Pubkey::new_unique(),
        };

        update_fixed_weight_component_units(&mut component, 2, 1_000_000, 2_000_000).unwrap();

        assert_eq!(component.units_per_index, 1);
    }

    #[test]
    fn target_amount_for_weight_uses_external_quote_nav() {
        let total_nav_nad = 300u128 * u128::from(NAD);

        assert_eq!(
            target_amount_for_weight(total_nav_nad, 5_000, 2 * NAD).unwrap(),
            75
        );
        assert_eq!(
            target_amount_for_weight(total_nav_nad, 5_000, NAD).unwrap(),
            150
        );
    }

    #[test]
    fn target_amount_for_weight_rejects_zero_price() {
        assert!(target_amount_for_weight(300u128 * u128::from(NAD), 5_000, 0).is_err());
    }
}
