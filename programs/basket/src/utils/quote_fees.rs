use anchor_lang::prelude::*;
use anchor_spl::{
    token, token_2022,
    token_interface::{self, TransferChecked},
};

use crate::{
    constants::{USDC_DECIMALS, USDC_MINT},
    errors::BasketError,
    state::IndexComponent,
};

use super::{
    basis_points_amount, load_interface_mint, load_interface_token_account, route_creator_fee,
    switchboard_feed_price, switchboard_price_to_nad, token_value_nad, SwitchboardPrice,
    REBALANCE_PRICE_SCALE,
};

pub const DIRECT_COMPONENT_ACCOUNT_STRIDE: usize = 4;
pub const DIRECT_QUOTE_FEE_ACCOUNT_COUNT: usize = 9;

#[derive(Clone)]
pub struct DirectQuoteFeeAccounts<'info> {
    pub quote_mint: AccountInfo<'info>,
    pub user_quote_token_account: AccountInfo<'info>,
    pub fee_recipient_quote_token_account: AccountInfo<'info>,
    pub creator_fee_recipient_quote_token_account: AccountInfo<'info>,
    pub quote_token_program: AccountInfo<'info>,
    pub switchboard_queue: AccountInfo<'info>,
    pub switchboard_quote: AccountInfo<'info>,
    pub slothashes: AccountInfo<'info>,
    pub instructions_sysvar: AccountInfo<'info>,
}

impl<'info> DirectQuoteFeeAccounts<'info> {
    pub fn from_slice(accounts: &[AccountInfo<'info>]) -> Result<Self> {
        require!(
            accounts.len() == DIRECT_QUOTE_FEE_ACCOUNT_COUNT,
            BasketError::InvalidRemainingAccounts
        );

        Ok(Self {
            quote_mint: accounts[0].clone(),
            user_quote_token_account: accounts[1].clone(),
            fee_recipient_quote_token_account: accounts[2].clone(),
            creator_fee_recipient_quote_token_account: accounts[3].clone(),
            quote_token_program: accounts[4].clone(),
            switchboard_queue: accounts[5].clone(),
            switchboard_quote: accounts[6].clone(),
            slothashes: accounts[7].clone(),
            instructions_sysvar: accounts[8].clone(),
        })
    }
}

pub fn validate_usdc_quote_fee_accounts(
    accounts: &DirectQuoteFeeAccounts<'_>,
    user: &Pubkey,
) -> Result<u8> {
    require_keys_eq!(
        accounts.quote_mint.key(),
        USDC_MINT,
        BasketError::InvalidQuoteMint
    );
    require!(
        accounts.quote_token_program.key() == token::ID
            || accounts.quote_token_program.key() == token_2022::ID,
        BasketError::InvalidTokenProgram
    );
    require_keys_eq!(
        *accounts.quote_mint.owner,
        accounts.quote_token_program.key(),
        BasketError::InvalidQuoteMint
    );

    let quote_mint = load_interface_mint(&accounts.quote_mint)?;
    require!(
        quote_mint.decimals == USDC_DECIMALS,
        BasketError::InvalidQuoteMint
    );

    let user_quote_account = load_interface_token_account(&accounts.user_quote_token_account)?;
    require_keys_eq!(
        user_quote_account.owner,
        *user,
        BasketError::InvalidUserTokenAccount
    );
    require_keys_eq!(
        user_quote_account.mint,
        accounts.quote_mint.key(),
        BasketError::InvalidUserTokenAccount
    );

    Ok(quote_mint.decimals)
}

pub fn quote_fee_split(
    quote_amount: u64,
    protocol_fee_bps: u16,
    creator_fee_bps: u16,
) -> Result<(u64, u64)> {
    Ok((
        basis_points_amount(quote_amount, protocol_fee_bps)?,
        basis_points_amount(quote_amount, creator_fee_bps)?,
    ))
}

pub fn quote_value_atoms_from_components(
    components: &[IndexComponent],
    component_amounts: &[u64],
    component_decimals: &[u8],
    switchboard_prices: &[SwitchboardPrice],
) -> Result<u64> {
    require!(
        components.len() == component_amounts.len() && components.len() == component_decimals.len(),
        BasketError::InvalidRemainingAccounts
    );

    let mut total_value_nad = 0u128;
    for ((component, amount), decimals) in components
        .iter()
        .zip(component_amounts.iter().copied())
        .zip(component_decimals.iter().copied())
    {
        if amount == 0 {
            continue;
        }

        let price_nad = if component.mint == USDC_MINT {
            require!(decimals == USDC_DECIMALS, BasketError::InvalidQuoteMint);
            REBALANCE_PRICE_SCALE
        } else {
            require_keys_neq!(
                component.oracle_pair,
                Pubkey::default(),
                BasketError::MissingSwitchboardFeed
            );
            switchboard_price_to_nad(switchboard_feed_price(
                switchboard_prices,
                &component.oracle_pair,
            )?)?
        };
        let component_value = token_value_nad(amount, decimals, price_nad)?;
        total_value_nad = total_value_nad
            .checked_add(component_value)
            .ok_or_else(|| error!(BasketError::ArithmeticOverflow))?;
    }

    quote_atoms_from_value_nad(total_value_nad)
}

pub fn quote_atoms_from_value_nad(value_nad: u128) -> Result<u64> {
    if value_nad == 0 {
        return Ok(0);
    }

    let quote_scale = pow10_u128(USDC_DECIMALS)?;
    let denominator = u128::from(REBALANCE_PRICE_SCALE);
    let atoms = value_nad
        .checked_mul(quote_scale)
        .and_then(|value| value.checked_add(denominator - 1))
        .and_then(|value| value.checked_div(denominator))
        .ok_or_else(|| error!(BasketError::ArithmeticOverflow))?;

    u64::try_from(atoms).map_err(|_| error!(BasketError::ArithmeticOverflow))
}

pub fn collect_quote_fees_from_user<'info>(
    accounts: &DirectQuoteFeeAccounts<'info>,
    user: AccountInfo<'info>,
    protocol_fee_owner: Pubkey,
    creator_fee_owner: Pubkey,
    protocol_fee: u64,
    creator_fee: u64,
    quote_decimals: u8,
) -> Result<()> {
    let user_quote_account = load_interface_token_account(&accounts.user_quote_token_account)?;
    require_keys_eq!(
        user_quote_account.owner,
        user.key(),
        BasketError::InvalidUserTokenAccount
    );
    require_keys_eq!(
        user_quote_account.mint,
        accounts.quote_mint.key(),
        BasketError::InvalidUserTokenAccount
    );

    let (protocol_fee, creator_fee) =
        route_creator_fee(protocol_fee, creator_fee, &creator_fee_owner)?;
    transfer_quote_fee_from_user(
        accounts,
        accounts.fee_recipient_quote_token_account.clone(),
        user.clone(),
        protocol_fee_owner,
        protocol_fee,
        false,
        quote_decimals,
    )?;
    transfer_quote_fee_from_user(
        accounts,
        accounts.creator_fee_recipient_quote_token_account.clone(),
        user,
        creator_fee_owner,
        creator_fee,
        true,
        quote_decimals,
    )
}

