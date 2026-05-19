use anchor_lang::prelude::*;
use anchor_spl::token::{self, MintTo, Token, TokenAccount, Transfer};

use crate::{
    constants::VAULT_AUTHORITY_SEED,
    errors::OmnindexError,
    events::IndexMinted,
    state::IndexState,
    utils::{
        add_fee, associated_token_address, create_associated_token_account_idempotent, load_mint,
        load_user_token_account, mint_component_backing_amount, require_remaining_account_pairs,
        validate_pending_component_targets_integral, validate_user_token_account,
        validate_vault_token_account, ASSOCIATED_TOKEN_ID,
    },
};

#[derive(AnchorSerialize, AnchorDeserialize, Clone, Debug)]
pub struct MintIndexArgs {
    pub amount: u64,
}

#[derive(Accounts)]
pub struct MintIndex<'info> {
    #[account(mut)]
    pub depositor: Signer<'info>,
    #[account(mut, has_one = index_mint @ OmnindexError::IndexMintMismatch)]
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
    /// CHECK: Created and validated as the depositor's index ATA before minting.
    #[account(mut)]
    pub depositor_index_token_account: UncheckedAccount<'info>,
    /// CHECK: Validated as the Associated Token Program.
    #[account(address = ASSOCIATED_TOKEN_ID @ OmnindexError::InvalidAssociatedTokenProgram)]
    pub associated_token_program: UncheckedAccount<'info>,
    pub token_program: Program<'info, Token>,
    pub system_program: Program<'info, System>,
}

impl<'info> MintIndex<'info> {
    pub fn handle(ctx: Context<'_, '_, 'info, 'info, Self>, args: MintIndexArgs) -> Result<()> {
        require!(args.amount > 0, OmnindexError::InvalidIndexAmount);

        let index = &ctx.accounts.index;
        let index_mint = load_mint(&ctx.accounts.index_mint.to_account_info())?;
        require!(!index.minting_paused, OmnindexError::MintingPaused);
        require!(index.mint_fee_bps == 0, OmnindexError::FeesRequireUsdcQuote);
        let current_supply = index_mint.supply;
        let post_supply = current_supply
            .checked_add(args.amount)
            .ok_or_else(|| error!(OmnindexError::ArithmeticOverflow))?;
        if index.max_supply > 0 {
            require!(
                post_supply <= index.max_supply,
                OmnindexError::SupplyCapExceeded
            );
        }
        validate_pending_component_targets_integral(index, post_supply)?;

        let base_units = index.index_base_units()?;
        require_remaining_account_pairs(ctx.remaining_accounts.len(), index.components.len())?;

        for (component, accounts) in index
            .components
            .iter()
            .zip(ctx.remaining_accounts.chunks_exact(2))
        {
            let user_token_info = &accounts[0];
            let vault_token_info = &accounts[1];

            let user_token_account = load_user_token_account(user_token_info)?;
            let vault_token_account = Account::<TokenAccount>::try_from(vault_token_info)?;

            validate_user_token_account(
                &user_token_account,
                &ctx.accounts.depositor.key(),
                &component.mint,
            )?;
            validate_vault_token_account(
                &vault_token_account,
                &vault_token_info.key(),
                &ctx.accounts.vault_authority.key(),
                &component.mint,
            )?;

            let component_amount = mint_component_backing_amount(
                component,
                args.amount,
                base_units,
                current_supply,
                vault_token_account.amount,
            )?;
            let (deposit_amount, _) = add_fee(component_amount, index.mint_fee_bps)?;

            token::transfer(
                CpiContext::new(
                    ctx.accounts.token_program.to_account_info(),
                    Transfer {
                        from: user_token_info.clone(),
                        to: vault_token_info.clone(),
                        authority: ctx.accounts.depositor.to_account_info(),
                    },
                ),
                deposit_amount,
            )?;
        }

        let signer_seeds: &[&[u8]] = &[
            VAULT_AUTHORITY_SEED,
            ctx.accounts.index.to_account_info().key.as_ref(),
            &[ctx.accounts.index.vault_authority_bump],
        ];

        create_depositor_index_ata_if_needed(&ctx)?;

        token::mint_to(
            CpiContext::new_with_signer(
                ctx.accounts.token_program.to_account_info(),
                MintTo {
                    mint: ctx.accounts.index_mint.to_account_info(),
                    to: ctx.accounts.depositor_index_token_account.to_account_info(),
                    authority: ctx.accounts.vault_authority.to_account_info(),
                },
                &[signer_seeds],
            ),
            args.amount,
        )?;

        emit!(IndexMinted {
            index: ctx.accounts.index.key(),
            depositor: ctx.accounts.depositor.key(),
            amount: args.amount,
        });

        Ok(())
    }
}

fn create_depositor_index_ata_if_needed<'info>(
    ctx: &Context<'_, '_, 'info, 'info, MintIndex<'info>>,
) -> Result<()> {
    let expected_ata = associated_token_address(
        &ctx.accounts.depositor.key(),
        &ctx.accounts.index_mint.key(),
    );
    require_keys_eq!(
        ctx.accounts.depositor_index_token_account.key(),
        expected_ata,
        OmnindexError::InvalidUserTokenAccount
    );

    create_associated_token_account_idempotent(
        ctx.accounts.associated_token_program.to_account_info(),
        ctx.accounts.depositor.to_account_info(),
        ctx.accounts.depositor_index_token_account.to_account_info(),
        ctx.accounts.depositor.to_account_info(),
        ctx.accounts.index_mint.to_account_info(),
        ctx.accounts.system_program.to_account_info(),
        ctx.accounts.token_program.to_account_info(),
    )
}
