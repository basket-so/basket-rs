# Nemesis Audit - Raw Working Notes

## Scope

- Language/framework: Rust, Anchor 0.31.1, Solana program.
- Program: `programs/omnindex`.
- Public entry points analyzed: 26 handlers exported from `programs/omnindex/src/lib.rs`.
- Primary modules: protocol/index setup, direct mint/redeem, Jupiter mint/redeem, fixed-unit rebalance, fixed-weight Jupiter rebalance, staking, metadata, token account helpers, Switchboard and Jupiter adapters.
- Tests run during audit: `cargo test -p omnindex` passed, 50/50 unit tests.
- Worktree note: the repository was already dirty before this audit. Prior `.audit/findings` files existed and were treated as context only, then re-verified against current source.

## Attacker Hit List

1. Permissionless `rebalance_fixed_weights_with_jupiter`, because it commits vault-derived component units and updates `fixed_weight_last_rebalanced_at` using caller-supplied route and oracle-age controls.
2. Jupiter redeem, because the program signs CPI routes with `vault_authority` and derives min-output and execution-price checks from token-account balance deltas.
3. Pending fixed-unit rebalances versus mint/redeem supply changes, because pending targets must remain integral after supply changes.
4. Staking reward accumulator and position checkpoints, because reward vault value and reward accounting are maintained separately.
5. Authority-controlled config and metadata setters, checked mainly for missing guards and state/event asymmetry.

## Coupled State Dependency Map

| Coupled state | Invariant | Main mutation/read paths |
|---|---|---|
| `index_mint.supply` and component vault balances | Index token supply must remain a pro-rata claim on active component vault balances | `mint_index`, `mint_index_with_jupiter`, `redeem_index`, `redeem_index_with_jupiter` |
| `pending_components` and live supply | While a fixed-unit rebalance is pending, post-mint/redeem supply must keep pending component targets integral | all mint/redeem paths, `propose_rebalance`, `execute_rebalance` |
| Switchboard quote freshness and rebalance price inputs | Permissionless fixed-weight rebalance should use fresh prices under protocol-controlled bounds | `rebalance_fixed_weights_with_jupiter`, `verified_switchboard_prices` |
| Fixed-weight target weights, current vault amounts, and stored `units_per_index` | Rebalance should compute targets from trusted prices, execute bounded swaps, then commit units derived from final vault amounts | `rebalance_fixed_weights_with_jupiter` |
| Jupiter route-declared accounts and signed PDA authority | A signed route should only mutate declared, expected vault endpoints | `redeem_index_with_jupiter`, `execute_rebalance`, `rebalance_fixed_weights_with_jupiter`, `invoke_jupiter_swap` |
| User quote token balance and quote-output accumulator | `total_quote_out` and per-component execution-price checks must count only output from the current operation | `redeem_index_with_jupiter` |
| User quote token balance and quote-input accumulator | `total_quote_spent` must count each USDC input once | `mint_index_with_jupiter` |
| Staking pool accumulator and stake positions | Positions should accrue rewards only from an accumulator that is reachable from on-chain reward inflows | `stake_basket`, `unstake_basket`, `claim_staking_rewards`, `accrue_staking_rewards` |
| Protocol/index authorities and guarded setters | Only current authority can mutate protocol/index config | update handlers |

## Function-State Matrix Summary

| Function group | Reads | Writes | External calls |
|---|---|---|---|
| Protocol/index setup | `ProtocolConfig`, program data, component inputs | config/index accounts, index mint | system program, SPL token |
| Direct mint/redeem/quote | `IndexState`, index mint supply, component vault amounts | component vaults, index mint supply | SPL token, ATA program |
| Jupiter mint/redeem | `IndexState`, index mint supply, Switchboard quote, component vaults, user quote account | vaults, user quote account, index mint supply | Jupiter CPI, Switchboard verifier, token/token-2022 |
| Fixed-unit rebalance | pending components, old/new component prices, vault balances | component vaults, active/pending component state | Jupiter CPI, Switchboard verifier, ATA program |
| Fixed-weight rebalance | target weights, oracle prices, vault balances, last rebalance timestamp | vaults, recomputed `units_per_index`, timestamp | Jupiter CPI, Switchboard verifier, ATA program |
| Staking | pool totals, accumulator, position checkpoints | stake vault, reward vault, pool, position | SPL token |
| Metadata | index metadata fields and metadata PDA | Metaplex metadata, stored URI | Metaplex CPI |

## Raw Suspects

| Raw ID | Source | Suspect | Initial severity | Verification result |
|---|---|---|---|---|
| R-001 | Feynman + State | Permissionless fixed-weight rebalance lets the executor choose an unbounded Switchboard quote age | High | True positive, NM-001 |
| R-002 | Feynman + State | `redeem_index_with_jupiter` uses a stale cached user quote balance after direct USDC component transfer | Medium | True positive, NM-002 |
| R-003 | State | `mint_index_with_jupiter` has the same stale cached quote balance, but it overcounts spend and fails closed | Low | True positive, NM-003 |
| R-004 | State | Staking reward accounting helper is not reachable from any instruction | Low | True positive, NM-004 |
| R-005 | Prior report re-check | Jupiter redeem/fixed-weight can touch unrelated current component vaults | High | False positive in current code: route scope now rejects protected current component vaults except declared endpoints |
| R-006 | Prior report re-check | Fixed-weight Jupiter slippage/drift/dust controls can be set to 10,000 bps | High | False positive in current code: controls are capped by constants at 500 bps |
| R-007 | Prior report re-check | Jupiter mint/redeem skip pending target integrality guard | Medium | False positive in current code: both Jupiter paths call `validate_pending_component_targets_integral` |
| R-008 | Prior report re-check | First staker captures zero-staker unallocated rewards | Medium | False positive in current code: `stake_basket` no longer flushes unallocated rewards after increasing stake |
| R-009 | Consistency | Fixed-unit indexes cannot store Switchboard feed IDs | Low | False positive in current code: fixed-unit config accepts non-default `oracle_pair` when target weights remain zero |

## Nemesis Feedback Loop Notes

- Feynman pass asked why a permissionless executor controls `switchboard_max_age_slots`. State mapper traced that value into `QuoteVerifier::max_age`, then into target amounts, execution-price checks, final drift, component unit commits, and `fixed_weight_last_rebalanced_at`.
- State pass flagged a gap between direct USDC component transfers and quote-account delta reads in Jupiter redeem. Feynman re-interrogation found the hidden assumption: Anchor account wrappers do not auto-refresh after CPI, so the next per-route quote delta includes the prior USDC component transfer.
- The same stale-account pattern exists in Jupiter mint, but its effect is stricter budget and buy-price checks. It can reject valid mints or force wider `max_quote_in`, but does not create undercollateralized minting.
- Staking state analysis found that the accumulator logic exists and is unit-tested, but no instruction calls `accrue_staking_rewards`. That eliminates the prior first-staker capture but leaves reward vault deposits without an on-chain accounting path.

## Verification

- All High and Medium raw suspects were verified by deep code trace.
- PoC-style arithmetic was used for NM-002:
  - Initial user USDC balance: 0.
  - Quote component branch transfers 100 USDC and sets `total_quote_out = 100`, but the cached account field remains 0.
  - Next non-USDC route returns only 1 USDC after spending the component backing.
  - Code computes `quote_received = 101 - 0 = 101`, validates the non-USDC sale with 101 instead of 1, and sets `total_quote_out = 201` even though actual output is 101.
- No mocked Jupiter/Switchboard integration test was added; the vulnerable behavior is visible at the CPI boundary and account-reload boundary.
