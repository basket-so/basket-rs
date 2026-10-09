use anchor_lang::prelude::*;

/// The key whose signed prices rebalances accept. Mints and redeems never read prices. The
/// oracle signs prices off chain for one rebalance intent at a time; each rebalance step checks
/// that signature in its own transaction (see `utils::signed_prices`), so nothing is stored here
/// but the key.
#[account]
pub struct PriceOracle {
    /// The only key whose price signatures rebalances accept. Set by the protocol authority;
    /// changing it invalidates every price the previous key signed.
    pub oracle: Pubkey,
    pub bump: u8,
    pub reserved: [u8; 32],
}

impl PriceOracle {
    pub const SPACE: usize = 32 + 1 + 32;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fits_its_account() {
        let account = PriceOracle { oracle: Pubkey::new_unique(), bump: 255, reserved: [0; 32] };
        assert_eq!(account.try_to_vec().unwrap().len(), PriceOracle::SPACE);
    }
}
