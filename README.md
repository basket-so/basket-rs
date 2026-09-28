# basket-rs

Anchor workspace for an index protocol on Solana. Index tokens are pro-rata
claims on component vault balances, with support for fixed-unit baskets and
fixed-weight target policies.

The protocol uses Jupiter for routed execution and Switchboard for price
verification. Jupiter is treated only as liquidity/execution; Switchboard feeds
are the source of truth for NAV checks, rebalance weights, and execution-price
guards.

## Accounting Model

Each index token represents a pro-rata share of the index component vaults.
Redeeming burns index tokens and returns:

```text
component_out = redeemed_index_tokens * component_vault_balance / total_index_supply
```

Minting after the initial supply uses the symmetric pro-rata rule and rounds up
per component so existing holders are not diluted:

```text
component_in = ceil(minted_index_tokens * component_vault_balance / total_index_supply)
```

When supply is zero, the first mint uses the configured `units_per_index`
starting basket. After that, vault balances are the economic source of truth.

## Fixed-Unit Indexes

A fixed-unit index has a target basket defined in atomic units per full index
token. Authority-managed rebalancing of that target basket is not currently
available; the legacy timelocked propose/execute flow was removed and its
replacement on the intent model has not shipped yet (see Rebalancing).

For non-USDC components, the component's `oracle_pair` is interpreted as its
Switchboard feed id. The field name is legacy.

## Fixed-Weight Indexes

A fixed-weight index stores target weights in basis points on each component.
Target weights are a rebalance policy, not a redemption formula.
Each component must specify nonzero `units_per_index` at creation time, except
a zero-weight USDC reserve in a FixedWeights basket.
Those units define the initial basket used by the first mint while supply is
zero; later mints and redeems use pro-rata vault-share accounting, and
fixed-weight rebalances refresh stored units from the post-rebalance vault
balances.

Fixed-weight rebalancing runs on a batched intent flow
(`open_rebalance_intent` -> `execute_rebalance_sell_batch` /
`execute_rebalance_buy_batch` -> `finalize_rebalance`), so the swaps span multiple transactions. It:

- uses Switchboard USD prices to compute current weights and target amounts
- uses native Solana USDC as the quote leg
- sells overweight components to USDC, then buys underweight components
- verifies every Jupiter route's mints, token accounts, protected vault scope,
  vault balance deltas, atomic per-leg execution price, quote dust, one-sided
  NAV preservation, and final drift
- counts USDC parked in the vault-authority quote ATA toward NAV so leftover
  quote (for example from an unwound rebalance) is recycled into components
- can always be abandoned: `unwind_rebalance` is permissionless after the
  intent expires (index authority any time) and re-syncs page accounting from
  live vault balances before releasing the operation lock

## Instructions

