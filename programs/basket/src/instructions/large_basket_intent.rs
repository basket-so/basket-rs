use anchor_lang::prelude::*;
use anchor_spl::{
    token::{self, Burn, MintTo, Token, TokenAccount, Transfer},
    token_interface::{
        self, TokenAccount as InterfaceTokenAccount, TokenInterface, TransferChecked,
    },
};

use crate::{
    constants::{
        LARGE_BASKET_COMPONENT_BITMAP_BYTES, LARGE_BASKET_COMPONENT_PAGE_SEED,
        LARGE_BASKET_INTENT_LOCK_SEED, LARGE_BASKET_INTENT_SEED, REFUND_ESCROW_SEED,
        MAX_FIXED_WEIGHT_EXECUTION_SLIPPAGE_BPS, MAX_LARGE_BASKET_COMPONENTS,
        MAX_LARGE_BASKET_COMPONENTS_PER_PAGE, MAX_LARGE_BASKET_INTENT_TTL_SECONDS,
        STAKING_AUTHORITY_SEED, STAKING_POOL_SEED, USDC_MINT, VAULT_AUTHORITY_SEED,
    },
    errors::BasketError,
    events::{LargeBasketComponentFilled, LargeBasketIntentFinalized, LargeBasketIntentOpened},
    state::{
        IndexState, LargeBasketComponent, LargeBasketComponentPage,
        LargeBasketIntent, LargeBasketIntentKind, LargeBasketIntentLock, LargeBasketIntentStatus,
        StakingPool,
    },
    utils::{
        accrue_staking_rewards, associated_token_address,
        associated_token_address_with_token_program,
        create_associated_token_account_idempotent,
        create_associated_token_account_idempotent_for_token_program,
        invoke_jupiter_swap_with_scratch, large_basket_fee_split, load_interface_token_account,
        load_mint, load_user_token_account, pro_rata_mint_amount, pro_rata_redeem_amount,
        quote_component_amount, route_creator_fee,
        switchboard_feed_price, validate_buy_execution_price, validate_jupiter_route_account_scope,
        validate_sell_execution_price,
        validate_staking_vault, validate_total_index_fee_bps, validate_user_token_account,
        validate_vault_authority_token_account_scope, verified_switchboard_prices,
        unpack_account_metas, bitmap_get, bitmap_set_once, JupiterInvokeScratch,
        LargeBasketSwapPlan, ASSOCIATED_TOKEN_ID,
    },
};

#[derive(AnchorSerialize, AnchorDeserialize, Clone, Debug)]
pub struct OpenLargeBasketMintIntentArgs {
    pub index_amount_out: u64,
    pub max_quote_in: u64,
    pub nonce: u64,
    pub expires_at: i64,
    /// Deposit the exact component tokens in-kind instead of swapping USDC -> components.
    pub in_kind: bool,
}

#[derive(AnchorSerialize, AnchorDeserialize, Clone, Debug)]
pub struct OpenLargeBasketRedeemIntentArgs {
    pub index_amount_in: u64,
    pub min_quote_out: u64,
    pub nonce: u64,
    pub expires_at: i64,
    /// Withdraw the exact component tokens in-kind instead of swapping components -> USDC.
    pub in_kind: bool,
}

#[derive(AnchorSerialize, AnchorDeserialize, Clone, Debug)]
pub struct ExecuteLargeBasketMintComponentArgs {
    pub component_index: u16,
    pub max_quote_in: u64,
    pub max_oracle_slippage_bps: u16,
    pub swap: Option<LargeBasketSwapPlan>,
    pub switchboard_max_age_slots: u64,
}

#[derive(AnchorSerialize, AnchorDeserialize, Clone, Debug)]
pub struct ExecuteLargeBasketRedeemComponentArgs {
    pub component_index: u16,
    pub min_quote_out: u64,
    pub max_oracle_slippage_bps: u16,
    pub swap: Option<LargeBasketSwapPlan>,
    pub switchboard_max_age_slots: u64,
}

// Batched execute: process several components in one transaction. Each entry's
// component accounts + route accounts live in remaining_accounts as a contiguous
// group [component_page, component_mint, component_vault, component_token_program,
// ...route_accounts] (route_account_count long). The swap.accounts indices are
// relative to THIS entry's candidate ordering (see execute_*_batch handler).
#[derive(AnchorSerialize, AnchorDeserialize, Clone, Debug)]
pub struct LargeBasketMintBatchEntry {
    pub component_index: u16,
    pub max_quote_in: u64,
    pub route_account_count: u8,
    pub swap: Option<LargeBasketSwapPlan>,
}

#[derive(AnchorSerialize, AnchorDeserialize, Clone, Debug)]
pub struct ExecuteLargeBasketMintBatchArgs {
    pub entries: Vec<LargeBasketMintBatchEntry>,
}

#[derive(AnchorSerialize, AnchorDeserialize, Clone, Debug)]
pub struct LargeBasketRedeemBatchEntry {
    pub component_index: u16,
    pub min_quote_out: u64,
    pub route_account_count: u8,
    pub swap: Option<LargeBasketSwapPlan>,
}

#[derive(AnchorSerialize, AnchorDeserialize, Clone, Debug)]
pub struct ExecuteLargeBasketRedeemBatchArgs {
    pub entries: Vec<LargeBasketRedeemBatchEntry>,
}

#[derive(Accounts)]
#[instruction(args: OpenLargeBasketMintIntentArgs)]
pub struct OpenLargeBasketMintIntent<'info> {
    #[account(mut)]
    pub owner: Signer<'info>,
    #[account(mut, has_one = index_mint @ BasketError::IndexMintMismatch)]
    pub index: Account<'info, IndexState>,
    /// CHECK: Validated as the configured classic SPL index mint.
    pub index_mint: UncheckedAccount<'info>,
    /// CHECK: Validated as native Solana USDC.
    #[account(address = USDC_MINT @ BasketError::InvalidQuoteMint)]
    pub quote_mint: UncheckedAccount<'info>,
    #[account(seeds = [STAKING_POOL_SEED], bump = staking_pool.bump)]
    pub staking_pool: Account<'info, StakingPool>,
    #[account(
        init,
        payer = owner,
        seeds = [
            LARGE_BASKET_INTENT_SEED,
            index.key().as_ref(),
            owner.key().as_ref(),
            &args.nonce.to_le_bytes(),
        ],
        bump,
        space = 8 + LargeBasketIntent::SPACE
    )]
    pub intent: Account<'info, LargeBasketIntent>,
    #[account(
        init_if_needed,
        payer = owner,
        seeds = [
            LARGE_BASKET_INTENT_LOCK_SEED,
            index.key().as_ref(),
            owner.key().as_ref(),
        ],
        bump,
        space = 8 + LargeBasketIntentLock::SPACE
    )]
    pub intent_lock: Account<'info, LargeBasketIntentLock>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
#[instruction(args: OpenLargeBasketRedeemIntentArgs)]
pub struct OpenLargeBasketRedeemIntent<'info> {
    #[account(mut)]
    pub owner: Signer<'info>,
    #[account(mut, has_one = index_mint @ BasketError::IndexMintMismatch)]
    pub index: Account<'info, IndexState>,
    /// CHECK: Validated as the configured classic SPL index mint. Mutable: the open
    /// burns index tokens, which decrements the mint supply.
    #[account(mut)]
    pub index_mint: UncheckedAccount<'info>,
    /// CHECK: Validated as the owner's index token account before opening the intent.
    /// Mutable: the open burns the redeemed index tokens from it.
    #[account(mut)]
    pub owner_index_token_account: UncheckedAccount<'info>,
    /// CHECK: Validated as native Solana USDC.
    #[account(address = USDC_MINT @ BasketError::InvalidQuoteMint)]
    pub quote_mint: UncheckedAccount<'info>,
    #[account(seeds = [STAKING_POOL_SEED], bump = staking_pool.bump)]
    pub staking_pool: Account<'info, StakingPool>,
    #[account(
        init,
        payer = owner,
        seeds = [
            LARGE_BASKET_INTENT_SEED,
            index.key().as_ref(),
            owner.key().as_ref(),
            &args.nonce.to_le_bytes(),
        ],
        bump,
        space = 8 + LargeBasketIntent::SPACE
    )]
    pub intent: Account<'info, LargeBasketIntent>,
    #[account(
        init_if_needed,
        payer = owner,
        seeds = [
            LARGE_BASKET_INTENT_LOCK_SEED,
            index.key().as_ref(),
            owner.key().as_ref(),
        ],
        bump,
        space = 8 + LargeBasketIntentLock::SPACE
    )]
    pub intent_lock: Account<'info, LargeBasketIntentLock>,
    pub token_program: Program<'info, Token>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct CollectLargeBasketIntentFees<'info> {
    #[account(mut)]
    pub owner: Signer<'info>,
    #[account(mut, has_one = owner @ BasketError::InvalidLargeBasketIntent)]
    pub intent: Account<'info, LargeBasketIntent>,
    /// CHECK: Validated as native Solana USDC.
    #[account(address = USDC_MINT @ BasketError::InvalidQuoteMint)]
    pub quote_mint: UncheckedAccount<'info>,
    /// CHECK: Validated as the owner's USDC token account.
    #[account(mut)]
    pub owner_quote_token_account: UncheckedAccount<'info>,
    #[account(mut)]
    pub fee_recipient_quote_token_account: Account<'info, TokenAccount>,
    #[account(mut)]
    pub creator_fee_recipient_quote_token_account: Account<'info, TokenAccount>,
    #[account(mut, seeds = [STAKING_POOL_SEED], bump = staking_pool.bump)]
    pub staking_pool: Account<'info, StakingPool>,
    /// CHECK: PDA authority over staking vaults.
    #[account(seeds = [STAKING_AUTHORITY_SEED], bump = staking_pool.staking_authority_bump)]
    pub staking_authority: UncheckedAccount<'info>,
    #[account(mut)]
    pub staking_reward_vault: Account<'info, TokenAccount>,
    pub token_program: Program<'info, Token>,
}

#[derive(Accounts)]
pub struct ExecuteLargeBasketMintComponent<'info> {
    #[account(mut)]
    pub owner: Signer<'info>,
    #[account(has_one = index_mint @ BasketError::IndexMintMismatch)]
    pub index: Account<'info, IndexState>,
    /// CHECK: Validated as the configured classic SPL index mint.
    pub index_mint: UncheckedAccount<'info>,
    #[account(mut, has_one = owner @ BasketError::InvalidLargeBasketIntent, has_one = index @ BasketError::InvalidLargeBasketIntent)]
    pub intent: Account<'info, LargeBasketIntent>,
    /// CHECK: PDA authority over component vaults.
    #[account(
        seeds = [VAULT_AUTHORITY_SEED, index.key().as_ref()],
        bump = index.vault_authority_bump
    )]
    pub vault_authority: UncheckedAccount<'info>,
    /// CHECK: Validated as native Solana USDC.
    #[account(address = USDC_MINT @ BasketError::InvalidQuoteMint)]
    pub quote_mint: UncheckedAccount<'info>,
    #[account(mut)]
    pub owner_quote_token_account: InterfaceAccount<'info, InterfaceTokenAccount>,
    /// CHECK: Validated against known Jupiter program ids when a swap is supplied.
    pub jupiter_program: UncheckedAccount<'info>,
    #[account(mut)]
    pub component_page: Account<'info, LargeBasketComponentPage>,
    /// CHECK: Validated against the component page.
    pub component_mint: UncheckedAccount<'info>,
    /// CHECK: Validated against the component page.
    #[account(mut)]
    pub component_vault: UncheckedAccount<'info>,
    /// CHECK: Validated against the component page.
    pub component_token_program: UncheckedAccount<'info>,
    /// CHECK: Validated as the Associated Token Program.
    #[account(address = ASSOCIATED_TOKEN_ID @ BasketError::InvalidAssociatedTokenProgram)]
    pub associated_token_program: UncheckedAccount<'info>,
    pub quote_token_program: Interface<'info, TokenInterface>,
    pub system_program: Program<'info, System>,
}

// Batched mint/redeem execute: only the fixed/shared accounts are named. Each
// entry's component accounts (page, mint, vault, token_program) + Jupiter route
// accounts arrive in remaining_accounts as contiguous per-entry groups.
#[derive(Accounts)]
pub struct ExecuteLargeBasketComponentBatch<'info> {
    #[account(mut)]
    pub owner: Signer<'info>,
    #[account(has_one = index_mint @ BasketError::IndexMintMismatch)]
    pub index: Account<'info, IndexState>,
    /// CHECK: Validated as the configured classic SPL index mint.
    pub index_mint: UncheckedAccount<'info>,
    #[account(mut, has_one = owner @ BasketError::InvalidLargeBasketIntent, has_one = index @ BasketError::InvalidLargeBasketIntent)]
    pub intent: Account<'info, LargeBasketIntent>,
    /// CHECK: PDA authority over component vaults.
    #[account(
        seeds = [VAULT_AUTHORITY_SEED, index.key().as_ref()],
        bump = index.vault_authority_bump
    )]
    pub vault_authority: UncheckedAccount<'info>,
    /// CHECK: Validated as native Solana USDC.
    #[account(address = USDC_MINT @ BasketError::InvalidQuoteMint)]
    pub quote_mint: UncheckedAccount<'info>,
    #[account(mut)]
    pub owner_quote_token_account: InterfaceAccount<'info, InterfaceTokenAccount>,
    /// CHECK: Validated against known Jupiter program ids when a swap is supplied.
    pub jupiter_program: UncheckedAccount<'info>,
    /// CHECK: Validated as the Associated Token Program.
    #[account(address = ASSOCIATED_TOKEN_ID @ BasketError::InvalidAssociatedTokenProgram)]
    pub associated_token_program: UncheckedAccount<'info>,
    pub quote_token_program: Interface<'info, TokenInterface>,
    pub system_program: Program<'info, System>,
}

// Batched swap-path redeem. Each leg's USDC is paid to the owner and that leg's share of
// the fee is charged in the same instruction, so there is no separate fee step to skip.
// The leg that completes the intent also finalizes it and releases the basket lock.
// Known gap: the fee basis is the USDC measured in owner_quote_token_account, and the
// redeemer chooses the Jupiter route, so a custom route can shrink that basis. Closing it
// needs a route-independent basis (in-kind skim before the swap, or an oracle floor).
#[derive(Accounts)]
// Large accounts are boxed: on the stack they push the handler past the SBF 4 KB frame.
pub struct ExecuteLargeBasketRedeemBatch<'info> {
    #[account(mut)]
    pub owner: Signer<'info>,
    #[account(mut, has_one = index_mint @ BasketError::IndexMintMismatch)]
    pub index: Box<Account<'info, IndexState>>,
    /// CHECK: Validated as the configured classic SPL index mint.
    pub index_mint: UncheckedAccount<'info>,
    #[account(mut, has_one = owner @ BasketError::InvalidLargeBasketIntent, has_one = index @ BasketError::InvalidLargeBasketIntent)]
    pub intent: Box<Account<'info, LargeBasketIntent>>,
    /// CHECK: PDA authority over component vaults.
    #[account(
        seeds = [VAULT_AUTHORITY_SEED, index.key().as_ref()],
        bump = index.vault_authority_bump
    )]
    pub vault_authority: UncheckedAccount<'info>,
    /// CHECK: Validated as native Solana USDC.
    #[account(address = USDC_MINT @ BasketError::InvalidQuoteMint)]
    pub quote_mint: UncheckedAccount<'info>,
    #[account(mut)]
    pub owner_quote_token_account: Box<InterfaceAccount<'info, InterfaceTokenAccount>>,
    /// CHECK: Validated against known Jupiter program ids when a swap is supplied.
    pub jupiter_program: UncheckedAccount<'info>,
    /// CHECK: Validated as the Associated Token Program.
    #[account(address = ASSOCIATED_TOKEN_ID @ BasketError::InvalidAssociatedTokenProgram)]
    pub associated_token_program: UncheckedAccount<'info>,
    pub quote_token_program: Interface<'info, TokenInterface>,
    pub system_program: Program<'info, System>,
    #[account(mut)]
    pub fee_recipient_quote_token_account: Box<Account<'info, TokenAccount>>,
    #[account(mut)]
    pub creator_fee_recipient_quote_token_account: Box<Account<'info, TokenAccount>>,
    #[account(mut, seeds = [STAKING_POOL_SEED], bump = staking_pool.bump)]
    pub staking_pool: Box<Account<'info, StakingPool>>,
    /// CHECK: PDA authority over staking vaults.
    #[account(seeds = [STAKING_AUTHORITY_SEED], bump = staking_pool.staking_authority_bump)]
    pub staking_authority: UncheckedAccount<'info>,
    #[account(mut)]
    pub staking_reward_vault: Box<Account<'info, TokenAccount>>,
    #[account(
        mut,
        seeds = [
            LARGE_BASKET_INTENT_LOCK_SEED,
            index.key().as_ref(),
            owner.key().as_ref(),
        ],
        bump = intent_lock.bump
    )]
    pub intent_lock: Account<'info, LargeBasketIntentLock>,
}

#[derive(Accounts)]
pub struct ExecuteLargeBasketRedeemComponent<'info> {
    #[account(mut)]
    pub owner: Signer<'info>,
    #[account(has_one = index_mint @ BasketError::IndexMintMismatch)]
    pub index: Account<'info, IndexState>,
    /// CHECK: Validated as the configured classic SPL index mint.
    pub index_mint: UncheckedAccount<'info>,
    #[account(mut, has_one = owner @ BasketError::InvalidLargeBasketIntent, has_one = index @ BasketError::InvalidLargeBasketIntent)]
    pub intent: Account<'info, LargeBasketIntent>,
    /// CHECK: PDA authority over component vaults.
    #[account(
        seeds = [VAULT_AUTHORITY_SEED, index.key().as_ref()],
        bump = index.vault_authority_bump
    )]
    pub vault_authority: UncheckedAccount<'info>,
    /// CHECK: Validated as native Solana USDC.
    #[account(address = USDC_MINT @ BasketError::InvalidQuoteMint)]
    pub quote_mint: UncheckedAccount<'info>,
    #[account(mut)]
    pub owner_quote_token_account: InterfaceAccount<'info, InterfaceTokenAccount>,
    /// CHECK: Validated against known Jupiter program ids when a swap is supplied.
    pub jupiter_program: UncheckedAccount<'info>,
    #[account(mut)]
    pub component_page: Account<'info, LargeBasketComponentPage>,
    /// CHECK: Validated against the component page.
    pub component_mint: UncheckedAccount<'info>,
    /// CHECK: Validated against the component page.
    #[account(mut)]
    pub component_vault: UncheckedAccount<'info>,
    /// CHECK: Validated against the component page.
    pub component_token_program: UncheckedAccount<'info>,
    pub quote_token_program: Interface<'info, TokenInterface>,
}

#[derive(AnchorSerialize, AnchorDeserialize, Clone, Debug)]
pub struct VerifyLargeBasketComponentPriceArgs {
    pub component_index: u16,
    pub max_oracle_slippage_bps: u16,
    pub switchboard_max_age_slots: u64,
}

