use std::collections::BTreeSet;

use anchor_lang::{prelude::*, system_program};
use anchor_spl::token::{self, Mint, Token};

use crate::{
    constants::{
        COMMUNITY_CONFIG_SEED, INDEX_MINT_SEED, INDEX_SEED, LARGE_BASKET_COMPONENT_PAGE_SEED,
        MAX_BASKET_COMPONENTS, MAX_LARGE_BASKET_COMPONENTS_PER_PAGE, MAX_LARGE_BASKET_PAGES,
        MAX_METADATA_URI_LEN, MAX_NAME_LEN, MAX_REBALANCE_DELAY_SECONDS, MAX_SYMBOL_LEN,
        PROTOCOL_CONFIG_SEED, SYMBOL_CLAIM_SEED, USDC_MINT, VAULT_AUTHORITY_SEED,
    },
    errors::BasketError,
    events::{
        CommunityBasketCreated, IndexCreated, IndexSymbolClaimed,
        LargeBasketComponentPageInitialized, LargeBasketConfigFinalized,
    },
    state::{
        CommunityConfig, IndexComponent, IndexComponentInput, IndexFees, IndexKind, IndexState,
        LargeBasketComponent, LargeBasketComponentPage, ProtocolConfig, symbol_claim_key, SymbolClaim,
    },
    utils::{
        associated_token_address_with_token_program,
        create_associated_token_account_idempotent_for_token_program, load_interface_mint,
        load_mint, require_community_component_mint, require_no_transfer_fee,
        validate_index_strategy_config, validate_no_self_component,
    },
};

#[derive(AnchorSerialize, AnchorDeserialize, Clone, Debug)]
pub struct CreateLargeBasketIndexArgs {
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
    pub component_count: u8,
    /// The symbol as its claim is seeded: symbol_claim_key(symbol).
    pub symbol_claim_key: String,
}

/// A community basket: anyone allowed by the protocol config can create one, for the
/// community config's creation fee. The creator picks each side's total fee, a whole number
/// of the community config's fee steps; each step splits between the protocol, the creator
/// and stakers as the config says. The fee recipient and keeper come from the config too.
#[derive(AnchorSerialize, AnchorDeserialize, Clone, Debug)]
pub struct CreateCommunityBasketIndexArgs {
    pub name: String,
    pub symbol: String,
    pub metadata_uri: String,
    pub decimals: u8,
    pub creator_fee_recipient: Pubkey,
    /// Total fee on each side, a whole number of fee steps (at least one).
    pub mint_fee_bps: u16,
    pub redeem_fee_bps: u16,
    pub max_supply: u64,
    pub kind: IndexKind,
    pub fixed_weight_rebalance_interval_seconds: i64,
    pub fixed_weight_drift_threshold_bps: u16,
    pub component_count: u8,
    /// The symbol as its claim is seeded: symbol_claim_key(symbol).
    pub symbol_claim_key: String,
}

#[derive(AnchorSerialize, AnchorDeserialize, Clone, Debug)]
pub struct InitializeLargeBasketComponentPageArgs {
    pub page_index: u8,
    pub start_component_index: u16,
    pub components: Vec<IndexComponentInput>,
}

#[derive(Accounts)]
#[instruction(args: CreateLargeBasketIndexArgs)]
pub struct CreateLargeBasketIndex<'info> {
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
            ),
    )]
    pub index: Account<'info, IndexState>,
    #[account(
        init,
        payer = payer,
        space = 8 + SymbolClaim::SPACE,
        seeds = [SYMBOL_CLAIM_SEED, args.symbol_claim_key.as_bytes()],
        bump
    )]
    pub symbol_claim: Account<'info, SymbolClaim>,
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

