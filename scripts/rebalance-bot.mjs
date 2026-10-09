// Fixed-weight rebalance worker.
//
// Detects when a FixedWeights basket has drifted past its configured threshold (or its
// time interval has elapsed) and drives the on-chain rebalance intent state machine:
//   open_rebalance_intent
//     -> execute_rebalance_sell_batch ×N   (component -> USDC, Jupiter, ExactIn)
//     -> execute_rebalance_buy_batch  ×N   (USDC -> component, Jupiter, ExactOut)
//     -> finalize_rebalance
//     -> close_rebalance_intent             (reclaim the intent rent)
// and, when it finds a stuck/expired intent, unwind_rebalance to release the lock.
//
// SAFETY: dry-run by default. It reads chain state, fetches prices/quotes, builds the
// full plan (and, with --preview-swaps, the real Jupiter swap encodings) and prints it,
// but sends NOTHING unless you pass --execute. Sending real transactions spends the
// keeper's SOL (transaction fees + rent) and moves basket assets.
//
// To avoid stranding an open intent, the --execute path builds and VALIDATES every swap
// batch (tx size + Jupiter route account scope) BEFORE sending any execute tx; if a leg
// can't be built/sized/scoped it cancels the just-opened intent (no legs executed yet)
// instead of getting stuck mid-rebalance.
//
// Prices: open, each swap batch and finalize read prices the oracle key signed for this
// rebalance intent (scripts/lib/signed-prices.mjs). Right before each of them the tokens
// involved are priced (scripts/lib/price-oracle.mjs) and signed, and the step is sent with the
// Ed25519 instruction carrying the signature immediately before it. With ORACLE_URL set, the
// oracle service (scripts/oracle-service.mjs, its own Fly app) prices and signs, and the keeper
// never holds the oracle key. Without it the bot signs with a local oracle key (development
// only). Either way the oracle key is separate from the keeper's.
//
//   node scripts/rebalance-bot.mjs                 # detect-only, scan all FixedWeights baskets
//   node scripts/rebalance-bot.mjs --preview-swaps # dry-run + encode the Jupiter swaps read-only
//   node scripts/rebalance-bot.mjs --show-prices   # dry-run + compute the prices it would post
//   node scripts/rebalance-bot.mjs --index <pk>    # only this index (skip discovery)
//   node scripts/rebalance-bot.mjs --execute       # actually rebalance triggered baskets
//   node scripts/rebalance-bot.mjs --watch         # loop forever (poll every --interval s)
//
// Env: SOLANA_RPC_URL (default mainnet-beta; discovery needs a getProgramAccounts-capable RPC),
//      KEEPER_KEYPAIR (secret key JSON array; takes precedence over ANCHOR_WALLET),
//      ANCHOR_WALLET (default deployer-keypair.json),
//      ORACLE_URL + ORACLE_API_TOKEN (the oracle service; ORACLE_KEYPAIR must then be unset),
//      ORACLE_KEYPAIR (secret key JSON array; takes precedence over ORACLE_WALLET),
//      ORACLE_WALLET (default oracle-keypair.json), JUPITER_API_KEY (optional: Jupiter's keyed
//      API, api.jup.ag, instead of the keyless one), JUPITER_SWAP_API, JUPITER_PRICE_API.
//
// In --watch mode SIGTERM/SIGINT stop the loop after the index being processed, so a
// restart doesn't abandon a rebalance mid-flight (see fly.toml kill_timeout).
//
// NOTE: build the program (anchor build, regenerates target/idl/basket.json) and deploy it
// before the --execute path works against a real basket.

import { OraclePriceError, PRICE_SCALE, fetchJson, jupiterApiKey, jupiterApis, oraclePrices } from "./lib/price-oracle.mjs";
import {
  MAX_PRICE_AGE_SLOTS,
  decodePriceMessage,
  encodePriceMessage,
  placeholderPriceInstruction,
  priceOracleAddress,
  priceSignatureInstruction,
  signPriceMessage,
  verifyPriceSignature,
} from "./lib/signed-prices.mjs";
import anchor from "@coral-xyz/anchor";
import {
  AddressLookupTableAccount,
  AddressLookupTableProgram,
  ComputeBudgetProgram,
  Connection,
  Keypair,
  PublicKey,
  SystemProgram,
  SYSVAR_CLOCK_PUBKEY,
  SYSVAR_INSTRUCTIONS_PUBKEY,
  TransactionMessage,
  VersionedTransaction,
} from "@solana/web3.js";
import {
  ASSOCIATED_TOKEN_PROGRAM_ID,
  TOKEN_2022_PROGRAM_ID,
  TOKEN_PROGRAM_ID,
  createAssociatedTokenAccountIdempotentInstruction,
  createInitializeAccount3Instruction,
  getAccountLenForMint,
  getAssociatedTokenAddressSync,
  getMint,
} from "@solana/spl-token";
import fs from "fs";
import path from "path";
import { pathToFileURL } from "node:url";

// --- constants ---------------------------------------------------------------

const PROGRAM_ID = new PublicKey("bskthjNMRWQ4ekDLxaAzA1e39ThPmEtUgHY3XHfs7qv");
const USDC_MINT = new PublicKey("EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v");
const JUPITER_V6 = new PublicKey("JUP6LkbZbjS1jKKwapdHNy74zcZ3tLUZoi5QNyVTaV4");

const VAULT_AUTHORITY_SEED = Buffer.from("vault-authority");
const REBALANCE_INTENT_SEED = Buffer.from("rebalance-intent");
const PAGE_SEED = Buffer.from("large-basket-component-page");
const COMPOSITION_CHANGE_SEED = Buffer.from("composition-change");
const COMPONENTS_PER_PAGE = 10;

const BPS = 10_000;
const TX_LIMIT = 1232; // Solana packet MTU; a compiled tx must not exceed this.
// Accounts one transaction may lock on mainnet (the feature raising it to 128 is inactive).
const MAX_TX_ACCOUNTS = 64;
// Compute for open and finalize (unwind takes finalize's), which price and load every
// component, and for creating or extending a lookup table.
const OPEN_CU_LIMIT = 400_000;
const FINALIZE_CU_LIMIT = 600_000;
const LOOKUP_TABLE_CU_LIMIT = 50_000;
// Address lookup tables: the meta layout puts the authority at byte 22 (u32 type, u64
// deactivation slot, u64 last extended slot, u8 start index, u8 authority option tag); a table
// holds at most 256 addresses, and one extend transaction carries this many.
const LOOKUP_TABLE_AUTHORITY_OFFSET = 22;
const LOOKUP_TABLE_MAX_ADDRESSES = 256;
const LOOKUP_TABLE_EXTEND_CHUNK = 20;
const LOOKUP_TABLE_META_BYTES = 56;
const USDC_DECIMALS = 6;
// Mirror of the program's constants (programs/basket/src/constants.rs); MAX_PRICE_AGE_SLOTS
// comes from scripts/lib/signed-prices.mjs.
const MAX_REBALANCE_SWAPS_PER_BATCH = 4;
const MIN_KEEPER_NAV_TOLERANCE_BPS = 10;
const MAX_KEEPER_NAV_TOLERANCE_BPS = 100;
const MIN_FIXED_WEIGHT_POST_REBALANCE_DRIFT_BPS = 25;
const MAX_FIXED_WEIGHT_POST_REBALANCE_DRIFT_BPS = 500;
const MAX_FIXED_WEIGHT_EXECUTION_SLIPPAGE_BPS = 500;
const MAX_INTENT_TTL_SECONDS = 1800;
// `kind` byte offset in IndexState (8 disc + 5*32 pubkeys + 4 small fields). FixedWeights = 1.
const INDEX_KIND_OFFSET = 172;
const INDEX_KIND_FIXED_WEIGHTS = 1;

const TOKEN_PROGRAM_IDS = new Set([TOKEN_PROGRAM_ID.toBase58(), TOKEN_2022_PROGRAM_ID.toBase58()]);

// --- tunables (env / flags) --------------------------------------------------

const argv = process.argv.slice(2);
const flags = new Set(argv.filter((a) => a.startsWith("--")));
function flagValue(name, fallback) {
  const idx = argv.indexOf(name);
  return idx >= 0 && idx + 1 < argv.length ? argv[idx + 1] : fallback;
}

const EXECUTE = flags.has("--execute");
// Dry-run only: build the real Jupiter swaps for the (estimated) legs and report the
// compact-plan encoding + would-be tx sizes, without sending. Validates the route
// encoding before you ever spend funds.
const PREVIEW_SWAPS = flags.has("--preview-swaps");
// Dry-run only: compute the oracle prices each basket's rebalance would have signed.
const SHOW_PRICES = flags.has("--show-prices");
const WATCH = flags.has("--watch");
const WATCH_INTERVAL_S = Number(flagValue("--interval", "60"));
const ONLY_INDEX = flagValue("--index", null);
const BATCH_SIZE = Math.max(
  1,
  Math.min(MAX_REBALANCE_SWAPS_PER_BATCH, Number(flagValue("--batch-size", "2"))),
);
const SLIPPAGE_BPS = Number(flagValue("--slippage-bps", "100"));
// Each swap atomically bounds the realized fill vs a FRESH oracle. That budget must
// cover the execution slippage already allowed PLUS the Jupiter-mid-vs-oracle basis and any
// intra-window oracle drift — so it is execution slippage + a buffer, NOT just the slippage.
const VERIFY_ORACLE_BUFFER_BPS = Number(flagValue("--verify-buffer-bps", "300"));
// Oracle pricing (scripts/lib/price-oracle.mjs): USDC size of the round trip that prices a
// token, the widest round-trip spread priced, and how close a reference must be to confirm.
const ORACLE_CONFIG = {
  probeUsd: Number(flagValue("--price-probe-usd", "50")),
  maxSpreadBps: Number(flagValue("--price-max-spread-bps", "400")),
  maxDeviationBps: Number(flagValue("--price-max-deviation-bps", "300")),
};
const PRIORITY_FEE_MICROLAMPORTS = Number(flagValue("--priority-fee", "5000"));
const NAV_TOLERANCE_BPS = Math.max(
  MIN_KEEPER_NAV_TOLERANCE_BPS,
  Math.min(MAX_KEEPER_NAV_TOLERANCE_BPS, Number(flagValue("--nav-tolerance-bps", "50"))),
);
// Open only when Jupiter-estimated drift exceeds the threshold by this margin, reducing
// opens the program then rejects (it re-decides from the signed prices, which can straddle).
const DRIFT_TRIGGER_MARGIN_BPS = Number(flagValue("--drift-margin-bps", "0"));
// Keep the intent TTL safely below the program's hard 1800s cap so wall-clock-vs-chain skew
// can't push expires_at over the cap and revert the open.
const TTL_SKEW_MARGIN_S = 90;
const INTENT_TTL_S = Math.max(
  60,
  Math.min(MAX_INTENT_TTL_SECONDS - TTL_SKEW_MARGIN_S, Number(flagValue("--ttl", "600"))),
);
const REBROADCAST_INTERVAL_MS = 2_000;
// After a failed rebalance attempt, leave that basket alone this long so a persistent failure
// can't drain the keeper's SOL on every poll.
const FAILURE_COOLDOWN_S = Number(flagValue("--failure-cooldown", "1800"));
// Mints and redeems run concurrently, so a rebalance first asks the program to hold back new
// ones (request_rebalance) and waits for the open ones to settle. The hold lapses on chain after
// REBALANCE_REQUEST_WINDOW_SECONDS; if it lapses without a rebalance, wait this long before
// holding users back again.
const REQUEST_WINDOW_S = 40 * 60;
const PROGRAM_REQUEST_COOLDOWN_S = 20 * 60; // REBALANCE_REQUEST_COOLDOWN_SECONDS
const REQUEST_COOLDOWN_S = Number(flagValue("--request-cooldown", "7200"));

const RPC_URL = process.env.SOLANA_RPC_URL ?? "https://api.mainnet-beta.solana.com";
const WALLET_PATH = process.env.ANCHOR_WALLET ?? "deployer-keypair.json";
const ORACLE_WALLET_PATH = process.env.ORACLE_WALLET ?? "oracle-keypair.json";
// The oracle service (scripts/oracle-service.mjs). When set, prices are signed by it, not here.
const ORACLE_URL = process.env.ORACLE_URL || null;
const ORACLE_API_TOKEN = process.env.ORACLE_API_TOKEN ?? "";
// Pricing a basket can take a while when price sources are slow.
const ORACLE_REQUEST_TIMEOUT_MS = 120_000;
const { swapApi: JUPITER_SWAP_API, priceApi: JUPITER_PRICE_API } = jupiterApis();

// --- small utilities ---------------------------------------------------------

function readJson(filePath) {
  return JSON.parse(fs.readFileSync(filePath, "utf8"));
}

function log(...args) {
  console.log(...args);
}

function bpsPct(bps) {
  return `${(Number(bps) / 100).toFixed(2)}%`;
}

class RebalanceBuildError extends Error {}

// SPL token account: amount = u64 LE at offset 64, owner = pubkey at offset 32.
function parseTokenAmount(accountInfo) {
  if (!accountInfo || accountInfo.data.length < 72) return 0n;
  return accountInfo.data.readBigUInt64LE(64);
}
function parseTokenOwner(accountInfo) {
  if (!accountInfo || accountInfo.data.length < 64) return null;
  return new PublicKey(accountInfo.data.subarray(32, 64));
}