// Deferred oracle price-bound check for a single component. Runs AFTER the
// component's swap (in its own swap-free tx) so the swap tx never has to carry the
// oracle quote. Validates the stored effective price against a fresh 1-feed quote.
#[derive(Accounts)]
pub struct VerifyLargeBasketComponentPrice<'info> {
    pub owner: Signer<'info>,
    #[account(has_one = index_mint @ BasketError::IndexMintMismatch)]
    pub index: Account<'info, IndexState>,
    /// CHECK: Validated as the configured classic SPL index mint.
    pub index_mint: UncheckedAccount<'info>,
    #[account(
        mut,
        has_one = owner @ BasketError::InvalidLargeBasketIntent,
        has_one = index @ BasketError::InvalidLargeBasketIntent
    )]
    pub intent: Account<'info, LargeBasketIntent>,
    pub component_page: Account<'info, LargeBasketComponentPage>,
    /// CHECK: Verified by Switchboard's quote verifier.
    pub switchboard_queue: UncheckedAccount<'info>,
    /// CHECK: Verified by Switchboard's quote verifier and canonical key check.
    pub switchboard_quote: UncheckedAccount<'info>,
    /// CHECK: Switchboard verifier validates this sysvar id.
    pub slothashes: UncheckedAccount<'info>,
    /// CHECK: Switchboard verifier validates this sysvar id.
    pub instructions_sysvar: UncheckedAccount<'info>,
}

#[derive(Accounts)]
pub struct FinalizeLargeBasketMintIntent<'info> {
    #[account(mut)]
    pub owner: Signer<'info>,
    #[account(mut, has_one = index_mint @ BasketError::IndexMintMismatch)]
    pub index: Account<'info, IndexState>,
    #[account(mut)]
    /// CHECK: Validated as the configured classic SPL Token mint by CPI calls.
    pub index_mint: UncheckedAccount<'info>,
    /// CHECK: PDA authority over the index mint and component vaults.
    #[account(
        seeds = [VAULT_AUTHORITY_SEED, index.key().as_ref()],
        bump = index.vault_authority_bump
    )]
    pub vault_authority: UncheckedAccount<'info>,
    #[account(mut, has_one = owner @ BasketError::InvalidLargeBasketIntent, has_one = index @ BasketError::InvalidLargeBasketIntent)]
    pub intent: Account<'info, LargeBasketIntent>,
    #[account(
        mut,
        seeds = [
            LARGE_BASKET_INTENT_LOCK_SEED,
            index.key().as_ref(),
            owner.key().as_ref(),
        ],
        bump = intent_lock.bump
    )]
    pub intent_lock: Account<'info, LargeBasketIntentLock>,
    /// CHECK: Created and validated as the owner's index ATA before minting.
    #[account(mut)]
    pub owner_index_token_account: UncheckedAccount<'info>,
    /// CHECK: Validated as the Associated Token Program.
    #[account(address = ASSOCIATED_TOKEN_ID @ BasketError::InvalidAssociatedTokenProgram)]
    pub associated_token_program: UncheckedAccount<'info>,
    pub token_program: Program<'info, Token>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct FinalizeLargeBasketRedeemIntent<'info> {
    #[account(mut)]
    pub index: Account<'info, IndexState>,
    #[account(mut, has_one = index @ BasketError::InvalidLargeBasketIntent)]
    pub intent: Account<'info, LargeBasketIntent>,
    #[account(
        mut,
        seeds = [
            LARGE_BASKET_INTENT_LOCK_SEED,
            index.key().as_ref(),
            intent.owner.as_ref(),
        ],
        bump = intent_lock.bump
    )]
    pub intent_lock: Account<'info, LargeBasketIntentLock>,
}

#[derive(Accounts)]
pub struct CancelUnfilledLargeBasketMintIntent<'info> {
    pub owner: Signer<'info>,
    #[account(mut)]
    pub index: Account<'info, IndexState>,
    #[account(mut, has_one = owner @ BasketError::InvalidLargeBasketIntent, has_one = index @ BasketError::InvalidLargeBasketIntent)]
    pub intent: Account<'info, LargeBasketIntent>,
    #[account(
        mut,
        seeds = [
            LARGE_BASKET_INTENT_LOCK_SEED,
            index.key().as_ref(),
            owner.key().as_ref(),
        ],
        bump = intent_lock.bump
    )]
    pub intent_lock: Account<'info, LargeBasketIntentLock>,
}

#[derive(Accounts)]
pub struct CancelUnfilledLargeBasketRedeemIntent<'info> {
    pub owner: Signer<'info>,
    #[account(mut, has_one = index_mint @ BasketError::IndexMintMismatch)]
    pub index: Account<'info, IndexState>,
    #[account(mut)]
    /// CHECK: Validated as the configured classic SPL index mint by the mint CPI.
    pub index_mint: UncheckedAccount<'info>,
    /// CHECK: PDA authority over the index mint and component vaults.
    #[account(
        seeds = [VAULT_AUTHORITY_SEED, index.key().as_ref()],
        bump = index.vault_authority_bump
    )]
    pub vault_authority: UncheckedAccount<'info>,
    #[account(mut, has_one = owner @ BasketError::InvalidLargeBasketIntent, has_one = index @ BasketError::InvalidLargeBasketIntent)]
    pub intent: Account<'info, LargeBasketIntent>,
    #[account(
        mut,
        seeds = [
            LARGE_BASKET_INTENT_LOCK_SEED,
            index.key().as_ref(),
            owner.key().as_ref(),
        ],
        bump = intent_lock.bump
    )]
    pub intent_lock: Account<'info, LargeBasketIntentLock>,
    /// CHECK: Validated as the owner's index token account before reminting.
    #[account(mut)]
    pub owner_index_token_account: UncheckedAccount<'info>,
    pub token_program: Program<'info, Token>,
}

#[derive(Accounts)]
pub struct CancelExpiredLargeBasketIntent<'info> {
    #[account(mut, has_one = index_mint @ BasketError::IndexMintMismatch)]
    pub index: Account<'info, IndexState>,
    #[account(mut)]
    /// CHECK: Validated as the configured classic SPL index mint when a redeem is rewound.
    pub index_mint: UncheckedAccount<'info>,
    /// CHECK: PDA authority over the index mint and component vaults.
    #[account(
        seeds = [VAULT_AUTHORITY_SEED, index.key().as_ref()],
        bump = index.vault_authority_bump
    )]
    pub vault_authority: UncheckedAccount<'info>,
    #[account(mut, has_one = index @ BasketError::InvalidLargeBasketIntent)]
    pub intent: Account<'info, LargeBasketIntent>,
    #[account(
        mut,
        seeds = [
            LARGE_BASKET_INTENT_LOCK_SEED,
            index.key().as_ref(),
            intent.owner.as_ref(),
        ],
        bump = intent_lock.bump
    )]
    pub intent_lock: Account<'info, LargeBasketIntentLock>,
    /// CHECK: Validated as the owner's index token account when a zero-fill redeem is rewound.
    #[account(mut)]
    pub owner_index_token_account: UncheckedAccount<'info>,
    pub token_program: Program<'info, Token>,
}

impl<'info> OpenLargeBasketMintIntent<'info> {
    pub fn handle(
        ctx: Context<'_, '_, 'info, 'info, Self>,
        args: OpenLargeBasketMintIntentArgs,
    ) -> Result<()> {
        require!(
            !ctx.accounts.index.minting_paused,
            BasketError::MintingPaused
        );
        require!(args.index_amount_out > 0, BasketError::InvalidIndexAmount);
        require!(
            ctx.accounts.index.large_basket_configured,
            BasketError::LargeBasketNotConfigured
        );
        // Intents from different owners run side by side; only a keeper rebalance pauses
        // new ones. The per-owner intent lock still allows one open intent per owner.
        require!(
            ctx.accounts.index.accepts_new_intents(Clock::get()?.unix_timestamp),
            BasketError::RebalancePending
        );
        initialize_or_validate_intent_lock(
            &mut ctx.accounts.intent_lock,
            ctx.accounts.index.key(),
            ctx.accounts.owner.key(),
            ctx.bumps.intent_lock,
        )?;

        let index_mint = load_mint(&ctx.accounts.index_mint.to_account_info())?;
        let current_supply = index_mint.supply;
        let post_supply = current_supply
            .checked_add(args.index_amount_out)
            .ok_or_else(|| error!(BasketError::ArithmeticOverflow))?;
        if ctx.accounts.index.max_supply > 0 {
            require!(
                post_supply <= ctx.accounts.index.max_supply,
                BasketError::SupplyCapExceeded
            );
        }
        let component_amounts = compute_intent_component_amounts(
            &ctx.accounts.index.key(),
            &ctx.accounts.index,
            ctx.remaining_accounts,
            args.index_amount_out,
            current_supply,
            LargeBasketIntentKind::Mint,
        )?;

        initialize_intent(
            &mut ctx.accounts.intent,
            IntentInit {
                index: ctx.accounts.index.key(),
                owner: ctx.accounts.owner.key(),
                nonce: args.nonce,
                kind: LargeBasketIntentKind::Mint,
                index_amount: args.index_amount_out,
                supply_snapshot: current_supply,
                post_supply,
                quote_mint: ctx.accounts.quote_mint.key(),
                component_amounts,
                max_quote_in: args.max_quote_in,
                min_quote_out: 0,
                protocol_fee_bps: ctx.accounts.index.mint_fee_bps,
                creator_fee_bps: ctx.accounts.index.creator_mint_fee_bps,
                staking_fee_bps: ctx.accounts.index.staking_mint_fee_bps,
                protocol_fee_recipient: ctx.accounts.index.fee_recipient,
                creator_fee_recipient: ctx.accounts.index.creator_fee_recipient,
                staking_pool: ctx.accounts.staking_pool.key(),
                component_count: large_basket_intent_component_count(&ctx.accounts.index),
                expires_at: args.expires_at,
                in_kind: args.in_kind,
                supply_era: ctx.accounts.index.supply_era,
                bump: ctx.bumps.intent,
            },
        )?;
        ctx.accounts.index.track_opened_intent()?;
        activate_intent_lock(&mut ctx.accounts.intent_lock, ctx.accounts.intent.key())?;
        Ok(())
    }
}

impl<'info> OpenLargeBasketRedeemIntent<'info> {
    pub fn handle(
        ctx: Context<'_, '_, 'info, 'info, Self>,
        args: OpenLargeBasketRedeemIntentArgs,
    ) -> Result<()> {
        require!(
            !ctx.accounts.index.redeeming_paused,
            BasketError::RedeemingPaused
        );
        require!(args.index_amount_in > 0, BasketError::InvalidIndexAmount);
        require!(
            ctx.accounts.index.large_basket_configured,
            BasketError::LargeBasketNotConfigured
        );
        require!(
            ctx.accounts.index.accepts_new_intents(Clock::get()?.unix_timestamp),
            BasketError::RebalancePending
        );
        initialize_or_validate_intent_lock(
            &mut ctx.accounts.intent_lock,
            ctx.accounts.index.key(),
            ctx.accounts.owner.key(),
            ctx.bumps.intent_lock,
        )?;

        let index_mint = load_mint(&ctx.accounts.index_mint.to_account_info())?;
        let current_supply = index_mint.supply;
        let post_supply = current_supply
            .checked_sub(args.index_amount_in)
            .ok_or_else(|| error!(BasketError::InvalidIndexAmount))?;

        let owner_index_account =
            load_user_token_account(&ctx.accounts.owner_index_token_account.to_account_info())?;
        validate_user_token_account(
            &owner_index_account,
            &ctx.accounts.owner.key(),
            &ctx.accounts.index_mint.key(),
        )?;
        require!(
            owner_index_account.amount >= args.index_amount_in,
            BasketError::InvalidIndexAmount
        );
        let component_amounts = compute_intent_component_amounts(
            &ctx.accounts.index.key(),
            &ctx.accounts.index,
            ctx.remaining_accounts,
            args.index_amount_in,
            current_supply,
            LargeBasketIntentKind::Redeem,
        )?;
        reserve_redeem_components(
            &ctx.accounts.index.key(),
            ctx.remaining_accounts,
            &component_amounts,
            ctx.program_id,
        )?;
        token::burn(
            CpiContext::new(
                ctx.accounts.token_program.to_account_info(),
                Burn {
                    mint: ctx.accounts.index_mint.to_account_info(),
                    from: ctx.accounts.owner_index_token_account.to_account_info(),
                    authority: ctx.accounts.owner.to_account_info(),
                },
            ),
            args.index_amount_in,
        )?;

        initialize_intent(
            &mut ctx.accounts.intent,
            IntentInit {
                index: ctx.accounts.index.key(),
                owner: ctx.accounts.owner.key(),
                nonce: args.nonce,
                kind: LargeBasketIntentKind::Redeem,
                index_amount: args.index_amount_in,
                supply_snapshot: current_supply,
                post_supply,
                quote_mint: ctx.accounts.quote_mint.key(),
                component_amounts,
                max_quote_in: 0,
                min_quote_out: args.min_quote_out,
                protocol_fee_bps: ctx.accounts.index.redeem_fee_bps,
                creator_fee_bps: ctx.accounts.index.creator_redeem_fee_bps,
                staking_fee_bps: ctx.accounts.index.staking_redeem_fee_bps,
                protocol_fee_recipient: ctx.accounts.index.fee_recipient,
                creator_fee_recipient: ctx.accounts.index.creator_fee_recipient,
                staking_pool: ctx.accounts.staking_pool.key(),
                component_count: large_basket_intent_component_count(&ctx.accounts.index),
                expires_at: args.expires_at,
                in_kind: args.in_kind,
                supply_era: ctx.accounts.index.supply_era,
                bump: ctx.bumps.intent,
            },
        )?;
        ctx.accounts.index.track_opened_intent()?;
        activate_intent_lock(&mut ctx.accounts.intent_lock, ctx.accounts.intent.key())?;
        Ok(())
    }
}

impl<'info> CollectLargeBasketIntentFees<'info> {
    pub fn handle(ctx: Context<'_, '_, '_, 'info, Self>) -> Result<()> {
        require!(
            ctx.accounts.intent.status == LargeBasketIntentStatus::Open,
            BasketError::InvalidLargeBasketIntent
        );
        // In-kind intents skim fees in-kind at execute time; there is no USDC fee
        // collection step for them.
        require!(
            !ctx.accounts.intent.in_kind,
            BasketError::InvalidLargeBasketIntent
        );
        // Swap-path redeems pay their fees with each leg's proceeds in
        // execute_large_basket_redeem_batch; only mints settle fees here.
        require!(
            ctx.accounts.intent.kind == LargeBasketIntentKind::Mint,
            BasketError::InvalidLargeBasketIntent
        );
        require!(
            !ctx.accounts.intent.fees_collected,
            BasketError::InvalidLargeBasketIntent
        );
        // Design B: fees are collected AFTER every component has executed, so
        // `quote_atoms_executed` reflects the real USD that moved through the
        // swaps. Require full completion, then derive fee atoms from that total.
        require!(
            ctx.accounts.intent.completed_components == ctx.accounts.intent.component_count,
            BasketError::LargeBasketComponentNotFilled
        );
        require_keys_eq!(
            ctx.accounts.intent.quote_mint,
            ctx.accounts.quote_mint.key(),
            BasketError::InvalidQuoteMint
        );
        require_keys_eq!(
            ctx.accounts.intent.staking_pool,
            ctx.accounts.staking_pool.key(),
            BasketError::InvalidStakingVault
        );
        require_keys_eq!(
            ctx.accounts.staking_pool.reward_mint,
            USDC_MINT,
            BasketError::InvalidRewardMint
        );

        // Compute realized fees from the executed quote total and persist them
        // on the intent so finalize can enforce the budget / min-out invariants.
        let fee_basis = ctx.accounts.intent.quote_atoms_executed;
        let split = large_basket_fee_split(
            fee_basis,
            ctx.accounts.intent.protocol_fee_bps,
            ctx.accounts.intent.creator_fee_bps,
            ctx.accounts.intent.staking_fee_bps,
        )?;
        let (protocol_fee, creator_fee) = route_creator_fee(
            split.protocol_fee,
            split.creator_fee,
            &ctx.accounts.intent.creator_fee_recipient,
        )?;
        ctx.accounts.intent.fee_basis_usdc_atoms = fee_basis;
        ctx.accounts.intent.protocol_fee_usdc_atoms = protocol_fee;
        ctx.accounts.intent.creator_fee_usdc_atoms = creator_fee;
        ctx.accounts.intent.staking_fee_usdc_atoms = split.staking_fee;

        let owner_quote_account =
            load_user_token_account(&ctx.accounts.owner_quote_token_account.to_account_info())?;
        validate_user_token_account(&owner_quote_account, &ctx.accounts.owner.key(), &USDC_MINT)?;
        let total_fees = ctx
            .accounts
            .intent
            .protocol_fee_usdc_atoms
            .checked_add(ctx.accounts.intent.creator_fee_usdc_atoms)
            .and_then(|value| value.checked_add(ctx.accounts.intent.staking_fee_usdc_atoms))
            .ok_or_else(|| error!(BasketError::ArithmeticOverflow))?;
        require!(
            owner_quote_account.amount >= total_fees,
            BasketError::QuoteBudgetExceeded
        );
        validate_fee_destination(
            &ctx.accounts.fee_recipient_quote_token_account,
            &ctx.accounts.intent.protocol_fee_recipient,
            ctx.accounts.intent.protocol_fee_usdc_atoms,
            BasketError::InvalidFeeRecipientTokenAccount,
        )?;
        validate_fee_destination(
            &ctx.accounts.creator_fee_recipient_quote_token_account,
            &ctx.accounts.intent.creator_fee_recipient,
            ctx.accounts.intent.creator_fee_usdc_atoms,
            BasketError::InvalidCreatorFeeRecipientTokenAccount,
        )?;
        validate_staking_vault(
            &ctx.accounts.staking_reward_vault,
            &ctx.accounts.staking_reward_vault.key(),
            &ctx.accounts.staking_authority.key(),
            &USDC_MINT,
        )?;

        transfer_fee_from_owner(
            &ctx,
            ctx.accounts
                .fee_recipient_quote_token_account
                .to_account_info(),
            ctx.accounts.intent.protocol_fee_usdc_atoms,
        )?;
        transfer_fee_from_owner(
            &ctx,
            ctx.accounts
                .creator_fee_recipient_quote_token_account
                .to_account_info(),
            ctx.accounts.intent.creator_fee_usdc_atoms,
        )?;
        if ctx.accounts.intent.staking_fee_usdc_atoms > 0 {
            transfer_fee_from_owner(
                &ctx,
                ctx.accounts.staking_reward_vault.to_account_info(),
                ctx.accounts.intent.staking_fee_usdc_atoms,
            )?;
            accrue_staking_rewards(
                &mut ctx.accounts.staking_pool,
                ctx.accounts.intent.staking_fee_usdc_atoms,
            )?;
        }

        ctx.accounts.intent.fees_collected = true;
        Ok(())
    }
}

