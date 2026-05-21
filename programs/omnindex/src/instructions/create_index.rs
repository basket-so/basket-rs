use anchor_lang::{prelude::*, system_program};
use anchor_spl::token::{self, Mint, Token};

use crate::{
    constants::{
        INDEX_MINT_SEED, INDEX_SEED, MAX_COMPONENTS, MAX_INDEX_DECIMALS, MAX_METADATA_URI_LEN,
        MAX_NAME_LEN, MAX_REBALANCE_DELAY_SECONDS, MAX_SYMBOL_LEN, PROTOCOL_CONFIG_SEED,
        VAULT_AUTHORITY_SEED,
    },
    errors::OmnindexError,
    events::IndexCreated,
    state::{IndexComponentInput, IndexKind, IndexState, ProtocolConfig},
    utils::{
        validate_component_inputs, validate_index_strategy_config, validate_no_self_component,
    },
};

#[derive(AnchorSerialize, AnchorDeserialize, Clone, Debug)]
pub struct CreateIndexArgs {
    pub name: String,
    pub symbol: String,
    pub metadata_uri: String,
    pub decimals: u8,
    pub fee_recipient: Pubkey,
    pub creator_fee_recipient: Pubkey,
    pub max_supply: u64,
    pub rebalance_delay_seconds: i64,
    pub kind: IndexKind,
    pub fixed_weight_quote_mint: Pubkey,
    pub fixed_weight_rebalance_interval_seconds: i64,
    pub fixed_weight_drift_threshold_bps: u16,
    pub fixed_weight_spot_ema_max_deviation_bps: u16,
    pub components: Vec<IndexComponentInput>,
}

#[derive(Accounts)]
#[instruction(args: CreateIndexArgs)]
pub struct CreateIndex<'info> {
    #[account(mut)]
    pub payer: Signer<'info>,
    pub authority: Signer<'info>,
    #[account(
        seeds = [PROTOCOL_CONFIG_SEED],
        bump = protocol_config.bump
    )]
    pub protocol_config: Account<'info, ProtocolConfig>,
    #[account(
        init,
        payer = payer,
        seeds = [INDEX_SEED, authority.key().as_ref(), args.symbol.as_bytes()],
        bump,
        space = 8
            + IndexState::space(
                MAX_NAME_LEN,
                MAX_SYMBOL_LEN,
                MAX_METADATA_URI_LEN,
                MAX_COMPONENTS,
            ),
    )]
    pub index: Account<'info, IndexState>,
    #[account(
        seeds = [INDEX_MINT_SEED, index.key().as_ref()],
        bump
    )]
    /// CHECK: Created and initialized as a classic SPL Token mint in the handler.
    #[account(mut)]
    pub index_mint: UncheckedAccount<'info>,
    /// CHECK: PDA authority over the mint and component vaults.
    #[account(
        seeds = [VAULT_AUTHORITY_SEED, index.key().as_ref()],
        bump
    )]
    pub vault_authority: UncheckedAccount<'info>,
    pub token_program: Program<'info, Token>,
    pub system_program: Program<'info, System>,
}

