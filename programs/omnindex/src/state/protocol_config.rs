use anchor_lang::prelude::*;

use crate::constants::MAX_INDEX_CREATOR_WHITELIST;

#[account]
pub struct ProtocolConfig {
    pub authority: Pubkey,
    pub index_creator: Pubkey,
    pub permissionless_index_creation: bool,
    pub bump: u8,
    pub reserved: [u8; 30],
    pub index_creator_whitelist: Vec<Pubkey>,
}

impl ProtocolConfig {
    pub const SPACE: usize = 32 + 32 + 1 + 1 + 30 + 4 + (MAX_INDEX_CREATOR_WHITELIST * 32);

    pub fn can_create_index(&self, creator: &Pubkey) -> bool {
        self.permissionless_index_creation
            || self.index_creator == *creator
            || self
                .index_creator_whitelist
                .iter()
                .any(|whitelisted| whitelisted == creator)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(permissionless_index_creation: bool, index_creator: Pubkey) -> ProtocolConfig {
        ProtocolConfig {
            authority: Pubkey::new_unique(),
            index_creator,
            permissionless_index_creation,
            bump: 255,
            reserved: [0; 30],
            index_creator_whitelist: Vec::new(),
        }
    }

    #[test]
    fn permissioned_creation_allows_configured_creator() {
        let creator = Pubkey::new_unique();
        let protocol_config = config(false, creator);

        assert!(protocol_config.can_create_index(&creator));
    }

    #[test]
    fn permissioned_creation_rejects_other_creators() {
        let protocol_config = config(false, Pubkey::new_unique());

        assert!(!protocol_config.can_create_index(&Pubkey::new_unique()));
    }

    #[test]
    fn permissionless_creation_allows_any_creator() {
        let protocol_config = config(true, Pubkey::new_unique());

        assert!(protocol_config.can_create_index(&Pubkey::new_unique()));
    }

    #[test]
    fn permissioned_creation_allows_whitelisted_creator() {
        let creator = Pubkey::new_unique();
        let mut protocol_config = config(false, Pubkey::new_unique());
        protocol_config.index_creator_whitelist.push(creator);

        assert!(protocol_config.can_create_index(&creator));
    }
}
