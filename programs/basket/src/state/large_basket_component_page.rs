use anchor_lang::prelude::*;

use crate::{constants::MAX_LARGE_BASKET_COMPONENTS_PER_PAGE, errors::BasketError};

#[derive(AnchorSerialize, AnchorDeserialize, Clone, Debug, PartialEq, Eq)]
pub struct LargeBasketComponent {
    pub mint: Pubkey,
    pub units_per_index: u64,
    pub target_weight_bps: u16,
    pub oracle_pair: Pubkey,
    pub token_program: Pubkey,
    pub vault: Pubkey,
    pub accounted_reserve: u64,
    pub decimals: u8,
}

impl LargeBasketComponent {
    pub const SPACE: usize = 32 + 8 + 2 + 32 + 32 + 32 + 8 + 1;
}

#[account]
pub struct LargeBasketComponentPage {
    pub index: Pubkey,
    pub page_index: u8,
    pub start_component_index: u16,
    pub component_count: u16,
    pub bump: u8,
    pub finalized: bool,
    pub reserved: [u8; 32],
    pub components: Vec<LargeBasketComponent>,
}

impl LargeBasketComponentPage {
    pub const SPACE: usize = 32
        + 1
        + 2
        + 2
        + 1
        + 1
        + 32
        + 4
        + (MAX_LARGE_BASKET_COMPONENTS_PER_PAGE * LargeBasketComponent::SPACE);

    pub fn component_offset(&self, component_index: u16) -> Result<usize> {
        let offset = component_index
            .checked_sub(self.start_component_index)
            .ok_or_else(|| error!(BasketError::InvalidComponentMint))?;
        require!(
            offset < self.component_count,
            BasketError::InvalidComponentMint
        );
        Ok(usize::from(offset))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn page() -> LargeBasketComponentPage {
        LargeBasketComponentPage {
            index: Pubkey::new_unique(),
            page_index: 1,
            start_component_index: 10,
            component_count: 3,
            bump: 255,
            finalized: false,
            reserved: [0; 32],
            components: Vec::new(),
        }
    }

    #[test]
    fn component_offset_maps_global_index_to_local_slot() {
        let page = page();

        assert_eq!(page.component_offset(10).unwrap(), 0);
        assert_eq!(page.component_offset(12).unwrap(), 2);
        assert!(page.component_offset(13).is_err());
    }
}
