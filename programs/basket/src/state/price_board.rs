use anchor_lang::prelude::*;

use crate::{
    constants::{MAX_PRICE_AGE_SLOTS, PRICE_BOARD_CAPACITY, USDC_MINT},
    errors::BasketError,
};

/// USD prices the protocol's own oracle posts for rebalances. Mints and redeems never read
/// it. One board serves every basket; rebalances look prices up by component mint.
#[account]
pub struct PriceBoard {
    /// The only key that may post prices. Set by the protocol authority; changing it clears
    /// every posted price.
    pub oracle: Pubkey,
    pub bump: u8,
    pub reserved: [u8; 32],
    pub prices: Vec<BoardPrice>,
}

#[derive(AnchorSerialize, AnchorDeserialize, Clone, Copy, Debug, PartialEq, Eq)]
pub struct BoardPrice {
    pub mint: Pubkey,
    /// USD per whole token, scaled by `PRICE_SCALE` (1e18).
    pub price: u128,
    /// Slot the price was posted in, set by the program.
    pub posted_slot: u64,
}

impl BoardPrice {
    pub const SPACE: usize = 32 + 16 + 8;
}

impl PriceBoard {
    pub const SPACE: usize = 32 + 1 + 32 + 4 + PRICE_BOARD_CAPACITY * BoardPrice::SPACE;

    /// Records `mint`'s price as of `slot`, replacing its previous one. A full board reuses the
    /// entry of the longest-unposted mint, but only once no rebalance could still read it.
    pub fn post(&mut self, mint: Pubkey, price: u128, slot: u64) -> Result<()> {
        require!(
            price > 0 && i128::try_from(price).is_ok(),
            BasketError::InvalidOraclePrice
        );
        // Rebalances value USDC at exactly $1 themselves.
        require!(
            mint != Pubkey::default() && mint != USDC_MINT,
            BasketError::InvalidOraclePrice
        );
        let posted = BoardPrice { mint, price, posted_slot: slot };
        if let Some(entry) = self.prices.iter_mut().find(|entry| entry.mint == mint) {
            *entry = posted;
        } else if self.prices.len() < PRICE_BOARD_CAPACITY {
            self.prices.push(posted);
        } else {
            let oldest = self
                .prices
                .iter_mut()
                .min_by_key(|entry| entry.posted_slot)
                .ok_or_else(|| error!(BasketError::PriceBoardFull))?;
            require!(
                slot.saturating_sub(oldest.posted_slot) > MAX_PRICE_AGE_SLOTS,
                BasketError::PriceBoardFull
            );
            *oldest = posted;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn board() -> PriceBoard {
        PriceBoard { oracle: Pubkey::new_unique(), bump: 255, reserved: [0; 32], prices: Vec::new() }
    }

    #[test]
    fn posting_replaces_a_mints_price() {
        let mut board = board();
        let mint = Pubkey::new_unique();
        board.post(mint, 5, 10).unwrap();
        board.post(mint, 7, 20).unwrap();
        assert_eq!(board.prices, vec![BoardPrice { mint, price: 7, posted_slot: 20 }]);
    }

    #[test]
    fn rejects_unusable_prices() {
        let mut board = board();
        assert!(board.post(Pubkey::new_unique(), 0, 1).is_err());
        assert!(board.post(Pubkey::new_unique(), u128::MAX, 1).is_err());
        assert!(board.post(USDC_MINT, 1, 1).is_err());
        assert!(board.post(Pubkey::default(), 1, 1).is_err());
        assert!(board.prices.is_empty());
    }

    #[test]
    fn full_board_reuses_only_unreadable_entries() {
        let mut board = board();
        for i in 0..PRICE_BOARD_CAPACITY {
            board.post(Pubkey::new_unique(), 1, 100 + i as u64).unwrap();
        }
        let stalest = board.prices[0].mint;
        // Every entry could still be read by a rebalance.
        let now = 100 + MAX_PRICE_AGE_SLOTS;
        assert!(board.post(Pubkey::new_unique(), 1, now).is_err());
        // The oldest has aged out: a new mint takes its place.
        let newcomer = Pubkey::new_unique();
        board.post(newcomer, 3, now + 1).unwrap();
        assert_eq!(board.prices.len(), PRICE_BOARD_CAPACITY);
        assert!(board.prices.iter().all(|entry| entry.mint != stalest));
        assert!(board.prices.iter().any(|entry| entry.mint == newcomer && entry.price == 3));
        // Known mints always update in place.
        let known = board.prices[5].mint;
        board.post(known, 9, now + 1).unwrap();
        assert_eq!(board.prices.len(), PRICE_BOARD_CAPACITY);
    }

    #[test]
    fn full_board_fits_its_account() {
        let mut board = board();
        for _ in 0..PRICE_BOARD_CAPACITY {
            board.post(Pubkey::new_unique(), u128::from(u64::MAX), u64::MAX).unwrap();
        }
        assert_eq!(board.try_to_vec().unwrap().len(), PriceBoard::SPACE);
    }
}
