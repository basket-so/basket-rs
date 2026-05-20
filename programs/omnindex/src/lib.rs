#![allow(deprecated)]
#![allow(unexpected_cfgs)]

use anchor_lang::prelude::*;

pub mod constants;
pub mod errors;
pub mod events;
pub mod instructions;
pub mod state;
pub mod utils;

pub use instructions::*;

declare_id!("H6JKCZU82gCADQZ98Jmfj7UzpHTdbyfnDpt7AD5NZ3Lt");

#[program]
pub mod omnindex {
    use super::*;

    pub fn initialize_protocol(
        ctx: Context<InitializeProtocol>,
        args: InitializeProtocolArgs,
    ) -> Result<()> {
        InitializeProtocol::handle(ctx, args)
    }

    pub fn update_protocol_config(
        ctx: Context<UpdateProtocolConfig>,
        args: UpdateProtocolConfigArgs,
    ) -> Result<()> {
        UpdateProtocolConfig::handle(ctx, args)
    }

    pub fn create_index<'info>(
        ctx: Context<'_, '_, 'info, 'info, CreateIndex<'info>>,
        args: CreateIndexArgs,
    ) -> Result<()> {
        CreateIndex::handle(ctx, args)
    }

    pub fn initialize_vaults<'info>(
        ctx: Context<'_, '_, 'info, 'info, InitializeVaults<'info>>,
    ) -> Result<()> {
        InitializeVaults::handle(ctx)
    }

    pub fn initialize_staking_pool<'info>(
        ctx: Context<'_, '_, 'info, 'info, InitializeStakingPool<'info>>,
    ) -> Result<()> {
        InitializeStakingPool::handle(ctx)
    }

    pub fn stake_basket(ctx: Context<StakeBasket>, args: StakeBasketArgs) -> Result<()> {
        StakeBasket::handle(ctx, args)
    }

    pub fn unstake_basket(ctx: Context<UnstakeBasket>, args: UnstakeBasketArgs) -> Result<()> {
        UnstakeBasket::handle(ctx, args)
    }

    pub fn claim_staking_rewards(ctx: Context<ClaimStakingRewards>) -> Result<()> {
        ClaimStakingRewards::handle(ctx)
    }

    pub fn mint_index<'info>(
        ctx: Context<'_, '_, 'info, 'info, MintIndex<'info>>,
        args: MintIndexArgs,
    ) -> Result<()> {
        MintIndex::handle(ctx, args)
    }

    pub fn mint_index_with_quote<'info>(
        ctx: Context<'_, '_, 'info, 'info, MintIndexWithQuote<'info>>,
        args: MintIndexWithQuoteArgs,
    ) -> Result<()> {
        MintIndexWithQuote::handle(ctx, args)
    }

    pub fn update_fees(ctx: Context<UpdateFees>, args: UpdateFeesArgs) -> Result<()> {
        UpdateFees::handle(ctx, args)
    }

    pub fn update_config(ctx: Context<UpdateConfig>, args: UpdateConfigArgs) -> Result<()> {
        UpdateConfig::handle(ctx, args)
    }

    pub fn update_fixed_weight_config(
        ctx: Context<UpdateFixedWeightConfig>,
        args: UpdateFixedWeightConfigArgs,
    ) -> Result<()> {
        UpdateFixedWeightConfig::handle(ctx, args)
    }

    pub fn update_authority(
        ctx: Context<UpdateAuthority>,
        args: UpdateAuthorityArgs,
    ) -> Result<()> {
        UpdateAuthority::handle(ctx, args)
    }

    pub fn create_index_metadata(
        ctx: Context<CreateIndexMetadata>,
        args: IndexMetadataArgs,
    ) -> Result<()> {
        CreateIndexMetadata::handle(ctx, args)
    }

    pub fn update_index_metadata(
        ctx: Context<UpdateIndexMetadata>,
        args: IndexMetadataArgs,
    ) -> Result<()> {
        UpdateIndexMetadata::handle(ctx, args)
    }

    pub fn migrate_index_metadata_authority(
        ctx: Context<MigrateIndexMetadataAuthority>,
    ) -> Result<()> {
        MigrateIndexMetadataAuthority::handle(ctx)
    }

    pub fn claim_fees<'info>(ctx: Context<'_, '_, 'info, 'info, ClaimFees<'info>>) -> Result<()> {
        ClaimFees::handle(ctx)
    }

    pub fn quote_mint_index<'info>(
        ctx: Context<'_, '_, 'info, 'info, QuoteIndex<'info>>,
        args: QuoteIndexArgs,
    ) -> Result<()> {
        QuoteIndex::quote_mint(ctx, args)
    }

    pub fn quote_redeem_index<'info>(
        ctx: Context<'_, '_, 'info, 'info, QuoteIndex<'info>>,
        args: QuoteIndexArgs,
    ) -> Result<()> {
        QuoteIndex::quote_redeem(ctx, args)
    }

    pub fn propose_rebalance<'info>(
        ctx: Context<'_, '_, 'info, 'info, ProposeRebalance<'info>>,
        args: ProposeRebalanceArgs,
    ) -> Result<()> {
        ProposeRebalance::handle(ctx, args)
    }

    pub fn cancel_rebalance(ctx: Context<CancelRebalance>) -> Result<()> {
        CancelRebalance::handle(ctx)
    }

    pub fn execute_rebalance<'info>(
        ctx: Context<'_, '_, 'info, 'info, ExecuteRebalance<'info>>,
        args: ExecuteRebalanceArgs,
    ) -> Result<()> {
        ExecuteRebalance::handle(ctx, args)
    }

    pub fn rebalance_fixed_weights<'info>(
        ctx: Context<'_, '_, 'info, 'info, RebalanceFixedWeights<'info>>,
        args: RebalanceFixedWeightsArgs,
    ) -> Result<()> {
        RebalanceFixedWeights::handle(ctx, args)
    }

    pub fn redeem_index<'info>(
        ctx: Context<'_, '_, 'info, 'info, RedeemIndex<'info>>,
        args: RedeemIndexArgs,
    ) -> Result<()> {
        RedeemIndex::handle(ctx, args)
    }

    pub fn redeem_index_to_quote<'info>(
        ctx: Context<'_, '_, 'info, 'info, RedeemIndexToQuote<'info>>,
        args: RedeemIndexToQuoteArgs,
    ) -> Result<()> {
        RedeemIndexToQuote::handle(ctx, args)
    }
}
