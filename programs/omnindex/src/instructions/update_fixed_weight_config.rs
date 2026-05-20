use anchor_lang::prelude::*;

use crate::{
    errors::OmnindexError,
    events::FixedWeightConfigUpdated,
    state::{IndexComponentInput, IndexKind, IndexState},
    utils::{
        load_mint, validate_component_inputs, validate_index_strategy_config,
        validate_no_self_component,
    },
};

#[derive(AnchorSerialize, AnchorDeserialize, Clone, Debug)]
pub struct UpdateFixedWeightConfigArgs {
    pub fixed_weight_quote_mint: Pubkey,
    pub fixed_weight_rebalance_interval_seconds: i64,
    pub fixed_weight_drift_threshold_bps: u16,
    pub fixed_weight_spot_ema_max_deviation_bps: u16,
    pub components: Vec<IndexComponentInput>,
}

#[derive(Accounts)]
pub struct UpdateFixedWeightConfig<'info> {
    pub authority: Signer<'info>,
    #[account(
        mut,
        has_one = authority @ OmnindexError::UnauthorizedAuthority,
        has_one = index_mint @ OmnindexError::IndexMintMismatch
    )]
    pub index: Account<'info, IndexState>,
    /// CHECK: Validated as the configured classic SPL Token mint in the handler.
    pub index_mint: UncheckedAccount<'info>,
}

impl<'info> UpdateFixedWeightConfig<'info> {
    pub fn handle(ctx: Context<Self>, args: UpdateFixedWeightConfigArgs) -> Result<()> {
        require!(
            ctx.accounts.index.kind == IndexKind::FixedWeights,
            OmnindexError::InvalidIndexKind
        );

        let index_mint = load_mint(&ctx.accounts.index_mint.to_account_info())?;
        let components = validate_component_inputs(args.components)?;
        validate_no_self_component(&components, &ctx.accounts.index_mint.key())?;
        validate_index_strategy_config(
            IndexKind::FixedWeights,
            &components,
            args.fixed_weight_quote_mint,
            args.fixed_weight_rebalance_interval_seconds,
            args.fixed_weight_drift_threshold_bps,
            args.fixed_weight_spot_ema_max_deviation_bps,
        )?;

        if index_mint.supply > 0 {
            require!(
                components.len() == ctx.accounts.index.components.len(),
                OmnindexError::InvalidComponentCount
            );
            for (old, new) in ctx.accounts.index.components.iter().zip(components.iter()) {
                require_keys_eq!(old.mint, new.mint, OmnindexError::InvalidComponentMint);
                require!(
                    old.units_per_index == new.units_per_index,
                    OmnindexError::InvalidFixedWeightConfig
                );
            }
        }

        let now = Clock::get()?.unix_timestamp;
        let index = &mut ctx.accounts.index;
        index.fixed_weight_quote_mint = args.fixed_weight_quote_mint;
        index.fixed_weight_rebalance_interval_seconds =
            args.fixed_weight_rebalance_interval_seconds;
        index.fixed_weight_drift_threshold_bps = args.fixed_weight_drift_threshold_bps;
        index.fixed_weight_spot_ema_max_deviation_bps =
            args.fixed_weight_spot_ema_max_deviation_bps;
        index.fixed_weight_last_rebalanced_at = now;
        index.component_count = components.len() as u8;
        index.components = components;

        emit!(FixedWeightConfigUpdated {
            index: index.key(),
            authority: ctx.accounts.authority.key(),
            quote_mint: index.fixed_weight_quote_mint,
            components: index.component_count,
            rebalance_interval_seconds: index.fixed_weight_rebalance_interval_seconds,
            drift_threshold_bps: index.fixed_weight_drift_threshold_bps,
            spot_ema_max_deviation_bps: index.fixed_weight_spot_ema_max_deviation_bps,
        });

        Ok(())
    }
}
