use anchor_lang::{
    prelude::*,
    solana_program::{
        instruction::{AccountMeta, Instruction},
        program::invoke_signed,
    },
};

use crate::errors::OmnindexError;

pub const OMNIPAIR_ID: Pubkey = pubkey!("omnixgS8fnqHfCcTGKWj6JtKjzpJZ1Y5y9pyFkQDkYE");
pub const TOKEN_2022_ID: Pubkey = pubkey!("TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb");
pub const NAD: u64 = 1_000_000_000;
pub const BPS_DENOMINATOR: u16 = 10_000;

const TAYLOR_TERMS: u64 = 5;
const TARGET_MS_PER_SLOT: u64 = 400;
const NATURAL_LOG_OF_TWO_NAD: u64 = 693_147_180;
const MILLISECONDS_PER_YEAR: u64 = 31_536_000_000_u64;
const DIRECTIONAL_EMA_HALF_LIFE_MS: u64 = 3_000;

const OMNIPAIR_EVENT_AUTHORITY_SEED: &[u8] = b"__event_authority";
pub const FUTARCHY_AUTHORITY_SEED_PREFIX: &[u8] = b"futarchy_authority";
pub const RESERVE_VAULT_SEED_PREFIX: &[u8] = b"reserve_vault";

const PAIR_DISCRIMINATOR: &[u8] = &[85, 72, 49, 176, 182, 228, 141, 82];
const RATE_MODEL_DISCRIMINATOR: &[u8] = &[94, 3, 203, 219, 107, 137, 4, 162];
const FUTARCHY_AUTHORITY_DISCRIMINATOR: &[u8] = &[175, 247, 160, 182, 140, 128, 211, 226];
const SWAP_DISCRIMINATOR: &[u8] = &[248, 198, 158, 145, 225, 117, 135, 200];

#[derive(AnchorSerialize, AnchorDeserialize, Clone, Copy, Debug, Default)]
pub struct VaultBumps {
    pub reserve0: u8,
    pub reserve1: u8,
    pub collateral0: u8,
    pub collateral1: u8,
}

#[derive(AnchorSerialize, AnchorDeserialize, Clone, Copy, Debug, Default)]
pub struct LastPriceEma {
    pub symmetric: u64,
    pub directional: u64,
}

#[derive(AnchorSerialize, AnchorDeserialize, Clone, Debug)]
pub struct Pair {
    pub token0: Pubkey,
    pub token1: Pubkey,
    pub lp_mint: Pubkey,
    pub rate_model: Pubkey,
    pub swap_fee_bps: u16,
    pub half_life: u64,
    pub fixed_cf_bps: Option<u16>,
    pub reserve0: u64,
    pub reserve1: u64,
    pub cash_reserve0: u64,
    pub cash_reserve1: u64,
    pub last_price0_ema: LastPriceEma,
    pub last_price1_ema: LastPriceEma,
    pub last_update: u64,
    pub last_rate0: u64,
    pub last_rate1: u64,
    pub total_debt0: u64,
    pub total_debt1: u64,
    pub total_debt0_shares: u128,
    pub total_debt1_shares: u128,
    pub total_supply: u64,
    pub total_collateral0: u64,
    pub total_collateral1: u64,
    pub token0_decimals: u8,
    pub token1_decimals: u8,
    pub params_hash: [u8; 32],
    pub version: u8,
    pub bump: u8,
    pub vault_bumps: VaultBumps,
    pub reduce_only: bool,
}

impl Pair {
    pub fn key_valid_for(&self, pair_key: &Pubkey) -> bool {
        let (expected, _) = Pubkey::find_program_address(
            &[
                b"gamm_pair",
                self.token0.as_ref(),
                self.token1.as_ref(),
                self.params_hash.as_ref(),
            ],
            &OMNIPAIR_ID,
        );
        expected == *pair_key
    }

    pub fn spot_price0_nad(&self) -> u64 {
        if self.reserve0 == 0 {
            0
        } else {
            u64::try_from((self.reserve1 as u128 * NAD as u128) / self.reserve0 as u128)
                .unwrap_or(u64::MAX)
        }
    }

    pub fn spot_price1_nad(&self) -> u64 {
        if self.reserve1 == 0 {
            0
        } else {
            u64::try_from((self.reserve0 as u128 * NAD as u128) / self.reserve1 as u128)
                .unwrap_or(u64::MAX)
        }
    }