- `initialize_protocol`: creates the protocol config PDA with permissioned index creation enabled by default
- `update_protocol_config`: rotates protocol authority, sets the primary approved index creator, or enables permissionless index creation
- `update_index_creator_whitelist`: adds or removes additional approved index creators while creation remains permissioned
- `create_large_basket_index`: creates the index account and the classic SPL index mint PDA, and records the index kind, fee recipients, supply cap, rebalance config, and component count
- `initialize_large_basket_component_page`: writes one page of up to ten components and idempotently creates each component's vault ATA; pages are filled while supply is zero and the config is unfinalized
- `finalize_large_basket_config`: checks that the pages tile every component index with no gaps or duplicates, runs strategy validation, and locks the component set
- `set_large_basket_component_oracle_pair`: re-points a component's Switchboard feed; only allowed while supply is zero and no intent is mid-flight
- `create_index_metadata`: creates Metaplex metadata for the index mint
- `update_index_metadata`: updates the index mint metadata URI
- `migrate_index_metadata_authority`: moves legacy Metaplex update authority to the index vault-authority PDA
- `initialize_staking_pool`: initializes the BASKET staking pool plus BASKET stake vault and USDC reward vault
- `stake_basket`: stakes BASKET into the protocol staking vault
- `unstake_basket`: settles rewards, then unstakes BASKET
- `fund_staking_rewards`: transfers USDC into the reward vault and accrues it to currently staked BASKET
- `claim_staking_rewards`: claims accrued USDC rewards for a BASKET staker
- `open_large_basket_mint_intent`: snapshots supply, sizes each component's backing (units-per-index basket while supply is zero, pro-rata vault share afterward), opens the intent, and takes the operation lock
- `open_large_basket_redeem_intent`: burns the redeemed index tokens up front, reserves each component's backing on its page, opens the intent, and takes the operation lock
- `execute_large_basket_mint_component` / `execute_large_basket_redeem_component`: fill one component by routing USDC<->component through caller-provided Jupiter routes (or moving USDC directly when the component is USDC), recording the leg's quote atoms
- `execute_large_basket_mint_component_in_kind` / `execute_large_basket_redeem_component_in_kind`: fill one component by depositing/withdrawing the exact component tokens to/from the owner, skimming the fee in-kind; used when the intent was opened `in_kind`
- `execute_large_basket_mint_batch` / `execute_large_basket_redeem_batch`: fill several components in one transaction, each route scoped to its own entry's accounts
- `verify_large_basket_mint_component_price` / `verify_large_basket_redeem_component_price`: optional deferred check that bounds a filled component's effective price against a fresh single-feed Switchboard quote
- `collect_large_basket_intent_fees`: after every component has executed, charges the USDC fee split (protocol, creator, staking) on the realized swap volume; swap-path intents only
- `finalize_large_basket_mint_intent`: requires collected fees on swap-path intents, re-checks backing and supply against the budget and cap, books the filled reserves, mints the index tokens, and releases the lock
- `finalize_large_basket_redeem_intent`: enforces the net USDC min-out on swap-path intents and releases the lock
- `cancel_unfilled_large_basket_mint_intent` / `cancel_unfilled_large_basket_redeem_intent`: owner aborts an intent before any component fills; redeem restores reserves and re-mints the burned tokens
- `cancel_expired_large_basket_intent`: permissionless after expiry; returns filled mint backing (or unfilled redeem backing) to the owner and releases the lock
- `open_rebalance_intent`: permissionlessly opens a fixed-weight rebalance when the drift or time trigger fires; prices NAV via Switchboard, derives per-component sell/buy legs, and takes the operation lock
- `execute_rebalance_sell_batch` / `execute_rebalance_buy_batch`: execute batched Jupiter swap legs (component->USDC, then USDC->component) validate execution prices atomically, and record each leg's quote and fill atoms; blocked after expiry or while rebalancing is paused
- `verify_rebalance_component_price`: legacy separate price check; new executions are already verified atomically
- `finalize_rebalance`: requires every leg executed and verified, enforces post-rebalance drift, one-sided NAV preservation, and quote dust, then rewrites page units/reserves and releases the lock
- `cancel_rebalance`: initiator-only cancel before any leg has executed
- `unwind_rebalance`: abandons a stuck rebalance (permissionless after expiry, index authority any time), re-syncing page accounting from live vault balances and releasing the lock
- `close_rebalance_intent`: initiator reclaims the intent account rent once the intent is finalized or cancelled
- `update_fees`: sets protocol, creator, and staking mint/redeem fee bps, capped at 10% total per direction
- `update_config`: sets protocol and creator fee recipients, supply cap, rebalance delay, and pause flags
- `update_authority`: transfers index authority to a new wallet, multisig, DAO, or governance PDA
- `claim_fees`: disabled under pro-rata vault-share accounting

Mint and redeem fees are configured per index by `update_fees`. Each direction
splits into a protocol fee (`mint_fee_bps` / `redeem_fee_bps`, paid to
`fee_recipient`), a creator fee (`creator_mint_fee_bps` / `creator_redeem_fee_bps`,
paid to `creator_fee_recipient`), and a staking fee (`staking_mint_fee_bps` /
`staking_redeem_fee_bps`, accrued to the BASKET staking reward vault). The sum of
the three splits for each direction is capped at 1,000 bps.
`create_large_basket_index` accepts `creator_fee_recipient`; pass the default
pubkey for no creator. When no creator recipient is configured, the creator-fee
amount is routed to the protocol `fee_recipient` instead.

Swap-path intents pay fees in native Solana USDC: `collect_large_basket_intent_fees`
runs after every component has executed and charges the splits on the realized USDC
that moved through the swaps. In-kind intents have no USDC fee step; the fee is
skimmed in the component token itself at execute time, with the staking share folded
into the protocol share so rewards stay a single token.

