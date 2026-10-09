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

use crate::errors::BasketError;

pub const ASSOCIATED_TOKEN_ID: Pubkey = pubkey!("ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL");
pub type SplMint = token::spl_token::state::Mint;
pub type SplTokenAccount = token::spl_token::state::Account;
const MINT_DECIMALS_OFFSET: usize = 44;
const MINT_INITIALIZED_OFFSET: usize = 45;
// A Token-2022 mint with extensions pads its base to a token account's length, then holds an
// account-type byte and the extensions as (type u16, length u16, value) entries.
const TOKEN_2022_ACCOUNT_TYPE_OFFSET: usize = SplTokenAccount::LEN;
const TOKEN_2022_MINT_ACCOUNT_TYPE: u8 = 1;
const TRANSFER_FEE_CONFIG_EXTENSION: u16 = 1;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct InterfaceMintInfo {
    pub decimals: u8,
}

pub fn require_remaining_account_pairs(actual: usize, component_count: usize) -> Result<()> {
    require!(
        actual == component_count * 2,
        BasketError::InvalidRemainingAccounts
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
        BasketError::InvalidAssociatedTokenProgram
    );
    require_keys_eq!(
        *token_program.key,
        token::ID,
        BasketError::InvalidTokenProgram
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
        BasketError::InvalidAssociatedTokenProgram
    );
    require!(
        *token_program.key == token::ID || *token_program.key == anchor_spl::token_2022::ID,
        BasketError::InvalidTokenProgram
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
    require_keys_eq!(*info.owner, token::ID, BasketError::InvalidTokenMint);
    let data = info.try_borrow_data()?;
    SplMint::unpack(&data).map_err(|_| error!(BasketError::InvalidTokenMint))
}

pub fn load_user_token_account(info: &AccountInfo) -> Result<SplTokenAccount> {
    require_keys_eq!(*info.owner, token::ID, BasketError::InvalidUserTokenAccount);
    let data = info.try_borrow_data()?;
    SplTokenAccount::unpack(&data).map_err(|_| error!(BasketError::InvalidUserTokenAccount))
}

pub fn load_interface_mint(info: &AccountInfo) -> Result<InterfaceMintInfo> {
    require!(
        *info.owner == token::ID || *info.owner == anchor_spl::token_2022::ID,
        BasketError::InvalidTokenMint
    );
    let data = info.try_borrow_data()?;
    interface_mint_info_from_data(&data)
}

fn interface_mint_info_from_data(data: &[u8]) -> Result<InterfaceMintInfo> {
    require!(data.len() >= SplMint::LEN, BasketError::InvalidTokenMint);
    require!(
        data[MINT_INITIALIZED_OFFSET] != 0,
        BasketError::InvalidTokenMint
    );

    Ok(InterfaceMintInfo {
        decimals: data[MINT_DECIMALS_OFFSET],
    })
}

/// Refuses a Token-2022 mint with the transfer-fee extension. A fee taken on the way into a
/// vault leaves the basket's books above what the vault holds, and the fee's authority can
/// raise it at any time, so only a mint without the extension is safe as a component.
pub fn require_no_transfer_fee(info: &AccountInfo) -> Result<()> {
    if *info.owner != anchor_spl::token_2022::ID {
        return Ok(());
    }
    let data = info.try_borrow_data()?;
    require!(
        !has_mint_extension(&data, TRANSFER_FEE_CONFIG_EXTENSION)?,
        BasketError::InvalidTokenMint
    );
    Ok(())
}

fn has_mint_extension(data: &[u8], extension_type: u16) -> Result<bool> {
    // A mint without extensions is just the base.
    if data.len() <= SplMint::LEN {
        return Ok(false);
    }
    require!(
        data.len() > TOKEN_2022_ACCOUNT_TYPE_OFFSET
            && data[TOKEN_2022_ACCOUNT_TYPE_OFFSET] == TOKEN_2022_MINT_ACCOUNT_TYPE,
        BasketError::InvalidTokenMint
    );
    let mut offset = TOKEN_2022_ACCOUNT_TYPE_OFFSET + 1;
    while offset + 4 <= data.len() {
        let kind = u16::from_le_bytes([data[offset], data[offset + 1]]);
        // Type 0 is uninitialized space: no extensions follow.
        if kind == 0 {
            break;
        }
        if kind == extension_type {
            return Ok(true);
        }
        let len = usize::from(u16::from_le_bytes([data[offset + 2], data[offset + 3]]));
        offset += 4 + len;
    }
    Ok(false)
}

pub fn load_interface_token_account(info: &AccountInfo) -> Result<InterfaceTokenAccount> {
    require!(
        *info.owner == token::ID || *info.owner == anchor_spl::token_2022::ID,
        BasketError::InvalidUserTokenAccount
    );
    let data = info.try_borrow_data()?;
    let mut data_ref: &[u8] = &data;
    InterfaceTokenAccount::try_deserialize_unchecked(&mut data_ref)
        .map_err(|_| error!(BasketError::InvalidUserTokenAccount))
}

pub fn validate_token_account_credit(
    before_amount: u64,
    after_amount: u64,
    expected_credit: u64,
) -> Result<()> {
    require!(
        after_amount >= before_amount,
        BasketError::ComponentTransferAmountMismatch
    );
    let actual_credit = after_amount - before_amount;
    require!(
        actual_credit == expected_credit,
        BasketError::ComponentTransferAmountMismatch
    );
    Ok(())
}

pub fn validate_user_token_account(
    token_account: &SplTokenAccount,
    expected_owner: &Pubkey,
    expected_mint: &Pubkey,
) -> Result<()> {
    require_keys_eq!(
        token_account.owner,
        *expected_owner,
        BasketError::InvalidUserTokenAccount
    );
    require_keys_eq!(
        token_account.mint,
        *expected_mint,
        BasketError::InvalidUserTokenAccount
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
        BasketError::InvalidVaultAccount
    );
    require_keys_eq!(
        token_account.owner,
        *vault_authority,
        BasketError::InvalidVaultAccount
    );
    require_keys_eq!(
        token_account.mint,
        *component_mint,
        BasketError::InvalidVaultAccount
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
        BasketError::InvalidVaultAccount
    );
    require_keys_eq!(
        token_account.owner,
        *vault_authority,
        BasketError::InvalidVaultAccount
    );
    require_keys_eq!(
        token_account.mint,
        *component_mint,
        BasketError::InvalidVaultAccount
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

    fn token_2022_mint(extensions: &[(u16, &[u8])]) -> Vec<u8> {
        let mut data = vec![0u8; TOKEN_2022_ACCOUNT_TYPE_OFFSET];
        data[MINT_INITIALIZED_OFFSET] = 1;
        data.push(TOKEN_2022_MINT_ACCOUNT_TYPE);
        for (kind, value) in extensions {
            data.extend_from_slice(&kind.to_le_bytes());
            data.extend_from_slice(&(value.len() as u16).to_le_bytes());
            data.extend_from_slice(value);
        }
        data
    }

    #[test]
    fn finds_the_transfer_fee_extension_among_others() {
        let metadata_pointer = (18u16, &[7u8; 64][..]);
        let transfer_fee = (TRANSFER_FEE_CONFIG_EXTENSION, &[0u8; 108][..]);
        let has_fee = |data: &[u8]| has_mint_extension(data, TRANSFER_FEE_CONFIG_EXTENSION).unwrap();

        assert!(!has_fee(&[0u8; SplMint::LEN]));
        assert!(!has_fee(&token_2022_mint(&[])));
        assert!(!has_fee(&token_2022_mint(&[metadata_pointer])));
        assert!(has_fee(&token_2022_mint(&[transfer_fee])));
        assert!(has_fee(&token_2022_mint(&[metadata_pointer, transfer_fee])));
        // Trailing uninitialized space ends the list.
        let mut padded = token_2022_mint(&[metadata_pointer]);
        padded.extend_from_slice(&[0u8; 16]);
        assert!(!has_fee(&padded));
    }

    #[test]
    fn rejects_extension_data_that_is_not_a_mint() {
        let mut data = token_2022_mint(&[]);
        data[TOKEN_2022_ACCOUNT_TYPE_OFFSET] = 2;
        assert!(has_mint_extension(&data, TRANSFER_FEE_CONFIG_EXTENSION).is_err());
    }

    #[test]
    fn token_credit_validation_accepts_exact_credit() {
        assert!(validate_token_account_credit(100, 125, 25).is_ok());
        assert!(validate_token_account_credit(100, 100, 0).is_ok());
    }

    #[test]
    fn token_credit_validation_rejects_short_or_extra_credit() {
        assert!(validate_token_account_credit(100, 124, 25).is_err());
        assert!(validate_token_account_credit(100, 126, 25).is_err());
        assert!(validate_token_account_credit(100, 99, 1).is_err());
    }
}