function bitmapGet(bytes, index) {
  const byte = bytes[index >> 3] ?? 0;
  return (byte & (1 << (index & 7))) !== 0;
}

function pow10Big(decimals) {
  return 10n ** BigInt(decimals);
}

// The signed prices aged out before the step landed (or the oracle's RPC was ahead of the
// leader's slot): re-sign and retry. Preflight failures name the error in their logs; a landed
// failure only carries its code.
const RESIGN_ERRORS = ["StaleOraclePrice", "SignedPriceSlotInFuture"];
function isStalePriceError(program, error) {
  const codes = (program.idl.errors ?? [])
    .filter((e) => RESIGN_ERRORS.some((name) => name.toLowerCase() === e.name.toLowerCase()))
    .map((e) => e.code);
  const text = String(error?.message ?? error) + (error?.logs ?? []).join("\n");
  return new RegExp(RESIGN_ERRORS.join("|"), "i").test(text)
    || codes.some((code) => text.includes(`"Custom":${code}`) || text.includes(`custom program error: 0x${code.toString(16)}`));
}

// The blockhash expired before the tx landed, so it never can: safe to rebuild and resend.
function isExpiredError(error) {
  return /block height exceeded|has expired/i.test(String(error?.message ?? error));
}

// --- chain context -----------------------------------------------------------

function loadKeypair(filePath) {
  const secret = process.env.KEEPER_KEYPAIR ? JSON.parse(process.env.KEEPER_KEYPAIR) : readJson(filePath);
  return Keypair.fromSecretKey(Uint8Array.from(secret));
}

// The key that signs rebalance prices locally (development); without it the bot can still dry-run.
function loadOracleKeypair(filePath) {
  if (process.env.ORACLE_KEYPAIR) return Keypair.fromSecretKey(Uint8Array.from(JSON.parse(process.env.ORACLE_KEYPAIR)));
  return fs.existsSync(filePath) ? Keypair.fromSecretKey(Uint8Array.from(readJson(filePath))) : null;
}

function loadProgram(connection, payer) {
  const wallet = new anchor.Wallet(payer);
  const provider = new anchor.AnchorProvider(connection, wallet, {
    commitment: "confirmed",
    preflightCommitment: "confirmed",
  });
  anchor.setProvider(provider);
  const idl = readJson(path.join(process.cwd(), "target", "idl", "basket.json"));
  return new anchor.Program(idl, provider);
}

async function chainNow(connection) {
  const info = await connection.getAccountInfo(SYSVAR_CLOCK_PUBKEY, "confirmed");
  if (!info) throw new Error("could not read Clock sysvar");
  return Number(info.data.readBigInt64LE(32)); // unix_timestamp
}

function vaultAuthorityPda(index) {
  return PublicKey.findProgramAddressSync([VAULT_AUTHORITY_SEED, index.toBuffer()], PROGRAM_ID)[0];
}
function pagePda(index, pageIndex) {
  return PublicKey.findProgramAddressSync(
    [PAGE_SEED, index.toBuffer(), Buffer.from([pageIndex])],
    PROGRAM_ID,
  )[0];
}
function compositionChangePda(index) {
  return PublicKey.findProgramAddressSync([COMPOSITION_CHANGE_SEED, index.toBuffer()], PROGRAM_ID)[0];
}
const PRICE_ORACLE = priceOracleAddress(PROGRAM_ID);
function intentPda(index, nonceBn) {
  return PublicKey.findProgramAddressSync(
    [REBALANCE_INTENT_SEED, index.toBuffer(), nonceBn.toArrayLike(Buffer, "le", 8)],
    PROGRAM_ID,
  )[0];
}
function enumKey(value) {
  return value ? Object.keys(value)[0] : null;
}

async function discoverFixedWeightsIndexes(program) {
  const accounts = await program.account.indexState.all([
    {
      memcmp: {
        offset: INDEX_KIND_OFFSET,
        bytes: anchor.utils.bytes.bs58.encode(Buffer.from([INDEX_KIND_FIXED_WEIGHTS])),
      },
    },
  ]);
  return accounts.map((a) => a.publicKey);
}

async function loadComponents(program, index, pageCount) {
  const pagePdas = [];
  for (let i = 0; i < pageCount; i += 1) pagePdas.push(pagePda(index, i));
  const pages = await Promise.all(
    pagePdas.map((p) => program.account.largeBasketComponentPage.fetch(p)),
  );
  const ordered = pages
    .map((page, i) => ({ page, pda: pagePdas[i] }))
    .sort((a, b) => a.page.startComponentIndex - b.page.startComponentIndex);
  const components = [];
  for (const { page, pda } of ordered) {
    for (const c of page.components) {
      const isQuote = c.mint.equals(USDC_MINT);
      components.push({
        globalIndex: components.length,
        mint: c.mint,
        unitsPerIndex: BigInt(c.unitsPerIndex.toString()),
        targetWeightBps: c.targetWeightBps,
        tokenProgram: c.tokenProgram,
        vault: c.vault,
        decimals: c.decimals,
        pagePda: pda,
        pageIndex: page.pageIndex,
        isQuote,
        // Removed by a composition change and already sold. The program neither prices nor
        // trades it (judged by its accounting, so dust sent to the vault is ignored).
        retired: !isQuote && c.targetWeightBps === 0 && c.accountedReserve.isZero(),
      });
    }
  }
  return { components, pagePdas: ordered.map((o) => o.pda) };
}

async function fetchPrices(mints) {
  if (!mints.length) return new Map();
  const ids = mints.map((m) => m.toBase58()).join(",");
  const payload = await fetchJson(`${JUPITER_PRICE_API}?ids=${encodeURIComponent(ids)}`, {
    headers: { accept: "application/json" },
  });
  const prices = new Map();
  for (const m of mints) {
    const usd = Number(payload?.[m.toBase58()]?.usdPrice);
    if (Number.isFinite(usd) && usd > 0) prices.set(m.toBase58(), usd);
  }
  return prices;
}

// --- detection ---------------------------------------------------------------

// Returns { nav, rows, maxDriftBps, driftKnown, missing }. Leg sizes are computed in
// integer (BigInt) space so the dry-run/preview matches the program's stored u64 legs.
// Tolerates a missing Jupiter price: that component is excluded from the drift estimate
// (driftKnown=false) rather than aborting — the program prices from the signed prices anyway.
function assessBasket(components, vaultAmounts, scratchAtoms, prices) {
  const hasQuoteComponent = components.some((c) => c.isQuote);
  const missing = components.filter((c) => !c.isQuote && !c.retired && !prices.get(c.mint.toBase58()));
  const driftKnown = missing.length === 0;

  // NAV in micro-USD (integer) using available prices; missing components contribute 0.
  const microUsd = (atoms, decimals, usd) =>
    (BigInt(atoms) * BigInt(Math.round(usd * 1e6))) / pow10Big(decimals);
  let navMicro = 0n;
  const priced = components.map((c) => {
    const amount = vaultAmounts.get(c.vault.toBase58()) ?? 0n;
    const usd = c.isQuote ? 1 : c.retired ? undefined : prices.get(c.mint.toBase58());
    const valueMicro = usd ? microUsd(amount, c.decimals, usd) : 0n;
    navMicro += valueMicro;
    return { c, amount, usd, valueMicro };
  });
  if (!hasQuoteComponent) navMicro += BigInt(scratchAtoms); // USDC = 1 micro-USD per atom
  const nav = Number(navMicro) / 1e6;

  let maxDriftBps = 0;
  const rows = priced.map(({ c, amount, usd, valueMicro }) => {
    const weightBps = navMicro > 0n ? Number((valueMicro * BigInt(BPS)) / navMicro) : 0;
    const driftBps = c.isQuote || !usd ? 0 : Math.abs(weightBps - c.targetWeightBps);
    if (!c.isQuote && usd && driftBps > maxDriftBps) maxDriftBps = driftBps;
    let leg = 0n;
    let side = "none";
    if (!c.isQuote && usd) {
      // target backing atoms = nav * targetWeight / price, in component units (BigInt).
      const targetMicro = (navMicro * BigInt(c.targetWeightBps)) / BigInt(BPS);
      const targetAtoms =
        (targetMicro * pow10Big(c.decimals)) / BigInt(Math.round(usd * 1e6));
      const diff = amount > targetAtoms ? amount - targetAtoms : targetAtoms - amount;
      const isDust = diff * BigInt(BPS) < (targetAtoms === 0n ? 1n : targetAtoms);
      if (!isDust && amount > targetAtoms) {
        leg = amount - targetAtoms;
        side = "sell";
      } else if (!isDust && targetAtoms > amount) {
        leg = targetAtoms - amount;
        side = "buy";
      }
    }
    return { component: c, amount, usd, weightBps, driftBps, leg, side };
  });
  return { nav, rows, maxDriftBps, driftKnown, missing };
}

// --- oracle prices -------------------------------------------------------------

// The tokens a step prices: non-USDC components the program has not retired, once each.
function pricedTokens(components) {
  const seen = new Map();
  for (const c of components) {
    if (c.isQuote || c.retired || seen.has(c.mint.toBase58())) continue;
    seen.set(c.mint.toBase58(), { mint: c.mint.toBase58(), decimals: c.decimals, label: c.mint.toBase58().slice(0, 6) });
  }
  return [...seen.values()];
}

function computeOraclePrices(tokens) {
  return oraclePrices(tokens, { ...ORACLE_CONFIG, swapApi: JUPITER_SWAP_API, priceApi: JUPITER_PRICE_API, fetchJson });
}

// Price sources blip and rate-limit; mid-rebalance, giving up leaves the intent to expire and
// unwind, so keep trying well within the intent's lifetime.
const PRICING_ATTEMPTS = 4;
const PRICING_BACKOFF_MS = 15_000;
async function patiently(label, attemptFn, retryable = () => true) {
  for (let attempt = 1; ; attempt += 1) {
    try {
      return await attemptFn();
    } catch (error) {
      if (attempt >= PRICING_ATTEMPTS || !retryable(error)) throw error;
      log(`    [prices] ${label}: ${error.message ?? error}; retrying in ${(attempt * PRICING_BACKOFF_MS) / 1000}s`);
      await new Promise((r) => setTimeout(r, attempt * PRICING_BACKOFF_MS));
    }
  }
}

// Asks the oracle service to price `mints` of basket `index` and, unless dryRun, sign them for
// the rebalance intent with that nonce. It prices them and maps them to component indexes
// itself; this only picks which and when. Returns { prices: Map(mint -> { scaled, usd,
// spreadBps, confirmedBy }), blockers, intent, oracle, and unless dryRun slot, message and
// signature }. Throws with `.status` set to the service's HTTP status (undefined if it could
// not be reached).
async function requestSignedPrices(oracleUrl, token, { index, nonce, mints, dryRun = false }) {
  let res;
  try {
    res = await fetch(new URL("/sign", oracleUrl), {
      method: "POST",
      headers: { "content-type": "application/json", authorization: `Bearer ${token}` },
      body: JSON.stringify({ index: index.toBase58(), nonce: nonce.toString(), mints, dryRun }),
      signal: AbortSignal.timeout(ORACLE_REQUEST_TIMEOUT_MS),
    });
  } catch (error) {
    throw new Error(`oracle service unreachable at ${oracleUrl}: ${error.message ?? error}`);
  }
  const body = await res.json().catch(() => ({}));
  if (!res.ok || !body.ok) {
    const error = new Error(`oracle service: ${body.error ?? `HTTP ${res.status}`}`);
    error.status = res.status;
    throw error;
  }
  const prices = new Map(Object.entries(body.prices).map(([mint, p]) => [mint, { ...p, scaled: BigInt(p.scaled) }]));
  return {
    prices,
    blockers: body.blockers ?? [],
    intent: new PublicKey(body.intent),
    oracle: new PublicKey(body.oracle),
    slot: body.slot,
    message: body.message ? Buffer.from(body.message, "base64") : undefined,
    signature: body.signature ? Buffer.from(body.signature, "base64") : undefined,
  };
}

// The service is down or a price source blipped: worth retrying. Anything else it refused
// (auth, unknown mint, a price that moved too far, a key the program doesn't accept) won't
// change by retrying soon.
function isRetryableOracleError(error) {
  return error.status === undefined || error.status >= 500;
}

// Mint decimals for the tokens a due composition change adds, so they can be priced up front.
async function additionTokens(connection, change) {
  if (!change?.additions.length) return [];
  const infos = await connection.getMultipleAccountsInfo(change.additions.map((x) => x.mint), "confirmed");
  return change.additions.map((x, i) => {
    if (!infos[i] || infos[i].data.length < 45) throw new Error(`composition change adds ${x.mint.toBase58()}, which is not a token mint`);
    return { mint: x.mint.toBase58(), decimals: infos[i].data[44], label: x.mint.toBase58().slice(0, 6) };
  });
}

