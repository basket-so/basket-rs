//! Batched FixedWeights keeper rebalance (RebalanceMode::KeeperDrift).
//!
//! Ports the old single-atomic-tx `rebalance_fixed_weights_with_jupiter` onto the same
//! open -> execute(batch) -> verify -> finalize INTENT state machine the large-basket
//! mint/redeem flow uses, so the swaps can span multiple transactions with the compact
//! `LargeBasketSwapPlan` (1 byte / route account) instead of the fat single-tx plan that
//! only fit ~1 swap per transaction.
//!
//! Lifecycle:
//!   1. `open_rebalance_intent`  — price the whole NAV (paged component vaults + the
//!      oracle's posted prices), gate on drift/time, derive each component's swap
//!      LEG (atoms to sell or buy) and direction, snapshot supply + NAV, take the
//!      `large_basket_operation_in_progress` lock and create the vault USDC ATA. Only the
//!      index authority or its rebalance keeper may open, and only with no mint/redeem
//!      intents open (see `request_rebalance`); its gate args are still clamped two-sided
//!      (see MIN/MAX_KEEPER_NAV_TOLERANCE_BPS).
//!   2. `execute_rebalance_sell_batch` ×N — batched component->USDC swaps (vault authority
//!      signs); `execute_rebalance_buy_batch` ×N — batched USDC->component swaps funded by
//!      the proceeds. Executes, verifies and finalize are keeper/authority-only. Each checks execution against the posted price in the same transaction and records the verified fill.
//!      Blocked once the intent expires or the index pauses rebalancing.
//!   3. `verify_rebalance_component_price` — bounds each executed leg's effective price
//!      (recorded quote / recorded fill) against a fresh posted price.
//!   4. `finalize_rebalance` — require every leg executed + verified, re-price the NAV,
//!      enforce post-rebalance drift + one-sided NAV preservation + quote dust, rewrite
//!      each page's `units_per_index` / `accounted_reserve`, release the lock.
//!   5. `cancel_rebalance` — initiator-only escape hatch, allowed only before any leg has
//!      executed, releasing the lock.
//!   6. `unwind_rebalance` — the stuck-intent escape hatch: permissionless once the intent
//!      expires (index authority any time). Re-syncs every page's accounting from the live
//!      vault balances (all funds stay in vault-authority custody throughout a rebalance,
//!      so abandoning one is purely an accounting re-sync) and releases the lock. Without
//!      this, a single unexecutable leg would lock mint/redeem forever.
//!   7. `close_rebalance_intent` — initiator reclaims the intent account's rent once the
//!      intent is no longer open.
//!
//! Quote (USDC) accounting: the vault authority's USDC ATA doubles as the swap scratch
//! account AND, when USDC is itself a component, as that component's vault. NAV therefore
//! always counts it — as the USDC component when one exists, otherwise as parked quote
//! value added on top of the component vaults — so leg sizing automatically recycles
//! leftover USDC (e.g. from an unwound rebalance) back into components, and the finalize
//! dust check bounds only the parked EXCESS, not the USDC component's own backing.
//!
//! Prices come from the price board, which only the protocol's oracle key may write
//! (`post_prices`): the keeper has fresh prices posted just before each step that reads
//! them. Open and finalize price the full basket in one transaction (the real FixedWeights
//! baskets are 4-10 components). Paged NAV pricing for larger baskets is a follow-up.

use anchor_lang::prelude::*;
use anchor_spl::token_interface::TokenInterface;

use crate::{
    constants::{
        BPS_DENOMINATOR, LARGE_BASKET_COMPONENT_BITMAP_BYTES, LARGE_BASKET_COMPONENT_PAGE_SEED,
        MAX_FIXED_WEIGHT_EXECUTION_SLIPPAGE_BPS, MAX_FIXED_WEIGHT_POST_REBALANCE_DRIFT_BPS,
        MAX_FIXED_WEIGHT_QUOTE_DUST_BPS, MAX_KEEPER_NAV_TOLERANCE_BPS,
        MAX_LARGE_BASKET_INTENT_TTL_SECONDS, MAX_REBALANCE_SWAPS_PER_BATCH,
        MAX_PRICE_AGE_SLOTS, MIN_FIXED_WEIGHT_POST_REBALANCE_DRIFT_BPS,
        MIN_KEEPER_NAV_TOLERANCE_BPS, PRICE_BOARD_SEED, REBALANCE_INTENT_SEED, USDC_DECIMALS,
        USDC_MINT, VAULT_AUTHORITY_SEED,
    },
    errors::BasketError,
    events::{
        FixedWeightRebalanceExecuted, IndexRebalanceCancelled, RebalanceComponentSwapped,
        RebalanceIntentOpened, RebalanceIntentUnwound,
    },
    state::{
        IndexKind, IndexState, LargeBasketComponent, LargeBasketComponentPage, PriceBoard,
        RebalanceIntent, RebalanceMode, RebalanceStatus,
    },
    utils::{
        associated_token_address_with_token_program, bitmap_all_set, bitmap_get, bitmap_set_once,
        board_price, create_associated_token_account_idempotent_for_token_program,
        invoke_jupiter_swap, load_interface_token_account, load_mint,
        nav_loss_within_tolerance_u128, unpack_account_metas, units_per_index_for_amount,
        units_per_index_for_amount_saturating, validate_buy_execution_price,
        validate_jupiter_route_account_scope, validate_price_age_slots,
        validate_sell_execution_price, validate_vault_authority_token_account_scope,
        ASSOCIATED_TOKEN_ID, LargeBasketSwapPlan, PRICE_NAD_SCALE_FACTOR, PRICE_SCALE,
    },
};

// ---------------------------------------------------------------------------
// Args
// ---------------------------------------------------------------------------

#[derive(AnchorSerialize, AnchorDeserialize, Clone, Debug)]
pub struct OpenRebalanceIntentArgs {
    pub nonce: u64,
    pub expires_at: i64,
    pub max_price_age_slots: u64,
    pub nav_tolerance_bps: u16,
    pub max_post_rebalance_drift_bps: u16,
}

#[derive(AnchorSerialize, AnchorDeserialize, Clone, Debug)]
pub struct RebalanceBatchEntry {
    pub component_index: u16,
    /// Sell leg: minimum USDC to receive. Buy leg: maximum USDC to spend.
    pub quote_limit: u64,
    pub route_account_count: u8,
    pub swap: LargeBasketSwapPlan,
}

#[derive(AnchorSerialize, AnchorDeserialize, Clone, Debug)]
pub struct ExecuteRebalanceBatchArgs {
    pub entries: Vec<RebalanceBatchEntry>,
    pub max_price_age_slots: u64,
    pub max_oracle_slippage_bps: u16,
}

#[derive(AnchorSerialize, AnchorDeserialize, Clone, Debug)]
pub struct VerifyRebalanceComponentPriceArgs {
    pub component_index: u16,
    pub max_oracle_slippage_bps: u16,
    pub max_price_age_slots: u64,
}

#[derive(AnchorSerialize, AnchorDeserialize, Clone, Debug)]
pub struct FinalizeRebalanceArgs {
    pub max_price_age_slots: u64,
}

// ---------------------------------------------------------------------------
// Accounts
// ---------------------------------------------------------------------------

#[derive(Accounts)]
#[instruction(args: OpenRebalanceIntentArgs)]
pub struct OpenRebalanceIntent<'info> {
    #[account(mut)]
    pub initiator: Signer<'info>,
    #[account(mut, has_one = index_mint @ BasketError::IndexMintMismatch)]
    pub index: Account<'info, IndexState>,
    /// CHECK: Validated as the configured classic SPL index mint via has_one + load_mint.
    pub index_mint: UncheckedAccount<'info>,
    /// CHECK: PDA authority over the component vaults.
    #[account(
        seeds = [VAULT_AUTHORITY_SEED, index.key().as_ref()],
        bump = index.vault_authority_bump
    )]
    pub vault_authority: UncheckedAccount<'info>,
    /// CHECK: Validated as native Solana USDC.
    #[account(address = USDC_MINT @ BasketError::InvalidQuoteMint)]
    pub quote_mint: UncheckedAccount<'info>,
    /// CHECK: Created + validated as the vault authority's USDC ATA (rebalance scratch).
    #[account(mut)]
    pub vault_quote_token_account: UncheckedAccount<'info>,
    #[account(
        init,
        payer = initiator,
        space = 8 + RebalanceIntent::SPACE,
        seeds = [REBALANCE_INTENT_SEED, index.key().as_ref(), &args.nonce.to_le_bytes()],
        bump
    )]
    pub intent: Account<'info, RebalanceIntent>,
    /// Prices the protocol's oracle posted; each must be at most `max_price_age_slots` old.
    #[account(seeds = [PRICE_BOARD_SEED], bump = price_board.bump)]
    pub price_board: Box<Account<'info, PriceBoard>>,
    /// CHECK: Validated as the Associated Token Program.
    #[account(address = ASSOCIATED_TOKEN_ID @ BasketError::InvalidAssociatedTokenProgram)]
    pub associated_token_program: UncheckedAccount<'info>,
    pub quote_token_program: Interface<'info, TokenInterface>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct ExecuteRebalanceBatch<'info> {
    #[account(mut)]
    pub keeper: Signer<'info>,
    pub index: Account<'info, IndexState>,
    #[account(mut, has_one = index @ BasketError::InvalidLargeBasketIntent)]
    pub intent: Account<'info, RebalanceIntent>,
    /// CHECK: PDA authority over the component vaults; signs the swaps.
    #[account(
        seeds = [VAULT_AUTHORITY_SEED, index.key().as_ref()],
        bump = index.vault_authority_bump
    )]
    pub vault_authority: UncheckedAccount<'info>,
    /// CHECK: Validated as native Solana USDC.
    #[account(address = USDC_MINT @ BasketError::InvalidQuoteMint)]
    pub quote_mint: UncheckedAccount<'info>,
    /// CHECK: Validated as the vault authority's USDC ATA.
    #[account(mut)]
    pub vault_quote_token_account: UncheckedAccount<'info>,
    /// CHECK: Validated against known Jupiter program ids.
    pub jupiter_program: UncheckedAccount<'info>,
    /// CHECK: Validated as the Associated Token Program.
    #[account(address = ASSOCIATED_TOKEN_ID @ BasketError::InvalidAssociatedTokenProgram)]
    pub associated_token_program: UncheckedAccount<'info>,
    pub quote_token_program: Interface<'info, TokenInterface>,
    pub system_program: Program<'info, System>,
    /// Prices the protocol's oracle posted; each must be at most `max_price_age_slots` old.
    #[account(seeds = [PRICE_BOARD_SEED], bump = price_board.bump)]
    pub price_board: Box<Account<'info, PriceBoard>>,
}

#[derive(Accounts)]
pub struct VerifyRebalanceComponentPrice<'info> {
    pub keeper: Signer<'info>,
    pub index: Account<'info, IndexState>,
    #[account(mut, has_one = index @ BasketError::InvalidLargeBasketIntent)]
    pub intent: Account<'info, RebalanceIntent>,
    pub component_page: Account<'info, LargeBasketComponentPage>,
    /// Prices the protocol's oracle posted; each must be at most `max_price_age_slots` old.
    #[account(seeds = [PRICE_BOARD_SEED], bump = price_board.bump)]
    pub price_board: Box<Account<'info, PriceBoard>>,
}

