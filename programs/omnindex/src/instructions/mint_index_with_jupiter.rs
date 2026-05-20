use anchor_lang::prelude::*;
use anchor_spl::{
    token::{self, MintTo, Token},
    token_interface::{
        self, Mint as InterfaceMint, TokenAccount as InterfaceTokenAccount, TokenInterface,
        TransferChecked,
    },
};

use crate::{
    constants::{USDC_MINT, VAULT_AUTHORITY_SEED},
    errors::OmnindexError,
    events::IndexMinted,
    state::{IndexComponent, IndexState},
    utils::{
        associated_token_address, associated_token_address_with_token_program,
        create_associated_token_account_idempotent,
        create_associated_token_account_idempotent_for_token_program, invoke_jupiter_swap,
        load_interface_mint, load_interface_token_account, load_mint,
        mint_component_backing_amount, switchboard_feed_price, validate_buy_execution_price,
        validate_pending_component_targets_integral, verified_switchboard_prices,
        JupiterAccountMetaInput, ASSOCIATED_TOKEN_ID,
    },
};

#[derive(AnchorSerialize, AnchorDeserialize, Clone, Debug)]
pub struct JupiterSwapPlan {
    pub input_mint: Pubkey,
    pub output_mint: Pubkey,
    pub source_token_account: Pubkey,
    pub destination_token_account: Pubkey,
    pub max_oracle_slippage_bps: u16,
    pub instruction_data: Vec<u8>,
    pub accounts: Vec<JupiterAccountMetaInput>,
}

#[derive(AnchorSerialize, AnchorDeserialize, Clone, Debug)]
pub struct MintIndexWithJupiterArgs {
    pub index_amount_out: u64,
    pub max_quote_in: u64,
    pub switchboard_max_age_slots: u64,
    pub swaps: Vec<JupiterSwapPlan>,
}

#[derive(Accounts)]
pub struct MintIndexWithJupiter<'info> {
    #[account(mut)]
    pub user: Signer<'info>,
    #[account(mut, has_one = index_mint @ OmnindexError::IndexMintMismatch)]
    pub index: Account<'info, IndexState>,
    #[account(mut)]
    /// CHECK: Validated as the configured classic SPL index mint.
    pub index_mint: UncheckedAccount<'info>,
    /// CHECK: PDA authority over the index mint and component vaults.
    #[account(
        seeds = [VAULT_AUTHORITY_SEED, index.key().as_ref()],
        bump = index.vault_authority_bump
    )]
    pub vault_authority: UncheckedAccount<'info>,
    pub quote_mint: InterfaceAccount<'info, InterfaceMint>,
    #[account(mut)]
    pub user_quote_token_account: InterfaceAccount<'info, InterfaceTokenAccount>,
    #[account(mut)]
    /// CHECK: Created and validated as the user's index ATA before minting.
    pub user_index_token_account: UncheckedAccount<'info>,
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
    #[account(address = ASSOCIATED_TOKEN_ID @ OmnindexError::InvalidAssociatedTokenProgram)]
    pub associated_token_program: UncheckedAccount<'info>,
    pub quote_token_program: Interface<'info, TokenInterface>,
    pub index_token_program: Program<'info, Token>,
    pub system_program: Program<'info, System>,
}

#[derive(Clone)]
struct ComponentAccount<'info> {
    mint_info: AccountInfo<'info>,
    vault_info: AccountInfo<'info>,
    token_program_info: AccountInfo<'info>,
    decimals: u8,
}