impl<'info> ExecuteLargeBasketMintComponent<'info> {
    pub fn handle(
        mut ctx: Context<'_, '_, 'info, 'info, Self>,
        args: ExecuteLargeBasketMintComponentArgs,
    ) -> Result<()> {
        validate_component_execution_args(args.max_oracle_slippage_bps)?;
        validate_open_intent(
            &ctx.accounts.intent,
            LargeBasketIntentKind::Mint,
            args.component_index,
        )?;
        // In-kind mints skip the USDC fee collect and finalize on `in_kind`; filling one
        // through a swap would mint with no fee at all.
        require!(
            !ctx.accounts.intent.in_kind,
            BasketError::InvalidLargeBasketIntent
        );
        // Design B: fees are collected after execution, so executes no longer
        // require fees_collected. The budget guard in add_mint_quote_spent
        // accounts for the pending fee using the stored bps.
        // Deposits are the amounts fixed at open even if other intents have since moved
        // supply, as long as the basket has not emptied and restarted (see finalize).
        require_mint_basis(&ctx.accounts.index, &ctx.accounts.index_mint, &ctx.accounts.intent)?;
        require!(
            !component_filled(&ctx.accounts.intent, args.component_index)?,
            BasketError::LargeBasketComponentAlreadyFilled
        );
        let component = validate_component_accounts(
            &ctx.accounts.index.key(),
            &ctx.accounts.component_page,
            &ctx.accounts.component_page.key(),
            args.component_index,
            &ctx.accounts.component_mint.to_account_info(),
            &ctx.accounts.component_vault.to_account_info(),
            &ctx.accounts.component_token_program.to_account_info(),
        )?;
        create_associated_token_account_idempotent_for_token_program(
            ctx.accounts.associated_token_program.to_account_info(),
            ctx.accounts.owner.to_account_info(),
            ctx.accounts.component_vault.to_account_info(),
            ctx.accounts.vault_authority.to_account_info(),
            ctx.accounts.component_mint.to_account_info(),
            ctx.accounts.system_program.to_account_info(),
            ctx.accounts.component_token_program.to_account_info(),
        )?;
        let amount = intent_component_amount(&ctx.accounts.intent, args.component_index)?;
        let quote_spent = if amount == 0 {
            0
        } else if component.mint == ctx.accounts.quote_mint.key() {
            require!(args.swap.is_none(), BasketError::InvalidJupiterRoute);
            transfer_checked_from_user_quote(
                &ctx,
                ctx.accounts.component_vault.to_account_info(),
                amount,
            )?;
            amount
        } else {
            let swap = args
                .swap
                .as_ref()
                .ok_or_else(|| error!(BasketError::InvalidJupiterRoute))?;
            execute_mint_swap(&mut ctx, swap, &component, amount)?
        };
        require!(
            quote_spent <= args.max_quote_in,
            BasketError::QuoteBudgetExceeded
        );
        add_mint_quote_spent(&mut ctx.accounts.intent, quote_spent)?;
        // Record this component's spend so verify_mint_component_price can check the
        // effective price against the oracle in a later (swap-free) transaction.
        set_component_quote_atoms(&mut ctx.accounts.intent, args.component_index, quote_spent)?;
        mark_component_filled(&mut ctx.accounts.intent, args.component_index)?;

        emit!(LargeBasketComponentFilled {
            intent: ctx.accounts.intent.key(),
            index: ctx.accounts.index.key(),
            owner: ctx.accounts.owner.key(),
            component_index: args.component_index,
            amount,
            quote_atoms: quote_spent,
        });

        Ok(())
    }
}

impl<'info> ExecuteLargeBasketComponentBatch<'info> {
    // Batched mint: process several components in one tx. Each entry runs the exact
    // same per-component logic as the single execute (validate, idempotent vault,
    // swap-or-transfer, budget guard, record, fill), but the component accounts come
    // from remaining_accounts and the Jupiter route is scoped to ONLY that entry's
    // own accounts (see the per-entry candidate list below).
    pub fn handle_mint(
        ctx: Context<'_, '_, 'info, 'info, Self>,
        args: ExecuteLargeBasketMintBatchArgs,
    ) -> Result<()> {
        require!(
            !args.entries.is_empty(),
            BasketError::InvalidRemainingAccounts
        );
        // In-kind mints skip the USDC fee collect and finalize on `in_kind`; filling one
        // through a swap would mint with no fee at all.
        require!(
            !ctx.accounts.intent.in_kind,
            BasketError::InvalidLargeBasketIntent
        );
        require_mint_basis(&ctx.accounts.index, &ctx.accounts.index_mint, &ctx.accounts.intent)?;

        // Candidate ordering the Jupiter route indices are resolved against. MUST
        // match the server route compaction exactly (compactLargeBasketComponentPlan):
        //   [owner, index, index_mint, intent, vault_authority, quote_mint,
        //    owner_quote, jupiter] ++ [page, mint, vault, token_program]
        //    ++ [associated_token_program, quote_token_program, system_program] ++ route
        let shared_head: [AccountInfo<'info>; 8] = [
            ctx.accounts.owner.to_account_info(),
            ctx.accounts.index.to_account_info(),
            ctx.accounts.index_mint.to_account_info(),
            ctx.accounts.intent.to_account_info(),
            ctx.accounts.vault_authority.to_account_info(),
            ctx.accounts.quote_mint.to_account_info(),
            ctx.accounts.owner_quote_token_account.to_account_info(),
            ctx.accounts.jupiter_program.to_account_info(),
        ];

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

            validate_open_intent(
                &ctx.accounts.intent,
                LargeBasketIntentKind::Mint,
                entry.component_index,
            )?;
            require!(
                !component_filled(&ctx.accounts.intent, entry.component_index)?,
                BasketError::LargeBasketComponentAlreadyFilled
            );
            require_keys_eq!(
                *page_info.owner,
                *ctx.program_id,
                BasketError::InvalidLargeBasketComponentPage
            );
            let page =
                LargeBasketComponentPage::try_deserialize(&mut &page_info.try_borrow_data()?[..])?;
            let component = validate_component_accounts(
                &ctx.accounts.index.key(),
                &page,
                &page_info.key(),
                entry.component_index,
                mint_info,
                vault_info,
                token_program_info,
            )?;
            create_associated_token_account_idempotent_for_token_program(
                ctx.accounts.associated_token_program.to_account_info(),
                ctx.accounts.owner.to_account_info(),
                vault_info.clone(),
                ctx.accounts.vault_authority.to_account_info(),
                mint_info.clone(),
                ctx.accounts.system_program.to_account_info(),
                token_program_info.clone(),
            )?;

            let amount = intent_component_amount(&ctx.accounts.intent, entry.component_index)?;
            let quote_spent = if amount == 0 {
                require!(entry.swap.is_none(), BasketError::InvalidJupiterRoute);
                0
            } else if component.mint == ctx.accounts.quote_mint.key() {
                require!(entry.swap.is_none(), BasketError::InvalidJupiterRoute);
                transfer_user_quote_to(
                    &ctx.accounts.quote_token_program.to_account_info(),
                    &ctx.accounts.owner_quote_token_account.to_account_info(),
                    &ctx.accounts.quote_mint.to_account_info(),
                    &ctx.accounts.owner.to_account_info(),
                    vault_info.clone(),
                    amount,
                )?;
                amount
            } else {
                let swap = entry
                    .swap
                    .as_ref()
                    .ok_or_else(|| error!(BasketError::InvalidJupiterRoute))?;
                // Candidate list scoped to THIS entry only (a malicious route here
                // cannot reference another entry's vault), in the exact server order.
                let mut candidates: Vec<AccountInfo<'info>> =
                    Vec::with_capacity(8 + 4 + 3 + route_count);
                candidates.extend_from_slice(&shared_head);
                candidates.push(page_info.clone());
                candidates.push(mint_info.clone());
                candidates.push(vault_info.clone());
                candidates.push(token_program_info.clone());
                candidates.push(ctx.accounts.associated_token_program.to_account_info());
                candidates.push(ctx.accounts.quote_token_program.to_account_info());
                candidates.push(ctx.accounts.system_program.to_account_info());
                candidates.extend_from_slice(route_accounts);
                execute_mint_swap_inner(
                    &ctx.accounts.jupiter_program.to_account_info(),
                    &ctx.accounts.owner.key(),
                    &ctx.accounts.owner_quote_token_account.to_account_info(),
                    vault_info,
                    &candidates,
                    swap,
                    amount,
                )?
            };
            require!(
                quote_spent <= entry.max_quote_in,
                BasketError::QuoteBudgetExceeded
            );
            add_mint_quote_spent(&mut ctx.accounts.intent, quote_spent)?;
            set_component_quote_atoms(&mut ctx.accounts.intent, entry.component_index, quote_spent)?;
            mark_component_filled(&mut ctx.accounts.intent, entry.component_index)?;

            emit!(LargeBasketComponentFilled {
                intent: ctx.accounts.intent.key(),
                index: ctx.accounts.index.key(),
                owner: ctx.accounts.owner.key(),
                component_index: entry.component_index,
                amount,
                quote_atoms: quote_spent,
            });
        }
        require!(
            cursor == ctx.remaining_accounts.len(),
            BasketError::InvalidRemainingAccounts
        );
        Ok(())
    }

    // Batched redeem: mirror of handle_mint but swaps each component vault -> USDC
    // (vault authority signs). remaining_accounts layout + per-entry scoping identical.
}

impl<'info> ExecuteLargeBasketRedeemBatch<'info> {
    pub fn handle(
        mut ctx: Context<'_, '_, 'info, 'info, Self>,
        args: ExecuteLargeBasketRedeemBatchArgs,
    ) -> Result<()> {
        require!(
            !args.entries.is_empty(),
            BasketError::InvalidRemainingAccounts
        );
        require_keys_eq!(
            ctx.accounts.intent.staking_pool,
            ctx.accounts.staking_pool.key(),
            BasketError::InvalidStakingVault
        );
        require_keys_eq!(
            ctx.accounts.staking_pool.reward_mint,
            USDC_MINT,
            BasketError::InvalidRewardMint
        );
        validate_staking_vault(
            &ctx.accounts.staking_reward_vault,
            &ctx.accounts.staking_reward_vault.key(),
            &ctx.accounts.staking_authority.key(),
            &USDC_MINT,
        )?;

        // Candidate ordering MUST match the server compaction (redeem mode has no
        // associated_token_program/system): [owner, index, index_mint, intent,
        // vault_authority, quote_mint, owner_quote, jupiter] ++ [page, mint, vault,
        // token_program] ++ [quote_token_program] ++ route.
        let shared_head: [AccountInfo<'info>; 8] = [
            ctx.accounts.owner.to_account_info(),
            ctx.accounts.index.to_account_info(),
            ctx.accounts.index_mint.to_account_info(),
            ctx.accounts.intent.to_account_info(),
            ctx.accounts.vault_authority.to_account_info(),
            ctx.accounts.quote_mint.to_account_info(),
            ctx.accounts.owner_quote_token_account.to_account_info(),
            ctx.accounts.jupiter_program.to_account_info(),
        ];

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

            validate_open_intent(
                &ctx.accounts.intent,
                LargeBasketIntentKind::Redeem,
                entry.component_index,
            )?;
            require!(
                !component_filled(&ctx.accounts.intent, entry.component_index)?,
                BasketError::LargeBasketComponentAlreadyFilled
            );
            require_keys_eq!(
                *page_info.owner,
                *ctx.program_id,
                BasketError::InvalidLargeBasketComponentPage
            );
            let page =
                LargeBasketComponentPage::try_deserialize(&mut &page_info.try_borrow_data()?[..])?;
            let component = validate_component_accounts(
                &ctx.accounts.index.key(),
                &page,
                &page_info.key(),
                entry.component_index,
                mint_info,
                vault_info,
                token_program_info,
            )?;

            let amount = intent_component_amount(&ctx.accounts.intent, entry.component_index)?;
            let quote_received = if amount == 0 {
                require!(entry.swap.is_none(), BasketError::InvalidJupiterRoute);
                0
            } else if component.mint == ctx.accounts.quote_mint.key() {
                require!(entry.swap.is_none(), BasketError::InvalidJupiterRoute);
                let signer_seeds: &[&[u8]] = &[
                    VAULT_AUTHORITY_SEED,
                    ctx.accounts.index.to_account_info().key.as_ref(),
                    std::slice::from_ref(&ctx.accounts.index.vault_authority_bump),
                ];
                token_interface::transfer_checked(
                    CpiContext::new_with_signer(
                        ctx.accounts.quote_token_program.to_account_info(),
                        TransferChecked {
                            from: vault_info.clone(),
                            mint: ctx.accounts.quote_mint.to_account_info(),
                            to: ctx.accounts.owner_quote_token_account.to_account_info(),
                            authority: ctx.accounts.vault_authority.to_account_info(),
                        },
                        &[signer_seeds],
                    ),
                    amount,
                    crate::constants::USDC_DECIMALS,
                )?;
                amount
            } else {
                let swap = entry
                    .swap
                    .as_ref()
                    .ok_or_else(|| error!(BasketError::InvalidJupiterRoute))?;
                let mut candidates: Vec<AccountInfo<'info>> =
                    Vec::with_capacity(8 + 4 + 1 + route_count);
                candidates.extend_from_slice(&shared_head);
                candidates.push(page_info.clone());
                candidates.push(mint_info.clone());
                candidates.push(vault_info.clone());
                candidates.push(token_program_info.clone());
                candidates.push(ctx.accounts.quote_token_program.to_account_info());
                candidates.extend_from_slice(route_accounts);
                execute_redeem_swap_inner(
                    &ctx.accounts.jupiter_program.to_account_info(),
                    &ctx.accounts.index.key(),
                    &ctx.accounts.vault_authority.to_account_info(),
                    ctx.accounts.index.vault_authority_bump,
                    &ctx.accounts.owner_quote_token_account.to_account_info(),
                    vault_info,
                    &associated_token_address(
                        &ctx.accounts.vault_authority.key(),
                        &ctx.accounts.quote_mint.key(),
                    ),
                    &candidates,
                    swap,
                    amount,
                )?
            };
            require!(
                quote_received >= entry.min_quote_out,
                BasketError::QuoteBudgetExceeded
            );
            add_redeem_quote_received(&mut ctx.accounts.intent, quote_received)?;
            set_component_quote_atoms(
                &mut ctx.accounts.intent,
                entry.component_index,
                quote_received,
            )?;
            mark_component_filled(&mut ctx.accounts.intent, entry.component_index)?;
            charge_redeem_leg_fees(&mut ctx)?;

            emit!(LargeBasketComponentFilled {
                intent: ctx.accounts.intent.key(),
                index: ctx.accounts.index.key(),
                owner: ctx.accounts.owner.key(),
                component_index: entry.component_index,
                amount,
                quote_atoms: quote_received,
            });
        }
        require!(
            cursor == ctx.remaining_accounts.len(),
            BasketError::InvalidRemainingAccounts
        );

        // Every leg has paid its fee (and the last leg passed the min-out check), so settle
        // now: no later step exists that could be skipped to keep the basket locked.
        if ctx.accounts.intent.completed_components == ctx.accounts.intent.component_count {
            let intent_key = ctx.accounts.intent.key();
            ctx.accounts.intent.fees_collected = true;
            ctx.accounts.intent.status = LargeBasketIntentStatus::Finalized;
            ctx.accounts.index.track_closed_intent();
            clear_intent_lock(&mut ctx.accounts.intent_lock, intent_key)?;
            emit!(LargeBasketIntentFinalized {
                intent: intent_key,
                index: ctx.accounts.index.key(),
                owner: ctx.accounts.intent.owner,
                kind: ctx.accounts.intent.kind,
                index_amount: ctx.accounts.intent.index_amount,
                quote_atoms_executed: ctx.accounts.intent.quote_atoms_executed,
            });
        }
        Ok(())
    }
}

// Redeem fees accrue leg by leg: the totals are re-derived from the cumulative proceeds and
// only the increase is charged, so the legs sum to exactly the fee on the whole redeem.
fn redeem_fee_targets(intent: &LargeBasketIntent, fee_basis: u64) -> Result<(u64, u64, u64)> {
    let split = large_basket_fee_split(
        fee_basis,
        intent.protocol_fee_bps,
        intent.creator_fee_bps,
        intent.staking_fee_bps,
    )?;
    let (protocol_fee, creator_fee) = route_creator_fee(
        split.protocol_fee,
        split.creator_fee,
        &intent.creator_fee_recipient,
    )?;
    Ok((protocol_fee, creator_fee, split.staking_fee))
}

#[inline(never)]
fn charge_redeem_leg_fees<'info>(
    ctx: &mut Context<'_, '_, 'info, 'info, ExecuteLargeBasketRedeemBatch<'info>>,
) -> Result<()> {
    let intent = &ctx.accounts.intent;
    let (protocol_fee, creator_fee, staking_fee) =
        redeem_fee_targets(intent, intent.quote_atoms_executed)?;
    let due = |target: u64, charged: u64| {
        target
            .checked_sub(charged)
            .ok_or_else(|| error!(BasketError::ArithmeticOverflow))
    };
    let protocol_due = due(protocol_fee, intent.protocol_fee_usdc_atoms)?;
    let creator_due = due(creator_fee, intent.creator_fee_usdc_atoms)?;
    let staking_due = due(staking_fee, intent.staking_fee_usdc_atoms)?;
    validate_fee_destination(
        &ctx.accounts.fee_recipient_quote_token_account,
        &intent.protocol_fee_recipient,
        protocol_due,
        BasketError::InvalidFeeRecipientTokenAccount,
    )?;
    validate_fee_destination(
        &ctx.accounts.creator_fee_recipient_quote_token_account,
        &intent.creator_fee_recipient,
        creator_due,
        BasketError::InvalidCreatorFeeRecipientTokenAccount,
    )?;

    let fee_recipient = ctx.accounts.fee_recipient_quote_token_account.to_account_info();
    let creator_fee_recipient = ctx
        .accounts
        .creator_fee_recipient_quote_token_account
        .to_account_info();
    let staking_reward_vault = ctx.accounts.staking_reward_vault.to_account_info();
    transfer_redeem_fee(ctx, fee_recipient, protocol_due)?;
    transfer_redeem_fee(ctx, creator_fee_recipient, creator_due)?;
    if staking_due > 0 {
        transfer_redeem_fee(ctx, staking_reward_vault, staking_due)?;
        accrue_staking_rewards(&mut ctx.accounts.staking_pool, staking_due)?;
    }

    let intent = &mut ctx.accounts.intent;
    intent.fee_basis_usdc_atoms = intent.quote_atoms_executed;
    intent.protocol_fee_usdc_atoms = protocol_fee;
    intent.creator_fee_usdc_atoms = creator_fee;
    intent.staking_fee_usdc_atoms = staking_fee;
    Ok(())
}

