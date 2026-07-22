# Design: defer per-component price-bound out of the execute (swap) transaction

## Problem
A large-basket mint `execute` tx must contain, in ONE transaction:
- the Jupiter swap CPI (~25 Orca Whirlpool accounts, ~976 B) — measured before/after, atomic
- the Switchboard oracle quote: ed25519 signature ix (~207 B data, incompressible) + verify ix

Measured: swap-only execute = ~1028 B (with 2 compute-budget ixs). Adding the oracle
quote pushes it past the 1232 B packet limit by ~46 B. The swap account set is
intrinsic to the venue (single Whirlpool hop already), and the ed25519 data is
incompressible (signed payload, not accounts), so neither shrinks via ALT. A 2-hop
component would overflow by far more. This is structural, not a tuning problem.

## Key soundness insight
`validate_buy_execution_price` compares the EFFECTIVE price (quote_spent /
component_received — pure token amounts, no oracle) against the oracle price within
max_oracle_slippage_bps. The effective price is a FIXED FACT the instant the swap
executes. The oracle is only the reference. Therefore the oracle check does NOT have
to happen in the same tx as the swap — as long as:
1. the effective price (quote_spent, component_received) is recorded on-chain at swap
   time, and
2. the deferred check uses a FRESH oracle quote (Switchboard's own max_age_slots /
   slothashes freshness enforces this).
Waiting to verify can only make the bound STRICTER (oracle may move away), never
weaker — so there is no stale-oracle gaming advantage. Verified against switchboard.rs.

## New flow
open (no oracle) → execute×N (swap only, records per-component spent/received,
no oracle) → verifyComponentPrice×N (1-feed quote each, tiny tx, checks that
component's stored effective price vs oracle) → collect fees → finalize (requires
ALL components verified).

## State changes (LargeBasketIntent)
- Add `component_quote_atoms: Vec<u64>` — quote spent (mint) / received (redeem) per
  component, indexed like component_amounts. Set at execute, read at verify.
- Add a verification bitmap `component_verified_bitmap: [u8; LARGE_BASKET_COMPONENT_BITMAP_BYTES]`
  mirroring component_fill_bitmap. (carve from reserved[58]; Vec<u64> grows SPACE —
  recompute LargeBasketIntent::SPACE and bump account size.)
- NOTE: USTOP10 already on-chain with the OLD layout. Since no intent is mid-flight
  and supply is 0, this is safe — but the intent account is created fresh per mint
  (init), so the new SPACE applies to all future intents. No migration needed.

## execute_large_basket_mint_component
- Remove the oracle accounts (switchboard_queue/quote/slothashes/instructions_sysvar)
  from the Accounts struct + the validate_buy_execution_price call + component_oracle_price.
- After the swap: store quote_spent into intent.component_quote_atoms[component_index];
  keep mark_component_filled. Budget guard (pending fees) stays.
- Tx now carries only swap + execute ix → ~1028 B, fits with margin.

## NEW: verify_mint_component_price (and redeem variant)
Accounts: owner, index, intent (mut), component_page, switchboard_queue,
switchboard_quote (1-feed), slothashes, instructions_sysvar.
Args: component_index, max_oracle_slippage_bps, switchboard_max_age_slots.
Logic: require component filled & not yet verified; load that component's
quote_atoms + the on-page component (units/decimals/oracle_pair); fetch the 1-feed
oracle price; validate_buy/sell_execution_price(stored_quote, component_amount, ...);
set verified bit.
Tx size: 1 swap-free ix + 1-feed ed25519 quote ≈ small, fits easily.

## finalize gating
Require completed_components == component_count AND all component_verified bits set,
before mint_to (mint) / quote release (redeem). Otherwise unchanged.

## cancel / expiry
Unverified-but-filled components: the swap already happened (vault holds tokens / user
holds USDC). cancel_expired already transfers filled components back to owner — that
path is unaffected (verification is only a finalize gate, not a fund-movement step).
A filled-but-unverified intent that expires unwinds exactly like a filled one today.

## Client (basket-ui)
- protocol-transactions: execute loop drops oracle accounts; add a verify tx per
  component (carry plan.switchboardQuote + plan.switchboardUpdateInstructions there
  instead of on execute). Order: [open], [execute_i...], [verify_i...], [collect], [finalize].
- The per-component quote the route already builds (task #8) moves from the execute
  tx to the verify tx. No route change needed beyond which tx consumes it.

## Risk / blast radius
Program: execute (mint+redeem), 2 new verify instructions, finalize gating, intent
state + SPACE, lib.rs entrypoints, events. Client: tx assembly. 4th mainnet upgrade.
Tests: per-component verify happy path + finalize-rejects-unverified + redeem.