    pub fn ema_price0_nad(&self) -> Result<u64> {
        if self.reserve0 == 0 {
            Ok(0)
        } else {
            compute_ema(
                self.last_price0_ema.symmetric,
                self.last_update,
                self.spot_price0_nad(),
                self.half_life,
            )
        }
    }

    pub fn ema_price1_nad(&self) -> Result<u64> {
        if self.reserve1 == 0 {
            Ok(0)
        } else {
            compute_ema(
                self.last_price1_ema.symmetric,
                self.last_update,
                self.spot_price1_nad(),
                self.half_life,
            )
        }
    }

    pub fn update(
        &mut self,
        rate_model: &RateModel,
        futarchy_authority: &FutarchyAuthority,
    ) -> Result<()> {
        let current_slot = Clock::get()?.slot;
        let spot_price0 = self.spot_price0_nad();
        let spot_price1 = self.spot_price1_nad();

        self.last_price0_ema.directional = self.last_price0_ema.directional.min(spot_price0);
        self.last_price1_ema.directional = self.last_price1_ema.directional.min(spot_price1);

        if current_slot <= self.last_update {
            return Ok(());
        }

        let time_elapsed = slots_to_ms(self.last_update, current_slot)
            .ok_or_else(|| error!(OmnindexError::ArithmeticOverflow))?;
        if time_elapsed == 0 {
            self.last_update = current_slot;
            return Ok(());
        }

        self.last_price0_ema.symmetric = compute_ema_at(
            self.last_price0_ema.symmetric,
            spot_price0,
            self.half_life,
            time_elapsed,
        )?;
        self.last_price1_ema.symmetric = compute_ema_at(
            self.last_price1_ema.symmetric,
            spot_price1,
            self.half_life,
            time_elapsed,
        )?;

        let new_ema0 = compute_ema_at(
            self.last_price0_ema.directional,
            spot_price0,
            DIRECTIONAL_EMA_HALF_LIFE_MS,
            time_elapsed,
        )?;
        self.last_price0_ema.directional = spot_price0.min(new_ema0);

        let new_ema1 = compute_ema_at(
            self.last_price1_ema.directional,
            spot_price1,
            DIRECTIONAL_EMA_HALF_LIFE_MS,
            time_elapsed,
        )?;
        self.last_price1_ema.directional = spot_price1.min(new_ema1);

        let util0 = if self.reserve0 == 0 {
            0
        } else {
            u64::try_from((self.total_debt0 as u128 * NAD as u128) / self.reserve0 as u128)
                .unwrap_or(u64::MAX)
        };
        let util1 = if self.reserve1 == 0 {
            0
        } else {
            u64::try_from((self.total_debt1 as u128 * NAD as u128) / self.reserve1 as u128)
                .unwrap_or(u64::MAX)
        };

        let (new_rate0, integral0) =
            rate_model.calculate_rate(self.last_rate0, time_elapsed, util0);
        let (new_rate1, integral1) =
            rate_model.calculate_rate(self.last_rate1, time_elapsed, util1);
        self.last_rate0 = new_rate0;
        self.last_rate1 = new_rate1;

        let total_interest0 = ceil_div(self.total_debt0 as u128 * integral0 as u128, NAD as u128)?;
        let total_interest1 = ceil_div(self.total_debt1 as u128 * integral1 as u128, NAD as u128)?;
        let protocol_fee0 = (total_interest0
            * u128::from(futarchy_authority.revenue_share.interest_bps))
            / u128::from(BPS_DENOMINATOR);
        let protocol_fee1 = (total_interest1
            * u128::from(futarchy_authority.revenue_share.interest_bps))
            / u128::from(BPS_DENOMINATOR);
        let lp_interest0 = u64::try_from(total_interest0)
            .map_err(|_| error!(OmnindexError::ArithmeticOverflow))?;
        let lp_interest1 = u64::try_from(total_interest1)
            .map_err(|_| error!(OmnindexError::ArithmeticOverflow))?;
        let protocol_fee0 =
            u64::try_from(protocol_fee0).map_err(|_| error!(OmnindexError::ArithmeticOverflow))?;
        let protocol_fee1 =
            u64::try_from(protocol_fee1).map_err(|_| error!(OmnindexError::ArithmeticOverflow))?;

        let borrower_cost0 = total_interest0
            .checked_add(u128::from(protocol_fee0))
            .ok_or_else(|| error!(OmnindexError::ArithmeticOverflow))?;
        let borrower_cost1 = total_interest1
            .checked_add(u128::from(protocol_fee1))
            .ok_or_else(|| error!(OmnindexError::ArithmeticOverflow))?;

        self.total_debt0 = self
            .total_debt0
            .checked_add(
                u64::try_from(borrower_cost0)
                    .map_err(|_| error!(OmnindexError::ArithmeticOverflow))?,
            )
            .ok_or_else(|| error!(OmnindexError::ArithmeticOverflow))?;
        self.total_debt1 = self
            .total_debt1
            .checked_add(
                u64::try_from(borrower_cost1)
                    .map_err(|_| error!(OmnindexError::ArithmeticOverflow))?,
            )
            .ok_or_else(|| error!(OmnindexError::ArithmeticOverflow))?;

        let cash_covered_fee0 = protocol_fee0.min(self.cash_reserve0);
        let cash_covered_fee1 = protocol_fee1.min(self.cash_reserve1);

        self.reserve0 = self
            .reserve0
            .saturating_add(lp_interest0.saturating_add(protocol_fee0 - cash_covered_fee0));
        self.reserve1 = self
            .reserve1
            .saturating_add(lp_interest1.saturating_add(protocol_fee1 - cash_covered_fee1));
        self.cash_reserve0 -= cash_covered_fee0;
        self.cash_reserve1 -= cash_covered_fee1;
        self.last_update = current_slot;

        Ok(())
    }
}

