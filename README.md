# omnindex-rs

Initial Anchor workspace for an index protocol on Solana. Index tokens are
pro-rata claims on component vault balances, with support for fixed-unit target
baskets and fixed-weight target policies.

## Accounting model

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
`units_per_index` remains useful as a target/policy description and as the
starting basket for a zero-supply index.

## Fixed-unit indexes

A fixed-unit index has a target basket defined in atomic units per full index
token.

Example:

- `XYZ` has `6` decimals
- `1 XYZ = 1 JUP + 1 OMFG`
- If `JUP` and `OMFG` each have `6` decimals, then both basket components are stored as `1_000_000` units per index token

The initial mint of `2.5 XYZ` means minting `2_500_000` atomic units of the
index token, which requires:

- `2_500_000` atomic units of `JUP`
- `2_500_000` atomic units of `OMFG`

Authority-managed rebalances can change the target basket, but mint/redeem
still uses the pro-rata vault-share rule once supply exists.

## Fixed-weight indexes

A fixed-weight index stores target weights in basis points on each component.
For example:

```text
Component A: 2500 bps
Component B: 5000 bps
Component C: 2500 bps
```

Target weights are a rebalance policy, not a redemption formula. The
permissionless `rebalance_fixed_weights` instruction reads configured Omnipair
oracles, computes actual weights from vault balances and oracle prices, and
executes when either:

```text
now >= fixed_weight_last_rebalanced_at + fixed_weight_rebalance_interval_seconds
OR
max(abs(actual_weight - target_weight)) >= fixed_weight_drift_threshold_bps
```

Before executing swaps, every non-quote component's current Omnipair spot price
must be within `fixed_weight_spot_ema_max_deviation_bps` of its Omnipair EMA
oracle price. This prevents fixed-weight rebalances from running through a pair
whose short-term price has moved too far away from the oracle used to compute
targets.

The caller supplies execution tolerances, but no price, target, or explicit
swap route data. The caller only passes the accounts the Solana runtime
requires the program to read or mutate. Components must store a configured
Omnipair pair against the fixed-weight quote mint unless the quote mint itself
is one of the index components. The quote mint may also be an external routing
asset such as USDC; in that case all component pairs route through that external
quote, and any remaining quote dust must be no larger than the caller-provided
`max_quote_dust`.

## Instructions

- `initialize_protocol`: creates the protocol config PDA with permissioned index creation enabled by default; the program upgrade authority must sign the first initialization
- `update_protocol_config`: lets the protocol authority rotate the protocol authority, set the approved index creator, or later enable permissionless index creation
- `create_index`: creates the index account and the index mint PDA
- `create_index_metadata`: creates Metaplex metadata for the index mint
- `update_index_metadata`: updates the index mint metadata URI
- `migrate_index_metadata_authority`: moves legacy Metaplex update authority to the index vault-authority PDA
- `initialize_vaults`: creates the vault ATA for each component mint
- `initialize_staking_pool`: initializes the BASKET staking pool plus BASKET stake vault and USDC reward vault
- `stake_basket`: stakes BASKET into the protocol staking vault
- `unstake_basket`: settles rewards, then unstakes BASKET
- `claim_staking_rewards`: claims accrued USDC rewards for a BASKET staker
- `quote_mint_index`: emits the component amounts needed for a direct component mint when mint fees are zero
- `quote_redeem_index`: emits the component amounts returned by a direct component redeem when redeem fees are zero
- `mint_index`: transfers the pro-rata vault share into vaults and mints index tokens
- `mint_index_with_quote`: spends up to a quote-token budget, buys missing basket assets through direct Omnipair markets, deposits the basket, and mints index tokens
- `update_fees`: lets the index authority set optional mint/redeem fees in basis points
- `update_config`: lets the index authority set fee recipient, supply cap, rebalance delay, and pause flags
- `update_authority`: transfers index authority to a new wallet, multisig, DAO, or governance PDA
- `claim_fees`: disabled under pro-rata vault-share accounting; vault surplus belongs to index holders unless future fee vaults separate it before it enters component vaults
- `propose_rebalance`: stages new basket weights behind the configured timelock
- `cancel_rebalance`: clears the pending rebalance proposal
- `execute_rebalance`: applies the pending rebalance by executing a vault-to-vault Omnipair swap plan, then verifying vaults satisfy the target basket
- `rebalance_fixed_weights`: permissionlessly rebalances a fixed-weight index when its interval or drift trigger has fired
- `redeem_index`: burns index tokens and transfers the pro-rata vault share back out of vaults
- `redeem_index_to_quote`: burns index tokens, swaps non-quote components through direct Omnipair markets, and pays out one quote token

