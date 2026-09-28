# Curated basket expansion

Research date: 2026-09-07. Canonical definitions are in `baskets.json`.
The advertised set was read from `../omnindex-landing/lib/data.js`.
No advertised AUM, holder counts, APYs, or sample ticker prices were imported.

## Advertised baskets

| Symbol | Name | Policy | Initial value allocation | Launch disposition |
| --- | --- | --- | --- | --- |
| AIDX | AI Compute | Fixed weights | RENDER 28%, TAO 22%, AKT 18%, FET 16%, IO 10%, GRASS 6% | Held: verified liquid Solana AKT/FET representations not established; TAO wrapper review outstanding |
| SOLB | Solana Bluechips | Fixed units | SOL 40%, JTO 18%, JUP 16%, PYTH 14%, RAY 7%, ORCA 5% | Eligible subject to live preflight |
| DPIN | DePIN Mainnet | Fixed weights | HNT 26%, RENDER 20%, HONEY 16%, MOBILE 14%, IOT 12%, GEOD 12% | Held: legacy IOT/MOBILE liquidity |
| MEME | Memecoin Almanac | Fixed units | WIF 22%, BONK 20%, POPCAT 16%, MEW 14%, Fartcoin 14%, PNUT 14% | Eligible subject to live preflight |
| RWAX | RWA Treasury | Fixed weights | OUSG 36%, USDY 28%, ONDO 16%, STBT 12%, PAXG 8% | Held: transfer eligibility and unverified Solana representations |
| LSTY | LST Yield Stack | Fixed weights | JitoSOL 30%, mSOL 22%, bSOL 18%, INF 16%, JupSOL 14% | Eligible subject to live preflight |