fn transfer_redeem_fee<'info>(
    ctx: &Context<'_, '_, 'info, 'info, ExecuteLargeBasketRedeemBatch<'info>>,
    destination: AccountInfo<'info>,
    amount: u64,
) -> Result<()> {
    if amount == 0 || destination.key() == ctx.accounts.owner_quote_token_account.key() {
        return Ok(());
    }

    token_interface::transfer_checked(
        CpiContext::new(
            ctx.accounts.quote_token_program.to_account_info(),
            TransferChecked {
                from: ctx.accounts.owner_quote_token_account.to_account_info(),
                mint: ctx.accounts.quote_mint.to_account_info(),
                to: destination,
                authority: ctx.accounts.owner.to_account_info(),
            },
        ),
        amount,
        crate::constants::USDC_DECIMALS,
    )
}

impl<'info> ExecuteLargeBasketRedeemComponent<'info> {
    // Closed: this single-leg path paid USDC out with no fee, leaving a separate collect
    // step the redeemer could skip. execute_large_basket_redeem_batch (one entry is fine)
    // charges each leg's fee with its proceeds.
    pub fn handle(
        _ctx: Context<'_, '_, 'info, 'info, Self>,
        _args: ExecuteLargeBasketRedeemComponentArgs,
    ) -> Result<()> {
        err!(BasketError::RedeemRequiresBatchExecution)
    }
}

// Shared logic for the deferred per-component price verification (mint & redeem).
fn verify_component_price_inner<'info>(
    intent: &mut LargeBasketIntent,
    page: &LargeBasketComponentPage,
    page_key: &Pubkey,
    queue: &AccountInfo<'info>,
    quote: &AccountInfo<'info>,
    slothashes: &AccountInfo<'info>,
    instructions: &AccountInfo<'info>,
    args: &VerifyLargeBasketComponentPriceArgs,
    is_mint: bool,
) -> Result<()> {
    validate_component_execution_args(args.max_oracle_slippage_bps)?;
    validate_open_intent(
        intent,
        if is_mint { LargeBasketIntentKind::Mint } else { LargeBasketIntentKind::Redeem },
        args.component_index,
    )?;
    // Must be filled (swap done) and not already verified.
    require!(
        component_filled(intent, args.component_index)?,
        BasketError::LargeBasketComponentNotFilled
    );
    // Resolve the component from the page it lives on.
    validate_component_page_identity(&intent.index, page_key, page)?;
    let local_index = page.component_offset(args.component_index)?;
    let component = &page.components[local_index];

    let component_amount = intent_component_amount(intent, args.component_index)?;
    let quote_atoms = intent_component_quote_atoms(intent, args.component_index)?;

    // A zero-amount component never swapped; nothing to price-check, just mark it.
    if component_amount > 0 {
        let oracle_price = {
            let prices = verified_switchboard_prices(
                queue,
                quote,
                slothashes,
                instructions,
                Clock::get()?.slot,
                args.switchboard_max_age_slots,
            )?;
            switchboard_feed_price(&prices, &component.oracle_pair)?
        };
        if is_mint {
            validate_buy_execution_price(
                quote_atoms,
                component_amount,
                crate::constants::USDC_DECIMALS,
                component.decimals,
                oracle_price,
                args.max_oracle_slippage_bps,
            )?;
        } else {
            validate_sell_execution_price(
                quote_atoms,
                component_amount,
                crate::constants::USDC_DECIMALS,
                component.decimals,
                oracle_price,
                args.max_oracle_slippage_bps,
            )?;
        }
    }

    mark_component_verified(intent, args.component_index)?;
    Ok(())
}

impl<'info> VerifyLargeBasketComponentPrice<'info> {
    pub fn handle_mint(
        ctx: Context<'_, '_, 'info, 'info, Self>,
        args: VerifyLargeBasketComponentPriceArgs,
    ) -> Result<()> {
        let page_key = ctx.accounts.component_page.key();
        verify_component_price_inner(
            &mut ctx.accounts.intent,
            &ctx.accounts.component_page,
            &page_key,
            &ctx.accounts.switchboard_queue.to_account_info(),
            &ctx.accounts.switchboard_quote.to_account_info(),
            &ctx.accounts.slothashes.to_account_info(),
            &ctx.accounts.instructions_sysvar.to_account_info(),
            &args,
            true,
        )
    }

    pub fn handle_redeem(
        ctx: Context<'_, '_, 'info, 'info, Self>,
        args: VerifyLargeBasketComponentPriceArgs,
    ) -> Result<()> {
        let page_key = ctx.accounts.component_page.key();
        verify_component_price_inner(
            &mut ctx.accounts.intent,
            &ctx.accounts.component_page,
            &page_key,
            &ctx.accounts.switchboard_queue.to_account_info(),
            &ctx.accounts.switchboard_quote.to_account_info(),
            &ctx.accounts.slothashes.to_account_info(),
            &ctx.accounts.instructions_sysvar.to_account_info(),
            &args,
            false,
        )
    }
}

impl<'info> FinalizeLargeBasketMintIntent<'info> {
    pub fn handle(ctx: Context<'_, '_, 'info, 'info, Self>) -> Result<()> {
        validate_finalizable_intent(&ctx.accounts.intent, LargeBasketIntentKind::Mint)?;
        // In-kind intents skim fees in-kind at execute time, so they have no USDC
        // collect step; swap intents must have collected USDC fees first.
        require!(
            ctx.accounts.intent.in_kind || ctx.accounts.intent.fees_collected,
            BasketError::InvalidLargeBasketIntent
        );
        // Per-component oracle price-verification was dropped in favor of relying on
        // Jupiter per-swap slippage + the intent's aggregate max_quote_in bound (a bad
        // swap only over-spends the minter's own budget, which is already capped). The
        // verify_*_component_price instructions remain available but are not required.
        let total_fees = intent_total_fees(&ctx.accounts.intent)?;
        let total_quote = ctx
            .accounts
            .intent
            .quote_atoms_executed
            .checked_add(total_fees)
            .ok_or_else(|| error!(BasketError::ArithmeticOverflow))?;
        require!(
            total_quote <= ctx.accounts.intent.max_quote_in,
            BasketError::QuoteBudgetExceeded
        );
        // The deposits were fixed at open from the basket's composition then. Other owners'
        // intents may settle in between, but within one supply era that only moves reserves
        // per token by rounding dust (rounded in holders' favour) or by donations, so the
        // requested amount stays backed. Crossing an empty basket would change the basis.
        let index_mint = load_mint(&ctx.accounts.index_mint.to_account_info())?;
        let opened_era = ctx.accounts.intent.supply_era;
        let opened_empty = ctx.accounts.intent.supply_snapshot == 0;
        ctx.accounts
            .index
            .settle_mint_era(opened_era, opened_empty, index_mint.supply)?;
        // max_supply is enforced when intents open. Rechecking here would let whoever settles
        // first strand other owners' fully paid mints, so concurrent mints may overshoot the
        // cap by at most what was already in flight.
        account_filled_mint_components(
            &ctx.accounts.index.key(),
            ctx.remaining_accounts,
            &ctx.accounts.intent,
            ctx.program_id,
        )?;
        create_owner_index_ata_if_needed(&ctx)?;
        let signer_seeds: &[&[u8]] = &[
            VAULT_AUTHORITY_SEED,
            ctx.accounts.index.to_account_info().key.as_ref(),
            &[ctx.accounts.index.vault_authority_bump],
        ];
        token::mint_to(
            CpiContext::new_with_signer(
                ctx.accounts.token_program.to_account_info(),
                MintTo {
                    mint: ctx.accounts.index_mint.to_account_info(),
                    to: ctx.accounts.owner_index_token_account.to_account_info(),
                    authority: ctx.accounts.vault_authority.to_account_info(),
                },
                &[signer_seeds],
            ),
            ctx.accounts.intent.index_amount,
        )?;
        ctx.accounts.intent.status = LargeBasketIntentStatus::Finalized;
        ctx.accounts.index.track_closed_intent();
        clear_intent_lock(&mut ctx.accounts.intent_lock, ctx.accounts.intent.key())?;

        emit!(LargeBasketIntentFinalized {
            intent: ctx.accounts.intent.key(),
            index: ctx.accounts.index.key(),
            owner: ctx.accounts.owner.key(),
            kind: ctx.accounts.intent.kind,
            index_amount: ctx.accounts.intent.index_amount,
            quote_atoms_executed: ctx.accounts.intent.quote_atoms_executed,
        });

        Ok(())
    }
}

impl<'info> FinalizeLargeBasketRedeemIntent<'info> {
    pub fn handle(ctx: Context<Self>) -> Result<()> {
        validate_finalizable_intent(&ctx.accounts.intent, LargeBasketIntentKind::Redeem)?;
        // In-kind redeem delivers component tokens directly and skims fees in-kind at
        // execute time, so there is no USDC collect / min-out leg to enforce.
        if !ctx.accounts.intent.in_kind {
            require!(
                ctx.accounts.intent.fees_collected,
                BasketError::InvalidLargeBasketIntent
            );
            // Per-component oracle verification dropped; aggregate min_quote_out
            // + Jupiter per-swap slippage bound the redeemer's proceeds.
            let total_fees = intent_total_fees(&ctx.accounts.intent)?;
            let net_quote = ctx
                .accounts
                .intent
                .quote_atoms_executed
                .checked_sub(total_fees)
                .ok_or_else(|| error!(BasketError::QuoteBudgetExceeded))?;
            require!(
                net_quote >= ctx.accounts.intent.min_quote_out,
                BasketError::QuoteBudgetExceeded
            );
        }
        ctx.accounts.intent.status = LargeBasketIntentStatus::Finalized;
        ctx.accounts.index.track_closed_intent();
        clear_intent_lock(&mut ctx.accounts.intent_lock, ctx.accounts.intent.key())?;

        emit!(LargeBasketIntentFinalized {
            intent: ctx.accounts.intent.key(),
            index: ctx.accounts.index.key(),
            owner: ctx.accounts.intent.owner,
            kind: ctx.accounts.intent.kind,
            index_amount: ctx.accounts.intent.index_amount,
            quote_atoms_executed: ctx.accounts.intent.quote_atoms_executed,
        });

        Ok(())
    }
}

impl<'info> CancelUnfilledLargeBasketMintIntent<'info> {
    pub fn handle(ctx: Context<Self>) -> Result<()> {
        require!(
            ctx.accounts.intent.status == LargeBasketIntentStatus::Open,
            BasketError::InvalidLargeBasketIntent
        );
        require!(
            ctx.accounts.intent.kind == LargeBasketIntentKind::Mint,
            BasketError::InvalidLargeBasketIntent
        );
        require!(
            ctx.accounts.intent.completed_components == 0,
            BasketError::LargeBasketComponentAlreadyFilled
        );
        ctx.accounts.intent.status = LargeBasketIntentStatus::Cancelled;
        ctx.accounts.index.track_closed_intent();
        clear_intent_lock(&mut ctx.accounts.intent_lock, ctx.accounts.intent.key())?;
        Ok(())
    }
}

impl<'info> CancelUnfilledLargeBasketRedeemIntent<'info> {
    pub fn handle(ctx: Context<'_, '_, 'info, 'info, Self>) -> Result<()> {
        require!(
            ctx.accounts.intent.status == LargeBasketIntentStatus::Open,
            BasketError::InvalidLargeBasketIntent
        );
        require!(
            ctx.accounts.intent.kind == LargeBasketIntentKind::Redeem,
            BasketError::InvalidLargeBasketIntent
        );
        require!(
            ctx.accounts.intent.completed_components == 0,
            BasketError::LargeBasketComponentAlreadyFilled
        );
        restore_unfilled_redeem_components(
            &ctx.accounts.index.key(),
            ctx.remaining_accounts,
            &ctx.accounts.intent,
            ctx.program_id,
        )?;
        remint_unfilled_redeem_intent(
            &ctx.accounts.index,
            &ctx.accounts.index_mint,
            &ctx.accounts.vault_authority,
            &ctx.accounts.intent,
            &ctx.accounts.owner_index_token_account,
            &ctx.accounts.token_program,
        )?;
        ctx.accounts.intent.status = LargeBasketIntentStatus::Cancelled;
        ctx.accounts.index.track_closed_intent();
        clear_intent_lock(&mut ctx.accounts.intent_lock, ctx.accounts.intent.key())?;
        Ok(())
    }
}

impl<'info> CancelExpiredLargeBasketIntent<'info> {
    /// Anyone may settle an expired intent, so a stale one can never hold up a rebalance.
    /// Owed components leave the vaults a few per call, either to a token account the owner
    /// holds or to the refund escrow, where the owner claims them later
    /// (claim_large_basket_refund). The intent stays Refunding, still counted as open, until
    /// every owed component has left the vaults.
    pub fn handle(ctx: Context<'_, '_, 'info, 'info, Self>) -> Result<()> {
        let status = ctx.accounts.intent.status;
        require!(
            status == LargeBasketIntentStatus::Open || status == LargeBasketIntentStatus::Refunding,
            BasketError::InvalidLargeBasketIntent
        );
        require!(
            Clock::get()?.unix_timestamp > ctx.accounts.intent.expires_at,
            BasketError::InvalidLargeBasketIntentExpiry
        );
        let kind = ctx.accounts.intent.kind;
        let completed = ctx.accounts.intent.completed_components;
        // A fully executed redeem has nothing to return; finalize_large_basket_redeem_intent
        // (no signer needed) settles it.
        require!(
            !(kind == LargeBasketIntentKind::Redeem
                && completed == ctx.accounts.intent.component_count),
            BasketError::InvalidLargeBasketIntent
        );

        if completed == 0 {
            if kind == LargeBasketIntentKind::Redeem {
                restore_unfilled_redeem_components(
                    &ctx.accounts.index.key(),
                    ctx.remaining_accounts,
                    &ctx.accounts.intent,
                    ctx.program_id,
                )?;
                remint_unfilled_redeem_intent(
                    &ctx.accounts.index,
                    &ctx.accounts.index_mint,
                    &ctx.accounts.vault_authority,
                    &ctx.accounts.intent,
                    &ctx.accounts.owner_index_token_account,
                    &ctx.accounts.token_program,
                )?;
            } else {
                require!(
                    ctx.remaining_accounts.is_empty(),
                    BasketError::InvalidRemainingAccounts
                );
            }
        } else {
            // Mints return what they deposited; redeems return what was not yet sold.
            let selection = if kind == LargeBasketIntentKind::Mint {
                CancelledComponentSelection::Filled
            } else {
                CancelledComponentSelection::Unfilled
            };
            let all_returned = return_owed_components(
                ctx.accounts.index.key(),
                ctx.accounts.index.vault_authority_bump,
                &ctx.accounts.vault_authority.to_account_info(),
                ctx.remaining_accounts,
                &mut ctx.accounts.intent,
                selection,
            )?;
            if !all_returned {
                ctx.accounts.intent.status = LargeBasketIntentStatus::Refunding;
                return Ok(());
            }
        }

        ctx.accounts.intent.status = LargeBasketIntentStatus::Cancelled;
        ctx.accounts.index.track_closed_intent();
        // With components waiting in escrow the owner's lock keeps pointing here, so their
        // next visit finds the claim; it only holds back that owner's own new intents.
        if ctx.accounts.intent.escrowed_bitmap.iter().all(|byte| *byte == 0) {
            clear_intent_lock(&mut ctx.accounts.intent_lock, ctx.accounts.intent.key())?;
        }
        Ok(())
    }
}

#[derive(Accounts)]
pub struct ClaimLargeBasketRefund<'info> {
    pub owner: Signer<'info>,
    pub index: Account<'info, IndexState>,
    #[account(mut, has_one = owner @ BasketError::InvalidLargeBasketIntent, has_one = index @ BasketError::InvalidLargeBasketIntent)]
    pub intent: Account<'info, LargeBasketIntent>,
    #[account(
        mut,
        seeds = [
            LARGE_BASKET_INTENT_LOCK_SEED,
            index.key().as_ref(),
            owner.key().as_ref(),
        ],
        bump = intent_lock.bump
    )]
    pub intent_lock: Account<'info, LargeBasketIntentLock>,
    /// CHECK: PDA owning the refund escrow token accounts; signs the claim transfers.
    #[account(seeds = [REFUND_ESCROW_SEED, index.key().as_ref()], bump)]
    pub refund_escrow: UncheckedAccount<'info>,
}

impl<'info> ClaimLargeBasketRefund<'info> {
    /// Moves components an expired intent left in the refund escrow to the owner.
    /// remaining_accounts: the index's pages, then [mint, escrow token account, owner token
    /// account, token program] groups for any of the escrowed components.
    pub fn handle(ctx: Context<'_, '_, 'info, 'info, Self>) -> Result<()> {
        require!(
            ctx.accounts.intent.status == LargeBasketIntentStatus::Cancelled,
            BasketError::InvalidLargeBasketIntent
        );
        let index_key = ctx.accounts.index.key();
        let page_count = usize::from(ctx.accounts.index.large_basket_page_count);
        require!(
            ctx.remaining_accounts.len() > page_count
                && (ctx.remaining_accounts.len() - page_count) % 4 == 0,
            BasketError::InvalidRemainingAccounts
        );
        let (page_infos, transfer_infos) = ctx.remaining_accounts.split_at(page_count);
        let pages = load_ordered_component_pages(&index_key, &ctx.accounts.index, page_infos)?;
        let escrow = ctx.accounts.refund_escrow.key();
        let escrow_bump = [ctx.bumps.refund_escrow];
        let signer_seeds: &[&[u8]] = &[REFUND_ESCROW_SEED, index_key.as_ref(), &escrow_bump];
        for group in transfer_infos.chunks(4) {
            let (component_index, component) = find_component_by_mint(&pages, &group[0].key())?;
            require!(
                bitmap_get(&ctx.accounts.intent.escrowed_bitmap, component_index)?,
                BasketError::InvalidRemainingAccounts
            );
            require_keys_eq!(*group[0].owner, component.token_program, BasketError::InvalidTokenMint);
            require_keys_eq!(group[3].key(), component.token_program, BasketError::InvalidTokenProgram);
            require_keys_eq!(
                group[1].key(),
                associated_token_address_with_token_program(&escrow, &component.mint, &component.token_program),
                BasketError::InvalidVaultAccount
            );
            let owner_token = load_interface_token_account(&group[2])?;
            require_keys_eq!(owner_token.owner, ctx.accounts.intent.owner, BasketError::InvalidUserTokenAccount);
            require_keys_eq!(owner_token.mint, component.mint, BasketError::InvalidUserTokenAccount);
            transfer_component_checked(
                &group[3],
                &group[1],
                &group[0],
                &group[2],
                &ctx.accounts.refund_escrow.to_account_info(),
                &[signer_seeds],
                intent_component_amount(&ctx.accounts.intent, component_index)?,
                component.decimals,
            )?;
            let (byte_index, bit) = bitmap_position(component_index)?;
            ctx.accounts.intent.escrowed_bitmap[byte_index] &= !bit;
        }
        if ctx.accounts.intent.escrowed_bitmap.iter().all(|byte| *byte == 0)
            && ctx.accounts.intent_lock.active_intent == ctx.accounts.intent.key()
        {
            clear_intent_lock(&mut ctx.accounts.intent_lock, ctx.accounts.intent.key())?;
        }
        Ok(())
    }
}