#[derive(Accounts)]
#[instruction(args: CreateCommunityBasketIndexArgs)]
pub struct CreateCommunityBasketIndex<'info> {
    /// Pays for the accounts and the creation fee.
    #[account(mut)]
    pub payer: Signer<'info>,
    pub authority: Signer<'info>,
    #[account(
        seeds = [PROTOCOL_CONFIG_SEED],
        bump = protocol_config.bump
    )]
    pub protocol_config: Account<'info, ProtocolConfig>,
    #[account(
        seeds = [COMMUNITY_CONFIG_SEED],
        bump = community_config.bump
    )]
    pub community_config: Account<'info, CommunityConfig>,
    /// CHECK: Receives the creation fee; must be the community config's treasury.
    #[account(mut, address = community_config.treasury @ BasketError::InvalidCommunityConfig)]
    pub treasury: UncheckedAccount<'info>,
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
            ),
    )]
    pub index: Account<'info, IndexState>,
    #[account(
        init,
        payer = payer,
        space = 8 + SymbolClaim::SPACE,
        seeds = [SYMBOL_CLAIM_SEED, args.symbol_claim_key.as_bytes()],
        bump
    )]
    pub symbol_claim: Account<'info, SymbolClaim>,
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

#[derive(Accounts)]
#[instruction(args: InitializeLargeBasketComponentPageArgs)]
pub struct InitializeLargeBasketComponentPage<'info> {
    #[account(mut)]
    pub payer: Signer<'info>,
    pub authority: Signer<'info>,
    #[account(
        has_one = authority @ BasketError::UnauthorizedAuthority,
        has_one = index_mint @ BasketError::IndexMintMismatch
    )]
    pub index: Account<'info, IndexState>,
    /// CHECK: Validated as the configured classic SPL Token mint.
    pub index_mint: UncheckedAccount<'info>,
    /// CHECK: PDA authority over component vaults.
    #[account(
        seeds = [VAULT_AUTHORITY_SEED, index.key().as_ref()],
        bump = index.vault_authority_bump
    )]
    pub vault_authority: UncheckedAccount<'info>,
    #[account(
        init,
        payer = payer,
        seeds = [
            LARGE_BASKET_COMPONENT_PAGE_SEED,
            index.key().as_ref(),
            &[args.page_index],
        ],
        bump,
        space = 8 + LargeBasketComponentPage::SPACE
    )]
    pub page: Account<'info, LargeBasketComponentPage>,
    /// CHECK: Validated as the Associated Token Program.
    #[account(address = crate::utils::ASSOCIATED_TOKEN_ID @ BasketError::InvalidAssociatedTokenProgram)]
    pub associated_token_program: UncheckedAccount<'info>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct FinalizeLargeBasketConfig<'info> {
    pub authority: Signer<'info>,
    #[account(
        mut,
        has_one = authority @ BasketError::UnauthorizedAuthority
    )]
    pub index: Account<'info, IndexState>,
}