The BASKET staking mint is fixed at
`2rNBaMg5VAr1aMNCwAPdDZVgzzdTaNDebUnNqPFNmeta`. Staking rewards are funded
through `fund_staking_rewards`, which accrues deposited USDC only when BASKET is
already staked. Component vault surplus belongs to index holders under pro-rata
accounting.

## Rebalancing

Fixed-weight rebalancing is an intent state machine scoped to one index at a
time; the intent shares the `large_basket_operation_in_progress` lock with the
mint/redeem intent flow, so a rebalance and a mint/redeem can never interleave.

`open_rebalance_intent` is permissionless but only fires when the index's drift
threshold or rebalance interval triggers, and its caller-supplied gates are
clamped two-sided: `nav_tolerance_bps` within
[`MIN_KEEPER_NAV_TOLERANCE_BPS`, `MAX_KEEPER_NAV_TOLERANCE_BPS`] and
`max_post_rebalance_drift_bps` within
[`MIN_FIXED_WEIGHT_POST_REBALANCE_DRIFT_BPS`,
`MAX_FIXED_WEIGHT_POST_REBALANCE_DRIFT_BPS`], so a malicious initiator can
neither loosen the safety gates nor open a no-op-tight one. When drift
triggering is enabled, the post-rebalance drift bound is additionally required
to sit strictly below the index's drift threshold so a finalized rebalance
cannot immediately re-trigger (a drift-loop guard). To keep that range
non-empty, a drift-enabled index must be configured with a drift threshold
strictly above `MIN_FIXED_WEIGHT_POST_REBALANCE_DRIFT_BPS`.