#[derive(Accounts)]
pub struct FinalizeRebalance<'info> {
    #[account(mut)]
    pub keeper: Signer<'info>,
    #[account(mut)]
    pub index: Account<'info, IndexState>,
    #[account(mut, has_one = index @ BasketError::InvalidLargeBasketIntent)]
    pub intent: Account<'info, RebalanceIntent>,
    /// CHECK: PDA authority over the component vaults.
    #[account(
        seeds = [VAULT_AUTHORITY_SEED, index.key().as_ref()],
        bump = index.vault_authority_bump
    )]
    pub vault_authority: UncheckedAccount<'info>,
    /// CHECK: Validated as native Solana USDC.
    #[account(address = USDC_MINT @ BasketError::InvalidQuoteMint)]
    pub quote_mint: UncheckedAccount<'info>,
    /// CHECK: Validated as the vault authority's USDC ATA.
    pub vault_quote_token_account: UncheckedAccount<'info>,
    /// Prices the protocol's oracle posted; each must be at most `max_price_age_slots` old.
    #[account(seeds = [PRICE_BOARD_SEED], bump = price_board.bump)]
    pub price_board: Box<Account<'info, PriceBoard>>,
    pub quote_token_program: Interface<'info, TokenInterface>,
}

#[derive(Accounts)]
pub struct CancelRebalance<'info> {
    #[account(mut)]
    pub initiator: Signer<'info>,
    #[account(mut)]
    pub index: Account<'info, IndexState>,
    #[account(mut, has_one = index @ BasketError::InvalidLargeBasketIntent)]
    pub intent: Account<'info, RebalanceIntent>,
}

#[derive(Accounts)]
pub struct UnwindRebalance<'info> {
    pub caller: Signer<'info>,
    #[account(mut)]
    pub index: Account<'info, IndexState>,
    #[account(mut, has_one = index @ BasketError::InvalidLargeBasketIntent)]
    pub intent: Account<'info, RebalanceIntent>,
}

#[derive(Accounts)]
pub struct CloseRebalanceIntent<'info> {
    #[account(mut)]
    pub initiator: Signer<'info>,
    #[account(
        mut,
        close = initiator,
        has_one = initiator @ BasketError::UnauthorizedAuthority,
        constraint = intent.status != RebalanceStatus::Open @ BasketError::RebalanceIntentStillOpen
    )]
    pub intent: Account<'info, RebalanceIntent>,
}

// ---------------------------------------------------------------------------
// Open
// ---------------------------------------------------------------------------

struct ComponentSnapshot {
    decimals: u8,
    target_weight_bps: u16,
    oracle_price: i128,
    current_amount: u64,
    // What the basket's accounting says it holds, for removed components (sold in full).
    accounted_reserve: u64,
    is_quote: bool,
    // Removed by a composition change and already sold: not priced and never traded.
    retired: bool,
    value: u128,
}

/// What a rebalance does with one component.
#[derive(Debug, PartialEq, Eq)]
enum LegPlan {
    Done,
    Sell(u64),
    Buy(u64),
}

/// Plans a component's leg toward its target share of `total_value`. A removed component
/// (zero weight) sells what the basket owns, never more: tokens sent to its public vault stay
/// put, so they cannot turn a write-off into a sell no swap can fill. Holdings worth under a
/// cent are not worth a swap (which might not return a quote atom) and are written off.
fn plan_leg(snap: &ComponentSnapshot, total_value: u128) -> Result<LegPlan> {
    if snap.is_quote || snap.retired {
        return Ok(LegPlan::Done);
    }
    if snap.target_weight_bps == 0 {
        let owned = snap.current_amount.min(snap.accounted_reserve);
        let owned_value = component_value_scaled(owned, snap.decimals, snap.oracle_price)?;
        return Ok(if owned_value < REMOVED_COMPONENT_DUST_VALUE {
            LegPlan::Done
        } else {
            LegPlan::Sell(owned)
        });
    }
    let target_value = total_value
        .checked_mul(u128::from(snap.target_weight_bps))
        .and_then(|v| v.checked_div(u128::from(BPS_DENOMINATOR)))
        .ok_or_else(|| error!(BasketError::ArithmeticOverflow))?;
    let target_amount = target_amount_for_value_scaled(target_value, snap.decimals, snap.oracle_price)?;
    Ok(if leg_is_dust(snap.current_amount, target_amount) {
        LegPlan::Done
    } else if snap.current_amount > target_amount {
        LegPlan::Sell(snap.current_amount - target_amount)
    } else {
        LegPlan::Buy(target_amount - snap.current_amount)
    })
}

/// A removed component is sold in full by a finalized rebalance (or written off as dust), so
/// anything its vault holds at finalize arrived after open and stays unaccounted.
fn sold_out_at_finalize(component: &LargeBasketComponent) -> bool {
    component.mint != USDC_MINT && component.target_weight_bps == 0
}

/// At unwind, only a removed component whose sell leg executed has been sold out.
fn sold_out_at_unwind(component: &LargeBasketComponent, sell_leg: bool, sell_done: bool) -> bool {
    sold_out_at_finalize(component) && sell_leg && sell_done
}

impl<'info> OpenRebalanceIntent<'info> {
    pub fn handle(
        ctx: Context<'_, '_, 'info, 'info, Self>,
        args: OpenRebalanceIntentArgs,
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
            ctx.accounts.index.large_basket_configured,
            BasketError::LargeBasketNotConfigured
        );
        require!(
            !ctx.accounts.index.large_basket_operation_in_progress,
            BasketError::InvalidLargeBasketIntent
        );
        // A rebalance stops new mints and redeems while it runs, so only the authority or
        // its keeper may start one; anyone else could hold the basket by reopening them.
        require!(
            ctx.accounts
                .index
                .is_rebalance_operator(&ctx.accounts.initiator.key()),
            BasketError::NotRebalanceOperator
        );
        // Page accounting is re-synced from live vault balances at finalize/unwind, which is
        // only sound with no mint deposits or redeem reservations in flight.
        require!(
            ctx.accounts.index.open_intent_count == 0,
            BasketError::IntentsStillOpen
        );
        require_keys_eq!(
            ctx.accounts.quote_mint.key(),
            ctx.accounts.index.fixed_weight_quote_mint,
            BasketError::InvalidQuoteMint
        );
        // NAV-loss tolerance is keeper input, so it is clamped two-sided: the ceiling caps
        // how much NAV a bad keeper can leak, the floor rejects a no-op gate. (After the
        // unwind escape hatch an over-tight gate is no longer terminal.)
        require!(
            args.nav_tolerance_bps >= MIN_KEEPER_NAV_TOLERANCE_BPS
                && args.nav_tolerance_bps <= MAX_KEEPER_NAV_TOLERANCE_BPS,
            BasketError::InvalidNavTolerance
        );
        // Two-sided clamp: the floor keeps the bound achievable (an exact-0 drift bound is
        // unreachable after real swaps, so it would make finalize impossible and strand a
        // round of swaps), the ceiling caps how far off-target a finalize may leave the
        // basket.
        require!(
            args.max_post_rebalance_drift_bps >= MIN_FIXED_WEIGHT_POST_REBALANCE_DRIFT_BPS
                && args.max_post_rebalance_drift_bps <= MAX_FIXED_WEIGHT_POST_REBALANCE_DRIFT_BPS,
            BasketError::InvalidFixedWeightConfig
        );
        // Drift-loop guard: a drift-triggered open is NOT time-spaced, so if a finalized
        // rebalance could leave the basket at or above the index's drift re-trigger
        // threshold the keeper could chain open->finalize->open in adjacent blocks and
        // compound the per-rebalance NAV-leak ceiling. Requiring the post-rebalance drift
        // bound to sit strictly below the trigger threshold means a successful finalize
        // leaves the basket below the re-trigger point, so the next drift open needs fresh
        // market movement. Only meaningful when drift triggering is enabled; the [MIN, MAX]
        // floor above plus the config-time `threshold > MIN` rule keep this range non-empty.
        if ctx.accounts.index.fixed_weight_drift_threshold_bps > 0 {
            require!(
                args.max_post_rebalance_drift_bps
                    < ctx.accounts.index.fixed_weight_drift_threshold_bps,
                BasketError::InvalidFixedWeightConfig
            );
        }
        require!(
            args.max_price_age_slots > 0
                && args.max_price_age_slots <= MAX_PRICE_AGE_SLOTS,
            BasketError::InvalidOraclePriceAge
        );
        let now = Clock::get()?.unix_timestamp;
        require!(
            args.expires_at > now
                && args.expires_at <= now + MAX_LARGE_BASKET_INTENT_TTL_SECONDS,
            BasketError::InvalidLargeBasketIntentExpiry
        );

        create_vault_quote_ata(
            &ctx.accounts.associated_token_program,
            &ctx.accounts.initiator,
            &ctx.accounts.vault_quote_token_account,
            &ctx.accounts.vault_authority,
            &ctx.accounts.quote_mint,
            &ctx.accounts.system_program,
            &ctx.accounts.quote_token_program.to_account_info(),
        )?;
        let scratch_quote_atoms =
            load_interface_token_account(&ctx.accounts.vault_quote_token_account.to_account_info())?
                .amount;

        let supply = load_mint(&ctx.accounts.index_mint.to_account_info())?.supply;
        require!(supply > 0, BasketError::InvalidIndexAmount);

        let prices = Prices {
            board: &ctx.accounts.price_board,
            slot: Clock::get()?.slot,
            max_age_slots: args.max_price_age_slots,
        };

        // remaining_accounts = [ordered component pages..] ++ [component vaults in global order..]
        let component_count = usize::from(ctx.accounts.index.large_basket_component_count);
        let page_count = usize::from(ctx.accounts.index.large_basket_page_count);
        require!(
            ctx.remaining_accounts.len() == page_count + component_count,
            BasketError::InvalidRemainingAccounts
        );
        let (page_infos, vault_infos) = ctx.remaining_accounts.split_at(page_count);
        let components =
            load_components_in_order(&ctx.accounts.index.key(), ctx.program_id, &ctx.accounts.index, page_infos)?;
        require!(
            components.len() == component_count,
            BasketError::InvalidRemainingAccounts
        );

        require!(components.iter().any(|c| c.mint == USDC_MINT), BasketError::InvalidFixedWeightConfig);
        // Pass 1: price every component, accumulate NAV.
        let mut snapshots = Vec::with_capacity(component_count);
        let mut open_amounts = Vec::with_capacity(component_count);
        let mut total_value: u128 = 0;
        let mut has_quote_component = false;
        for (i, component) in components.iter().enumerate() {
            let vault_info = &vault_infos[i];
            require_keys_eq!(
                vault_info.key(),
                component.vault,
                BasketError::InvalidVaultAccount
            );
            let is_quote = component.mint == USDC_MINT;
            if is_quote {
                // A USDC component's vault IS the rebalance scratch ATA (same owner, mint
                // and token program). Pin that aliasing so the scratch/dust accounting
                // below can rely on it.
                require_keys_eq!(
                    component.vault,
                    ctx.accounts.vault_quote_token_account.key(),
                    BasketError::InvalidVaultAccount
                );
                has_quote_component = true;
            }
            let current_amount = load_interface_token_account(vault_info)?.amount;
            let retired = is_retired(component);
            let (oracle_price, value) = if retired {
                (0, 0)
            } else {
                let oracle_price = prices.of(component)?;
                (
                    oracle_price,
                    component_value_scaled(current_amount, component.decimals, oracle_price)?,
                )
            };
            total_value = total_value
                .checked_add(value)
                .ok_or_else(|| error!(BasketError::ArithmeticOverflow))?;
            open_amounts.push(current_amount);
            snapshots.push(ComponentSnapshot {
                decimals: component.decimals,
                target_weight_bps: component.target_weight_bps,
                oracle_price,
                current_amount,
                accounted_reserve: component.accounted_reserve,
                is_quote,
                retired,
                value,
            });
        }
        if !has_quote_component {
            // USDC parked in the scratch ATA (dust or proceeds of an unwound rebalance)
            // belongs to holders: count it in NAV so the weight-derived buy targets grow
            // to recycle it back into components this rebalance.
            let parked_value = component_value_scaled(
                scratch_quote_atoms,
                USDC_DECIMALS,
                PRICE_SCALE as i128,
            )?;
            total_value = total_value
                .checked_add(parked_value)
                .ok_or_else(|| error!(BasketError::ArithmeticOverflow))?;
        }
        require!(total_value > 0, BasketError::InvalidOraclePrice);

