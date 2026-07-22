use anchor_lang::{
    prelude::*,
    solana_program::{
        instruction::{AccountMeta, Instruction},
        program::invoke_signed,
    },
};
use anchor_spl::{token, token_2022};

use crate::errors::BasketError;

use super::token_accounts::load_interface_token_account;

pub const JUPITER_V6_ID: Pubkey = pubkey!("JUP6LkbZbjS1jKKwapdHNy74zcZ3tLUZoi5QNyVTaV4");
pub const JUPITER_V4_ID: Pubkey = pubkey!("JUP4Fb2cqiRUcaTHdrPC8h2gNsA2ETXiPDD33WcGuJB");

#[derive(AnchorSerialize, AnchorDeserialize, Clone, Debug)]
pub struct JupiterAccountMetaInput {
    pub account_index: u8,
    pub is_signer: bool,
    pub is_writable: bool,
}

// Bit-packed account meta: index in bits 0-5 (so index < 64), is_signer in bit 6,
// is_writable in bit 7. Lets the large-basket compact swap encode each route account
// in 1 byte instead of 3 — saving ~50 B/swap so more swaps fit per batched tx.
pub const PACKED_META_INDEX_MASK: u8 = 0b0011_1111;
pub const PACKED_META_SIGNER_BIT: u8 = 0b0100_0000;
pub const PACKED_META_WRITABLE_BIT: u8 = 0b1000_0000;

pub fn unpack_account_metas(packed: &[u8]) -> Vec<JupiterAccountMetaInput> {
    packed
        .iter()
        .map(|&byte| JupiterAccountMetaInput {
            account_index: byte & PACKED_META_INDEX_MASK,
            is_signer: byte & PACKED_META_SIGNER_BIT != 0,
            is_writable: byte & PACKED_META_WRITABLE_BIT != 0,
        })
        .collect()
}

// Large-basket swap plan: Jupiter route data + the route's account references packed
// 1 byte each (see unpack_account_metas). Isolated from the regular-mint
// JupiterCompactSwapPlan so that path is unaffected.
#[derive(AnchorSerialize, AnchorDeserialize, Clone, Debug)]
pub struct LargeBasketSwapPlan {
    pub instruction_data: Vec<u8>,
    pub accounts: Vec<u8>,
}

pub struct JupiterInvokeScratch<'info> {
    metas: Vec<AccountMeta>,
    infos: Vec<AccountInfo<'info>>,
    data: Vec<u8>,
}

impl<'info> JupiterInvokeScratch<'info> {
    pub fn new() -> Self {
        Self {
            metas: Vec::new(),
            infos: Vec::new(),
            data: Vec::new(),
        }
    }
}

impl<'info> Default for JupiterInvokeScratch<'info> {
    fn default() -> Self {
        Self::new()
    }
}

pub fn validate_jupiter_program(program: &Pubkey) -> Result<()> {
    require!(
        *program == JUPITER_V6_ID || *program == JUPITER_V4_ID,
        BasketError::InvalidJupiterProgram
    );
    Ok(())
}

