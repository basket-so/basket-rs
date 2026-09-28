# Replacement review and validation

The review covered protocol initialization and authority checks, paged basket
configuration, mint/redeem intent locking and reserves, cancellation, fee routing,
staking reward accounting, and rebalance execution/finalization/unwind. This was
a targeted engineering review, not an independent security audit.

## Fixed before deployment

Rebalance swaps previously committed before their separate oracle-verification
transaction. A permissionless keeper could choose an extremely unfavorable
route and then abandon the intent; deferred verification and final NAV checks
could not roll back the completed token transfers.

`ExecuteRebalanceBatch` now obtains verified Switchboard prices in the execution
transaction and checks each actual swap fill before recording completion. A bad
execution reverts the entire transaction. The keeper's tolerance remains bounded
by `MAX_FIXED_WEIGHT_EXECUTION_SLIPPAGE_BPS`. Successfully executed legs are marked
price-verified immediately. Regression tests reject dust sale proceeds, excessive
purchase prices, and attempts to exceed the permitted tolerance.

Rebalance clients must provide the new oracle accounts and tolerance arguments.
An oracle update and its swap must fit in the same transaction; use smaller
batches as necessary. The separate verify instruction is no longer required for
freshly executed legs. No automated keeper service was deployed by this task.

## Validation

- Fresh `anchor build` completed for the vanity program and requested staking mint.
- 93 Rust unit tests passed, including the new rebalance regressions.
- Local Agave 2.3 validator tests passed against the final SBF binary: protocol
  initialization; six-decimal staking, USDC funding/claiming and unstaking;
  fixed-unit and fixed-weight paged basket creation; first and subsequent pro-rata
  mints; cancellation; complete redemption back to zero supply and empty vaults;
  swap-path batch execution with a USDC component; fee collection and staking-fee
  routing; rejection of an excessive supply.
- UI TypeScript validation, production build, and all 25 tests passed. The builder
  regression checks new mint/redeem argument encoding, writable page accounts,
  and transaction packet sizes.
- Landing production build and all 6 tests passed.
- Catalog preflight checked current mint identity/decimals, two-way Jupiter
  routes and oracle prices for all 12 baskets. Each initial basket is sized to
  $1 within the catalog's $0.00001 rounding tolerance at its recorded price snapshot.

The local swap lifecycle deliberately uses USDC to avoid external liquidity and
oracle dependencies. It does not claim a live Jupiter trade or a full live
rebalance has been executed. The contract retains fixed-unit authority-rebalance
limitations described in the main README. Initial catalog fees remain zero, as
on the old deployment; initializing staking does not itself produce rewards.

## Authorized old-state closure

All 14 old index mint supplies and staking balances were zero. The old XTEST5
vaults contained the ten tiny balances recorded in `approved-stranded-vaults.json`.
The user explicitly authorized leaving those tokens inaccessible when closing
the old program. No other nonzero vault balances or active intents were approved
for abandonment. The closure script compares the fresh preflight with that exact
approved set before submitting the close.