// The program checks every swap's fill against the signed price ± the execution bound. Check
// each built leg at its worst allowed fill (Jupiter's slippage threshold) the same way, before
// any swap runs, so a leg that could never pass cancels the intent cleanly instead of failing
// after other legs have traded.
function assertLegsWithinOracle(entries, prices, side) {
  const bound = BigInt(Math.min(MAX_FIXED_WEIGHT_EXECUTION_SLIPPAGE_BPS, SLIPPAGE_BPS + VERIFY_ORACLE_BUFFER_BPS));
  for (const b of entries) {
    const c = b.component;
    const signed = prices.get(c.mint.toBase58());
    if (!signed) throw new RebalanceBuildError(`no signed price for component ${c.globalIndex}`);
    const worst = (BigInt(b.entry.quoteLimit.toString()) * pow10Big(c.decimals) * PRICE_SCALE) / (BigInt(b.atoms) * pow10Big(USDC_DECIMALS));
    const ok = side === "sell"
      ? worst * BigInt(BPS) >= signed.scaled * (BigInt(BPS) - bound)
      : worst * BigInt(BPS) <= signed.scaled * (BigInt(BPS) + bound);
    if (!ok) {
      const usd = (x) => (Number(x) / Number(PRICE_SCALE)).toPrecision(6);
      throw new RebalanceBuildError(
        `component ${c.globalIndex} ${side} could fill at $${usd(worst)}, outside ${bpsPct(Number(bound))} of the signed $${usd(signed.scaled)}`,
      );
    }
  }
}

// Checks what the oracle service signed is what this step will send: this intent, a valid
// signature, and exactly the asked-for components at the prices it reported. Returns the signed
// prices by mint.
function checkSignedPrices(signed, ctx, components, tokens) {
  const decoded = decodePriceMessage(signed.message);
  if (!decoded.intent.equals(ctx.intent)) {
    throw new Error(`the oracle service signed prices for ${decoded.intent.toBase58()}, not intent ${ctx.intent.toBase58()}`);
  }
  if (!verifyPriceSignature(signed.oracle, signed.message, signed.signature)) {
    throw new Error("the oracle service's signature does not verify");
  }
  const byIndex = new Map(components.map((c) => [c.globalIndex, c.mint.toBase58()]));
  const prices = new Map();
  for (const { componentIndex, price } of decoded.entries) {
    const mint = byIndex.get(componentIndex);
    if (!mint || signed.prices.get(mint)?.scaled !== price) {
      throw new Error(`the oracle service signed component ${componentIndex} at a price it did not report for it`);
    }
    prices.set(mint, signed.prices.get(mint));
  }
  const missing = tokens.filter((t) => !prices.has(t.mint));
  if (missing.length) throw new Error(`the oracle service did not sign ${missing.map((t) => t.mint).join(", ")}`);
  return prices;
}

// Prices `components` for one step of the rebalance intent `ctx.intent` (basket `ctx.index`,
// nonce `ctx.nonce`) and has them signed. Returns the signed prices (mint -> { scaled, usd, ...
// }), the slot they are stamped with, and the Ed25519 instruction carrying the signature, which
// must go immediately before the step. With ORACLE_URL the oracle service prices and signs;
// otherwise the bot signs with its local oracle key (development only).
async function signPrices(env, ctx, components, label) {
  const tokens = pricedTokens(components);
  const describe = (prices) => [...prices].map(([mint, p]) => `${mint.slice(0, 6)}.. $${p.usd.toPrecision(6)}`).join(", ");
  if (env.oracleUrl) {
    const signed = await patiently(label, () => requestSignedPrices(env.oracleUrl, env.oracleToken, {
      index: ctx.index,
      nonce: ctx.nonce,
      mints: tokens.map((t) => t.mint),
    }), isRetryableOracleError);
    const prices = checkSignedPrices(signed, ctx, components, tokens);
    log(`    [prices] ${label}: ${describe(prices)} signed by the oracle service at slot ${signed.slot}`);
    return { prices, slot: signed.slot, ix: priceSignatureInstruction(signed) };
  }
  // Stamped with the slot read before pricing, as the oracle service does, so the program's age
  // limit bounds how old the prices are. Each try re-reads it.
  const { slot, prices } = await patiently(label, async () => {
    const slot = await env.connection.getSlot("confirmed");
    return { slot, prices: await (env.priceTokens ?? computeOraclePrices)(tokens) };
  });
  const message = encodePriceMessage({
    intent: ctx.intent,
    slot,
    entries: components
      .filter((c) => !c.isQuote && prices.has(c.mint.toBase58()))
      .map((c) => ({ componentIndex: c.globalIndex, price: prices.get(c.mint.toBase58()).scaled })),
  });
  const signature = signPriceMessage(env.oracle.secretKey, message);
  log(`    [prices] ${label}: ${describe(prices)} signed locally at slot ${slot}`);
  return { prices, slot, ix: priceSignatureInstruction({ oracle: env.oracle.publicKey, message, signature }) };
}

// The oracle stamps prices with a slot its own RPC had confirmed, and this keeper's preflight
// simulates at the slot its RPC has confirmed, where prices from a later slot are refused
// (SignedPriceSlotInFuture). Wait, briefly, until this keeper's RPC has reached the stamp; if it
// lags longer than that, send anyway and let the retry re-sign.
const SLOT_CATCH_UP_MS = 5_000;
async function waitForSlot(connection, slot, label, timeoutMs = SLOT_CATCH_UP_MS) {
  const deadline = Date.now() + timeoutMs;
  for (;;) {
    const current = await connection.getSlot("confirmed");
    if (current >= slot) return;
    if (Date.now() > deadline) {
      return void log(`    [warn] ${label}: this keeper's RPC is at slot ${current}, behind the prices' slot ${slot}; sending anyway`);
    }
    await new Promise((r) => setTimeout(r, 400));
  }
}

// --- transaction send --------------------------------------------------------

function budgetIxs(units) {
  return [
    ComputeBudgetProgram.setComputeUnitLimit({ units }),
    ComputeBudgetProgram.setComputeUnitPrice({ microLamports: PRIORITY_FEE_MICROLAMPORTS }),
  ];
}

// Bytes and accounts of the compiled transaction (bytes is Infinity if it cannot serialize).
function compiledFootprint(payer, instructions, lookupTables = []) {
  const message = new TransactionMessage({
    payerKey: payer.publicKey,
    recentBlockhash: PublicKey.default.toBase58(),
    instructions,
  }).compileToV0Message(lookupTables);
  const accounts = message.staticAccountKeys.length
    + message.addressTableLookups.reduce((n, l) => n + l.writableIndexes.length + l.readonlyIndexes.length, 0);
  try {
    return { bytes: new VersionedTransaction(message).serialize().length, accounts };
  } catch (error) {
    // web3.js uses fixed serialization buffers; an oversized candidate must split.
    if (error instanceof RangeError && /encoding overruns Uint8Array|offset is outside the bounds/i.test(error.message)) return { bytes: Infinity, accounts };
    throw error;
  }
}

function compiledSize(payer, instructions, lookupTables = []) {
  return compiledFootprint(payer, instructions, lookupTables).bytes;
}

function fitsOneTransaction({ bytes, accounts }) {
  return bytes <= TX_LIMIT && accounts <= MAX_TX_ACCOUNTS;
}

async function sendV0(connection, payer, instructions, label, lookupTables = [], signers = []) {
  const footprint = compiledFootprint(payer, instructions, lookupTables);
  const size = footprint.bytes;
  if (!fitsOneTransaction(footprint)) {
    throw new RebalanceBuildError(`${label}: compiled tx is ${size} bytes and ${footprint.accounts} accounts (limits ${TX_LIMIT} and ${MAX_TX_ACCOUNTS})`);
  }
  const latest = await connection.getLatestBlockhash("confirmed");
  const message = new TransactionMessage({
    payerKey: payer.publicKey,
    recentBlockhash: latest.blockhash,
    instructions,
  }).compileToV0Message(lookupTables);
  const tx = new VersionedTransaction(message);
  tx.sign([payer, ...signers]);
  log(`    [send] ${label} (${size} bytes)`);
  const sig = await connection.sendTransaction(tx, {
    skipPreflight: false,
    preflightCommitment: "confirmed",
    maxRetries: 5,
  });
  // RPCs drop transactions under load. Rebroadcast the same signed tx until it confirms or
  // its blockhash expires; it can never land twice.
  const rebroadcast = setInterval(() => {
    connection.sendTransaction(tx, { skipPreflight: true, maxRetries: 0 }).catch(() => {});
  }, REBROADCAST_INTERVAL_MS);
  let confirmation;
  try {
    confirmation = await connection.confirmTransaction(
      { signature: sig, blockhash: latest.blockhash, lastValidBlockHeight: latest.lastValidBlockHeight },
      "confirmed",
    );
  } finally {
    clearInterval(rebroadcast);
  }
  if (confirmation.value.err) {
    throw new Error(`${label}: transaction ${sig} failed: ${JSON.stringify(confirmation.value.err)}`);
  }
  log(`    [ok] ${label}: ${sig}`);
  return sig;
}

// Have fresh prices for `components` signed, then send the step that reads them as
// [budget, Ed25519 signature, step] once this keeper's RPC has reached the prices' slot,
// re-signing if the step reverts on prices that aged out of the 50-slot window before it landed
// (or were still ahead of its RPC), or expires without landing. Returns the signed prices.
async function sendWithFreshPrices(env, ctx, components, label, buildConsumerIx, cuLimit, attempts = 3, lookupTables = []) {
  const { connection, program, payer } = env;
  let lastError;
  for (let attempt = 1; attempt <= attempts; attempt += 1) {
    const { prices, slot, ix: priceIx } = await signPrices(env, ctx, components, label);
    await waitForSlot(connection, slot, label);
    const ix = await buildConsumerIx();
    try {
      await sendV0(connection, payer, [...budgetIxs(cuLimit), priceIx, ix], label, lookupTables);
      return prices;
    } catch (error) {
      lastError = error;
      if (attempt < attempts && (isStalePriceError(program, error) || isExpiredError(error))) {
        log(`    ${label} failed (${isExpiredError(error) ? "expired" : "prices aged out"}), re-signing and resending`);
        continue;
      }
      throw error;
    }
  }
  throw lastError;
}

// --- Jupiter swap-plan encoding ---------------------------------------------

// pubkey base58 -> first index in the program's per-entry candidate array (rebalance_candidates).
function candidateMap(ctx, component) {
  const order = [
    ctx.keeper, // 0
    ctx.index, // 1
    ctx.vaultAuthority, // 2
    USDC_MINT, // 3 quote_mint
    ctx.vaultQuote, // 4
    JUPITER_V6, // 5
    ASSOCIATED_TOKEN_PROGRAM_ID, // 6
    TOKEN_PROGRAM_ID, // 7 quote_token_program (USDC)
    component.pagePda, // 8
    component.mint, // 9
    component.vault, // 10
    component.tokenProgram, // 11
    ASSOCIATED_TOKEN_PROGRAM_ID, // 12 (dup of 6)
    TOKEN_PROGRAM_ID, // 13 (dup of 7)
    SystemProgram.programId, // 14
  ];
  const map = new Map();
  order.forEach((pk, i) => {
    const key = pk.toBase58();
    if (!map.has(key)) map.set(key, i);
  });
  return map;
}

// Map a Jupiter swap instruction to the compact LargeBasketSwapPlan + ordered route accounts.
function encodeSwapPlan(swapInstruction, ctx, component) {
  const map = candidateMap(ctx, component);
  const routeAccounts = [];
  const accountBytes = [];
  for (const meta of swapInstruction.accounts) {
    const key = meta.pubkey;
    let idx = map.get(key);
    if (idx === undefined) {
      idx = 15 + routeAccounts.length;
      if (idx > 63) {
        throw new RebalanceBuildError(
          `route for component ${component.globalIndex} needs >48 accounts; cannot pack`,
        );
      }
      routeAccounts.push({
        pubkey: new PublicKey(key),
        isSigner: false,
        isWritable: Boolean(meta.isWritable),
      });
      map.set(key, idx);
    }
    let byte = idx & 0x3f;
    if (meta.isSigner) byte |= 0x40;
    if (meta.isWritable) byte |= 0x80;
    accountBytes.push(byte);
  }
  return {
    plan: {
      instructionData: Buffer.from(swapInstruction.data, "base64"),
      accounts: Buffer.from(accountBytes),
    },
    routeAccounts,
  };
}

// Route sizes (accounts) a leg is quoted with: the widest first, then smaller routes when the
// leg's transaction, with its price signature, would not fit.
const ROUTE_MAX_ACCOUNTS = [16, 12, 8];
// Venues keeper swaps never route through: proprietary market makers and RFQ, whose fills
// depend on their own fresh quotes and can refuse a swap the program makes through Jupiter (an
// LSTY sell through Obsidian failed with its error 10 on 2026-10-09). Pools quote what they fill.
const EXCLUDED_DEXES = flagValue(
  "--exclude-dexes",
  "Obsidian,HumidiFi,SolFi,SolFi V2,TesseraV,ZeroFi,GoonFi V2,BisonFi,AlphaQ,WhaleStreet,Aquifer,Quantum,JupiterRfqV2",
).split(",").map((s) => s.trim()).filter(Boolean);