#[derive(AnchorSerialize, AnchorDeserialize, Clone, Debug)]
pub struct RateModel {
    pub exp_rate: u64,
    pub target_util_start: u64,
    pub target_util_end: u64,
    pub half_life_ms: u64,
    pub min_rate: u64,
    pub max_rate: u64,
    pub initial_rate: u64,
}

impl RateModel {
    pub fn calculate_rate(&self, last_rate: u64, time_elapsed: u64, last_util: u64) -> (u64, u64) {
        let dt = time_elapsed as u128;
        if dt == 0 {
            return (last_rate, 0);
        }

        let exp_rate = self.exp_rate as u128;
        let x = exp_rate.saturating_mul(dt);
        let gd = taylor_exp(-(x as i64), NAD, TAYLOR_TERMS) as u128;
        let min_nad = self.min_rate as u128;
        let max_nad = self.max_rate as u128;
        let has_max_cap = max_nad > 0;
        let last = (last_rate as u128).max(min_nad);

        if (last_util as u128) > (self.target_util_end as u128) {
            let curr_unclamped = last.saturating_mul(NAD as u128) / gd.max(1);
            let curr = if has_max_cap && curr_unclamped > max_nad {
                if last >= max_nad {
                    let integral =
                        ceil_div_u128(max_nad.saturating_mul(dt), MILLISECONDS_PER_YEAR as u128);
                    return (
                        max_nad.min(u64::MAX as u128) as u64,
                        integral.min(u64::MAX as u128) as u64,
                    );
                }
                let t_to_max =
                    Self::time_to_reach_closed_form(last, max_nad, exp_rate, true).min(dt);
                let exp_part = ceil_div_u128(
                    max_nad.saturating_sub(last).saturating_mul(NAD as u128),
                    exp_rate,
                );
                let flat_part = max_nad.saturating_mul(dt.saturating_sub(t_to_max));
                let integral = ceil_div_u128(exp_part + flat_part, MILLISECONDS_PER_YEAR as u128);
                return (
                    max_nad.min(u64::MAX as u128) as u64,
                    integral.min(u64::MAX as u128) as u64,
                );
            } else {
                curr_unclamped
            };

            let numer = curr.saturating_sub(last).saturating_mul(NAD as u128);
            let integral_pre = numer / exp_rate.max(1);
            let integral = ceil_div_u128(integral_pre, MILLISECONDS_PER_YEAR as u128);
            return (
                curr.min(u64::MAX as u128) as u64,
                integral.min(u64::MAX as u128) as u64,
            );
        }

        if (last_util as u128) < (self.target_util_start as u128) {
            let r1_unclamped = last.saturating_mul(gd) / (NAD as u128);

            if r1_unclamped >= min_nad {
                let numer = last
                    .saturating_sub(r1_unclamped)
                    .saturating_mul(NAD as u128);
                let integral_pre = numer / exp_rate.max(1);
                let integral = ceil_div_u128(integral_pre, MILLISECONDS_PER_YEAR as u128);
                return (
                    r1_unclamped.min(u64::MAX as u128) as u64,
                    integral.min(u64::MAX as u128) as u64,
                );
            }

            if last <= min_nad {
                let integral =
                    ceil_div_u128(min_nad.saturating_mul(dt), MILLISECONDS_PER_YEAR as u128);
                return (
                    min_nad.min(u64::MAX as u128) as u64,
                    integral.min(u64::MAX as u128) as u64,
                );
            }

            let t_to_min = Self::time_to_reach_closed_form(last, min_nad, exp_rate, false).min(dt);
            let exp_part = ceil_div_u128(
                last.saturating_sub(min_nad).saturating_mul(NAD as u128),
                exp_rate,
            );
            let flat_part = min_nad.saturating_mul(dt.saturating_sub(t_to_min));
            let integral = ceil_div_u128(exp_part + flat_part, MILLISECONDS_PER_YEAR as u128);
            return (
                min_nad.min(u64::MAX as u128) as u64,
                integral.min(u64::MAX as u128) as u64,
            );
        }

        let integral = ceil_div_u128(last.saturating_mul(dt), MILLISECONDS_PER_YEAR as u128);
        (
            last.min(u64::MAX as u128) as u64,
            integral.min(u64::MAX as u128) as u64,
        )
    }

