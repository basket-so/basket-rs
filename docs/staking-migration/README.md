# Staking mint migration preflight

Status: **not executed**. No on-chain transactions, program upgrades, or UI mint changes were submitted.

Requested mint: `2rNBaMg5VAr1aMNCwAPdDZVgzzdTaNDebUnNqPFNmeta` (classic SPL Token, initialized, 6 decimals).
Existing mint: `5yTFbtAE5RDjxpiVpDfyWuzcCWgwh659CEu7a7ZQtSpk` (9 decimals).

The mainnet pool and both existing vaults are initialized and empty. The one existing stake-position account has zero stake, zero pending rewards, and zero fractional rewards. Pool reward accumulator, unallocated rewards, and remainder are also zero. These conditions must be rechecked and enforced on-chain at execution.

## Blocking prerequisites

1. Obtain the source and build configuration for the current mainnet executable. HEAD `9b784de` contains an incompatible pending rewrite that removes the live small-index instructions; predecessor `05bfcd9` does not contain the deployed large-basket instructions. Neither local program artifact matches the deployed binary. No additional unreachable commits were found. Do not deploy either version as a staking-only change.
2. Fund the existing upgrade authority sufficiently for a temporary upload buffer. Preflight found approximately 0.365 SOL available and no reusable buffers. A buffer comparable to the existing program allocation requires approximately 10.65 SOL, before fees. Recalculate from the final tested artifact before funding or uploading; buffer rent is normally returned to the spill account on successful upgrade.

## Intended migration after prerequisites

- Preserve the current deployed instructions and state layouts.
- Introduce an authority-restricted, one-time migration from the exact old mint to the exact requested new mint.
- Enforce zero total stake, zero old stake-vault balance, zero reward-vault balance, and zero reward accounting; validate existing empty positions without resetting obligations.
- Make staking and unstaking validate the mint recorded in the pool so the upgrade itself does not disable the old pool before migration succeeds.
- Validate/create the new mint's canonical stake ATA owned by the existing staking authority, then switch the pool's basket mint. Preserve the pool address, USDC reward mint/vault, and fee routing.
- Exercise successful migration, unauthorized/repeated/wrong-mint attempts, nonempty pool/vault guards, and staking/unstaking with the new 6-decimal mint on a local validator with the old state.
- Simulate against mainnet, upgrade with the existing authority, migrate, verify on-chain, then update the app's fallback/configuration and deploy the UI.

`preflight.json` records the read-only checks. `deployed-before.so` is a backup of the existing ProgramData executable region, including its trailing allocation padding; its hash is recorded in the report.
