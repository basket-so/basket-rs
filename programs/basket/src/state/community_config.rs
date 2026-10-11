use anchor_lang::prelude::*;

/// Terms every community basket gets, set by the protocol authority. A basket's fee on each
/// side is a whole number of fee steps, chosen by its creator; one step splits into the
/// protocol, creator and staking shares below (for example 4 + 1 + 5 = 10 bps, 0.10%, split
/// 40/10/50). A basket keeps the split it was created with, and changing these terms only
/// affects baskets created afterwards.
#[account]
pub struct CommunityConfig {
    /// Receives the creation fee in SOL, and the protocol share of each basket's fees in USDC.
    pub treasury: Pubkey,
    /// The rebalance keeper every community basket is created with.
    pub keeper: Pubkey,
    pub creation_fee_lamports: u64,
    pub protocol_step_bps: u16,
    pub creator_step_bps: u16,
    pub staking_step_bps: u16,
    pub bump: u8,
    pub reserved: [u8; 64],
}

impl CommunityConfig {
    pub const SPACE: usize = 32 + 32 + 8 + 2 * 3 + 1 + 64;

    pub fn step_bps(&self) -> u16 {
        self.protocol_step_bps
            .saturating_add(self.creator_step_bps)
            .saturating_add(self.staking_step_bps)
    }

    /// The (protocol, creator, staking) shares of a fee of `total_bps` on one side: a whole
    /// number of steps, at least one.
    pub fn split(&self, total_bps: u16) -> Option<(u16, u16, u16)> {
        let step = self.step_bps();
        if step == 0 || total_bps == 0 || total_bps % step != 0 {
            return None;
        }
        let steps = total_bps / step;
        Some((
            steps * self.protocol_step_bps,
            steps * self.creator_step_bps,
            steps * self.staking_step_bps,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> CommunityConfig {
        CommunityConfig {
            treasury: Pubkey::new_unique(),
            keeper: Pubkey::new_unique(),
            creation_fee_lamports: 200_000_000,
            protocol_step_bps: 4,
            creator_step_bps: 1,
            staking_step_bps: 5,
            bump: 255,
            reserved: [0; 64],
        }
    }

    #[test]
    fn fees_are_whole_steps_split_40_10_50() {
        let c = config();
        assert_eq!(c.split(10), Some((4, 1, 5)));
        assert_eq!(c.split(30), Some((12, 3, 15)));
        assert_eq!(c.split(100), Some((40, 10, 50)));
        assert_eq!(c.split(0), None);
        assert_eq!(c.split(15), None);
    }
}
