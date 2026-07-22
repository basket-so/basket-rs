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

declare_id!("9LNEoShrH93XekWQTFmZBdUdMu8ugJxBr5cbfqJQC1mw");

#[program]
pub mod basket {
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

    pub fn update_index_creator_whitelist(
        ctx: Context<UpdateIndexCreatorWhitelist>,
        args: UpdateIndexCreatorWhitelistArgs,
    ) -> Result<()> {
        UpdateIndexCreatorWhitelist::handle(ctx, args)
    }

    pub fn create_large_basket_index<'info>(
        ctx: Context<'_, '_, 'info, 'info, CreateLargeBasketIndex<'info>>,
        args: CreateLargeBasketIndexArgs,
    ) -> Result<()> {
        CreateLargeBasketIndex::handle(ctx, args)
    }

    pub fn initialize_large_basket_component_page<'info>(
        ctx: Context<'_, '_, 'info, 'info, InitializeLargeBasketComponentPage<'info>>,
        args: InitializeLargeBasketComponentPageArgs,
    ) -> Result<()> {
        InitializeLargeBasketComponentPage::handle(ctx, args)
    }

    pub fn finalize_large_basket_config<'info>(
        ctx: Context<'_, '_, 'info, 'info, FinalizeLargeBasketConfig<'info>>,
    ) -> Result<()> {
        FinalizeLargeBasketConfig::handle(ctx)
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

    pub fn fund_staking_rewards(
        ctx: Context<FundStakingRewards>,
        args: FundStakingRewardsArgs,
    ) -> Result<()> {
        FundStakingRewards::handle(ctx, args)
    }

    pub fn open_large_basket_mint_intent<'info>(
        ctx: Context<'_, '_, 'info, 'info, OpenLargeBasketMintIntent<'info>>,
        args: OpenLargeBasketMintIntentArgs,
    ) -> Result<()> {
        OpenLargeBasketMintIntent::handle(ctx, args)
    }

    pub fn open_large_basket_redeem_intent<'info>(
        ctx: Context<'_, '_, 'info, 'info, OpenLargeBasketRedeemIntent<'info>>,
        args: OpenLargeBasketRedeemIntentArgs,
    ) -> Result<()> {
        OpenLargeBasketRedeemIntent::handle(ctx, args)
    }

    pub fn collect_large_basket_intent_fees<'info>(
        ctx: Context<'_, '_, 'info, 'info, CollectLargeBasketIntentFees<'info>>,
    ) -> Result<()> {
        CollectLargeBasketIntentFees::handle(ctx)
    }

    pub fn execute_large_basket_mint_component<'info>(
        ctx: Context<'_, '_, 'info, 'info, ExecuteLargeBasketMintComponent<'info>>,
        args: ExecuteLargeBasketMintComponentArgs,
    ) -> Result<()> {
        ExecuteLargeBasketMintComponent::handle(ctx, args)
    }

    pub fn execute_large_basket_redeem_component<'info>(
        ctx: Context<'_, '_, 'info, 'info, ExecuteLargeBasketRedeemComponent<'info>>,
        args: ExecuteLargeBasketRedeemComponentArgs,
    ) -> Result<()> {
        ExecuteLargeBasketRedeemComponent::handle(ctx, args)
    }

    pub fn execute_large_basket_mint_component_in_kind<'info>(
        ctx: Context<'_, '_, 'info, 'info, ExecuteLargeBasketMintComponentInKind<'info>>,
        args: ExecuteLargeBasketMintComponentInKindArgs,
    ) -> Result<()> {
        ExecuteLargeBasketMintComponentInKind::handle(ctx, args)
    }

    pub fn execute_large_basket_redeem_component_in_kind<'info>(
        ctx: Context<'_, '_, 'info, 'info, ExecuteLargeBasketRedeemComponentInKind<'info>>,
        args: ExecuteLargeBasketRedeemComponentInKindArgs,
    ) -> Result<()> {
        ExecuteLargeBasketRedeemComponentInKind::handle(ctx, args)
    }

    pub fn execute_large_basket_mint_batch<'info>(
        ctx: Context<'_, '_, 'info, 'info, ExecuteLargeBasketComponentBatch<'info>>,
        args: ExecuteLargeBasketMintBatchArgs,
    ) -> Result<()> {
        ExecuteLargeBasketComponentBatch::handle_mint(ctx, args)
    }

    pub fn execute_large_basket_redeem_batch<'info>(
        ctx: Context<'_, '_, 'info, 'info, ExecuteLargeBasketComponentBatch<'info>>,
        args: ExecuteLargeBasketRedeemBatchArgs,
    ) -> Result<()> {
        ExecuteLargeBasketComponentBatch::handle_redeem(ctx, args)
    }

    pub fn verify_large_basket_mint_component_price<'info>(
        ctx: Context<'_, '_, 'info, 'info, VerifyLargeBasketComponentPrice<'info>>,
        args: VerifyLargeBasketComponentPriceArgs,
    ) -> Result<()> {
        VerifyLargeBasketComponentPrice::handle_mint(ctx, args)
    }

    pub fn verify_large_basket_redeem_component_price<'info>(
        ctx: Context<'_, '_, 'info, 'info, VerifyLargeBasketComponentPrice<'info>>,
        args: VerifyLargeBasketComponentPriceArgs,
    ) -> Result<()> {
        VerifyLargeBasketComponentPrice::handle_redeem(ctx, args)
    }

    pub fn finalize_large_basket_mint_intent<'info>(
        ctx: Context<'_, '_, 'info, 'info, FinalizeLargeBasketMintIntent<'info>>,
    ) -> Result<()> {
        FinalizeLargeBasketMintIntent::handle(ctx)
    }

    pub fn finalize_large_basket_redeem_intent(
        ctx: Context<FinalizeLargeBasketRedeemIntent>,
    ) -> Result<()> {
        FinalizeLargeBasketRedeemIntent::handle(ctx)
    }

    pub fn cancel_unfilled_large_basket_mint_intent(
        ctx: Context<CancelUnfilledLargeBasketMintIntent>,
    ) -> Result<()> {
        CancelUnfilledLargeBasketMintIntent::handle(ctx)
    }

    pub fn cancel_unfilled_large_basket_redeem_intent<'info>(
        ctx: Context<'_, '_, 'info, 'info, CancelUnfilledLargeBasketRedeemIntent<'info>>,
    ) -> Result<()> {
        CancelUnfilledLargeBasketRedeemIntent::handle(ctx)
    }

    pub fn cancel_expired_large_basket_intent<'info>(
        ctx: Context<'_, '_, 'info, 'info, CancelExpiredLargeBasketIntent<'info>>,
    ) -> Result<()> {
        CancelExpiredLargeBasketIntent::handle(ctx)
    }

    pub fn set_large_basket_component_oracle_pair(
        ctx: Context<SetLargeBasketComponentOraclePair>,
        args: SetLargeBasketComponentOraclePairArgs,
    ) -> Result<()> {
        SetLargeBasketComponentOraclePair::handle(ctx, args)
    }

    pub fn open_rebalance_intent<'info>(
        ctx: Context<'_, '_, 'info, 'info, OpenRebalanceIntent<'info>>,
        args: OpenRebalanceIntentArgs,
    ) -> Result<()> {
        OpenRebalanceIntent::handle(ctx, args)
    }

    pub fn execute_rebalance_sell_batch<'info>(
        ctx: Context<'_, '_, 'info, 'info, ExecuteRebalanceBatch<'info>>,
        args: ExecuteRebalanceBatchArgs,
    ) -> Result<()> {
        ExecuteRebalanceBatch::handle_sell(ctx, args)
    }

    pub fn execute_rebalance_buy_batch<'info>(
        ctx: Context<'_, '_, 'info, 'info, ExecuteRebalanceBatch<'info>>,
        args: ExecuteRebalanceBatchArgs,
    ) -> Result<()> {
        ExecuteRebalanceBatch::handle_buy(ctx, args)
    }

    pub fn verify_rebalance_component_price<'info>(
        ctx: Context<'_, '_, 'info, 'info, VerifyRebalanceComponentPrice<'info>>,
        args: VerifyRebalanceComponentPriceArgs,
    ) -> Result<()> {
        VerifyRebalanceComponentPrice::handle(ctx, args)
    }

    pub fn finalize_rebalance<'info>(
        ctx: Context<'_, '_, 'info, 'info, FinalizeRebalance<'info>>,
        args: FinalizeRebalanceArgs,
    ) -> Result<()> {
        FinalizeRebalance::handle(ctx, args)
    }

    pub fn cancel_rebalance(ctx: Context<CancelRebalance>) -> Result<()> {
        CancelRebalance::handle(ctx)
    }

    pub fn unwind_rebalance<'info>(
        ctx: Context<'_, '_, 'info, 'info, UnwindRebalance<'info>>,
    ) -> Result<()> {
        UnwindRebalance::handle(ctx)
    }

    pub fn close_rebalance_intent(ctx: Context<CloseRebalanceIntent>) -> Result<()> {
        CloseRebalanceIntent::handle(ctx)
    }

    pub fn update_fees(ctx: Context<UpdateFees>, args: UpdateFeesArgs) -> Result<()> {
        UpdateFees::handle(ctx, args)
    }

    pub fn update_config(ctx: Context<UpdateConfig>, args: UpdateConfigArgs) -> Result<()> {
        UpdateConfig::handle(ctx, args)
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
}