async function jupiterQuote({ inputMint, outputMint, amount, swapMode, maxAccounts = ROUTE_MAX_ACCOUNTS[0] }) {
  const url =
    `${JUPITER_SWAP_API}/quote?inputMint=${inputMint.toBase58()}` +
    `&outputMint=${outputMint.toBase58()}&amount=${amount.toString()}` +
    `&swapMode=${swapMode}&slippageBps=${SLIPPAGE_BPS}&onlyDirectRoutes=false` +
    `&restrictIntermediateTokens=true&maxAccounts=${maxAccounts}` +
    (EXCLUDED_DEXES.length ? `&excludeDexes=${encodeURIComponent(EXCLUDED_DEXES.join(","))}` : "");
  return fetchJson(url, { headers: { accept: "application/json" } });
}

async function jupiterSwapInstruction(quoteResponse, vaultAuthority) {
  const payload = await fetchJson(`${JUPITER_SWAP_API}/swap-instructions`, {
    method: "POST",
    headers: { "content-type": "application/json", accept: "application/json" },
    body: JSON.stringify({
      quoteResponse,
      userPublicKey: vaultAuthority.toBase58(),
      // Shared accounts route through Jupiter-owned intermediates, so (when honored) the only
      // vault-authority-owned token accounts are the input/output ATAs. We additionally
      // pre-screen the route below, since shared accounts are a request, not a guarantee.
      useSharedAccounts: true,
      wrapAndUnwrapSol: false,
      skipUserAccountsRpcCalls: true,
    }),
  });
  if (!payload.swapInstruction) {
    throw new RebalanceBuildError(
      `Jupiter returned no swapInstruction: ${JSON.stringify(payload).slice(0, 300)}`,
    );
  }
  return payload;
}

async function loadLookupTables(connection, addresses) {
  const tables = [];
  for (const addr of addresses ?? []) {
    const res = await connection.getAddressLookupTable(new PublicKey(addr), { commitment: "confirmed" });
    if (res.value) tables.push(res.value);
  }
  return tables;
}

// The program rejects any route that references a vault-authority-owned token account other
// than the declared source/dest (validate_vault_authority_token_account_scope). Pre-screen the
// route so such a leg fails at BUILD time (before the intent is opened) rather than reverting
// the execute batch mid-rebalance.
async function assertRouteScope(connection, routeAccounts, vaultAuthority, allowed) {
  if (!routeAccounts.length) return;
  const infos = await connection.getMultipleAccountsInfo(
    routeAccounts.map((r) => r.pubkey),
    "confirmed",
  );
  routeAccounts.forEach((r, i) => {
    const info = infos[i];
    if (!info || !TOKEN_PROGRAM_IDS.has(info.owner.toBase58())) return;
    const owner = parseTokenOwner(info);
    if (owner && owner.equals(vaultAuthority) && !allowed.some((a) => a.equals(r.pubkey))) {
      throw new RebalanceBuildError(
        `route references vault-authority-owned token account ${r.pubkey.toBase58()} ` +
          "(not source/dest) — Jupiter did not use shared accounts; the program would reject it",
      );
    }
  });
}

// Build one batch entry (compact plan + remaining accounts + lookup tables) for a single leg.
// With `program`, a leg whose transaction would not fit alone (with its price signature) is
// re-quoted on a smaller route; packBatches reports one that still does not fit.
async function buildLegEntry(connection, ctx, component, legAtoms, side, program = null) {
  const method = side === "sell" ? "executeRebalanceSellBatch" : "executeRebalanceBuyBatch";
  let built = await buildLegEntryOnRoute(connection, ctx, component, legAtoms, side, ROUTE_MAX_ACCOUNTS[0]);
  for (const maxAccounts of ROUTE_MAX_ACCOUNTS.slice(1)) {
    if (!program || (await batchFits(program, ctx, method, [built]))) return built;
    log(`    [route] component ${component.globalIndex} ${side}: its route does not fit one transaction; trying one of up to ${maxAccounts} accounts`);
    try {
      built = await buildLegEntryOnRoute(connection, ctx, component, legAtoms, side, maxAccounts);
    } catch (error) {
      log(`    [route] component ${component.globalIndex} ${side}: no smaller route (${error.message ?? error})`);
      return built;
    }
  }
  return built;
}

// A buy is quoted for its exact output, the leg. Some tokens can only be bought by exact input
// (their routes, such as Sanctum's stake pools, don't support ExactOut): those are quoted for an
// input sized so that even the slippage-bounded minimum output covers the leg. The buy then
// spends exactly that input and receives at least the leg, and the program bounds its price and
// spend against the signed price as for any buy.
const EXACT_IN_SIZING_ROUNDS = 3;
const isNoRouteError = (error) => /NO_ROUTES_FOUND|COULD_NOT_FIND_ANY_ROUTE|No routes found/i.test(String(error?.message ?? error));

async function buyQuote(component, legAtoms, maxAccounts, quote = jupiterQuote) {
  const leg = BigInt(legAtoms);
  try {
    return await quote({ inputMint: USDC_MINT, outputMint: component.mint, amount: leg, swapMode: "ExactOut", maxAccounts });
  } catch (error) {
    if (!isNoRouteError(error)) throw error;
  }
  // What the leg sells for prices it; each round scales the input by how far the guaranteed
  // output fell short, with a little margin, until it covers the leg.
  const probe = await quote({ inputMint: component.mint, outputMint: USDC_MINT, amount: leg, swapMode: "ExactIn", maxAccounts });
  let input = (BigInt(probe.outAmount) * BigInt(BPS + SLIPPAGE_BPS)) / BigInt(BPS - SLIPPAGE_BPS) + 1n;
  for (let round = 1; round <= EXACT_IN_SIZING_ROUNDS; round += 1) {
    const exactIn = await quote({ inputMint: USDC_MINT, outputMint: component.mint, amount: input, swapMode: "ExactIn", maxAccounts });
    const guaranteed = BigInt(exactIn.otherAmountThreshold);
    if (guaranteed >= leg) {
      log(`    [route] component ${component.globalIndex} buy: no ExactOut route; buying with exactly ${input} USDC atoms for at least ${guaranteed} (leg ${leg})`);
      return exactIn;
    }
    if (guaranteed === 0n) break;
    input = (input * leg * 1_002n) / (guaranteed * 1_000n) + 1n;
  }
  throw new RebalanceBuildError(`component ${component.globalIndex} buy: no ExactOut route, and no ExactIn input found that guarantees the leg`);
}

async function buildLegEntryOnRoute(connection, ctx, component, legAtoms, side, maxAccounts) {
  const isSell = side === "sell";
  const quote = isSell
    ? await jupiterQuote({ inputMint: component.mint, outputMint: USDC_MINT, amount: legAtoms, swapMode: "ExactIn", maxAccounts })
    : await buyQuote(component, legAtoms, maxAccounts);
  const exactInBuy = !isSell && quote.swapMode === "ExactIn";
  const swap = await jupiterSwapInstruction(quote, ctx.vaultAuthority);
  if (swap.swapInstruction.programId !== JUPITER_V6.toBase58()) {
    throw new RebalanceBuildError(`unexpected Jupiter program ${swap.swapInstruction.programId}`);
  }
  const { plan, routeAccounts } = encodeSwapPlan(swap.swapInstruction, ctx, component);
  const sourceDest = isSell
    ? [component.vault, ctx.vaultQuote]
    : [ctx.vaultQuote, component.vault];
  await assertRouteScope(connection, routeAccounts, ctx.vaultAuthority, sourceDest);
  // quote_limit: sell -> min USDC out; buy -> max USDC in. Jupiter's otherAmountThreshold is
  // the slippage-adjusted bound for the chosen swap mode; an ExactIn buy spends exactly its input.
  const quoteLimit = new anchor.BN(exactInBuy ? quote.inAmount : quote.otherAmountThreshold);
  const remaining = [
    { pubkey: component.pagePda, isSigner: false, isWritable: false },
    { pubkey: component.mint, isSigner: false, isWritable: false },
    { pubkey: component.vault, isSigner: false, isWritable: true },
    { pubkey: component.tokenProgram, isSigner: false, isWritable: false },
    ...routeAccounts,
  ];
  const lookupTables = await loadLookupTables(connection, swap.addressLookupTableAddresses);
  return {
    component,
    // What the swap is sure to deliver, which the pre-swap price check divides by: the leg, or
    // for an ExactIn buy its minimum output.
    atoms: exactInBuy ? BigInt(quote.otherAmountThreshold) : BigInt(legAtoms),
    entry: {
      componentIndex: component.globalIndex,
      quoteLimit,
      routeAccountCount: routeAccounts.length,
      swap: plan,
    },
    remaining,
    lookupTables,
  };
}

function dedupeTables(tables) {
  const seen = new Map();
  for (const t of tables) seen.set(t.key.toBase58(), t);
  return [...seen.values()];
}

function executeBatchIx(program, ctx, method, batch) {
  return program.methods[method]({
    entries: batch.map((b) => b.entry),
    maxPriceAgeSlots: new anchor.BN(MAX_PRICE_AGE_SLOTS),
    maxOracleSlippageBps: Math.min(MAX_FIXED_WEIGHT_EXECUTION_SLIPPAGE_BPS, SLIPPAGE_BPS + VERIFY_ORACLE_BUFFER_BPS),
  })
    .accounts({
      priceOracle: PRICE_ORACLE,
      instructionsSysvar: SYSVAR_INSTRUCTIONS_PUBKEY,
      keeper: ctx.keeper,
      index: ctx.index,
      intent: ctx.intent,
      vaultAuthority: ctx.vaultAuthority,
      quoteMint: USDC_MINT,
      vaultQuoteTokenAccount: ctx.vaultQuote,
      jupiterProgram: JUPITER_V6,
      associatedTokenProgram: ASSOCIATED_TOKEN_PROGRAM_ID,
      quoteTokenProgram: TOKEN_PROGRAM_ID,
      systemProgram: SystemProgram.programId,
    })
    .remainingAccounts(batch.flatMap((b) => b.remaining))
    .instruction();
}

// Whether these legs fit one execute transaction, counting the Ed25519 instruction that signs
// their prices (one per leg).
async function batchFits(program, ctx, method, group) {
  const ix = await executeBatchIx(program, ctx, method, group);
  const tables = stepTables(ctx, group.flatMap((b) => b.lookupTables));
  return fitsOneTransaction(compiledFootprint({ publicKey: ctx.keeper }, [...budgetIxs(1_400_000), placeholderPriceInstruction(group.length), ix], tables));
}

// The lookup tables a step compiles with: its Jupiter routes' and the basket's own.
function stepTables(ctx, routeTables = []) {
  return dedupeTables([...routeTables, ...(ctx.lookupTable ? [ctx.lookupTable] : [])]);
}

// Greedily pack entries into batches that fit BOTH the per-batch swap cap and the 1232-byte
// tx limit. Throws (RebalanceBuildError) if a single entry can't fit alone.
async function packBatches(connection, program, ctx, method, entries) {
  const batches = [];
  let cur = [];
  const fits = (group) => batchFits(program, ctx, method, group);
  for (const e of entries) {
    const trial = [...cur, e];
    if (trial.length <= BATCH_SIZE && (await fits(trial))) {
      cur = trial;
      continue;
    }
    if (cur.length === 0) {
      throw new RebalanceBuildError(
        `component ${e.component.globalIndex} swap does not fit a single transaction`,
      );
    }
    batches.push(cur);
    cur = [e];
    if (!(await fits(cur))) {
      throw new RebalanceBuildError(
        `component ${e.component.globalIndex} swap does not fit a single transaction`,
      );
    }
  }
  if (cur.length) batches.push(cur);
  return batches;
}

function batchCuLimit(legCount) {
  return Math.min(1_400_000, 250_000 + legCount * 450_000);
}

async function sendBatches(env, ctx, method, batches) {
  for (let i = 0; i < batches.length; i += 1) {
    const batch = batches[i];
    await sendWithFreshPrices(env, ctx, batch.map((b) => b.component),
      `${method} batch ${i + 1}/${batches.length}`,
      () => executeBatchIx(env.program, ctx, method, batch),
      batchCuLimit(batch.length), 3, stepTables(ctx, batch.flatMap((b) => b.lookupTables)));
  }
}

// --- execution steps ---------------------------------------------------------

function buildContext(keeper, indexPk, indexState) {
  const vaultAuthority = vaultAuthorityPda(indexPk);
  const vaultQuote = getAssociatedTokenAddressSync(
    USDC_MINT,
    vaultAuthority,
    true,
    TOKEN_PROGRAM_ID,
    ASSOCIATED_TOKEN_PROGRAM_ID,
  );
  return { keeper, index: indexPk, indexMint: indexState.indexMint, vaultAuthority, vaultQuote };
}

