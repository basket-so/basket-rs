# Nemesis Audit - Verified Findings

## Scope

- Language/framework: Rust, Anchor 0.31.1, Solana program.
- Modules analyzed: `programs/omnindex/src`.
- Entry points analyzed: 26 public handlers from `lib.rs`.
- Coupled state groups mapped: 9 primary groups.
- Nemesis loop iterations: 2 full passes plus 1 targeted feedback pass.
- Verification: deep code trace for all High/Medium findings; `cargo test -p omnindex` passed 50/50.

## Verification Summary

| ID | Source | Coupled pair / invariant | Breaking op | Severity | Verdict |
|---|---|---|---|---|---|
| NM-001 | Feynman -> State | Switchboard quote freshness vs permissionless rebalance commit | `rebalance_fixed_weights_with_jupiter` | High | True positive |
| NM-002 | State -> Feynman | User quote balance vs quote-output accumulator | `redeem_index_with_jupiter` | Medium | True positive |
| NM-003 | State | User quote balance vs quote-input accumulator | `mint_index_with_jupiter` | Low | True positive |
| NM-004 | State | Reward vault funds vs staking reward accumulator | staking reward flow | Low | True positive |

## Verified Findings

### NM-001: Permissionless fixed-weight rebalances accept caller-selected stale oracle age

**Severity:** High  
**Verification:** Code trace  
**Files:** `programs/omnindex/src/instructions/rebalance_fixed_weights_with_jupiter.rs`, `programs/omnindex/src/utils/switchboard.rs`

**Invariant:** A permissionless rebalance must use oracle freshness limits controlled by protocol/index configuration, not by the executor who benefits from choosing the route.

**Finding:** `RebalanceFixedWeightsWithJupiterArgs` exposes `switchboard_max_age_slots` to the executor at `rebalance_fixed_weights_with_jupiter.rs:32-36`. The handler passes it directly into `verified_switchboard_prices` at `rebalance_fixed_weights_with_jupiter.rs:131-138`, and the verifier uses it directly as `QuoteVerifier::max_age` at `switchboard.rs:49-55`.

Those prices then drive:

- current component values at `rebalance_fixed_weights_with_jupiter.rs:147-156`
- target amounts at `rebalance_fixed_weights_with_jupiter.rs:193`
- Jupiter execution-price checks inside the swap helper
- final drift and stored component unit commits at `rebalance_fixed_weights_with_jupiter.rs:201-235`

The code caps slippage, post-drift, and quote dust, but there is no equivalent cap for oracle age.

**Trigger sequence:**

1. A fixed-weight index is eligible for permissionless rebalance.
2. A stale but still structurally valid Switchboard quote exists for the component feeds.
3. An executor submits `switchboard_max_age_slots` large enough to accept that stale quote.
4. The program computes drift, target amounts, and route price checks from stale prices.
5. The rebalance commits new `units_per_index` values and updates `fixed_weight_last_rebalanced_at`.

**Consequence:** If stale prices are materially wrong, a public executor can route trades that are acceptable against old prices but damaging against current market value. Example: if a component's true price is $10 but the accepted stale quote says $1, a sell route around $1.05 passes the stale oracle guard while losing most of the component's current value.

**Fix:** Store a maximum Switchboard age in `IndexState` or protocol config and reject `args.switchboard_max_age_slots` above that bound. Use the stored limit for permissionless fixed-weight rebalances. Consider applying the same bound to all Switchboard-using paths for consistency.

### NM-002: Jupiter redeem double-counts prior USDC component output in later route checks

**Severity:** Medium  
**Verification:** Code trace plus arithmetic trace  
**File:** `programs/omnindex/src/instructions/redeem_index_with_jupiter.rs`

**Invariant:** Each per-component sell route should validate only the quote amount received by that route, and `total_quote_out` should count each USDC atom once.

**Finding:** When USDC is itself a component, the direct quote branch transfers `backing_amount` to the user and adds it to `total_quote_out` at `redeem_index_with_jupiter.rs:159-168`, then continues without reloading `user_quote_token_account`.

If a later non-USDC component is processed, `quote_before` is read from the stale cached account field at `redeem_index_with_jupiter.rs:195`. After Jupiter returns, the handler reloads the account and computes `quote_received = quote_after - quote_before` at `redeem_index_with_jupiter.rs:205-217`. That delta includes the earlier direct USDC transfer. The inflated `quote_received` is then added again to `total_quote_out` at `redeem_index_with_jupiter.rs:218-220` and used for `validate_sell_execution_price` at `redeem_index_with_jupiter.rs:222-230`.

