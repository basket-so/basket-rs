use anchor_lang::prelude::*;

/// Reserves a basket symbol for one basket, so no two baskets share a symbol. Seeded by
/// symbol_claim_key, so symbols that only differ by look-alike characters share one claim.
#[account]
pub struct SymbolClaim {
    pub index: Pubkey,
    pub bump: u8,
}

impl SymbolClaim {
    pub const SPACE: usize = 32 + 1;
}

/// The seed a symbol is claimed under: the symbol (uppercase A-Z and 0-9, see validate_symbol)
/// with the digits that pass for letters folded into them, 0 into O and 1 into I. "MDA0"
/// therefore collides with "MDAO".
pub fn symbol_claim_key(symbol: &str) -> Vec<u8> {
    symbol
        .bytes()
        .map(|byte| match byte {
            b'0' => b'O',
            b'1' => b'I',
            other => other,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn look_alike_symbols_share_a_claim() {
        assert_eq!(symbol_claim_key("MDA0"), symbol_claim_key("MDAO"));
        assert_eq!(symbol_claim_key("B1G"), symbol_claim_key("BIG"));
        assert_eq!(symbol_claim_key("SOLB"), b"SOLB".to_vec());
        assert_ne!(symbol_claim_key("SOLB"), symbol_claim_key("SOLC"));
    }
}
