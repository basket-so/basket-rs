# basket-rs

Anchor workspace for an index protocol on Solana. Index tokens are pro-rata
claims on component vault balances, with support for fixed-unit baskets and
fixed-weight target policies.

The protocol uses Jupiter for routed execution. Mints and redeems need no
oracle: each is bounded by the user's own `max_quote_in` / `min_quote_out`.
Fixed-weight rebalances read USD prices the protocol's oracle key signs for each
rebalance step (see Rebalance Prices); those prices drive rebalance weights, NAV
checks, and execution-price guards.

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

Components keep an `oracle_pair` field from the Switchboard era; nothing reads it.
Rebalances read each component's price from the oracle's signed prices by component index.

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

- uses the oracle's signed USD prices to compute current weights and target amounts
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
- `set_large_basket_component_oracle_pair`: sets a component's legacy `oracle_pair` field (unused); only allowed while supply is zero and no intent is mid-flight
- `set_price_oracle`: protocol authority sets the key whose signed prices rebalances accept, creating the price oracle account on first use; prices the previous key signed stop verifying at once
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
- `collect_large_basket_intent_fees`: after every component has executed, charges the USDC fee split (protocol, creator, staking) on the realized swap volume; swap-path intents only
- `finalize_large_basket_mint_intent`: requires collected fees on swap-path intents, re-checks backing and supply against the budget and cap, books the filled reserves, mints the index tokens, and releases the lock
- `finalize_large_basket_redeem_intent`: enforces the net USDC min-out on swap-path intents and releases the lock
- `cancel_unfilled_large_basket_mint_intent` / `cancel_unfilled_large_basket_redeem_intent`: owner aborts an intent before any component fills; redeem restores reserves and re-mints the burned tokens
- `cancel_expired_large_basket_intent`: permissionless after expiry; returns filled mint backing (or unfilled redeem backing) to the owner and releases the lock
- `close_large_basket_intent`: permissionless; closes a settled intent (finalized, or cancelled with nothing left in the refund escrow) that its owner's lock no longer points at, and returns its rent to the owner
- `open_rebalance_intent`: the index authority or its rebalance keeper opens a fixed-weight rebalance when the drift, time, or composition trigger fires; prices NAV from the oracle's signed prices, derives per-component sell/buy legs, and takes the operation lock. The keeper opens inside its own live request, or once the request window and cooldown have passed since its last request or the last cancelled or unwound rebalance, so it cannot hold the basket by reopening each time one is abandoned
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
check, which caps each swap's effective price against the signed price before
the transaction can commit. The finalize
NAV-preservation gate is a one-sided aggregate backstop: it re-prices the
original holdings and the post-rebalance holdings at the same fresh prices (so
market drift over the intent's life nets out), but because the keeper chooses
when prices are signed it cannot tighten below the per-leg bound and should not
be read as a hard standalone NAV guarantee.

Swap legs execute in batches and validate their effective quote/fill amounts
against the signed prices in the same transaction. Open, every batch, and
finalize each require their prices to be signed for that intent at most
`max_price_age_slots` (capped at `MAX_PRICE_AGE_SLOTS` = 50, about 20 seconds) earlier,
so the keeper has fresh prices signed just before each step.
Execution stops at `expires_at` (at most 30 minutes after open) and while the
authority has paused rebalancing.

`finalize_rebalance` re-prices the basket, enforces post-rebalance drift,
one-sided NAV loss within `nav_tolerance_bps`, and a quote-dust bound, then
refreshes every page's `units_per_index` / `accounted_reserve` from live vault
balances. Those gates judge what the rebalance's own swaps left in each vault (the
open balances moved by each leg's recorded fill), so tokens anyone sends to the
public vaults mid-rebalance cannot block finalize or count toward its bounds; a
vault holding less than that amount fails finalize. The gifts are booked to holders
by the refresh. When USDC is itself a component, its vault is the same ATA as the
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

## Rebalance Prices

Switchboard shut down in September 2026. Rebalances now read prices the protocol's
oracle key signs off chain for one rebalance intent (a pull oracle; nothing is stored on
chain but the key). Each rebalance step (open, each execute batch, verify, finalize) must
come right after a native Ed25519 program instruction carrying the oracle's signature over
this message (little-endian):

| bytes | field |
| --- | --- |
| 0..16 | tag `basket-prices-v1` |
| 16..48 | the rebalance intent address (for open, the PDA it creates from index + nonce) |
| 48..56 | `u64` slot the oracle signed at |
| 56 | `u8` entry count |
| then 6 per entry | `u8` global component index, `u32` mantissa, `u8` exponent: USD per whole token scaled by 1e18 is mantissa × 10^exponent |

The runtime verifies the signature before any instruction runs; the step then reads the
Ed25519 instruction through the instructions sysvar and requires exactly one signature,
signature/key/message offsets that all point into that instruction's own data (offsets into
another instruction would make the runtime verify bytes the program never reads), the key
in the `["price-oracle"]` account, this intent, a slot no later than the current one and at
most `max_price_age_slots` old (capped at `MAX_PRICE_AGE_SLOTS` = 50, about 20 seconds),
no component listed twice, and every price positive and within `i128` (checked math). USDC is
always $1 and is never signed. Every byte counts, since open and finalize price every
component in one transaction and a swap shares its transaction with a Jupiter route:
- Prices name components by index, not mint. A basket's slots are append-only and each mint
  holds one, so an index always means the same mint, and the oracle maps mints to indexes
  from the basket's pages itself. A `u8` index covers every component (the program asserts
  `MAX_LARGE_BASKET_COMPONENTS` ≤ 256 at compile time).
