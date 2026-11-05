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
// A token account's state byte (SPL Token and Token-2022 share the base layout): after the
// mint, owner, amount and the delegate COption.
const TOKEN_ACCOUNT_STATE_OFFSET: usize = 108;
const ACCOUNT_STATE_INITIALIZED: u8 = 1;
const ACCOUNT_STATE_FROZEN: u8 = 2;
// A Token-2022 mint with extensions pads its base to a token account's length, then holds an
// account-type byte and the extensions as (type u16, length u16, value) entries.
const TOKEN_2022_ACCOUNT_TYPE_OFFSET: usize = SplTokenAccount::LEN;
const TOKEN_2022_ACCOUNT_TYPE_ACCOUNT: u8 = 2;
const TOKEN_2022_MINT_ACCOUNT_TYPE: u8 = 1;
const TRANSFER_FEE_CONFIG_EXTENSION: u16 = 1;
const CONFIDENTIAL_TRANSFER_MINT_EXTENSION: u16 = 4;
const DEFAULT_ACCOUNT_STATE_EXTENSION: u16 = 6;
const PERMANENT_DELEGATE_EXTENSION: u16 = 12;
const TRANSFER_HOOK_EXTENSION: u16 = 14;
// MetadataPointer, TokenMetadata, GroupPointer, TokenGroup, GroupMemberPointer and
// TokenGroupMember are numbered 18 to 23. None of them affects transfers.
const METADATA_POINTER_EXTENSION: u16 = 18;
const TOKEN_GROUP_MEMBER_EXTENSION: u16 = 23;
const SCALED_UI_AMOUNT_EXTENSION: u16 = 25;
// PausableConfig: the pause authority (32 bytes), then whether the mint is paused.
const PAUSABLE_EXTENSION: u16 = 26;
const PAUSABLE_PAUSED_OFFSET: usize = 32;
// TransferHook: its authority (32 bytes), then the hook program (zero when unset).
const TRANSFER_HOOK_PROGRAM: core::ops::Range<usize> = 32..64;

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

/// Refuses a component a community basket's books can't follow. Issuer controls are allowed:
/// a freeze authority, a pause switch, a permanent delegate, a transfer-hook authority. If one
/// of them stops a component from moving, a redeemer skips that component and still takes
/// everything else (skip_large_basket_redeem_component, cancel_expired_large_basket_intent),
/// and rebalances leave it out, so no issuer can trap the rest of a basket.
///
/// Still refused: a token already paused or with a transfer-hook program set (it could not
/// enter a vault), token accounts that start frozen, non-transferable tokens, interest-bearing
/// amounts, confidential mint/burn and transfer fees, a mint close authority (a closed mint can
/// come back with different rules), and any extension this program doesn't know. Transfer fees
/// are refused for every basket (require_no_transfer_fee).
pub fn require_community_component_mint(info: &AccountInfo) -> Result<()> {
    require!(
        *info.owner == token::ID || *info.owner == anchor_spl::token_2022::ID,
        BasketError::CommunityTokenNotAllowed
    );
    let data = info.try_borrow_data()?;
    require!(
        community_extensions_allowed(&data)?,
        BasketError::CommunityTokenNotAllowed
    );
    Ok(())
}

fn community_extensions_allowed(data: &[u8]) -> Result<bool> {
    Ok(mint_extensions(data)?.iter().all(|(kind, value)| match *kind {
        CONFIDENTIAL_TRANSFER_MINT_EXTENSION
        | PERMANENT_DELEGATE_EXTENSION
        | METADATA_POINTER_EXTENSION..=TOKEN_GROUP_MEMBER_EXTENSION
        | SCALED_UI_AMOUNT_EXTENSION => true,
        DEFAULT_ACCOUNT_STATE_EXTENSION => value.first() == Some(&ACCOUNT_STATE_INITIALIZED),
        // TransferHook { authority, program_id }: the program must be unset.
        TRANSFER_HOOK_EXTENSION => {
            value.len() == 64 && value[TRANSFER_HOOK_PROGRAM].iter().all(|byte| *byte == 0)
        }
        PAUSABLE_EXTENSION => value.get(PAUSABLE_PAUSED_OFFSET) == Some(&0),
        _ => false,
    }))
}