impl<'info> CreateIndex<'info> {
    pub fn handle(ctx: Context<'_, '_, '_, 'info, Self>, args: CreateIndexArgs) -> Result<()> {
        require!(
            ctx.accounts
                .protocol_config
                .can_create_index(&ctx.accounts.authority.key()),
            OmnindexError::UnauthorizedIndexCreator
        );
        require!(!args.name.is_empty(), OmnindexError::EmptyName);
        require!(!args.symbol.is_empty(), OmnindexError::EmptySymbol);
        require!(args.name.len() <= MAX_NAME_LEN, OmnindexError::NameTooLong);
        require!(
            args.symbol.len() <= MAX_SYMBOL_LEN,
            OmnindexError::SymbolTooLong
        );
        require!(
            args.metadata_uri.len() <= MAX_METADATA_URI_LEN,
            OmnindexError::MetadataUriTooLong
        );
        require!(
            !args.components.is_empty(),
            OmnindexError::InvalidComponentCount
        );
        require!(
            args.components.len() <= MAX_COMPONENTS,
            OmnindexError::TooManyComponents
        );
        require!(
            args.decimals <= MAX_INDEX_DECIMALS,
            OmnindexError::UnsupportedIndexDecimals
        );
        require!(
            args.rebalance_delay_seconds >= 0
                && args.rebalance_delay_seconds <= MAX_REBALANCE_DELAY_SECONDS,
            OmnindexError::InvalidRebalanceDelay
        );

        create_index_mint(&ctx, args.decimals)?;

        let fee_recipient = if args.fee_recipient == Pubkey::default() {
            ctx.accounts.authority.key()
        } else {
            args.fee_recipient
        };

        let components = validate_component_inputs(args.components)?;
        validate_no_self_component(&components, &ctx.accounts.index_mint.key())?;
        validate_index_strategy_config(
            args.kind,
            &components,
            args.fixed_weight_quote_mint,
            args.fixed_weight_rebalance_interval_seconds,
            args.fixed_weight_drift_threshold_bps,
            args.fixed_weight_spot_ema_max_deviation_bps,
        )?;
        let now = Clock::get()?.unix_timestamp;

        let index = &mut ctx.accounts.index;
        index.authority = ctx.accounts.authority.key();
        index.creator = if args.creator_fee_recipient == Pubkey::default() {
            Pubkey::default()
        } else {
            ctx.accounts.authority.key()
        };
        index.fee_recipient = fee_recipient;
        index.creator_fee_recipient = args.creator_fee_recipient;
        index.index_mint = ctx.accounts.index_mint.key();
        index.vault_authority_bump = ctx.bumps.vault_authority;
        index.index_bump = ctx.bumps.index;
        index.index_mint_bump = ctx.bumps.index_mint;
        index.decimals = args.decimals;
        index.kind = args.kind;
        index.component_count = components.len() as u8;
        index.pending_component_count = 0;
        index.mint_fee_bps = 0;
        index.redeem_fee_bps = 0;
        index.creator_mint_fee_bps = 0;
        index.creator_redeem_fee_bps = 0;
        index.max_supply = args.max_supply;
        index.rebalance_delay_seconds = args.rebalance_delay_seconds;
        index.fixed_weight_rebalance_interval_seconds =
            args.fixed_weight_rebalance_interval_seconds;
        index.fixed_weight_last_rebalanced_at = if args.kind == IndexKind::FixedWeights {
            now
        } else {
            0
        };
        index.pending_rebalance_available_at = 0;
        index.pending_rebalance_nonce = 0;
        index.pending_rebalance_quote_mint = Pubkey::default();
        index.fixed_weight_quote_mint = args.fixed_weight_quote_mint;
        index.pending_rebalance_oracle_price_tolerance_bps = 0;
        index.pending_rebalance_nav_tolerance_bps = 0;
        index.fixed_weight_drift_threshold_bps = args.fixed_weight_drift_threshold_bps;
        index.fixed_weight_spot_ema_max_deviation_bps =
            args.fixed_weight_spot_ema_max_deviation_bps;
        index.minting_paused = false;
        index.redeeming_paused = false;
        index.rebalancing_paused = false;
        index.reserved = [0; 1];
        index.name = args.name;
        index.symbol = args.symbol;
        index.metadata_uri = args.metadata_uri;
        index.components = components;
        index.pending_components = Vec::new();

        emit!(IndexCreated {
            index: index.key(),
            index_mint: index.index_mint,
            authority: index.authority,
            kind: index.kind,
            components: index.component_count,
        });

        Ok(())
    }
}

fn create_index_mint<'info>(
    ctx: &Context<'_, '_, '_, 'info, CreateIndex<'info>>,
    decimals: u8,
) -> Result<()> {
    let index_key = ctx.accounts.index.key();
    let signer_seeds: &[&[u8]] = &[INDEX_MINT_SEED, index_key.as_ref(), &[ctx.bumps.index_mint]];
    let lamports = Rent::get()?.minimum_balance(Mint::LEN);

    system_program::create_account(
        CpiContext::new_with_signer(
            ctx.accounts.system_program.to_account_info(),
            system_program::CreateAccount {
                from: ctx.accounts.payer.to_account_info(),
                to: ctx.accounts.index_mint.to_account_info(),
            },
            &[signer_seeds],
        ),
        lamports,
        Mint::LEN as u64,
        &ctx.accounts.token_program.key(),
    )?;

    token::initialize_mint2(
        CpiContext::new(
            ctx.accounts.token_program.to_account_info(),
            token::InitializeMint2 {
                mint: ctx.accounts.index_mint.to_account_info(),
            },
        ),
        decimals,
        &ctx.accounts.vault_authority.key(),
        Some(&ctx.accounts.vault_authority.key()),
    )
}
