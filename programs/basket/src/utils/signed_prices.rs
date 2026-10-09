//! Rebalance prices signed by the protocol's price oracle.
//!
//! The oracle signs a price message off chain for one rebalance intent; the keeper puts the
//! native Ed25519 program's instruction carrying that signature immediately before the step
//! that reads the prices. The runtime verifies the signature before any instruction runs, and
//! the step then reads the verified public key and message back out of that instruction
//! through the instructions sysvar and checks them here.
//!
//! Message (little-endian):
//!   [0..16)   tag "basket-prices-v1"
//!   [16..48)  rebalance intent address (for open, the PDA it creates from index + nonce)
//!   [48..56)  u64 slot the oracle signed at
//!   [56]      u8 entry count
//!   then 6 bytes per entry: u8 global component index, u32 mantissa, u8 exponent; the price,
//!   USD per whole token at PRICE_SCALE, is mantissa × 10^exponent
//!
//! Every byte counts: open and finalize price every component of the basket in one transaction,
//! and a swap step shares its transaction with a Jupiter route. So the intent address (a PDA of
//! this program, which already ties the message to it) stands in for the program id, entries
//! name components by index rather than mint, and prices are decimal floating point: the oracle
//! keeps the mantissa as large as fits in a u32, which keeps 9 to 10 significant digits (a
//! relative error under 1.2e-9, far below the bps-level bounds rebalances check prices against).
//! A basket's slots are append-only and each mint holds one slot, so an index always means the
//! same mint; the oracle resolves it from the basket's pages on chain, never from the keeper.

use anchor_lang::prelude::*;
use anchor_lang::solana_program::{
    ed25519_program,
    instruction::Instruction,
    sysvar::instructions::{load_current_index_checked, load_instruction_at_checked},
};

use crate::{
    constants::MAX_LARGE_BASKET_COMPONENTS, errors::BasketError, utils::validate_price_age_slots,
};

pub const PRICE_MESSAGE_TAG: [u8; 16] = *b"basket-prices-v1";
const PRICE_MESSAGE_HEADER_LEN: usize = 16 + 32 + 8 + 1;
const PRICE_ENTRY_LEN: usize = 1 + 4 + 1;
// Entries name a component by a u8 index.
const _: () = assert!(MAX_LARGE_BASKET_COMPONENTS <= u8::MAX as usize + 1);

// The Ed25519 program's instruction: u8 signature count, u8 padding, then per signature the
// offsets of its signature, public key and message, each paired with the index of the
// instruction holding it.
const ED25519_OFFSETS_START: usize = 2;
const ED25519_OFFSETS_LEN: usize = 14;
const ED25519_PUBKEY_LEN: usize = 32;
const ED25519_SIGNATURE_LEN: usize = 64;
/// An Ed25519 offset's instruction index meaning "this instruction's own data".
const THIS_INSTRUCTION: u16 = u16::MAX;

/// Verified prices for one rebalance step, by global component index.
#[derive(Debug, PartialEq, Eq)]
pub struct SignedPrices {
    pub slot: u64,
    entries: Vec<(u16, i128)>,
}

impl SignedPrices {
    /// USD per whole token (PRICE_SCALE) the oracle signed for this component.
    pub fn price(&self, component_index: u16) -> Result<i128> {
        self.entries
            .iter()
            .find(|(index, _)| *index == component_index)
            .map(|(_, price)| *price)
            .ok_or_else(|| error!(BasketError::MissingOraclePrice))
    }
}

/// The prices signed by `oracle` for `intent`, from the Ed25519 instruction immediately before
/// the instruction now executing, at most `max_age_slots` old at `current_slot`.
pub fn load_signed_prices(
    instructions_sysvar: &AccountInfo,
    oracle: &Pubkey,
    intent: &Pubkey,
    current_slot: u64,
    max_age_slots: u64,
) -> Result<SignedPrices> {
    let current = load_current_index_checked(instructions_sysvar)?;
    require!(current > 0, BasketError::MissingPriceSignature);
    let signature_ix = load_instruction_at_checked(usize::from(current - 1), instructions_sysvar)?;
    verify_signed_prices(&signature_ix, oracle, intent, current_slot, max_age_slots)
}