The binding per-rebalance value protection is the atomic per-leg execution
check, which caps each swap's effective price against verified Switchboard
prices before the transaction can commit. The finalize
NAV-preservation gate is a one-sided aggregate backstop: it re-prices the
original holdings and the post-rebalance holdings at the same fresh oracle (so
market drift over the intent's life nets out), but because the keeper selects
the finalize quote it cannot tighten below the per-leg bound and should not be
read as a hard standalone NAV guarantee.

Swap legs execute in batches and validate their effective quote/fill amounts
against Switchboard in the same transaction. Keep batches small enough for the
oracle update and swap to fit together; oversized batches must be split.
Execution stops at `expires_at` (at most 30 minutes after open) and while the
authority has paused rebalancing.

`finalize_rebalance` re-prices the basket, enforces post-rebalance drift,
one-sided NAV loss within `nav_tolerance_bps`, and a quote-dust bound, then
refreshes every page's `units_per_index` / `accounted_reserve` from live vault
balances. When USDC is itself a component, its vault is the same ATA as the
rebalance scratch account; the dust bound then applies only to the excess over
that component's target backing.

A rebalance that cannot complete is never terminal: `unwind_rebalance`
(permissionless after expiry, index authority any time) re-syncs page
accounting from live vault balances and releases the lock. Funds never leave
vault-authority custody during a rebalance, so the unwind moves no tokens; any
USDC left in the scratch ATA is counted into NAV by the next
`open_rebalance_intent`, which sizes its buy legs to recycle it.

Fixed-unit (authority-proposed, timelocked) rebalancing is not currently
available; the legacy atomic flow was removed with the batched rewrite and its
replacement on the intent model has not shipped yet.

## Remaining Account Order

`initialize_large_basket_component_page` expects, after the named accounts, one
group per component in this page's `components` argument order:

1. component mint
2. vault ATA for the vault-authority PDA, that mint, and that mint's token program
3. token program that owns the mint (`spl_token::ID` or Token-2022)

The handler creates each vault ATA idempotently.

`finalize_large_basket_config` expects every component page account for the index,
all writable. Order is normalized by `page_index` internally; the pages must tile
component indices `0..component_count` with no gaps or duplicates.
`set_large_basket_component_oracle_pair` takes no remaining accounts; it addresses
a single component by `page_index` and `component_index`.

`open_large_basket_mint_intent` and `open_large_basket_redeem_intent` expect all
component pages in page-index order; the open reads each page to size per-component
backing. For redeem the pages must be writable: the open reserves backing against
each page and burns the index tokens before the intent goes live.

`execute_large_basket_mint_component` and `execute_large_basket_redeem_component`
name the single component's page, mint, vault, and token program. When the component
is not USDC, append the accounts required by the Jupiter route after the named
accounts; the mint route must spend the owner's USDC into the component vault, and
the redeem route must spend the component vault into the vault-authority USDC ATA.
When the component is USDC, pass no swap and the program moves USDC directly.
Vault-authority-owned token accounts other than the declared source and destination
are rejected from every route.

`execute_large_basket_mint_batch` and `execute_large_basket_redeem_batch` name only
the shared accounts. Each `entries` element brings a contiguous remaining-account
group of `4 + route_account_count` accounts:

1. component page
2. component mint
3. component vault
4. component token program
5. ...the Jupiter route accounts for that entry

Groups appear in the same order as `entries`, and each entry's swap plan indices are
scoped to that entry's own accounts, so one entry's route can never touch another's
vault.

`execute_large_basket_mint_component_in_kind` and
`execute_large_basket_redeem_component_in_kind` take no remaining accounts; the
owner's component token account and the protocol and creator fee recipients'
component token accounts are named. Mint deposits the exact backing plus the in-kind
fee from the owner; redeem releases the net backing to the owner and skims the fee,
all from the component vault.

`verify_large_basket_mint_component_price` and
`verify_large_basket_redeem_component_price` name the component's page plus the
Switchboard queue, verified quote account, slot hashes sysvar, and instructions
sysvar. Include the Switchboard quote update/signature instructions before the
Basket instruction in the same transaction. These checks are optional: finalize
relies on the aggregate `max_quote_in` / `min_quote_out` bounds and Jupiter per-swap
slippage instead.

`collect_large_basket_intent_fees` takes no remaining accounts; it requires every
component executed, then transfers the USDC fee split from the owner's USDC account
to the protocol and creator fee token accounts and the staking reward vault.

`finalize_large_basket_mint_intent` expects all component pages in page order, all
writable; it re-checks backing, books the filled reserves onto the pages, mints the
index tokens, and releases the lock. `finalize_large_basket_redeem_intent` takes no
remaining accounts.

`cancel_unfilled_large_basket_mint_intent` takes no remaining accounts.
`cancel_unfilled_large_basket_redeem_intent` expects all component pages (writable)
so it can restore the reserved backing before re-minting the burned tokens.

`cancel_expired_large_basket_intent` expects all component pages (writable) first,
then a `[component mint, component vault, owner token account, component token
program]` group for every component whose backing is returned to the owner (filled
components for a mint, unfilled components for a redeem). Pass no component groups
when there is nothing to return.

`open_rebalance_intent` takes `OpenRebalanceIntentArgs { nonce, expires_at,
switchboard_max_age_slots, nav_tolerance_bps, max_post_rebalance_drift_bps }`
and remaining accounts of all component pages in page-index order followed by
all component vaults in global component order.

`execute_rebalance_sell_batch` / `execute_rebalance_buy_batch` take
`ExecuteRebalanceBatchArgs { entries }` where each entry carries
`component_index`, a keeper-side `quote_limit` (sell: minimum USDC out; buy:
maximum USDC in), `route_account_count`, and the compact Jupiter swap plan.
Remaining accounts are per-entry groups of
`[component page, component mint, component vault, component token program,
...route accounts]`. Each sell route must spend exactly the leg from its
component vault into the vault-authority USDC ATA; each buy route must spend
from that USDC ATA within the protocol oracle-value spending cap and deliver at
least the cost-adjusted minimum into its component vault.
Vault-authority-owned token accounts other than the declared source and
destination are rejected from every route.

`verify_rebalance_component_price` takes `component_index`,
`max_oracle_slippage_bps` (capped by
`MAX_FIXED_WEIGHT_EXECUTION_SLIPPAGE_BPS`), and `switchboard_max_age_slots`,
and bounds the recorded quote/fill effective price against a fresh single-feed
Switchboard quote.

`finalize_rebalance` takes `switchboard_max_age_slots` plus the same
pages-then-vaults remaining accounts as open (pages writable).
`unwind_rebalance` takes no args and the same pages-then-vaults layout.

## Current Scope

This cut targets paged large-basket indexes (up to 50 components across pages of
ten) with a classic SPL index mint and Token-2022-capable component vaults, the
batched mint/redeem intent state machine (Jupiter swap path plus in-kind variants),
Switchboard-priced execution and fee checks, permissionless fixed-weight rebalancing
on the intent model, Metaplex metadata, BASKET staking with protocol/creator/staking
fee splits, pause/supply-cap controls, and external governance ownership of index
authority. Fixed-unit (authority-proposed, timelocked) rebalancing is not yet
available (see Rebalancing).

## Rebalance Worker

`scripts/rebalance-bot.mjs` is a keeper worker for fixed-weight baskets. It auto-discovers
every FixedWeights index (or targets one with `--index <pubkey>`), prices the basket from
Jupiter, computes each component's weight drift, and — when the index's drift threshold or
time interval has triggered — drives the on-chain rebalance state machine end to end:
`open_rebalance_intent` → batched `execute_rebalance_sell_batch` / `execute_rebalance_buy_batch`
(Jupiter routes encoded as the compact `LargeBasketSwapPlan`) → `verify_rebalance_component_price`
per leg → `finalize_rebalance`. It also detects a stuck/expired intent and calls
`unwind_rebalance` to release the operation lock.

It is **dry-run by default** — it reads chain state, fetches quotes, and prints the plan but
sends no transactions. Pass `--execute` to send. `--execute` spends the keeper's SOL (transaction
fees + rent) and moves basket assets; trading costs are paid from basket assets, so validate with the dry run first.

