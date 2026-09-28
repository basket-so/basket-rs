use std::collections::BTreeSet;

use anchor_lang::{prelude::*, system_program};
use anchor_spl::token::{self, Mint, Token};

use crate::{
    constants::{
        INDEX_MINT_SEED, INDEX_SEED, LARGE_BASKET_COMPONENT_PAGE_SEED,
        MAX_LARGE_BASKET_COMPONENTS, MAX_LARGE_BASKET_COMPONENTS_PER_PAGE, MAX_LARGE_BASKET_PAGES,
        MAX_METADATA_URI_LEN, MAX_NAME_LEN, MAX_REBALANCE_DELAY_SECONDS, MAX_SYMBOL_LEN,
        PROTOCOL_CONFIG_SEED, USDC_MINT, VAULT_AUTHORITY_SEED,
    },
    errors::BasketError,
    events::{IndexCreated, LargeBasketComponentPageInitialized, LargeBasketConfigFinalized},
    state::{
        IndexComponent, IndexComponentInput, IndexKind, IndexState, LargeBasketComponent,
        LargeBasketComponentPage, ProtocolConfig,
    },
    utils::{
        associated_token_address_with_token_program,
        create_associated_token_account_idempotent_for_token_program, load_interface_mint,
        load_mint, validate_index_strategy_config, validate_no_self_component,
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
    pub fn handle(
        ctx: Context<'_, '_, '_, 'info, Self>,
        args: CreateLargeBasketIndexArgs,
    ) -> Result<()> {
        require!(
            ctx.accounts
                .protocol_config
                .can_create_index(&ctx.accounts.authority.key()),
            BasketError::UnauthorizedIndexCreator
        );
        validate_large_index_args(&args)?;
        create_index_mint(&ctx, args.decimals)?;

        let fee_recipient = if args.fee_recipient == Pubkey::default() {
            ctx.accounts.authority.key()
        } else {
            args.fee_recipient
        };
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
        index.large_basket_component_count = args.component_count;
        index.large_basket_page_count = 0;
        index.page_generation = 0;
        index.large_basket_configured = false;
        index.large_basket_operation_in_progress = false;
        index.mint_fee_bps = 0;
        index.redeem_fee_bps = 0;
        index.creator_mint_fee_bps = 0;
        index.creator_redeem_fee_bps = 0;
        index.staking_mint_fee_bps = 0;
        index.staking_redeem_fee_bps = 0;
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
        index.active_rebalance_intent = Pubkey::default();
        index.pending_rebalance_oracle_price_tolerance_bps = 0;
        index.pending_rebalance_nav_tolerance_bps = 0;
        index.fixed_weight_drift_threshold_bps = args.fixed_weight_drift_threshold_bps;
        index.fixed_weight_spot_ema_max_deviation_bps =
            args.fixed_weight_spot_ema_max_deviation_bps;
        index.minting_paused = false;
        index.redeeming_paused = false;
        index.rebalancing_paused = false;
        index.pending_rebalance_ready = false;
        index.reserved = [0; 1];
        index.name = args.name;
        index.symbol = args.symbol;
        index.metadata_uri = args.metadata_uri;

        emit!(IndexCreated {
            index: index.key(),
            index_mint: index.index_mint,
            authority: index.authority,
            kind: index.kind,
            components: index.large_basket_component_count,
        });

        Ok(())
    }
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
    require!(!args.name.is_empty(), BasketError::EmptyName);
    require!(!args.symbol.is_empty(), BasketError::EmptySymbol);
    require!(args.name.len() <= MAX_NAME_LEN, BasketError::NameTooLong);
    require!(
        args.symbol.len() <= MAX_SYMBOL_LEN,
        BasketError::SymbolTooLong
    );
    require!(
        args.metadata_uri.len() <= MAX_METADATA_URI_LEN,
        BasketError::MetadataUriTooLong
    );
    require!(
        args.component_count as usize >= 1
            && args.component_count as usize <= MAX_LARGE_BASKET_COMPONENTS,
        BasketError::InvalidComponentCount
    );
    require!(
        args.decimals <= crate::constants::MAX_INDEX_DECIMALS,
        BasketError::UnsupportedIndexDecimals
    );
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

fn expected_page_start(page_index: u8) -> Result<u16> {
    let start = usize::from(page_index)
        .checked_mul(MAX_LARGE_BASKET_COMPONENTS_PER_PAGE)
        .ok_or_else(|| error!(BasketError::ArithmeticOverflow))?;
    u16::try_from(start).map_err(|_| error!(BasketError::ArithmeticOverflow))
}

fn create_index_mint<'info>(
    ctx: &Context<'_, '_, '_, 'info, CreateLargeBasketIndex<'info>>,
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