/// Checks one Ed25519 program instruction (already verified by the runtime) and the price
/// message it signs.
pub fn verify_signed_prices(
    signature_ix: &Instruction,
    oracle: &Pubkey,
    intent: &Pubkey,
    current_slot: u64,
    max_age_slots: u64,
) -> Result<SignedPrices> {
    validate_price_age_slots(max_age_slots)?;
    require_keys_eq!(
        signature_ix.program_id,
        ed25519_program::ID,
        BasketError::MissingPriceSignature
    );
    let (signer, message) = ed25519_signed_message(&signature_ix.data)?;
    require_keys_eq!(signer, *oracle, BasketError::InvalidPriceSignature);
    let prices = decode_price_message(message, intent)?;
    require!(prices.slot <= current_slot, BasketError::SignedPriceSlotInFuture);
    require!(
        current_slot - prices.slot <= max_age_slots,
        BasketError::StaleOraclePrice
    );
    Ok(prices)
}

/// The public key and message of an Ed25519 instruction carrying exactly one signature whose
/// signature, key and message all sit in the instruction's own data. An offset may name another
/// instruction, and the runtime then verifies bytes there; reading the key and message from this
/// instruction's data would then check bytes nobody verified, so that is refused outright.
fn ed25519_signed_message(data: &[u8]) -> Result<(Pubkey, &[u8])> {
    require!(
        data.len() >= ED25519_OFFSETS_START + ED25519_OFFSETS_LEN && data[0] == 1,
        BasketError::InvalidPriceSignature
    );
    let field = |n: usize| read_u16(data, ED25519_OFFSETS_START + 2 * n);
    let (signature_offset, signature_ix) = (field(0)?, field(1)?);
    let (pubkey_offset, pubkey_ix) = (field(2)?, field(3)?);
    let (message_offset, message_size, message_ix) = (field(4)?, field(5)?, field(6)?);
    require!(
        signature_ix == THIS_INSTRUCTION
            && pubkey_ix == THIS_INSTRUCTION
            && message_ix == THIS_INSTRUCTION,
        BasketError::InvalidPriceSignature
    );
    within(data, signature_offset, ED25519_SIGNATURE_LEN)?;
    let pubkey = within(data, pubkey_offset, ED25519_PUBKEY_LEN)?;
    let message = within(data, message_offset, usize::from(message_size))?;
    let pubkey = Pubkey::try_from(pubkey).map_err(|_| error!(BasketError::InvalidPriceSignature))?;
    Ok((pubkey, message))
}

fn read_u16(data: &[u8], at: usize) -> Result<u16> {
    let bytes = data
        .get(at..at + 2)
        .ok_or_else(|| error!(BasketError::InvalidPriceSignature))?;
    Ok(u16::from_le_bytes([bytes[0], bytes[1]]))
}

fn within(data: &[u8], offset: u16, len: usize) -> Result<&[u8]> {
    let start = usize::from(offset);
    data.get(start..start + len)
        .ok_or_else(|| error!(BasketError::InvalidPriceSignature))
}

fn decode_price_message(message: &[u8], intent: &Pubkey) -> Result<SignedPrices> {
    require!(
        message.len() >= PRICE_MESSAGE_HEADER_LEN && message[..16] == PRICE_MESSAGE_TAG,
        BasketError::InvalidPriceMessage
    );
    require!(message[16..48] == intent.to_bytes(), BasketError::SignedPricesMismatch);
    let slot = u64::from_le_bytes(message[48..56].try_into().unwrap());
    let count = usize::from(message[56]);
    require!(
        message.len() == PRICE_MESSAGE_HEADER_LEN + count * PRICE_ENTRY_LEN,
        BasketError::InvalidPriceMessage
    );
    let mut entries: Vec<(u16, i128)> = Vec::with_capacity(count);
    for entry in message[PRICE_MESSAGE_HEADER_LEN..].chunks_exact(PRICE_ENTRY_LEN) {
        let index = u16::from(entry[0]);
        let mantissa = u32::from_le_bytes(entry[1..5].try_into().unwrap());
        let price = compact_price(mantissa, entry[5])?;
        require!(
            entries.iter().all(|(seen, _)| *seen != index),
            BasketError::DuplicateOraclePrice
        );
        entries.push((index, price));
    }
    Ok(SignedPrices { slot, entries })
}

