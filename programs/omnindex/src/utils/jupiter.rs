use anchor_lang::{
    prelude::*,
    solana_program::{
        instruction::{AccountMeta, Instruction},
        program::invoke_signed,
    },
};

use crate::errors::OmnindexError;

pub const JUPITER_V6_ID: Pubkey = pubkey!("JUP6LkbZbjS1jKKwapdHNy74zcZ3tLUZoi5QNyVTaV4");
pub const JUPITER_V4_ID: Pubkey = pubkey!("JUP4Fb2cqiRUcaTHdrPC8h2gNsA2ETXiPDD33WcGuJB");

#[derive(AnchorSerialize, AnchorDeserialize, Clone, Debug)]
pub struct JupiterAccountMetaInput {
    pub pubkey: Pubkey,
    pub is_signer: bool,
    pub is_writable: bool,
}

pub fn validate_jupiter_program(program: &Pubkey) -> Result<()> {
    require!(
        *program == JUPITER_V6_ID || *program == JUPITER_V4_ID,
        OmnindexError::InvalidJupiterProgram
    );
    Ok(())
}

pub fn validate_jupiter_route_account_scope(
    account_metas: &[JupiterAccountMetaInput],
    protected_accounts: &[Pubkey],
    allowed_protected_accounts: &[Pubkey],
) -> Result<()> {
    for meta in account_metas {
        if protected_accounts
            .iter()
            .any(|account| *account == meta.pubkey)
            && !allowed_protected_accounts
                .iter()
                .any(|account| *account == meta.pubkey)
        {
            return err!(OmnindexError::InvalidJupiterRoute);
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
    validate_jupiter_program(jupiter_program.key)?;
    require!(
        !instruction_data.is_empty(),
        OmnindexError::InvalidJupiterRoute
    );
    require!(
        !account_metas.is_empty(),
        OmnindexError::InvalidJupiterRoute
    );

    let mut metas = Vec::with_capacity(account_metas.len());
    let mut infos = Vec::with_capacity(account_metas.len() + 1);

    for meta in account_metas {
        if meta.is_signer {
            require!(
                Some(meta.pubkey) == signer
                    || account_candidates
                        .iter()
                        .any(|info| info.key() == meta.pubkey && info.is_signer),
                OmnindexError::InvalidJupiterRoute
            );
        }

        let info = account_candidates
            .iter()
            .find(|info| info.key() == meta.pubkey)
            .ok_or_else(|| error!(OmnindexError::InvalidJupiterRoute))?;

        metas.push(if meta.is_writable {
            AccountMeta::new(meta.pubkey, meta.is_signer)
        } else {
            AccountMeta::new_readonly(meta.pubkey, meta.is_signer)
        });
        infos.push(info.clone());
    }

    infos.push(jupiter_program.clone());

    invoke_signed(
        &Instruction {
            program_id: jupiter_program.key(),
            accounts: metas,
            data: instruction_data.to_vec(),
        },
        &infos,
        signer_seeds,
    )
    .map_err(Into::into)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn meta(pubkey: Pubkey) -> JupiterAccountMetaInput {
        JupiterAccountMetaInput {
            pubkey,
            is_signer: false,
            is_writable: true,
        }
    }

    #[test]
    fn route_scope_rejects_unallowed_protected_accounts() {
        let source = Pubkey::new_unique();
        let destination = Pubkey::new_unique();
        let unrelated_vault = Pubkey::new_unique();
        let result = validate_jupiter_route_account_scope(
            &[meta(source), meta(unrelated_vault), meta(destination)],
            &[source, destination, unrelated_vault],
            &[source, destination],
        );

        assert!(result.is_err());
    }

    #[test]
    fn route_scope_allows_declared_route_endpoints() {
        let source = Pubkey::new_unique();
        let destination = Pubkey::new_unique();
        let result = validate_jupiter_route_account_scope(
            &[meta(source), meta(destination), meta(Pubkey::new_unique())],
            &[source, destination],
            &[source, destination],
        );

        assert!(result.is_ok());
    }
}
