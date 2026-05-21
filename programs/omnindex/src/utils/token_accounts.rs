use anchor_lang::{
    prelude::*,
    solana_program::{
        instruction::{AccountMeta, Instruction},
        program::invoke_signed,
        program_pack::Pack,
    },
};
use anchor_spl::token::{self, TokenAccount};
use anchor_spl::token_interface::TokenAccount as InterfaceTokenAccount;

use crate::errors::OmnindexError;

pub const ASSOCIATED_TOKEN_ID: Pubkey = pubkey!("ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL");
pub type SplMint = token::spl_token::state::Mint;
pub type SplTokenAccount = token::spl_token::state::Account;
const MINT_DECIMALS_OFFSET: usize = 44;
const MINT_INITIALIZED_OFFSET: usize = 45;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct InterfaceMintInfo {
    pub decimals: u8,
}

pub fn require_remaining_account_pairs(actual: usize, component_count: usize) -> Result<()> {
    require!(
        actual == component_count * 2,
        OmnindexError::InvalidRemainingAccounts
    );
    Ok(())
}

pub fn associated_token_address(authority: &Pubkey, mint: &Pubkey) -> Pubkey {
    associated_token_address_with_token_program(authority, mint, &token::ID)
}

pub fn associated_token_address_with_token_program(
    authority: &Pubkey,
    mint: &Pubkey,
    token_program: &Pubkey,
) -> Pubkey {
    Pubkey::find_program_address(
        &[authority.as_ref(), token_program.as_ref(), mint.as_ref()],
        &ASSOCIATED_TOKEN_ID,
    )
    .0
}

pub fn create_associated_token_account_idempotent<'info>(
    associated_token_program: AccountInfo<'info>,
    payer: AccountInfo<'info>,
    associated_token: AccountInfo<'info>,
    authority: AccountInfo<'info>,
    mint: AccountInfo<'info>,
    system_program: AccountInfo<'info>,
    token_program: AccountInfo<'info>,
) -> Result<()> {
    require_keys_eq!(
        *associated_token_program.key,
        ASSOCIATED_TOKEN_ID,
        OmnindexError::InvalidAssociatedTokenProgram
    );
    require_keys_eq!(
        *token_program.key,
        token::ID,
        OmnindexError::InvalidTokenProgram
    );

    let instruction = Instruction {
        program_id: ASSOCIATED_TOKEN_ID,
        accounts: vec![
            AccountMeta::new(*payer.key, true),
            AccountMeta::new(*associated_token.key, false),
            AccountMeta::new_readonly(*authority.key, false),
            AccountMeta::new_readonly(*mint.key, false),
            AccountMeta::new_readonly(*system_program.key, false),
            AccountMeta::new_readonly(*token_program.key, false),
        ],
        data: vec![1],
    };

    invoke_signed(
        &instruction,
        &[
            payer,
            associated_token,
            authority,
            mint,
            system_program,
            token_program,
        ],
        &[],
    )
    .map_err(Into::into)
}

pub fn create_associated_token_account_idempotent_for_token_program<'info>(
    associated_token_program: AccountInfo<'info>,
    payer: AccountInfo<'info>,
    associated_token: AccountInfo<'info>,
    authority: AccountInfo<'info>,
    mint: AccountInfo<'info>,
    system_program: AccountInfo<'info>,
    token_program: AccountInfo<'info>,
) -> Result<()> {
    require_keys_eq!(
        *associated_token_program.key,
        ASSOCIATED_TOKEN_ID,
        OmnindexError::InvalidAssociatedTokenProgram
    );
    require!(
        *token_program.key == token::ID || *token_program.key == anchor_spl::token_2022::ID,
        OmnindexError::InvalidTokenProgram
    );

    let instruction = Instruction {
        program_id: ASSOCIATED_TOKEN_ID,
        accounts: vec![
            AccountMeta::new(*payer.key, true),
            AccountMeta::new(*associated_token.key, false),
            AccountMeta::new_readonly(*authority.key, false),
            AccountMeta::new_readonly(*mint.key, false),
            AccountMeta::new_readonly(*system_program.key, false),
            AccountMeta::new_readonly(*token_program.key, false),
        ],
        data: vec![1],
    };

    invoke_signed(
        &instruction,
        &[
            payer,
            associated_token,
            authority,
            mint,
            system_program,
            token_program,
        ],
        &[],
    )
    .map_err(Into::into)
}