    fn time_to_reach_closed_form(r0: u128, target: u128, exp_rate: u128, up: bool) -> u128 {
        if exp_rate == 0 {
            return 0;
        }
        let ratio_nad = if up {
            if target <= r0 {
                return 0;
            }
            (target.saturating_mul(NAD as u128)) / r0.max(1)
        } else {
            if r0 <= target {
                return 0;
            }
            (r0.saturating_mul(NAD as u128)) / target.max(1)
        };
        let ratio_nad_u64 = u64::try_from(ratio_nad).unwrap_or(u64::MAX);
        if ratio_nad_u64 == 0 {
            return 0;
        }
        let ln_ratio = Self::ln_nad(ratio_nad_u64);
        let t = ln_ratio / (exp_rate as i128);
        if t <= 0 {
            0
        } else {
            t as u128
        }
    }

    fn ln_nad(x_nad: u64) -> i128 {
        let mut z = x_nad as u128;
        let mut k: i128 = 0;

        while z < (NAD as u128) / 2 {
            z = z.saturating_mul(2);
            k -= 1;
        }
        while z >= (NAD as u128) * 2 {
            z /= 2;
            k += 1;
        }

        let z_i = z as i128;
        let num = (z_i - NAD as i128) * NAD as i128;
        let den = (z_i + NAD as i128).max(1);
        let v = num / den;
        let v2 = (v * v) / (NAD as i128);
        let v3 = (v2 * v) / (NAD as i128);
        let v5 = (v3 * v2) / (NAD as i128);
        let v7 = (v5 * v2) / (NAD as i128);
        let v9 = (v7 * v2) / (NAD as i128);
        let series = v + v3 / 3 + v5 / 5 + v7 / 7 + v9 / 9;
        2 * series + k * NATURAL_LOG_OF_TWO_NAD as i128
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, AnchorSerialize, AnchorDeserialize)]
pub struct RevenueShare {
    pub swap_bps: u16,
    pub interest_bps: u16,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, AnchorSerialize, AnchorDeserialize)]
pub struct RevenueRecipients {
    pub futarchy_treasury: Pubkey,
    pub buybacks_vault: Pubkey,
    pub team_treasury: Pubkey,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, AnchorSerialize, AnchorDeserialize)]
pub struct RevenueDistribution {
    pub futarchy_treasury_bps: u16,
    pub buybacks_vault_bps: u16,
    pub team_treasury_bps: u16,
}

#[derive(AnchorSerialize, AnchorDeserialize, Clone, Debug)]
pub struct FutarchyAuthority {
    pub version: u8,
    pub authority: Pubkey,
    pub recipients: RevenueRecipients,
    pub revenue_share: RevenueShare,
    pub revenue_distribution: RevenueDistribution,
    pub global_reduce_only: bool,
    pub bump: u8,
}

