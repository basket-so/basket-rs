use anchor_lang::prelude::*;
use mpl_token_metadata::{
    accounts::Metadata as MplMetadata,
    instructions::{
        CreateMetadataAccountV3Cpi, CreateMetadataAccountV3CpiAccounts,
        CreateMetadataAccountV3InstructionArgs, UpdateMetadataAccountV2Cpi,
        UpdateMetadataAccountV2CpiAccounts, UpdateMetadataAccountV2InstructionArgs,
    },
    types::DataV2,
};

use crate::{
    constants::{MAX_METADATA_URI_LEN, VAULT_AUTHORITY_SEED},
    errors::BasketError,
    events::{IndexMetadataAuthorityMigrated, IndexMetadataUpdated},
    state::IndexState,
};

#[derive(AnchorSerialize, AnchorDeserialize, Clone, Debug)]
pub struct IndexMetadataArgs {
    pub uri: String,
}

#[derive(Accounts)]
pub struct CreateIndexMetadata<'info> {
    #[account(mut)]
    pub payer: Signer<'info>,
    pub authority: Signer<'info>,
    #[account(
        mut,
        has_one = authority @ BasketError::UnauthorizedAuthority,
        has_one = index_mint @ BasketError::IndexMintMismatch
    )]
    pub index: Account<'info, IndexState>,
    /// CHECK: Validated by the index state and used as the metadata mint.
    pub index_mint: UncheckedAccount<'info>,
    /// CHECK: Validated against the Metaplex metadata PDA for the index mint.
    #[account(mut)]
    pub metadata: UncheckedAccount<'info>,
    /// CHECK: PDA authority over the index mint and component vaults.
    #[account(
        seeds = [VAULT_AUTHORITY_SEED, index.key().as_ref()],
        bump = index.vault_authority_bump
    )]
    pub vault_authority: UncheckedAccount<'info>,
    /// CHECK: Validated as the Metaplex Token Metadata program.
    #[account(address = mpl_token_metadata::ID @ BasketError::InvalidMetadataAccount)]
    pub metadata_program: UncheckedAccount<'info>,
    pub system_program: Program<'info, System>,
    pub rent: Sysvar<'info, Rent>,
}

#[derive(Accounts)]
pub struct UpdateIndexMetadata<'info> {
    pub authority: Signer<'info>,
    #[account(
        mut,
        has_one = authority @ BasketError::UnauthorizedAuthority,
        has_one = index_mint @ BasketError::IndexMintMismatch
    )]
    pub index: Account<'info, IndexState>,
    /// CHECK: Validated by the index state and used as the metadata mint.
    pub index_mint: UncheckedAccount<'info>,
    /// CHECK: Validated against the Metaplex metadata PDA for the index mint.
    #[account(mut)]
    pub metadata: UncheckedAccount<'info>,
    /// CHECK: PDA authority over the index mint, component vaults, and metadata.
    #[account(
        seeds = [VAULT_AUTHORITY_SEED, index.key().as_ref()],
        bump = index.vault_authority_bump
    )]
    pub vault_authority: UncheckedAccount<'info>,
    /// CHECK: Validated as the Metaplex Token Metadata program.
    #[account(address = mpl_token_metadata::ID @ BasketError::InvalidMetadataAccount)]
    pub metadata_program: UncheckedAccount<'info>,
}

#[derive(Accounts)]
pub struct MigrateIndexMetadataAuthority<'info> {
    pub authority: Signer<'info>,
    pub metadata_authority: Signer<'info>,
    #[account(
        has_one = authority @ BasketError::UnauthorizedAuthority,
        has_one = index_mint @ BasketError::IndexMintMismatch
    )]
    pub index: Account<'info, IndexState>,
    /// CHECK: Validated by the index state and used as the metadata mint.
    pub index_mint: UncheckedAccount<'info>,
    /// CHECK: Validated against the Metaplex metadata PDA for the index mint.
    #[account(mut)]
    pub metadata: UncheckedAccount<'info>,
    /// CHECK: PDA authority over the index mint, component vaults, and metadata.
    #[account(
        seeds = [VAULT_AUTHORITY_SEED, index.key().as_ref()],
        bump = index.vault_authority_bump
    )]
    pub vault_authority: UncheckedAccount<'info>,
    /// CHECK: Validated as the Metaplex Token Metadata program.
    #[account(address = mpl_token_metadata::ID @ BasketError::InvalidMetadataAccount)]
    pub metadata_program: UncheckedAccount<'info>,
}

