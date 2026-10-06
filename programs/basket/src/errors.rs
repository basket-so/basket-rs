use anchor_lang::prelude::*;

#[error_code]
pub enum BasketError {
    #[msg("The index name cannot be empty.")]
    EmptyName,
    #[msg("The index symbol cannot be empty.")]
    EmptySymbol,
    #[msg("The index name is too long.")]
    NameTooLong,
    #[msg("The index symbol is too long.")]
    SymbolTooLong,
    #[msg("The component list must contain at least one asset.")]
    InvalidComponentCount,
    #[msg("The component list exceeds the maximum supported size.")]
    TooManyComponents,
    #[msg("Component mints must be unique.")]
    DuplicateComponentMint,
    #[msg("Each component must contribute a non-zero amount per index token.")]
    ZeroComponentUnits,
    #[msg("The index decimals exceed the current supported maximum.")]
    UnsupportedIndexDecimals,
    #[msg("The provided remaining accounts do not match the basket definition.")]
    InvalidRemainingAccounts,
    #[msg("A component mint account does not match the index definition.")]
    InvalidComponentMint,
    #[msg("A vault token account does not match the expected ATA.")]
    InvalidVaultAccount,
    #[msg("A user token account does not match the expected owner or mint.")]
    InvalidUserTokenAccount,
    #[msg("The requested index amount must be greater than zero.")]
    InvalidIndexAmount,
    #[msg("The requested amount would require fractional underlying token atoms.")]
    NonIntegralBasketAmount,
    #[msg("An arithmetic overflow occurred.")]
    ArithmeticOverflow,
    #[msg("The provided index mint does not match the stored configuration.")]
    IndexMintMismatch,
    #[msg("The provided quote mint is not supported for this flow.")]
    InvalidQuoteMint,
    #[msg("The provided token mint is invalid.")]
    InvalidTokenMint,
    #[msg("The provided token program account is invalid.")]
    InvalidTokenProgram,
    #[msg("The quote budget would be exceeded.")]
    QuoteBudgetExceeded,
    #[msg("The provided user component token account is invalid.")]
    InvalidUserComponentTokenAccount,
    #[msg("The provided associated token program account is invalid.")]
    InvalidAssociatedTokenProgram,
    #[msg("Only the index authority can perform this action.")]
    UnauthorizedAuthority,
    #[msg("The requested fee is outside the supported range.")]
    InvalidFeeBps,
    #[msg("The provided creator fee recipient token account is invalid.")]
    InvalidCreatorFeeRecipientTokenAccount,
    #[msg("The metadata URI is too long.")]
    MetadataUriTooLong,
    #[msg("The requested rebalance delay is outside the supported range.")]
    InvalidRebalanceDelay,
    #[msg("Minting is currently paused for this index.")]
    MintingPaused,
    #[msg("Redeeming is currently paused for this index.")]
    RedeemingPaused,
    #[msg("Rebalancing is currently paused for this index.")]
    RebalancingPaused,
    #[msg("The mint would exceed this index's supply cap.")]
    SupplyCapExceeded,
    #[msg("There is no pending rebalance proposal.")]
    NoPendingRebalance,
    #[msg("The pending rebalance proposal is still timelocked.")]
    RebalanceTimelockActive,
    #[msg("The provided metadata account is invalid.")]
    InvalidMetadataAccount,
    #[msg("The provided fee recipient token account is invalid.")]
    InvalidFeeRecipientTokenAccount,
    #[msg("The provided authority cannot be the default public key.")]
    InvalidAuthority,
    #[msg("The provided program data account is invalid.")]
    InvalidProgramData,
    #[msg("Only an approved index creator can create indexes.")]
    UnauthorizedIndexCreator,
    #[msg("The index creator whitelist is full.")]
    IndexCreatorWhitelistFull,
    #[msg("The rebalance swap plan is invalid.")]
    InvalidRebalanceSwap,
    #[msg("A rebalance swap would sell assets required by the target basket.")]
    RebalanceWouldSellTargetBacking,
    #[msg("The rebalance did not produce enough assets to satisfy the target basket.")]
    RebalanceTargetNotMet,
    #[msg("The rebalance swap list exceeds the maximum supported size.")]
    TooManyRebalanceSwaps,
    #[msg("The provided rebalance price input is invalid.")]
    InvalidRebalancePriceInput,
    #[msg("The explicit price is outside the allowed oracle tolerance.")]
    PriceOutsideOracleTolerance,
    #[msg("The old and new rebalance NAV differ outside the allowed tolerance.")]
    RebalanceNavMismatch,
    #[msg("The NAV tolerance is outside the supported range.")]
    InvalidNavTolerance,
    #[msg("The oracle price tolerance is outside the supported range.")]
    InvalidOraclePriceTolerance,
    #[msg("A component vault is below the supply-scaled target backing.")]
    VaultBelowTargetBacking,
    #[msg("The fixed-weight index configuration is invalid.")]
    InvalidFixedWeightConfig,
    #[msg("The requested instruction is not supported for this index kind.")]
    InvalidIndexKind,
    #[msg("The fixed-weight index has not reached its time or drift rebalance trigger.")]
    RebalanceNotNeeded,
    #[msg("Vault surplus belongs to index holders under pro-rata accounting.")]
    VaultSurplusClaimDisabled,
    #[msg("The provided BASKET mint is invalid.")]
    InvalidBasketMint,
    #[msg("The provided staking reward mint is invalid.")]
    InvalidRewardMint,
    #[msg("The provided staking vault is invalid.")]
    InvalidStakingVault,
    #[msg("The staking position does not belong to the signer or pool.")]
    InvalidStakePosition,
    #[msg("The stake amount must be greater than zero.")]
    InvalidStakeAmount,
    #[msg("The staking reward amount must be greater than zero.")]
    InvalidRewardAmount,
    #[msg("The staking position does not have enough staked tokens.")]
    InsufficientStakedAmount,
    #[msg("Staking rewards cannot be accrued while no BASKET is staked.")]
    NoStakedTokens,
    #[msg("There are no staking rewards to claim.")]
    NoRewardsToClaim,
    #[msg("Nonzero fees are disabled for the current Jupiter-only protocol.")]
    FeesRequireUsdcQuote,
    #[msg("The provided Jupiter program account is invalid.")]
    InvalidJupiterProgram,
    #[msg("The provided Jupiter route instruction is invalid.")]
    InvalidJupiterRoute,
    #[msg("Switchboard oracle verification failed.")]
    SwitchboardVerificationFailed,
    #[msg("The requested Switchboard quote maximum age is outside the supported range.")]
    InvalidSwitchboardMaxAge,
    #[msg("The required Switchboard feed was not present in the verified quote.")]
    MissingSwitchboardFeed,
    #[msg("The Switchboard oracle price is invalid.")]
    InvalidSwitchboardPrice,
    #[msg("The Jupiter execution price is outside the configured oracle tolerance.")]
    ExecutionPriceOutsideOracleTolerance,
    #[msg("Staking fee distribution requires the large-basket intent flow.")]
    StakingFeesRequireLargeBasketIntent,
    #[msg("The large-basket intent is invalid.")]
    InvalidLargeBasketIntent,
    #[msg("The large-basket intent has expired.")]
    LargeBasketIntentExpired,
    #[msg("The large-basket intent expiry is outside the supported range.")]
    InvalidLargeBasketIntentExpiry,
    #[msg("The large-basket component page is invalid.")]
    InvalidLargeBasketComponentPage,
    #[msg("The large-basket index is not fully configured.")]
    LargeBasketNotConfigured,
    #[msg("The large-basket component has already been filled for this intent.")]
    LargeBasketComponentAlreadyFilled,
    #[msg("The large-basket component has not been filled for this intent.")]
    LargeBasketComponentNotFilled,
    #[msg("The component transfer did not credit the required token amount.")]
    ComponentTransferAmountMismatch,
    #[msg("A large-basket component oracle pair can only be updated while the index supply is zero.")]
    LargeBasketOracleUpdateRequiresZeroSupply,
    #[msg("The large-basket component price has already been verified for this intent.")]
    LargeBasketComponentAlreadyVerified,
    #[msg("All filled large-basket components must be price-verified before finalizing.")]
    LargeBasketComponentNotVerified,
    #[msg("The rebalance intent has expired.")]
    RebalanceIntentExpired,
    #[msg("Only the index authority can unwind a rebalance intent before it expires.")]
    RebalanceIntentNotExpired,
    #[msg("The rebalance intent is still open.")]
    RebalanceIntentStillOpen,
    #[msg("Swap-path redeems must use execute_large_basket_redeem_batch, which charges fees with the proceeds.")]
    RedeemRequiresBatchExecution,
    #[msg("A rebalance is pending or in progress; new mints and redeems reopen when it completes.")]
    RebalancePending,
    #[msg("Mint and redeem intents are still open; they must settle before a rebalance can open.")]
    IntentsStillOpen,
    #[msg("Only the index authority or its rebalance keeper can do this.")]
    NotRebalanceOperator,
    #[msg("The basket emptied after this mint opened; cancel it once expired to recover the deposits.")]
    MintBasisChanged,
    #[msg("A rebalance was requested too recently.")]
    RebalanceRequestCooldown,
}