- Prices are decimal floating point. The oracle rounds to the nearest price with the largest
  mantissa that fits a `u32`, which keeps 9 to 10 significant digits: a relative error under
  1.2e-9, about a millionth of a basis point, far below the bps-level bounds prices are
  checked against.
- The message names no program id: the intent is a PDA of this program, which already ties
  the signature to it.

Only the protocol authority sets the oracle key (`set_price_oracle`); prices the previous
key signed stop verifying at once. The oracle key is separate from the rebalance keeper's:
the keeper trades but cannot set prices, and the oracle sets prices but cannot trade, so
one leaked key alone cannot move value through a rebalance beyond the per-leg and NAV
bounds. That only holds while the two keys live on different hosts, so the oracle key
belongs in the oracle service (see Oracle Service), not in the keeper's deployment. Holders
trust the protocol's oracle for rebalance pricing; mints and redeems never read it.

`scripts/lib/price-oracle.mjs` computes the signed prices from free sources: the midpoint
of a small Jupiter round trip (USDC -> token -> USDC). It is signed only when every
available reference (Jupiter's price API, if recently updated, and DexScreener's most
liquid pair) agrees within 3%, at least one is available, and the round trip costs under
4% without gaining money. A token that fails any check is not priced, so its basket does
not rebalance until it can be. Before trading, the keeper also checks every leg's worst
allowed fill against the signed price and the per-leg bound, and cancels the intent before
any swap if one cannot pass. `scripts/lib/signed-prices.mjs` encodes and signs the message
for both the keeper and the oracle service.

The signature instruction costs 170 bytes plus 6 per price. With the basket's address lookup
table (see Rebalance Worker), every account a step names costs one byte, so transaction
bytes no longer limit basket size: a 50-component open is about 955 bytes. What binds is
mainnet's limit of 64 accounts per transaction (the feature raising it to 128 is inactive):
open names every page and vault plus 15 fixed accounts (2 of them, the Ed25519 program and
the instructions sysvar, are the oracle's), so it fits baskets of up to 45 components, and
finalize up to 48. The local validator test opens and finalizes a 45-component basket
through its table with that limit enforced (about 300k compute units each). Every basket,
fixed-unit or fixed-weight, is capped below that at 40 components (`MAX_BASKET_COMPONENTS`,
checked at creation and when the USDC slot is registered; composition changes already stop
at 20), so a 40-slot open names 58 accounts. The cap counts slots, removed components
included, exactly as open names them. It is a validation cap only: account layouts stay sized
for `MAX_LARGE_BASKET_COMPONENTS` (50), because intent accounts already on chain have that
size. Raising the cap past 45 would need open to name fewer accounts, open and finalize to
page over several transactions, or the 128-account feature. Execute
batches fit two swap legs per transaction for most routes; a leg whose route would not fit
is re-quoted on a smaller route.

## Remaining Account Order

`initialize_large_basket_component_page` expects, after the named accounts, one
group per component in this page's `components` argument order:

1. component mint
2. vault ATA for the vault-authority PDA, that mint, and that mint's token program
3. token program that owns the mint (`spl_token::ID` or Token-2022)

The handler creates each vault ATA idempotently. A Token-2022 mint with the
transfer-fee extension is refused: a fee taken on the way into a vault would leave
the books above what the vault holds.

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
fee from the owner, and refuses a deposit the vault does not receive in full; redeem
releases the net backing to the owner and skims the fee, all from the component vault.
The in-kind fee is the configured total rate rounded up once and then split between
the protocol (with the staking share) and the creator, so it never exceeds the
component amount.

`collect_large_basket_intent_fees` takes no remaining accounts; it requires every
component executed, then transfers the USDC fee split from the owner's USDC account
to the protocol and creator fee token accounts and the staking reward vault.

`finalize_large_basket_mint_intent` expects all component pages in page order, all
writable; it re-checks backing, books the filled reserves onto the pages, mints the
index tokens, and releases the lock. A swap fill records everything its route delivered
to the vault, so an ExactIn route's surplus is booked as backing (or returned with an
expiry refund) rather than left outside the books. `finalize_large_basket_redeem_intent` takes no
remaining accounts.