function openIx(program, ctx, components, pagePdas, { expiresAt, maxPostRebalanceDriftBps }) {
  return program.methods
    .openRebalanceIntent({
      nonce: ctx.nonce,
      expiresAt: new anchor.BN(expiresAt),
      maxPriceAgeSlots: new anchor.BN(MAX_PRICE_AGE_SLOTS),
      navToleranceBps: NAV_TOLERANCE_BPS,
      maxPostRebalanceDriftBps,
    })
    .accounts({
      initiator: ctx.keeper,
      index: ctx.index,
      indexMint: ctx.indexMint,
      vaultAuthority: ctx.vaultAuthority,
      quoteMint: USDC_MINT,
      vaultQuoteTokenAccount: ctx.vaultQuote,
      intent: ctx.intent,
      priceOracle: PRICE_ORACLE,
      instructionsSysvar: SYSVAR_INSTRUCTIONS_PUBKEY,
      associatedTokenProgram: ASSOCIATED_TOKEN_PROGRAM_ID,
      quoteTokenProgram: TOKEN_PROGRAM_ID,
      systemProgram: SystemProgram.programId,
    })
    .remainingAccounts([
      ...pagePdas.map((p) => ({ pubkey: p, isSigner: false, isWritable: false })),
      ...components.map((c) => ({ pubkey: c.vault, isSigner: false, isWritable: false })),
    ])
    .instruction();
}

function finalizeIx(program, ctx, components, pagePdas) {
  return program.methods
    .finalizeRebalance({ maxPriceAgeSlots: new anchor.BN(MAX_PRICE_AGE_SLOTS) })
    .accounts({
      keeper: ctx.keeper,
      index: ctx.index,
      intent: ctx.intent,
      vaultAuthority: ctx.vaultAuthority,
      quoteMint: USDC_MINT,
      vaultQuoteTokenAccount: ctx.vaultQuote,
      priceOracle: PRICE_ORACLE,
      instructionsSysvar: SYSVAR_INSTRUCTIONS_PUBKEY,
      quoteTokenProgram: TOKEN_PROGRAM_ID,
    })
    .remainingAccounts([
      ...pagePdas.map((p) => ({ pubkey: p, isSigner: false, isWritable: true })),
      ...components.map((c) => ({ pubkey: c.vault, isSigner: false, isWritable: false })),
    ])
    .instruction();
}

// Bytes and accounts of a step that prices every component (open, finalize), with its
// signature instruction and the basket's lookup table.
async function pricedStepFootprint(payer, ctx, components, buildIx, cuLimit) {
  return compiledFootprint(payer, [...budgetIxs(cuLimit), placeholderPriceInstruction(pricedTokens(components).length), await buildIx()], stepTables(ctx));
}

async function openIntent(env, ctx, components, pagePdas, driftThresholdBps, nowOnChain) {
  const { program } = env;
  ctx.nonce = new anchor.BN(Date.now());
  ctx.intent = intentPda(ctx.index, ctx.nonce);

  let maxPost = 100;
  if (driftThresholdBps > 0) maxPost = Math.min(maxPost, driftThresholdBps - 1);
  maxPost = Math.max(
    MIN_FIXED_WEIGHT_POST_REBALANCE_DRIFT_BPS,
    Math.min(MAX_FIXED_WEIGHT_POST_REBALANCE_DRIFT_BPS, maxPost),
  );

  const prices = await sendWithFreshPrices(
    env,
    ctx,
    components,
    "open rebalance intent",
    () => openIx(program, ctx, components, pagePdas, { expiresAt: nowOnChain + INTENT_TTL_S, maxPostRebalanceDriftBps: maxPost }),
    OPEN_CU_LIMIT,
    3,
    stepTables(ctx),
  );
  return { intent: ctx.intent, prices };
}

async function finalize(env, ctx, components, pagePdas) {
  await sendWithFreshPrices(
    env,
    ctx,
    components,
    "finalize rebalance",
    () => finalizeIx(env.program, ctx, components, pagePdas),
    FINALIZE_CU_LIMIT,
    3,
    stepTables(ctx),
  );
}

async function cancelIntent(connection, program, payer, ctx, intentPk) {
  const ix = await program.methods
    .cancelRebalance()
    .accounts({ initiator: payer.publicKey, index: ctx.index, intent: intentPk })
    .instruction();
  await sendV0(connection, payer, [...budgetIxs(100_000), ix], "cancel rebalance intent");
}

// Whether no leg of an open rebalance intent has swapped yet, as cancel_rebalance counts it:
// components without a leg count as completed from the open.
function noLegExecuted(intent) {
  const count = intent.componentCount;
  const legs = (bitmap) => Array.from({ length: count }, (_, i) => bitmapGet(bitmap, i)).filter(Boolean).length;
  return intent.completedSells === count - legs(intent.sellLegBitmap)
    && intent.completedBuys === count - legs(intent.buyLegBitmap);
}

// An account just written may not be visible yet on the RPC node that answers the next read:
// reads it until it is, briefly.
async function fetchWritten(account, address, { attempts = 10, delayMs = 500 } = {}) {
  for (let attempt = 1; ; attempt += 1) {
    const value = await account.fetchNullable(address, "confirmed");
    if (value) return value;
    if (attempt >= attempts) throw new Error(`${address.toBase58()} is still not visible after ${attempts} reads`);
    await new Promise((r) => setTimeout(r, delayMs));
  }
}

// Closes a settled intent for its rent. Right after a cancel, the RPC node that simulates the
// close may not have seen the cancel yet and refuse it as still open, so that refusal is retried
// a few times; anything left is closed by the next pass's sweep (closeSettledRebalanceIntents).
async function closeIntent(connection, program, payer, intentPk, { attempts = 4, retryDelayMs = 2_000 } = {}) {
  for (let attempt = 1; ; attempt += 1) {
    try {
      const ix = await program.methods
        .closeRebalanceIntent()
        .accounts({ initiator: payer.publicKey, intent: intentPk })
        .instruction();
      await sendV0(connection, payer, [...budgetIxs(50_000), ix], "close rebalance intent (rent)");
      return true;
    } catch (error) {
      const text = String(error?.message ?? error) + (error?.logs ?? []).join("\n");
      if (/RebalanceIntentStillOpen/.test(text) && attempt < attempts) {
        await new Promise((r) => setTimeout(r, retryDelayMs));
        continue;
      }
      log(`    [warn] could not close intent (rent left for the next sweep): ${error.message ?? error}`);
      return false;
    }
  }
}

// Rebalance intents this keeper opened that are settled but still hold its rent, because a close
// failed: closed at the start of each pass. The initiator follows the discriminator and index.
async function closeSettledRebalanceIntents(env) {
  const { connection, program, payer } = env;
  const mine = await program.account.rebalanceIntent.all([{ memcmp: { offset: 8 + 32, bytes: payer.publicKey.toBase58() } }]);
  for (const { publicKey, account } of mine) {
    if (enumKey(account.status) === "open") continue;
    log(`closing settled rebalance intent ${publicKey.toBase58()} (${enumKey(account.status)}) for its rent`);
    await closeIntent(connection, program, payer, publicKey);
  }
}

// Unwind never depends on the lookup table: it uses the basket's when one is found, and a basket
// small enough goes without.
async function unwind(connection, program, payer, ctx, intentPk, components, pagePdas) {
  const remaining = [
    ...pagePdas.map((p) => ({ pubkey: p, isSigner: false, isWritable: true })),
    ...components.map((c) => ({ pubkey: c.vault, isSigner: false, isWritable: false })),
  ];
  const ix = await program.methods
    .unwindRebalance()
    .accounts({ caller: payer.publicKey, index: ctx.index, intent: intentPk })
    .remainingAccounts(remaining)
    .instruction();
  await sendV0(connection, payer, [...budgetIxs(FINALIZE_CU_LIMIT), ix], "unwind rebalance", stepTables(ctx));
}

// --- the basket's address lookup table ------------------------------------------
//
// Open and finalize name every component's vault and carry a signed price for each, and a swap
// shares its transaction with a Jupiter route. One lookup table per basket, created and paid for
// by the keeper (its authority), holds the basket's accounts so each costs one byte instead of
// 32. The keeper finds it again by authority and contents, so nothing is stored between runs.

// The accounts rebalance steps name other than signers, the intent and top-level programs.
function basketLookupAddresses(ctx, components, pagePdas) {
  const keys = [
    ctx.index, ctx.indexMint, ctx.vaultAuthority, ctx.vaultQuote, USDC_MINT, PRICE_ORACLE,
    SYSVAR_INSTRUCTIONS_PUBKEY, ASSOCIATED_TOKEN_PROGRAM_ID, TOKEN_PROGRAM_ID, SystemProgram.programId, JUPITER_V6,
    ...pagePdas,
    ...components.flatMap((c) => [c.vault, c.mint, c.tokenProgram]),
  ];
  return [...new Map(keys.map((k) => [k.toBase58(), k])).values()];
}

// Active lookup tables `authority` controls.
async function lookupTablesOf(connection, authority) {
  const accounts = await connection.getProgramAccounts(AddressLookupTableProgram.programId, {
    commitment: "confirmed",
    filters: [{ memcmp: { offset: LOOKUP_TABLE_AUTHORITY_OFFSET, bytes: authority.toBase58() } }],
  });
  return accounts
    .map(({ pubkey, account }) => new AddressLookupTableAccount({ key: pubkey, state: AddressLookupTableAccount.deserialize(account.data) }))
    .filter((table) => table.isActive());
}

// The table holding the basket's index account and the most of `wanted`, if any.
function pickBasketTable(tables, index, wanted) {
  let best = null;
  let bestCount = -1;
  for (const table of tables) {
    const held = new Set(table.state.addresses.map((a) => a.toBase58()));
    if (!held.has(index.toBase58())) continue;
    const count = wanted.filter((a) => held.has(a.toBase58())).length;
    if (count > bestCount) [best, bestCount] = [table, count];
  }
  return best;
}

function missingFrom(table, wanted) {
  const held = new Set(table?.state.addresses.map((a) => a.toBase58()) ?? []);
  return wanted.filter((a) => !held.has(a.toBase58()));
}

// The table once it holds `wanted` and its last extension has warmed up (addresses added in a
// slot are usable from the next one).
async function usableLookupTable(connection, address, wanted, timeoutMs = 60_000) {
  const deadline = Date.now() + timeoutMs;
  for (;;) {
    const [{ value: table }, slot] = await Promise.all([
      connection.getAddressLookupTable(address, { commitment: "confirmed" }),
      connection.getSlot("confirmed"),
    ]);
    if (table?.isActive() && !missingFrom(table, wanted).length && slot > table.state.lastExtendedSlot) return table;
    if (Date.now() > deadline) throw new RebalanceBuildError(`lookup table ${address.toBase58()} is not usable yet`);
    await new Promise((r) => setTimeout(r, 500));
  }
}

function lookupTableRent(connection, addressCount) {
  return connection.getMinimumBalanceForRentExemption(LOOKUP_TABLE_META_BYTES + 32 * addressCount);
}

// basket index -> table address, so a long-running keeper skips the search.
const basketTables = new Map();

/**
 * The basket's lookup table, created or extended (with --execute) so it holds every account
 * rebalance steps name. Without --execute it only logs what it would create or extend and returns
 * the table found, if any. Never creates one when the search for an existing one failed.
 */
async function findBasketLookupTable(env, ctx, wanted) {
  const { connection, payer } = env;
  const cached = basketTables.get(ctx.index.toBase58());
  if (cached) {
    const { value: table } = await connection.getAddressLookupTable(cached, { commitment: "confirmed" });
    if (table?.isActive()) return table;
  }
  return pickBasketTable(await lookupTablesOf(connection, payer.publicKey), ctx.index, wanted);
}

async function ensureBasketLookupTable(env, ctx, components, pagePdas, { execute = EXECUTE } = {}) {
  const { connection, payer } = env;
  const wanted = basketLookupAddresses(ctx, components, pagePdas);
  const key = ctx.index.toBase58();
  const table = await findBasketLookupTable(env, ctx, wanted);
  const missing = missingFrom(table, wanted);
  if (table && !missing.length) {
    basketTables.set(key, table.key);
    return usableLookupTable(connection, table.key, wanted);
  }
  const total = (table?.state.addresses.length ?? 0) + missing.length;
  if (total > LOOKUP_TABLE_MAX_ADDRESSES) {
    throw new RebalanceBuildError(`the basket's lookup table would need ${total} addresses (max ${LOOKUP_TABLE_MAX_ADDRESSES})`);
  }
  const rent = (await lookupTableRent(connection, total)) - (table ? await lookupTableRent(connection, table.state.addresses.length) : 0);
  const action = table
    ? `extend lookup table ${table.key.toBase58()} by ${missing.length} address(es)`
    : `create a lookup table holding the basket's ${missing.length} accounts`;
  const txCount = Math.ceil(missing.length / LOOKUP_TABLE_EXTEND_CHUNK);
  if (!execute) {
    log(`  [dry-run] would ${action}: ${(rent / 1e9).toFixed(6)} SOL rent and ${txCount} transaction(s) from the keeper`);
    return table;
  }
  log(`  ${action} (${(rent / 1e9).toFixed(6)} SOL rent, ${txCount} transaction(s))`);
  const chunks = [];
  for (let i = 0; i < missing.length; i += LOOKUP_TABLE_EXTEND_CHUNK) chunks.push(missing.slice(i, i + LOOKUP_TABLE_EXTEND_CHUNK));
  const extendIx = (lookupTable, addresses) =>
    AddressLookupTableProgram.extendLookupTable({ lookupTable, authority: payer.publicKey, payer: payer.publicKey, addresses });
  let address = table?.key;
  if (!address) {
    const recentSlot = await connection.getSlot("finalized");
    const [createIx, created] = AddressLookupTableProgram.createLookupTable({ authority: payer.publicKey, payer: payer.publicKey, recentSlot });
    address = created;
    await sendV0(connection, payer, [...budgetIxs(LOOKUP_TABLE_CU_LIMIT), createIx, extendIx(address, chunks.shift())], "create lookup table");
  }
  for (const chunk of chunks) {
    await sendV0(connection, payer, [...budgetIxs(LOOKUP_TABLE_CU_LIMIT), extendIx(address, chunk)], "extend lookup table");
  }
  basketTables.set(key, address);
  const usable = await usableLookupTable(connection, address, wanted);
  log(`  lookup table ${address.toBase58()} holds ${usable.state.addresses.length} addresses`);
  return usable;
}