Fees are stored on the index as `mint_fee_bps` and `redeem_fee_bps`, and
default to zero. Nonzero fees are charged only through the quote-token
mint/redeem flows. If the quote mint is native Solana USDC
(`EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v`), the fee is transferred
directly into the staking reward vault. If the quote mint is another token, the
program stages the quote-token fee in a staking-authority ATA and swaps it
through a direct Omnipair `quote/USDC` market before accruing rewards. Direct
component `mint_index`, `redeem_index`, and their direct quote helpers reject
when the corresponding fee is nonzero because those paths do not route
component fees into USDC.

Mint fees are computed as basis points of the quote backing spent by
`mint_index_with_quote`; `max_quote_in` covers both basket purchases and the
quote-token fee. Redeem fees are computed as basis points of the quote output in
`redeem_index_to_quote`; the user receives the net quote amount after the
quote-denominated fee is removed. BASKET stakers accrue the resulting USDC
output from direct transfer or conversion. If fees arrive while no BASKET is
staked, they are tracked as unallocated rewards in the staking pool.

The BASKET staking mint is fixed at
`5yTFbtAE5RDjxpiVpDfyWuzcCWgwh659CEu7a7ZQtSpk`. `claim_fees` remains disabled;
vault balances are holder NAV under pro-rata accounting, while protocol fees
live in the separate staking reward vault.

Rebalancing is a two-step flow. The authority proposes the new basket with `propose_rebalance`, waits until `pending_rebalance_available_at`, then calls `execute_rebalance` with fresh prices and a swap plan. The proposal and execution both check old basket NAV against new basket NAV using prices in a quote mint such as USDC. If a component price is omitted, the program uses the Omnipair pair's EMA oracle. If a component price is supplied explicitly, it must be within `oracle_price_tolerance_bps` of the Omnipair EMA oracle. While a rebalance is pending, mints and redeems are rejected if the resulting supply would make pending component targets fractional. Execution computes the backing required for the current index supply under the new basket, swaps surplus old basket assets into deficient new basket assets through Omnipair, and updates the basket only if final vault balances satisfy the target amounts. The executor does not accept authority token accounts for top-ups or withdrawals.

For example, changing:

```text
1 index = 1 ABC + 1 XYZ + 1 DFG
```

to:

```text
1 index = 0.75 ABC + 0.75 XYZ + 0.75 DFG + 0.25 LMN
```

creates sellable surplus in ABC/XYZ/DFG. The `execute_rebalance` swap plan can sell those surplus amounts into LMN, then the program verifies that the vaults hold enough ABC/XYZ/DFG/LMN for the new basket before committing it. Proposing `1 ABC + 1 XYZ + 1 DFG + 1 LMN` would still require extra value from somewhere; the protocol will not silently donate that value during execution.

Risk controls are stored on the index. `max_supply = 0` means uncapped supply; otherwise minting fails once the cap would be exceeded. `minting_paused`, `redeeming_paused`, and `rebalancing_paused` gate their respective flows.

Index creation is permissioned by default. `initialize_protocol` creates a singleton protocol config PDA and sets an approved `index_creator`; `create_index` requires that signer unless `permissionless_index_creation` has been explicitly enabled with `update_protocol_config`. This keeps the early protocol curated while still allowing governance to open creation later.

## Remaining account order

`initialize_vaults` expects pairs of remaining accounts:

1. component mint
2. vault ATA for the vault authority PDA and that mint

`initialize_staking_pool` creates and validates:

1. staking pool PDA
2. staking authority PDA
3. staking authority BASKET ATA
4. staking authority USDC ATA

`stake_basket`, `unstake_basket`, and `claim_staking_rewards` use the staking
pool PDA, staking authority PDA, and the caller's stake-position PDA. Staking
and unstaking use the BASKET stake vault; claiming uses the USDC reward vault.

`quote_mint_index` expects no remaining accounts when current supply is zero.
When current supply is nonzero, pass one vault account per component in basket
order:

1. vault ATA for that component

`quote_redeem_index` expects one vault account per component in basket order:

1. vault ATA for that component

`mint_index` expects pairs of remaining accounts:

1. depositor source token account
2. vault ATA for that component

`mint_index` rejects when `mint_fee_bps > 0`; use `mint_index_with_quote` to
pay fees to BASKET stakers in USDC.

`mint_index_with_quote` expects component groups in basket order:

For every component:

1. component mint
2. user component token ATA (or the passed quote token account when the component itself is the quote token)
3. vault ATA for that component

For every component whose mint is not the quote mint, append:

4. Omnipair pair account for `quote/component`
5. Omnipair rate model account
6. Omnipair reserve vault for the quote mint
7. Omnipair reserve vault for the component mint

When `mint_fee_bps > 0`, the fixed accounts must include the staking pool,
staking authority, and staking reward vault. If the quote mint is not USDC,
append these fee conversion accounts after all component groups:

1. staking authority ATA for the quote mint
2. USDC mint
3. Omnipair pair account for `quote/USDC`
4. Omnipair rate model account
5. Omnipair reserve vault for the quote mint
6. Omnipair reserve vault for USDC