/// Why a component's vault can't pay out right now, judged from chain state alone, never from
/// the caller's word.
#[derive(AnchorSerialize, AnchorDeserialize, Clone, Copy, Debug, PartialEq, Eq)]
pub enum ComponentBlock {
    /// The vault token account is frozen.
    VaultFrozen,
    /// The mint is paused (Token-2022 Pausable).
    MintPaused,
    /// The mint has a transfer-hook program. This program's transfers don't carry a hook's
    /// accounts, so every transfer of the mint fails.
    TransferHookSet,
    /// The vault holds less than is owed (someone moved tokens out, e.g. a permanent delegate).
    VaultShort,
    /// A refund has nowhere to go: the refund escrow's account for the token is frozen, and
    /// the mint makes every new account frozen, so no account of the owner's can take it.
    RefundBlocked,
}

/// Whether a token account is frozen. Only a token account of SPL Token or Token-2022 counts:
/// any other account (one whose byte 108 merely reads 2) is refused.
pub fn token_account_frozen(info: &AccountInfo) -> Result<bool> {
    require!(
        *info.owner == token::ID || *info.owner == anchor_spl::token_2022::ID,
        BasketError::InvalidUserTokenAccount
    );
    token_account_frozen_from_data(&info.try_borrow_data()?)
}

fn token_account_frozen_from_data(data: &[u8]) -> Result<bool> {
    // A token account is the 165-byte base, then (Token-2022 with extensions) the account type.
    require!(
        data.len() == SplTokenAccount::LEN
            || (data.len() > SplTokenAccount::LEN
                && data[TOKEN_2022_ACCOUNT_TYPE_OFFSET] == TOKEN_2022_ACCOUNT_TYPE_ACCOUNT),
        BasketError::InvalidUserTokenAccount
    );
    Ok(data[TOKEN_ACCOUNT_STATE_OFFSET] == ACCOUNT_STATE_FROZEN)
}

/// Whether every new account of this mint starts frozen (Token-2022 DefaultAccountState).
pub fn mint_default_frozen(info: &AccountInfo) -> Result<bool> {
    if *info.owner != anchor_spl::token_2022::ID {
        return Ok(false);
    }
    mint_default_frozen_from_data(&info.try_borrow_data()?)
}

fn mint_default_frozen_from_data(data: &[u8]) -> Result<bool> {
    Ok(mint_extensions(data)?.iter().any(|(kind, value)| {
        *kind == DEFAULT_ACCOUNT_STATE_EXTENSION && value.first() == Some(&ACCOUNT_STATE_FROZEN)
    }))
}

/// Whether a refund of `mint` can't be paid anywhere: `escrow_account` (already checked to be
/// the refund escrow's own account for the mint) is frozen, and any account the owner could
/// open for it would start frozen too.
pub fn refund_has_nowhere_to_go(mint: &AccountInfo, escrow_account: &AccountInfo) -> Result<bool> {
    Ok(token_account_frozen(escrow_account)? && mint_default_frozen(mint)?)
}

/// Whether every transfer of this mint fails right now: it is paused, or it has a
/// transfer-hook program set.
pub fn mint_transfers_blocked(info: &AccountInfo) -> Result<Option<ComponentBlock>> {
    if *info.owner != anchor_spl::token_2022::ID {
        return Ok(None);
    }
    mint_transfers_blocked_from_data(&info.try_borrow_data()?)
}

fn mint_transfers_blocked_from_data(data: &[u8]) -> Result<Option<ComponentBlock>> {
    for (kind, value) in mint_extensions(data)? {
        match kind {
            PAUSABLE_EXTENSION if value.get(PAUSABLE_PAUSED_OFFSET).is_some_and(|b| *b != 0) => {
                return Ok(Some(ComponentBlock::MintPaused));
            }
            TRANSFER_HOOK_EXTENSION
                if value
                    .get(TRANSFER_HOOK_PROGRAM)
                    .is_some_and(|program| program.iter().any(|b| *b != 0)) =>
            {
                return Ok(Some(ComponentBlock::TransferHookSet));
            }
            _ => {}
        }
    }
    Ok(None)
}