        // Drift / time gate.
        let (max_drift_bps, drift_triggered) =
            drift_status(&snapshots, total_value, ctx.accounts.index.fixed_weight_drift_threshold_bps)?;
        let time_triggered = ctx.accounts.index.fixed_weight_rebalance_interval_seconds > 0
            && now
                >= ctx
                    .accounts
                    .index
                    .fixed_weight_last_rebalanced_at
                    .checked_add(ctx.accounts.index.fixed_weight_rebalance_interval_seconds)
                    .ok_or_else(|| error!(BasketError::ArithmeticOverflow))?;
        // An applied composition change leaves holdings off the new targets, even when
        // the shift is smaller than the drift threshold.
        let composition_triggered = ctx.accounts.index.composition_rebalance_due;
        require!(
            drift_triggered || time_triggered || composition_triggered,
            BasketError::RebalanceNotNeeded
        );

        // Pass 2: derive each component's target backing and swap leg.
        let mut legs = vec![0u64; component_count];
        let mut sell_leg = [0u8; LARGE_BASKET_COMPONENT_BITMAP_BYTES];
        let mut buy_leg = [0u8; LARGE_BASKET_COMPONENT_BITMAP_BYTES];
        let mut sell_done = [0u8; LARGE_BASKET_COMPONENT_BITMAP_BYTES];
        let mut buy_done = [0u8; LARGE_BASKET_COMPONENT_BITMAP_BYTES];
        let mut sell_legs: u16 = 0;
        let mut buy_legs: u16 = 0;
        // Components with no swap leg (quote / already on target) have nothing to
        // price-verify, so they are pre-marked verified; the keeper only verifies real legs.
        let mut verified = [0u8; LARGE_BASKET_COMPONENT_BITMAP_BYTES];
        for (i, snap) in snapshots.iter().enumerate() {
            let idx = i as u16;
            match plan_leg(snap, total_value)? {
                // Quote, retired, removed dust, or already on target: nothing to swap or verify.
                LegPlan::Done => {
                    bitmap_set_once(&mut sell_done, idx)?;
                    bitmap_set_once(&mut buy_done, idx)?;
                    bitmap_set_once(&mut verified, idx)?;
                }
                LegPlan::Sell(amount) => {
                    legs[i] = amount;
                    bitmap_set_once(&mut sell_leg, idx)?;
                    bitmap_set_once(&mut buy_done, idx)?; // no buy needed
                    sell_legs += 1;
                }
                LegPlan::Buy(amount) => {
                    legs[i] = amount;
                    bitmap_set_once(&mut buy_leg, idx)?;
                    bitmap_set_once(&mut sell_done, idx)?; // no sell needed
                    buy_legs += 1;
                }
            }
        }

        let completed_sells = (component_count as u16) - sell_legs;
        let completed_buys = (component_count as u16) - buy_legs;
        let total_nav_nad = total_value
            .checked_div(PRICE_NAD_SCALE_FACTOR)
            .ok_or_else(|| error!(BasketError::ArithmeticOverflow))?;

        let intent = &mut ctx.accounts.intent;
        intent.index = ctx.accounts.index.key();
        intent.initiator = ctx.accounts.initiator.key();
        intent.nonce = args.nonce;
        intent.mode = RebalanceMode::KeeperDrift;
        intent.status = RebalanceStatus::Open;
        intent.supply_snapshot = supply;
        intent.opened_at = now;
        intent.expires_at = args.expires_at;
        intent.component_count = component_count as u16;
        intent.target_generation = ctx.accounts.index.page_generation;
        intent.component_target_amounts = legs;
        intent.sell_done_bitmap = sell_done;
        intent.buy_done_bitmap = buy_done;
        intent.completed_sells = completed_sells;
        intent.completed_buys = completed_buys;
        intent.component_quote_atoms = vec![0u64; component_count];
        intent.component_verified_bitmap = verified;
        intent.old_nav_nad = total_nav_nad;
        intent.new_nav_nad = 0;
        intent.nav_priced_bitmap = [0u8; LARGE_BASKET_COMPONENT_BITMAP_BYTES];
        intent.total_value_snapshot = total_value;
        intent.nav_tolerance_bps = args.nav_tolerance_bps;
        intent.max_post_rebalance_drift_bps = args.max_post_rebalance_drift_bps;
        intent.bump = ctx.bumps.intent;
        intent.sell_leg_bitmap = sell_leg;
        intent.buy_leg_bitmap = buy_leg;
        intent.component_fill_atoms = vec![0u64; component_count];
        intent.component_open_amounts = open_amounts;
        intent.open_scratch_quote_atoms = scratch_quote_atoms;
        intent.reserved = [0u8; 18];

        ctx.accounts.index.large_basket_operation_in_progress = true;
        ctx.accounts.index.active_rebalance_intent = intent.key();
        // The request has done its job; the operation flag now holds new intents.
        ctx.accounts.index.rebalance_requested = false;

        emit!(RebalanceIntentOpened {
            intent: intent.key(),
            index: ctx.accounts.index.key(),
            initiator: ctx.accounts.initiator.key(),
            nonce: args.nonce,
            component_count: component_count as u16,
            sell_legs,
            buy_legs,
            total_nav_nad,
            max_drift_bps,
            time_triggered,
            drift_triggered,
            expires_at: args.expires_at,
        });
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Execute (sell / buy batch)
// ---------------------------------------------------------------------------

impl<'info> ExecuteRebalanceBatch<'info> {
    pub fn handle_sell(
        ctx: Context<'_, '_, 'info, 'info, Self>,
        args: ExecuteRebalanceBatchArgs,
    ) -> Result<()> {
        Self::handle(ctx, args, true)
    }

    pub fn handle_buy(
        ctx: Context<'_, '_, 'info, 'info, Self>,
        args: ExecuteRebalanceBatchArgs,
    ) -> Result<()> {
        Self::handle(ctx, args, false)
    }

