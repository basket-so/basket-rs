use anchor_lang::prelude::*;

pub const INDEX_SEED: &[u8] = b"index";
pub const INDEX_MINT_SEED: &[u8] = b"index-mint";
pub const PROTOCOL_CONFIG_SEED: &[u8] = b"protocol-config";
pub const VAULT_AUTHORITY_SEED: &[u8] = b"vault-authority";
pub const STAKING_POOL_SEED: &[u8] = b"staking-pool";
pub const STAKING_AUTHORITY_SEED: &[u8] = b"staking-authority";
pub const STAKE_POSITION_SEED: &[u8] = b"stake-position";

pub const MAX_COMPONENTS: usize = 8;
pub const MAX_LARGE_BASKET_COMPONENTS: usize = 50;
pub const MAX_LARGE_BASKET_COMPONENTS_PER_PAGE: usize = 10;
pub const MAX_LARGE_BASKET_PAGES: usize =
    MAX_LARGE_BASKET_COMPONENTS.div_ceil(MAX_LARGE_BASKET_COMPONENTS_PER_PAGE);
pub const LARGE_BASKET_COMPONENT_BITMAP_BYTES: usize = MAX_LARGE_BASKET_COMPONENTS.div_ceil(8);
pub const MAX_INDEX_CREATOR_WHITELIST: usize = 64;
pub const MAX_REBALANCE_SWAPS: usize = 32;
pub const MAX_REBALANCE_SWAPS_PER_BATCH: usize = 4;
pub const LARGE_BASKET_INTENT_SEED: &[u8] = b"large-basket-intent";
pub const LARGE_BASKET_INTENT_LOCK_SEED: &[u8] = b"large-basket-intent-lock";
pub const LARGE_BASKET_COMPONENT_PAGE_SEED: &[u8] = b"large-basket-component-page";
pub const REBALANCE_INTENT_SEED: &[u8] = b"rebalance-intent";
pub const COMPOSITION_CHANGE_SEED: &[u8] = b"composition-change";
pub const PRICE_BOARD_SEED: &[u8] = b"price-board";
// Distinct mints the price board holds. A full board reuses the entry of a price too old to
// be read; this is several times the tokens every rebalanced basket holds together. The board
// is allocated at this size, so raising it later needs a realloc path first.
pub const PRICE_BOARD_CAPACITY: usize = 64;
// Prices one post may carry (a post must fit one transaction).
pub const MAX_PRICES_PER_POST: usize = 20;
// Oldest posted price (about a minute) open, swap and finalize accept.
pub const MAX_PRICE_AGE_SLOTS: u64 = 150;
// Notice holders get before a basket's weights or components change, so they can redeem first.
pub const COMPOSITION_CHANGE_DELAY_SECONDS: i64 = 3 * 24 * 60 * 60;
// A due change must be applied within this window; after it the proposal lapses and has to
// be proposed again (with fresh notice).
pub const COMPOSITION_CHANGE_APPLY_WINDOW_SECONDS: i64 = 7 * 24 * 60 * 60;
// One change appends at most one page's worth of components (so at most one new page).
pub const MAX_COMPOSITION_ADDITIONS: usize = MAX_LARGE_BASKET_COMPONENTS_PER_PAGE;
// Removed components keep their slot, and rebalances load every slot's vault and price every
// held component in one transaction. A change may leave at most this many slots, and this
// many components to price while the basket switches over.
pub const MAX_COMPOSITION_COMPONENTS: usize = 2 * MAX_LARGE_BASKET_COMPONENTS_PER_PAGE;
pub const MAX_COMPOSITION_PRICED_COMPONENTS: usize = 10;
// Smallest nonzero target weight a change may set, so a component's backing per index token
// does not round to zero units.
pub const MIN_COMPOSITION_WEIGHT_BPS: u16 = 50;
// Holds components returned from expired intents until their owners claim them.
pub const REFUND_ESCROW_SEED: &[u8] = b"refund-escrow";
pub const MAX_LARGE_BASKET_INTENT_TTL_SECONDS: i64 = 30 * 60;
// Long enough for every intent open at request time to expire and be cleaned up; after
// this a request lapses on its own.
pub const REBALANCE_REQUEST_WINDOW_SECONDS: i64 = MAX_LARGE_BASKET_INTENT_TTL_SECONDS + 10 * 60;
// Gap after a request's window before the next request, so a request alone can hold new
// intents back for at most two thirds of any hour. (The authority can remove a keeper that
// misuses requests or rebalance opens.)
pub const REBALANCE_REQUEST_COOLDOWN_SECONDS: i64 = 20 * 60;
pub const MAX_FIXED_WEIGHT_EXECUTION_SLIPPAGE_BPS: u16 = 500;
pub const MAX_FIXED_WEIGHT_POST_REBALANCE_DRIFT_BPS: u16 = 500;
// Keeper rebalances are permissionless, so the initiator-supplied NAV-loss tolerance is
// clamped two-sided: the ceiling bounds how much NAV a malicious keeper can leak per
// rebalance, the floor prevents a no-op-tight gate.
pub const MIN_KEEPER_NAV_TOLERANCE_BPS: u16 = 10;
pub const MAX_KEEPER_NAV_TOLERANCE_BPS: u16 = 100;
// The post-rebalance drift bound is clamped two-sided AND, at open, constrained to stay
// strictly below the index's drift re-trigger threshold so a finalized rebalance cannot
// immediately re-trigger (drift-loop guard). The floor keeps the bound achievable (an
// exact-0 bound is unreachable after real swaps); a drift-enabled basket must therefore
// be configured with a threshold strictly above this floor (validate_fixed_weight_config)
// so the resulting [MIN, threshold) range is always non-empty and finalizable.
pub const MIN_FIXED_WEIGHT_POST_REBALANCE_DRIFT_BPS: u16 = 25;
// Post-rebalance idle quote (USDC left in the scratch ATA, beyond any USDC component's
// own target backing) must be under this fraction of NAV at finalize. Kept tight so a
// keeper cannot finalize with a meaningful slice of NAV sitting un-invested; genuine
// swap dust is far below this.
pub const MAX_FIXED_WEIGHT_QUOTE_DUST_BPS: u16 = 100;
pub const MAX_TOTAL_INDEX_FEE_BPS: u16 = 1_000;
pub const MAX_ORACLE_PRICE_TOLERANCE_BPS: u16 = BPS_DENOMINATOR;
pub const MAX_NAV_TOLERANCE_BPS: u16 = BPS_DENOMINATOR;
pub const MAX_NAME_LEN: usize = 32;
pub const MAX_SYMBOL_LEN: usize = 10;
pub const MAX_METADATA_URI_LEN: usize = 200;
pub const MAX_INDEX_DECIMALS: u8 = 9;
pub const MAX_REBALANCE_DELAY_SECONDS: i64 = 30 * 24 * 60 * 60;

pub const BPS_DENOMINATOR: u16 = 10_000;
pub const REWARD_PER_TOKEN_SCALE: u128 = 1_000_000_000_000_000_000;

pub const BASKET_MINT: Pubkey = pubkey!("2rNBaMg5VAr1aMNCwAPdDZVgzzdTaNDebUnNqPFNmeta");
pub const USDC_MINT: Pubkey = pubkey!("EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v");
pub const USDC_DECIMALS: u8 = 6;