`cancel_unfilled_large_basket_mint_intent` takes no remaining accounts.
`cancel_unfilled_large_basket_redeem_intent` expects all component pages (writable)
so it can restore the reserved backing before re-minting the burned tokens.

`cancel_expired_large_basket_intent` expects all component pages (writable) first,
then a `[component mint, component vault, owner token account, component token
program]` group for every component whose backing is returned to the owner (filled
components for a mint, unfilled components for a redeem). Pass no component groups
when there is nothing to return, including an intent whose only filled (mint) or
unfilled (redeem) components are zero amounts; the pages alone settle it.

Every priced rebalance step (`open_rebalance_intent`, both execute batches,
`verify_rebalance_component_price`, `finalize_rebalance`) names the `["price-oracle"]`
account and the instructions sysvar, and must come immediately after the oracle's Ed25519
signature instruction (see Rebalance Prices).

`open_rebalance_intent` takes `OpenRebalanceIntentArgs { nonce, expires_at,
max_price_age_slots, nav_tolerance_bps, max_post_rebalance_drift_bps }` and remaining
accounts of all component pages in page-index order followed by all component vaults in
global component order.

`execute_rebalance_sell_batch` / `execute_rebalance_buy_batch` take
`ExecuteRebalanceBatchArgs { entries, max_price_age_slots, max_oracle_slippage_bps }`,
where each entry carries
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
`MAX_FIXED_WEIGHT_EXECUTION_SLIPPAGE_BPS`), and `max_price_age_slots`,
and bounds the recorded quote/fill effective price against a fresh signed price.

`finalize_rebalance` takes `max_price_age_slots` and the same pages-then-vaults remaining
accounts as open (pages writable).
`unwind_rebalance` takes no args and the same pages-then-vaults layout.

## Current Scope

This cut targets paged large-basket indexes (up to 40 components across pages of
ten, so a rebalance fits one transaction with room to spare) with a classic SPL index mint and Token-2022-capable component vaults, the
batched mint/redeem intent state machine (Jupiter swap path plus in-kind variants),
keeper-run fixed-weight rebalancing on the intent model priced by the protocol's
signed-price oracle, Metaplex metadata, BASKET staking with protocol/creator/staking
fee splits, pause/supply-cap controls, and external governance ownership of index
authority. Fixed-unit (authority-proposed, timelocked) rebalancing is not yet
available (see Rebalancing).

## Rebalance Worker

`scripts/rebalance-bot.mjs` is a keeper worker for fixed-weight baskets. It auto-discovers
every FixedWeights index (or targets one with `--index <pubkey>`), prices the basket from
Jupiter, computes each component's weight drift, and — when the index's drift threshold or
time interval has triggered — drives the on-chain rebalance state machine end to end:
`open_rebalance_intent` → batched `execute_rebalance_sell_batch` / `execute_rebalance_buy_batch`
(Jupiter routes encoded as the compact `LargeBasketSwapPlan`, each leg price-checked
atomically) → `finalize_rebalance`. Before each of those steps the tokens involved are
priced with `scripts/lib/price-oracle.mjs` and signed for the intent by the oracle key: by
the oracle service when `ORACLE_URL` is set (see Oracle Service), otherwise by a local
oracle key (development only). Each step goes out as `[compute budget, Ed25519 signature,
step]`. It also detects a stuck/expired intent and calls `unwind_rebalance` to release the
operation lock.

