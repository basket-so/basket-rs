use anchor_lang::prelude::*;

#[account]
pub struct ProtocolConfig {
    pub authority: Pubkey,
    pub index_creator: Pubkey,
    pub permissionless_index_creation: bool,
    pub bump: u8,
    pub reserved: [u8; 30],
}

impl ProtocolConfig {
    pub const SPACE: usize = 32 + 32 + 1 + 1 + 30;

    pub fn can_create_index(&self, creator: &Pubkey) -> bool {
        self.permissionless_index_creation || self.index_creator == *creator
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
}