// The basket's table if one exists (read-only), else a stand-in holding what it would, for
// sizing a dry run.
async function basketTableForSizing(env, ctx, components, pagePdas) {
  const table = await ensureBasketLookupTable(env, ctx, components, pagePdas, { execute: false })
    .catch((error) => void log(`  [warn] could not look up the basket's table: ${error.message ?? error}`));
  if (table && !missingFrom(table, basketLookupAddresses(ctx, components, pagePdas)).length) return table;
  return standInLookupTable(env, ctx, components, pagePdas);
}

// A table holding exactly what the basket's would. Transactions compile to the same size and
// accounts through it, since a lookup costs one byte whatever else the real table holds.
function standInLookupTable(env, ctx, components, pagePdas) {
  return new AddressLookupTableAccount({
    key: PublicKey.default,
    state: {
      deactivationSlot: 2n ** 64n - 1n,
      lastExtendedSlot: 0,
      lastExtendedSlotStartIndex: 0,
      authority: env.payer.publicKey,
      addresses: basketLookupAddresses(ctx, components, pagePdas),
    },
  });
}

// --- rebalance request and intent cleanup -------------------------------------

async function setRebalanceRequest(env, indexPk, requested) {
  const { connection, program, payer } = env;
  const ix = await program.methods[requested ? "requestRebalance" : "cancelRebalanceRequest"]()
    .accounts({ operator: payer.publicKey, index: indexPk })
    .instruction();
  await sendV0(connection, payer, [...budgetIxs(50_000), ix], requested ? "request rebalance" : "cancel rebalance request");
}

// Instructions that give `owner` a token account for `mint` the program will accept as theirs.
// `fresh` creates a new account owned by them inside the same transaction, so nothing the owner
// does to their own accounts (closing, reassigning, requiring memos) can make the refund fail.
async function ownerTokenAccount(env, owner, mint, tokenProgram, fresh) {
  const { connection, payer } = env;
  if (!fresh) {
    const ata = getAssociatedTokenAddressSync(mint, owner, true, tokenProgram, ASSOCIATED_TOKEN_PROGRAM_ID);
    return { address: ata, setup: [createAssociatedTokenAccountIdempotentInstruction(payer.publicKey, ata, owner, mint, tokenProgram, ASSOCIATED_TOKEN_PROGRAM_ID)] };
  }
  const seed = `refund-${Date.now().toString(36)}-${Math.random().toString(36).slice(2, 8)}`;
  const address = await PublicKey.createWithSeed(payer.publicKey, seed, tokenProgram);
  const space = getAccountLenForMint(await getMint(connection, mint, "confirmed", tokenProgram));
  return {
    address,
    setup: [
      SystemProgram.createAccountWithSeed({ fromPubkey: payer.publicKey, newAccountPubkey: address, basePubkey: payer.publicKey, seed, lamports: await connection.getMinimumBalanceForRentExemption(space), space, programId: tokenProgram }),
      createInitializeAccount3Instruction(address, mint, owner, tokenProgram),
    ],
  };
}

// Send one settle step, first into the owner's own account and, if that is refused, into a
// fresh account created for them in the same transaction.
async function sendSettleStep(env, label, build) {
  const { connection, payer } = env;
  try {
    await sendV0(connection, payer, [...budgetIxs(400_000), ...(await build(false))], label);
  } catch (error) {
    log(`    owner account refused (${error.message ?? error}); retrying into a fresh account`);
    await sendV0(connection, payer, [...budgetIxs(400_000), ...(await build(true))], `${label} (fresh account)`);
  }
}

// Settle every expired mint/redeem intent of this basket, as anyone may: move what the owner
// deposited or is still owed into the refund escrow, re-mint a redeem that never sold
// anything, or finalize a redeem whose legs all completed.
async function settleExpiredIntents(env, indexPk, indexState, components, pagePdas, now) {
  const { connection, program, payer } = env;
  const STATUS_OFFSET = 8 + 32 + 32 + 8 + 1; // discriminator, index, owner, nonce, kind
  const bs58 = anchor.utils.bytes.bs58;
  const unsettled = [];
  for (const status of [0, 3]) { // Open, Refunding
    unsettled.push(...await program.account.largeBasketIntent.all([
      { memcmp: { offset: 8, bytes: indexPk.toBase58() } },
      { memcmp: { offset: STATUS_OFFSET, bytes: bs58.encode(Buffer.from([status])) } },
    ]));
  }
  const vaultAuthority = vaultAuthorityPda(indexPk);
  const pages = pagePdas.map((p) => meta(p, true));
  for (const { publicKey: intentPk, account: intent } of unsettled) {
    if (now <= Number(intent.expiresAt)) continue;
    const kind = enumKey(intent.kind);
    const owner = intent.owner;
    const intentLock = PublicKey.findProgramAddressSync([Buffer.from("large-basket-intent-lock"), indexPk.toBuffer(), owner.toBuffer()], PROGRAM_ID)[0];
    const label = `${kind} intent ${intentPk.toBase58().slice(0, 8)}.. of ${owner.toBase58().slice(0, 8)}..`;
    const cancelIx = (ownerIndexTokenAccount, remaining) => program.methods.cancelExpiredLargeBasketIntent()
      .accounts({ index: indexPk, indexMint: indexState.indexMint, vaultAuthority, intent: intentPk, intentLock, ownerIndexTokenAccount, tokenProgram: TOKEN_PROGRAM_ID })
      .remainingAccounts(remaining)
      .instruction();
    const indexAta = getAssociatedTokenAddressSync(indexState.indexMint, owner, true);
    try {
      if (kind === "redeem" && intent.completedComponents === intent.componentCount) {
        const ix = await program.methods.finalizeLargeBasketRedeemIntent().accounts({ index: indexPk, intent: intentPk, intentLock }).instruction();
        await sendV0(connection, payer, [...budgetIxs(100_000), ix], `finalize expired ${label}`);
        continue;
      }
      if (intent.completedComponents === 0) {
        if (kind === "mint") {
          await sendV0(connection, payer, [...budgetIxs(100_000), await cancelIx(indexAta, [])], `cancel expired ${label}`);
        } else {
          // Restores the reservation and re-mints the basket tokens to the owner.
          await sendSettleStep(env, `cancel expired ${label}`, async (fresh) => {
            const account = await ownerTokenAccount(env, owner, indexState.indexMint, TOKEN_PROGRAM_ID, fresh);
            return [...account.setup, await cancelIx(account.address, pages)];
          });
        }
        continue;
      }
      // Mints return the filled components, redeems the unfilled ones; skip what is back already.
      // They go to the program's refund escrow, where the owner claims them: no rent per
      // refund and nothing the owner can make refuse the transfer.
      const owed = components.filter((c) => {
        const filled = bitmapGet(intent.componentFillBitmap, c.globalIndex);
        return (kind === "mint" ? filled : !filled)
          && !intent.componentAmounts[c.globalIndex].isZero()
          && !bitmapGet(intent.refundedBitmap, c.globalIndex);
      });
      const escrow = refundEscrowPda(indexPk);
      for (let i = 0; i < owed.length; i += REFUNDS_PER_TX) {
        const batch = owed.slice(i, i + REFUNDS_PER_TX);
        const setup = batch.map((c) => createAssociatedTokenAccountIdempotentInstruction(payer.publicKey, refundEscrowAccount(escrow, c), escrow, c.mint, c.tokenProgram, ASSOCIATED_TOKEN_PROGRAM_ID));
        const groups = batch.flatMap((c) => [meta(c.mint), meta(c.vault, true), meta(refundEscrowAccount(escrow, c), true), meta(c.tokenProgram)]);
        await sendV0(connection, payer, [...budgetIxs(400_000), ...setup, await cancelIx(indexAta, [...pages, ...groups])], `escrow ${batch.length} component(s) of ${label}`);
      }
    } catch (error) {
      log(`    [warn] could not settle expired ${label}: ${error.message ?? error}`);
    }
  }
}

const REFUNDS_PER_TX = 3;

function refundEscrowPda(index) {
  return PublicKey.findProgramAddressSync([Buffer.from("refund-escrow"), index.toBuffer()], PROGRAM_ID)[0];
}

function refundEscrowAccount(escrow, component) {
  return getAssociatedTokenAddressSync(component.mint, escrow, true, component.tokenProgram, ASSOCIATED_TOKEN_PROGRAM_ID);
}

function meta(pubkey, isWritable = false) {
  return { pubkey, isSigner: false, isWritable };
}

// --- preview (dry-run, read-only) -------------------------------------------

async function previewSwaps(env, ctx, components, pagePdas, legs) {
  const { connection, program, payer } = env;
  const previewIntent = intentPda(ctx.index, new anchor.BN(Date.now()));
  const pctx = { ...ctx, intent: previewIntent, lookupTable: await basketTableForSizing(env, ctx, components, pagePdas) };
  for (const side of ["sell", "buy"]) {
    const sideLegs = legs.filter((l) => l.side === side);
    if (!sideLegs.length) continue;
    const method = side === "sell" ? "executeRebalanceSellBatch" : "executeRebalanceBuyBatch";
    const entries = [];
    for (const l of sideLegs) {
      try {
        const built = await buildLegEntry(connection, pctx, l.component, l.leg, side, program);
        entries.push(built);
        log(
          `    [preview] ${side} component ${l.component.globalIndex}: ` +
            `${built.entry.swap.accounts.length} route metas, ` +
            `${built.entry.swap.instructionData.length}B ix data`,
        );
      } catch (error) {
        log(`    [preview] ${side} component ${l.component.globalIndex} failed: ${error.message ?? error}`);
      }
    }
    if (!entries.length) continue;
    try {
      const batches = await packBatches(connection, program, pctx, method, entries);
      for (let i = 0; i < batches.length; i += 1) {
        const ix = await executeBatchIx(program, pctx, method, batches[i]);
        const tables = stepTables(pctx, batches[i].flatMap((b) => b.lookupTables));
        const { bytes, accounts } = compiledFootprint(payer, [...budgetIxs(batchCuLimit(batches[i].length)), placeholderPriceInstruction(batches[i].length), ix], tables);
        log(`    [preview] ${method} batch ${i + 1}/${batches.length}: ${batches[i].length} legs, ${bytes} tx bytes, ${accounts} accounts, ${tables.length} ALT(s)`);
      }
    } catch (error) {
      log(`    [preview] ${method} packing failed: ${error.message ?? error}`);
    }
  }
}

// --- oracle readiness ----------------------------------------------------------

// Why this bot cannot get prices signed that the program accepts: the program's price oracle
// is not set, there is no oracle key or healthy service, or the program accepts another key.
async function oracleProblems(env) {
  let signer;
  if (env.oracleUrl) {
    const health = await fetch(new URL("/health", env.oracleUrl), { signal: AbortSignal.timeout(15_000) })
      .then((res) => res.json())
      .catch((error) => ({ ok: false, error: error.message ?? String(error) }));
    if (!health.ok) return `the oracle service at ${env.oracleUrl} is not healthy: ${health.error ?? "no answer"}`;
    if (health.oracle === env.payer.publicKey.toBase58()) return "the oracle service signs with the keeper's own key";
    signer = { key: health.oracle, what: "the oracle service's" };
  } else {
    if (!env.oracle) return `no oracle key: set ORACLE_URL, ORACLE_KEYPAIR or ${ORACLE_WALLET_PATH}`;
    signer = { key: env.oracle.publicKey.toBase58(), what: "this oracle key" };
  }
  const account = await env.program.account.priceOracle.fetchNullable(PRICE_ORACLE);
  if (!account) return "the program's price oracle is not set yet (scripts/set-price-oracle.mjs)";
  const accepted = account.oracle.toBase58();
  if (accepted !== signer.key) return `the program accepts prices signed by ${accepted}, not ${signer.what} ${signer.key}`;
  return null;
}