It is **dry-run by default** — it reads chain state, fetches quotes, and prints the plan but
sends no transactions. Pass `--execute` to send. `--execute` spends the keeper's SOL (transaction
fees + rent) and moves basket assets; trading costs are paid from basket assets, so validate with the dry run first.

```bash
node scripts/rebalance-bot.mjs                 # detect-only, scan all FixedWeights baskets
node scripts/rebalance-bot.mjs --preview-swaps # dry-run + encode the Jupiter swaps read-only
node scripts/rebalance-bot.mjs --show-prices   # dry-run + compute the prices it would have signed
node scripts/rebalance-bot.mjs --index <pk>    # only this index (skips getProgramAccounts)
node scripts/rebalance-bot.mjs --execute       # actually rebalance triggered baskets
node scripts/rebalance-bot.mjs --watch         # poll forever (--interval <seconds>)
```

On `--execute` the worker builds, tx-size-packs, and route-scope-validates every sell and
buy batch *before* sending any execute transaction; if a leg can't be built/sized/scoped it
cancels the just-opened (zero-legs-executed) intent and reclaims its rent rather than
stranding the operation lock. It derives time from the on-chain clock, keeps the intent TTL
under the program cap, sizes compute units per batch, refuses up front a basket whose open
or finalize would not fit one transaction, and has fresh prices signed before each step,
re-signing and resending (up to three attempts) if the step lands after the 50-slot window.
Failed transaction confirmations stop execution.

Every rebalance step compiles with the basket's address lookup table, which the worker owns
and maintains itself:
- **What it holds:** the basket's accounts that steps name: index state, index mint, vault
  authority and quote account, pages, every component's vault and mint, the `price-oracle`
  account, the instructions sysvar, and the token, associated-token, system and Jupiter
  programs. Not signers, the intent, or the programs instructions invoke directly.
- **Finding it:** nothing is stored between runs. The worker finds its table with
  `getProgramAccounts` on the lookup table program, filtered by its own key as authority,
  and picks the active table that holds the basket's index state.
- **Creating and extending:** on the first `--execute` rebalance of a basket, before the
  intent opens, the worker creates the table, with the keeper key as authority and payer.
  It appends missing accounts 20 per transaction, for example after a composition change
  adds components, and waits out the one-slot warm-up before using it. If the search
  fails, it never creates a second table; the rebalance is skipped instead.
- **Cost:** the keeper's SOL pays the table's rent, 0.0035 to 0.0048 SOL for the live
  baskets (56 + 32 bytes per address; 16 to 24 addresses each, about 0.021 SOL for all
  five), plus one or two transaction fees. The rent is recoverable by deactivating and
  closing the table.
- **Dry runs:** without `--execute` the worker only logs the table it would create or
  extend, and `--preview-swaps` sizes batches against a stand-in.
- **Unwind:** it uses the table when one is found, but never depends on it.