/// Why `vault` can't pay `owed` of `mint` now, if it can't. The caller has already matched
/// `mint` and `vault` to the component.
pub fn component_payout_blocked(
    mint: &AccountInfo,
    vault: &AccountInfo,
    owed: u64,
) -> Result<Option<ComponentBlock>> {
    if token_account_frozen(vault)? {
        return Ok(Some(ComponentBlock::VaultFrozen));
    }
    if let Some(block) = mint_transfers_blocked(mint)? {
        return Ok(Some(block));
    }
    if load_interface_token_account(vault)?.amount < owed {
        return Ok(Some(ComponentBlock::VaultShort));
    }
    Ok(None)
}

/// How much of a forfeited component can go back into the basket's books: the owed amount, but
/// never more than the vault holds beyond what is already booked, so a drained vault can't
/// leave the books above its balance.
pub fn forfeit_restorable(owed: u64, vault_amount: u64, accounted_reserve: u64) -> u64 {
    owed.min(vault_amount.saturating_sub(accounted_reserve))
}

fn has_mint_extension(data: &[u8], extension_type: u16) -> Result<bool> {
    Ok(mint_extensions(data)?
        .iter()
        .any(|(kind, _)| *kind == extension_type))
}

/// A Token-2022 mint's extensions as (type, value) pairs; none for a mint that is just the base.
fn mint_extensions(data: &[u8]) -> Result<Vec<(u16, &[u8])>> {
    let mut extensions = Vec::new();
    if data.len() <= SplMint::LEN {
        return Ok(extensions);
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
        let len = usize::from(u16::from_le_bytes([data[offset + 2], data[offset + 3]]));
        let value = data
            .get(offset + 4..offset + 4 + len)
            .ok_or_else(|| error!(BasketError::InvalidTokenMint))?;
        extensions.push((kind, value));
        offset += 4 + len;
    }
    Ok(extensions)
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

    /// An xStock's extensions as they are on mainnet: metadata, permanent delegate, accounts
    /// that start initialized, scaled UI amount, an unpaused pause switch, confidential
    /// transfers and a hook authority with no hook program.
    fn xstock_extensions() -> Vec<(u16, Vec<u8>)> {
        let mut hook = vec![0u8; 64];
        hook[..32].fill(5);
        let mut pausable = vec![0u8; 33];
        pausable[..32].fill(6);
        vec![
            (18, vec![7u8; 64]),
            (PERMANENT_DELEGATE_EXTENSION, vec![5u8; 32]),
            (DEFAULT_ACCOUNT_STATE_EXTENSION, vec![ACCOUNT_STATE_INITIALIZED]),
            (SCALED_UI_AMOUNT_EXTENSION, vec![0u8; 56]),
            (PAUSABLE_EXTENSION, pausable),
            (CONFIDENTIAL_TRANSFER_MINT_EXTENSION, vec![0u8; 65]),
            (TRANSFER_HOOK_EXTENSION, hook),
            (19, vec![1u8; 90]),
        ]
    }

    fn mint_of(extensions: &[(u16, Vec<u8>)]) -> Vec<u8> {
        let refs: Vec<(u16, &[u8])> = extensions.iter().map(|(k, v)| (*k, v.as_slice())).collect();
        token_2022_mint(&refs)
    }

    #[test]
    fn community_baskets_accept_issuer_controls_but_not_what_the_books_cant_follow() {
        let allowed = |extensions: &[(u16, &[u8])]| {
            community_extensions_allowed(&token_2022_mint(extensions)).unwrap()
        };
        assert!(allowed(&[]));
        assert!(allowed(&[(18, &[7u8; 64]), (19, &[1u8; 90]), (23, &[0u8; 72])]));
        assert!(community_extensions_allowed(&mint_of(&xstock_extensions())).unwrap());

        let mut hook_with_program = [0u8; 64];
        hook_with_program[40] = 9;
        assert!(!allowed(&[(TRANSFER_HOOK_EXTENSION, &hook_with_program)]));
        let mut paused = [0u8; 33];
        paused[PAUSABLE_PAUSED_OFFSET] = 1;
        assert!(!allowed(&[(PAUSABLE_EXTENSION, &paused)]));
        assert!(!allowed(&[(DEFAULT_ACCOUNT_STATE_EXTENSION, &[ACCOUNT_STATE_FROZEN])]));
        // Transfer fee, close authority, non-transferable, interest, confidential fee,
        // confidential mint/burn, and an unknown type.
        for refused in [1u16, 3, 9, 10, 16, 24, 27, 99] {
            assert!(!allowed(&[(18, &[7u8; 64]), (refused, &[0u8; 8])]), "type {refused}");
        }
    }

    #[test]
    fn a_paused_mint_or_a_set_hook_program_blocks_every_transfer() {
        let blocked = |extensions: &[(u16, Vec<u8>)]| {
            mint_transfers_blocked_from_data(&mint_of(extensions)).unwrap()
        };
        assert_eq!(blocked(&xstock_extensions()), None);
        assert_eq!(mint_transfers_blocked_from_data(&[0u8; SplMint::LEN]).unwrap(), None);

        let mut paused = xstock_extensions();
        paused[4].1[PAUSABLE_PAUSED_OFFSET] = 1;
        assert_eq!(blocked(&paused), Some(ComponentBlock::MintPaused));

        let mut hooked = xstock_extensions();
        hooked[6].1[50] = 3;
        assert_eq!(blocked(&hooked), Some(ComponentBlock::TransferHookSet));
    }

    #[test]
    fn reads_whether_a_token_account_is_frozen() {
        let mut data = [0u8; SplTokenAccount::LEN];
        data[TOKEN_ACCOUNT_STATE_OFFSET] = ACCOUNT_STATE_INITIALIZED;
        assert!(!token_account_frozen_from_data(&data).unwrap());
        data[TOKEN_ACCOUNT_STATE_OFFSET] = ACCOUNT_STATE_FROZEN;
        assert!(token_account_frozen_from_data(&data).unwrap());
        assert!(token_account_frozen_from_data(&data[..100]).is_err());
        // A Token-2022 account with extensions carries its account type after the base.
        let mut extended = data.to_vec();
        extended.extend_from_slice(&[TOKEN_2022_ACCOUNT_TYPE_ACCOUNT, 7, 0, 0, 0]);
        assert!(token_account_frozen_from_data(&extended).unwrap());
        // A mint (account type 1) is not a token account, whatever its byte 108 says.
        extended[TOKEN_2022_ACCOUNT_TYPE_OFFSET] = TOKEN_2022_MINT_ACCOUNT_TYPE;
        assert!(token_account_frozen_from_data(&extended).is_err());
    }

    #[test]
    fn reads_whether_new_accounts_of_a_mint_start_frozen() {
        let frozen = token_2022_mint(&[(DEFAULT_ACCOUNT_STATE_EXTENSION, &[ACCOUNT_STATE_FROZEN])]);
        assert!(mint_default_frozen_from_data(&frozen).unwrap());
        let initialized = token_2022_mint(&[(DEFAULT_ACCOUNT_STATE_EXTENSION, &[ACCOUNT_STATE_INITIALIZED])]);
        assert!(!mint_default_frozen_from_data(&initialized).unwrap());
        assert!(!mint_default_frozen_from_data(&token_2022_mint(&[])).unwrap());
        assert!(!mint_default_frozen_from_data(&[0u8; SplMint::LEN]).unwrap());
    }

    #[test]
    fn a_forfeit_restores_no_more_than_the_vault_holds_beyond_the_books() {
        // Frozen but whole: the vault holds the books plus what was owed.
        assert_eq!(forfeit_restorable(40, 140, 100), 40);
        // Some of it was taken: only what is left goes back.
        assert_eq!(forfeit_restorable(40, 120, 100), 20);
        // Drained below the books: nothing goes back.
        assert_eq!(forfeit_restorable(40, 90, 100), 0);
        assert_eq!(forfeit_restorable(0, 140, 100), 0);
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