// Dry-run: the prices a rebalance would have signed now, next to Jupiter's price API. With
// ORACLE_URL, the oracle service prices them (without signing) and says what would stop it.
async function showPrices(env, ctx, components, jupiterPrices) {
  try {
    const tokens = pricedTokens(components);
    let prices;
    if (env.oracleUrl) {
      const answer = await requestSignedPrices(env.oracleUrl, env.oracleToken, {
        index: ctx.index,
        nonce: new anchor.BN(Date.now()),
        mints: tokens.map((t) => t.mint),
        dryRun: true,
      });
      prices = answer.prices;
      for (const blocker of answer.blockers) log(`    [price] the oracle service would not sign: ${blocker}`);
    } else {
      prices = await computeOraclePrices(tokens);
    }
    for (const [mint, p] of prices) {
      const jup = jupiterPrices.get(mint);
      log(`    [price] ${mint.slice(0, 6)}.. $${p.usd.toPrecision(6)} (round-trip spread ${bpsPct(p.spreadBps)}, confirmed by ${p.confirmedBy.join(" + ")}${jup ? `; price API $${jup.toPrecision(6)}` : ""})`);
    }
  } catch (error) {
    const failures = error instanceof OraclePriceError ? error.failures.map((f) => `${f.label}: ${f.reason}`) : [error.message ?? String(error)];
    for (const failure of failures) log(`    [price] NOT PRICED ${failure}`);
  }
}

// --- per-index processing ----------------------------------------------------

// index pubkey -> epoch ms before which a failed rebalance is not retried.
const rebalanceRetryAt = new Map();

async function processIndex(env, indexPk) {
  const { connection, program, payer } = env;
  const indexState = await program.account.indexState.fetch(indexPk);

  if (enumKey(indexState.kind) !== "fixedWeights") {
    log(`  skip ${indexPk.toBase58()}: not a FixedWeights index`);
    return;
  }
  log(`\nindex ${indexPk.toBase58()} (${indexState.symbol})`);
  if (!indexState.largeBasketConfigured) return void log("  skip: large basket not configured");
  if (!indexState.fixedWeightQuoteMint.equals(USDC_MINT)) {
    return void log("  skip: quote mint is not native USDC");
  }

  const pageCount = indexState.largeBasketPageCount;
  let { components, pagePdas } = await loadComponents(program, indexPk, pageCount);
  const ctx = buildContext(payer.publicKey, indexPk, indexState);
  const now = await chainNow(connection);

  // Stuck / in-progress intent first — a live operation blocks everything else.
  if (indexState.largeBasketOperationInProgress) {
    const active = indexState.activeRebalanceIntent;
    if (active.equals(PublicKey.default)) {
      return void log("  skip: an operation (mint/redeem?) is in progress, no active rebalance intent");
    }
    const intent = await program.account.rebalanceIntent.fetchNullable(active);
    if (!intent || enumKey(intent.status) !== "open") {
      return void log("  skip: op-in-progress set but no open rebalance intent to unwind (inspect manually)");
    }
    const expired = now > Number(intent.expiresAt);
    const isAuthority = indexState.authority.equals(payer.publicKey);
    const isInitiator = intent.initiator.equals(payer.publicKey);
    log(`  in-flight rebalance intent ${active.toBase58()} (expires ${intent.expiresAt}, expired=${expired})`);
    // One this keeper opened whose legs never started (a pass that failed between the open and
    // the first swap) is cancelled now rather than holding the basket's lock until it expires.
    // The program allows the cancel only while no leg has swapped.
    if (!expired && isInitiator && noLegExecuted(intent)) {
      log(`  ${EXECUTE ? "cancelling" : "[dry-run] would cancel"} it: no leg has executed`);
      if (EXECUTE) {
        await cancelIntent(connection, program, payer, ctx, active);
        await closeIntent(connection, program, payer, active);
      }
      return;
    }
    if (!expired && !isAuthority) {
      return void log("  skip: rebalance in progress, not expired, keeper is not the index authority");
    }
    log(`  ${EXECUTE ? "UNWINDING" : "[dry-run] would unwind"} stuck intent to release the lock`);
    if (EXECUTE) {
      // The basket's table if it exists and is complete; unwind never creates or waits on one.
      const wanted = basketLookupAddresses(ctx, components, pagePdas);
      const table = await findBasketLookupTable(env, ctx, wanted).catch(() => null);
      if (table && !missingFrom(table, wanted).length) ctx.lookupTable = table;
      await unwind(connection, program, payer, ctx, active, components, pagePdas);
      log("  unwound; lock released");
      if (isInitiator) await closeIntent(connection, program, payer, active);
    }
    return;
  }

  if (!components.some((c) => c.isQuote)) {
    return void log("  skip: register the USDC reserve component with scripts/register-rebalance-quote.mjs before rebalancing");
  }
  if (indexState.rebalancingPaused) return void log("  skip: rebalancing is paused for this index");

  const nonQuote = components.filter((c) => !c.isQuote && !c.retired).map((c) => c.mint);
  const prices = await fetchPrices(nonQuote);
  const vaultInfos = await connection.getMultipleAccountsInfo(
    [...components.map((c) => c.vault), ctx.vaultQuote],
    "confirmed",
  );
  const vaultAmounts = new Map();
  components.forEach((c, i) => vaultAmounts.set(c.vault.toBase58(), parseTokenAmount(vaultInfos[i])));
  const scratchAtoms = parseTokenAmount(vaultInfos[components.length]);

  const a = assessBasket(components, vaultAmounts, scratchAtoms, prices);
  const changePk = compositionChangePda(indexPk);
  const change = await program.account.compositionChange.fetchNullable(changePk);
  const changeDue = Boolean(change) && now >= Number(change.effectiveAt);
  if (change) {
    const weights = change.targetWeightsBps.map(bpsPct).join(", ");
    const additions = change.additions.map((x) => `${x.mint.toBase58().slice(0, 6)}.. ${bpsPct(x.targetWeightBps)}`).join(", ");
    log(`  composition change ${changeDue ? "DUE" : `pending until ${new Date(Number(change.effectiveAt) * 1000).toISOString()}`}: weights [${weights}]${additions ? ` + add ${additions}` : ""}`);
  }
  // An applied change leaves holdings off the new targets until a rebalance finishes.
  const compositionTriggered = changeDue || indexState.compositionRebalanceDue;
  const driftThresholdBps = indexState.fixedWeightDriftThresholdBps;
  const intervalS = Number(indexState.fixedWeightRebalanceIntervalSeconds);
  const lastRebalancedAt = Number(indexState.fixedWeightLastRebalancedAt);

  // Time trigger is exact (on-chain clock + on-chain last_rebalanced_at), independent of prices.
  const timeTriggered = intervalS > 0 && now >= lastRebalancedAt + intervalS;
  // Drift trigger is an estimate from Jupiter prices; require a margin so the program's
  // recomputation from the signed prices is likely to agree, and only when all prices are known.
  const driftTriggered =
    a.driftKnown &&
    driftThresholdBps > 0 &&
    a.maxDriftBps >= driftThresholdBps + DRIFT_TRIGGER_MARGIN_BPS;

  log(`  NAV ~$${a.nav.toFixed(2)}  maxDrift ${a.driftKnown ? bpsPct(a.maxDriftBps) : "unknown"} (threshold ${bpsPct(driftThresholdBps)})`);
  if (a.missing.length) {
    log(`  [warn] missing Jupiter price for ${a.missing.length} component(s); drift estimate skipped (time trigger still applies)`);
  }
  for (const r of a.rows) {
    const tag = r.side === "none" ? "" : ` -> ${r.side} ${r.leg} atoms`;
    log(
      `    [${r.component.globalIndex}] ${r.component.mint.toBase58().slice(0, 6)}.. ` +
        `weight ${r.usd ? bpsPct(r.weightBps) : "?"} target ${bpsPct(r.component.targetWeightBps)}${tag}`,
    );
  }
  if (intervalS > 0) {
    const due = lastRebalancedAt + intervalS;
    log(`  time trigger: ${timeTriggered ? "DUE" : `next at ${new Date(due * 1000).toISOString()}`}`);
  }

  const requestLive = indexState.rebalanceRequested && now < Number(indexState.rebalanceRequestedAt) + REQUEST_WINDOW_S;
  // Never hold users back without a rebalance to run.
  const releaseRequest = async (why) => {
    if (!EXECUTE || !requestLive) return;
    log(`  releasing the rebalance request (${why})`);
    await setRebalanceRequest(env, indexPk, false).catch((e) => log(`  [warn] could not cancel the request: ${e.message ?? e}`));
  };
  if (SHOW_PRICES && !EXECUTE) await showPrices(env, ctx, components, prices);
  if (!driftTriggered && !timeTriggered && !compositionTriggered) {
    await releaseRequest("no rebalance needed");
    return void log("  -> no rebalance needed");
  }
  log(`  -> rebalance TRIGGERED (drift=${driftTriggered}, time=${timeTriggered}, composition=${compositionTriggered})`);

  if (!EXECUTE) {
    if (changeDue) log("  [dry-run] would apply the composition change first");
    const legs = a.rows.filter((r) => r.side !== "none");
    log(`  [dry-run] would rebalance: ${legs.filter((r) => r.side === "sell").length} sell + ${legs.filter((r) => r.side === "buy").length} buy legs`);
    if (PREVIEW_SWAPS && legs.length) {
      log("  [dry-run] building Jupiter swaps for the estimated legs (no transactions sent)...");
      await previewSwaps(env, ctx, components, pagePdas, legs).catch((e) => log(`  [preview] failed: ${e.message ?? e}`));
    } else {
      log("  [dry-run] pass --execute to run it, or --preview-swaps to encode the Jupiter swaps read-only");
    }
    return;
  }

  const key = indexPk.toBase58();
  const retryAt = rebalanceRetryAt.get(key) ?? 0;
  if (Date.now() < retryAt) {
    await releaseRequest("in failure cooldown");
    return void log(`  skip: last attempt failed; next try after ${new Date(retryAt).toISOString()}`);
  }
  const isOperator = indexState.authority.equals(payer.publicKey) || indexState.rebalanceKeeper.equals(payer.publicKey);
  if (!isOperator) {
    return void log(`  skip: ${payer.publicKey.toBase58()} is not this basket's authority or rebalance keeper (set_rebalance_keeper)`);
  }
  // Every rebalance step needs signed prices; don't hold users back without them.
  const oracleProblem = await oracleProblems(env);
  if (oracleProblem) return void log(`  skip: ${oracleProblem}`);
  try {
    await computeOraclePrices([...pricedTokens(components), ...(changeDue ? await additionTokens(connection, change) : [])]);
  } catch (error) {
    rebalanceRetryAt.set(key, Date.now() + FAILURE_COOLDOWN_S * 1000);
    await releaseRequest("its tokens cannot be priced");
    return void log(`  skip: ${error.message ?? error}`);
  }

  // Mints and redeems run concurrently and a rebalance needs them all settled: hold new ones
  // back, clean up the expired ones, and open once none are left.
  if (indexState.openIntentCount > 0) {
    if (!requestLive) {
      const lastRequestAt = Number(indexState.rebalanceRequestedAt);
      // The program spaces requests itself; after one lapsed unused, back off further.
      const lapsedUnused = lastRequestAt > lastRebalancedAt;
      const nextRequestAt = lastRequestAt + REQUEST_WINDOW_S + (lapsedUnused ? REQUEST_COOLDOWN_S : PROGRAM_REQUEST_COOLDOWN_S);
      if (lastRequestAt > 0 && now < nextRequestAt) {
        return void log(`  ${indexState.openIntentCount} intent(s) open; the next rebalance request is allowed after ${new Date(nextRequestAt * 1000).toISOString()}`);
      }
      log(`  ${indexState.openIntentCount} mint/redeem intent(s) open; requesting a rebalance so new ones wait`);
      await setRebalanceRequest(env, indexPk, true);
    }
    await settleExpiredIntents(env, indexPk, indexState, components, pagePdas, now);
    const remaining = (await program.account.indexState.fetch(indexPk)).openIntentCount;
    if (remaining > 0) return void log(`  waiting for ${remaining} open intent(s) to settle or expire`);
  }
  try {
    if (changeDue) {
      // Needs no open intents, like the rebalance itself; the open below then trades onto it.
      await applyCompositionChange(env, ctx, indexState, change, changePk, pagePdas);
      const applied = await program.account.indexState.fetch(indexPk);
      ({ components, pagePdas } = await loadComponents(program, indexPk, applied.largeBasketPageCount));
      log(`  composition change applied: ${applied.largeBasketComponentCount} components`);
    }
    await executeRebalance(env, ctx, components, pagePdas, driftThresholdBps, now);
  } catch (error) {
    rebalanceRetryAt.set(key, Date.now() + FAILURE_COOLDOWN_S * 1000);
    // A successful open clears the request; if we never got there, let users back in now.
    const fresh = await program.account.indexState.fetch(indexPk).catch(() => null);
    if (fresh?.rebalanceRequested) {
      await setRebalanceRequest(env, indexPk, false).catch((e) => log(`  [warn] could not cancel the request: ${e.message ?? e}`));
    }
    throw error;
  }
}

// Reduce total target backing by at most the intent's NAV tolerance. Match the
// program's integer rounding, retaining at least one atom for each buy leg.
function buyAmount(open, leg, costBps) {
  const reduction = ((open + leg) * BigInt(costBps)) / 10_000n;
  return leg > reduction ? leg - reduction : 1n;
}