Each step is checked against both 1232 bytes and mainnet's 64 accounts per transaction.

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
RPC), `KEEPER_KEYPAIR` or `ANCHOR_WALLET` (keeper keypair, default `deployer-keypair.json`),
`ORACLE_URL` and `ORACLE_API_TOKEN` (the oracle service; `ORACLE_KEYPAIR` must then be unset),
or else `ORACLE_KEYPAIR` or `ORACLE_WALLET` (a local oracle key, default
`oracle-keypair.json`; it needs no SOL and must differ from the keeper's), `JUPITER_SWAP_API`,
`JUPITER_PRICE_API`. Without either the worker can still dry-run but skips rebalances. With
`ORACLE_URL`, `--show-prices` asks the service for a dry run and prints what would stop it
signing. Flags: `--batch-size` (default 2, at most 4; with signed prices and the basket's
lookup table most routes fit two legs, never three), `--slippage-bps`, `--verify-buffer-bps`,
`--nav-tolerance-bps`, `--drift-margin-bps`, `--ttl`, `--priority-fee`, `--interval`,
`--price-probe-usd` (default 50), `--price-max-spread-bps` (default 400),
`--price-max-deviation-bps` (default 300).

Set or rotate the oracle key with `node scripts/set-price-oracle.mjs [--oracle <pubkey>]
[--execute]` (signed by the protocol authority; creates the price oracle account on first use).

Prerequisites: the program build that includes the rebalance intent flow must be deployed and
`target/idl/basket.json` regenerated (`anchor build`) so the worker decodes accounts and
encodes instructions against the matching layout. The worker only manages USDC-quoted
FixedWeights baskets.

## Oracle Service

`scripts/oracle-service.mjs` is the only holder of the oracle key. It runs as its own Fly app
(`fly.oracle.toml`, `Dockerfile.oracle-service`) with no public address; the keeper reaches
it on Fly's private network. Before each rebalance step the keeper sends it the basket, the
intent's nonce and the mints to price (`POST /sign {index, nonce, mints}`, bearer
`ORACLE_API_TOKEN`). The service derives the intent address itself, maps each mint to its
component index from the basket's pages on chain, prices the mints with
`scripts/lib/price-oracle.mjs`, and returns the signed message (`{ slot, intent, message,
signature, oracle, prices }`). The keeper chooses which mints and when, never the prices or
the indexes. The service sends no transactions, so its key needs no SOL.

Whatever it is asked, the service only signs for a FixedWeights basket and mints that
basket holds. It signs at most `--max-signatures-per-hour` messages (default 120). It refuses
a price more than `--max-move-bps` (default 1500) from the first price it signed for the same
intent, which pins a rebalance to its opening prices, or otherwise from the last price it
signed for that mint within `--move-window-s` (default 900). Every `--self-check-interval-s`
(default 3600) it prices every token a rebalance would need, without signing, and logs the
result. `GET /health` reports its key, whether the program's price oracle account accepts
it, the signatures left this hour and the last self-check.

Until the program accepts its key it stands by: dry runs (`"dryRun": true`) and self-checks
work, signing is refused with 409, and a keeper pointed at it skips rebalances.

```bash
fly deploy -c fly.oracle.toml --ha=false        # secrets: ORACLE_KEYPAIR, ORACLE_API_TOKEN
fly logs -a basket-price-oracle                 # startup status and self-checks
```

To switch rebalances over, once the program build with signed prices is deployed:

1. Redeploy the oracle service with this code (`fly deploy -c fly.oracle.toml --ha=false`)
   and read its key from `GET /health` or its startup log.
2. Point the program at that key with
   `node scripts/set-price-oracle.mjs --oracle <key> --execute` (protocol authority). Its
   `/health` then reports `accepted: true`.
3. Copy `target/idl/basket.json` to `docs/program-upgrade/basket.idl.json` (the keeper image
   ships that IDL). On the keeper app, set `ORACLE_URL=http://basket-price-oracle.internal:8080`
   and the same `ORACLE_API_TOKEN`, unset `ORACLE_KEYPAIR`, and redeploy it. The keeper
   refuses to start while `ORACLE_URL` and `ORACLE_KEYPAIR` are both set. Its RPC must allow
   `getProgramAccounts` (discovery already needs it; the lookup table search does too).
4. Nothing else to set up for lookup tables: on each basket's first `--execute` rebalance the
   keeper creates its table (about 0.004 to 0.005 SOL rent from the keeper wallet) before
   opening the intent, and reuses it afterwards. A dry run (`node scripts/rebalance-bot.mjs`)
   beforehand logs the tables it would create.

Run its checks with `node --test tests/oracle-service.test.mjs`.

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
Rebalance batch execution requires `max_price_age_slots`, `max_oracle_slippage_bps`,
and the oracle's signed prices. Execution validates the realized price atomically against
the signed price and marks the leg verified; a deferred check cannot protect an already
committed swap.

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
operation or duplicate USDC component; a fixed-weight basket already at the 40-component
limit cannot append cash. Reserve one of the 40 slots for USDC in new fixed-weight baskets.

Rebalance buys also have a protocol-enforced spending ceiling: the original buy leg's
value at the execution oracle, plus the bounded execution slippage. A keeper's quote
limit can tighten this ceiling but cannot increase it. Over-delivery at a better price
remains valid, while spending the whole cash reserve on a small buy is rejected before
any swap transaction can commit. The final NAV/drift checks and unwind remain in place.