The landing's RNDR is normalized to the issuer's current Solana RENDER mint,
as documented by the [Render Foundation](https://know.rendernetwork.com/general-render-network/rndr-to-render-what-you-need-to-know/render-network-upgrade-portal-faq).
Its FART label is mapped to Fartcoin, and WIF resolves to Jupiter's `$WIF` entry
by mint, not a similarly named token. MEME's prose says “one unit per name,”
which conflicts with its displayed allocations and the requested $1 NAV;
the implementation preserves those displayed initial allocations and then fixes
the corresponding quantities. Dynamic means price-driven weights, not automatic
market-cap selection or periodic membership changes.

[Helium HIP 138](https://www.helium.com/simple) consolidates rewards into HNT;
IOT/MOBILE remain legacy tradable/redeemable tokens. The discovery snapshot
reported only approximately $344 total IOT liquidity and $4,496 MOBILE liquidity.
Those are unsuitable for an automatically rebalanced 12%/14% allocation.

[OUSG](https://docs.ondo.finance/qualified-access-products/ousg/overview) transfers
are between onboarded eligible investors. No evidence establishes that the
protocol vault can receive/redeem it. [STBT](https://www.matrixdock.com/stbt?language=en)
is an Ethereum rebasing Treasury token. A permissionless SPL vault cannot simply
hold the advertised cross-chain basket. Missing entries have no fabricated mints
or placeholder oracle IDs and are excluded from execution.

## Six selected additions

Weights are curator choices, not outputs of a backtest or a claim of optimality.

| Symbol | Basket and allocations | Policy and rationale |
| --- | --- | --- |
| DEXS | Solana Exchange Leaders: JUP 50%, RAY 30%, ORCA 20% | Fixed weights: maintain diversified trading-protocol exposure |
| STKG | Staking Governance: JTO 50%, CLOUD 30%, MNDE 20% | Fixed units: governance exposure without forced recurring trades in thinner MNDE liquidity |
| USDX | USD Stablecoin Basket: USDC 50%, USDT 50% | Fixed units: diversify issuers without automatically buying more of a depegged asset |
| SOLC | Solana Balanced Core: SOL 40%, USDC 40%, JitoSOL 20% | Fixed weights: restore the 60% initial SOL exposure / 40% dollar allocation |
| AISP | Solana AI Infrastructure: RENDER 50%, IO 25%, GRASS 25% | Fixed weights: native Solana compute/rendering/data exposure without unverified cross-chain assets |
| DEFI | Solana DeFi Infrastructure: JUP 25%, RAY 20%, KMNO 20%, PYTH 15%, JTO 10%, ORCA 10% | Fixed weights: broader trading, lending, oracle and staking infrastructure exposure |

Primary-source basis:

- [Orca token documentation](https://docs.orca.so/governance/tokenomics) identifies
  its Solana governance/utility token and official mint. DEXS holds protocol
  tokens; it does not stake them or automatically collect protocol fees.
  [Jupiter's DAO](https://discuss.jup.ag/t/jupiter-dao-faq/26635) and
  [Raydium's token documentation](https://docs.raydium.io/ray) establish the
  other trading-protocol constituents.
- [Jito governance](https://www.jito.network/docs/governance/the-jito-governance-token-jto/),
  [Marinade MNDE](https://docs.marinade.finance/the-mnde-token), and
  [Sanctum CLOUD](https://sanctum.so/blog/state-of-cloud-q4-2025) support STKG's
  governance theme. Holding these tokens is distinct from holding LSTs.
- USDX uses the native Solana USDC/USDT mints verified against Jupiter and the
  chain, and the issuer references from [Circle](https://www.circle.com/multi-chain-usdc/solana?outputType=chromeless)
  and [Tether](https://tether.to/en/supported-protocols/).
  It is not a Treasury fund, yield product, or guarantee of a $1 future NAV.
  The deployed protocol values native USDC at $1; USDT receives a live oracle feed.
  [PYUSD](https://www.paypalobjects.com/devdoc/community/PYUSD-Solana-White-Paper.pdf)
  was researched and passed route checks but excluded because the deployed
  small-index vault path requires classic SPL tokens, whereas PYUSD uses Token-2022.
- [JitoSOL](https://www.jito.network/) accrues staking/network rewards within the
  token; [Sanctum's LST documentation](https://learn.sanctum.so/docs/introduction-to-lsts/from-native-to-liquid-staking)
  explains LST value accrual. SOLC combines that exposure with ordinary SOL and USDC.
- [Render](https://upgrade.rendernetwork.com/), [IO tokenomics](https://io.net/docs/guides/coin/io-tokenomics),
  and [Grass](https://www.grass.io/) establish AISP's infrastructure theme.
- [Kamino](https://kamino.com/governance-and-staking) and
  [Pyth](https://docs.pyth.network/pyth-token) support the lending/governance and
  oracle additions in DEFI. A protocol-token basket does not itself earn those
  protocols' lending yields.

Alternatives considered and excluded:

- Magnificent Seven xStocks: the [issuer supports Solana tokenized equities](https://xstocks.com/products),
  but several checked mints have non-unit scaled-UI multipliers. A durable
  integration needs explicit raw-unit oracle prices, as required by
  [Solana's integration guidance](https://solana.com/docs/tokens/extensions/scaled-ui-amount/integration-guide).
  A near-1 multiplier is not sufficient evidence to ignore this issue.
- Drift-based lending/trading basket: excluded after the official
  [June 2026 recovery update](https://www.drift.trade/updates/drift-recovery-update-june-3-2026)
  described recovery following the April exploit.

## $1 initialization and deployment

Each full index token starts with component atoms calculated as
`round(weight_bps * 10^decimals / (10000 * oracle_usd_price))` using integer
decimal arithmetic. The aggregate rounding error must be at most $0.00001.
MAJR explicitly allocates 5% to native USDC as a rounding reserve: after sizing
the other components, its USDC atoms are the nearest micro-dollar to the remaining
NAV. The adjustment may not exceed $0.0005 (5 basis points of starting NAV).
This accommodates cbBTC's relatively valuable atomic unit without relaxing the
aggregate $1 tolerance or changing any previously created basket.
This targets $1 at the creation price snapshot; token prices subsequently move.
It does not peg NAV, buy initial backing, or guarantee a $1 first mint if that
mint happens later. Supply starts at zero; the first mint deposits the configured
component quantities, and subsequent mints/redeems use pro-rata backing.

Fixed-weight baskets use 30-day / 500-bps absolute component drift triggers.
RWAX's held definition retains a 90-day interval. A keeper must operate the
deployed program's fixed-weight rebalance flow; configuring an interval does not run a keeper.
Fixed-unit baskets store zero target-weight bps and no rebalance settings.

The deployed mainnet program differs from this repository's newer source/IDL:
simulation confirms `create_large_basket_index` rejects 1–8 components and accepts
9–10, while `create_index` accepts SOLB's six components. The runner therefore
pins a minimal ABI copied from the existing app's
`public/generated/idl/omnindex.json` in `scripts/idl/catalog-mainnet.json`.
It uses `create_index` / `initialize_vaults`, with classic SPL components only.
It does not deploy the repository's pending program rewrite. The current
rebalance-bot source targets that newer rewrite; do not assume it can run against
the older mainnet ABI without adaptation.

The first paged-creation simulation failed before any index was created. Its
SOLB lookup table is reused by the compatible small-index deployment. The runner
uses a modest priority fee and retries an expired transaction only after checking
that its signature did not land. Confirmed index/vault/metadata creation is atomic.

`npm run catalog:check` checks mainnet, creator authority, mint owners/decimals,
active extensions, prices, at least $10,000 reported token liquidity, live $10
two-way routes with at most 3% round-trip loss, and Switchboard feed simulations.
Jupiter trade prices may be up to 9,000 slots old but must agree with the live
route midpoint within 2%; oracle/Jupiter disagreement above 1% blocks creation.
Feed jobs try Jupiter, then a pinned liquid/traded DexScreener base-token pair
whose setup price agrees with Jupiter within 1%. No constant fallback is used.
These checks are a launch snapshot, not continuous liquidity monitoring.

`npm run catalog:deploy` sends the eligible batch after checking available SOL.
Use `-- --symbols=SOLB,MEME` to select a subset. Catalog-held baskets cannot be
selected for deployment. Index/vault/metadata instructions are submitted atomically
with preflight and confirmed results; post-deployment state is verified. `deployment.json` records
transactions, mint/index addresses, component atoms, oracle IDs and lookup tables.
Existing indexes are never silently overwritten; interrupted creation requires
inspection of this journal and the chain before a resume.

`npm run catalog:verify` independently reads each journaled index, mint, component
vault, metadata account and lookup table, checks the recorded initial quantities,
and writes `DEPLOYMENT.md`. Keep `SOLANA_RPC_URL` pointed at a mainnet endpoint.

The runner creates Metaplex name/symbol metadata with an empty optional URI;
no nonexistent hosted JSON/image URL is advertised. Fees initialize to the
protocol's default zero. It does not upgrade the program or change creation
permissions. Permanent delegates, freeze authorities and mutable Token-2022
settings remain issuer risks, even where current transfers work.

## Three additional researched baskets (September 2026 expansion)

These additions use fixed units and therefore dynamic market weights, with no
scheduled rebalance keeper requirement. Initial percentages below are allocations
at the creation price snapshot, not permanent targets. All start at $1 and hold
unstaked component tokens. `token-discovery-expansion.json` records the candidate
search snapshot; `deployment.json` records the actual creation prices and atoms.

| Symbol | Basket | Initial allocation | Selection and weight rationale |
| --- | --- | --- | --- |
| MAJR | Crypto Majors Reserve | cbBTC 45%, Portal ETH 30%, SOL 20%, USDC 5% | Adds Bitcoin/Ether exposure to the existing Solana-heavy catalog. BTC receives the largest allocation; the small cash reserve absorbs initial atomic rounding. Fixed units avoid recurring turnover. |
| PHYS | Wireless and Location | HNT 60%, GEOD 40% | Physical wireless connectivity and precision positioning, separate from the existing compute/data AI basket. More weight to Helium, with meaningful location-network exposure. Fixed units avoid regular trading in smaller pools. |
| DATA | Cross-chain Data Infrastructure | PYTH 60%, W 40% | Financial market data and cross-chain messaging protocol exposure. Pyth receives the larger allocation; Wormhole adds interoperability. Fixed units avoid recurring governance-token turnover. |

These are discretionary thematic weights, not market-cap weights or a forecast
of returns. PHYS and DATA are intentionally concentrated two-token baskets.

### Primary research and mint identity

- [Coinbase's wrapped-assets page](https://www.coinbase.com/cbbtc) identifies the
  Solana cbBTC mint `cbbtcf3aa214zXHbiAZQwf4122FBYbraNdFqgw4iMij` and describes
  one-for-one Bitcoin custody backing. MAJR inherits Coinbase custody, freeze,
  and redemption-access risks; it does not hold native Bitcoin.
- [Wormhole Foundation's Solana token registry](https://github.com/wormhole-foundation/wormhole-token-list/blob/main/content/dest_solana.md)
  identifies Ether (Portal), mint `7vfCXTUXx5WJV5JADk17DUJ4ksgau7utNKj4b963voxs`,
  eight decimals, originating from Ethereum WETH. MAJR holds that bridge
  representation and inherits Wormhole bridge/Guardian risks.
- [Helium HNT documentation](https://docs.helium.com/tokens/hnt-token/) identifies
  the exact Solana HNT mint and explains hotspot rewards and burning HNT into
  Data Credits used for wireless transmissions. Holding HNT in PHYS does not
  operate a hotspot or earn those hardware rewards.
- [GEODNET token documentation](https://docs.geodnet.com/geod-token/geod-token-introduction)
  identifies the exact Solana GEOD mint, NTT multichain support, base-station
  rewards and payment for RTK positioning data. PHYS holds tokens without
  operating stations; GEOD retains multichain and project concentration risks.
- [Pyth's token documentation](https://docs.pyth.network/pyth-token) describes
  governance and staking utility. DATA does not stake PYTH or receive staking
  rewards. Its exposure is to the token, not a claim on oracle-service income.
- [Wormhole's multichain launch](https://wormhole.com/blog/w-is-now-natively-multichain-on-ethereum-and-layer-2s)
  confirms native Solana SPL W and NTT support;
  [its staking documentation](https://wormhole.com/blog/w-staking-rewards-program)
  explains that rewards require staking. W's exact mint is corroborated through
  Jupiter's verified token registry and on-chain mint validation. DATA leaves W
  unstaked. Governance-token issuance/unlocks and adoption affect both components.

### Screening and initialization checks

Discovery reported approximately $27.79m cbBTC liquidity, $21.47m Portal ETH,
$296.7k HNT, $319.1k GEOD and $122.3k W. These are reported token liquidity
snapshots, not guaranteed executable depth. Every selected asset subsequently
passed the runner's mainnet mint/decimal checks, $10 two-way route check and live
Switchboard/Jupiter/route-price agreement checks in the pre-deployment dry run.

Gaming candidates were not selected: GMT had only about $2.2k reported Solana
liquidity, while ATLAS/POLIS had about $32.7k/$15.3k. The
[Star Atlas token documentation](https://build.staratlas.com/dev-resources/apis-and-data/galaxy-api/tokens)
and [DAO description](https://experience.staratlas.com/newsroom/star-atlas-news/the-star-atlas-dao-the-game-of-polis)
also show ATLAS/POLIS are two tokens from a single game ecosystem, offering less
project diversification than the selected additions. NOS was screened but adds
compute exposure already represented in AISP.

Tests cover high-price BTC rounding in both directions, the reserve's maximum
adjustment and exact asset/valuation requirements, in addition to the existing
catalog, duplicate mint, overflow and $1 sizing checks.