impl<'info> MintIndexWithJupiter<'info> {
    pub fn handle(
        ctx: Context<'_, '_, 'info, 'info, Self>,
        args: MintIndexWithJupiterArgs,
    ) -> Result<()> {
        require!(args.index_amount_out > 0, OmnindexError::InvalidIndexAmount);
        require!(
            !ctx.accounts.index.minting_paused,
            OmnindexError::MintingPaused
        );
        require_keys_eq!(
            ctx.accounts.quote_mint.key(),
            USDC_MINT,
            OmnindexError::InvalidQuoteMint
        );
        require_keys_eq!(
            *ctx.accounts.quote_mint.to_account_info().owner,
            ctx.accounts.quote_token_program.key(),
            OmnindexError::InvalidQuoteMint
        );
        require!(
            ctx.accounts.index.mint_fee_bps == 0,
            OmnindexError::FeesRequireUsdcQuote
        );
        require!(
            args.swaps.len() <= ctx.accounts.index.components.len(),
            OmnindexError::InvalidJupiterRoute
        );

        let index_mint = load_mint(&ctx.accounts.index_mint.to_account_info())?;
        let current_supply = index_mint.supply;
        let post_supply = current_supply
            .checked_add(args.index_amount_out)
            .ok_or_else(|| error!(OmnindexError::ArithmeticOverflow))?;
        if ctx.accounts.index.max_supply > 0 {
            require!(
                post_supply <= ctx.accounts.index.max_supply,
                OmnindexError::SupplyCapExceeded
            );
        }
        validate_pending_component_targets_integral(&ctx.accounts.index, post_supply)?;

        let prices = verified_switchboard_prices(
            &ctx.accounts.switchboard_queue.to_account_info(),
            &ctx.accounts.switchboard_quote.to_account_info(),
            &ctx.accounts.slothashes.to_account_info(),
            &ctx.accounts.instructions_sysvar.to_account_info(),
            Clock::get()?.slot,
            args.switchboard_max_age_slots,
        )?;

        let components = ctx.accounts.index.components.clone();
        let mut remaining = ctx.remaining_accounts.iter();
        let mut component_accounts = Vec::with_capacity(components.len());
        for component in &components {
            component_accounts.push(load_component_account(&ctx, &mut remaining, component)?);
        }
        let route_accounts = remaining.as_slice();
        let candidates = account_candidates(&ctx, &component_accounts, route_accounts);

        let base_units = ctx.accounts.index.index_base_units()?;
        let quote_decimals = ctx.accounts.quote_mint.decimals;
        let mut total_quote_spent = 0u64;
        let mut swap_index = 0usize;

        for (component, accounts) in components.iter().zip(component_accounts.into_iter()) {
            let required_amount = mint_component_backing_amount(
                component,
                args.index_amount_out,
                base_units,
                current_supply,
                load_interface_token_account(&accounts.vault_info)?.amount,
            )?;
            if required_amount == 0 {
                continue;
            }

            if component.mint == ctx.accounts.quote_mint.key() {
                let new_total =
                    require_quote_budget(total_quote_spent, required_amount, args.max_quote_in)?;
                transfer_checked_from_user_quote(
                    &ctx,
                    accounts.vault_info.clone(),
                    required_amount,
                )?;
                ctx.accounts.user_quote_token_account.reload()?;
                total_quote_spent = new_total;
                continue;
            }

            let swap = args
                .swaps
                .get(swap_index)
                .ok_or_else(|| error!(OmnindexError::InvalidJupiterRoute))?;
            swap_index = swap_index
                .checked_add(1)
                .ok_or_else(|| error!(OmnindexError::ArithmeticOverflow))?;

            validate_component_buy_swap(
                ctx.accounts.quote_mint.key(),
                ctx.accounts.user_quote_token_account.key(),
                component.mint,
                accounts.vault_info.key(),
                swap,
            )?;
            let quote_before = ctx.accounts.user_quote_token_account.amount;
            let component_before = load_interface_token_account(&accounts.vault_info)?.amount;

            invoke_jupiter_swap(
                ctx.accounts.jupiter_program.to_account_info(),
                &candidates,
                &swap.accounts,
                &swap.instruction_data,
                Some(ctx.accounts.user.key()),
                &[],
            )?;

            ctx.accounts.user_quote_token_account.reload()?;
            let component_after = load_interface_token_account(&accounts.vault_info)?.amount;
            let quote_after = ctx.accounts.user_quote_token_account.amount;
            let quote_spent = quote_before
                .checked_sub(quote_after)
                .ok_or_else(|| error!(OmnindexError::InvalidJupiterRoute))?;
            let component_received = component_after
                .checked_sub(component_before)
                .ok_or_else(|| error!(OmnindexError::InvalidJupiterRoute))?;
            require!(
                component_received >= required_amount,
                OmnindexError::InvalidJupiterRoute
            );
            total_quote_spent =
                require_quote_budget(total_quote_spent, quote_spent, args.max_quote_in)?;

            let oracle_price = switchboard_feed_price(&prices, &component.oracle_pair)?;
            validate_buy_execution_price(
                quote_spent,
                component_received,
                quote_decimals,
                accounts.decimals,
                oracle_price,
                swap.max_oracle_slippage_bps,
            )?;
        }

        require!(
            swap_index == args.swaps.len(),
            OmnindexError::InvalidJupiterRoute
        );

        mint_index_tokens(&ctx, args.index_amount_out)?;

        emit!(IndexMinted {
            index: ctx.accounts.index.key(),
            depositor: ctx.accounts.user.key(),
            amount: args.index_amount_out,
        });

        Ok(())
    }
}

