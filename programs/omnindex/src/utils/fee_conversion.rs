use anchor_lang::prelude::*;
use anchor_spl::token::{self, TokenAccount, Transfer};

use crate::{constants::USDC_MINT, errors::OmnindexError};

use super::{
    omnipair::{
        load_pair, load_rate_model, quote_exact_output_for_pair_input, reserve_vault_address, swap,
        FutarchyAuthority, SwapAccounts, SwapArgs,
    },
    token_accounts::{
        associated_token_address, create_associated_token_account_idempotent, load_mint,
        load_user_token_account, validate_user_token_account,
    },
};

pub struct FeeConversionSwap<'info> {
    pub payer: AccountInfo<'info>,
    pub associated_token_program: AccountInfo<'info>,
    pub system_program: AccountInfo<'info>,
    pub omnipair_program: AccountInfo<'info>,
    pub omnipair_futarchy_authority: AccountInfo<'info>,
    pub omnipair_event_authority: AccountInfo<'info>,
    pub source_mint_info: AccountInfo<'info>,
    pub funding_token_account: AccountInfo<'info>,
    pub funding_authority: AccountInfo<'info>,
    pub staking_authority: AccountInfo<'info>,
    pub staking_reward_vault: AccountInfo<'info>,
    pub token_program: AccountInfo<'info>,
    pub token_2022_program: AccountInfo<'info>,
}

pub fn swap_fee_to_usdc<'info>(
    remaining: &mut std::slice::Iter<'info, AccountInfo<'info>>,
    futarchy_authority: &FutarchyAuthority,
    source_mint: &Pubkey,
    amount_in: u64,
    accounts: FeeConversionSwap<'info>,
    funding_signer_seeds: &[&[&[u8]]],
    staking_authority_signer_seeds: &[&[&[u8]]],
) -> Result<u64> {
    require_keys_eq!(
        accounts.source_mint_info.key(),
        *source_mint,
        OmnindexError::InvalidQuoteMint
    );

    let staking_source_token_account_info = next_account_info(remaining)?;
    let usdc_mint_info = next_account_info(remaining)?;
    let pair_info = next_account_info(remaining)?;
    let rate_model_info = next_account_info(remaining)?;
    let source_reserve_vault_info = next_account_info(remaining)?;
    let usdc_reserve_vault_info = next_account_info(remaining)?;

    require_keys_eq!(
        usdc_mint_info.key(),
        USDC_MINT,
        OmnindexError::InvalidRewardMint
    );
    load_mint(usdc_mint_info)?;

    let expected_staking_source_token_account =
        associated_token_address(accounts.staking_authority.key, source_mint);
    require_keys_eq!(
        staking_source_token_account_info.key(),
        expected_staking_source_token_account,
        OmnindexError::InvalidStakingVault
    );
    create_associated_token_account_idempotent(
        accounts.associated_token_program.clone(),
        accounts.payer.clone(),
        staking_source_token_account_info.clone(),
        accounts.staking_authority.clone(),
        accounts.source_mint_info.clone(),
        accounts.system_program.clone(),
        accounts.token_program.clone(),
    )?;
    let staking_source_token_account = load_user_token_account(staking_source_token_account_info)?;
    validate_user_token_account(
        &staking_source_token_account,
        accounts.staking_authority.key,
        source_mint,
    )?;

    let pair = load_pair(pair_info)?;
    let rate_model = load_rate_model(rate_model_info)?;
    require_keys_eq!(
        rate_model_info.key(),
        pair.rate_model,
        OmnindexError::InvalidOmnipairRateModel
    );
    require!(
        (pair.token0 == *source_mint && pair.token1 == USDC_MINT)
            || (pair.token1 == *source_mint && pair.token0 == USDC_MINT),
        OmnindexError::InvalidOmnipairPair
    );

    validate_omnipair_reserve(pair_info.key(), source_mint, source_reserve_vault_info)?;
    validate_omnipair_reserve(pair_info.key(), &USDC_MINT, usdc_reserve_vault_info)?;

    let usdc_out = quote_exact_output_for_pair_input(
        &pair,
        &rate_model,
        futarchy_authority,
        source_mint,
        &USDC_MINT,
        amount_in,
    )?;
    require!(usdc_out > 0, OmnindexError::FeeConversionOutputTooSmall);

    token::transfer(
        CpiContext::new_with_signer(
            accounts.token_program.clone(),
            Transfer {
                from: accounts.funding_token_account,
                to: staking_source_token_account_info.clone(),
                authority: accounts.funding_authority,
            },
            funding_signer_seeds,
        ),
        amount_in,
    )?;

    swap(
        accounts.omnipair_program,
        SwapAccounts {
            pair: pair_info.clone(),
            rate_model: rate_model_info.clone(),
            futarchy_authority: accounts.omnipair_futarchy_authority,
            token_in_vault: source_reserve_vault_info.clone(),
            token_out_vault: usdc_reserve_vault_info.clone(),
            user_token_in_account: staking_source_token_account_info.clone(),
            user_token_out_account: accounts.staking_reward_vault,
            token_in_mint: accounts.source_mint_info,
            token_out_mint: usdc_mint_info.clone(),
            user: accounts.staking_authority,
            token_program: accounts.token_program,
            token_2022_program: accounts.token_2022_program,
            event_authority: accounts.omnipair_event_authority,
        },
        SwapArgs {
            amount_in,
            min_amount_out: usdc_out,
        },
        staking_authority_signer_seeds,
    )?;

    Ok(usdc_out)
}

fn validate_omnipair_reserve<'info>(
    pair: Pubkey,
    mint: &Pubkey,
    reserve_vault_info: &'info AccountInfo<'info>,
) -> Result<()> {
    require_keys_eq!(
        reserve_vault_info.key(),
        reserve_vault_address(&pair, mint),
        OmnindexError::InvalidOmnipairVault
    );
    let reserve_vault_account = Account::<TokenAccount>::try_from(reserve_vault_info)?;
    require_keys_eq!(
        reserve_vault_account.owner,
        pair,
        OmnindexError::InvalidOmnipairVault
    );
    require_keys_eq!(
        reserve_vault_account.mint,
        *mint,
        OmnindexError::InvalidOmnipairVault
    );
    Ok(())
}