#[derive(AnchorSerialize, AnchorDeserialize, Clone, Debug)]
pub struct SwapArgs {
    pub amount_in: u64,
    pub min_amount_out: u64,
}

pub struct SwapAccounts<'info> {
    pub pair: AccountInfo<'info>,
    pub rate_model: AccountInfo<'info>,
    pub futarchy_authority: AccountInfo<'info>,
    pub token_in_vault: AccountInfo<'info>,
    pub token_out_vault: AccountInfo<'info>,
    pub user_token_in_account: AccountInfo<'info>,
    pub user_token_out_account: AccountInfo<'info>,
    pub token_in_mint: AccountInfo<'info>,
    pub token_out_mint: AccountInfo<'info>,
    pub user: AccountInfo<'info>,
    pub token_program: AccountInfo<'info>,
    pub token_2022_program: AccountInfo<'info>,
    pub event_authority: AccountInfo<'info>,
}

pub fn load_pair(info: &AccountInfo) -> Result<Pair> {
    let pair: Pair = deserialize_omnipair_account(info, PAIR_DISCRIMINATOR)?;
    require!(
        pair.key_valid_for(info.key),
        OmnindexError::InvalidOmnipairPair
    );
    Ok(pair)
}

pub fn load_rate_model(info: &AccountInfo) -> Result<RateModel> {
    deserialize_omnipair_account(info, RATE_MODEL_DISCRIMINATOR)
}

pub fn load_futarchy_authority(info: &AccountInfo) -> Result<FutarchyAuthority> {
    deserialize_omnipair_account(info, FUTARCHY_AUTHORITY_DISCRIMINATOR)
}

pub fn swap<'info>(
    program: AccountInfo<'info>,
    accounts: SwapAccounts<'info>,
    args: SwapArgs,
    signer_seeds: &[&[&[u8]]],
) -> Result<()> {
    require_keys_eq!(
        *program.key,
        OMNIPAIR_ID,
        OmnindexError::InvalidOmnipairProgram
    );

    let mut data = Vec::with_capacity(24);
    data.extend_from_slice(SWAP_DISCRIMINATOR);
    args.serialize(&mut data)
        .map_err(|_| error!(OmnindexError::ArithmeticOverflow))?;

    let metas = vec![
        AccountMeta::new(*accounts.pair.key, false),
        AccountMeta::new(*accounts.rate_model.key, false),
        AccountMeta::new_readonly(*accounts.futarchy_authority.key, false),
        AccountMeta::new(*accounts.token_in_vault.key, false),
        AccountMeta::new(*accounts.token_out_vault.key, false),
        AccountMeta::new(*accounts.user_token_in_account.key, false),
        AccountMeta::new(*accounts.user_token_out_account.key, false),
        AccountMeta::new_readonly(*accounts.token_in_mint.key, false),
        AccountMeta::new_readonly(*accounts.token_out_mint.key, false),
        AccountMeta::new_readonly(*accounts.user.key, true),
        AccountMeta::new_readonly(*accounts.token_program.key, false),
        AccountMeta::new_readonly(*accounts.token_2022_program.key, false),
        AccountMeta::new_readonly(*accounts.event_authority.key, false),
        AccountMeta::new_readonly(*program.key, false),
    ];

    let infos = [
        accounts.pair,
        accounts.rate_model,
        accounts.futarchy_authority,
        accounts.token_in_vault,
        accounts.token_out_vault,
        accounts.user_token_in_account,
        accounts.user_token_out_account,
        accounts.token_in_mint,
        accounts.token_out_mint,
        accounts.user,
        accounts.token_program,
        accounts.token_2022_program,
        accounts.event_authority,
        program,
    ];

    invoke_signed(
        &Instruction {
            program_id: OMNIPAIR_ID,
            accounts: metas,
            data,
        },
        &infos,
        signer_seeds,
    )
    .map_err(Into::into)
}

pub fn omnipair_event_authority_address() -> Pubkey {
    Pubkey::find_program_address(&[OMNIPAIR_EVENT_AUTHORITY_SEED], &OMNIPAIR_ID).0
}

pub fn omnipair_futarchy_authority_address() -> Pubkey {
    Pubkey::find_program_address(&[FUTARCHY_AUTHORITY_SEED_PREFIX], &OMNIPAIR_ID).0
}