impl<'info> CreateLargeBasketIndex<'info> {
    /// A curated basket, made by the protocol's index creator. Everyone else makes
    /// community baskets (create_community_basket_index).
    pub fn handle(
        ctx: Context<'_, '_, '_, 'info, Self>,
        args: CreateLargeBasketIndexArgs,
    ) -> Result<()> {
        require_keys_eq!(
            ctx.accounts.authority.key(),
            ctx.accounts.protocol_config.index_creator,
            BasketError::NotCuratedCreator
        );
        validate_large_index_args(&args)?;
        let index_key = ctx.accounts.index.key();
        create_index_mint(
            &ctx.accounts.payer,
            &ctx.accounts.index_mint,
            &ctx.accounts.vault_authority.key(),
            &ctx.accounts.token_program,
            &ctx.accounts.system_program,
            &index_key,
            ctx.bumps.index_mint,
            args.decimals,
        )?;

        let authority = ctx.accounts.authority.key();
        write_new_index(
            &mut ctx.accounts.index,
            NewIndex {
                authority,
                creator: if args.creator_fee_recipient == Pubkey::default() {
                    Pubkey::default()
                } else {
                    authority
                },
                fee_recipient: if args.fee_recipient == Pubkey::default() {
                    authority
                } else {
                    args.fee_recipient
                },
                creator_fee_recipient: args.creator_fee_recipient,
                index_mint: ctx.accounts.index_mint.key(),
                vault_authority_bump: ctx.bumps.vault_authority,
                index_bump: ctx.bumps.index,
                index_mint_bump: ctx.bumps.index_mint,
                decimals: args.decimals,
                kind: args.kind,
                component_count: args.component_count,
                fees: IndexFees::default(),
                max_supply: args.max_supply,
                rebalance_delay_seconds: args.rebalance_delay_seconds,
                fixed_weight_quote_mint: args.fixed_weight_quote_mint,
                fixed_weight_rebalance_interval_seconds: args
                    .fixed_weight_rebalance_interval_seconds,
                fixed_weight_drift_threshold_bps: args.fixed_weight_drift_threshold_bps,
                fixed_weight_spot_ema_max_deviation_bps: args
                    .fixed_weight_spot_ema_max_deviation_bps,
                rebalance_keeper: Pubkey::default(),
                community: false,
                name: args.name,
                symbol: args.symbol,
                metadata_uri: args.metadata_uri,
            },
        )?;
        claim_symbol(&mut ctx.accounts.symbol_claim, index_key, ctx.bumps.symbol_claim, &ctx.accounts.index.symbol);
        Ok(())
    }
}

impl<'info> CreateCommunityBasketIndex<'info> {
    pub fn handle(
        ctx: Context<'_, '_, '_, 'info, Self>,
        args: CreateCommunityBasketIndexArgs,
    ) -> Result<()> {
        require!(
            ctx.accounts
                .protocol_config
                .can_create_index(&ctx.accounts.authority.key()),
            BasketError::UnauthorizedIndexCreator
        );
        validate_symbol_claim_key(&args.symbol, &args.symbol_claim_key)?;
        validate_index_basics(
            &args.name,
            &args.symbol,
            &args.metadata_uri,
            args.component_count,
            args.decimals,
        )?;
        let fixed_weights = args.kind == IndexKind::FixedWeights;
        require!(
            fixed_weights
                || (args.fixed_weight_rebalance_interval_seconds == 0
                    && args.fixed_weight_drift_threshold_bps == 0),
            BasketError::InvalidFixedWeightConfig
        );

        let config = &ctx.accounts.community_config;
        let (mint_protocol, mint_creator, mint_staking) = config
            .split(args.mint_fee_bps)
            .ok_or_else(|| error!(BasketError::CommunityFeeSplit))?;
        let (redeem_protocol, redeem_creator, redeem_staking) = config
            .split(args.redeem_fee_bps)
            .ok_or_else(|| error!(BasketError::CommunityFeeSplit))?;
        let fees = IndexFees {
            mint_fee_bps: mint_protocol,
            redeem_fee_bps: redeem_protocol,
            creator_mint_fee_bps: mint_creator,
            creator_redeem_fee_bps: redeem_creator,
            staking_mint_fee_bps: mint_staking,
            staking_redeem_fee_bps: redeem_staking,
        };
        fees.validate()?;
        // A creator fee needs somewhere to go; without a recipient it would fall to the treasury.
        require!(
            args.creator_fee_recipient != Pubkey::default() || config.creator_step_bps == 0,
            BasketError::InvalidAuthority
        );
        let creation_fee_lamports = config.creation_fee_lamports;
        let treasury = config.treasury;
        let keeper = config.keeper;

        if creation_fee_lamports > 0 {
            system_program::transfer(
                CpiContext::new(
                    ctx.accounts.system_program.to_account_info(),
                    system_program::Transfer {
                        from: ctx.accounts.payer.to_account_info(),
                        to: ctx.accounts.treasury.to_account_info(),
                    },
                ),
                creation_fee_lamports,
            )?;
        }

        let index_key = ctx.accounts.index.key();
        create_index_mint(
            &ctx.accounts.payer,
            &ctx.accounts.index_mint,
            &ctx.accounts.vault_authority.key(),
            &ctx.accounts.token_program,
            &ctx.accounts.system_program,
            &index_key,
            ctx.bumps.index_mint,
            args.decimals,
        )?;

        let authority = ctx.accounts.authority.key();
        write_new_index(
            &mut ctx.accounts.index,
            NewIndex {
                authority,
                creator: authority,
                fee_recipient: treasury,
                creator_fee_recipient: args.creator_fee_recipient,
                index_mint: ctx.accounts.index_mint.key(),
                vault_authority_bump: ctx.bumps.vault_authority,
                index_bump: ctx.bumps.index,
                index_mint_bump: ctx.bumps.index_mint,
                decimals: args.decimals,
                kind: args.kind,
                component_count: args.component_count,
                fees,
                max_supply: args.max_supply,
                rebalance_delay_seconds: 0,
                fixed_weight_quote_mint: if fixed_weights { USDC_MINT } else { Pubkey::default() },
                fixed_weight_rebalance_interval_seconds: args
                    .fixed_weight_rebalance_interval_seconds,
                fixed_weight_drift_threshold_bps: args.fixed_weight_drift_threshold_bps,
                fixed_weight_spot_ema_max_deviation_bps: 0,
                rebalance_keeper: keeper,
                community: true,
                name: args.name,
                symbol: args.symbol,
                metadata_uri: args.metadata_uri,
            },
        )?;
        claim_symbol(&mut ctx.accounts.symbol_claim, index_key, ctx.bumps.symbol_claim, &ctx.accounts.index.symbol);

        emit!(CommunityBasketCreated {
            index: index_key,
            creator: authority,
            creation_fee_lamports,
            fees,
        });
        Ok(())
    }
}