fn find_component_by_mint<'a>(
    pages: &'a [Account<'_, LargeBasketComponentPage>],
    mint: &Pubkey,
) -> Result<(u16, &'a LargeBasketComponent)> {
    for page in pages {
        for (local_index, component) in page.components.iter().enumerate() {
            if component.mint == *mint {
                let component_index = page
                    .start_component_index
                    .checked_add(local_index as u16)
                    .ok_or_else(|| error!(BasketError::ArithmeticOverflow))?;
                return Ok((component_index, component));
            }
        }
    }
    err!(BasketError::InvalidComponentMint)
}

// ---------------------------------------------------------------------------
// In-kind execute: deposit/withdraw the EXACT component tokens to/from the owner
// instead of swapping via Jupiter. No oracle, no USDC leg; the fee is skimmed in the
// component token itself at execute time. Used when intent.in_kind == true. These
// instructions are deliberately separate from the swap-path execute handlers so the
// audited Jupiter route-scoping logic stays untouched.
// ---------------------------------------------------------------------------

#[derive(AnchorSerialize, AnchorDeserialize, Clone, Debug)]
pub struct ExecuteLargeBasketMintComponentInKindArgs {
    pub component_index: u16,
}

#[derive(AnchorSerialize, AnchorDeserialize, Clone, Debug)]
pub struct ExecuteLargeBasketRedeemComponentInKindArgs {
    pub component_index: u16,
}

#[derive(Accounts)]
pub struct ExecuteLargeBasketMintComponentInKind<'info> {
    #[account(mut)]
    pub owner: Signer<'info>,
    #[account(has_one = index_mint @ BasketError::IndexMintMismatch)]
    pub index: Account<'info, IndexState>,
    /// CHECK: Validated as the configured classic SPL index mint.
    pub index_mint: UncheckedAccount<'info>,
    #[account(
        mut,
        has_one = owner @ BasketError::InvalidLargeBasketIntent,
        has_one = index @ BasketError::InvalidLargeBasketIntent
    )]
    pub intent: Account<'info, LargeBasketIntent>,
    /// CHECK: PDA authority over component vaults.
    #[account(
        seeds = [VAULT_AUTHORITY_SEED, index.key().as_ref()],
        bump = index.vault_authority_bump
    )]
    pub vault_authority: UncheckedAccount<'info>,
    #[account(mut)]
    pub component_page: Account<'info, LargeBasketComponentPage>,
    /// CHECK: Validated against the component page.
    pub component_mint: UncheckedAccount<'info>,
    /// CHECK: Validated against the component page.
    #[account(mut)]
    pub component_vault: UncheckedAccount<'info>,
    /// CHECK: Validated against the component page (the component's token program).
    pub component_token_program: UncheckedAccount<'info>,
    /// CHECK: The owner's component token account, source of the in-kind deposit.
    #[account(mut)]
    pub owner_component_token_account: UncheckedAccount<'info>,
    /// CHECK: Protocol fee recipient's component token account; validated when a fee is due.
    #[account(mut)]
    pub protocol_fee_component_account: UncheckedAccount<'info>,
    /// CHECK: Creator fee recipient's component token account; validated when a fee is due.
    #[account(mut)]
    pub creator_fee_component_account: UncheckedAccount<'info>,
    /// CHECK: Validated as the Associated Token Program.
    #[account(address = ASSOCIATED_TOKEN_ID @ BasketError::InvalidAssociatedTokenProgram)]
    pub associated_token_program: UncheckedAccount<'info>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct ExecuteLargeBasketRedeemComponentInKind<'info> {
    #[account(mut)]
    pub owner: Signer<'info>,
    #[account(has_one = index_mint @ BasketError::IndexMintMismatch)]
    pub index: Account<'info, IndexState>,
    /// CHECK: Validated as the configured classic SPL index mint.
    pub index_mint: UncheckedAccount<'info>,
    #[account(
        mut,
        has_one = owner @ BasketError::InvalidLargeBasketIntent,
        has_one = index @ BasketError::InvalidLargeBasketIntent
    )]
    pub intent: Account<'info, LargeBasketIntent>,
    /// CHECK: PDA authority over component vaults; signs the in-kind withdrawals.
    #[account(
        seeds = [VAULT_AUTHORITY_SEED, index.key().as_ref()],
        bump = index.vault_authority_bump
    )]
    pub vault_authority: UncheckedAccount<'info>,
    #[account(mut)]
    pub component_page: Account<'info, LargeBasketComponentPage>,
    /// CHECK: Validated against the component page.
    pub component_mint: UncheckedAccount<'info>,
    /// CHECK: Validated against the component page.
    #[account(mut)]
    pub component_vault: UncheckedAccount<'info>,
    /// CHECK: Validated against the component page (the component's token program).
    pub component_token_program: UncheckedAccount<'info>,
    /// CHECK: The owner's component token account, destination of the in-kind withdrawal.
    #[account(mut)]
    pub owner_component_token_account: UncheckedAccount<'info>,
    /// CHECK: Protocol fee recipient's component token account; validated when a fee is due.
    #[account(mut)]
    pub protocol_fee_component_account: UncheckedAccount<'info>,
    /// CHECK: Creator fee recipient's component token account; validated when a fee is due.
    #[account(mut)]
    pub creator_fee_component_account: UncheckedAccount<'info>,
    /// CHECK: Validated as the Associated Token Program.
    #[account(address = ASSOCIATED_TOKEN_ID @ BasketError::InvalidAssociatedTokenProgram)]
    pub associated_token_program: UncheckedAccount<'info>,
    pub system_program: Program<'info, System>,
}

impl<'info> ExecuteLargeBasketMintComponentInKind<'info> {
    pub fn handle(
        ctx: Context<'_, '_, 'info, 'info, Self>,
        args: ExecuteLargeBasketMintComponentInKindArgs,
    ) -> Result<()> {
        validate_open_intent(
            &ctx.accounts.intent,
            LargeBasketIntentKind::Mint,
            args.component_index,
        )?;
        require!(
            ctx.accounts.intent.in_kind,
            BasketError::InvalidLargeBasketIntent
        );
        require_mint_basis(&ctx.accounts.index, &ctx.accounts.index_mint, &ctx.accounts.intent)?;
        require!(
            !component_filled(&ctx.accounts.intent, args.component_index)?,
            BasketError::LargeBasketComponentAlreadyFilled
        );
        let component = validate_component_accounts(
            &ctx.accounts.index.key(),
            &ctx.accounts.component_page,
            &ctx.accounts.component_page.key(),
            args.component_index,
            &ctx.accounts.component_mint.to_account_info(),
            &ctx.accounts.component_vault.to_account_info(),
            &ctx.accounts.component_token_program.to_account_info(),
        )?;
        create_associated_token_account_idempotent_for_token_program(
            ctx.accounts.associated_token_program.to_account_info(),
            ctx.accounts.owner.to_account_info(),
            ctx.accounts.component_vault.to_account_info(),
            ctx.accounts.vault_authority.to_account_info(),
            ctx.accounts.component_mint.to_account_info(),
            ctx.accounts.system_program.to_account_info(),
            ctx.accounts.component_token_program.to_account_info(),
        )?;
        let amount = intent_component_amount(&ctx.accounts.intent, args.component_index)?;
        if amount > 0 {
            // Deposit the exact backing amount from the owner into the component vault.
            transfer_component_checked(
                &ctx.accounts.component_token_program.to_account_info(),
                &ctx.accounts.owner_component_token_account.to_account_info(),
                &ctx.accounts.component_mint.to_account_info(),
                &ctx.accounts.component_vault.to_account_info(),
                &ctx.accounts.owner.to_account_info(),
                &[],
                amount,
                component.decimals,
            )?;
            // Skim the in-kind fee on top of the deposit, paid by the owner.
            let (protocol_amount, creator_amount) =
                in_kind_fee_amounts(&ctx.accounts.intent, amount)?;
            if protocol_amount > 0 {
                validate_recipient_component_ata(
                    &ctx.accounts.protocol_fee_component_account.to_account_info(),
                    &ctx.accounts.intent.protocol_fee_recipient,
                    &component.mint,
                )?;
                transfer_component_checked(
                    &ctx.accounts.component_token_program.to_account_info(),
                    &ctx.accounts.owner_component_token_account.to_account_info(),
                    &ctx.accounts.component_mint.to_account_info(),
                    &ctx.accounts.protocol_fee_component_account.to_account_info(),
                    &ctx.accounts.owner.to_account_info(),
                    &[],
                    protocol_amount,
                    component.decimals,
                )?;
            }
            if creator_amount > 0 {
                validate_recipient_component_ata(
                    &ctx.accounts.creator_fee_component_account.to_account_info(),
                    &ctx.accounts.intent.creator_fee_recipient,
                    &component.mint,
                )?;
                transfer_component_checked(
                    &ctx.accounts.component_token_program.to_account_info(),
                    &ctx.accounts.owner_component_token_account.to_account_info(),
                    &ctx.accounts.component_mint.to_account_info(),
                    &ctx.accounts.creator_fee_component_account.to_account_info(),
                    &ctx.accounts.owner.to_account_info(),
                    &[],
                    creator_amount,
                    component.decimals,
                )?;
            }
        }
        set_component_quote_atoms(&mut ctx.accounts.intent, args.component_index, 0)?;
        mark_component_filled(&mut ctx.accounts.intent, args.component_index)?;
        emit!(LargeBasketComponentFilled {
            intent: ctx.accounts.intent.key(),
            index: ctx.accounts.index.key(),
            owner: ctx.accounts.owner.key(),
            component_index: args.component_index,
            amount,
            quote_atoms: 0,
        });
        Ok(())
    }
}

impl<'info> ExecuteLargeBasketRedeemComponentInKind<'info> {
    pub fn handle(
        ctx: Context<'_, '_, 'info, 'info, Self>,
        args: ExecuteLargeBasketRedeemComponentInKindArgs,
    ) -> Result<()> {
        validate_open_intent(
            &ctx.accounts.intent,
            LargeBasketIntentKind::Redeem,
            args.component_index,
        )?;
        require!(
            ctx.accounts.intent.in_kind,
            BasketError::InvalidLargeBasketIntent
        );
        require!(
            !component_filled(&ctx.accounts.intent, args.component_index)?,
            BasketError::LargeBasketComponentAlreadyFilled
        );
        let component = validate_component_accounts(
            &ctx.accounts.index.key(),
            &ctx.accounts.component_page,
            &ctx.accounts.component_page.key(),
            args.component_index,
            &ctx.accounts.component_mint.to_account_info(),
            &ctx.accounts.component_vault.to_account_info(),
            &ctx.accounts.component_token_program.to_account_info(),
        )?;
        // Ensure the owner's destination ATA exists.
        create_associated_token_account_idempotent_for_token_program(
            ctx.accounts.associated_token_program.to_account_info(),
            ctx.accounts.owner.to_account_info(),
            ctx.accounts.owner_component_token_account.to_account_info(),
            ctx.accounts.owner.to_account_info(),
            ctx.accounts.component_mint.to_account_info(),
            ctx.accounts.system_program.to_account_info(),
            ctx.accounts.component_token_program.to_account_info(),
        )?;
        let amount = intent_component_amount(&ctx.accounts.intent, args.component_index)?;
        if amount > 0 {
            let (protocol_amount, creator_amount) =
                in_kind_fee_amounts(&ctx.accounts.intent, amount)?;
            let total_fee = protocol_amount
                .checked_add(creator_amount)
                .ok_or_else(|| error!(BasketError::ArithmeticOverflow))?;
            let owner_amount = amount
                .checked_sub(total_fee)
                .ok_or_else(|| error!(BasketError::ArithmeticOverflow))?;
            let index_key = ctx.accounts.index.key();
            let vault_authority_bump = [ctx.accounts.index.vault_authority_bump];
            let signer_seeds: &[&[u8]] =
                &[VAULT_AUTHORITY_SEED, index_key.as_ref(), &vault_authority_bump];
            // Release the net amount to the owner, then skim fees, all from the vault.
            transfer_component_checked(
                &ctx.accounts.component_token_program.to_account_info(),
                &ctx.accounts.component_vault.to_account_info(),
                &ctx.accounts.component_mint.to_account_info(),
                &ctx.accounts.owner_component_token_account.to_account_info(),
                &ctx.accounts.vault_authority.to_account_info(),
                &[signer_seeds],
                owner_amount,
                component.decimals,
            )?;
            if protocol_amount > 0 {
                validate_recipient_component_ata(
                    &ctx.accounts.protocol_fee_component_account.to_account_info(),
                    &ctx.accounts.intent.protocol_fee_recipient,
                    &component.mint,
                )?;
                transfer_component_checked(
                    &ctx.accounts.component_token_program.to_account_info(),
                    &ctx.accounts.component_vault.to_account_info(),
                    &ctx.accounts.component_mint.to_account_info(),
                    &ctx.accounts.protocol_fee_component_account.to_account_info(),
                    &ctx.accounts.vault_authority.to_account_info(),
                    &[signer_seeds],
                    protocol_amount,
                    component.decimals,
                )?;
            }
            if creator_amount > 0 {
                validate_recipient_component_ata(
                    &ctx.accounts.creator_fee_component_account.to_account_info(),
                    &ctx.accounts.intent.creator_fee_recipient,
                    &component.mint,
                )?;
                transfer_component_checked(
                    &ctx.accounts.component_token_program.to_account_info(),
                    &ctx.accounts.component_vault.to_account_info(),
                    &ctx.accounts.component_mint.to_account_info(),
                    &ctx.accounts.creator_fee_component_account.to_account_info(),
                    &ctx.accounts.vault_authority.to_account_info(),
                    &[signer_seeds],
                    creator_amount,
                    component.decimals,
                )?;
            }
        }
        set_component_quote_atoms(&mut ctx.accounts.intent, args.component_index, 0)?;
        mark_component_filled(&mut ctx.accounts.intent, args.component_index)?;
        emit!(LargeBasketComponentFilled {
            intent: ctx.accounts.intent.key(),
            index: ctx.accounts.index.key(),
            owner: ctx.accounts.owner.key(),
            component_index: args.component_index,
            amount,
            quote_atoms: 0,
        });
        Ok(())
    }
}

// Returns (protocol_amount, creator_amount) of the in-kind fee for a component
// `amount`. The staking share is folded into the protocol amount because staking
// rewards must be a single token, not per-component component dust.
fn in_kind_fee_amounts(intent: &LargeBasketIntent, amount: u64) -> Result<(u64, u64)> {
    let split = large_basket_fee_split(
        amount,
        intent.protocol_fee_bps,
        intent.creator_fee_bps,
        intent.staking_fee_bps,
    )?;
    let (protocol_fee, creator_fee) = route_creator_fee(
        split.protocol_fee,
        split.creator_fee,
        &intent.creator_fee_recipient,
    )?;
    let protocol_amount = protocol_fee
        .checked_add(split.staking_fee)
        .ok_or_else(|| error!(BasketError::ArithmeticOverflow))?;
    Ok((protocol_amount, creator_fee))
}

fn validate_recipient_component_ata(
    ata_info: &AccountInfo<'_>,
    recipient: &Pubkey,
    component_mint: &Pubkey,
) -> Result<()> {
    let account = load_interface_token_account(ata_info)?;
    require_keys_eq!(
        account.owner,
        *recipient,
        BasketError::InvalidFeeRecipientTokenAccount
    );
    require_keys_eq!(
        account.mint,
        *component_mint,
        BasketError::InvalidFeeRecipientTokenAccount
    );
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn transfer_component_checked<'info>(
    token_program: &AccountInfo<'info>,
    from: &AccountInfo<'info>,
    mint: &AccountInfo<'info>,
    to: &AccountInfo<'info>,
    authority: &AccountInfo<'info>,
    signer_seeds: &[&[&[u8]]],
    amount: u64,
    decimals: u8,
) -> Result<()> {
    let accounts = TransferChecked {
        from: from.clone(),
        mint: mint.clone(),
        to: to.clone(),
        authority: authority.clone(),
    };
    if signer_seeds.is_empty() {
        token_interface::transfer_checked(
            CpiContext::new(token_program.clone(), accounts),
            amount,
            decimals,
        )
    } else {
        token_interface::transfer_checked(
            CpiContext::new_with_signer(token_program.clone(), accounts, signer_seeds),
            amount,
            decimals,
        )
    }
}

struct IntentInit {
    index: Pubkey,
    owner: Pubkey,
    nonce: u64,
    kind: LargeBasketIntentKind,
    index_amount: u64,
    supply_snapshot: u64,
    post_supply: u64,
    quote_mint: Pubkey,
    component_amounts: Vec<u64>,
    max_quote_in: u64,
    min_quote_out: u64,
    protocol_fee_bps: u16,
    creator_fee_bps: u16,
    staking_fee_bps: u16,
    protocol_fee_recipient: Pubkey,
    creator_fee_recipient: Pubkey,
    staking_pool: Pubkey,
    component_count: u8,
    expires_at: i64,
    in_kind: bool,
    supply_era: u32,
    bump: u8,
}

fn require_mint_basis<'info>(
    index: &IndexState,
    index_mint: &UncheckedAccount<'info>,
    intent: &LargeBasketIntent,
) -> Result<()> {
    let supply = load_mint(&index_mint.to_account_info())?.supply;
    require!(
        index.mint_basis_holds(intent.supply_era, intent.supply_snapshot == 0, supply),
        BasketError::MintBasisChanged
    );
    Ok(())
}

fn large_basket_intent_component_count(index: &IndexState) -> u8 {
    index.large_basket_component_count
}

fn initialize_or_validate_intent_lock(
    intent_lock: &mut Account<LargeBasketIntentLock>,
    index: Pubkey,
    owner: Pubkey,
    bump: u8,
) -> Result<()> {
    if intent_lock.index == Pubkey::default() {
        intent_lock.index = index;
        intent_lock.owner = owner;
        intent_lock.active_intent = Pubkey::default();
        intent_lock.bump = bump;
        intent_lock.reserved = [0; 31];
    }

    require_keys_eq!(
        intent_lock.index,
        index,
        BasketError::InvalidLargeBasketIntent
    );
    require_keys_eq!(
        intent_lock.owner,
        owner,
        BasketError::InvalidLargeBasketIntent
    );
    require_keys_eq!(
        intent_lock.active_intent,
        Pubkey::default(),
        BasketError::InvalidLargeBasketIntent
    );
    Ok(())
}

fn activate_intent_lock(
    intent_lock: &mut Account<LargeBasketIntentLock>,
    intent: Pubkey,
) -> Result<()> {
    require_keys_eq!(
        intent_lock.active_intent,
        Pubkey::default(),
        BasketError::InvalidLargeBasketIntent
    );
    intent_lock.active_intent = intent;
    Ok(())
}