pub fn reserve_vault_address(pair: &Pubkey, mint: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(
        &[RESERVE_VAULT_SEED_PREFIX, pair.as_ref(), mint.as_ref()],
        &OMNIPAIR_ID,
    )
    .0
}

pub fn quote_exact_input_for_pair_output(
    pair: &Pair,
    rate_model: &RateModel,
    futarchy_authority: &FutarchyAuthority,
    quote_mint: &Pubkey,
    output_mint: &Pubkey,
    output_amount: u64,
) -> Result<u64> {
    require!(output_amount > 0, OmnindexError::InvalidIndexAmount);

    let mut preview = pair.clone();
    preview.update(rate_model, futarchy_authority)?;

    let (reserve_in, reserve_out) =
        if preview.token0 == *quote_mint && preview.token1 == *output_mint {
            (preview.reserve0, preview.reserve1)
        } else if preview.token1 == *quote_mint && preview.token0 == *output_mint {
            (preview.reserve1, preview.reserve0)
        } else {
            return err!(OmnindexError::InvalidOmnipairPair);
        };

    let net_input = calculate_amount_in(reserve_in, reserve_out, output_amount)?;
    gross_input_for_net_input(net_input, preview.swap_fee_bps)
}

pub fn quote_exact_output_for_pair_input(
    pair: &Pair,
    rate_model: &RateModel,
    futarchy_authority: &FutarchyAuthority,
    input_mint: &Pubkey,
    quote_mint: &Pubkey,
    input_amount: u64,
) -> Result<u64> {
    require!(input_amount > 0, OmnindexError::InvalidIndexAmount);

    let mut preview = pair.clone();
    preview.update(rate_model, futarchy_authority)?;

    let (reserve_in, reserve_out) =
        if preview.token0 == *input_mint && preview.token1 == *quote_mint {
            (preview.reserve0, preview.reserve1)
        } else if preview.token1 == *input_mint && preview.token0 == *quote_mint {
            (preview.reserve1, preview.reserve0)
        } else {
            return err!(OmnindexError::InvalidOmnipairPair);
        };

    let swap_fee = ceil_div(
        u128::from(input_amount)
            .checked_mul(u128::from(preview.swap_fee_bps))
            .ok_or_else(|| error!(OmnindexError::ArithmeticOverflow))?,
        u128::from(BPS_DENOMINATOR),
    )?;
    let amount_in_after_fee = u128::from(input_amount)
        .checked_sub(swap_fee)
        .ok_or_else(|| error!(OmnindexError::ArithmeticOverflow))?;

    calculate_amount_out(
        reserve_in,
        reserve_out,
        u64::try_from(amount_in_after_fee)
            .map_err(|_| error!(OmnindexError::ArithmeticOverflow))?,
    )
}

pub fn oracle_price_for_component(
    pair: &Pair,
    rate_model: &RateModel,
    futarchy_authority: &FutarchyAuthority,
    component_mint: &Pubkey,
    quote_mint: &Pubkey,
) -> Result<u64> {
    let (ema_price, _) = oracle_spot_and_ema_price_for_component(
        pair,
        rate_model,
        futarchy_authority,
        component_mint,
        quote_mint,
    )?;
    Ok(ema_price)
}

pub fn oracle_spot_and_ema_price_for_component(
    pair: &Pair,
    rate_model: &RateModel,
    futarchy_authority: &FutarchyAuthority,
    component_mint: &Pubkey,
    quote_mint: &Pubkey,
) -> Result<(u64, u64)> {
    let mut preview = pair.clone();
    preview.update(rate_model, futarchy_authority)?;

    let (ema_price, spot_price) =
        if preview.token0 == *component_mint && preview.token1 == *quote_mint {
            (preview.ema_price0_nad()?, preview.spot_price0_nad())
        } else if preview.token1 == *component_mint && preview.token0 == *quote_mint {
            (preview.ema_price1_nad()?, preview.spot_price1_nad())
        } else {
            return err!(OmnindexError::InvalidOmnipairPair);
        };

    require!(
        ema_price > 0 && spot_price > 0,
        OmnindexError::InvalidOmnipairOraclePrice
    );
    Ok((ema_price, spot_price))
}