/// Everything a new basket's state is set from; the rest starts at zero.
struct NewIndex {
    authority: Pubkey,
    creator: Pubkey,
    fee_recipient: Pubkey,
    creator_fee_recipient: Pubkey,
    index_mint: Pubkey,
    vault_authority_bump: u8,
    index_bump: u8,
    index_mint_bump: u8,
    decimals: u8,
    kind: IndexKind,
    component_count: u8,
    fees: IndexFees,
    max_supply: u64,
    rebalance_delay_seconds: i64,
    fixed_weight_quote_mint: Pubkey,
    fixed_weight_rebalance_interval_seconds: i64,
    fixed_weight_drift_threshold_bps: u16,
    fixed_weight_spot_ema_max_deviation_bps: u16,
    rebalance_keeper: Pubkey,
    community: bool,
    name: String,
    symbol: String,
    metadata_uri: String,
}

fn write_new_index(index: &mut Account<IndexState>, new: NewIndex) -> Result<()> {
    let now = Clock::get()?.unix_timestamp;
    index.authority = new.authority;
    index.creator = new.creator;
    index.fee_recipient = new.fee_recipient;
    index.creator_fee_recipient = new.creator_fee_recipient;
    index.index_mint = new.index_mint;
    index.vault_authority_bump = new.vault_authority_bump;
    index.index_bump = new.index_bump;
    index.index_mint_bump = new.index_mint_bump;
    index.decimals = new.decimals;
    index.kind = new.kind;
    index.large_basket_component_count = new.component_count;
    index.large_basket_page_count = 0;
    index.page_generation = 0;
    index.large_basket_configured = false;
    index.large_basket_operation_in_progress = false;
    index.set_fees(&new.fees);
    index.max_supply = new.max_supply;
    index.rebalance_delay_seconds = new.rebalance_delay_seconds;
    index.fixed_weight_rebalance_interval_seconds = new.fixed_weight_rebalance_interval_seconds;
    index.fixed_weight_last_rebalanced_at = if new.kind == IndexKind::FixedWeights {
        now
    } else {
        0
    };
    index.rebalance_requested_at = 0;
    index.open_intent_count = 0;
    index.supply_era = 0;
    index.rebalance_keeper = new.rebalance_keeper;
    index.fixed_weight_quote_mint = new.fixed_weight_quote_mint;
    index.active_rebalance_intent = Pubkey::default();
    index.community = new.community;
    index.community_reserved = 0;
    index.pending_rebalance_nav_tolerance_bps = 0;
    index.fixed_weight_drift_threshold_bps = new.fixed_weight_drift_threshold_bps;
    index.fixed_weight_spot_ema_max_deviation_bps = new.fixed_weight_spot_ema_max_deviation_bps;
    index.minting_paused = false;
    index.retired_redeem_pause = false;
    index.rebalancing_paused = false;
    index.rebalance_requested = false;
    index.composition_rebalance_due = false;
    index.name = new.name;
    index.symbol = new.symbol;
    index.metadata_uri = new.metadata_uri;

    emit!(IndexCreated {
        index: index.key(),
        index_mint: index.index_mint,
        authority: index.authority,
        kind: index.kind,
        components: index.large_basket_component_count,
    });
    Ok(())
}