**Trigger sequence:**

1. Create or configure an index where the USDC component appears before at least one non-USDC component.
2. User redeems through `redeem_index_with_jupiter`.
3. The USDC component branch transfers, for example, 100 USDC and records `total_quote_out = 100`.
4. The cached `user_quote_token_account.amount` remains at the pre-transfer value.
5. The next component's malicious or poor route spends the required component backing but returns only 1 USDC.
6. The handler computes the next route's `quote_received` as 101 instead of 1, so the route price check and final `min_quote_out` check can pass with output that was already counted.

**Consequence:** A malicious route builder or compromised frontend can make a redeemer sell non-USDC backing for too little while the program's oracle and `min_quote_out` checks appear satisfied. This is user value loss rather than dilution of remaining holders, because the redeemed backing belongs to the burning user.

**Fix:** After the direct USDC component transfer, call `ctx.accounts.user_quote_token_account.reload()?` before continuing, or avoid cached account fields entirely by taking fresh token-account snapshots from `AccountInfo` before every route. `total_quote_out` should be derived from fresh before/after deltas or from a single final balance delta, not both.

### NM-003: Jupiter mint overcounts quote spend after a prior USDC component transfer

**Severity:** Low  
**Verification:** Code trace  
**File:** `programs/omnindex/src/instructions/mint_index_with_jupiter.rs`

**Finding:** The mint side has the same stale-account ordering pattern. The direct USDC component branch spends quote and updates `total_quote_spent` at `mint_index_with_jupiter.rs:169-178`, then a later non-USDC route reads `quote_before` from the stale cached account at `mint_index_with_jupiter.rs:196`. After reload, `quote_spent` at `mint_index_with_jupiter.rs:208-222` includes the earlier direct spend.

**Impact:** This fails closed: budget and buy-price checks become stricter, so valid mints can be rejected or users/integrators may need to supply a wider `max_quote_in` than the actual intended spend. It does not allow undercollateralized minting because each component vault still must receive at least its required amount.

**Fix:** Reload `user_quote_token_account` after direct quote transfers and before any later per-route delta calculation.

### NM-004: Staking reward accrual is not reachable from the current instruction set

**Severity:** Low  
**Verification:** Code trace  
**Files:** `programs/omnindex/src/utils/staking.rs`, `programs/omnindex/src/instructions/staking.rs`

**Finding:** `accrue_staking_rewards` updates `unallocated_rewards`, `reward_per_token_accumulator`, and `reward_remainder_scaled` at `utils/staking.rs:11-44`, but `rg` finds no production instruction calling it. `stake_basket`, `unstake_basket`, and `claim_staking_rewards` only settle positions against the existing accumulator at `staking.rs:180-181`, `staking.rs:267`, and `staking.rs:349`.

**Impact:** Under the current instruction set, USDC sent to the reward vault does not become claimable staking rewards because no handler moves vault inflow into pool accounting. This also means the earlier first-staker capture issue is gone, but the reward feature is inert until a reward-accrual instruction or fee path is added.

**Fix:** Add an authority/keeper-controlled reward notification instruction that transfers or verifies newly deposited USDC and calls `accrue_staking_rewards`, or explicitly document staking rewards as disabled and remove/guard claim flows until accrual is implemented.

## False Positives Eliminated

- Over-broad current component vault access in signed Jupiter routes: current code protects active component vaults with `validate_jupiter_route_account_scope` in redeem, fixed-unit rebalance, and fixed-weight rebalance.
- Caller-controlled 10,000 bps fixed-weight slippage/drift/dust: current code caps route slippage, post-rebalance drift, and dust budget at 500 bps constants.
- Missing pending-rebalance integrality checks in Jupiter mint/redeem: both current Jupiter paths call `validate_pending_component_targets_integral`.
- First-staker capture of zero-staker rewards: `stake_basket` no longer flushes `unallocated_rewards` after increasing stake.
- Fixed-unit indexes rejecting Switchboard feed IDs: fixed-unit strategy validation now accepts non-default `oracle_pair` while requiring zero target weights.

## Summary

- Raw findings: 1 High, 1 Medium, 2 Low, 5 eliminated prior/current suspects.
- Verified true positives: 1 High, 1 Medium, 2 Low.
- Highest priority fixes: cap oracle quote age for permissionless fixed-weight rebalances, and reload or freshly snapshot quote token accounts after direct USDC component transfers in Jupiter mint/redeem.
