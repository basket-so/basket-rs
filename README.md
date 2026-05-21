# omnindex-rs

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
token. Authority-managed rebalances can change that target basket behind the
configured timelock. Rebalance proposals and execution use Switchboard prices
for old-vs-new NAV checks, and execution uses caller-provided Jupiter routes.

For non-USDC components, `IndexComponent.oracle_pair` is interpreted as the
component's Switchboard feed id. The field name is legacy.

## Fixed-Weight Indexes

A fixed-weight index stores target weights in basis points on each component.
Target weights are a rebalance policy, not a redemption formula.
Each component must also specify nonzero `units_per_index` at creation time.
Those units define the initial basket used by the first mint while supply is
zero; later mints and redeems use pro-rata vault-share accounting, and
fixed-weight rebalances refresh stored units from the post-rebalance vault
balances.

`rebalance_fixed_weights_with_jupiter` is the fixed-weight execution path. It:

- uses Switchboard USD prices to compute current weights and target amounts
- uses native Solana USDC as the quote leg
- sells overweight components to USDC, then buys underweight components
- verifies every Jupiter route's mints, token accounts, protected vault scope,
  vault balance deltas, execution price, quote dust, and final drift

## Instructions

- `initialize_protocol`: creates the protocol config PDA with permissioned index creation enabled by default
- `update_protocol_config`: rotates protocol authority, sets the primary approved index creator, or enables permissionless index creation
- `update_index_creator_whitelist`: adds or removes additional approved index creators while creation remains permissioned
- `create_index`: creates the index account and index mint PDA
- `create_index_metadata`: creates Metaplex metadata for the index mint
- `update_index_metadata`: updates the index mint metadata URI
- `migrate_index_metadata_authority`: moves legacy Metaplex update authority to the index vault-authority PDA
- `initialize_vaults`: creates the vault ATA for each component mint
- `initialize_staking_pool`: initializes the BASKET staking pool plus BASKET stake vault and USDC reward vault
- `stake_basket`: stakes BASKET into the protocol staking vault
- `unstake_basket`: settles rewards, then unstakes BASKET
- `fund_staking_rewards`: transfers USDC into the reward vault and accrues it to currently staked BASKET
- `claim_staking_rewards`: claims accrued USDC rewards for a BASKET staker
- `quote_mint_index`: emits component amounts needed for a direct component mint when mint fees are zero
- `quote_redeem_index`: emits component amounts returned by a direct component redeem when redeem fees are zero
- `mint_index`: transfers the pro-rata vault share into vaults and mints index tokens
- `mint_index_with_jupiter`: spends USDC through caller-provided Jupiter routes, verifies fills against Switchboard, deposits into vaults, and mints index tokens
- `update_fees`: sets protocol and creator mint/redeem fee bps, capped at 10% total per direction
- `update_config`: sets protocol and creator fee recipients, supply cap, rebalance delay, and pause flags
- `update_authority`: transfers index authority to a new wallet, multisig, DAO, or governance PDA
- `claim_fees`: disabled under pro-rata vault-share accounting
- `propose_rebalance`: stages a fixed-unit basket update behind the configured timelock using Switchboard NAV checks
- `cancel_rebalance`: clears the pending rebalance proposal
- `execute_rebalance`: executes the pending fixed-unit rebalance through Jupiter routes and verifies final vault backing
- `rebalance_fixed_weights_with_jupiter`: permissionlessly rebalances a fixed-weight index through Jupiter routes with Switchboard price guards
- `redeem_index`: burns index tokens and transfers the pro-rata vault share back out of vaults
- `redeem_index_with_jupiter`: burns index tokens, routes component backing to USDC through Jupiter, verifies execution against Switchboard, and pays the user in USDC

Nonzero mint/redeem fees are supported on the USDC Jupiter paths. `mint_fee_bps`
and `redeem_fee_bps` are protocol fees paid to `fee_recipient`;
`creator_mint_fee_bps` and `creator_redeem_fee_bps` are creator fees paid to
`creator_fee_recipient`. The total protocol plus creator fee for each direction
is capped at 1,000 bps. Direct component mint/redeem and quote helpers reject
nonzero fees because those paths do not have a single USDC quote asset to split.

The BASKET staking mint is fixed at
`5yTFbtAE5RDjxpiVpDfyWuzcCWgwh659CEu7a7ZQtSpk`. Staking rewards are funded
through `fund_staking_rewards`, which accrues deposited USDC only when BASKET is
already staked. Component vault surplus belongs to index holders under pro-rata
accounting.

## Rebalancing

Fixed-unit rebalancing is a two-step flow. The authority proposes the new
basket with `propose_rebalance`, waits until `pending_rebalance_available_at`,
then calls `execute_rebalance` with fresh Switchboard verification and a Jupiter
swap plan.

Both proposal and execution check old basket NAV against new basket NAV using
Switchboard prices. If a `RebalancePriceInput.price_nad` is omitted, the
verified Switchboard price is used. If it is supplied, it must be within
`oracle_price_tolerance_bps` of the verified Switchboard price. Prices are
scaled by `1e9` and interpreted as UI-token USDC prices. Mint decimals are
loaded from the supplied mint accounts.