fn load_component_account<'info>(
    ctx: &Context<'_, '_, 'info, 'info, MintIndexWithJupiter<'info>>,
    remaining: &mut std::slice::Iter<'info, AccountInfo<'info>>,
    component: &IndexComponent,
) -> Result<ComponentAccount<'info>> {
    let mint_info = next_account_info(remaining)?;
    let vault_info = next_account_info(remaining)?;
    let token_program_info = next_account_info(remaining)?;

    require_keys_eq!(
        mint_info.key(),
        component.mint,
        OmnindexError::InvalidComponentMint
    );
    require_keys_eq!(
        *mint_info.owner,
        token_program_info.key(),
        OmnindexError::InvalidTokenMint
    );

    let expected_vault = associated_token_address_with_token_program(
        &ctx.accounts.vault_authority.key(),
        &component.mint,
        token_program_info.key,
    );
    require_keys_eq!(
        vault_info.key(),
        expected_vault,
        OmnindexError::InvalidVaultAccount
    );
    create_associated_token_account_idempotent_for_token_program(
        ctx.accounts.associated_token_program.to_account_info(),
        ctx.accounts.user.to_account_info(),
        vault_info.clone(),
        ctx.accounts.vault_authority.to_account_info(),
        mint_info.clone(),
        ctx.accounts.system_program.to_account_info(),
        token_program_info.clone(),
    )?;

    let mint = load_interface_mint(mint_info)?;
    let vault = load_interface_token_account(vault_info)?;
    require_keys_eq!(
        vault.owner,
        ctx.accounts.vault_authority.key(),
        OmnindexError::InvalidVaultAccount
    );
    require_keys_eq!(
        vault.mint,
        component.mint,
        OmnindexError::InvalidVaultAccount
    );

    Ok(ComponentAccount {
        mint_info: mint_info.clone(),
        vault_info: vault_info.clone(),
        token_program_info: token_program_info.clone(),
        decimals: mint.decimals,
    })
}

fn validate_component_buy_swap(
    quote_mint: Pubkey,
    user_quote_token_account: Pubkey,
    component_mint: Pubkey,
    component_vault: Pubkey,
    swap: &JupiterSwapPlan,
) -> Result<()> {
    require_keys_eq!(
        swap.input_mint,
        quote_mint,
        OmnindexError::InvalidJupiterRoute
    );
    require_keys_eq!(
        swap.output_mint,
        component_mint,
        OmnindexError::InvalidJupiterRoute
    );
    require_keys_eq!(
        swap.source_token_account,
        user_quote_token_account,
        OmnindexError::InvalidJupiterRoute
    );
    require_keys_eq!(
        swap.destination_token_account,
        component_vault,
        OmnindexError::InvalidJupiterRoute
    );
    Ok(())
}