fn claim_symbol(claim: &mut Account<SymbolClaim>, index: Pubkey, bump: u8, symbol: &str) {
    claim.index = index;
    claim.bump = bump;
    emit!(IndexSymbolClaimed {
        index,
        symbol: symbol.to_string(),
    });
}

impl<'info> InitializeLargeBasketComponentPage<'info> {
    pub fn handle(
        ctx: Context<'_, '_, 'info, 'info, Self>,
        args: InitializeLargeBasketComponentPageArgs,
    ) -> Result<()> {
        require!(
            ctx.accounts.index.large_basket_component_count > 0,
            BasketError::InvalidLargeBasketComponentPage
        );
        require!(
            !ctx.accounts.index.large_basket_configured,
            BasketError::InvalidLargeBasketComponentPage
        );
        require!(
            args.page_index < MAX_LARGE_BASKET_PAGES as u8,
            BasketError::InvalidLargeBasketComponentPage
        );
        require!(
            args.start_component_index == expected_page_start(args.page_index)?,
            BasketError::InvalidLargeBasketComponentPage
        );
        require!(
            !args.components.is_empty()
                && args.components.len() <= MAX_LARGE_BASKET_COMPONENTS_PER_PAGE,
            BasketError::InvalidComponentCount
        );
        let end = args
            .start_component_index
            .checked_add(args.components.len() as u16)
            .ok_or_else(|| error!(BasketError::ArithmeticOverflow))?;
        require!(
            end <= u16::from(ctx.accounts.index.large_basket_component_count),
            BasketError::InvalidLargeBasketComponentPage
        );

        let index_mint = load_mint(&ctx.accounts.index_mint.to_account_info())?;
        require!(index_mint.supply == 0, BasketError::InvalidIndexAmount);

        let mut seen = BTreeSet::new();
        let mut components = Vec::with_capacity(args.components.len());
        let mut remaining = ctx.remaining_accounts.iter();
        for component in args.components {
            require!(
                component.units_per_index > 0 || (ctx.accounts.index.kind == IndexKind::FixedWeights
                    && component.mint == USDC_MINT && component.target_weight_bps == 0),
                BasketError::ZeroComponentUnits
            );
            require!(
                seen.insert(component.mint),
                BasketError::DuplicateComponentMint
            );
            require_keys_neq!(
                component.mint,
                ctx.accounts.index_mint.key(),
                BasketError::InvalidComponentMint
            );

            let mint_info = next_account_info(&mut remaining)?;
            let vault_info = next_account_info(&mut remaining)?;
            let token_program_info = next_account_info(&mut remaining)?;
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
                ctx.accounts.payer.to_account_info(),
                vault_info.clone(),
                ctx.accounts.vault_authority.to_account_info(),
                mint_info.clone(),
                ctx.accounts.system_program.to_account_info(),
                token_program_info.clone(),
            )?;
            let mint = load_interface_mint(mint_info)?;
            require_no_transfer_fee(mint_info)?;
            if ctx.accounts.index.community {
                require_community_component_mint(mint_info)?;
            }
            components.push(LargeBasketComponent {
                mint: component.mint,
                units_per_index: component.units_per_index,
                target_weight_bps: component.target_weight_bps,
                oracle_pair: component.oracle_pair,
                token_program: token_program_info.key(),
                vault: vault_info.key(),
                accounted_reserve: 0,
                decimals: mint.decimals,
            });
        }
        require!(
            remaining.next().is_none(),
            BasketError::InvalidRemainingAccounts
        );