pub fn load_mint(info: &AccountInfo) -> Result<SplMint> {
    require_keys_eq!(*info.owner, token::ID, OmnindexError::InvalidTokenMint);
    let data = info.try_borrow_data()?;
    SplMint::unpack(&data).map_err(|_| error!(OmnindexError::InvalidTokenMint))
}

pub fn load_user_token_account(info: &AccountInfo) -> Result<SplTokenAccount> {
    require_keys_eq!(
        *info.owner,
        token::ID,
        OmnindexError::InvalidUserTokenAccount
    );
    let data = info.try_borrow_data()?;
    SplTokenAccount::unpack(&data).map_err(|_| error!(OmnindexError::InvalidUserTokenAccount))
}

pub fn load_interface_mint(info: &AccountInfo) -> Result<InterfaceMintInfo> {
    require!(
        *info.owner == token::ID || *info.owner == anchor_spl::token_2022::ID,
        OmnindexError::InvalidTokenMint
    );
    let data = info.try_borrow_data()?;
    interface_mint_info_from_data(&data)
}

fn interface_mint_info_from_data(data: &[u8]) -> Result<InterfaceMintInfo> {
    require!(data.len() >= SplMint::LEN, OmnindexError::InvalidTokenMint);
    require!(
        data[MINT_INITIALIZED_OFFSET] != 0,
        OmnindexError::InvalidTokenMint
    );

    Ok(InterfaceMintInfo {
        decimals: data[MINT_DECIMALS_OFFSET],
    })
}

pub fn load_interface_token_account(info: &AccountInfo) -> Result<InterfaceTokenAccount> {
    require!(
        *info.owner == token::ID || *info.owner == anchor_spl::token_2022::ID,
        OmnindexError::InvalidUserTokenAccount
    );
    let data = info.try_borrow_data()?;
    let mut data_ref: &[u8] = &data;
    InterfaceTokenAccount::try_deserialize_unchecked(&mut data_ref)
        .map_err(|_| error!(OmnindexError::InvalidUserTokenAccount))
}

pub fn validate_user_token_account(
    token_account: &SplTokenAccount,
    expected_owner: &Pubkey,
    expected_mint: &Pubkey,
) -> Result<()> {
    require_keys_eq!(
        token_account.owner,
        *expected_owner,
        OmnindexError::InvalidUserTokenAccount
    );
    require_keys_eq!(
        token_account.mint,
        *expected_mint,
        OmnindexError::InvalidUserTokenAccount
    );
    Ok(())
}

pub fn validate_vault_token_account(
    token_account: &TokenAccount,
    actual_key: &Pubkey,
    vault_authority: &Pubkey,
    component_mint: &Pubkey,
) -> Result<()> {
    let expected_vault = associated_token_address(vault_authority, component_mint);
    require_keys_eq!(
        *actual_key,
        expected_vault,
        OmnindexError::InvalidVaultAccount
    );
    require_keys_eq!(
        token_account.owner,
        *vault_authority,
        OmnindexError::InvalidVaultAccount
    );
    require_keys_eq!(
        token_account.mint,
        *component_mint,
        OmnindexError::InvalidVaultAccount
    );
    Ok(())
}

pub fn validate_vault_spl_token_account(
    token_account: &SplTokenAccount,
    actual_key: &Pubkey,
    vault_authority: &Pubkey,
    component_mint: &Pubkey,
) -> Result<()> {
    let expected_vault = associated_token_address(vault_authority, component_mint);
    require_keys_eq!(
        *actual_key,
        expected_vault,
        OmnindexError::InvalidVaultAccount
    );
    require_keys_eq!(
        token_account.owner,
        *vault_authority,
        OmnindexError::InvalidVaultAccount
    );
    require_keys_eq!(
        token_account.mint,
        *component_mint,
        OmnindexError::InvalidVaultAccount
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interface_mint_reader_extracts_decimals_from_base_header() {
        let mut data = [0u8; SplMint::LEN];
        data[MINT_DECIMALS_OFFSET] = 6;
        data[MINT_INITIALIZED_OFFSET] = 1;

        let mint = interface_mint_info_from_data(&data).unwrap();

        assert_eq!(mint.decimals, 6);
    }

    #[test]
    fn interface_mint_reader_rejects_uninitialized_mint() {
        let data = [0u8; SplMint::LEN];

        assert!(interface_mint_info_from_data(&data).is_err());
    }
}