pub fn validate_jupiter_route_account_scope(
    account_candidates: &[AccountInfo<'_>],
    account_metas: &[JupiterAccountMetaInput],
    protected_accounts: &[Pubkey],
    allowed_protected_accounts: &[Pubkey],
) -> Result<()> {
    for meta in account_metas {
        let pubkey = account_candidates
            .get(meta.account_index as usize)
            .ok_or_else(|| error!(BasketError::InvalidJupiterRoute))?
            .key();

        if protected_accounts.contains(&pubkey) && !allowed_protected_accounts.contains(&pubkey) {
            return err!(BasketError::InvalidJupiterRoute);
        }
    }

    Ok(())
}

pub fn validate_vault_authority_token_account_scope(
    account_candidates: &[AccountInfo<'_>],
    account_metas: &[JupiterAccountMetaInput],
    vault_authority: Pubkey,
    allowed_vault_authority_token_accounts: &[Pubkey],
) -> Result<()> {
    for meta in account_metas {
        let info = account_candidates
            .get(meta.account_index as usize)
            .ok_or_else(|| error!(BasketError::InvalidJupiterRoute))?;

        if allowed_vault_authority_token_accounts.contains(info.key) {
            continue;
        }
        if *info.owner != token::ID && *info.owner != token_2022::ID {
            continue;
        }

        let Ok(token_account) = load_interface_token_account(info) else {
            continue;
        };
        if token_account.owner == vault_authority {
            return err!(BasketError::InvalidJupiterRoute);
        }
    }

    Ok(())
}

#[cfg(test)]
fn validate_jupiter_route_account_key_scope(
    account_keys: &[Pubkey],
    account_metas: &[JupiterAccountMetaInput],
    protected_accounts: &[Pubkey],
    allowed_protected_accounts: &[Pubkey],
) -> Result<()> {
    for meta in account_metas {
        let pubkey = account_keys
            .get(meta.account_index as usize)
            .ok_or_else(|| error!(BasketError::InvalidJupiterRoute))?;

        if protected_accounts.contains(pubkey) && !allowed_protected_accounts.contains(pubkey) {
            return err!(BasketError::InvalidJupiterRoute);
        }
    }

    Ok(())
}

pub fn invoke_jupiter_swap<'info>(
    jupiter_program: AccountInfo<'info>,
    account_candidates: &[AccountInfo<'info>],
    account_metas: &[JupiterAccountMetaInput],
    instruction_data: &[u8],
    signer: Option<Pubkey>,
    signer_seeds: &[&[&[u8]]],
) -> Result<()> {
    let mut scratch = JupiterInvokeScratch::new();
    invoke_jupiter_swap_with_scratch(
        jupiter_program,
        account_candidates,
        account_metas,
        instruction_data,
        signer,
        signer_seeds,
        &mut scratch,
    )
}

pub fn invoke_jupiter_swap_with_scratch<'info>(
    jupiter_program: AccountInfo<'info>,
    account_candidates: &[AccountInfo<'info>],
    account_metas: &[JupiterAccountMetaInput],
    instruction_data: &[u8],
    signer: Option<Pubkey>,
    signer_seeds: &[&[&[u8]]],
    scratch: &mut JupiterInvokeScratch<'info>,
) -> Result<()> {
    validate_jupiter_program(jupiter_program.key)?;
    require!(
        !instruction_data.is_empty(),
        BasketError::InvalidJupiterRoute
    );
    require!(!account_metas.is_empty(), BasketError::InvalidJupiterRoute);

    scratch.metas.clear();
    scratch.infos.clear();
    scratch.data.clear();
    scratch.metas.reserve(account_metas.len());
    scratch.infos.reserve(account_metas.len() + 1);
    scratch.data.reserve(instruction_data.len());

    for meta in account_metas {
        let info = account_candidates
            .get(meta.account_index as usize)
            .ok_or_else(|| error!(BasketError::InvalidJupiterRoute))?;
        let pubkey = info.key();

        if meta.is_signer {
            require!(
                Some(pubkey) == signer || info.is_signer,
                BasketError::InvalidJupiterRoute
            );
        }

        scratch.metas.push(if meta.is_writable {
            AccountMeta::new(pubkey, meta.is_signer)
        } else {
            AccountMeta::new_readonly(pubkey, meta.is_signer)
        });
        scratch.infos.push(info.clone());
    }

    scratch.infos.push(jupiter_program.clone());
    scratch.data.extend_from_slice(instruction_data);

    let instruction = Instruction {
        program_id: jupiter_program.key(),
        accounts: std::mem::take(&mut scratch.metas),
        data: std::mem::take(&mut scratch.data),
    };
    let result = invoke_signed(&instruction, &scratch.infos, signer_seeds).map_err(Into::into);

    scratch.metas = instruction.accounts;
    scratch.data = instruction.data;
    scratch.metas.clear();
    scratch.infos.clear();
    scratch.data.clear();

    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use anchor_lang::solana_program::{program_option::COption, program_pack::Pack};
    use anchor_spl::token::spl_token::state::{
        Account as SplTokenAccount, AccountState as SplTokenAccountState,
    };

    fn meta(account_index: u8) -> JupiterAccountMetaInput {
        JupiterAccountMetaInput {
            account_index,
            is_signer: false,
            is_writable: true,
        }
    }

    #[test]
    fn route_scope_rejects_unallowed_protected_accounts() {
        let source = Pubkey::new_unique();
        let destination = Pubkey::new_unique();
        let unrelated_vault = Pubkey::new_unique();
        let account_keys = [source, destination, unrelated_vault];
        let result = validate_jupiter_route_account_key_scope(
            &account_keys,
            &[meta(0), meta(2), meta(1)],
            &[source, destination, unrelated_vault],
            &[source, destination],
        );

        assert!(result.is_err());
    }

    #[test]
    fn route_scope_allows_declared_route_endpoints() {
        let source = Pubkey::new_unique();
        let destination = Pubkey::new_unique();
        let unrelated = Pubkey::new_unique();
        let account_keys = [source, destination, unrelated];
        let result = validate_jupiter_route_account_key_scope(
            &account_keys,
            &[meta(0), meta(1), meta(2)],
            &[source, destination],
            &[source, destination],
        );

        assert!(result.is_ok());
    }

    fn token_account_data(mint: Pubkey, owner: Pubkey, amount: u64) -> Vec<u8> {
        let token_account = SplTokenAccount {
            mint,
            owner,
            amount,
            delegate: COption::None,
            state: SplTokenAccountState::Initialized,
            is_native: COption::None,
            delegated_amount: 0,
            close_authority: COption::None,
        };
        let mut data = vec![0u8; SplTokenAccount::LEN];
        SplTokenAccount::pack(token_account, &mut data).unwrap();
        data
    }

    #[test]
    fn vault_authority_scope_rejects_unlisted_vault_owned_token_account() {
        let vault_authority = Pubkey::new_unique();
        let allowed_key = Pubkey::new_unique();
        let blocked_key = Pubkey::new_unique();
        let mint = Pubkey::new_unique();
        let mut allowed_lamports = 0;
        let mut blocked_lamports = 0;
        let mut allowed_data = token_account_data(mint, vault_authority, 1);
        let mut blocked_data = token_account_data(mint, vault_authority, 1);
        let token_program = token::ID;
        let allowed_info = AccountInfo::new(
            &allowed_key,
            false,
            true,
            &mut allowed_lamports,
            &mut allowed_data,
            &token_program,
            false,
            0,
        );
        let blocked_info = AccountInfo::new(
            &blocked_key,
            false,
            true,
            &mut blocked_lamports,
            &mut blocked_data,
            &token_program,
            false,
            0,
        );
        let candidates = [allowed_info, blocked_info];

        let result = validate_vault_authority_token_account_scope(
            &candidates,
            &[meta(0), meta(1)],
            vault_authority,
            &[allowed_key],
        );

        assert!(result.is_err());
    }

    #[test]
    fn vault_authority_scope_allows_listed_or_user_owned_token_accounts() {
        let vault_authority = Pubkey::new_unique();
        let user = Pubkey::new_unique();
        let allowed_key = Pubkey::new_unique();
        let user_key = Pubkey::new_unique();
        let mint = Pubkey::new_unique();
        let mut allowed_lamports = 0;
        let mut user_lamports = 0;
        let mut allowed_data = token_account_data(mint, vault_authority, 1);
        let mut user_data = token_account_data(mint, user, 1);
        let token_program = token::ID;
        let allowed_info = AccountInfo::new(
            &allowed_key,
            false,
            true,
            &mut allowed_lamports,
            &mut allowed_data,
            &token_program,
            false,
            0,
        );
        let user_info = AccountInfo::new(
            &user_key,
            false,
            true,
            &mut user_lamports,
            &mut user_data,
            &token_program,
            false,
            0,
        );
        let candidates = [allowed_info, user_info];

        let result = validate_vault_authority_token_account_scope(
            &candidates,
            &[meta(0), meta(1)],
            vault_authority,
            &[allowed_key],
        );

        assert!(result.is_ok());
    }
}
