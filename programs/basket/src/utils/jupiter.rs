use anchor_lang::{
    prelude::*,
    solana_program::{
        instruction::{AccountMeta, Instruction},
        program::invoke_signed,
    },
};

use crate::errors::BasketError;

pub const JUPITER_V6_ID: Pubkey = pubkey!("JUP6LkbZbjS1jKKwapdHNy74zcZ3tLUZoi5QNyVTaV4");
pub const JUPITER_V4_ID: Pubkey = pubkey!("JUP4Fb2cqiRUcaTHdrPC8h2gNsA2ETXiPDD33WcGuJB");

#[derive(AnchorSerialize, AnchorDeserialize, Clone, Debug)]
pub struct JupiterAccountMetaInput {
    pub account_index: u8,
    pub is_signer: bool,
    pub is_writable: bool,
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
}
