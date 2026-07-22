use anchor_lang::prelude::*;

use crate::{constants::MAX_TOTAL_INDEX_FEE_BPS, errors::BasketError};

use super::basis_points_amount;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LargeBasketFeeSplit {
    pub protocol_fee: u64,
    pub creator_fee: u64,
    pub staking_fee: u64,
}

pub fn validate_total_index_fee_bps(
    protocol_fee_bps: u16,
    creator_fee_bps: u16,
    staking_fee_bps: u16,
) -> Result<()> {
    let total = protocol_fee_bps
        .checked_add(creator_fee_bps)
        .and_then(|value| value.checked_add(staking_fee_bps))
        .ok_or_else(|| error!(BasketError::InvalidFeeBps))?;
    require!(total <= MAX_TOTAL_INDEX_FEE_BPS, BasketError::InvalidFeeBps);
    Ok(())
}

pub fn large_basket_fee_split(
    fee_basis_usdc_atoms: u64,
    protocol_fee_bps: u16,
    creator_fee_bps: u16,
    staking_fee_bps: u16,
) -> Result<LargeBasketFeeSplit> {
    validate_total_index_fee_bps(protocol_fee_bps, creator_fee_bps, staking_fee_bps)?;
    Ok(LargeBasketFeeSplit {
        protocol_fee: basis_points_amount(fee_basis_usdc_atoms, protocol_fee_bps)?,
        creator_fee: basis_points_amount(fee_basis_usdc_atoms, creator_fee_bps)?,
        staking_fee: basis_points_amount(fee_basis_usdc_atoms, staking_fee_bps)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_large_basket_fees_predictably() {
        let split = large_basket_fee_split(1_000_000, 40, 20, 10).unwrap();

        assert_eq!(
            split,
            LargeBasketFeeSplit {
                protocol_fee: 4_000,
                creator_fee: 2_000,
                staking_fee: 1_000,
            }
        );
    }

    #[test]
    fn rejects_total_fee_above_cap() {
        assert!(validate_total_index_fee_bps(500, 400, 101).is_err());
    }
}