pub fn spot_ema_deviation_bps(spot_price: u64, ema_price: u64) -> Result<u64> {
    require!(
        spot_price > 0 && ema_price > 0,
        OmnindexError::InvalidOmnipairOraclePrice
    );
    let delta = u128::from(spot_price.abs_diff(ema_price));
    let deviation = ceil_div(
        delta
            .checked_mul(u128::from(BPS_DENOMINATOR))
            .ok_or_else(|| error!(OmnindexError::ArithmeticOverflow))?,
        u128::from(ema_price),
    )?;
    u64::try_from(deviation).map_err(|_| error!(OmnindexError::ArithmeticOverflow))
}

pub fn validate_spot_ema_deviation(
    spot_price: u64,
    ema_price: u64,
    max_deviation_bps: u16,
) -> Result<()> {
    let deviation_bps = spot_ema_deviation_bps(spot_price, ema_price)?;
    require!(
        deviation_bps <= u64::from(max_deviation_bps),
        OmnindexError::SpotPriceOutsideEmaTolerance
    );
    Ok(())
}

pub fn gross_input_for_net_input(net_input: u64, fee_bps: u16) -> Result<u64> {
    if fee_bps == 0 {
        return Ok(net_input);
    }

    let bps = u128::from(BPS_DENOMINATOR);
    let fee = u128::from(fee_bps);
    require!(fee < bps, OmnindexError::ArithmeticOverflow);

    let net = u128::from(net_input);
    let mut lo = net;
    let mut hi = net
        .checked_mul(bps)
        .ok_or_else(|| error!(OmnindexError::ArithmeticOverflow))?
        .checked_div(
            bps.checked_sub(fee)
                .ok_or_else(|| error!(OmnindexError::ArithmeticOverflow))?,
        )
        .ok_or_else(|| error!(OmnindexError::ArithmeticOverflow))?
        .checked_add(2)
        .ok_or_else(|| error!(OmnindexError::ArithmeticOverflow))?;

    while lo < hi {
        let mid = lo + ((hi - lo) / 2);
        let swap_fee = ceil_div(
            mid.checked_mul(fee)
                .ok_or_else(|| error!(OmnindexError::ArithmeticOverflow))?,
            bps,
        )?;
        let effective_input = mid
            .checked_sub(swap_fee)
            .ok_or_else(|| error!(OmnindexError::ArithmeticOverflow))?;

        if effective_input >= net {
            hi = mid;
        } else {
            lo = mid
                .checked_add(1)
                .ok_or_else(|| error!(OmnindexError::ArithmeticOverflow))?;
        }
    }

    u64::try_from(lo).map_err(|_| error!(OmnindexError::ArithmeticOverflow))
}

fn deserialize_omnipair_account<T: AnchorDeserialize>(
    info: &AccountInfo,
    discriminator: &[u8],
) -> Result<T> {
    require_keys_eq!(
        *info.owner,
        OMNIPAIR_ID,
        OmnindexError::InvalidOmnipairProgram
    );
    let data = info.try_borrow_data()?;
    require!(
        data.len() >= discriminator.len(),
        OmnindexError::InvalidOmnipairPair
    );
    require!(
        &data[..discriminator.len()] == discriminator,
        OmnindexError::InvalidOmnipairPair
    );

    T::deserialize(&mut &data[discriminator.len()..])
        .map_err(|_| error!(OmnindexError::InvalidOmnipairPair))
}

fn slots_to_ms(start_slot: u64, end_slot: u64) -> Option<u64> {
    end_slot
        .checked_sub(start_slot)?
        .checked_mul(TARGET_MS_PER_SLOT)
}

fn compute_ema(last_ema: u64, last_update: u64, input: u64, half_life: u64) -> Result<u64> {
    let dt = slots_to_ms(last_update, Clock::get()?.slot)
        .ok_or_else(|| error!(OmnindexError::ArithmeticOverflow))?;
    compute_ema_at(last_ema, input, half_life, dt)
}

fn compute_ema_at(last_ema: u64, input: u64, half_life: u64, dt: u64) -> Result<u64> {
    if dt == 0 || half_life == 0 {
        return Ok(last_ema);
    }

    let x = (dt as u128 * NATURAL_LOG_OF_TWO_NAD as u128) / half_life as u128;
    let alpha = taylor_exp(-(x as i64), NAD, TAYLOR_TERMS);
    let result = (u128::from(input) * u128::from(NAD - alpha)
        + u128::from(last_ema) * u128::from(alpha))
        / u128::from(NAD);
    u64::try_from(result).map_err(|_| error!(OmnindexError::ArithmeticOverflow))
}