fn account_candidates<'info>(
    ctx: &Context<'_, '_, 'info, 'info, MintIndexWithJupiter<'info>>,
    component_accounts: &[ComponentAccount<'info>],
    route_accounts: &[AccountInfo<'info>],
) -> Vec<AccountInfo<'info>> {
    let mut candidates = vec![
        ctx.accounts.user.to_account_info(),
        ctx.accounts.index.to_account_info(),
        ctx.accounts.index_mint.to_account_info(),
        ctx.accounts.vault_authority.to_account_info(),
        ctx.accounts.quote_mint.to_account_info(),
        ctx.accounts.user_quote_token_account.to_account_info(),
        ctx.accounts.user_index_token_account.to_account_info(),
        ctx.accounts.jupiter_program.to_account_info(),
        ctx.accounts.associated_token_program.to_account_info(),
        ctx.accounts.quote_token_program.to_account_info(),
        ctx.accounts.index_token_program.to_account_info(),
        ctx.accounts.system_program.to_account_info(),
    ];
    for account in component_accounts {
        candidates.push(account.mint_info.clone());
        candidates.push(account.vault_info.clone());
        candidates.push(account.token_program_info.clone());
    }
    candidates.extend_from_slice(route_accounts);
    candidates
}

fn transfer_checked_from_user_quote<'info>(
    ctx: &Context<'_, '_, 'info, 'info, MintIndexWithJupiter<'info>>,
    destination: AccountInfo<'info>,
    amount: u64,
) -> Result<()> {
    token_interface::transfer_checked(
        CpiContext::new(
            ctx.accounts.quote_token_program.to_account_info(),
            TransferChecked {
                from: ctx.accounts.user_quote_token_account.to_account_info(),
                mint: ctx.accounts.quote_mint.to_account_info(),
                to: destination,
                authority: ctx.accounts.user.to_account_info(),
            },
        ),
        amount,
        ctx.accounts.quote_mint.decimals,
    )
}

fn mint_index_tokens<'info>(
    ctx: &Context<'_, '_, 'info, 'info, MintIndexWithJupiter<'info>>,
    amount: u64,
) -> Result<()> {
    create_user_index_ata_if_needed(ctx)?;
    let signer_seeds: &[&[u8]] = &[
        VAULT_AUTHORITY_SEED,
        ctx.accounts.index.to_account_info().key.as_ref(),
        &[ctx.accounts.index.vault_authority_bump],
    ];

    token::mint_to(
        CpiContext::new_with_signer(
            ctx.accounts.index_token_program.to_account_info(),
            MintTo {
                mint: ctx.accounts.index_mint.to_account_info(),
                to: ctx.accounts.user_index_token_account.to_account_info(),
                authority: ctx.accounts.vault_authority.to_account_info(),
            },
            &[signer_seeds],
        ),
        amount,
    )
}

fn create_user_index_ata_if_needed<'info>(
    ctx: &Context<'_, '_, 'info, 'info, MintIndexWithJupiter<'info>>,
) -> Result<()> {
    let expected_ata =
        associated_token_address(&ctx.accounts.user.key(), &ctx.accounts.index_mint.key());
    require_keys_eq!(
        ctx.accounts.user_index_token_account.key(),
        expected_ata,
        OmnindexError::InvalidUserTokenAccount
    );

    create_associated_token_account_idempotent(
        ctx.accounts.associated_token_program.to_account_info(),
        ctx.accounts.user.to_account_info(),
        ctx.accounts.user_index_token_account.to_account_info(),
        ctx.accounts.user.to_account_info(),
        ctx.accounts.index_mint.to_account_info(),
        ctx.accounts.system_program.to_account_info(),
        ctx.accounts.index_token_program.to_account_info(),
    )
}

fn require_quote_budget(
    total_quote_spent: u64,
    quote_input: u64,
    max_quote_in: u64,
) -> Result<u64> {
    let new_total_quote_spent = total_quote_spent
        .checked_add(quote_input)
        .ok_or_else(|| error!(OmnindexError::ArithmeticOverflow))?;
    require!(
        new_total_quote_spent <= max_quote_in,
        OmnindexError::QuoteBudgetExceeded
    );
    Ok(new_total_quote_spent)
}