fn clear_intent_lock(
    intent_lock: &mut Account<LargeBasketIntentLock>,
    intent: Pubkey,
) -> Result<()> {
    require_keys_eq!(
        intent_lock.active_intent,
        intent,
        BasketError::InvalidLargeBasketIntent
    );
    intent_lock.active_intent = Pubkey::default();
    Ok(())
}

fn initialize_intent(intent: &mut Account<LargeBasketIntent>, init: IntentInit) -> Result<()> {
    require!(
        init.component_count > 0
            && usize::from(init.component_count) <= MAX_LARGE_BASKET_COMPONENTS,
        BasketError::InvalidComponentCount
    );
    require!(
        init.component_amounts.len() == usize::from(init.component_count),
        BasketError::InvalidLargeBasketIntent
    );

    let now = Clock::get()?.unix_timestamp;
    require!(
        init.expires_at > now,
        BasketError::InvalidLargeBasketIntentExpiry
    );
    let max_expires_at = now
        .checked_add(MAX_LARGE_BASKET_INTENT_TTL_SECONDS)
        .ok_or_else(|| error!(BasketError::ArithmeticOverflow))?;
    require!(
        init.expires_at <= max_expires_at,
        BasketError::InvalidLargeBasketIntentExpiry
    );
    // Design B: fees are not known at open (they depend on the actual quote
    // executed across the swaps). Validate the fee RATES here and store them;
    // the realized fee atoms are computed at collect time, after all components
    // have executed and `quote_atoms_executed` is final.
    validate_total_index_fee_bps(
        init.protocol_fee_bps,
        init.creator_fee_bps,
        init.staking_fee_bps,
    )?;

    intent.index = init.index;
    intent.owner = init.owner;
    intent.nonce = init.nonce;
    intent.kind = init.kind;
    intent.status = LargeBasketIntentStatus::Open;
    intent.index_amount = init.index_amount;
    intent.supply_snapshot = init.supply_snapshot;
    intent.post_supply = init.post_supply;
    intent.quote_mint = init.quote_mint;
    intent.fee_basis_usdc_atoms = 0;
    let component_amounts_len = init.component_amounts.len();
    intent.component_amounts = init.component_amounts;
    intent.quote_atoms_executed = 0;
    intent.max_quote_in = init.max_quote_in;
    intent.min_quote_out = init.min_quote_out;
    intent.protocol_fee_usdc_atoms = 0;
    intent.creator_fee_usdc_atoms = 0;
    intent.staking_fee_usdc_atoms = 0;
    intent.protocol_fee_bps = init.protocol_fee_bps;
    intent.creator_fee_bps = init.creator_fee_bps;
    intent.staking_fee_bps = init.staking_fee_bps;
    intent.protocol_fee_recipient = init.protocol_fee_recipient;
    intent.creator_fee_recipient = init.creator_fee_recipient;
    intent.staking_pool = init.staking_pool;
    intent.opened_at = now;
    intent.expires_at = init.expires_at;
    intent.component_count = u16::from(init.component_count);
    intent.completed_components = 0;
    intent.component_fill_bitmap = [0; crate::constants::LARGE_BASKET_COMPONENT_BITMAP_BYTES];
    intent.fees_collected = false;
    intent.component_quote_atoms = vec![0u64; component_amounts_len];
    intent.component_verified_bitmap = [0; crate::constants::LARGE_BASKET_COMPONENT_BITMAP_BYTES];
    intent.in_kind = init.in_kind;
    intent.supply_era = init.supply_era;
    intent.refunded_bitmap = [0; crate::constants::LARGE_BASKET_COMPONENT_BITMAP_BYTES];
    intent.escrowed_bitmap = [0; crate::constants::LARGE_BASKET_COMPONENT_BITMAP_BYTES];
    intent.bump = init.bump;
    intent.reserved = [0; 6];

    emit!(LargeBasketIntentOpened {
        intent: intent.key(),
        index: intent.index,
        owner: intent.owner,
        nonce: intent.nonce,
        kind: intent.kind,
        index_amount: intent.index_amount,
        fee_basis_usdc_atoms: 0,
        protocol_fee_usdc_atoms: 0,
        creator_fee_usdc_atoms: 0,
        staking_fee_usdc_atoms: 0,
        expires_at: intent.expires_at,
    });

    Ok(())
}

// Design B: component amounts are pure pro-rata math (no oracle). The basket's
// USD value is NOT estimated up front anymore — fees are charged at finalize
// from the actual `quote_atoms_executed`. This removes the Switchboard quote
// from `open` entirely, so it has no per-quote feed-count limit and scales to
// any component count.
fn compute_intent_component_amounts<'info>(
    index_key: &Pubkey,
    index: &IndexState,
    page_infos: &'info [AccountInfo<'info>],
    index_amount: u64,
    current_supply: u64,
    kind: LargeBasketIntentKind,
) -> Result<Vec<u64>> {
    let pages = load_ordered_component_pages(index_key, index, page_infos)?;
    require!(index.kind != crate::state::IndexKind::FixedWeights ||
        pages.iter().any(|p| p.components.iter().any(|c| c.mint == USDC_MINT)),
        BasketError::InvalidFixedWeightConfig);
    let base_units = index.index_base_units()?;
    let component_count = usize::from(index.large_basket_component_count);
    let mut component_amounts = vec![0u64; component_count];

    for page in pages {
        for (local_index, component) in page.components.iter().enumerate() {
            let component_index = usize::from(page.start_component_index)
                .checked_add(local_index)
                .ok_or_else(|| error!(BasketError::ArithmeticOverflow))?;
            require!(
                component_index < component_count,
                BasketError::InvalidLargeBasketComponentPage
            );
            let amount = match kind {
                LargeBasketIntentKind::Mint => {
                    if current_supply == 0 {
                        quote_component_amount(component.units_per_index, index_amount, base_units)?
                    } else {
                        pro_rata_mint_amount(
                            index_amount,
                            component.accounted_reserve,
                            current_supply,
                        )?
                    }
                }
                LargeBasketIntentKind::Redeem => pro_rata_redeem_amount(
                    index_amount,
                    component.accounted_reserve,
                    current_supply,
                )?,
            };
            component_amounts[component_index] = amount;
        }
    }

    Ok(component_amounts)
}

fn load_ordered_component_pages<'info>(
    index_key: &Pubkey,
    index: &IndexState,
    page_infos: &'info [AccountInfo<'info>],
) -> Result<Vec<Account<'info, LargeBasketComponentPage>>> {
    require!(
        index.large_basket_configured,
        BasketError::LargeBasketNotConfigured
    );
    require!(
        page_infos.len() == usize::from(index.large_basket_page_count),
        BasketError::InvalidLargeBasketComponentPage
    );
    let mut pages = Vec::with_capacity(page_infos.len());
    for info in page_infos {
        let page = Account::<LargeBasketComponentPage>::try_from(info)?;
        validate_component_page_identity(index_key, &info.key(), &page)?;
        pages.push(page);
    }
    pages.sort_by_key(|page| page.page_index);
    validate_ordered_page_coverage(&pages, u16::from(index.large_basket_component_count))?;

    Ok(pages)
}

fn load_ordered_writable_component_pages<'info>(
    index: &Pubkey,
    page_infos: &'info [AccountInfo<'info>],
    component_count: u16,
) -> Result<Vec<Account<'info, LargeBasketComponentPage>>> {
    let mut pages = Vec::with_capacity(page_infos.len());
    for info in page_infos {
        require!(
            info.is_writable,
            BasketError::InvalidLargeBasketComponentPage
        );
        let page = Account::<LargeBasketComponentPage>::try_from(info)?;
        validate_component_page_identity(index, &info.key(), &page)?;
        pages.push(page);
    }
    pages.sort_by_key(|page| page.page_index);
    validate_ordered_page_coverage(&pages, component_count)?;
    Ok(pages)
}

fn validate_component_page_identity(
    index: &Pubkey,
    page_key: &Pubkey,
    page: &LargeBasketComponentPage,
) -> Result<()> {
    require_keys_eq!(
        page.index,
        *index,
        BasketError::InvalidLargeBasketComponentPage
    );
    require!(page.finalized, BasketError::InvalidLargeBasketComponentPage);
    require!(
        page.start_component_index == expected_page_start(page.page_index)?,
        BasketError::InvalidLargeBasketComponentPage
    );
    let expected_page = Pubkey::find_program_address(
        &[
            LARGE_BASKET_COMPONENT_PAGE_SEED,
            index.as_ref(),
            &[page.page_index],
        ],
        &crate::ID,
    )
    .0;
    require_keys_eq!(
        *page_key,
        expected_page,
        BasketError::InvalidLargeBasketComponentPage
    );
    Ok(())
}

fn validate_ordered_page_coverage(
    pages: &[Account<LargeBasketComponentPage>],
    component_count: u16,
) -> Result<()> {
    require!(
        pages.len() == usize::from(expected_page_count(component_count)?),
        BasketError::InvalidLargeBasketComponentPage
    );

    let mut expected_start = 0u16;
    let mut expected_page_index = 0u8;
    for page in pages {
        require!(
            page.page_index == expected_page_index,
            BasketError::InvalidLargeBasketComponentPage
        );
        require!(
            page.start_component_index == expected_start,
            BasketError::InvalidLargeBasketComponentPage
        );
        require!(
            page.component_count > 0
                && usize::from(page.component_count) <= MAX_LARGE_BASKET_COMPONENTS_PER_PAGE,
            BasketError::InvalidLargeBasketComponentPage
        );
        require!(
            page.components.len() == usize::from(page.component_count),
            BasketError::InvalidLargeBasketComponentPage
        );
        expected_start = expected_start
            .checked_add(page.component_count)
            .ok_or_else(|| error!(BasketError::ArithmeticOverflow))?;
        expected_page_index = expected_page_index
            .checked_add(1)
            .ok_or_else(|| error!(BasketError::ArithmeticOverflow))?;
    }
    require!(
        expected_start == component_count,
        BasketError::InvalidLargeBasketComponentPage
    );
    Ok(())
}

fn expected_page_start(page_index: u8) -> Result<u16> {
    let start = usize::from(page_index)
        .checked_mul(MAX_LARGE_BASKET_COMPONENTS_PER_PAGE)
        .ok_or_else(|| error!(BasketError::ArithmeticOverflow))?;
    u16::try_from(start).map_err(|_| error!(BasketError::ArithmeticOverflow))
}

fn expected_page_count(component_count: u16) -> Result<u8> {
    require!(component_count > 0, BasketError::InvalidComponentCount);
    let page_count = usize::from(component_count).div_ceil(MAX_LARGE_BASKET_COMPONENTS_PER_PAGE);
    u8::try_from(page_count).map_err(|_| error!(BasketError::ArithmeticOverflow))
}

fn reserve_redeem_components<'info>(
    index: &Pubkey,
    page_infos: &'info [AccountInfo<'info>],
    component_amounts: &[u64],
    program_id: &Pubkey,
) -> Result<()> {
    let component_count = u16::try_from(component_amounts.len())
        .map_err(|_| error!(BasketError::InvalidLargeBasketIntent))?;
    let mut pages = load_ordered_writable_component_pages(index, page_infos, component_count)?;
    for page in pages.iter_mut() {
        for local_index in 0..page.components.len() {
            let component_index = usize::from(page.start_component_index)
                .checked_add(local_index)
                .ok_or_else(|| error!(BasketError::ArithmeticOverflow))?;
            let amount = *component_amounts
                .get(component_index)
                .ok_or_else(|| error!(BasketError::InvalidLargeBasketIntent))?;
            page.components[local_index].accounted_reserve = page.components[local_index]
                .accounted_reserve
                .checked_sub(amount)
                .ok_or_else(|| error!(BasketError::InvalidLargeBasketIntent))?;
        }
    }
    for page in pages {
        page.exit(program_id)?;
    }
    Ok(())
}

fn account_filled_mint_components<'info>(
    index: &Pubkey,
    page_infos: &'info [AccountInfo<'info>],
    intent: &LargeBasketIntent,
    program_id: &Pubkey,
) -> Result<()> {
    let mut pages =
        load_ordered_writable_component_pages(index, page_infos, intent.component_count)?;

    let mut expected_start = 0u16;
    for page in pages.iter_mut() {
        require!(
            page.start_component_index == expected_start,
            BasketError::InvalidLargeBasketComponentPage
        );
        for local_index in 0..page.components.len() {
            let component_index = page
                .start_component_index
                .checked_add(local_index as u16)
                .ok_or_else(|| error!(BasketError::ArithmeticOverflow))?;
            require!(
                component_index < intent.component_count,
                BasketError::InvalidLargeBasketComponentPage
            );
            if !component_filled(intent, component_index)? {
                continue;
            }
            let amount = intent_component_amount(intent, component_index)?;
            page.components[local_index].accounted_reserve = page.components[local_index]
                .accounted_reserve
                .checked_add(amount)
                .ok_or_else(|| error!(BasketError::ArithmeticOverflow))?;
        }
        expected_start = expected_start
            .checked_add(page.component_count)
            .ok_or_else(|| error!(BasketError::ArithmeticOverflow))?;
    }
    require!(
        expected_start == intent.component_count,
        BasketError::InvalidLargeBasketComponentPage
    );

    for page in pages {
        page.exit(program_id)?;
    }
    Ok(())
}

fn restore_unfilled_redeem_components<'info>(
    index: &Pubkey,
    page_infos: &'info [AccountInfo<'info>],
    intent: &LargeBasketIntent,
    program_id: &Pubkey,
) -> Result<()> {
    let mut pages =
        load_ordered_writable_component_pages(index, page_infos, intent.component_count)?;

    let mut expected_start = 0u16;
    for page in pages.iter_mut() {
        require!(
            page.start_component_index == expected_start,
            BasketError::InvalidLargeBasketComponentPage
        );
        for local_index in 0..page.components.len() {
            let component_index = page
                .start_component_index
                .checked_add(local_index as u16)
                .ok_or_else(|| error!(BasketError::ArithmeticOverflow))?;
            require!(
                component_index < intent.component_count,
                BasketError::InvalidLargeBasketComponentPage
            );
            if component_filled(intent, component_index)? {
                continue;
            }
            let amount = intent_component_amount(intent, component_index)?;
            page.components[local_index].accounted_reserve = page.components[local_index]
                .accounted_reserve
                .checked_add(amount)
                .ok_or_else(|| error!(BasketError::ArithmeticOverflow))?;
        }
        expected_start = expected_start
            .checked_add(page.component_count)
            .ok_or_else(|| error!(BasketError::ArithmeticOverflow))?;
    }
    require!(
        expected_start == intent.component_count,
        BasketError::InvalidLargeBasketComponentPage
    );

    for page in pages {
        page.exit(program_id)?;
    }
    Ok(())
}

#[derive(Clone, Copy)]
enum CancelledComponentSelection {
    Filled,
    Unfilled,
}

fn component_owed(
    intent: &LargeBasketIntent,
    selection: CancelledComponentSelection,
    component_index: u16,
) -> Result<bool> {
    let filled = component_filled(intent, component_index)?;
    let selected = match selection {
        CancelledComponentSelection::Filled => filled,
        CancelledComponentSelection::Unfilled => !filled,
    };
    Ok(selected && intent_component_amount(intent, component_index)? > 0)
}

/// Returns the owed components named in `account_infos` (pages, then one or more
/// [mint, vault, destination, token program] groups, in any order) and reports whether every
/// owed component has now left the vaults. The destination is a token account the owner holds,
/// or the refund escrow's associated token account, which costs the caller no rent and which
/// the owner cannot make refuse a transfer.
fn return_owed_components<'info>(
    index: Pubkey,
    vault_authority_bump: u8,
    vault_authority: &AccountInfo<'info>,
    account_infos: &'info [AccountInfo<'info>],
    intent: &mut LargeBasketIntent,
    selection: CancelledComponentSelection,
) -> Result<bool> {
    let page_count = usize::from(expected_page_count(intent.component_count)?);
    require!(
        account_infos.len() >= page_count,
        BasketError::InvalidRemainingAccounts
    );
    let (page_infos, transfer_infos) = account_infos.split_at(page_count);
    // Every call must return something, so an expired but finalizable intent is not flipped
    // to Refunding by a no-op.
    require!(
        !transfer_infos.is_empty() && transfer_infos.len() % 4 == 0,
        BasketError::InvalidRemainingAccounts
    );
    let pages = load_ordered_writable_component_pages(&index, page_infos, intent.component_count)?;
    let escrow = Pubkey::find_program_address(&[REFUND_ESCROW_SEED, index.as_ref()], &crate::ID).0;
    let mut components: Vec<(u16, &LargeBasketComponent)> = Vec::new();
    for page in pages.iter() {
        for (local_index, component) in page.components.iter().enumerate() {
            let component_index = page
                .start_component_index
                .checked_add(local_index as u16)
                .ok_or_else(|| error!(BasketError::ArithmeticOverflow))?;
            components.push((component_index, component));
        }
    }

    for group in transfer_infos.chunks(4) {
        let (component_index, component) = *components
            .iter()
            .find(|(_, component)| component.mint == group[0].key())
            .ok_or_else(|| error!(BasketError::InvalidComponentMint))?;
        require!(
            component_owed(intent, selection, component_index)?
                && !bitmap_get(&intent.refunded_bitmap, component_index)?,
            BasketError::InvalidRemainingAccounts
        );
        let to_escrow = load_interface_token_account(&group[2])?.owner == escrow;
        if to_escrow {
            require_keys_eq!(
                group[2].key(),
                associated_token_address_with_token_program(&escrow, &component.mint, &component.token_program),
                BasketError::InvalidUserTokenAccount
            );
        }
        transfer_component_to_owner(
            CancelledComponentTransfer {
                index,
                vault_authority_bump,
                vault_authority,
                owner: if to_escrow { escrow } else { intent.owner },
                component,
                amount: intent_component_amount(intent, component_index)?,
            },
            CancelledComponentTransferAccounts {
                mint: &group[0],
                vault: &group[1],
                owner_token: &group[2],
                token_program: &group[3],
            },
        )?;
        bitmap_set_once(&mut intent.refunded_bitmap, component_index)?;
        if to_escrow {
            bitmap_set_once(&mut intent.escrowed_bitmap, component_index)?;
        }
    }

    for (component_index, _) in components {
        if component_owed(intent, selection, component_index)?
            && !bitmap_get(&intent.refunded_bitmap, component_index)?
        {
            return Ok(false);
        }
    }
    Ok(true)
}

struct CancelledComponentTransfer<'a, 'info> {
    index: Pubkey,
    vault_authority_bump: u8,
    vault_authority: &'a AccountInfo<'info>,
    owner: Pubkey,
    component: &'a LargeBasketComponent,
    amount: u64,
}