```bash
node scripts/rebalance-bot.mjs                 # detect-only, scan all FixedWeights baskets
node scripts/rebalance-bot.mjs --preview-swaps # dry-run + encode the Jupiter swaps read-only
node scripts/rebalance-bot.mjs --index <pk>    # only this index (skips getProgramAccounts)
node scripts/rebalance-bot.mjs --execute       # actually rebalance triggered baskets
node scripts/rebalance-bot.mjs --watch         # poll forever (--interval <seconds>)
```

On `--execute` the worker builds, tx-size-packs, and route-scope-validates every sell and
buy batch *before* sending any execute transaction; if a leg can't be built/sized/scoped it
cancels the just-opened (zero-legs-executed) intent and reclaims its rent rather than
stranding the operation lock. It derives time from the on-chain clock, keeps the intent TTL
under the program cap, sizes compute units per batch, and retries a step if its Switchboard
quote goes stale. Each batch includes its oracle accounts in transaction sizing and
refreshes its managed quote before execution. Failed transaction confirmations stop execution.

Before selling, the worker bounds total buy cost by existing USDC plus minimum sell
proceeds. If needed, it reduces buy targets within the intent's NAV tolerance, using
integer rounding that matches the program. If no affordable plan exists, it cancels
before any trade. Buy fills must retain at least the original target backing less
that tolerance (and acquire at least one atom); final NAV, drift, and dust gates still
apply. This requires deploying the updated buy-fill handler; the account layout is unchanged.
Market movement or route failures can still interrupt a multi-transaction rebalance.
`--verify-buffer-bps` now configures the buffer for atomic execution-price checks.

Run keeper regression checks with `npm run test:keeper` after generating the current IDL.

Env: `SOLANA_RPC_URL` (default mainnet-beta; discovery needs a `getProgramAccounts`-capable
RPC), `ANCHOR_WALLET` (keeper keypair, default `deployer-keypair.json`), `JUPITER_SWAP_API`,
`JUPITER_PRICE_API`. Flags: `--batch-size` (default 2), `--slippage-bps`, `--verify-buffer-bps`,
`--nav-tolerance-bps`, `--drift-margin-bps`, `--ttl`, `--priority-fee`, `--interval`.

Prerequisites: the program build that includes the rebalance intent flow must be deployed and
`target/idl/basket.json` regenerated (`anchor build`) so the worker decodes accounts and
encodes instructions against the matching layout. The worker only manages USDC-quoted
FixedWeights baskets.

## Building on Windows