        let page = &mut ctx.accounts.page;
        page.index = ctx.accounts.index.key();
        page.page_index = args.page_index;
        page.start_component_index = args.start_component_index;
        page.component_count = components.len() as u16;
        page.bump = ctx.bumps.page;
        page.finalized = false;
        page.reserved = [0; 32];
        page.components = components;

        emit!(LargeBasketComponentPageInitialized {
            index: page.index,
            page: page.key(),
            page_index: page.page_index,
            start_component_index: page.start_component_index,
            component_count: page.component_count,
        });

        Ok(())
    }
}

impl<'info> FinalizeLargeBasketConfig<'info> {
    pub fn handle(ctx: Context<'_, '_, 'info, 'info, Self>) -> Result<()> {
        require!(
            ctx.accounts.index.large_basket_component_count > 0,
            BasketError::InvalidLargeBasketComponentPage
        );
        require!(
            !ctx.accounts.index.large_basket_configured,
            BasketError::InvalidLargeBasketComponentPage
        );
        let mut pages = Vec::with_capacity(ctx.remaining_accounts.len());
        for info in ctx.remaining_accounts {
            require!(
                info.is_writable,
                BasketError::InvalidLargeBasketComponentPage
            );
            let page = Account::<LargeBasketComponentPage>::try_from(info)?;
            require_keys_eq!(
                page.index,
                ctx.accounts.index.key(),
                BasketError::InvalidLargeBasketComponentPage
            );
            require!(
                !page.finalized,
                BasketError::InvalidLargeBasketComponentPage
            );
            pages.push(page);
        }
        pages.sort_by_key(|page| page.page_index);

        let mut expected_start = 0u16;
        let mut expected_page_index = 0u8;
        let mut mints = BTreeSet::new();
        let mut components = Vec::new();
        for page in pages.iter_mut() {
            require!(
                page.page_index == expected_page_index,
                BasketError::InvalidLargeBasketComponentPage
            );
            require!(
                page.start_component_index == expected_start,
                BasketError::InvalidLargeBasketComponentPage
            );
            require!(
                page.start_component_index == expected_page_start(page.page_index)?,
                BasketError::InvalidLargeBasketComponentPage
            );
            for component in &page.components {
                require!(
                    mints.insert(component.mint),
                    BasketError::DuplicateComponentMint
                );
                components.push(IndexComponent {
                    mint: component.mint,
                    units_per_index: component.units_per_index,
                    target_weight_bps: component.target_weight_bps,
                    oracle_pair: component.oracle_pair,
                });
            }
            expected_start = expected_start
                .checked_add(page.component_count)
                .ok_or_else(|| error!(BasketError::ArithmeticOverflow))?;
            expected_page_index = expected_page_index
                .checked_add(1)
                .ok_or_else(|| error!(BasketError::ArithmeticOverflow))?;
        }
        require!(
            expected_start == u16::from(ctx.accounts.index.large_basket_component_count),
            BasketError::InvalidLargeBasketComponentPage
        );
        require!(ctx.accounts.index.kind != IndexKind::FixedWeights || mints.contains(&USDC_MINT),
            BasketError::InvalidFixedWeightConfig);
        validate_no_self_component(&components, &ctx.accounts.index.index_mint)?;
        validate_index_strategy_config(
            ctx.accounts.index.kind,
            &components,
            ctx.accounts.index.fixed_weight_quote_mint,
            ctx.accounts.index.fixed_weight_rebalance_interval_seconds,
            ctx.accounts.index.fixed_weight_drift_threshold_bps,
            ctx.accounts.index.fixed_weight_spot_ema_max_deviation_bps,
        )?;

        let index = &mut ctx.accounts.index;
        index.large_basket_page_count = pages.len() as u8;
        index.large_basket_configured = true;

        for mut page in pages {
            page.finalized = true;
            page.exit(ctx.program_id)?;
        }

        emit!(LargeBasketConfigFinalized {
            index: index.key(),
            component_count: index.large_basket_component_count,
            page_count: index.large_basket_page_count,
        });

        Ok(())
    }
}