fn transfer_quote_fee_from_user<'info>(
    accounts: &DirectQuoteFeeAccounts<'info>,
    destination: AccountInfo<'info>,
    user: AccountInfo<'info>,
    expected_owner: Pubkey,
    amount: u64,
    creator_fee: bool,
    quote_decimals: u8,
) -> Result<()> {
    if amount == 0 {
        return Ok(());
    }

    let destination_account = load_interface_token_account(&destination)?;
    if creator_fee {
        require_keys_eq!(
            destination_account.owner,
            expected_owner,
            BasketError::InvalidCreatorFeeRecipientTokenAccount
        );
        require_keys_eq!(
            destination_account.mint,
            accounts.quote_mint.key(),
            BasketError::InvalidCreatorFeeRecipientTokenAccount
        );
    } else {
        require_keys_eq!(
            destination_account.owner,
            expected_owner,
            BasketError::InvalidFeeRecipientTokenAccount
        );
        require_keys_eq!(
            destination_account.mint,
            accounts.quote_mint.key(),
            BasketError::InvalidFeeRecipientTokenAccount
        );
    }

    if destination.key() == accounts.user_quote_token_account.key() {
        return Ok(());
    }

    token_interface::transfer_checked(
        CpiContext::new(
            accounts.quote_token_program.clone(),
            TransferChecked {
                from: accounts.user_quote_token_account.clone(),
                mint: accounts.quote_mint.clone(),
                to: destination,
                authority: user,
            },
        ),
        amount,
        quote_decimals,
    )
}

fn pow10_u128(decimals: u8) -> Result<u128> {
    let mut value = 1u128;
    for _ in 0..decimals {
        value = value
            .checked_mul(10)
            .ok_or_else(|| error!(BasketError::ArithmeticOverflow))?;
    }
    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quote_atoms_from_value_rounds_up_fractional_atoms() {
        assert_eq!(quote_atoms_from_value_nad(1).unwrap(), 1);
        assert_eq!(
            quote_atoms_from_value_nad(u128::from(REBALANCE_PRICE_SCALE)).unwrap(),
            1_000_000
        );
    }

    #[test]
    fn quote_fee_split_rounds_each_side_up() {
        assert_eq!(quote_fee_split(199, 50, 25).unwrap(), (1, 1));
    }
}