`redeem_index` expects pairs of remaining accounts:

1. vault ATA for that component
2. redeemer destination token account

`redeem_index` rejects when `redeem_fee_bps > 0`; use `redeem_index_to_quote`
to pay fees to BASKET stakers in USDC.

`claim_fees` is disabled under the current accounting model.

`propose_rebalance` takes:

1. new component list
2. quote mint used for NAV pricing
3. price inputs for every unique old/new component mint
4. oracle price tolerance in basis points
5. old-vs-new NAV tolerance in basis points

Price inputs must be ordered by:

1. all old basket components in old basket order
2. followed by any new basket component mint not already included, in new basket order

For each non-quote component mint, append remaining accounts:

1. Omnipair pair account for `component/quote`
2. Omnipair rate model account

If `price_nad` is `None`, the program uses the Omnipair EMA oracle. If `price_nad` is `Some`, it is accepted only when it is within `oracle_price_tolerance_bps` of the Omnipair EMA oracle. Prices are quote-token atoms per component atom, scaled by Omnipair's `NAD` (`1e9`). The quote mint itself uses price `NAD` and does not need pair accounts.

`execute_rebalance` takes `ExecuteRebalanceArgs { swaps, prices }`. The `prices` list uses the same mint order and validation rules as `propose_rebalance`, but execution reads the pending quote mint and tolerance settings from the stored proposal.

For each non-quote component mint in the price list, first append remaining accounts:

1. Omnipair pair account for `component/quote`
2. Omnipair rate model account

Each swap has:

1. token-in mint
2. token-out mint
3. amount in
4. minimum amount out

It expects pairs for every unique mint in:

1. all old basket components in old basket order
2. followed by any new basket component mint not already included, in new basket order

For each mint, pass:

1. component mint
2. vault ATA for the vault authority PDA and that mint

For each swap in `swaps`, append:

1. Omnipair pair account for `token-in/token-out`
2. Omnipair rate model account
3. Omnipair reserve vault for token-in
4. Omnipair reserve vault for token-out

The instruction refuses to sell any amount needed for the target basket. Removed components must be fully sold down to zero, otherwise execution fails to avoid stranding value outside the active basket.

`rebalance_fixed_weights` takes:

1. `max_quote_dust`: maximum allowed remaining external quote-token atoms after
   execution
2. `max_post_rebalance_drift_bps`: maximum allowed final component drift after
   swaps, fees, price impact, and integer rounding

It expects a vault-authority ATA for the configured fixed-weight quote mint in
the fixed accounts, then component groups in current component order:

For every component:

1. component mint
2. component vault ATA

For every component whose mint is not the fixed-weight quote mint, append:

3. configured Omnipair pair account for `component/quote`
4. Omnipair rate model account
5. Omnipair reserve vault for the component mint
6. Omnipair reserve vault for the quote mint

`redeem_index_to_quote` expects component groups in basket order:

For every component:

1. component mint
2. vault ATA for that component

For every component whose mint is not the quote mint, append:

3. Omnipair pair account for `component/quote`
4. Omnipair rate model account
5. Omnipair reserve vault for the component mint
6. Omnipair reserve vault for the quote mint

When `redeem_fee_bps > 0`, the fixed accounts must include the staking pool,
staking authority, and staking reward vault. If the quote mint is not USDC,
append these fee conversion accounts after all component groups:

1. staking authority ATA for the quote mint
2. USDC mint
3. Omnipair pair account for `quote/USDC`
4. Omnipair rate model account
5. Omnipair reserve vault for the quote mint
6. Omnipair reserve vault for USDC

## Current scope

This first cut targets classic SPL Token accounts, Metaplex metadata,
authority-managed timelocked rebalancing, permissionless fixed-weight
rebalancing, BASKET staking with USDC fee rewards, pause/supply-cap controls,
pro-rata component mint/redeem flows, and direct Omnipair quote-token
mint/redeem flows. External governance can own the index authority.

## Mainnet deployment

Mainnet deployment is intentionally program-only. It must not call `create_index`,
`initialize_vaults`, or any bootstrap script that creates an index. The first
mainnet index should be created explicitly by the protocol operator/admin after
the program and protocol config are initialized.

Current mainnet IDs:

- Omnindex: `H6JKCZU82gCADQZ98Jmfj7UzpHTdbyfnDpt7AD5NZ3Lt`
- Omnipair dependency: `omnixgS8fnqHfCcTGKWj6JtKjzpJZ1Y5y9pyFkQDkYE`

Safe sequence:

1. Build and deploy Omnindex with the mainnet program ID.
2. Have the current program upgrade authority call `initialize_protocol` with
   the desired protocol authority and approved `index_creator`.
3. Call `initialize_staking_pool` to create the BASKET stake vault and USDC
   reward vault.
4. Verify `permissionless_index_creation` is false.
5. Stop. Do not create an index as part of deployment.
6. Create the first curated index manually through the admin flow.