    fn handle(
        ctx: Context<'_, '_, 'info, 'info, Self>,
        args: ExecuteRebalanceBatchArgs,
        is_sell: bool,
    ) -> Result<()> {
        // Swaps are sized and priced by the caller within the oracle bounds, so only the
        // keeper or authority may run them; anyone else could fill legs at the worst price
        // the bounds allow.
        require!(
            ctx.accounts.index.is_rebalance_operator(&ctx.accounts.keeper.key()),
            BasketError::NotRebalanceOperator
        );
        require!(!args.entries.is_empty(), BasketError::InvalidRemainingAccounts);
        require!(
            args.entries.len() <= MAX_REBALANCE_SWAPS_PER_BATCH,
            BasketError::TooManyRebalanceSwaps
        );
        require!(
            ctx.accounts.intent.status == RebalanceStatus::Open,
            BasketError::InvalidLargeBasketIntent
        );
        require_keys_eq!(
            ctx.accounts.index.active_rebalance_intent,
            ctx.accounts.intent.key(),
            BasketError::InvalidLargeBasketIntent
        );
        // No swaps past expiry (the permissionless unwind takes over from there) and
        // none while the authority has paused rebalancing — pausing must be able to
        // halt an in-flight intent, not just block new ones. Verify/finalize/unwind
        // stay available: they only close out value that has already moved.
        require!(
            Clock::get()?.unix_timestamp <= ctx.accounts.intent.expires_at,
            BasketError::RebalanceIntentExpired
        );
        require!(
            !ctx.accounts.index.rebalancing_paused,
            BasketError::RebalancingPaused
        );
        validate_vault_quote_account(
            &ctx.accounts.vault_quote_token_account,
            &ctx.accounts.vault_authority,
            &ctx.accounts.quote_mint,
            &ctx.accounts.quote_token_program.to_account_info(),
        )?;
        let vault_quote_info = ctx.accounts.vault_quote_token_account.to_account_info();
        let vault_authority_bump = ctx.accounts.index.vault_authority_bump;
        let index_key = ctx.accounts.index.key();

        let shared_head: [AccountInfo<'info>; 8] = [
            ctx.accounts.keeper.to_account_info(),
            ctx.accounts.index.to_account_info(),
            ctx.accounts.vault_authority.to_account_info(),
            ctx.accounts.quote_mint.to_account_info(),
            ctx.accounts.vault_quote_token_account.to_account_info(),
            ctx.accounts.jupiter_program.to_account_info(),
            ctx.accounts.associated_token_program.to_account_info(),
            ctx.accounts.quote_token_program.to_account_info(),
        ];

        // Value protection must be atomic with the CPI: deferred verification cannot
        // undo an already committed bad trade, especially if the intent is unwound.
        require!(args.max_oracle_slippage_bps <= MAX_FIXED_WEIGHT_EXECUTION_SLIPPAGE_BPS,
            BasketError::InvalidOraclePriceTolerance);
        validate_price_age_slots(args.max_price_age_slots)?;
        let prices = Prices {
            board: &ctx.accounts.price_board,
            slot: Clock::get()?.slot,
            max_age_slots: args.max_price_age_slots,
        };

        let mut cursor = 0usize;
        for entry in &args.entries {
            let route_count = usize::from(entry.route_account_count);
            let group_len = 4 + route_count;
            require!(
                cursor + group_len <= ctx.remaining_accounts.len(),
                BasketError::InvalidRemainingAccounts
            );
            let page_info = &ctx.remaining_accounts[cursor];
            let mint_info = &ctx.remaining_accounts[cursor + 1];
            let vault_info = &ctx.remaining_accounts[cursor + 2];
            let token_program_info = &ctx.remaining_accounts[cursor + 3];
            let route_accounts = &ctx.remaining_accounts[cursor + 4..cursor + group_len];
            cursor += group_len;

            let component_index = entry.component_index;
            let component = validate_rebalance_component(
                &index_key,
                ctx.program_id,
                page_info,
                component_index,
                mint_info,
                vault_info,
                token_program_info,
            )?;
            require!(
                component.mint != USDC_MINT,
                BasketError::InvalidJupiterRoute
            );

            // Direction + not-yet-done guards.
            let (leg_bitmap, done_bitmap) = if is_sell {
                (&ctx.accounts.intent.sell_leg_bitmap, &ctx.accounts.intent.sell_done_bitmap)
            } else {
                (&ctx.accounts.intent.buy_leg_bitmap, &ctx.accounts.intent.buy_done_bitmap)
            };
            require!(
                bitmap_get(leg_bitmap, component_index)?,
                BasketError::InvalidRebalanceSwap
            );
            require!(
                !bitmap_get(done_bitmap, component_index)?,
                BasketError::LargeBasketComponentAlreadyFilled
            );

            let leg = intent_leg_amount(&ctx.accounts.intent, component_index)?;
            require!(leg > 0, BasketError::InvalidRebalanceSwap);

            // Buys deposit into the component vault, which must exist (idempotent create).
            if !is_sell {
                create_associated_token_account_idempotent_for_token_program(
                    ctx.accounts.associated_token_program.to_account_info(),
                    ctx.accounts.keeper.to_account_info(),
                    vault_info.clone(),
                    ctx.accounts.vault_authority.to_account_info(),
                    mint_info.clone(),
                    ctx.accounts.system_program.to_account_info(),
                    token_program_info.clone(),
                )?;
            }

            let candidates = rebalance_candidates(
                &shared_head,
                page_info,
                mint_info,
                vault_info,
                token_program_info,
                &ctx.accounts.associated_token_program.to_account_info(),
                &ctx.accounts.quote_token_program.to_account_info(),
                &ctx.accounts.system_program.to_account_info(),
                route_accounts,
            );

            // Sell: component_vault -> vault_quote. Buy: vault_quote -> component_vault.
            let (source_vault, dest_vault) = if is_sell {
                (vault_info, &vault_quote_info)
            } else {
                (&vault_quote_info, vault_info)
            };
            let (source_spent, dest_received) = execute_rebalance_swap(
                &ctx.accounts.jupiter_program.to_account_info(),
                &ctx.accounts.vault_authority.to_account_info(),
                &index_key,
                vault_authority_bump,
                source_vault,
                dest_vault,
                &candidates,
                &entry.swap,
            )?;

            let (component_atoms, quote_atoms) = if is_sell {
                // Sold exactly `leg` component for `dest_received` USDC.
                require!(source_spent == leg, BasketError::RebalanceWouldSellTargetBacking);
                require!(
                    dest_received >= entry.quote_limit,
                    BasketError::QuoteBudgetExceeded
                );
                (source_spent, dest_received)
            } else {
                // Buy backing may absorb the bounded trading-cost allowance.
                let open_amount = *ctx.accounts.intent.component_open_amounts
                    .get(usize::from(component_index))
                    .ok_or_else(|| error!(BasketError::InvalidLargeBasketIntent))?;
                let minimum = minimum_rebalance_buy_amount(
                    open_amount, leg, ctx.accounts.intent.nav_tolerance_bps)?;
                require!(dest_received >= minimum, BasketError::RebalanceTargetNotMet);
                let oracle_price = prices.of(&component)?;
                let max_spend = maximum_rebalance_buy_quote(leg, component.decimals,
                    oracle_price, args.max_oracle_slippage_bps)?;
                require!(source_spent <= max_spend, BasketError::QuoteBudgetExceeded);
                require!(
                    source_spent <= entry.quote_limit,
                    BasketError::QuoteBudgetExceeded
                );
                (dest_received, source_spent)
            };

            validate_atomic_rebalance_fill(is_sell, quote_atoms, component_atoms,
                component.decimals, prices.of(&component)?, args.max_oracle_slippage_bps)?;

            // Only persist completion after the atomic price check succeeds.
            let intent = &mut ctx.accounts.intent;
            set_component_quote_atoms(intent, component_index, quote_atoms)?;
            set_component_fill_atoms(intent, component_index, component_atoms)?;
            bitmap_set_once(&mut intent.component_verified_bitmap, component_index)?;
            if is_sell {
                bitmap_set_once(&mut intent.sell_done_bitmap, component_index)?;
                intent.completed_sells = intent
                    .completed_sells
                    .checked_add(1)
                    .ok_or_else(|| error!(BasketError::ArithmeticOverflow))?;
            } else {
                bitmap_set_once(&mut intent.buy_done_bitmap, component_index)?;
                intent.completed_buys = intent
                    .completed_buys
                    .checked_add(1)
                    .ok_or_else(|| error!(BasketError::ArithmeticOverflow))?;
            }

            emit!(RebalanceComponentSwapped {
                intent: intent.key(),
                index: index_key,
                component_index,
                is_sell,
                component_atoms,
                quote_atoms,
            });
        }
        require!(
            cursor == ctx.remaining_accounts.len(),
            BasketError::InvalidRemainingAccounts
        );
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Verify (deferred per-leg oracle bound)
// ---------------------------------------------------------------------------

impl<'info> VerifyRebalanceComponentPrice<'info> {
    pub fn handle(
        ctx: Context<'_, '_, 'info, 'info, Self>,
        args: VerifyRebalanceComponentPriceArgs,
    ) -> Result<()> {
        require!(
            ctx.accounts.index.is_rebalance_operator(&ctx.accounts.keeper.key()),
            BasketError::NotRebalanceOperator
        );
        require!(
            args.max_oracle_slippage_bps <= MAX_FIXED_WEIGHT_EXECUTION_SLIPPAGE_BPS,
            BasketError::InvalidOraclePriceTolerance
        );
        require!(
            ctx.accounts.intent.status == RebalanceStatus::Open,
            BasketError::InvalidLargeBasketIntent
        );
        let component_index = args.component_index;
        let is_sell = bitmap_get(&ctx.accounts.intent.sell_leg_bitmap, component_index)?;
        let is_buy = bitmap_get(&ctx.accounts.intent.buy_leg_bitmap, component_index)?;

        // The leg must be executed before it can be price-verified.
        if is_sell {
            require!(
                bitmap_get(&ctx.accounts.intent.sell_done_bitmap, component_index)?,
                BasketError::LargeBasketComponentNotFilled
            );
        } else if is_buy {
            require!(
                bitmap_get(&ctx.accounts.intent.buy_done_bitmap, component_index)?,
                BasketError::LargeBasketComponentNotFilled
            );
        }

        let page = &ctx.accounts.component_page;
        validate_page_identity(&ctx.accounts.intent.index, &page.key(), ctx.program_id, page)?;
        let local = page.component_offset(component_index)?;
        let component = &page.components[local];
        let leg = intent_leg_amount(&ctx.accounts.intent, component_index)?;
        let quote = intent_leg_quote_atoms(&ctx.accounts.intent, component_index)?;
        // The actual atoms the executed swap moved (== leg for sells; >= leg for buys on
        // over-delivery). The effective price must be computed against the real fill or
        // an over-delivered buy would look more expensive per atom than it was.
        let fill = intent_fill_atoms(&ctx.accounts.intent, component_index)?;

        if leg > 0 {
            require!(fill > 0, BasketError::InvalidLargeBasketIntent);
            let oracle_price = Prices {
                board: &ctx.accounts.price_board,
                slot: Clock::get()?.slot,
                max_age_slots: args.max_price_age_slots,
            }
            .of(component)?;
            if is_sell {
                validate_sell_execution_price(
                    quote,
                    fill,
                    USDC_DECIMALS,
                    component.decimals,
                    oracle_price,
                    args.max_oracle_slippage_bps,
                )?;
            } else if is_buy {
                validate_buy_execution_price(
                    quote,
                    fill,
                    USDC_DECIMALS,
                    component.decimals,
                    oracle_price,
                    args.max_oracle_slippage_bps,
                )?;
            }
        }

        bitmap_set_once(&mut ctx.accounts.intent.component_verified_bitmap, component_index)?;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Finalize
// ---------------------------------------------------------------------------

impl<'info> FinalizeRebalance<'info> {
    pub fn handle(
        ctx: Context<'_, '_, 'info, 'info, Self>,
        args: FinalizeRebalanceArgs,
    ) -> Result<()> {
        // Finalize rewrites the basket's accounting; past expiry anyone can still unwind.
        require!(
            ctx.accounts.index.is_rebalance_operator(&ctx.accounts.keeper.key()),
            BasketError::NotRebalanceOperator
        );
        require!(
            ctx.accounts.intent.status == RebalanceStatus::Open,
            BasketError::InvalidLargeBasketIntent
        );
        require!(
            ctx.accounts.intent.mode == RebalanceMode::KeeperDrift,
            BasketError::InvalidIndexKind
        );
        require_keys_eq!(
            ctx.accounts.index.active_rebalance_intent,
            ctx.accounts.intent.key(),
            BasketError::InvalidLargeBasketIntent
        );
        let count = ctx.accounts.intent.component_count;
        require!(
            bitmap_all_set(&ctx.accounts.intent.sell_done_bitmap, count)?,
            BasketError::LargeBasketComponentNotFilled
        );
        require!(
            bitmap_all_set(&ctx.accounts.intent.buy_done_bitmap, count)?,
            BasketError::LargeBasketComponentNotFilled
        );
        require!(
            bitmap_all_set(&ctx.accounts.intent.component_verified_bitmap, count)?,
            BasketError::LargeBasketComponentNotVerified
        );
        validate_vault_quote_account(
            &ctx.accounts.vault_quote_token_account,
            &ctx.accounts.vault_authority,
            &ctx.accounts.quote_mint,
            &ctx.accounts.quote_token_program.to_account_info(),
        )?;

        let supply = ctx.accounts.intent.supply_snapshot;
        let base_units = ctx.accounts.index.index_base_units()?;
        validate_price_age_slots(args.max_price_age_slots)?;
        let prices = Prices {
            board: &ctx.accounts.price_board,
            slot: Clock::get()?.slot,
            max_age_slots: args.max_price_age_slots,
        };

        let component_count = usize::from(ctx.accounts.index.large_basket_component_count);
        let page_count = usize::from(ctx.accounts.index.large_basket_page_count);
        require!(
            ctx.remaining_accounts.len() == page_count + component_count,
            BasketError::InvalidRemainingAccounts
        );
        let (page_infos, vault_infos) = ctx.remaining_accounts.split_at(page_count);

        // Load writable page Accounts in page-index order; mutate + persist via exit().
        let index_key = ctx.accounts.index.key();
        let mut pages =
            load_writable_pages_in_order(&index_key, ctx.program_id, component_count, page_infos)?;
        let amounts = collect_vault_amounts(&pages, vault_infos)?;

        // The scratch ATA is part of NAV: as the USDC component's vault when one exists
        // (the aliasing is pinned at open and re-checked below), otherwise as parked
        // quote value on top of the component vaults — mirroring how open priced it.
        let scratch_atoms = load_interface_token_account(
            &ctx.accounts.vault_quote_token_account.to_account_info(),
        )?
        .amount;
        let scratch_value =
            component_value_scaled(scratch_atoms, USDC_DECIMALS, PRICE_SCALE as i128)?;

        // Re-price BOTH the post-rebalance NAV and the pre-rebalance holdings at the SAME
        // fresh oracle. `expected_value_now` is what the basket would be worth had the
        // rebalance not happened (open-time amounts at current prices); gating against it
        // instead of the open-time priced snapshot nets out market drift during the
        // intent's life, so a keeper cannot pass the NAV gate by leaking execution value
        // into appreciation that accrued in the window.
        let open_amounts = &ctx.accounts.intent.component_open_amounts;
        require!(
            open_amounts.len() == component_count,
            BasketError::InvalidLargeBasketIntent
        );
        let mut final_total_value: u128 = 0;
        let mut expected_value_now: u128 = 0;
        let mut values = Vec::with_capacity(component_count);
        let mut usdc_component_weight_bps: Option<u16> = None;
        {
            let mut global = 0usize;
            for page in pages.iter() {
                for component in page.components.iter() {
                    if component.mint == USDC_MINT {
                        require_keys_eq!(
                            component.vault,
                            ctx.accounts.vault_quote_token_account.key(),
                            BasketError::InvalidVaultAccount
                        );
                        usdc_component_weight_bps = Some(component.target_weight_bps);
                    }
                    let (value, open_value) =
                        if is_retired(component) {
                            (0, 0)
                        } else {
                            let oracle_price = prices.of(component)?;
                            (
                                component_value_scaled(
                                    amounts[global],
                                    component.decimals,
                                    oracle_price,
                                )?,
                                component_value_scaled(
                                    open_amounts[global],
                                    component.decimals,
                                    oracle_price,
                                )?,
                            )
                        };
                    final_total_value = final_total_value
                        .checked_add(value)
                        .ok_or_else(|| error!(BasketError::ArithmeticOverflow))?;
                    expected_value_now = expected_value_now
                        .checked_add(open_value)
                        .ok_or_else(|| error!(BasketError::ArithmeticOverflow))?;
                    // What is left of a removed component is being written off (sent to its
                    // vault, or dust), so it does not count as drift from its zero target.
                    let drift_value = if sold_out_at_finalize(component) { 0 } else { value };
                    values.push((drift_value, component.target_weight_bps));
                    global += 1;
                }
            }
        }
        if usdc_component_weight_bps.is_none() {
            // No USDC component: the scratch ATA is parked quote on both sides — final
            // balance for the post-NAV, open balance for the held-still reference.
            final_total_value = final_total_value
                .checked_add(scratch_value)
                .ok_or_else(|| error!(BasketError::ArithmeticOverflow))?;
            let open_scratch_value = component_value_scaled(
                ctx.accounts.intent.open_scratch_quote_atoms,
                USDC_DECIMALS,
                PRICE_SCALE as i128,
            )?;
            expected_value_now = expected_value_now
                .checked_add(open_scratch_value)
                .ok_or_else(|| error!(BasketError::ArithmeticOverflow))?;
        }
        require!(final_total_value > 0, BasketError::InvalidOraclePrice);

        // Post-rebalance drift must be within tolerance. With no USDC component, parked
        // scratch value inflates the denominator and depresses every actual weight, so
        // this also forces the keeper to have recycled parked USDC into components.
        let mut post_drift_bps = 0u16;
        for (value, target_weight_bps) in &values {
            let drift = weight_drift_bps(*value, final_total_value, *target_weight_bps)?;
            post_drift_bps = post_drift_bps.max(drift);
        }
        require!(
            post_drift_bps <= ctx.accounts.intent.max_post_rebalance_drift_bps,
            BasketError::RebalanceTargetNotMet
        );

        // NAV preservation, one-sided: the post-rebalance basket must not be worth more
        // than `nav_tolerance_bps` LESS than holding the original basket would be worth
        // right now (both priced at this same fresh oracle, so market drift over the
        // intent's life nets out and a gain never blocks finalize).
        //
        // This is a BACKSTOP, not the binding value bound. The per-leg
        // atomic execution-price check caps every executed leg's effective
        // price at `max_oracle_slippage_bps` of that leg's fresh oracle, which is what
        // actually bounds extractable value (≤ that fraction of the traded notional). The
        // aggregate gate here re-marks at the finalize price vector, which the keeper picks
        // among valid recent quotes; that freedom lets the changed-leg term be tilted
        // within genuine intra-window oracle volatility, so this gate cannot tighten BELOW
        // the per-leg bound — it only catches gross aggregate shortfalls the per-leg checks
        // somehow let through. Do not treat `nav_tolerance_bps` as a hard standalone bound.
        require!(
            nav_loss_within_tolerance_u128(
                expected_value_now,
                final_total_value,
                ctx.accounts.intent.nav_tolerance_bps,
            )?,
            BasketError::RebalanceNavMismatch
        );

        // Leftover quote in the scratch ATA must be bounded. When USDC is a component
        // the scratch IS its vault, so only the EXCESS over that component's target
        // backing counts as dust (its weight deviation is already drift-bounded above).
        let allowed_dust_value = final_total_value
            .checked_mul(u128::from(MAX_FIXED_WEIGHT_QUOTE_DUST_BPS))
            .and_then(|v| v.checked_div(u128::from(BPS_DENOMINATOR)))
            .ok_or_else(|| error!(BasketError::ArithmeticOverflow))?;
        let usdc_component_target_value = match usdc_component_weight_bps {
            Some(weight_bps) => final_total_value
                .checked_mul(u128::from(weight_bps))
                .and_then(|v| v.checked_div(u128::from(BPS_DENOMINATOR)))
                .ok_or_else(|| error!(BasketError::ArithmeticOverflow))?,
            None => 0,
        };
        let dust_value = scratch_value.saturating_sub(usdc_component_target_value);
        require!(dust_value <= allowed_dust_value, BasketError::RebalanceTargetNotMet);

        // Rewrite each component's units_per_index + accounted_reserve from the new balances.
        // Every zero-weight component has been sold in full (or was dust): anything its vault
        // holds now arrived after open and is left unaccounted.
        let sold_out: Vec<bool> = pages
            .iter()
            .flat_map(|page| page.components.iter())
            .map(sold_out_at_finalize)
            .collect();
        rewrite_pages_from_amounts(&mut pages, &amounts, &sold_out, base_units, supply, true, ctx.program_id)?;

        let now = Clock::get()?.unix_timestamp;
        let new_nav_nad = final_total_value
            .checked_div(PRICE_NAD_SCALE_FACTOR)
            .ok_or_else(|| error!(BasketError::ArithmeticOverflow))?;

        ctx.accounts.index.fixed_weight_last_rebalanced_at = now;
        ctx.accounts.index.composition_rebalance_due = false;
        ctx.accounts.index.large_basket_operation_in_progress = false;
        ctx.accounts.index.active_rebalance_intent = Pubkey::default();
        ctx.accounts.intent.status = RebalanceStatus::Finalized;
        ctx.accounts.intent.new_nav_nad = new_nav_nad;

        emit!(FixedWeightRebalanceExecuted {
            index: ctx.accounts.index.key(),
            executor: ctx.accounts.keeper.key(),
            supply,
            total_nav_nad: new_nav_nad,
            max_drift_bps: post_drift_bps,
            time_triggered: false,
            drift_triggered: false,
        });
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Cancel (only before any leg executed)
// ---------------------------------------------------------------------------

impl<'info> CancelRebalance<'info> {
    pub fn handle(ctx: Context<Self>) -> Result<()> {
        require!(
            ctx.accounts.intent.status == RebalanceStatus::Open,
            BasketError::InvalidLargeBasketIntent
        );
        require_keys_eq!(
            ctx.accounts.index.active_rebalance_intent,
            ctx.accounts.intent.key(),
            BasketError::InvalidLargeBasketIntent
        );
        require_keys_eq!(
            ctx.accounts.initiator.key(),
            ctx.accounts.intent.initiator,
            BasketError::UnauthorizedAuthority
        );
        // No mid-rebalance unwind: cancel is only allowed before any leg has swapped.
        let count = ctx.accounts.intent.component_count;
        let sell_legs = count_set(&ctx.accounts.intent.sell_leg_bitmap, count)?;
        let buy_legs = count_set(&ctx.accounts.intent.buy_leg_bitmap, count)?;
        require!(
            ctx.accounts.intent.completed_sells == count - sell_legs
                && ctx.accounts.intent.completed_buys == count - buy_legs,
            BasketError::InvalidLargeBasketIntent
        );

        ctx.accounts.intent.status = RebalanceStatus::Cancelled;
        ctx.accounts.index.large_basket_operation_in_progress = false;
        ctx.accounts.index.active_rebalance_intent = Pubkey::default();

        emit!(IndexRebalanceCancelled {
            index: ctx.accounts.index.key(),
            authority: ctx.accounts.initiator.key(),
            nonce: ctx.accounts.intent.nonce,
        });
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Unwind (stuck-intent escape hatch)
// ---------------------------------------------------------------------------

impl<'info> UnwindRebalance<'info> {
    /// Abandon an open rebalance regardless of progress: permissionless once the intent
    /// has expired, index-authority-only before that (the emergency stop for an intent
    /// the authority does not trust). All funds stay in vault-authority custody for the
    /// whole rebalance — sells park USDC in the scratch ATA, buys move it into component
    /// vaults — so abandoning is purely an accounting re-sync: rewrite every page's
    /// `units_per_index` / `accounted_reserve` from the live vault balances and release
    /// the lock. USDC parked in the scratch ATA stays there; the next rebalance counts
    /// it in NAV at open and sizes its buy legs to recycle it into components.
    ///
    /// Deliberately oracle-free and value-bound-free. The only way value leaves custody
    /// during a rebalance is the Jupiter swap inside execute, which is itself bounded by
    /// the keeper's per-leg `quote_limit`; any bad-execution loss is therefore ALREADY
    /// realized on-chain before unwind runs — unwind only records the resulting balances,
    /// it cannot create new loss. The deferred per-leg oracle verify and the finalize NAV
    /// gate are intentionally skipped here: requiring a fresh posted price (or any
    /// value gate that can fail) would reintroduce exactly the liveness hole this hatch
    /// exists to close (a down/stale oracle, or an un-passable bound, must never be able
    /// to keep the operation lock stuck). The exposure between execute and unwind is thus
    /// bounded by the atomic oracle tolerance on every executed leg.
    ///
    /// remaining_accounts = [writable component pages in page-index order..]
    ///                   ++ [component vaults in global component order..]
    pub fn handle(ctx: Context<'_, '_, 'info, 'info, Self>) -> Result<()> {
        require!(
            ctx.accounts.intent.status == RebalanceStatus::Open,
            BasketError::InvalidLargeBasketIntent
        );
        require_keys_eq!(
            ctx.accounts.index.active_rebalance_intent,
            ctx.accounts.intent.key(),
            BasketError::InvalidLargeBasketIntent
        );
        let expired = Clock::get()?.unix_timestamp > ctx.accounts.intent.expires_at;
        require!(
            expired || ctx.accounts.caller.key() == ctx.accounts.index.authority,
            BasketError::RebalanceIntentNotExpired
        );

        let component_count = usize::from(ctx.accounts.index.large_basket_component_count);
        let page_count = usize::from(ctx.accounts.index.large_basket_page_count);
        require!(
            ctx.remaining_accounts.len() == page_count + component_count,
            BasketError::InvalidRemainingAccounts
        );
        let (page_infos, vault_infos) = ctx.remaining_accounts.split_at(page_count);

        let index_key = ctx.accounts.index.key();
        let mut pages =
            load_writable_pages_in_order(&index_key, ctx.program_id, component_count, page_infos)?;
        let amounts = collect_vault_amounts(&pages, vault_infos)?;
        // No ZeroComponentUnits gate here: the unwind must never be blockable, and a
        // component balance that floors to zero units is still accounted correctly by
        // `accounted_reserve` (the in-kind mint/redeem basis). A zero-weight component whose
        // sell leg executed was sold in full, so its remaining balance arrived since open.
        let intent = &ctx.accounts.intent;
        let mut sold_out = Vec::with_capacity(component_count);
        for component in pages.iter().flat_map(|page| page.components.iter()) {
            let i = sold_out.len() as u16;
            sold_out.push(sold_out_at_unwind(
                component,
                bitmap_get(&intent.sell_leg_bitmap, i)?,
                bitmap_get(&intent.sell_done_bitmap, i)?,
            ));
        }
        rewrite_pages_from_amounts(
            &mut pages,
            &amounts,
            &sold_out,
            ctx.accounts.index.index_base_units()?,
            ctx.accounts.intent.supply_snapshot,
            false,
            ctx.program_id,
        )?;

        ctx.accounts.intent.status = RebalanceStatus::Cancelled;
        ctx.accounts.index.large_basket_operation_in_progress = false;
        ctx.accounts.index.active_rebalance_intent = Pubkey::default();

        emit!(RebalanceIntentUnwound {
            intent: ctx.accounts.intent.key(),
            index: index_key,
            caller: ctx.accounts.caller.key(),
            nonce: ctx.accounts.intent.nonce,
            expired,
            completed_sells: ctx.accounts.intent.completed_sells,
            completed_buys: ctx.accounts.intent.completed_buys,
        });
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Close (rent recovery)
// ---------------------------------------------------------------------------

impl<'info> CloseRebalanceIntent<'info> {
    /// All checks live in the Accounts constraints: only the initiator may close, and
    /// only once the intent is Finalized or Cancelled (closing an Open intent would
    /// strand the index lock, which points at this account).
    pub fn handle(_ctx: Context<Self>) -> Result<()> {
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

// A removed component worth less than this (USD at oracle price scale: one cent) is left
// unsold by a rebalance and written off at finalize.
const REMOVED_COMPONENT_DUST_VALUE: u128 = PRICE_SCALE / 100;

/// A component a composition change removed (zero weight, not USDC) whose holdings a
/// rebalance has already sold. It is neither priced nor traded, so the oracle can stop
/// pricing it. Judged by the basket's own accounting, not the live vault: anyone can send
/// tokens to a public vault, and dust there must not force a dead token back into pricing.
fn is_retired(component: &LargeBasketComponent) -> bool {
    component.mint != USDC_MINT
        && component.target_weight_bps == 0
        && component.accounted_reserve == 0
}

/// The posted prices a rebalance step reads: each at most `max_age_slots` old at `slot`.
struct Prices<'a> {
    board: &'a PriceBoard,
    slot: u64,
    max_age_slots: u64,
}

impl Prices<'_> {
    /// USD per whole token (PRICE_SCALE). USDC, the quote asset, is always worth $1.
    fn of(&self, component: &LargeBasketComponent) -> Result<i128> {
        if component.mint == USDC_MINT {
            Ok(PRICE_SCALE as i128)
        } else {
            board_price(self.board, &component.mint, self.slot, self.max_age_slots)
        }
    }
}

fn component_value_scaled(amount: u64, decimals: u8, oracle_price: i128) -> Result<u128> {
    require!(oracle_price > 0, BasketError::InvalidOraclePrice);
    let oracle_price =
        u128::try_from(oracle_price).map_err(|_| error!(BasketError::InvalidOraclePrice))?;
    let denominator = pow10_u128(decimals)?;
    u128::from(amount)
        .checked_mul(oracle_price)
        .and_then(|v| v.checked_div(denominator))
        .ok_or_else(|| error!(BasketError::ArithmeticOverflow))
}

fn target_amount_for_value_scaled(value: u128, decimals: u8, oracle_price: i128) -> Result<u64> {
    require!(oracle_price > 0, BasketError::InvalidOraclePrice);
    let oracle_price =
        u128::try_from(oracle_price).map_err(|_| error!(BasketError::InvalidOraclePrice))?;
    let amount = value
        .checked_mul(pow10_u128(decimals)?)
        .and_then(|v| v.checked_div(oracle_price))
        .ok_or_else(|| error!(BasketError::ArithmeticOverflow))?;
    u64::try_from(amount).map_err(|_| error!(BasketError::ArithmeticOverflow))
}

fn weight_drift_bps(value: u128, total_value: u128, target_weight_bps: u16) -> Result<u16> {
    let actual_weight_bps = value
        .checked_mul(u128::from(BPS_DENOMINATOR))
        .and_then(|v| v.checked_div(total_value))
        .ok_or_else(|| error!(BasketError::ArithmeticOverflow))?;
    let actual_weight_bps =
        u16::try_from(actual_weight_bps).map_err(|_| error!(BasketError::ArithmeticOverflow))?;
    Ok(actual_weight_bps.abs_diff(target_weight_bps))
}

fn drift_status(
    snapshots: &[ComponentSnapshot],
    total_value: u128,
    drift_threshold_bps: u16,
) -> Result<(u16, bool)> {
    let mut max_drift_bps = 0u16;
    for snap in snapshots {
        let drift = weight_drift_bps(snap.value, total_value, snap.target_weight_bps)?;
        max_drift_bps = max_drift_bps.max(drift);
    }
    Ok((
        max_drift_bps,
        drift_threshold_bps > 0 && max_drift_bps >= drift_threshold_bps,
    ))
}

// Cap spending by the intended leg, even if a route over-delivers tokens.
fn maximum_rebalance_buy_quote(leg: u64, decimals: u8, price: i128, slippage_bps: u16) -> Result<u64> {
    require!(slippage_bps <= MAX_FIXED_WEIGHT_EXECUTION_SLIPPAGE_BPS,
        BasketError::InvalidOraclePriceTolerance);
    let value = component_value_scaled(leg, decimals, price)?;
    let numerator = value.checked_mul(10_000 + u128::from(slippage_bps))
        .and_then(|v| v.checked_mul(1_000_000))
        .ok_or_else(|| error!(BasketError::ArithmeticOverflow))?;
    let denominator = PRICE_SCALE * 10_000;
    let atoms = numerator / denominator + u128::from(numerator % denominator != 0);
    u64::try_from(atoms).map_err(|_| error!(BasketError::ArithmeticOverflow))
}

// Trading costs may reduce backing only within the existing intent loss allowance.
fn minimum_rebalance_buy_amount(open: u64, leg: u64, tolerance_bps: u16) -> Result<u64> {
    require!(tolerance_bps <= MAX_KEEPER_NAV_TOLERANCE_BPS, BasketError::InvalidNavTolerance);
    let target = open.checked_add(leg)
        .ok_or_else(|| error!(BasketError::ArithmeticOverflow))?;
    let reduction = (u128::from(target) * u128::from(tolerance_bps)) / 10_000;
    Ok(leg.saturating_sub(reduction as u64).max(1))
}

/// True if current and target are within 1 bps of each other.
fn leg_is_dust(current: u64, target: u64) -> bool {
    let diff = current.abs_diff(target);
    if diff == 0 {
        return true;
    }
    // diff * 10000 < target  <=>  diff < target / 10000  (1 bps of target)
    (diff as u128).saturating_mul(u128::from(BPS_DENOMINATOR)) < u128::from(target)
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

fn count_set(bitmap: &[u8; LARGE_BASKET_COMPONENT_BITMAP_BYTES], count: u16) -> Result<u16> {
    let mut n = 0u16;
    for i in 0..count {
        if bitmap_get(bitmap, i)? {
            n += 1;
        }
    }
    Ok(n)
}

fn intent_leg_amount(intent: &RebalanceIntent, component_index: u16) -> Result<u64> {
    intent
        .component_target_amounts
        .get(usize::from(component_index))
        .copied()
        .ok_or_else(|| error!(BasketError::InvalidLargeBasketIntent))
}

fn intent_leg_quote_atoms(intent: &RebalanceIntent, component_index: u16) -> Result<u64> {
    intent
        .component_quote_atoms
        .get(usize::from(component_index))
        .copied()
        .ok_or_else(|| error!(BasketError::InvalidLargeBasketIntent))
}

fn set_component_quote_atoms(
    intent: &mut RebalanceIntent,
    component_index: u16,
    quote_atoms: u64,
) -> Result<()> {
    let slot = intent
        .component_quote_atoms
        .get_mut(usize::from(component_index))
        .ok_or_else(|| error!(BasketError::InvalidLargeBasketIntent))?;
    *slot = quote_atoms;
    Ok(())
}

fn intent_fill_atoms(intent: &RebalanceIntent, component_index: u16) -> Result<u64> {
    intent
        .component_fill_atoms
        .get(usize::from(component_index))
        .copied()
        .ok_or_else(|| error!(BasketError::InvalidLargeBasketIntent))
}

fn set_component_fill_atoms(
    intent: &mut RebalanceIntent,
    component_index: u16,
    fill_atoms: u64,
) -> Result<()> {
    let slot = intent
        .component_fill_atoms
        .get_mut(usize::from(component_index))
        .ok_or_else(|| error!(BasketError::InvalidLargeBasketIntent))?;
    *slot = fill_atoms;
    Ok(())
}

fn validate_page_identity(
    index_key: &Pubkey,
    page_key: &Pubkey,
    program_id: &Pubkey,
    page: &LargeBasketComponentPage,
) -> Result<()> {
    require_keys_eq!(page.index, *index_key, BasketError::InvalidLargeBasketComponentPage);
    require!(page.finalized, BasketError::LargeBasketNotConfigured);
    let (expected, _) = Pubkey::find_program_address(
        &[LARGE_BASKET_COMPONENT_PAGE_SEED, index_key.as_ref(), &[page.page_index]],
        program_id,
    );
    require_keys_eq!(*page_key, expected, BasketError::InvalidLargeBasketComponentPage);
    Ok(())
}

fn validate_rebalance_component(
    index_key: &Pubkey,
    program_id: &Pubkey,
    page_info: &AccountInfo,
    component_index: u16,
    mint_info: &AccountInfo,
    vault_info: &AccountInfo,
    token_program_info: &AccountInfo,
) -> Result<LargeBasketComponent> {
    require_keys_eq!(
        *page_info.owner,
        *program_id,
        BasketError::InvalidLargeBasketComponentPage
    );
    let page =
        LargeBasketComponentPage::try_deserialize(&mut &page_info.try_borrow_data()?[..])?;
    validate_page_identity(index_key, &page_info.key(), program_id, &page)?;
    let local = page.component_offset(component_index)?;
    let component = page.components[local].clone();
    require_keys_eq!(mint_info.key(), component.mint, BasketError::InvalidComponentMint);
    require_keys_eq!(*mint_info.owner, component.token_program, BasketError::InvalidTokenMint);
    require_keys_eq!(
        token_program_info.key(),
        component.token_program,
        BasketError::InvalidTokenProgram
    );
    require_keys_eq!(vault_info.key(), component.vault, BasketError::InvalidVaultAccount);
    Ok(component)
}

// Pages must be passed in page-index order (0, 1, 2, ...). Returns them in that order so
// callers can align them positionally with the per-component vaults (which are passed in
// the same global order) and write each page back to its own account by position.
fn load_pages_in_order(
    index_key: &Pubkey,
    program_id: &Pubkey,
    index: &IndexState,
    page_infos: &[AccountInfo],
) -> Result<Vec<LargeBasketComponentPage>> {
    require!(
        page_infos.len() == usize::from(index.large_basket_page_count),
        BasketError::InvalidRemainingAccounts
    );
    let mut pages = Vec::with_capacity(page_infos.len());
    let mut expected_start = 0u16;
    for (i, info) in page_infos.iter().enumerate() {
        require_keys_eq!(*info.owner, *program_id, BasketError::InvalidLargeBasketComponentPage);
        let page =
            LargeBasketComponentPage::try_deserialize(&mut &info.try_borrow_data()?[..])?;
        validate_page_identity(index_key, &info.key(), program_id, &page)?;
        require!(
            usize::from(page.page_index) == i,
            BasketError::InvalidLargeBasketComponentPage
        );
        require!(
            page.start_component_index == expected_start,
            BasketError::InvalidLargeBasketComponentPage
        );
        require!(
            usize::from(page.component_count) == page.components.len(),
            BasketError::InvalidLargeBasketComponentPage
        );
        expected_start = expected_start
            .checked_add(page.component_count)
            .ok_or_else(|| error!(BasketError::ArithmeticOverflow))?;
        pages.push(page);
    }
    require!(
        usize::from(expected_start) == usize::from(index.large_basket_component_count),
        BasketError::InvalidRemainingAccounts
    );
    Ok(pages)
}

pub(crate) fn load_components_in_order(
    index_key: &Pubkey,
    program_id: &Pubkey,
    index: &IndexState,
    page_infos: &[AccountInfo],
) -> Result<Vec<LargeBasketComponent>> {
    let pages = load_pages_in_order(index_key, program_id, index, page_infos)?;
    let mut components = Vec::new();
    for page in &pages {
        components.extend(page.components.iter().cloned());
    }
    Ok(components)
}

/// Writable twin of `load_pages_in_order`: loads page Accounts (owner + discriminator
/// checked by `Account::try_from`) in page-index order so callers can mutate them and
/// persist via `exit()`. Same identity/ordering/coverage validations.
pub(crate) fn load_writable_pages_in_order<'info>(
    index_key: &Pubkey,
    program_id: &Pubkey,
    component_count: usize,
    page_infos: &'info [AccountInfo<'info>],
) -> Result<Vec<Account<'info, LargeBasketComponentPage>>> {
    let mut pages: Vec<Account<'info, LargeBasketComponentPage>> =
        Vec::with_capacity(page_infos.len());
    let mut expected_start = 0u16;
    for (i, info) in page_infos.iter().enumerate() {
        require!(info.is_writable, BasketError::InvalidRemainingAccounts);
        let page: Account<'info, LargeBasketComponentPage> = Account::try_from(info)?;
        validate_page_identity(index_key, &info.key(), program_id, &page)?;
        require!(
            usize::from(page.page_index) == i,
            BasketError::InvalidLargeBasketComponentPage
        );
        require!(
            page.start_component_index == expected_start,
            BasketError::InvalidLargeBasketComponentPage
        );
        require!(
            usize::from(page.component_count) == page.components.len(),
            BasketError::InvalidLargeBasketComponentPage
        );
        expected_start = expected_start
            .checked_add(page.component_count)
            .ok_or_else(|| error!(BasketError::ArithmeticOverflow))?;
        pages.push(page);
    }
    require!(
        usize::from(expected_start) == component_count,
        BasketError::InvalidRemainingAccounts
    );
    Ok(pages)
}

/// Reads every component vault's live balance, positionally aligned with the pages'
/// global component order and key-checked against each component's stored vault.
fn collect_vault_amounts<'info>(
    pages: &[Account<'info, LargeBasketComponentPage>],
    vault_infos: &[AccountInfo<'info>],
) -> Result<Vec<u64>> {
    let mut amounts = Vec::with_capacity(vault_infos.len());
    let mut global = 0usize;
    for page in pages.iter() {
        for component in page.components.iter() {
            let vault_info = vault_infos
                .get(global)
                .ok_or_else(|| error!(BasketError::InvalidRemainingAccounts))?;
            require_keys_eq!(
                vault_info.key(),
                component.vault,
                BasketError::InvalidVaultAccount
            );
            amounts.push(load_interface_token_account(vault_info)?.amount);
            global += 1;
        }
    }
    require!(
        global == vault_infos.len(),
        BasketError::InvalidRemainingAccounts
    );
    Ok(amounts)
}

/// Re-derives every component's `units_per_index` / `accounted_reserve` from the given
/// vault balances and persists the pages.
///
/// `require_nonzero_units` distinguishes the two callers:
///   * finalize (`true`): a weighted component flooring to zero units is a failed
///     rebalance, and unit derivation uses the checked math (overflow is a real error).
///   * unwind (`false`): the re-sync MUST NOT be blockable, so unit derivation saturates
///     instead of erroring. A griefer can airdrop tokens into a public component vault to
///     drive `amount * base_units / supply` past `u64::MAX`; with checked math that would
///     revert the unwind and strand the operation lock forever. `accounted_reserve` (the
///     live mint/redeem basis) takes the real `u64` balance regardless; `units_per_index`
///     only matters when supply returns to zero, so a saturated value there is benign.
fn rewrite_pages_from_amounts<'info>(
    pages: &mut [Account<'info, LargeBasketComponentPage>],
    amounts: &[u64],
    sold_out: &[bool],
    base_units: u64,
    supply: u64,
    require_nonzero_units: bool,
    program_id: &Pubkey,
) -> Result<()> {
    let mut global = 0usize;
    for page in pages.iter_mut() {
        for component in page.components.iter_mut() {
            let (units, reserve) = rewritten_accounting(
                component,
                amounts[global],
                sold_out[global],
                base_units,
                supply,
                require_nonzero_units,
            )?;
            component.units_per_index = units;
            component.accounted_reserve = reserve;
            global += 1;
        }
    }
    for page in pages.iter() {
        page.exit(program_id)?;
    }
    Ok(())
}

/// A component's (units_per_index, accounted_reserve) after a rebalance, from its vault
/// balance. A retired component, or a removed one this rebalance sold out (`sold_out`), stays
/// at zero: what its public vault holds was sent there, and must not put the component back
/// into pricing (the oracle may no longer price it) or leave a dust sell leg no swap can fill.
fn rewritten_accounting(
    component: &LargeBasketComponent,
    amount: u64,
    sold_out: bool,
    base_units: u64,
    supply: u64,
    require_nonzero_units: bool,
) -> Result<(u64, u64)> {
    if sold_out || is_retired(component) {
        return Ok((0, 0));
    }
    // A removed component not yet sold (an unwind before its sell ran) keeps what the basket
    // owned; tokens sent to its vault never raise that.
    let amount = if sold_out_at_finalize(component) {
        amount.min(component.accounted_reserve)
    } else {
        amount
    };
    let units = if require_nonzero_units {
        let units = units_per_index_for_amount(amount, base_units, supply)?;
        require!(
            component.target_weight_bps == 0 || units > 0,
            BasketError::ZeroComponentUnits
        );
        units
    } else {
        units_per_index_for_amount_saturating(amount, base_units, supply)
    };
    Ok((units, amount))
}

#[allow(clippy::too_many_arguments)]
fn rebalance_candidates<'info>(
    shared_head: &[AccountInfo<'info>; 8],
    page: &AccountInfo<'info>,
    mint: &AccountInfo<'info>,
    vault: &AccountInfo<'info>,
    token_program: &AccountInfo<'info>,
    associated_token_program: &AccountInfo<'info>,
    quote_token_program: &AccountInfo<'info>,
    system_program: &AccountInfo<'info>,
    route: &[AccountInfo<'info>],
) -> Vec<AccountInfo<'info>> {
    let mut candidates = Vec::with_capacity(8 + 4 + 3 + route.len());
    candidates.extend_from_slice(shared_head);
    candidates.push(page.clone());
    candidates.push(mint.clone());
    candidates.push(vault.clone());
    candidates.push(token_program.clone());
    candidates.push(associated_token_program.clone());
    candidates.push(quote_token_program.clone());
    candidates.push(system_program.clone());
    candidates.extend_from_slice(route);
    candidates
}

#[allow(clippy::too_many_arguments)]
fn execute_rebalance_swap<'info>(
    jupiter_program: &AccountInfo<'info>,
    vault_authority: &AccountInfo<'info>,
    index_key: &Pubkey,
    vault_authority_bump: u8,
    source_vault: &AccountInfo<'info>,
    dest_vault: &AccountInfo<'info>,
    candidates: &[AccountInfo<'info>],
    swap: &LargeBasketSwapPlan,
) -> Result<(u64, u64)> {
    let metas = unpack_account_metas(&swap.accounts);
    let protected = [source_vault.key(), dest_vault.key()];
    // The effective containment here is the vault-authority scope check (no token
    // account owned by the signing vault authority may appear in the route except the
    // declared source/dest) plus the before/after balance deltas below; the route-scope
    // call documents the declared endpoints, mirroring the large-basket execute path.
    validate_jupiter_route_account_scope(candidates, &metas, &protected, &protected)?;
    validate_vault_authority_token_account_scope(
        candidates,
        &metas,
        *vault_authority.key,
        &protected,
    )?;

    let source_before = load_interface_token_account(source_vault)?.amount;
    let dest_before = load_interface_token_account(dest_vault)?.amount;
    let bump = [vault_authority_bump];
    let signer_seeds: &[&[u8]] = &[VAULT_AUTHORITY_SEED, index_key.as_ref(), &bump];
    invoke_jupiter_swap(
        jupiter_program.clone(),
        candidates,
        &metas,
        &swap.instruction_data,
        Some(*vault_authority.key),
        &[signer_seeds],
    )?;
    let source_after = load_interface_token_account(source_vault)?.amount;
    let dest_after = load_interface_token_account(dest_vault)?.amount;
    let spent = source_before
        .checked_sub(source_after)
        .ok_or_else(|| error!(BasketError::InvalidJupiterRoute))?;
    let received = dest_after
        .checked_sub(dest_before)
        .ok_or_else(|| error!(BasketError::InvalidJupiterRoute))?;
    require!(spent > 0 && received > 0, BasketError::InvalidJupiterRoute);
    Ok((spent, received))
}

fn create_vault_quote_ata<'info>(
    associated_token_program: &UncheckedAccount<'info>,
    payer: &Signer<'info>,
    vault_quote_token_account: &UncheckedAccount<'info>,
    vault_authority: &UncheckedAccount<'info>,
    quote_mint: &UncheckedAccount<'info>,
    system_program: &Program<'info, System>,
    quote_token_program: &AccountInfo<'info>,
) -> Result<()> {
    let expected = associated_token_address_with_token_program(
        &vault_authority.key(),
        &quote_mint.key(),
        quote_token_program.key,
    );
    require_keys_eq!(
        vault_quote_token_account.key(),
        expected,
        BasketError::InvalidVaultAccount
    );
    create_associated_token_account_idempotent_for_token_program(
        associated_token_program.to_account_info(),
        payer.to_account_info(),
        vault_quote_token_account.to_account_info(),
        vault_authority.to_account_info(),
        quote_mint.to_account_info(),
        system_program.to_account_info(),
        quote_token_program.clone(),
    )?;
    let account =
        load_interface_token_account(&vault_quote_token_account.to_account_info())?;
    require_keys_eq!(account.owner, vault_authority.key(), BasketError::InvalidVaultAccount);
    require_keys_eq!(account.mint, quote_mint.key(), BasketError::InvalidVaultAccount);
    Ok(())
}

fn validate_vault_quote_account<'info>(
    vault_quote_token_account: &UncheckedAccount<'info>,
    vault_authority: &UncheckedAccount<'info>,
    quote_mint: &UncheckedAccount<'info>,
    quote_token_program: &AccountInfo<'info>,
) -> Result<Pubkey> {
    let expected = associated_token_address_with_token_program(
        &vault_authority.key(),
        &quote_mint.key(),
        quote_token_program.key,
    );
    require_keys_eq!(
        vault_quote_token_account.key(),
        expected,
        BasketError::InvalidVaultAccount
    );
    let account =
        load_interface_token_account(&vault_quote_token_account.to_account_info())?;
    require_keys_eq!(account.owner, vault_authority.key(), BasketError::InvalidVaultAccount);
    require_keys_eq!(account.mint, quote_mint.key(), BasketError::InvalidVaultAccount);
    Ok(expected)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snap(value_units: u128, target_weight_bps: u16) -> ComponentSnapshot {
        ComponentSnapshot {
            decimals: 6,
            target_weight_bps,
            oracle_price: PRICE_SCALE as i128,
            current_amount: 0,
            accounted_reserve: 0,
            is_quote: false,
            retired: false,
            value: value_units * PRICE_SCALE,
        }
    }

    // $1 tokens with 6 decimals: 1_000_000 atoms = $1.
    fn holding(current: u64, accounted: u64, target_weight_bps: u16) -> ComponentSnapshot {
        ComponentSnapshot {
            decimals: 6,
            target_weight_bps,
            oracle_price: PRICE_SCALE as i128,
            current_amount: current,
            accounted_reserve: accounted,
            is_quote: false,
            retired: false,
            value: u128::from(current) * PRICE_SCALE / 1_000_000,
        }
    }

    #[test]
    fn removed_components_sell_only_what_the_basket_owns() {
        let total = 100 * PRICE_SCALE;
        // Owned $5, plus $1 sent to the vault: sells the $5.
        assert_eq!(plan_leg(&holding(6_000_000, 5_000_000, 0), total).unwrap(), LegPlan::Sell(5_000_000));
        // Owned less than a cent: written off, even with a larger donation on top.
        assert_eq!(plan_leg(&holding(5_000_000, 9_999, 0), total).unwrap(), LegPlan::Done);
        assert_eq!(plan_leg(&holding(9_999, 9_999, 0), total).unwrap(), LegPlan::Done);
        // Vault below the books (never expected): sells what is there.
        assert_eq!(plan_leg(&holding(2_000_000, 5_000_000, 0), total).unwrap(), LegPlan::Sell(2_000_000));
        // Retired and quote components never trade.
        let mut retired = holding(3_000_000, 0, 0);
        retired.retired = true;
        assert_eq!(plan_leg(&retired, total).unwrap(), LegPlan::Done);
        let mut quote = holding(3_000_000, 3_000_000, 0);
        quote.is_quote = true;
        assert_eq!(plan_leg(&quote, total).unwrap(), LegPlan::Done);
    }

    #[test]
    fn weighted_components_trade_toward_their_target() {
        let total = 100 * PRICE_SCALE;
        // 50% of $100 is $50.
        assert_eq!(plan_leg(&holding(60_000_000, 60_000_000, 5_000), total).unwrap(), LegPlan::Sell(10_000_000));
        assert_eq!(plan_leg(&holding(40_000_000, 40_000_000, 5_000), total).unwrap(), LegPlan::Buy(10_000_000));
        assert_eq!(plan_leg(&holding(50_000_000, 50_000_000, 5_000), total).unwrap(), LegPlan::Done);
        // A newly added component holds nothing yet.
        assert_eq!(plan_leg(&holding(0, 0, 1_000), total).unwrap(), LegPlan::Buy(10_000_000));
    }

    #[test]
    fn sold_out_masks() {
        let removed = component(Pubkey::new_unique(), 0);
        assert!(sold_out_at_finalize(&removed));
        assert!(!sold_out_at_finalize(&component(Pubkey::new_unique(), 100)));
        assert!(!sold_out_at_finalize(&component(USDC_MINT, 0)));
        assert!(sold_out_at_unwind(&removed, true, true));
        assert!(!sold_out_at_unwind(&removed, true, false), "sell leg not executed");
        assert!(!sold_out_at_unwind(&removed, false, true), "no sell leg (dust or retired)");
    }

    fn component(mint: Pubkey, target_weight_bps: u16) -> LargeBasketComponent {
        LargeBasketComponent {
            mint,
            units_per_index: 0,
            target_weight_bps,
            oracle_pair: Pubkey::new_unique(),
            token_program: anchor_spl::token::ID,
            vault: Pubkey::new_unique(),
            accounted_reserve: 0,
            decimals: 6,
        }
    }

    #[test]
    fn removed_components_stay_unaccounted_once_sold() {
        let base = 1_000_000;
        let supply = 2_000_000;
        // Sold out this rebalance: a donation that arrived since open is not accounted.
        let removed = LargeBasketComponent { accounted_reserve: 7_000, ..component(Pubkey::new_unique(), 0) };
        assert_eq!(rewritten_accounting(&removed, 1, true, base, supply, true).unwrap(), (0, 0));
        // Unwound before its sell executed: holders still own it, and a donation on top is
        // not added to the books.
        assert_eq!(rewritten_accounting(&removed, 7_000, false, base, supply, false).unwrap(), (3_500, 7_000));
        assert_eq!(rewritten_accounting(&removed, 9_000, false, base, supply, false).unwrap(), (3_500, 7_000));
        // Retired: donations stay unaccounted at finalize and unwind.
        let retired = component(Pubkey::new_unique(), 0);
        assert_eq!(rewritten_accounting(&retired, 9, false, base, supply, false).unwrap(), (0, 0));
        // Weighted components take their live balance.
        let held = component(Pubkey::new_unique(), 5_000);
        assert_eq!(rewritten_accounting(&held, 4_000_000, false, base, supply, true).unwrap(), (2_000_000, 4_000_000));
        assert!(rewritten_accounting(&held, 1, false, base, supply, true).is_err(), "weighted component flooring to zero units");
    }

    #[test]
    fn only_removed_and_sold_components_skip_pricing() {
        let mut removed = component(Pubkey::new_unique(), 0);
        assert!(is_retired(&removed));
        // Still holding accounted tokens: it must be priced and sold.
        removed.accounted_reserve = 5;
        assert!(!is_retired(&removed));
        // Weighted components and the USDC cash slot are always priced.
        assert!(!is_retired(&component(Pubkey::new_unique(), 1_000)));
        assert!(!is_retired(&component(USDC_MINT, 0)));
    }

    #[test]
    fn value_and_target_round_trip() {
        let price = 2_i128 * PRICE_SCALE as i128; // $2, 1e18-scaled
        // 1.5 tokens (6 decimals) at $2 = $3 worth.
        let value = component_value_scaled(1_500_000, 6, price).unwrap();
        assert_eq!(value, 3 * PRICE_SCALE);
        let amount = target_amount_for_value_scaled(value, 6, price).unwrap();
        assert_eq!(amount, 1_500_000);
    }

    #[test]
    fn weight_drift_is_absolute_bps_gap() {
        let total = 100 * PRICE_SCALE;
        let value = 40 * PRICE_SCALE; // 4000 bps of NAV
        assert_eq!(weight_drift_bps(value, total, 3000).unwrap(), 1000);
        assert_eq!(weight_drift_bps(value, total, 4000).unwrap(), 0);
    }

    #[test]
    fn drift_status_triggers_at_threshold_and_zero_disables() {
        let total = 100 * PRICE_SCALE;
        // actual weights 6000 / 4000 vs targets 5000 / 5000 -> max drift 1000 bps.
        let snaps = vec![snap(60, 5000), snap(40, 5000)];
        let (max_drift, triggered) = drift_status(&snaps, total, 1000).unwrap();
        assert_eq!(max_drift, 1000);
        assert!(triggered, "drift == threshold should trigger");
        assert!(!drift_status(&snaps, total, 1001).unwrap().1, "below threshold");
        assert!(!drift_status(&snaps, total, 0).unwrap().1, "0 threshold disables drift");
    }

    #[test]
    fn leg_dust_threshold_is_under_one_bps() {
        assert!(leg_is_dust(1_000_000, 1_000_000)); // exactly on target
        assert!(leg_is_dust(1_000_000, 1_000_050)); // ~0.5 bps off
        assert!(!leg_is_dust(1_000_000, 1_000_200)); // 2 bps off
        assert!(!leg_is_dust(0, 10_000)); // 100% deficit is never dust
    }

    #[test]
    fn buy_cost_allowance_is_bounded_and_rounds_conservatively() {
        assert_eq!(minimum_rebalance_buy_amount(400_000_000, 100_000_000, 50).unwrap(), 97_500_000);
        assert_eq!(minimum_rebalance_buy_amount(400, 100, 0).unwrap(), 100);
        assert_eq!(minimum_rebalance_buy_amount(1_000_000, 1, 50).unwrap(), 1);
        assert_eq!(minimum_rebalance_buy_amount(0, 101, 50).unwrap(), 101);
        assert!(minimum_rebalance_buy_amount(u64::MAX, 1, 50).is_err());
        assert!(minimum_rebalance_buy_amount(400, 100, MAX_KEEPER_NAV_TOLERANCE_BPS + 1).is_err());
        // $100 sold with 0.3% cost funds a smaller buy at 0.3% cost,
        // while retaining more than the minimum permitted backing.
        let fill = 99_700_000_u64 * 1000 / 1003;
        assert!(fill >= minimum_rebalance_buy_amount(400_000_000, 100_000_000, 50).unwrap());
    }

    #[test]
    fn buy_spend_cap_rejects_cash_sweep_even_at_fair_execution_price() {
        let price = PRICE_SCALE as i128;
        let cap = maximum_rebalance_buy_quote(10_000_000, 6, price, 500).unwrap();
        assert_eq!(cap, 10_500_000);
        // Fair price alone used to accept spending the entire $910 cash vault
        // on the intended $10 buy, bypassing allocation checks via unwind.
        assert!(validate_atomic_rebalance_fill(false, 910_000_000, 910_000_000, 6, price, 500).is_ok());
        assert!(910_000_000 > cap);
        assert!(10_100_000 <= cap);
        assert_eq!(maximum_rebalance_buy_quote(1, 9, price, 0).unwrap(), 1);
        assert_eq!(maximum_rebalance_buy_quote(100_000_000, 8, price * 2, 100).unwrap(), 2_020_000);
        assert!(maximum_rebalance_buy_quote(1, 6, price, 501).is_err());
    }
}

// This check runs after CPI and before commit. Returning an error rolls back the
// entire batch, including token transfers, so unwind cannot bypass price bounds.
fn validate_atomic_rebalance_fill(is_sell: bool, quote_atoms: u64, component_atoms: u64,
    component_decimals: u8, oracle_price: i128, max_slippage_bps: u16) -> Result<()> {
    require!(max_slippage_bps <= MAX_FIXED_WEIGHT_EXECUTION_SLIPPAGE_BPS,
        BasketError::InvalidOraclePriceTolerance);
    if is_sell {
        validate_sell_execution_price(quote_atoms, component_atoms, USDC_DECIMALS,
            component_decimals, oracle_price, max_slippage_bps)
    } else {
        validate_buy_execution_price(quote_atoms, component_atoms, USDC_DECIMALS,
            component_decimals, oracle_price, max_slippage_bps)
    }
}

#[cfg(test)]
mod atomic_execution_tests {
    use super::*;
    #[test]
    fn rejects_keeper_selling_backing_for_dust_or_buying_at_excessive_cost() {
        let price = PRICE_SCALE as i128;
        assert!(validate_atomic_rebalance_fill(true, 1, 1_000_000, 6, price, 100).is_err());
        assert!(validate_atomic_rebalance_fill(false, 2_000_000, 1_000_000, 6, price, 100).is_err());
        assert!(validate_atomic_rebalance_fill(true, 990_000, 1_000_000, 6, price, 100).is_ok());
        assert!(validate_atomic_rebalance_fill(false, 1_010_000, 1_000_000, 6, price, 100).is_ok());
    }
    #[test]
    fn keeper_cannot_disable_atomic_price_bound() {
        assert!(validate_atomic_rebalance_fill(true, 1, 1_000_000, 6,
            PRICE_SCALE as i128, MAX_FIXED_WEIGHT_EXECUTION_SLIPPAGE_BPS + 1).is_err());
    }
}