fn validate_large_index_args(args: &CreateLargeBasketIndexArgs) -> Result<()> {
    validate_symbol_claim_key(&args.symbol, &args.symbol_claim_key)?;
    validate_index_basics(
        &args.name,
        &args.symbol,
        &args.metadata_uri,
        args.component_count,
        args.decimals,
    )?;
    require!(
        args.rebalance_delay_seconds >= 0
            && args.rebalance_delay_seconds <= MAX_REBALANCE_DELAY_SECONDS,
        BasketError::InvalidRebalanceDelay
    );
    match args.kind {
        IndexKind::FixedUnits => {
            require_keys_eq!(
                args.fixed_weight_quote_mint,
                Pubkey::default(),
                BasketError::InvalidFixedWeightConfig
            );
            require!(
                args.fixed_weight_rebalance_interval_seconds == 0
                    && args.fixed_weight_drift_threshold_bps == 0
                    && args.fixed_weight_spot_ema_max_deviation_bps == 0,
                BasketError::InvalidFixedWeightConfig
            );
        }
        IndexKind::FixedWeights => {
            // Fixed-weight baskets rebalance by routing through USDC, so the quote
            // mint must be USDC. Per-component weights/oracle pairs are validated at
            // finalize via validate_index_strategy_config.
            require_keys_eq!(
                args.fixed_weight_quote_mint,
                USDC_MINT,
                BasketError::InvalidFixedWeightConfig
            );
        }
    }
    Ok(())
}

fn validate_index_basics(
    name: &str,
    symbol: &str,
    metadata_uri: &str,
    component_count: u8,
    decimals: u8,
) -> Result<()> {
    require!(!name.is_empty(), BasketError::EmptyName);
    require!(name.len() <= MAX_NAME_LEN, BasketError::NameTooLong);
    validate_symbol(symbol)?;
    require!(
        metadata_uri.len() <= MAX_METADATA_URI_LEN,
        BasketError::MetadataUriTooLong
    );
    // Every basket kind shares one cap (see MAX_BASKET_COMPONENTS).
    require!(
        component_count as usize >= 1 && component_count as usize <= MAX_BASKET_COMPONENTS,
        BasketError::InvalidComponentCount
    );
    require!(
        decimals <= crate::constants::MAX_INDEX_DECIMALS,
        BasketError::UnsupportedIndexDecimals
    );
    Ok(())
}

/// A symbol is 1 to MAX_SYMBOL_LEN characters, each A-Z or 0-9: one case and no characters
/// from other scripts, so its symbol claim (seeded by symbol_claim_key, which also folds the
/// look-alike digits) reserves it, and every symbol that passes for it, for one basket.
fn validate_symbol(symbol: &str) -> Result<()> {
    require!(!symbol.is_empty(), BasketError::EmptySymbol);
    require!(symbol.len() <= MAX_SYMBOL_LEN, BasketError::SymbolTooLong);
    require!(
        symbol
            .bytes()
            .all(|byte| byte.is_ascii_uppercase() || byte.is_ascii_digit()),
        BasketError::InvalidSymbol
    );
    Ok(())
}