struct CancelledComponentTransferAccounts<'a, 'info> {
    mint: &'a AccountInfo<'info>,
    vault: &'a AccountInfo<'info>,
    owner_token: &'a AccountInfo<'info>,
    token_program: &'a AccountInfo<'info>,
}

fn transfer_component_to_owner<'info>(
    transfer: CancelledComponentTransfer<'_, 'info>,
    accounts: CancelledComponentTransferAccounts<'_, 'info>,
) -> Result<()> {
    require_keys_eq!(
        accounts.mint.key(),
        transfer.component.mint,
        BasketError::InvalidComponentMint
    );
    require_keys_eq!(
        *accounts.mint.owner,
        transfer.component.token_program,
        BasketError::InvalidTokenMint
    );
    require_keys_eq!(
        accounts.vault.key(),
        transfer.component.vault,
        BasketError::InvalidVaultAccount
    );
    require_keys_eq!(
        accounts.token_program.key(),
        transfer.component.token_program,
        BasketError::InvalidTokenProgram
    );

    let vault_token_account = load_interface_token_account(accounts.vault)?;
    require_keys_eq!(
        vault_token_account.owner,
        transfer.vault_authority.key(),
        BasketError::InvalidVaultAccount
    );
    require_keys_eq!(
        vault_token_account.mint,
        transfer.component.mint,
        BasketError::InvalidVaultAccount
    );
    let owner_token_account = load_interface_token_account(accounts.owner_token)?;
    require_keys_eq!(
        owner_token_account.owner,
        transfer.owner,
        BasketError::InvalidUserTokenAccount
    );
    require_keys_eq!(
        owner_token_account.mint,
        transfer.component.mint,
        BasketError::InvalidUserTokenAccount
    );

    let signer_seeds: &[&[u8]] = &[
        VAULT_AUTHORITY_SEED,
        transfer.index.as_ref(),
        &[transfer.vault_authority_bump],
    ];
    token_interface::transfer_checked(
        CpiContext::new_with_signer(
            accounts.token_program.clone(),
            TransferChecked {
                from: accounts.vault.clone(),
                mint: accounts.mint.clone(),
                to: accounts.owner_token.clone(),
                authority: transfer.vault_authority.clone(),
            },
            &[signer_seeds],
        ),
        transfer.amount,
        transfer.component.decimals,
    )
}

fn remint_unfilled_redeem_intent<'info>(
    index: &Account<'info, IndexState>,
    index_mint: &UncheckedAccount<'info>,
    vault_authority: &UncheckedAccount<'info>,
    intent: &LargeBasketIntent,
    owner_index_token_account: &UncheckedAccount<'info>,
    token_program: &Program<'info, Token>,
) -> Result<()> {
    let owner_index_account =
        load_user_token_account(&owner_index_token_account.to_account_info())?;
    validate_user_token_account(&owner_index_account, &intent.owner, &index_mint.key())?;
    let signer_seeds: &[&[u8]] = &[
        VAULT_AUTHORITY_SEED,
        index.to_account_info().key.as_ref(),
        &[index.vault_authority_bump],
    ];
    token::mint_to(
        CpiContext::new_with_signer(
            token_program.to_account_info(),
            MintTo {
                mint: index_mint.to_account_info(),
                to: owner_index_token_account.to_account_info(),
                authority: vault_authority.to_account_info(),
            },
            &[signer_seeds],
        ),
        intent.index_amount,
    )
}

fn validate_component_execution_args(max_oracle_slippage_bps: u16) -> Result<()> {
    require!(
        max_oracle_slippage_bps <= MAX_FIXED_WEIGHT_EXECUTION_SLIPPAGE_BPS,
        BasketError::InvalidOraclePriceTolerance
    );
    Ok(())
}

fn validate_open_intent(
    intent: &LargeBasketIntent,
    expected_kind: LargeBasketIntentKind,
    component_index: u16,
) -> Result<()> {
    require!(
        intent.status == LargeBasketIntentStatus::Open,
        BasketError::InvalidLargeBasketIntent
    );
    require!(
        intent.kind == expected_kind,
        BasketError::InvalidLargeBasketIntent
    );
    require!(
        component_index < intent.component_count,
        BasketError::InvalidLargeBasketIntent
    );
    require!(
        Clock::get()?.unix_timestamp <= intent.expires_at,
        BasketError::LargeBasketIntentExpired
    );
    Ok(())
}

fn validate_finalizable_intent(
    intent: &LargeBasketIntent,
    expected_kind: LargeBasketIntentKind,
) -> Result<()> {
    require!(
        intent.status == LargeBasketIntentStatus::Open,
        BasketError::InvalidLargeBasketIntent
    );
    require!(
        intent.kind == expected_kind,
        BasketError::InvalidLargeBasketIntent
    );
    require!(
        intent.completed_components == intent.component_count,
        BasketError::LargeBasketComponentNotFilled
    );
    Ok(())
}

fn bitmap_position(component_index: u16) -> Result<(usize, u8)> {
    require!(
        usize::from(component_index) < MAX_LARGE_BASKET_COMPONENTS,
        BasketError::InvalidLargeBasketIntent
    );
    let byte_index = usize::from(component_index) / 8;
    require!(
        byte_index < LARGE_BASKET_COMPONENT_BITMAP_BYTES,
        BasketError::InvalidLargeBasketIntent
    );
    let bit = 1u8 << (component_index % 8);
    Ok((byte_index, bit))
}

fn component_filled(intent: &LargeBasketIntent, component_index: u16) -> Result<bool> {
    let (byte_index, bit) = bitmap_position(component_index)?;
    Ok((intent.component_fill_bitmap[byte_index] & bit) != 0)
}

fn mark_component_filled(intent: &mut LargeBasketIntent, component_index: u16) -> Result<()> {
    if component_filled(intent, component_index)? {
        return err!(BasketError::LargeBasketComponentAlreadyFilled);
    }
    let (byte_index, bit) = bitmap_position(component_index)?;
    intent.component_fill_bitmap[byte_index] |= bit;
    intent.completed_components = intent
        .completed_components
        .checked_add(1)
        .ok_or_else(|| error!(BasketError::ArithmeticOverflow))?;
    Ok(())
}

fn intent_component_amount(intent: &LargeBasketIntent, component_index: u16) -> Result<u64> {
    intent
        .component_amounts
        .get(usize::from(component_index))
        .copied()
        .ok_or_else(|| error!(BasketError::InvalidLargeBasketIntent))
}

// Record the quote spent (mint) / received (redeem) for a component at execute time.
fn set_component_quote_atoms(
    intent: &mut LargeBasketIntent,
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

fn intent_component_quote_atoms(intent: &LargeBasketIntent, component_index: u16) -> Result<u64> {
    intent
        .component_quote_atoms
        .get(usize::from(component_index))
        .copied()
        .ok_or_else(|| error!(BasketError::InvalidLargeBasketIntent))
}

fn component_verified(intent: &LargeBasketIntent, component_index: u16) -> Result<bool> {
    let (byte_index, bit) = bitmap_position(component_index)?;
    Ok((intent.component_verified_bitmap[byte_index] & bit) != 0)
}

fn mark_component_verified(intent: &mut LargeBasketIntent, component_index: u16) -> Result<()> {
    if component_verified(intent, component_index)? {
        return err!(BasketError::LargeBasketComponentAlreadyVerified);
    }
    let (byte_index, bit) = bitmap_position(component_index)?;
    intent.component_verified_bitmap[byte_index] |= bit;
    Ok(())
}

// True only if every filled component has also been price-verified. No longer a
// finalize gate (per-component verify was dropped), but retained for the optional
// verify_*_component_price path and tests.
#[allow(dead_code)]
fn all_components_verified(intent: &LargeBasketIntent) -> bool {
    intent.component_fill_bitmap == intent.component_verified_bitmap
}

fn validate_component_accounts(
    index: &Pubkey,
    page: &LargeBasketComponentPage,
    page_key: &Pubkey,
    component_index: u16,
    mint_info: &AccountInfo<'_>,
    vault_info: &AccountInfo<'_>,
    token_program_info: &AccountInfo<'_>,
) -> Result<LargeBasketComponent> {
    validate_component_page_identity(index, page_key, page)?;
    let local_index = page.component_offset(component_index)?;
    let component = page.components[local_index].clone();
    require_keys_eq!(
        mint_info.key(),
        component.mint,
        BasketError::InvalidComponentMint
    );
    require_keys_eq!(
        *mint_info.owner,
        component.token_program,
        BasketError::InvalidTokenMint
    );
    require_keys_eq!(
        token_program_info.key(),
        component.token_program,
        BasketError::InvalidTokenProgram
    );
    require_keys_eq!(
        vault_info.key(),
        component.vault,
        BasketError::InvalidVaultAccount
    );
    Ok(component)
}

fn add_mint_quote_spent(intent: &mut LargeBasketIntent, quote_spent: u64) -> Result<()> {
    intent.quote_atoms_executed = intent
        .quote_atoms_executed
        .checked_add(quote_spent)
        .ok_or_else(|| error!(BasketError::ArithmeticOverflow))?;
    // Design B: fee atoms aren't set until collect; estimate the pending fee
    // from the stored bps so the running budget still bounds quote_in + fees.
    let total_quote_with_fees = intent
        .quote_atoms_executed
        .checked_add(pending_total_fees(intent, intent.quote_atoms_executed)?)
        .ok_or_else(|| error!(BasketError::ArithmeticOverflow))?;
    require!(
        total_quote_with_fees <= intent.max_quote_in,
        BasketError::QuoteBudgetExceeded
    );
    Ok(())
}

fn add_redeem_quote_received(intent: &mut LargeBasketIntent, quote_received: u64) -> Result<()> {
    let quote_atoms_executed = intent
        .quote_atoms_executed
        .checked_add(quote_received)
        .ok_or_else(|| error!(BasketError::ArithmeticOverflow))?;
    let completed_components = intent
        .completed_components
        .checked_add(1)
        .ok_or_else(|| error!(BasketError::ArithmeticOverflow))?;
    if completed_components == intent.component_count {
        let net_quote = quote_atoms_executed
            .checked_sub(pending_total_fees(intent, quote_atoms_executed)?)
            .ok_or_else(|| error!(BasketError::QuoteBudgetExceeded))?;
        require!(
            net_quote >= intent.min_quote_out,
            BasketError::QuoteBudgetExceeded
        );
    }
    intent.quote_atoms_executed = quote_atoms_executed;
    Ok(())
}

fn intent_total_fees(intent: &LargeBasketIntent) -> Result<u64> {
    intent
        .protocol_fee_usdc_atoms
        .checked_add(intent.creator_fee_usdc_atoms)
        .and_then(|value| value.checked_add(intent.staking_fee_usdc_atoms))
        .ok_or_else(|| error!(BasketError::ArithmeticOverflow))
}

// Fee atoms implied by the intent's stored fee bps over a given quote basis.
// Used to bound the budget during execution, before fees are realized at collect.
fn pending_total_fees(intent: &LargeBasketIntent, fee_basis: u64) -> Result<u64> {
    let split = large_basket_fee_split(
        fee_basis,
        intent.protocol_fee_bps,
        intent.creator_fee_bps,
        intent.staking_fee_bps,
    )?;
    split
        .protocol_fee
        .checked_add(split.creator_fee)
        .and_then(|value| value.checked_add(split.staking_fee))
        .ok_or_else(|| error!(BasketError::ArithmeticOverflow))
}

fn transfer_checked_from_user_quote<'info>(
    ctx: &Context<'_, '_, 'info, 'info, ExecuteLargeBasketMintComponent<'info>>,
    destination: AccountInfo<'info>,
    amount: u64,
) -> Result<()> {
    transfer_user_quote_to(
        &ctx.accounts.quote_token_program.to_account_info(),
        &ctx.accounts.owner_quote_token_account.to_account_info(),
        &ctx.accounts.quote_mint.to_account_info(),
        &ctx.accounts.owner.to_account_info(),
        destination,
        amount,
    )
}

// Account-parameterized USDC transfer (owner -> destination), reused by single and
// batched execute for the USDC-component path.
fn transfer_user_quote_to<'info>(
    quote_token_program: &AccountInfo<'info>,
    owner_quote_token_account: &AccountInfo<'info>,
    quote_mint: &AccountInfo<'info>,
    owner: &AccountInfo<'info>,
    destination: AccountInfo<'info>,
    amount: u64,
) -> Result<()> {
    token_interface::transfer_checked(
        CpiContext::new(
            quote_token_program.clone(),
            TransferChecked {
                from: owner_quote_token_account.clone(),
                mint: quote_mint.clone(),
                to: destination,
                authority: owner.clone(),
            },
        ),
        amount,
        crate::constants::USDC_DECIMALS,
    )
}

fn mint_account_candidates<'info>(
    ctx: &Context<'_, '_, 'info, 'info, ExecuteLargeBasketMintComponent<'info>>,
) -> Vec<AccountInfo<'info>> {
    let mut candidates = vec![
        ctx.accounts.owner.to_account_info(),
        ctx.accounts.index.to_account_info(),
        ctx.accounts.index_mint.to_account_info(),
        ctx.accounts.intent.to_account_info(),
        ctx.accounts.vault_authority.to_account_info(),
        ctx.accounts.quote_mint.to_account_info(),
        ctx.accounts.owner_quote_token_account.to_account_info(),
        ctx.accounts.jupiter_program.to_account_info(),
        ctx.accounts.component_page.to_account_info(),
        ctx.accounts.component_mint.to_account_info(),
        ctx.accounts.component_vault.to_account_info(),
        ctx.accounts.component_token_program.to_account_info(),
        ctx.accounts.associated_token_program.to_account_info(),
        ctx.accounts.quote_token_program.to_account_info(),
        ctx.accounts.system_program.to_account_info(),
    ];
    candidates.extend_from_slice(ctx.remaining_accounts);
    candidates
}

// Performs the component swap and returns quote_spent. The oracle price-bound is
// NOT checked here anymore — it is deferred to verify_mint_component_price so the
// swap tx doesn't also have to carry the (incompressible) oracle quote, which would
// exceed the 1232-byte transaction limit. quote_spent/component_received are facts
// fixed by this swap; the deferred check compares them to a fresh oracle later.
fn execute_mint_swap<'info>(
    ctx: &mut Context<'_, '_, 'info, 'info, ExecuteLargeBasketMintComponent<'info>>,
    swap: &LargeBasketSwapPlan,
    _component: &LargeBasketComponent,
    required_amount: u64,
) -> Result<u64> {
    let candidates = mint_account_candidates(ctx);
    execute_mint_swap_inner(
        &ctx.accounts.jupiter_program.to_account_info(),
        &ctx.accounts.owner.key(),
        &ctx.accounts.owner_quote_token_account.to_account_info(),
        &ctx.accounts.component_vault.to_account_info(),
        &candidates,
        swap,
        required_amount,
    )
}

// Account-parameterized swap core, reused by both the single-component execute and
// the batched execute. `candidates` is the set of accounts the Jupiter route is
// allowed to reference (fixed/shared accounts + THIS component's accounts + THIS
// entry's route accounts) — scoping is enforced per-call so a batched route for one
// component can never reach another component's vault or any protocol account.
// quote_spent/component_received are measured fresh from chain state before/after
// the CPI; the oracle price-bound is deferred to verify_*_component_price.
#[allow(clippy::too_many_arguments)]
fn execute_mint_swap_inner<'info>(
    jupiter_program: &AccountInfo<'info>,
    owner: &Pubkey,
    owner_quote_token_account: &AccountInfo<'info>,
    component_vault: &AccountInfo<'info>,
    candidates: &[AccountInfo<'info>],
    swap: &LargeBasketSwapPlan,
    required_amount: u64,
) -> Result<u64> {
    // The compact swap plan carries no input/output mint or source/dest pubkeys
    // (they're derivable and were redundant) and packs each route-account reference
    // into 1 byte (see unpack_account_metas) so more swaps fit per batched tx.
    // Safety is enforced by the route-account scope check below (the route may only
    // write component_vault) + the before/after balance checks (USDC left the owner,
    // >= required component landed in the vault).
    let account_metas = unpack_account_metas(&swap.accounts);
    validate_jupiter_route_account_scope(
        candidates,
        &account_metas,
        &[component_vault.key()],
        &[component_vault.key()],
    )?;
    let mut scratch = JupiterInvokeScratch::new();
    let quote_before = load_interface_token_account(owner_quote_token_account)?.amount;
    let component_before = load_interface_token_account(component_vault)?;
    invoke_jupiter_swap_with_scratch(
        jupiter_program.clone(),
        candidates,
        &account_metas,
        &swap.instruction_data,
        Some(*owner),
        &[],
        &mut scratch,
    )?;
    let quote_after = load_interface_token_account(owner_quote_token_account)?.amount;
    let component_after = load_interface_token_account(component_vault)?;
    let quote_spent = quote_before
        .checked_sub(quote_after)
        .ok_or_else(|| error!(BasketError::InvalidJupiterRoute))?;
    require!(quote_spent > 0, BasketError::InvalidJupiterRoute);
    let component_received = component_after
        .amount
        .checked_sub(component_before.amount)
        .ok_or_else(|| error!(BasketError::InvalidJupiterRoute))?;
    // Jupiter offers ExactIn only for xStocks (no ExactOut route), so the swap
    // delivers a VARIABLE amount that is >= required (the route sizes the swap so
    // Jupiter's guaranteed min-out >= required). Accept over-delivery: the vault is
    // backed by at least the pro-rata target; any surplus accrues pro-rata to all
    // holders, and the minter's spend stays bounded by max_quote_in. Requiring exact
    // equality is impossible with ExactIn and was the cause of InvalidJupiterRoute.
    require!(
        component_received >= required_amount,
        BasketError::InvalidJupiterRoute
    );
    Ok(quote_spent)
}