async function affordableBuys(components, targets, opens, tolerance, budget, build) {
  const plan = async (bps) => {
    const entries = [];
    for (const c of components) {
      entries.push(await build(c, buyAmount(opens[c.globalIndex], targets[c.globalIndex], bps)));
    }
    return { entries, cost: entries.reduce((sum, b) => sum + BigInt(b.entry.quoteLimit.toString()), 0n) };
  };
  let best = await plan(0);
  if (best.cost <= budget) return best.entries;
  best = await plan(tolerance);
  if (best.cost > budget) throw new RebalanceBuildError("Buy budget exceeds guaranteed USDC proceeds even at the permitted cost allowance");
  let low = 0, high = tolerance;
  while (high - low > 1) {
    const mid = Math.floor((low + high) / 2);
    const candidate = await plan(mid);
    if (candidate.cost <= budget) { high = mid; best = candidate; }
    else low = mid;
  }
  return best.entries;
}

async function applyCompositionChange(env, ctx, indexState, change, changePk, pagePdas) {
  const { connection, program, payer } = env;
  const count = indexState.largeBasketComponentCount;
  const freeInLast = count % COMPONENTS_PER_PAGE === 0 ? 0 : COMPONENTS_PER_PAGE - (count % COMPONENTS_PER_PAGE);
  // Additions fill the last page, then one new page.
  const newPage = change.additions.length > freeInLast ? [pagePda(ctx.index, Math.ceil(count / COMPONENTS_PER_PAGE))] : [];
  const mintInfos = await connection.getMultipleAccountsInfo(change.additions.map((x) => x.mint), "confirmed");
  const additionAccounts = change.additions.flatMap((x, i) => {
    const tokenProgram = mintInfos[i]?.owner;
    if (!tokenProgram || !TOKEN_PROGRAM_IDS.has(tokenProgram.toBase58())) {
      throw new Error(`composition change adds ${x.mint.toBase58()}, which is not a token mint`);
    }
    const vault = getAssociatedTokenAddressSync(x.mint, ctx.vaultAuthority, true, tokenProgram, ASSOCIATED_TOKEN_PROGRAM_ID);
    return [meta(x.mint), meta(vault, true), meta(tokenProgram)];
  });
  log(`  applying the composition change (${change.additions.length} new component(s))...`);
  const ix = await program.methods
    .applyCompositionChange()
    .accounts({
      operator: payer.publicKey,
      index: ctx.index,
      vaultAuthority: ctx.vaultAuthority,
      compositionChange: changePk,
      proposer: change.proposer,
      associatedTokenProgram: ASSOCIATED_TOKEN_PROGRAM_ID,
      systemProgram: SystemProgram.programId,
    })
    .remainingAccounts([...pagePdas.map((p) => meta(p, true)), ...newPage.map((p) => meta(p, true)), ...additionAccounts])
    .instruction();
  await sendV0(connection, payer, [...budgetIxs(400_000), ix], "apply composition change");
}

async function executeRebalance(env, ctx, components, pagePdas, driftThresholdBps, nowOnChain) {
  const { connection, program, payer } = env;
  // Open and finalize carry every component's vault and a signed price for each. Refuse a
  // basket whose open or finalize cannot fit one transaction, through the table it would have,
  // before anything goes on chain or any rent is spent on that table.
  const sizing = {
    ...ctx,
    nonce: new anchor.BN(0),
    intent: intentPda(ctx.index, new anchor.BN(0)),
    lookupTable: standInLookupTable(env, ctx, components, pagePdas),
  };
  for (const [step, build, cuLimit] of [
    ["open", () => openIx(program, sizing, components, pagePdas, { expiresAt: 0, maxPostRebalanceDriftBps: 0 }), OPEN_CU_LIMIT],
    ["finalize", () => finalizeIx(program, sizing, components, pagePdas), FINALIZE_CU_LIMIT],
  ]) {
    const footprint = await pricedStepFootprint(payer, sizing, components, build, cuLimit);
    if (!fitsOneTransaction(footprint)) {
      throw new RebalanceBuildError(
        `${step} would be ${footprint.bytes} bytes and ${footprint.accounts} accounts with ${pricedTokens(components).length} signed prices (limits ${TX_LIMIT} and ${MAX_TX_ACCOUNTS})`,
      );
    }
  }
  // Every step compiles with the basket's lookup table; it is created or extended (and warmed
  // up) before the intent opens.
  ctx.lookupTable = await ensureBasketLookupTable(env, ctx, components, pagePdas);
  log("  opening rebalance intent...");
  const { intent: intentPk, prices } = await openIntent(env, ctx, components, pagePdas, driftThresholdBps, nowOnChain);
  ctx.intent = intentPk;

  // Read the program's authoritative legs (it recomputes them from the signed prices), then
  // build + validate EVERY batch (Jupiter quote, route-scope, tx-size packing) BEFORE sending
  // any execute tx. If anything fails here, no leg has executed yet, so we cancel cleanly
  // instead of stranding the intent (which would hold the lock until expiry).
  let sellBatches;
  let buyBatches;
  try {
    const intent = await fetchWritten(program.account.rebalanceIntent, intentPk);
    const targets = intent.componentTargetAmounts.map((bn) => BigInt(bn.toString()));
    const sellComponents = components.filter((c) => bitmapGet(intent.sellLegBitmap, c.globalIndex));
    const buyComponents = components.filter((c) => bitmapGet(intent.buyLegBitmap, c.globalIndex));
    log(`  intent open: ${sellComponents.length} sell, ${buyComponents.length} buy legs`);
    const sellEntries = [];
    for (const c of sellComponents) sellEntries.push(await buildLegEntry(connection, ctx, c, targets[c.globalIndex], "sell", program));
    const scratch = parseTokenAmount(await connection.getAccountInfo(ctx.vaultQuote, "confirmed"));
    const budget = scratch + sellEntries.reduce((sum, b) => sum + BigInt(b.entry.quoteLimit.toString()), 0n);
    const buyEntries = await affordableBuys(buyComponents, targets,
      intent.componentOpenAmounts.map((bn) => BigInt(bn.toString())),
      Number(intent.navToleranceBps), budget,
      (c, atoms) => buildLegEntry(connection, ctx, c, atoms, "buy", program));
    assertLegsWithinOracle(sellEntries, prices, "sell");
    assertLegsWithinOracle(buyEntries, prices, "buy");
    sellBatches = await packBatches(connection, program, ctx, "executeRebalanceSellBatch", sellEntries);
    buyBatches = await packBatches(connection, program, ctx, "executeRebalanceBuyBatch", buyEntries);
  } catch (error) {
    log(`  build/validation failed (${error.message ?? error}); cancelling the un-executed intent`);
    await cancelIntent(connection, program, payer, ctx, intentPk).catch((e) =>
      log(`  [warn] cancel failed, intent will need unwind after expiry: ${e.message ?? e}`),
    );
    await closeIntent(connection, program, payer, intentPk);
    throw error;
  }

  try {
    log("  sending sell batches...");
    await sendBatches(env, ctx, "executeRebalanceSellBatch", sellBatches);
    log("  sending buy batches...");
    await sendBatches(env, ctx, "executeRebalanceBuyBatch", buyBatches);

    log("  finalizing...");
    await finalize(env, ctx, components, pagePdas);
  } catch (error) {
    // cancel_rebalance only succeeds while no leg has executed (the program enforces it);
    // otherwise the intent holds the lock until expiry and a later pass unwinds it.
    log(`  execution failed (${error.message ?? error}); cancelling the intent if no leg executed`);
    try {
      await cancelIntent(connection, program, payer, ctx, intentPk);
      await closeIntent(connection, program, payer, intentPk);
    } catch (e) {
      log(`  [warn] cancel failed, intent will be unwound after expiry: ${e.message ?? e}`);
    }
    throw error;
  }
  await closeIntent(connection, program, payer, intentPk);
  log(`  DONE: rebalanced ${ctx.index.toBase58()}`);
}

// --- main --------------------------------------------------------------------

let stopRequested = false;

async function runOnce(env) {
  let indexes;
  if (ONLY_INDEX) {
    indexes = [new PublicKey(ONLY_INDEX)];
    log(`targeting single index ${ONLY_INDEX}`);
  } else {
    log("discovering FixedWeights indexes...");
    try {
      indexes = await discoverFixedWeightsIndexes(env.program);
    } catch (error) {
      log(
        `  discovery failed (${error.message ?? error}).\n` +
          "  Public RPCs often disable getProgramAccounts — set SOLANA_RPC_URL to a full RPC, " +
          "or pass --index <pubkey> to target a basket directly.",
      );
      return;
    }
    log(`found ${indexes.length} FixedWeights index(es)`);
  }
  if (EXECUTE) {
    await closeSettledRebalanceIntents(env).catch((error) => log(`  [warn] could not sweep settled rebalance intents: ${error.message ?? error}`));
  }
  for (const indexPk of indexes) {
    if (stopRequested) return;
    try {
      await processIndex(env, indexPk);
    } catch (error) {
      const msg = error.message ?? String(error);
      log(`  ERROR processing ${indexPk.toBase58()}: ${msg}`);
      if (/Invalid bool|Invalid option|buffer|discriminator/i.test(msg)) {
        log(
          "  (a decode error usually means the on-chain account layout doesn't match " +
            "target/idl/basket.json — rebuild with `anchor build` and deploy the matching program)",
        );
      }
    }
  }
}

async function main() {
  const connection = new Connection(RPC_URL, "confirmed");
  const payer = loadKeypair(WALLET_PATH);
  if (ORACLE_URL && process.env.ORACLE_KEYPAIR) {
    throw new Error("ORACLE_KEYPAIR is set but prices come from ORACLE_URL: remove the oracle key from the keeper so it lives only in the oracle service");
  }
  if (ORACLE_URL && ORACLE_API_TOKEN.length < 32) throw new Error("ORACLE_URL needs ORACLE_API_TOKEN (the oracle service's token)");
  const oracle = ORACLE_URL ? null : loadOracleKeypair(ORACLE_WALLET_PATH);
  const program = loadProgram(connection, payer);

  if (!program.programId.equals(PROGRAM_ID)) {
    throw new Error(`IDL program id ${program.programId.toBase58()} != ${PROGRAM_ID.toBase58()}`);
  }
  if (oracle?.publicKey.equals(payer.publicKey)) {
    throw new Error("the oracle key must not be the keeper key: either alone could then set prices and trade");
  }

  log(
    JSON.stringify(
      {
        mode: EXECUTE ? "EXECUTE (will send transactions)" : "dry-run (read-only)",
        rpc: new URL(RPC_URL).origin,
        keeper: payer.publicKey.toBase58(),
        watch: WATCH,
        batchSize: BATCH_SIZE,
        slippageBps: SLIPPAGE_BPS,
        navToleranceBps: NAV_TOLERANCE_BPS,
        verifyOracleBps: Math.min(MAX_FIXED_WEIGHT_EXECUTION_SLIPPAGE_BPS, SLIPPAGE_BPS + VERIFY_ORACLE_BUFFER_BPS),
        ttlSeconds: INTENT_TTL_S,
        oracle: ORACLE_URL ? `service ${new URL(ORACLE_URL).origin}` : oracle?.publicKey.toBase58() ?? null,
        priceOracle: PRICE_ORACLE.toBase58(),
        maxPriceAgeSlots: MAX_PRICE_AGE_SLOTS,
        jupiter: new URL(JUPITER_SWAP_API).origin,
        jupiterKey: Boolean(jupiterApiKey()),
        oraclePricing: ORACLE_CONFIG,
      },
      null,
      2,
    ),
  );

  const env = { connection, program, payer, oracle, oracleUrl: ORACLE_URL, oracleToken: ORACLE_API_TOKEN };
  if (EXECUTE) {
    const problem = await oracleProblems(env).catch((e) => `could not read the price oracle: ${e.message ?? e}`);
    if (problem) log(`[warn] rebalances will be skipped: ${problem}`);
  }
  if (WATCH) {
    let wake = () => {};
    const stop = (signal) => {
      log(`${signal} received; stopping after the current index`);
      stopRequested = true;
      wake();
    };
    process.once("SIGTERM", () => stop("SIGTERM"));
    process.once("SIGINT", () => stop("SIGINT"));
    while (!stopRequested) {
      await runOnce(env).catch((e) => log(`run error: ${e.message ?? e}`));
      if (stopRequested) break;
      log(`\nsleeping ${WATCH_INTERVAL_S}s...\n`);
      await new Promise((r) => {
        wake = r;
        setTimeout(r, WATCH_INTERVAL_S * 1000);
      });
    }
    log("stopped");
    process.exit(0);
  } else {
    await runOnce(env);
  }
}

export {
  compiledSize, compiledFootprint, fitsOneTransaction, PROGRAM_ID, PRICE_ORACLE, sendV0, executeBatchIx, openIx, finalizeIx,
  packBatches, pricedStepFootprint, signPrices, sendWithFreshPrices, waitForSlot, isStalePriceError, assertLegsWithinOracle,
  affordableBuys, buyAmount, settleExpiredIntents, requestSignedPrices, oracleProblems, loadComponents, buildContext,
  buildLegEntry, intentPda, basketLookupAddresses, ensureBasketLookupTable, basketTableForSizing,
  buyQuote, closeIntent, closeSettledRebalanceIntents, noLegExecuted, fetchWritten,
};
if (process.argv[1] && import.meta.url === pathToFileURL(path.resolve(process.argv[1])).href) {
  await main();
}
