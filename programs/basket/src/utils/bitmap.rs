use anchor_lang::prelude::*;

use crate::{
    constants::{LARGE_BASKET_COMPONENT_BITMAP_BYTES, MAX_LARGE_BASKET_COMPONENTS},
    errors::BasketError,
};

// Generic fixed-size component bitmap helpers. Used by the rebalance intent, whose
// swap/verify/NAV progress is tracked across several independent bitmaps (the large
// basket mint/redeem intent keeps its own copies for its single fill bitmap).

fn bitmap_position(index: u16) -> Result<(usize, u8)> {
    require!(
        usize::from(index) < MAX_LARGE_BASKET_COMPONENTS,
        BasketError::InvalidLargeBasketIntent
    );
    let byte_index = usize::from(index) / 8;
    require!(
        byte_index < LARGE_BASKET_COMPONENT_BITMAP_BYTES,
        BasketError::InvalidLargeBasketIntent
    );
    Ok((byte_index, 1u8 << (index % 8)))
}

pub fn bitmap_get(
    bitmap: &[u8; LARGE_BASKET_COMPONENT_BITMAP_BYTES],
    index: u16,
) -> Result<bool> {
    let (byte_index, bit) = bitmap_position(index)?;
    Ok((bitmap[byte_index] & bit) != 0)
}

/// Sets the bit for `index`, erroring if it was already set (idempotency guard).
pub fn bitmap_set_once(
    bitmap: &mut [u8; LARGE_BASKET_COMPONENT_BITMAP_BYTES],
    index: u16,
) -> Result<()> {
    let (byte_index, bit) = bitmap_position(index)?;
    require!(
        bitmap[byte_index] & bit == 0,
        BasketError::LargeBasketComponentAlreadyFilled
    );
    bitmap[byte_index] |= bit;
    Ok(())
}

/// True iff every bit in `0..count` is set.
pub fn bitmap_all_set(
    bitmap: &[u8; LARGE_BASKET_COMPONENT_BITMAP_BYTES],
    count: u16,
) -> Result<bool> {
    for index in 0..count {
        if !bitmap_get(bitmap, index)? {
            return Ok(false);
        }
    }
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sets_each_bit_once() {
        let mut bitmap = [0u8; LARGE_BASKET_COMPONENT_BITMAP_BYTES];
        assert!(!bitmap_get(&bitmap, 13).unwrap());
        bitmap_set_once(&mut bitmap, 13).unwrap();
        assert!(bitmap_get(&bitmap, 13).unwrap());
        assert!(bitmap_set_once(&mut bitmap, 13).is_err());
    }

    #[test]
    fn all_set_tracks_full_coverage() {
        let mut bitmap = [0u8; LARGE_BASKET_COMPONENT_BITMAP_BYTES];
        for index in 0..3 {
            bitmap_set_once(&mut bitmap, index).unwrap();
        }
        assert!(bitmap_all_set(&bitmap, 3).unwrap());
        assert!(!bitmap_all_set(&bitmap, 4).unwrap());
    }

    #[test]
    fn rejects_out_of_range_index() {
        let bitmap = [0u8; LARGE_BASKET_COMPONENT_BITMAP_BYTES];
        assert!(bitmap_get(&bitmap, MAX_LARGE_BASKET_COMPONENTS as u16).is_err());
    }
}