impl<'info> CreateIndexMetadata<'info> {
    pub fn handle(ctx: Context<Self>, args: IndexMetadataArgs) -> Result<()> {
        require!(
            args.uri.len() <= MAX_METADATA_URI_LEN,
            BasketError::MetadataUriTooLong
        );
        validate_metadata_pda(&ctx.accounts.metadata.key(), &ctx.accounts.index_mint.key())?;

        let signer_seeds: &[&[u8]] = &[
            VAULT_AUTHORITY_SEED,
            ctx.accounts.index.to_account_info().key.as_ref(),
            &[ctx.accounts.index.vault_authority_bump],
        ];

        let metadata_program_info = ctx.accounts.metadata_program.to_account_info();
        let metadata_info = ctx.accounts.metadata.to_account_info();
        let mint_info = ctx.accounts.index_mint.to_account_info();
        let mint_authority_info = ctx.accounts.vault_authority.to_account_info();
        let payer_info = ctx.accounts.payer.to_account_info();
        let update_authority_info = ctx.accounts.vault_authority.to_account_info();
        let system_program_info = ctx.accounts.system_program.to_account_info();
        let rent_info = ctx.accounts.rent.to_account_info();

        CreateMetadataAccountV3Cpi::new(
            &metadata_program_info,
            CreateMetadataAccountV3CpiAccounts {
                metadata: &metadata_info,
                mint: &mint_info,
                mint_authority: &mint_authority_info,
                payer: &payer_info,
                update_authority: (&update_authority_info, true),
                system_program: &system_program_info,
                rent: Some(&rent_info),
            },
            CreateMetadataAccountV3InstructionArgs {
                data: metadata_data(&ctx.accounts.index, args.uri.clone()),
                is_mutable: true,
                collection_details: None,
            },
        )
        .invoke_signed(&[signer_seeds])?;

        let index = &mut ctx.accounts.index;
        index.metadata_uri = args.uri;

        emit!(IndexMetadataUpdated {
            index: index.key(),
            metadata: ctx.accounts.metadata.key(),
            uri: index.metadata_uri.clone(),
        });

        Ok(())
    }
}

impl<'info> UpdateIndexMetadata<'info> {
    pub fn handle(ctx: Context<Self>, args: IndexMetadataArgs) -> Result<()> {
        require!(
            args.uri.len() <= MAX_METADATA_URI_LEN,
            BasketError::MetadataUriTooLong
        );
        validate_metadata_pda(&ctx.accounts.metadata.key(), &ctx.accounts.index_mint.key())?;

        let metadata_program_info = ctx.accounts.metadata_program.to_account_info();
        let metadata_info = ctx.accounts.metadata.to_account_info();
        let update_authority_info = ctx.accounts.vault_authority.to_account_info();
        let signer_seeds: &[&[u8]] = &[
            VAULT_AUTHORITY_SEED,
            ctx.accounts.index.to_account_info().key.as_ref(),
            &[ctx.accounts.index.vault_authority_bump],
        ];

        UpdateMetadataAccountV2Cpi::new(
            &metadata_program_info,
            UpdateMetadataAccountV2CpiAccounts {
                metadata: &metadata_info,
                update_authority: &update_authority_info,
            },
            UpdateMetadataAccountV2InstructionArgs {
                data: Some(metadata_data(&ctx.accounts.index, args.uri.clone())),
                new_update_authority: None,
                primary_sale_happened: None,
                is_mutable: Some(true),
            },
        )
        .invoke_signed(&[signer_seeds])?;

        let index = &mut ctx.accounts.index;
        index.metadata_uri = args.uri;

        emit!(IndexMetadataUpdated {
            index: index.key(),
            metadata: ctx.accounts.metadata.key(),
            uri: index.metadata_uri.clone(),
        });

        Ok(())
    }
}

impl<'info> MigrateIndexMetadataAuthority<'info> {
    pub fn handle(ctx: Context<Self>) -> Result<()> {
        validate_metadata_pda(&ctx.accounts.metadata.key(), &ctx.accounts.index_mint.key())?;

        let metadata_program_info = ctx.accounts.metadata_program.to_account_info();
        let metadata_info = ctx.accounts.metadata.to_account_info();
        let metadata_authority_info = ctx.accounts.metadata_authority.to_account_info();

        UpdateMetadataAccountV2Cpi::new(
            &metadata_program_info,
            UpdateMetadataAccountV2CpiAccounts {
                metadata: &metadata_info,
                update_authority: &metadata_authority_info,
            },
            UpdateMetadataAccountV2InstructionArgs {
                data: None,
                new_update_authority: Some(ctx.accounts.vault_authority.key()),
                primary_sale_happened: None,
                is_mutable: None,
            },
        )
        .invoke()?;

        emit!(IndexMetadataAuthorityMigrated {
            index: ctx.accounts.index.key(),
            metadata: ctx.accounts.metadata.key(),
            update_authority: ctx.accounts.vault_authority.key(),
        });

        Ok(())
    }
}

fn metadata_data(index: &IndexState, uri: String) -> DataV2 {
    DataV2 {
        name: index.name.clone(),
        symbol: index.symbol.clone(),
        uri,
        seller_fee_basis_points: 0,
        creators: None,
        collection: None,
        uses: None,
    }
}

fn validate_metadata_pda(metadata: &Pubkey, mint: &Pubkey) -> Result<()> {
    let expected_metadata = MplMetadata::find_pda(mint).0;
    require_keys_eq!(
        *metadata,
        expected_metadata,
        BasketError::InvalidMetadataAccount
    );
    Ok(())
}