// Redeem swap core (component -> USDC) for the batched redeem. Vault authority signs via
// PDA seeds. Scoping enforced per-call. Kept out of line: inlined into the batch handler its
// locals push that frame past the SBF 4 KB stack limit.
#[inline(never)]
#[allow(clippy::too_many_arguments)]
fn execute_redeem_swap_inner<'info>(
    jupiter_program: &AccountInfo<'info>,
    index_key: &Pubkey,
    vault_authority: &AccountInfo<'info>,
    vault_authority_bump: u8,
    owner_quote_token_account: &AccountInfo<'info>,
    component_vault: &AccountInfo<'info>,
    vault_quote_token_account: &Pubkey,
    candidates: &[AccountInfo<'info>],
    swap: &LargeBasketSwapPlan,
    required_amount: u64,
) -> Result<u64> {
    // Compact swap plan (no redundant pubkeys; route accounts packed 1 byte each —
    // see unpack_account_metas); scope + balance checks enforce safety.
    let account_metas = unpack_account_metas(&swap.accounts);
    validate_jupiter_route_account_scope(
        candidates,
        &account_metas,
        &[component_vault.key(), owner_quote_token_account.key()],
        &[component_vault.key(), owner_quote_token_account.key()],
    )?;
    // The redeem route routes proceeds through the vault authority's quote (USDC) ATA as
    // an intermediate (Jupiter taker = vault authority, so its own quote ATA is the swap
    // output buffer before the transfer to owner_quote_token_account). That account is
    // owned by the vault authority, so it must be explicitly allowed here — it is NOT a
    // component vault (those hold component mints, never the quote mint), and the
    // before/after balance + min_quote_out checks below bound the proceeds, so a
    // malicious route cannot divert value through it.
    validate_vault_authority_token_account_scope(
        candidates,
        &account_metas,
        *vault_authority.key,
        &[component_vault.key(), *vault_quote_token_account],
    )?;
    let mut scratch = JupiterInvokeScratch::new();
    let signer_seeds: &[&[u8]] = &[
        VAULT_AUTHORITY_SEED,
        index_key.as_ref(),
        std::slice::from_ref(&vault_authority_bump),
    ];
    let quote_before = load_interface_token_account(owner_quote_token_account)?.amount;
    let component_before = load_interface_token_account(component_vault)?;
    // The vault's USDC account is only a pass-through for proceeds here, but it is also the
    // USDC component's vault, so a route must not leave it poorer.
    let vault_quote = candidates
        .iter()
        .find(|info| info.key() == *vault_quote_token_account && info.key() != component_vault.key());
    let vault_quote_before = vault_quote
        .map(|info| load_interface_token_account(info).map(|account| account.amount))
        .transpose()?;
    invoke_jupiter_swap_with_scratch(
        jupiter_program.clone(),
        candidates,
        &account_metas,
        &swap.instruction_data,
        Some(*vault_authority.key),
        &[signer_seeds],
        &mut scratch,
    )?;
    let quote_after = load_interface_token_account(owner_quote_token_account)?.amount;
    let component_after = load_interface_token_account(component_vault)?;
    if let (Some(info), Some(before)) = (vault_quote, vault_quote_before) {
        require!(
            load_interface_token_account(info)?.amount >= before,
            BasketError::InvalidJupiterRoute
        );
    }
    let component_spent = component_before
        .amount
        .checked_sub(component_after.amount)
        .ok_or_else(|| error!(BasketError::InvalidJupiterRoute))?;
    let quote_received = quote_after
        .checked_sub(quote_before)
        .ok_or_else(|| error!(BasketError::InvalidJupiterRoute))?;
    require!(
        component_spent == required_amount,
        BasketError::InvalidJupiterRoute
    );
    Ok(quote_received)
}

fn create_owner_index_ata_if_needed<'info>(
    ctx: &Context<'_, '_, 'info, 'info, FinalizeLargeBasketMintIntent<'info>>,
) -> Result<()> {
    let expected_ata =
        associated_token_address(&ctx.accounts.owner.key(), &ctx.accounts.index_mint.key());
    require_keys_eq!(
        ctx.accounts.owner_index_token_account.key(),
        expected_ata,
        BasketError::InvalidUserTokenAccount
    );

    create_associated_token_account_idempotent(
        ctx.accounts.associated_token_program.to_account_info(),
        ctx.accounts.owner.to_account_info(),
        ctx.accounts.owner_index_token_account.to_account_info(),
        ctx.accounts.owner.to_account_info(),
        ctx.accounts.index_mint.to_account_info(),
        ctx.accounts.system_program.to_account_info(),
        ctx.accounts.token_program.to_account_info(),
    )
}

fn validate_fee_destination(
    account: &TokenAccount,
    expected_owner: &Pubkey,
    amount: u64,
    error: BasketError,
) -> Result<()> {
    if amount == 0 {
        return Ok(());
    }

    require_keys_eq!(account.owner, *expected_owner, error);
    require_keys_eq!(account.mint, USDC_MINT, error);
    Ok(())
}

fn transfer_fee_from_owner<'info>(
    ctx: &Context<'_, '_, '_, 'info, CollectLargeBasketIntentFees<'info>>,
    destination: AccountInfo<'info>,
    amount: u64,
) -> Result<()> {
    if amount == 0 || destination.key() == ctx.accounts.owner_quote_token_account.key() {
        return Ok(());
    }

    token::transfer(
        CpiContext::new(
            ctx.accounts.token_program.to_account_info(),
            Transfer {
                from: ctx.accounts.owner_quote_token_account.to_account_info(),
                to: destination,
                authority: ctx.accounts.owner.to_account_info(),
            },
        ),
        amount,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::constants::REBALANCE_REQUEST_WINDOW_SECONDS;

    fn intent(component_count: u16) -> LargeBasketIntent {
        LargeBasketIntent {
            index: Pubkey::new_unique(),
            owner: Pubkey::new_unique(),
            nonce: 1,
            kind: LargeBasketIntentKind::Mint,
            status: LargeBasketIntentStatus::Open,
            index_amount: 1,
            supply_snapshot: 0,
            post_supply: 1,
            quote_mint: USDC_MINT,
            fee_basis_usdc_atoms: 1,
            protocol_fee_usdc_atoms: 0,
            creator_fee_usdc_atoms: 0,
            staking_fee_usdc_atoms: 0,
            protocol_fee_recipient: Pubkey::new_unique(),
            creator_fee_recipient: Pubkey::default(),
            staking_pool: Pubkey::new_unique(),
            opened_at: 0,
            expires_at: i64::MAX,
            component_count,
            completed_components: 0,
            component_fill_bitmap: [0; LARGE_BASKET_COMPONENT_BITMAP_BYTES],
            component_amounts: vec![0; usize::from(component_count)],
            quote_atoms_executed: 0,
            max_quote_in: 0,
            min_quote_out: 0,
            fees_collected: false,
            protocol_fee_bps: 0,
            creator_fee_bps: 0,
            staking_fee_bps: 0,
            component_quote_atoms: vec![0; usize::from(component_count)],
            component_verified_bitmap: [0; LARGE_BASKET_COMPONENT_BITMAP_BYTES],
            in_kind: false,
            supply_era: 0,
            refunded_bitmap: [0; LARGE_BASKET_COMPONENT_BITMAP_BYTES],
            escrowed_bitmap: [0; LARGE_BASKET_COMPONENT_BITMAP_BYTES],
            bump: 255,
            reserved: [0; 6],
        }
    }

    fn index_with_component_counts(large_basket_component_count: u8) -> IndexState {
        IndexState {
            authority: Pubkey::new_unique(),
            creator: Pubkey::default(),
            fee_recipient: Pubkey::new_unique(),
            creator_fee_recipient: Pubkey::default(),
            index_mint: Pubkey::new_unique(),
            vault_authority_bump: 255,
            index_bump: 254,
            index_mint_bump: 253,
            decimals: 6,
            kind: crate::state::IndexKind::FixedUnits,
            large_basket_component_count,
            large_basket_page_count: 1,
            page_generation: 0,
            large_basket_configured: true,
            large_basket_operation_in_progress: false,
            mint_fee_bps: 0,
            redeem_fee_bps: 0,
            creator_mint_fee_bps: 0,
            creator_redeem_fee_bps: 0,
            staking_mint_fee_bps: 0,
            staking_redeem_fee_bps: 0,
            max_supply: 0,
            rebalance_delay_seconds: 0,
            fixed_weight_rebalance_interval_seconds: 0,
            fixed_weight_last_rebalanced_at: 0,
            rebalance_requested_at: 0,
            open_intent_count: 0,
            supply_era: 0,
            rebalance_keeper: Pubkey::default(),
            fixed_weight_quote_mint: Pubkey::default(),
            active_rebalance_intent: Pubkey::default(),
            pending_rebalance_oracle_price_tolerance_bps: 0,
            pending_rebalance_nav_tolerance_bps: 0,
            fixed_weight_drift_threshold_bps: 0,
            fixed_weight_spot_ema_max_deviation_bps: 0,
            minting_paused: false,
            redeeming_paused: false,
            rebalancing_paused: false,
            rebalance_requested: false,
            reserved: [0; 1],
            name: "Large".to_string(),
            symbol: "LRG".to_string(),
            metadata_uri: String::new(),
        }
    }

    #[test]
    fn in_kind_fee_folds_staking_into_protocol_and_splits_creator() {
        let mut i = intent(1);
        i.protocol_fee_bps = 100; // 1.0%
        i.creator_fee_bps = 50; // 0.5%
        i.staking_fee_bps = 50; // 0.5%
        i.creator_fee_recipient = Pubkey::new_unique();
        let (protocol_amount, creator_amount) = in_kind_fee_amounts(&i, 1_000_000).unwrap();
        // protocol (1.0%) + staking (0.5%) folded together, creator (0.5%) kept apart.
        assert_eq!(protocol_amount, 15_000);
        assert_eq!(creator_amount, 5_000);
    }

    #[test]
    fn in_kind_fee_routes_creator_to_protocol_when_recipient_unset() {
        let mut i = intent(1);
        i.protocol_fee_bps = 100; // 1.0%
        i.creator_fee_bps = 50; // 0.5%
        i.staking_fee_bps = 0;
        i.creator_fee_recipient = Pubkey::default();
        let (protocol_amount, creator_amount) = in_kind_fee_amounts(&i, 1_000_000).unwrap();
        assert_eq!(protocol_amount, 15_000);
        assert_eq!(creator_amount, 0);
    }

    #[test]
    fn component_bitmap_tracks_fills_once() {
        let mut intent = intent(50);

        assert!(!component_filled(&intent, 49).unwrap());
        mark_component_filled(&mut intent, 49).unwrap();
        assert!(component_filled(&intent, 49).unwrap());
        assert_eq!(intent.completed_components, 1);
        assert!(mark_component_filled(&mut intent, 49).is_err());
    }

    #[test]
    fn verified_bitmap_marks_once() {
        let mut intent = intent(50);
        assert!(!component_verified(&intent, 7).unwrap());
        mark_component_verified(&mut intent, 7).unwrap();
        assert!(component_verified(&intent, 7).unwrap());
        // double-verify rejected
        assert!(mark_component_verified(&mut intent, 7).is_err());
    }

    #[test]
    fn all_verified_requires_every_filled_component() {
        let mut intent = intent(3);
        // fill all 3, verify only 2 -> not all verified
        for i in 0..3 {
            mark_component_filled(&mut intent, i).unwrap();
        }
        mark_component_verified(&mut intent, 0).unwrap();
        mark_component_verified(&mut intent, 1).unwrap();
        assert!(!all_components_verified(&intent), "2 of 3 verified must not pass");
        mark_component_verified(&mut intent, 2).unwrap();
        assert!(all_components_verified(&intent), "all filled+verified must pass");
    }

    #[test]
    fn set_and_read_component_quote_atoms() {
        let mut intent = intent(3);
        set_component_quote_atoms(&mut intent, 1, 12_345).unwrap();
        assert_eq!(intent_component_quote_atoms(&intent, 1).unwrap(), 12_345);
        assert_eq!(intent_component_quote_atoms(&intent, 0).unwrap(), 0);
        // out of range
        assert!(set_component_quote_atoms(&mut intent, 9, 1).is_err());
    }

    #[test]
    fn mint_quote_budget_includes_all_fee_buckets() {
        // Design B: fees are derived from the stored bps over the running quote
        // total, not pre-set atoms. 500+250+250 = 1000 bps = 10% total fee.
        // Budget 1_100 covers quote 1_000 (+100 fee) exactly; one more atom of
        // quote pushes quote+fee over budget.
        let mut intent = intent(1);
        intent.max_quote_in = 1_100;
        intent.protocol_fee_bps = 500;
        intent.creator_fee_bps = 250;
        intent.staking_fee_bps = 250;

        assert!(add_mint_quote_spent(&mut intent, 1_000).is_ok());
        assert!(add_mint_quote_spent(&mut intent, 1).is_err());
    }

    #[test]
    fn redeem_last_fill_requires_aggregate_min_quote() {
        let mut intent = intent(2);
        intent.kind = LargeBasketIntentKind::Redeem;
        intent.completed_components = 1;
        intent.min_quote_out = 100;

        assert!(add_redeem_quote_received(&mut intent, 99).is_err());
        assert_eq!(intent.quote_atoms_executed, 0);
    }

    #[test]
    fn redeem_non_last_fill_does_not_require_aggregate_min_quote() {
        let mut intent = intent(3);
        intent.kind = LargeBasketIntentKind::Redeem;
        intent.completed_components = 1;
        intent.min_quote_out = 1_000;

        assert!(add_redeem_quote_received(&mut intent, 10).is_ok());
        assert_eq!(intent.quote_atoms_executed, 10);
    }

    #[test]
    fn redeem_last_fill_accepts_net_quote_after_fees() {
        let mut intent = intent(2);
        intent.kind = LargeBasketIntentKind::Redeem;
        intent.completed_components = 1;
        intent.min_quote_out = 90;
        intent.protocol_fee_usdc_atoms = 5;
        intent.creator_fee_usdc_atoms = 3;
        intent.staking_fee_usdc_atoms = 2;

        assert!(add_redeem_quote_received(&mut intent, 100).is_ok());
        assert_eq!(intent.quote_atoms_executed, 100);
    }

    #[test]
    fn large_basket_intent_uses_large_component_count() {
        let index = index_with_component_counts(9);

        assert_eq!(large_basket_intent_component_count(&index), 9);
    }

    #[test]
    fn ratio_priced_mint_settles_while_supply_moves_within_its_era() {
        let mut index = index_with_component_counts(1);
        index.supply_era = 3;
        // Other owners' intents changed supply from the open snapshot; still the same era.
        assert!(index.settle_mint_era(3, false, 7).is_ok());
        assert!(index.settle_mint_era(3, false, 1).is_ok());
        assert_eq!(index.supply_era, 3);
    }

    #[test]
    fn ratio_priced_mint_rejects_an_emptied_or_restarted_basket() {
        let mut index = index_with_component_counts(1);
        index.supply_era = 3;
        assert!(index.settle_mint_era(3, false, 0).is_err());
        index.supply_era = 4;
        assert!(index.settle_mint_era(3, false, 9).is_err());
    }

    #[test]
    fn units_priced_mints_share_the_restart_they_open_into() {
        let mut index = index_with_component_counts(1);
        index.supply_era = 3;
        // First settler into the empty basket starts era 4.
        assert!(index.settle_mint_era(3, true, 0).is_ok());
        assert_eq!(index.supply_era, 4);
        // A second mint opened into the same empty basket joins era 4.
        assert!(index.settle_mint_era(3, true, 5).is_ok());
        assert_eq!(index.supply_era, 4);
    }

    #[test]
    fn units_priced_mint_rejects_a_ratio_composition() {
        let mut index = index_with_component_counts(1);
        index.supply_era = 3;
        // Supply came back (e.g. a cancelled redeem re-minted) without a new era: the
        // composition is the old ratio-priced one, not units_per_index.
        assert!(index.settle_mint_era(3, true, 5).is_err());
        // Or the basket restarted twice since this intent opened.
        index.supply_era = 5;
        assert!(index.settle_mint_era(3, true, 5).is_err());
    }

    #[test]
    fn intent_counter_gates_new_intents_and_never_blocks_settling() {
        let mut index = index_with_component_counts(1);
        index.track_opened_intent().unwrap();
        index.track_opened_intent().unwrap();
        assert_eq!(index.open_intent_count, 2);
        // Open intents from other owners never stop new ones.
        assert!(index.accepts_new_intents(100));
        index.track_closed_intent();
        index.track_closed_intent();
        index.track_closed_intent();
        assert_eq!(index.open_intent_count, 0);
    }

    #[test]
    fn rebalance_request_holds_new_intents_until_it_lapses() {
        let mut index = index_with_component_counts(1);
        index.rebalance_requested = true;
        index.rebalance_requested_at = 1_000;
        assert!(!index.accepts_new_intents(1_000));
        assert!(!index.accepts_new_intents(1_000 + REBALANCE_REQUEST_WINDOW_SECONDS - 1));
        assert!(index.accepts_new_intents(1_000 + REBALANCE_REQUEST_WINDOW_SECONDS));
        index.rebalance_requested = false;
        index.large_basket_operation_in_progress = true;
        assert!(!index.accepts_new_intents(1_000));
    }

    #[test]
    fn only_authority_or_set_keeper_operates_rebalances() {
        let mut index = index_with_component_counts(1);
        let keeper = Pubkey::new_unique();
        assert!(index.is_rebalance_operator(&index.authority.clone()));
        assert!(!index.is_rebalance_operator(&keeper));
        assert!(!index.is_rebalance_operator(&Pubkey::default()));
        index.rebalance_keeper = keeper;
        assert!(index.is_rebalance_operator(&keeper));
        assert!(!index.is_rebalance_operator(&Pubkey::new_unique()));
    }

    // Mirrors charge_redeem_leg_fees without accounts: returns what each party was charged.
    fn charge_redeem_legs(intent: &mut LargeBasketIntent, legs: &[u64]) -> (u64, u64, u64) {
        let mut paid = (0, 0, 0);
        for leg in legs {
            intent.quote_atoms_executed += leg;
            let (protocol, creator, staking) =
                redeem_fee_targets(intent, intent.quote_atoms_executed).unwrap();
            paid.0 += protocol - intent.protocol_fee_usdc_atoms;
            paid.1 += creator - intent.creator_fee_usdc_atoms;
            paid.2 += staking - intent.staking_fee_usdc_atoms;
            intent.protocol_fee_usdc_atoms = protocol;
            intent.creator_fee_usdc_atoms = creator;
            intent.staking_fee_usdc_atoms = staking;
        }
        paid
    }

    #[test]
    fn redeem_leg_fees_sum_to_the_fee_on_total_proceeds() {
        let legs = [1, 999, 123_456_789, 7, 50_000_000_000, 3];
        let total: u64 = legs.iter().sum();
        for (bps, creator) in [
            ((5, 0, 5), Pubkey::default()),
            ((3, 2, 5), Pubkey::new_unique()),
            ((3, 2, 5), Pubkey::default()),
        ] {
            let mut redeem = intent(legs.len() as u16);
            redeem.kind = LargeBasketIntentKind::Redeem;
            (redeem.protocol_fee_bps, redeem.creator_fee_bps, redeem.staking_fee_bps) = bps;
            redeem.creator_fee_recipient = creator;
            let paid = charge_redeem_legs(&mut redeem, &legs);
            assert_eq!(paid, redeem_fee_targets(&redeem, total).unwrap());
            // The last leg's min-out check reserves exactly what the legs charge.
            assert_eq!(paid.0 + paid.1 + paid.2, pending_total_fees(&redeem, total).unwrap());
        }
    }

    #[test]
    fn redeem_creator_share_goes_to_protocol_without_a_creator() {
        let mut redeem = intent(1);
        (redeem.protocol_fee_bps, redeem.creator_fee_bps, redeem.staking_fee_bps) = (3, 2, 5);
        assert_eq!(redeem_fee_targets(&redeem, 1_000_000).unwrap(), (500, 0, 500));
        redeem.creator_fee_recipient = Pubkey::new_unique();
        assert_eq!(redeem_fee_targets(&redeem, 1_000_000).unwrap(), (300, 200, 500));
    }
}