/// mantissa × 10^exponent, which must be positive and fit an i128.
fn compact_price(mantissa: u32, exponent: u8) -> Result<i128> {
    require!(mantissa > 0, BasketError::InvalidOraclePrice);
    10u128
        .checked_pow(u32::from(exponent))
        .and_then(|scale| scale.checked_mul(u128::from(mantissa)))
        .and_then(|price| i128::try_from(price).ok())
        .ok_or_else(|| error!(BasketError::InvalidOraclePrice))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{constants::MAX_PRICE_AGE_SLOTS, utils::PRICE_SCALE};

    const SLOT: u64 = 1_000;

    struct Setup {
        oracle: Pubkey,
        intent: Pubkey,
    }

    fn setup() -> Setup {
        Setup { oracle: Pubkey::new_unique(), intent: Pubkey::new_unique() }
    }

    /// Entries are (component index, mantissa, exponent).
    fn message(intent: &Pubkey, slot: u64, entries: &[(u8, u32, u8)]) -> Vec<u8> {
        let mut m = Vec::new();
        m.extend_from_slice(&PRICE_MESSAGE_TAG);
        m.extend_from_slice(intent.as_ref());
        m.extend_from_slice(&slot.to_le_bytes());
        m.push(entries.len() as u8);
        for (index, mantissa, exponent) in entries {
            m.push(*index);
            m.extend_from_slice(&mantissa.to_le_bytes());
            m.push(*exponent);
        }
        m
    }

    /// The layout web3.js's Ed25519Program.createInstructionWithPublicKey produces: offsets,
    /// then key, signature and message, every index "this instruction". The signature bytes are
    /// a placeholder; the runtime, not the program, checks them.
    fn ed25519_ix(signer: &Pubkey, message: &[u8]) -> Instruction {
        let pubkey_offset = 16u16;
        let signature_offset = pubkey_offset + 32;
        let message_offset = signature_offset + 64;
        let mut data = vec![1u8, 0];
        for value in [
            signature_offset,
            THIS_INSTRUCTION,
            pubkey_offset,
            THIS_INSTRUCTION,
            message_offset,
            message.len() as u16,
            THIS_INSTRUCTION,
        ] {
            data.extend_from_slice(&value.to_le_bytes());
        }
        data.extend_from_slice(signer.as_ref());
        data.extend_from_slice(&[7u8; 64]);
        data.extend_from_slice(message);
        Instruction { program_id: ed25519_program::ID, accounts: vec![], data }
    }

    fn set_u16(ix: &mut Instruction, field: usize, value: u16) {
        let at = ED25519_OFFSETS_START + 2 * field;
        ix.data[at..at + 2].copy_from_slice(&value.to_le_bytes());
    }

    /// Component 0 at $2 and component 3 at $0.25.
    fn prices() -> Vec<(u8, u32, u8)> {
        vec![(0, 2, 18), (3, 25, 16)]
    }

    fn valid(s: &Setup) -> Instruction {
        ed25519_ix(&s.oracle, &message(&s.intent, SLOT, &prices()))
    }

    fn verify(s: &Setup, ix: &Instruction, current_slot: u64) -> Result<SignedPrices> {
        verify_signed_prices(ix, &s.oracle, &s.intent, current_slot, MAX_PRICE_AGE_SLOTS)
    }

    fn assert_error<T: std::fmt::Debug>(result: Result<T>, expected: BasketError) {
        match result {
            Err(anchor_lang::error::Error::AnchorError(e)) => {
                assert_eq!(e.error_name, format!("{expected:?}"), "{e}")
            }
            other => panic!("expected {expected:?}, got {other:?}"),
        }
    }

    #[test]
    fn accepts_the_oracles_prices_for_this_intent() {
        let s = setup();
        let signed = verify(&s, &valid(&s), SLOT + 10).unwrap();
        assert_eq!(signed.slot, SLOT);
        assert_eq!(signed.price(0).unwrap(), 2 * PRICE_SCALE as i128);
        assert_eq!(signed.price(3).unwrap(), (PRICE_SCALE / 4) as i128);
        // Signed in the current slot, and at exactly the oldest allowed age.
        assert!(verify(&s, &valid(&s), SLOT).is_ok());
        assert!(verify(&s, &valid(&s), SLOT + MAX_PRICE_AGE_SLOTS).is_ok());
    }

    #[test]
    fn missing_component_is_refused_when_read() {
        let s = setup();
        let signed = verify(&s, &valid(&s), SLOT).unwrap();
        assert_error(signed.price(1), BasketError::MissingOraclePrice);
    }

    #[test]
    fn requires_the_ed25519_program() {
        let s = setup();
        let mut ix = valid(&s);
        ix.program_id = Pubkey::new_unique();
        assert_error(verify(&s, &ix, SLOT), BasketError::MissingPriceSignature);
    }

    #[test]
    fn requires_exactly_one_signature() {
        let s = setup();
        for count in [0u8, 2] {
            let mut ix = valid(&s);
            ix.data[0] = count;
            assert_error(verify(&s, &ix, SLOT), BasketError::InvalidPriceSignature);
        }
        let mut short = valid(&s);
        short.data.truncate(ED25519_OFFSETS_START + ED25519_OFFSETS_LEN - 1);
        assert_error(verify(&s, &short, SLOT), BasketError::InvalidPriceSignature);
    }

    #[test]
    fn refuses_offsets_into_other_instructions() {
        let s = setup();
        // Signature, public key and message instruction indexes, each pointed elsewhere.
        for field in [1, 3, 6] {
            for index in [0u16, 1, 2, u16::MAX - 1] {
                let mut ix = valid(&s);
                set_u16(&mut ix, field, index);
                assert_error(verify(&s, &ix, SLOT), BasketError::InvalidPriceSignature);
            }
        }
    }

    #[test]
    fn refuses_out_of_bounds_offsets() {
        let s = setup();
        let len = valid(&s).data.len() as u16;
        let cases: [(usize, u16); 5] = [
            (0, len - 63), // signature runs past the end
            (2, len - 31), // public key runs past the end
            (4, len),      // message starts at the end and is not empty
            (5, u16::MAX), // message longer than the data
            (0, u16::MAX), // offset far past the end
        ];
        for (field, value) in cases {
            let mut ix = valid(&s);
            set_u16(&mut ix, field, value);
            assert_error(verify(&s, &ix, SLOT), BasketError::InvalidPriceSignature);
        }
    }

    #[test]
    fn requires_the_configured_oracle_key() {
        let s = setup();
        let ix = ed25519_ix(&Pubkey::new_unique(), &message(&s.intent, SLOT, &prices()));
        assert_error(verify(&s, &ix, SLOT), BasketError::InvalidPriceSignature);
    }

    #[test]
    fn requires_the_price_message_tag_and_length() {
        let s = setup();
        let mut tagged = message(&s.intent, SLOT, &prices());
        tagged[..16].copy_from_slice(b"basket-prices-v2");
        assert_error(verify(&s, &ed25519_ix(&s.oracle, &tagged), SLOT), BasketError::InvalidPriceMessage);
        let mut long = message(&s.intent, SLOT, &prices());
        long.push(0);
        assert_error(verify(&s, &ed25519_ix(&s.oracle, &long), SLOT), BasketError::InvalidPriceMessage);
        let mut short = message(&s.intent, SLOT, &prices());
        short.pop();
        assert_error(verify(&s, &ed25519_ix(&s.oracle, &short), SLOT), BasketError::InvalidPriceMessage);
        let header_only = &message(&s.intent, SLOT, &[])[..PRICE_MESSAGE_HEADER_LEN - 1];
        assert_error(verify(&s, &ed25519_ix(&s.oracle, header_only), SLOT), BasketError::InvalidPriceMessage);
    }

    #[test]
    fn requires_this_intent() {
        let s = setup();
        let other_intent = ed25519_ix(&s.oracle, &message(&Pubkey::new_unique(), SLOT, &prices()));
        assert_error(verify(&s, &other_intent, SLOT), BasketError::SignedPricesMismatch);
        // An intent of another program is another address too (PDAs include the program id).
        let index = Pubkey::new_unique();
        let seeds: &[&[u8]] = &[b"rebalance-intent", index.as_ref(), &1u64.to_le_bytes()];
        let ours = Pubkey::find_program_address(seeds, &crate::ID).0;
        let theirs = Pubkey::find_program_address(seeds, &Pubkey::new_unique()).0;
        let elsewhere = ed25519_ix(&s.oracle, &message(&theirs, SLOT, &prices()));
        assert_error(
            verify_signed_prices(&elsewhere, &s.oracle, &ours, SLOT, MAX_PRICE_AGE_SLOTS),
            BasketError::SignedPricesMismatch,
        );
    }

    #[test]
    fn refuses_stale_and_future_prices() {
        let s = setup();
        assert_error(verify(&s, &valid(&s), SLOT + MAX_PRICE_AGE_SLOTS + 1), BasketError::StaleOraclePrice);
        assert_error(verify(&s, &valid(&s), SLOT - 1), BasketError::SignedPriceSlotInFuture);
        // A caller may ask for a tighter age, never a looser one.
        assert_error(
            verify_signed_prices(&valid(&s), &s.oracle, &s.intent, SLOT + 11, 10),
            BasketError::StaleOraclePrice,
        );
        assert_error(
            verify_signed_prices(&valid(&s), &s.oracle, &s.intent, SLOT, MAX_PRICE_AGE_SLOTS + 1),
            BasketError::InvalidOraclePriceAge,
        );
    }

    #[test]
    fn refuses_duplicate_and_unusable_prices() {
        let s = setup();
        let duplicate = message(&s.intent, SLOT, &[(3, 1, 18), (3, 2, 18)]);
        assert_error(verify(&s, &ed25519_ix(&s.oracle, &duplicate), SLOT), BasketError::DuplicateOraclePrice);
        let unusable = [
            (0, 18),        // zero
            (1, 39),        // 10^39 overflows u128
            (u32::MAX, 38), // the product overflows u128
            (2, 38),        // 2e38 fits u128 but not the i128 prices are computed in
        ];
        for (mantissa, exponent) in unusable {
            let bad = message(&s.intent, SLOT, &[(0, mantissa, exponent)]);
            assert_error(verify(&s, &ed25519_ix(&s.oracle, &bad), SLOT), BasketError::InvalidOraclePrice);
        }
    }

    #[test]
    fn compact_prices_span_every_usable_price() {
        assert_eq!(compact_price(1, 0).unwrap(), 1, "1e-18 dollars");
        assert_eq!(compact_price(1, 38).unwrap(), 10i128.pow(38));
        assert_eq!(compact_price(u32::MAX, 0).unwrap(), i128::from(u32::MAX));
        // $123.456789 kept to 9 significant digits.
        assert_eq!(compact_price(1_234_567_890, 11).unwrap(), 123_456_789_000_000_000_000);
        let s = setup();
        let signed = verify(&s, &ed25519_ix(&s.oracle, &message(&s.intent, SLOT, &[(255, 7, 17)])), SLOT).unwrap();
        assert_eq!(signed.price(255).unwrap(), (7 * PRICE_SCALE / 10) as i128, "the last u8 index");
    }

    #[test]
    fn an_empty_price_list_still_binds_the_intent() {
        let s = setup();
        let empty = ed25519_ix(&s.oracle, &message(&s.intent, SLOT, &[]));
        let signed = verify(&s, &empty, SLOT).unwrap();
        assert_error(signed.price(0), BasketError::MissingOraclePrice);
    }
}
