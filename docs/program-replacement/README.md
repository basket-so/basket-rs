# Mainnet program replacement

Completed on 2026-09-08 UTC.

- Previous program: `9LNEoShrH93XekWQTFmZBdUdMu8ugJxBr5cbfqJQC1mw` (closed).
- New program: `bskthjNMRWQ4ekDLxaAzA1e39ThPmEtUgHY3XHfs7qv` (deployed and executable).
- Staking mint: `2rNBaMg5VAr1aMNCwAPdDZVgzzdTaNDebUnNqPFNmeta` (6 decimals).
- Staking pool: `4KYActSwKD9mTbfnw1sXpiqUg9yZxkjK8RWLhxsgXTPX`.

All 12 visible baskets were recreated with their original compositions, policies,
and zero fees. Initial NAV was $1 within $0.00001 rounding tolerance at each
recorded creation-price snapshot. Subsequent NAV follows component prices.
The new accounts were verified with zero supply and empty vaults after creation.

The user explicitly authorized abandoning the ten tiny XTEST5 vault balances in
`approved-stranded-vaults.json`. Closure checked this exact approved set, zero
index supplies, zero staking balances, and no active intents. The old program
closure recovered 11.7050844 SOL; `closure.json` records the transaction.
Historical preflight and previous IDL/program data are retained for reference.
Preflight/recovery scripts target the closed deployment and cannot be used to
inspect the new program.

`program.json` records deployment, binary verification, protocol/staking
initialization, accounts, and transaction signatures. `deployment.json` and
`DEPLOYMENT.md` record all basket addresses, creation prices, transactions, and
verification. `readiness.json` records mint, oracle, and liquidity route checks.
Private keypairs remain only in ignored local files.

See `REVIEW.md` for the rebalance vulnerability fixed before deployment and the
validation scope. The deployed binary was verified byte-for-byte:

- Size: 1,422,640 bytes.
- SHA256: `9c3eb0d80fb899977bb676a0ac60300a53d4092f1253ed76d3da9f343aaa5a49`.

UI clients and configuration now use the new program, paged basket ABI, all 12
new index mints, their individual lookup tables, and the six-decimal staking mint.
The landing page retains five featured baskets with refreshed identities/data.

## Published apps

- UI: https://app.basketsolana.xyz — Fly image
  `deployment-01M1ZKR6RDW3CYJY8GHRTXXY3H`.
- Landing: https://basketsolana.xyz — Fly image
  `deployment-01M1ZKSJQGXFM4H221A7QTKA2H`.

Both deployments passed Fly health checks. Public verification returned HTTP 200
for SOLB, markets, and staking; the published configuration contains the new
program; `/api/protocol` reports all 12 new basket mints and initialized staking
with the requested six-decimal mint. Landing HTML contains all five new featured
mint identities. Existing landing waitlist storage was preserved.