fn taylor_exp(x: i64, scale: u64, precision: u64) -> u64 {
    let is_negative = x < 0;
    let abs_x = if is_negative { -x } else { x };
    let n = 10u64;
    let reduced_x = abs_x / (n as i64);
    let mut term = scale as u128;
    let mut sum = scale as u128;

    for i in 1..=precision {
        term = term
            .checked_mul(reduced_x as u128)
            .and_then(|t| t.checked_div(i as u128 * scale as u128))
            .unwrap_or(0);
        sum = sum.saturating_add(term);
    }

    let mut result = scale as u128;
    for _ in 0..n {
        result = result
            .checked_mul(sum)
            .and_then(|r| r.checked_div(scale as u128))
            .unwrap_or(u128::MAX);
    }

    if is_negative {
        result = (scale as u128 * scale as u128) / result.max(1);
    }

    result as u64
}

fn calculate_amount_out(reserve_in: u64, reserve_out: u64, amount_in: u64) -> Result<u64> {
    let denominator = u128::from(reserve_in)
        .checked_add(u128::from(amount_in))
        .ok_or_else(|| error!(OmnindexError::ArithmeticOverflow))?;
    let amount_out = u128::from(amount_in)
        .checked_mul(u128::from(reserve_out))
        .ok_or_else(|| error!(OmnindexError::ArithmeticOverflow))?
        .checked_div(denominator)
        .ok_or_else(|| error!(OmnindexError::ArithmeticOverflow))?;
    u64::try_from(amount_out).map_err(|_| error!(OmnindexError::ArithmeticOverflow))
}

fn calculate_amount_in(reserve_in: u64, reserve_out: u64, amount_out: u64) -> Result<u64> {
    let denominator = u128::from(reserve_out)
        .checked_sub(u128::from(amount_out))
        .ok_or_else(|| error!(OmnindexError::ArithmeticOverflow))?;
    let numerator = u128::from(amount_out)
        .checked_mul(u128::from(reserve_in))
        .ok_or_else(|| error!(OmnindexError::ArithmeticOverflow))?;
    u64::try_from(ceil_div(numerator, denominator)?)
        .map_err(|_| error!(OmnindexError::ArithmeticOverflow))
}

fn ceil_div(numerator: u128, denominator: u128) -> Result<u128> {
    require!(denominator > 0, OmnindexError::ArithmeticOverflow);
    numerator
        .checked_add(denominator - 1)
        .and_then(|value| value.checked_div(denominator))
        .ok_or_else(|| error!(OmnindexError::ArithmeticOverflow))
}

fn ceil_div_u128(numerator: u128, denominator: u128) -> u128 {
    if denominator == 0 {
        return 0;
    }
    numerator
        .checked_add(denominator - 1)
        .and_then(|value| value.checked_div(denominator))
        .unwrap_or(numerator / denominator)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gross_input_covers_net_after_fee_rounding() {
        let gross = gross_input_for_net_input(1_000, 30).unwrap();
        let fee = (u128::from(gross) * 30).div_ceil(10_000);
        let effective = u128::from(gross) - fee;
        assert!(effective >= 1_000);
    }

    #[test]
    fn gross_input_is_minimal_for_small_example() {
        let gross = gross_input_for_net_input(997, 30).unwrap();
        assert_eq!(gross, 1_000);
    }

    #[test]
    fn spot_ema_deviation_uses_ema_as_denominator() {
        assert_eq!(spot_ema_deviation_bps(105, 100).unwrap(), 500);
        assert_eq!(spot_ema_deviation_bps(95, 100).unwrap(), 500);
    }

    #[test]
    fn spot_ema_deviation_rounds_up() {
        assert_eq!(spot_ema_deviation_bps(1001, 999).unwrap(), 21);
    }

    #[test]
    fn validate_spot_ema_deviation_rejects_above_limit() {
        assert!(validate_spot_ema_deviation(106, 100, 500).is_err());
        assert!(validate_spot_ema_deviation(105, 100, 500).is_ok());
    }
}