`.cargo/config.toml` (local, untracked) points `rustc-wrapper` at
`scripts/rustc-wrapper.exe`, a small shim that strips `\\?\` UNC prefixes from
paths so the SBF toolchain accepts them. The binary is not tracked in git;
rebuild it with:

```bash
rustc scripts/rustc-wrapper.rs -o scripts/rustc-wrapper.exe
```

## Mainnet Deployment

### Curated index catalog

`docs/catalog/baskets.json` contains the six landing-page basket definitions and
nine additional researched baskets, each targeting a $1 initial NAV. See
[`docs/catalog/RESEARCH.md`](docs/catalog/RESEARCH.md) for allocations, sources,
weight policies, and launch blockers. This is a separate, explicitly invoked
index-creation flow; it is not part of program deployment.

```bash
npm run test:catalog
npm run catalog:check
npm run catalog:deploy
# Optional subset:
npm run catalog:deploy -- --symbols=SOLB,MEME
```

The default is read-only. Execution uses `SOLANA_RPC_URL` and `ANCHOR_WALLET`
(default `deployer-keypair.json`), validates live prices/routes/oracles, and
requires sufficient SOL for the eligible batch. It records results in
`docs/catalog/readiness.json` and confirmed creation transactions in
`docs/catalog/deployment.json`. Catalog entries with unresolved asset or
liquidity blockers are never created. The $1 is an initialization valuation,
not a peg or a guarantee of the price at a later first mint.

### Program deployment

Mainnet deployment is intentionally program-only. It must not call
`create_large_basket_index`, `initialize_large_basket_component_page`,
`finalize_large_basket_config`, or any bootstrap script that creates an index. The
first mainnet index should be created explicitly by the protocol operator/admin
after the program and protocol config are initialized.

Current mainnet ID:

- Basket: `9LNEoShrH93XekWQTFmZBdUdMu8ugJxBr5cbfqJQC1mw`

Safe sequence:

1. Build and deploy Basket with the mainnet program ID.
2. Have the current program upgrade authority call `initialize_protocol` with the desired protocol authority and approved `index_creator`.
3. Call `initialize_staking_pool` to create the BASKET stake vault and USDC reward vault.
4. Verify `permissionless_index_creation` is false.
5. Stop. Do not create an index as part of deployment.
6. Create the first curated index manually through the admin flow.

## Fresh mainnet deployment

The replacement program is `bskthjNMRWQ4ekDLxaAzA1e39ThPmEtUgHY3XHfs7qv`.
See `docs/program-replacement` for its deployment journal and review.
Rebalance batch execution now requires `switchboard_max_age_slots`,
`max_oracle_slippage_bps`, and the Switchboard queue, quote, slot-hashes and
instruction-sysvar accounts. Execution validates the realized price atomically
and marks the leg verified. Size batches to fit the oracle update and swap in
one transaction; a deferred check cannot protect an already committed swap.

## USDC reserve accounting migration

FixedWeights baskets must include native USDC as a component, even when its target
weight and initial units are zero. This accounts for cash remaining after a rebalance
or unwind in every proportional mint, redemption, fee, and cancellation. The reserve
uses the existing vault USDC ATA and does not change any other component's target weight.
New configurations without USDC are rejected. Existing configurations without it
cannot open mint, redemption, or rebalance intents until migrated.

Before upgrading, pause new operations and finish or cancel all outstanding intents.
After upgrading, run the migration for each fixed-weight basket missing USDC:

```sh
node scripts/register-rebalance-quote.mjs --index <address>
node scripts/register-rebalance-quote.mjs --index <address> --execute
```

The first command builds the instruction without sending it. The second appends the
cash component, initializes its accounted reserve from the live USDC balance, and
checks the result. It pays rent only if a new page or ATA is needed. Existing component
indexes remain unchanged. Accounts retain their existing layouts. Refresh cached page
and component counts in clients before resuming operations. Migration refuses an active
operation or duplicate USDC component; a basket already at the 50-component limit needs
a separate capacity migration before it can append cash. Reserve one of the 50 slots
for USDC in new baskets.

Rebalance buys also have a protocol-enforced spending ceiling: the original buy leg's
value at the execution oracle, plus the bounded execution slippage. A keeper's quote
limit can tighten this ceiling but cannot increase it. Over-delivery at a better price
remains valid, while spending the whole cash reserve on a small buy is rejected before
any swap transaction can commit. The final NAV/drift checks and unwind remain in place.