Execution computes the backing required for the current index supply under the
new basket. It refuses to sell any amount needed for the target basket, invokes
Jupiter with the vault authority PDA only for the declared route source and
destination vaults, and commits the new basket only if final vault balances
satisfy target amounts. Removed components must be sold down to zero.

While a rebalance is pending, mints and redeems are rejected if the resulting
supply would make pending component targets fractional.

## Remaining Account Order

`initialize_vaults` expects pairs of remaining accounts:

1. component mint
2. vault ATA for the vault authority PDA and that mint

`quote_mint_index` expects no remaining accounts when current supply is zero.
When current supply is nonzero, pass one vault account per component in basket
order.

`quote_redeem_index` expects one vault account per component in basket order.

`mint_index` expects pairs of remaining accounts:

1. depositor source token account
2. vault ATA for that component

`redeem_index` expects pairs of remaining accounts:

1. vault ATA for that component
2. redeemer destination token account

`mint_index_with_jupiter` expects native Solana USDC as the quote mint.
Component oracle fields are interpreted as Switchboard feed IDs. Fixed accounts
include the Jupiter program, Switchboard queue, verified Switchboard quote
account, slot hashes sysvar, and instructions sysvar. Include the Switchboard
quote update/signature instructions before the Omnindex instruction in the same
transaction.

When mint fees are nonzero, pass the protocol fee recipient's USDC token account
and the creator fee recipient's USDC token account in the fixed account list.
Fees are charged after backing purchases, and `max_quote_in` covers backing plus
both fee splits.

Remaining accounts start with component groups in basket order:

1. component mint
2. vault ATA for that component
3. token program for that component mint

After all component groups, append every account required by the Jupiter swap
instructions returned by Jupiter. The `swaps` args must be ordered by basket
component order for every non-USDC component. Each route must spend from the
user's USDC token account and deposit into the component vault.

`redeem_index_with_jupiter` mirrors `mint_index_with_jupiter`. Component groups
are:

1. component mint
2. vault ATA for that component
3. token program for that component mint

Append all Jupiter route accounts after the component groups. The `swaps` args
must be ordered by basket component order for every non-USDC component. Each
route must spend from the component vault and deposit into the user's USDC token
account.

Redeem fees are deducted from the user's gross USDC output after route
execution. `min_quote_out` is checked against the user's net USDC after protocol
and creator fees.

`propose_rebalance` takes:

1. new component list
2. quote mint, currently native Solana USDC
3. price inputs for every unique old/new component mint
4. oracle price tolerance in basis points
5. old-vs-new NAV tolerance in basis points
6. maximum age for the verified Switchboard quote

Price inputs and remaining mint accounts must be ordered by:

1. all old basket components in old basket order
2. followed by any new basket component mint not already included, in new basket order

For each mint in that order, append:

1. component mint account

`execute_rebalance` takes `ExecuteRebalanceArgs { swaps, prices,
switchboard_max_age_slots }`. It reads the pending quote mint and tolerance
settings from the stored proposal. The price input order matches
`propose_rebalance`.

Remaining accounts start with triples for every unique old/new component mint:

1. component mint
2. vault ATA for the vault authority PDA, that mint, and the mint token program
3. mint token program (`spl_token::ID` or Token-2022)

After all component triples, append every account required by the supplied
Jupiter route instructions. Each route must use a component vault as its source
and a component vault as its destination. Protected component vaults that are
not the declared source or destination are rejected.

`rebalance_fixed_weights_with_jupiter` takes:

1. `max_quote_dust`: maximum allowed remaining external USDC atoms when USDC is not itself a component
2. `max_post_rebalance_drift_bps`: maximum allowed final component drift
3. `switchboard_max_age_slots`: maximum age for the verified Switchboard quote
4. `swaps`: Jupiter route instructions, first sells for overweight components in component order, then buys for underweight components ordered by largest value deficit

Remaining accounts start with component groups in current component order:

1. component mint
2. component vault ATA
3. token program for that component mint

After all component groups, append every account required by the supplied
Jupiter swap instructions. Each sell route must spend from a component vault and
deposit into the vault-authority USDC ATA. Each buy route must spend from that
USDC ATA and deposit into the destination component vault.

## Current Scope

This cut targets classic SPL Token direct component mint/redeem flows,
Token-2022-compatible Jupiter/Switchboard paths, Metaplex metadata,
authority-managed timelocked rebalancing, permissionless fixed-weight
rebalancing, BASKET staking, pause/supply-cap controls, and external governance
ownership of index authority.

## Mainnet Deployment

Mainnet deployment is intentionally program-only. It must not call
`create_index`, `initialize_vaults`, or any bootstrap script that creates an
index. The first mainnet index should be created explicitly by the protocol
operator/admin after the program and protocol config are initialized.

Current mainnet ID:

- Omnindex: `H6JKCZU82gCADQZ98Jmfj7UzpHTdbyfnDpt7AD5NZ3Lt`

Safe sequence:

1. Build and deploy Omnindex with the mainnet program ID.
2. Have the current program upgrade authority call `initialize_protocol` with the desired protocol authority and approved `index_creator`.
3. Call `initialize_staking_pool` to create the BASKET stake vault and USDC reward vault.
4. Verify `permissionless_index_creation` is false.
5. Stop. Do not create an index as part of deployment.
6. Create the first curated index manually through the admin flow.
