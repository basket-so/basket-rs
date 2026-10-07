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
// Prices: open, each swap batch and finalize read the program's price board. Right before
// each of them the bot prices the tokens involved (scripts/lib/price-oracle.mjs) and posts
// them with `post_prices`, signed by the oracle key; the keeper pays the fee. The oracle key
// is deliberately separate from the keeper's, so neither alone can both set prices and trade.
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
//      ORACLE_KEYPAIR (secret key JSON array; takes precedence over ORACLE_WALLET),
//      ORACLE_WALLET (default oracle-keypair.json), JUPITER_SWAP_API, JUPITER_PRICE_API.
//
// In --watch mode SIGTERM/SIGINT stop the loop after the index being processed, so a
// restart doesn't abandon a rebalance mid-flight (see fly.toml kill_timeout).
//
// NOTE: build the program (anchor build, regenerates target/idl/basket.json) and deploy it
// before the --execute path works against a real basket.

import { OraclePriceError, PRICE_SCALE, oraclePrices } from "./lib/price-oracle.mjs";
import anchor from "@coral-xyz/anchor";
import {
  ComputeBudgetProgram,
  Connection,
  Keypair,
  PublicKey,
  SystemProgram,
  SYSVAR_CLOCK_PUBKEY,
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
const PRICE_BOARD_SEED = Buffer.from("price-board");
const COMPONENTS_PER_PAGE = 10;

const BPS = 10_000;
const TX_LIMIT = 1232; // Solana packet MTU; a compiled tx must not exceed this.
const USDC_DECIMALS = 6;
// Mirror of the program's constants (programs/basket/src/constants.rs).
const MAX_PRICE_AGE_SLOTS = 150;
const MAX_PRICES_PER_POST = 20;
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
// Dry-run only: compute the oracle prices each basket's rebalance would post.
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
// opens the program then rejects (it re-decides from the posted prices, which can straddle).
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
const JUPITER_SWAP_API = process.env.JUPITER_SWAP_API ?? "https://lite-api.jup.ag/swap/v1";
const JUPITER_PRICE_API = process.env.JUPITER_PRICE_API ?? "https://lite-api.jup.ag/price/v3";

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

async function fetchJson(url, init, attempts = 4) {
  let lastError;
  for (let attempt = 1; attempt <= attempts; attempt += 1) {
    try {
      const res = await fetch(url, init);
      if (res.ok) return res.json();
      const body = await res.text();
      lastError = new Error(`${url} -> ${res.status}: ${body.slice(0, 300)}`);
      if (res.status < 500 || attempt === attempts) throw lastError;
    } catch (error) {
      lastError = error;
      if (attempt === attempts) throw error;
    }
    await new Promise((r) => setTimeout(r, attempt * 1_500));
  }
  throw lastError;
}

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

// The step read a price that aged out (or was never posted) before it landed: post and retry.
// Preflight failures name the error in their logs; a landed failure only carries its code.
function isStalePriceError(program, error) {
  const codes = (program.idl.errors ?? [])
    .filter((e) => /^(stale|missing)OraclePrice$/i.test(e.name))
    .map((e) => e.code);
  const text = String(error?.message ?? error) + (error?.logs ?? []).join("\n");
  return /StaleOraclePrice|MissingOraclePrice/i.test(text)
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

// The key that signs posted prices; without it the bot can still dry-run.
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
const PRICE_BOARD = PublicKey.findProgramAddressSync([PRICE_BOARD_SEED], PROGRAM_ID)[0];
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
// (driftKnown=false) rather than aborting — the program prices from the posted prices anyway.
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
async function computeOraclePricesPatiently(tokens, label) {
  for (let attempt = 1; ; attempt += 1) {
    try {
      return await computeOraclePrices(tokens);
    } catch (error) {
      if (attempt >= PRICING_ATTEMPTS) throw error;
      log(`    [prices] ${label}: ${error.message ?? error}; retrying in ${(attempt * PRICING_BACKOFF_MS) / 1000}s`);
      await new Promise((r) => setTimeout(r, attempt * PRICING_BACKOFF_MS));
    }
  }
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

// The program checks every swap's fill against the posted price ± the execution bound. Check
// each built leg at its worst allowed fill (Jupiter's slippage threshold) the same way, before
// any swap runs, so a leg that could never pass cancels the intent cleanly instead of failing
// after other legs have traded.
function assertLegsWithinOracle(entries, prices, side) {
  const bound = BigInt(Math.min(MAX_FIXED_WEIGHT_EXECUTION_SLIPPAGE_BPS, SLIPPAGE_BPS + VERIFY_ORACLE_BUFFER_BPS));
  for (const b of entries) {
    const c = b.component;
    const posted = prices.get(c.mint.toBase58());
    if (!posted) throw new RebalanceBuildError(`no posted price for component ${c.globalIndex}`);
    const worst = (BigInt(b.entry.quoteLimit.toString()) * pow10Big(c.decimals) * PRICE_SCALE) / (BigInt(b.atoms) * pow10Big(USDC_DECIMALS));
    const ok = side === "sell"
      ? worst * BigInt(BPS) >= posted.scaled * (BigInt(BPS) - bound)
      : worst * BigInt(BPS) <= posted.scaled * (BigInt(BPS) + bound);
    if (!ok) {
      const usd = (x) => (Number(x) / Number(PRICE_SCALE)).toPrecision(6);
      throw new RebalanceBuildError(
        `component ${c.globalIndex} ${side} could fill at $${usd(worst)}, outside ${bpsPct(Number(bound))} of the posted $${usd(posted.scaled)}`,
      );
    }
  }
}

function postPricesIx(program, oracle, entries) {
  return program.methods
    .postPrices({ prices: entries.map(([mint, p]) => ({ mint: new PublicKey(mint), price: new anchor.BN(p.scaled.toString()) })) })
    .accounts({ oracle, priceBoard: PRICE_BOARD })
    .instruction();
}

// Prices `components` and posts them to the price board, in as few transactions as fit.
// Returns the posted prices (mint -> { scaled, usd, ... }).
async function postPrices(env, components, label) {
  const { connection, program, payer, oracle } = env;
  const tokens = pricedTokens(components);
  if (!tokens.length) return new Map();
  const prices = await computeOraclePricesPatiently(tokens, label);
  log(`    [prices] ${label}: ${[...prices].map(([mint, p]) => `${mint.slice(0, 6)}.. $${p.usd.toPrecision(6)}`).join(", ")}`);
  const entries = [...prices];
  for (let i = 0; i < entries.length;) {
    let n = Math.min(MAX_PRICES_PER_POST, entries.length - i);
    let ixs;
    for (;; n -= 1) {
      ixs = [...budgetIxs(60_000), await postPricesIx(program, oracle.publicKey, entries.slice(i, i + n))];
      if (n === 1 || compiledSize(payer, ixs) <= TX_LIMIT) break;
    }
    await sendV0(connection, payer, ixs, `${label}: post ${n} price(s)`, [], [oracle]);
    i += n;
  }
  return prices;
}

// --- transaction send --------------------------------------------------------

function budgetIxs(units) {
  return [
    ComputeBudgetProgram.setComputeUnitLimit({ units }),
    ComputeBudgetProgram.setComputeUnitPrice({ microLamports: PRIORITY_FEE_MICROLAMPORTS }),
  ];
}

function compiledSize(payer, instructions, lookupTables = []) {
  const message = new TransactionMessage({
    payerKey: payer.publicKey,
    recentBlockhash: PublicKey.default.toBase58(),
    instructions,
  }).compileToV0Message(lookupTables);
  try {
    return new VersionedTransaction(message).serialize().length;
  } catch (error) {
    // web3.js uses fixed serialization buffers; an oversized candidate must split.
    if (error instanceof RangeError && /encoding overruns Uint8Array|offset is outside the bounds/i.test(error.message)) return Infinity;
    throw error;
  }
}

async function sendV0(connection, payer, instructions, label, lookupTables = [], signers = []) {
  const size = compiledSize(payer, instructions, lookupTables);
  if (size > TX_LIMIT) {
    throw new RebalanceBuildError(`${label}: compiled tx is ${size} > ${TX_LIMIT} bytes`);
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

// Post fresh prices for `components`, then send the step that reads them, retrying the PAIR
// if the step reverts on an aged-out price (the two are separate txs, so a congested gap can
// lapse the 150-slot freshness window) or expires without landing. Returns the posted prices.
async function sendWithFreshPrices(env, components, label, buildConsumerIx, cuLimit, attempts = 2, lookupTables = []) {
  const { connection, program, payer } = env;
  let lastError;
  for (let attempt = 1; attempt <= attempts; attempt += 1) {
    const prices = await postPrices(env, components, label);
    const ix = await buildConsumerIx();
    try {
      await sendV0(connection, payer, [...budgetIxs(cuLimit), ix], label, lookupTables);
      return prices;
    } catch (error) {
      lastError = error;
      if (attempt < attempts && (isStalePriceError(program, error) || isExpiredError(error))) {
        log(`    ${label} failed (${isExpiredError(error) ? "expired" : "price aged out"}), retrying with fresh prices`);
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

async function jupiterQuote({ inputMint, outputMint, amount, swapMode }) {
  const url =
    `${JUPITER_SWAP_API}/quote?inputMint=${inputMint.toBase58()}` +
    `&outputMint=${outputMint.toBase58()}&amount=${amount.toString()}` +
    `&swapMode=${swapMode}&slippageBps=${SLIPPAGE_BPS}&onlyDirectRoutes=false` +
    `&restrictIntermediateTokens=true&maxAccounts=16`;
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
async function buildLegEntry(connection, ctx, component, legAtoms, side) {
  const isSell = side === "sell";
  const quote = await jupiterQuote({
    inputMint: isSell ? component.mint : USDC_MINT,
    outputMint: isSell ? USDC_MINT : component.mint,
    amount: legAtoms,
    swapMode: isSell ? "ExactIn" : "ExactOut",
  });
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
  // the slippage-adjusted bound for the chosen swap mode.
  const quoteLimit = new anchor.BN(quote.otherAmountThreshold);
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
    atoms: BigInt(legAtoms),
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
      priceBoard: PRICE_BOARD,
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

// Greedily pack entries into batches that fit BOTH the per-batch swap cap and the 1232-byte
// tx limit. Throws (RebalanceBuildError) if a single entry can't fit alone.
async function packBatches(connection, program, ctx, method, entries) {
  const batches = [];
  let cur = [];
  const fits = async (group) => {
    const ix = await executeBatchIx(program, ctx, method, group);
    const tables = dedupeTables(group.flatMap((b) => b.lookupTables));
    const size = compiledSize({ publicKey: ctx.keeper }, [...budgetIxs(1_400_000), ix], tables);
    return size <= TX_LIMIT;
  };
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
    const tables = dedupeTables(batch.flatMap((b) => b.lookupTables));
    await sendWithFreshPrices(env, batch.map((b) => b.component),
      `${method} batch ${i + 1}/${batches.length}`,
      () => executeBatchIx(env.program, ctx, method, batch),
      batchCuLimit(batch.length), 2, tables);
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

async function openIntent(env, ctx, components, pagePdas, driftThresholdBps, nowOnChain) {
  const { connection, program, payer } = env;
  const nonce = new anchor.BN(Date.now());
  const intent = intentPda(ctx.index, nonce);

  let maxPost = 100;
  if (driftThresholdBps > 0) maxPost = Math.min(maxPost, driftThresholdBps - 1);
  maxPost = Math.max(
    MIN_FIXED_WEIGHT_POST_REBALANCE_DRIFT_BPS,
    Math.min(MAX_FIXED_WEIGHT_POST_REBALANCE_DRIFT_BPS, maxPost),
  );

  const remaining = [
    ...pagePdas.map((p) => ({ pubkey: p, isSigner: false, isWritable: false })),
    ...components.map((c) => ({ pubkey: c.vault, isSigner: false, isWritable: false })),
  ];
  const prices = await sendWithFreshPrices(
    env,
    components,
    "open rebalance intent",
    () =>
      program.methods
        .openRebalanceIntent({
          nonce,
          expiresAt: new anchor.BN(nowOnChain + INTENT_TTL_S),
          maxPriceAgeSlots: new anchor.BN(MAX_PRICE_AGE_SLOTS),
          navToleranceBps: NAV_TOLERANCE_BPS,
          maxPostRebalanceDriftBps: maxPost,
        })
        .accounts({
          initiator: payer.publicKey,
          index: ctx.index,
          indexMint: ctx.indexMint,
          vaultAuthority: ctx.vaultAuthority,
          quoteMint: USDC_MINT,
          vaultQuoteTokenAccount: ctx.vaultQuote,
          intent,
          priceBoard: PRICE_BOARD,
          associatedTokenProgram: ASSOCIATED_TOKEN_PROGRAM_ID,
          quoteTokenProgram: TOKEN_PROGRAM_ID,
          systemProgram: SystemProgram.programId,
        })
        .remainingAccounts(remaining)
        .instruction(),
    400_000,
  );
  return { intent, prices };
}

async function finalize(env, ctx, components, pagePdas) {
  const remaining = [
    ...pagePdas.map((p) => ({ pubkey: p, isSigner: false, isWritable: true })),
    ...components.map((c) => ({ pubkey: c.vault, isSigner: false, isWritable: false })),
  ];
  await sendWithFreshPrices(
    env,
    components,
    "finalize rebalance",
    () =>
      env.program.methods
        .finalizeRebalance({ maxPriceAgeSlots: new anchor.BN(MAX_PRICE_AGE_SLOTS) })
        .accounts({
          keeper: ctx.keeper,
          index: ctx.index,
          intent: ctx.intent,
          vaultAuthority: ctx.vaultAuthority,
          quoteMint: USDC_MINT,
          vaultQuoteTokenAccount: ctx.vaultQuote,
          priceBoard: PRICE_BOARD,
          quoteTokenProgram: TOKEN_PROGRAM_ID,
        })
        .remainingAccounts(remaining)
        .instruction(),
    600_000,
  );
}

async function cancelIntent(connection, program, payer, ctx, intentPk) {
  const ix = await program.methods
    .cancelRebalance()
    .accounts({ initiator: payer.publicKey, index: ctx.index, intent: intentPk })
    .instruction();
  await sendV0(connection, payer, [...budgetIxs(100_000), ix], "cancel rebalance intent");
}

async function closeIntent(connection, program, payer, intentPk) {
  try {
    const ix = await program.methods
      .closeRebalanceIntent()
      .accounts({ initiator: payer.publicKey, intent: intentPk })
      .instruction();
    await sendV0(connection, payer, [...budgetIxs(50_000), ix], "close rebalance intent (rent)");
  } catch (error) {
    log(`    [warn] could not close intent (rent left behind): ${error.message ?? error}`);
  }
}

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
  await sendV0(connection, payer, [...budgetIxs(400_000), ix], "unwind rebalance", []);
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

async function previewSwaps(connection, program, payer, ctx, legs) {
  const previewIntent = intentPda(ctx.index, new anchor.BN(Date.now()));
  const pctx = { ...ctx, intent: previewIntent };
  for (const side of ["sell", "buy"]) {
    const sideLegs = legs.filter((l) => l.side === side);
    if (!sideLegs.length) continue;
    const method = side === "sell" ? "executeRebalanceSellBatch" : "executeRebalanceBuyBatch";
    const entries = [];
    for (const l of sideLegs) {
      try {
        const built = await buildLegEntry(connection, pctx, l.component, l.leg, side);
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
        const tables = dedupeTables(batches[i].flatMap((b) => b.lookupTables));
        const size = compiledSize(payer, [...budgetIxs(batchCuLimit(batches[i].length)), ix], tables);
        log(`    [preview] ${method} batch ${i + 1}/${batches.length}: ${batches[i].length} legs, ${size} tx bytes, ${tables.length} ALT(s)`);
      }
    } catch (error) {
      log(`    [preview] ${method} packing failed: ${error.message ?? error}`);
    }
  }
}

// --- oracle readiness ----------------------------------------------------------

// Why this bot cannot post prices (no key, no board, or a key the board doesn't accept).
async function oracleProblems(env) {
  if (!env.oracle) return `no oracle key: set ORACLE_KEYPAIR or ${ORACLE_WALLET_PATH}`;
  const board = await env.program.account.priceBoard.fetchNullable(PRICE_BOARD);
  if (!board) return "the price board does not exist yet (scripts/set-price-oracle.mjs)";
  if (!board.oracle.equals(env.oracle.publicKey)) {
    return `the price board accepts ${board.oracle.toBase58()}, not this oracle key ${env.oracle.publicKey.toBase58()}`;
  }
  return null;
}

// Dry-run: the prices a rebalance would post now, next to Jupiter's price API.
async function showPrices(components, jupiterPrices) {
  try {
    const prices = await computeOraclePrices(pricedTokens(components));
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
    if (!expired && !isAuthority) {
      return void log("  skip: rebalance in progress, not expired, keeper is not the index authority");
    }
    log(`  ${EXECUTE ? "UNWINDING" : "[dry-run] would unwind"} stuck intent to release the lock`);
    if (EXECUTE) {
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
  // recomputation from the posted prices is likely to agree, and only when all prices are known.
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
  if (SHOW_PRICES && !EXECUTE) await showPrices(components, prices);
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
      await previewSwaps(connection, program, payer, ctx, legs).catch((e) => log(`  [preview] failed: ${e.message ?? e}`));
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
  // Every rebalance step needs posted prices; don't hold users back without them.
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
  log("  opening rebalance intent...");
  const { intent: intentPk, prices } = await openIntent(env, ctx, components, pagePdas, driftThresholdBps, nowOnChain);
  ctx.intent = intentPk;

  // Read the program's authoritative legs (it recomputes them from the posted prices).
  const intent = await program.account.rebalanceIntent.fetch(intentPk);
  const targets = intent.componentTargetAmounts.map((bn) => BigInt(bn.toString()));
  const sellComponents = components.filter((c) => bitmapGet(intent.sellLegBitmap, c.globalIndex));
  const buyComponents = components.filter((c) => bitmapGet(intent.buyLegBitmap, c.globalIndex));
  log(`  intent open: ${sellComponents.length} sell, ${buyComponents.length} buy legs`);

  // Build + validate EVERY batch (Jupiter quote, route-scope, tx-size packing) BEFORE sending
  // any execute tx. If anything fails here, no leg has executed yet, so we cancel cleanly
  // instead of stranding the intent (which would hold the lock until expiry).
  let sellBatches;
  let buyBatches;
  try {
    const sellEntries = [];
    for (const c of sellComponents) sellEntries.push(await buildLegEntry(connection, ctx, c, targets[c.globalIndex], "sell"));
    const scratch = parseTokenAmount(await connection.getAccountInfo(ctx.vaultQuote, "confirmed"));
    const budget = scratch + sellEntries.reduce((sum, b) => sum + BigInt(b.entry.quoteLimit.toString()), 0n);
    const buyEntries = await affordableBuys(buyComponents, targets,
      intent.componentOpenAmounts.map((bn) => BigInt(bn.toString())),
      Number(intent.navToleranceBps), budget,
      (c, atoms) => buildLegEntry(connection, ctx, c, atoms, "buy"));
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
  const oracle = loadOracleKeypair(ORACLE_WALLET_PATH);
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
        oracle: oracle?.publicKey.toBase58() ?? null,
        priceBoard: PRICE_BOARD.toBase58(),
        oraclePricing: ORACLE_CONFIG,
      },
      null,
      2,
    ),
  );

  const env = { connection, program, payer, oracle };
  if (EXECUTE) {
    const problem = await oracleProblems(env).catch((e) => `could not read the price board: ${e.message ?? e}`);
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

export { compiledSize, PROGRAM_ID, PRICE_BOARD, sendV0, executeBatchIx, postPricesIx, isStalePriceError, assertLegsWithinOracle, affordableBuys, buyAmount, settleExpiredIntents };
if (process.argv[1] && import.meta.url === pathToFileURL(path.resolve(process.argv[1])).href) {
  await main();
}