/// The claim is seeded by a key the client passes (seeds can only name accounts and
/// arguments), so check it is the symbol's.
pub(crate) fn validate_symbol_claim_key(symbol: &str, key: &str) -> Result<()> {
    require!(
        key.as_bytes() == symbol_claim_key(symbol).as_slice(),
        BasketError::InvalidSymbol
    );
    Ok(())
}

fn expected_page_start(page_index: u8) -> Result<u16> {
    let start = usize::from(page_index)
        .checked_mul(MAX_LARGE_BASKET_COMPONENTS_PER_PAGE)
        .ok_or_else(|| error!(BasketError::ArithmeticOverflow))?;
    u16::try_from(start).map_err(|_| error!(BasketError::ArithmeticOverflow))
}

#[allow(clippy::too_many_arguments)]
fn create_index_mint<'info>(
    payer: &Signer<'info>,
    index_mint: &UncheckedAccount<'info>,
    vault_authority: &Pubkey,
    token_program: &Program<'info, Token>,
    system_program: &Program<'info, System>,
    index_key: &Pubkey,
    index_mint_bump: u8,
    decimals: u8,
) -> Result<()> {
    let signer_seeds: &[&[u8]] = &[INDEX_MINT_SEED, index_key.as_ref(), &[index_mint_bump]];
    let lamports = Rent::get()?.minimum_balance(Mint::LEN);

    system_program::create_account(
        CpiContext::new_with_signer(
            system_program.to_account_info(),
            system_program::CreateAccount {
                from: payer.to_account_info(),
                to: index_mint.to_account_info(),
            },
            &[signer_seeds],
        ),
        lamports,
        Mint::LEN as u64,
        &token_program.key(),
    )?;

    token::initialize_mint2(
        CpiContext::new(
            token_program.to_account_info(),
            token::InitializeMint2 {
                mint: index_mint.to_account_info(),
            },
        ),
        decimals,
        vault_authority,
        Some(vault_authority),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(kind: IndexKind, component_count: u8) -> CreateLargeBasketIndexArgs {
        let fixed_weights = kind == IndexKind::FixedWeights;
        CreateLargeBasketIndexArgs {
            name: "Index".into(),
            symbol: "IDX".into(),
            metadata_uri: String::new(),
            decimals: 6,
            fee_recipient: Pubkey::new_unique(),
            creator_fee_recipient: Pubkey::default(),
            max_supply: 0,
            rebalance_delay_seconds: 0,
            kind,
            fixed_weight_quote_mint: if fixed_weights { USDC_MINT } else { Pubkey::default() },
            fixed_weight_rebalance_interval_seconds: 0,
            fixed_weight_drift_threshold_bps: 0,
            fixed_weight_spot_ema_max_deviation_bps: 0,
            component_count,
            symbol_claim_key: "IDX".into(),
        }
    }

    #[test]
    fn symbols_are_uppercase_letters_and_digits() {
        for symbol in ["SOLB", "METADAO", "X", "B2B", "ABCDEFGHIJ"] {
            assert!(validate_symbol(symbol).is_ok(), "{symbol}");
        }
        for symbol in ["", "solb", "SOL-B", "SOL B", "SОLB", "ABCDEFGHIJK"] {
            assert!(validate_symbol(symbol).is_err(), "{symbol}");
        }
    }

    #[test]
    fn every_basket_kind_is_capped_at_forty_components() {
        for kind in [IndexKind::FixedWeights, IndexKind::FixedUnits] {
            assert!(validate_large_index_args(&args(kind, 1)).is_ok());
            assert!(validate_large_index_args(&args(kind, 40)).is_ok());
            assert!(validate_large_index_args(&args(kind, 41)).is_err());
            assert!(validate_large_index_args(&args(kind, 0)).is_err());
        }
    }
}
