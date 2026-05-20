use anchor_lang::prelude::*;

pub const INDEX_SEED: &[u8] = b"index";
pub const INDEX_MINT_SEED: &[u8] = b"index-mint";
pub const PROTOCOL_CONFIG_SEED: &[u8] = b"protocol-config";
pub const VAULT_AUTHORITY_SEED: &[u8] = b"vault-authority";
pub const STAKING_POOL_SEED: &[u8] = b"staking-pool";
pub const STAKING_AUTHORITY_SEED: &[u8] = b"staking-authority";
pub const STAKE_POSITION_SEED: &[u8] = b"stake-position";

pub const MAX_COMPONENTS: usize = 8;
pub const MAX_REBALANCE_SWAPS: usize = 32;
pub const MAX_FIXED_WEIGHT_EXECUTION_SLIPPAGE_BPS: u16 = 500;
pub const MAX_FIXED_WEIGHT_POST_REBALANCE_DRIFT_BPS: u16 = 500;
pub const MAX_FIXED_WEIGHT_QUOTE_DUST_BPS: u16 = 500;
pub const MAX_SWITCHBOARD_QUOTE_AGE_SLOTS: u64 = 150;
pub const MAX_ORACLE_PRICE_TOLERANCE_BPS: u16 = BPS_DENOMINATOR;
pub const MAX_NAV_TOLERANCE_BPS: u16 = BPS_DENOMINATOR;
pub const MAX_NAME_LEN: usize = 32;
pub const MAX_SYMBOL_LEN: usize = 10;
pub const MAX_METADATA_URI_LEN: usize = 200;
pub const MAX_INDEX_DECIMALS: u8 = 9;
pub const MAX_REBALANCE_DELAY_SECONDS: i64 = 30 * 24 * 60 * 60;

pub const BPS_DENOMINATOR: u16 = 10_000;
pub const REWARD_PER_TOKEN_SCALE: u128 = 1_000_000_000_000_000_000;

pub const BASKET_MINT: Pubkey = pubkey!("5yTFbtAE5RDjxpiVpDfyWuzcCWgwh659CEu7a7ZQtSpk");
pub const USDC_MINT: Pubkey = pubkey!("EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v");
pub const USDC_DECIMALS: u8 = 6;
