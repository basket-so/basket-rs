// Fixed-weight rebalance worker.
//
// Detects when a FixedWeights basket has drifted past its configured threshold (or its
// time interval has elapsed) and drives the on-chain rebalance intent state machine:
//   open_rebalance_intent
//     -> execute_rebalance_sell_batch ×N   (component -> USDC, Jupiter, ExactIn)
//     -> execute_rebalance_buy_batch  ×N   (USDC -> component, Jupiter, ExactOut)
//     -> verify_rebalance_component_price ×(legs)  (deferred per-leg oracle bound)
//     -> finalize_rebalance
//     -> close_rebalance_intent             (reclaim the intent rent)
// and, when it finds a stuck/expired intent, unwind_rebalance to release the lock.
//
// SAFETY: dry-run by default. It reads chain state, fetches prices/quotes, builds the
// full plan (and, with --preview-swaps, the real Jupiter swap encodings) and prints it,
// but sends NOTHING unless you pass --execute. Sending real transactions spends the
// keeper's funds (swap fees + slippage) and moves basket assets.
//
// To avoid stranding an open intent, the --execute path builds and VALIDATES every swap
// batch (tx size + Jupiter route account scope) BEFORE sending any execute tx; if a leg
// can't be built/sized/scoped it cancels the just-opened intent (no legs executed yet)
// instead of getting stuck mid-rebalance.
//
//   node scripts/rebalance-bot.mjs                 # detect-only, scan all FixedWeights baskets
//   node scripts/rebalance-bot.mjs --preview-swaps # dry-run + encode the Jupiter swaps read-only
//   node scripts/rebalance-bot.mjs --index <pk>    # only this index (skip discovery)
//   node scripts/rebalance-bot.mjs --execute       # actually rebalance triggered baskets
//   node scripts/rebalance-bot.mjs --watch         # loop forever (poll every --interval s)
//
// Env: SOLANA_RPC_URL (default mainnet-beta; discovery needs a getProgramAccounts-capable RPC),
//      ANCHOR_WALLET (default deployer-keypair.json), JUPITER_SWAP_API, JUPITER_PRICE_API.
//
// NOTE: build the program (anchor build, regenerates target/idl/basket.json) and deploy it
// before the --execute path works against a real basket.

import anchor from "@coral-xyz/anchor";
import {
  ComputeBudgetProgram,
  Connection,
  Keypair,
  PublicKey,
  SystemProgram,
  SYSVAR_CLOCK_PUBKEY,
  SYSVAR_INSTRUCTIONS_PUBKEY,
  SYSVAR_SLOT_HASHES_PUBKEY,
  TransactionMessage,
  VersionedTransaction,
} from "@solana/web3.js";
import {
  ASSOCIATED_TOKEN_PROGRAM_ID,
  TOKEN_2022_PROGRAM_ID,
  TOKEN_PROGRAM_ID,
  getAssociatedTokenAddressSync,
} from "@solana/spl-token";
import { CrossbarClient, CrossbarNetwork } from "@switchboard-xyz/common";
import { OracleQuote, getDefaultQueue } from "@switchboard-xyz/on-demand";
import fs from "fs";
import path from "path";

// --- constants ---------------------------------------------------------------

const PROGRAM_ID = new PublicKey("9LNEoShrH93XekWQTFmZBdUdMu8ugJxBr5cbfqJQC1mw");
const USDC_MINT = new PublicKey("EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v");
const JUPITER_V6 = new PublicKey("JUP6LkbZbjS1jKKwapdHNy74zcZ3tLUZoi5QNyVTaV4");

const VAULT_AUTHORITY_SEED = Buffer.from("vault-authority");
const REBALANCE_INTENT_SEED = Buffer.from("rebalance-intent");
const PAGE_SEED = Buffer.from("large-basket-component-page");

const BPS = 10_000;
const TX_LIMIT = 1232; // Solana packet MTU; a compiled tx must not exceed this.
const USDC_DECIMALS = 6;
// Mirror of the program's constants (programs/basket/src/constants.rs).
const SWITCHBOARD_MAX_AGE_SLOTS = 150;
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
const WATCH = flags.has("--watch");
const WATCH_INTERVAL_S = Number(flagValue("--interval", "60"));
const ONLY_INDEX = flagValue("--index", null);
const BATCH_SIZE = Math.max(
  1,
  Math.min(MAX_REBALANCE_SWAPS_PER_BATCH, Number(flagValue("--batch-size", "2"))),
);
const SLIPPAGE_BPS = Number(flagValue("--slippage-bps", "100"));
// The deferred per-leg verify bounds the realized fill vs a FRESH oracle. That budget must
// cover the execution slippage already allowed PLUS the Jupiter-mid-vs-oracle basis and any
// intra-window oracle drift — so it is execution slippage + a buffer, NOT just the slippage.
const VERIFY_ORACLE_BUFFER_BPS = Number(flagValue("--verify-buffer-bps", "300"));
const PRIORITY_FEE_MICROLAMPORTS = Number(flagValue("--priority-fee", "5000"));
const NAV_TOLERANCE_BPS = Math.max(
  MIN_KEEPER_NAV_TOLERANCE_BPS,
  Math.min(MAX_KEEPER_NAV_TOLERANCE_BPS, Number(flagValue("--nav-tolerance-bps", "50"))),
);
// Open only when Jupiter-estimated drift exceeds the threshold by this margin, reducing
// opens the program then rejects (it re-decides from Switchboard, which can straddle).
const DRIFT_TRIGGER_MARGIN_BPS = Number(flagValue("--drift-margin-bps", "0"));
// Keep the intent TTL safely below the program's hard 1800s cap so wall-clock-vs-chain skew
// can't push expires_at over the cap and revert the open.
const TTL_SKEW_MARGIN_S = 90;
const INTENT_TTL_S = Math.max(
  60,
  Math.min(MAX_INTENT_TTL_SECONDS - TTL_SKEW_MARGIN_S, Number(flagValue("--ttl", "600"))),
);

const RPC_URL = process.env.SOLANA_RPC_URL ?? "https://api.mainnet-beta.solana.com";
const WALLET_PATH = process.env.ANCHOR_WALLET ?? "deployer-keypair.json";
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

function feedIdHexFromOraclePair(oraclePair) {
  return `0x${oraclePair.toBuffer().toString("hex")}`;
}

function bitmapGet(bytes, index) {
  const byte = bytes[index >> 3] ?? 0;
  return (byte & (1 << (index & 7))) !== 0;
}

function pow10Big(decimals) {
  return 10n ** BigInt(decimals);
}

function isSwitchboardStaleError(error) {
  return /switchboard|verification|stale|quote/i.test(String(error?.message ?? error));
}

// --- chain context -----------------------------------------------------------

function loadKeypair(filePath) {
  return Keypair.fromSecretKey(Uint8Array.from(readJson(filePath)));
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
      components.push({
        globalIndex: components.length,
        mint: c.mint,
        unitsPerIndex: BigInt(c.unitsPerIndex.toString()),
        targetWeightBps: c.targetWeightBps,
        oraclePair: c.oraclePair,
        tokenProgram: c.tokenProgram,
        vault: c.vault,
        decimals: c.decimals,
        pagePda: pda,
        pageIndex: page.pageIndex,
        isQuote: c.mint.equals(USDC_MINT),
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
// (driftKnown=false) rather than aborting — the program prices from Switchboard anyway.
function assessBasket(components, vaultAmounts, scratchAtoms, prices) {
  const hasQuoteComponent = components.some((c) => c.isQuote);
  const missing = components.filter((c) => !c.isQuote && !prices.get(c.mint.toBase58()));
  const driftKnown = missing.length === 0;

  // NAV in micro-USD (integer) using available prices; missing components contribute 0.
  const microUsd = (atoms, decimals, usd) =>
    (BigInt(atoms) * BigInt(Math.round(usd * 1e6))) / pow10Big(decimals);
  let navMicro = 0n;
  const priced = components.map((c) => {
    const amount = vaultAmounts.get(c.vault.toBase58()) ?? 0n;
    const usd = c.isQuote ? 1 : prices.get(c.mint.toBase58());
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

// --- Switchboard managed quote ----------------------------------------------

let cachedQueue = null;
async function getQueue(connection) {
  if (!cachedQueue) cachedQueue = await getDefaultQueue(connection.rpcEndpoint);
  return cachedQueue;
}

async function buildManagedUpdate(connection, crossbar, payer, feedIds) {
  const queue = await getQueue(connection);
  const [quoteAccount] = OracleQuote.getCanonicalPubkey(queue.pubkey, feedIds);
  const instructions = await queue.fetchManagedUpdateIxs(crossbar, feedIds, {
    payer: payer.publicKey,
    numSignatures: 1,
    instructionIdx: 0,
  });
  return { queue, quoteAccount, instructions };
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
  return new VersionedTransaction(message).serialize().length;
}

async function sendV0(connection, payer, instructions, label, lookupTables = []) {
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
  tx.sign([payer]);
  log(`    [send] ${label} (${size} bytes)`);
  const sig = await connection.sendTransaction(tx, {
    skipPreflight: false,
    preflightCommitment: "confirmed",
    maxRetries: 5,
  });
  await connection.confirmTransaction(
    { signature: sig, blockhash: latest.blockhash, lastValidBlockHeight: latest.lastValidBlockHeight },
    "confirmed",
  );
  log(`    [ok] ${label}: ${sig}`);
  return sig;
}

// Send a managed Switchboard update then the consuming program ix, retrying the PAIR if the
// consumer reverts on a stale quote (the two are separate txs, so a congested gap can lapse
// the 150-slot freshness window).
async function sendWithFreshQuote(env, feedIds, label, buildConsumerIx, cuLimit, attempts = 2) {
  const { connection, crossbar, payer } = env;
  let lastError;
  for (let attempt = 1; attempt <= attempts; attempt += 1) {
    const update = await buildManagedUpdate(connection, crossbar, payer, feedIds);
    await sendV0(connection, payer, update.instructions, `${label}: switchboard update`);
    const ix = await buildConsumerIx(update);
    try {
      return await sendV0(connection, payer, [...budgetIxs(cuLimit), ix], label);
    } catch (error) {
      lastError = error;
      if (attempt < attempts && isSwitchboardStaleError(error)) {
        log(`    ${label} reverted (stale quote?), retrying with a fresh quote`);
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
    `&restrictIntermediateTokens=true`;
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
  return program.methods[method]({ entries: batch.map((b) => b.entry) })
    .accounts({
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
async function packBatches(program, ctx, method, entries) {
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

async function sendBatches(connection, program, payer, ctx, method, batches) {
  for (let i = 0; i < batches.length; i += 1) {
    const batch = batches[i];
    const ix = await executeBatchIx(program, ctx, method, batch);
    const tables = dedupeTables(batch.flatMap((b) => b.lookupTables));
    await sendV0(
      connection,
      payer,
      [...budgetIxs(batchCuLimit(batch.length)), ix],
      `${method} batch ${i + 1}/${batches.length} (${batch.length} legs)`,
      tables,
    );
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

  const feedIds = components.filter((c) => !c.isQuote).map((c) => feedIdHexFromOraclePair(c.oraclePair));
  const remaining = [
    ...pagePdas.map((p) => ({ pubkey: p, isSigner: false, isWritable: false })),
    ...components.map((c) => ({ pubkey: c.vault, isSigner: false, isWritable: false })),
  ];
  await sendWithFreshQuote(
    env,
    feedIds,
    "open rebalance intent",
    (update) =>
      program.methods
        .openRebalanceIntent({
          nonce,
          expiresAt: new anchor.BN(nowOnChain + INTENT_TTL_S),
          switchboardMaxAgeSlots: new anchor.BN(SWITCHBOARD_MAX_AGE_SLOTS),
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
          switchboardQueue: update.queue.pubkey,
          switchboardQuote: update.quoteAccount,
          slothashes: SYSVAR_SLOT_HASHES_PUBKEY,
          instructionsSysvar: SYSVAR_INSTRUCTIONS_PUBKEY,
          associatedTokenProgram: ASSOCIATED_TOKEN_PROGRAM_ID,
          quoteTokenProgram: TOKEN_PROGRAM_ID,
          systemProgram: SystemProgram.programId,
        })
        .remainingAccounts(remaining)
        .instruction(),
    400_000,
  );
  return intent;
}

async function verifyLeg(env, ctx, component) {
  const feedId = feedIdHexFromOraclePair(component.oraclePair);
  const maxOracleSlippageBps = Math.min(
    MAX_FIXED_WEIGHT_EXECUTION_SLIPPAGE_BPS,
    SLIPPAGE_BPS + VERIFY_ORACLE_BUFFER_BPS,
  );
  await sendWithFreshQuote(
    env,
    [feedId],
    `verify component ${component.globalIndex}`,
    (update) =>
      env.program.methods
        .verifyRebalanceComponentPrice({
          componentIndex: component.globalIndex,
          maxOracleSlippageBps,
          switchboardMaxAgeSlots: new anchor.BN(SWITCHBOARD_MAX_AGE_SLOTS),
        })
        .accounts({
          keeper: ctx.keeper,
          index: ctx.index,
          intent: ctx.intent,
          componentPage: component.pagePda,
          switchboardQueue: update.queue.pubkey,
          switchboardQuote: update.quoteAccount,
          slothashes: SYSVAR_SLOT_HASHES_PUBKEY,
          instructionsSysvar: SYSVAR_INSTRUCTIONS_PUBKEY,
        })
        .instruction(),
    200_000,
  );
}

async function finalize(env, ctx, components, pagePdas) {
  const feedIds = components.filter((c) => !c.isQuote).map((c) => feedIdHexFromOraclePair(c.oraclePair));
  const remaining = [
    ...pagePdas.map((p) => ({ pubkey: p, isSigner: false, isWritable: true })),
    ...components.map((c) => ({ pubkey: c.vault, isSigner: false, isWritable: false })),
  ];
  await sendWithFreshQuote(
    env,
    feedIds,
    "finalize rebalance",
    (update) =>
      env.program.methods
        .finalizeRebalance({ switchboardMaxAgeSlots: new anchor.BN(SWITCHBOARD_MAX_AGE_SLOTS) })
        .accounts({
          keeper: ctx.keeper,
          index: ctx.index,
          intent: ctx.intent,
          vaultAuthority: ctx.vaultAuthority,
          quoteMint: USDC_MINT,
          vaultQuoteTokenAccount: ctx.vaultQuote,
          switchboardQueue: update.queue.pubkey,
          switchboardQuote: update.quoteAccount,
          slothashes: SYSVAR_SLOT_HASHES_PUBKEY,
          instructionsSysvar: SYSVAR_INSTRUCTIONS_PUBKEY,
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
      const batches = await packBatches(program, pctx, method, entries);
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

// --- per-index processing ----------------------------------------------------

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
  const { components, pagePdas } = await loadComponents(program, indexPk, pageCount);
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

  if (indexState.rebalancingPaused) return void log("  skip: rebalancing is paused for this index");

  const nonQuote = components.filter((c) => !c.isQuote).map((c) => c.mint);
  const prices = await fetchPrices(nonQuote);
  const vaultInfos = await connection.getMultipleAccountsInfo(
    [...components.map((c) => c.vault), ctx.vaultQuote],
    "confirmed",
  );
  const vaultAmounts = new Map();
  components.forEach((c, i) => vaultAmounts.set(c.vault.toBase58(), parseTokenAmount(vaultInfos[i])));
  const scratchAtoms = parseTokenAmount(vaultInfos[components.length]);

  const a = assessBasket(components, vaultAmounts, scratchAtoms, prices);
  const driftThresholdBps = indexState.fixedWeightDriftThresholdBps;
  const intervalS = Number(indexState.fixedWeightRebalanceIntervalSeconds);
  const lastRebalancedAt = Number(indexState.fixedWeightLastRebalancedAt);

  // Time trigger is exact (on-chain clock + on-chain last_rebalanced_at), independent of prices.
  const timeTriggered = intervalS > 0 && now >= lastRebalancedAt + intervalS;
  // Drift trigger is an estimate from Jupiter prices; require a margin so the program's
  // Switchboard recomputation is likely to agree, and only when all prices are known.
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

  if (!driftTriggered && !timeTriggered) return void log("  -> no rebalance needed");
  log(`  -> rebalance TRIGGERED (drift=${driftTriggered}, time=${timeTriggered})`);

  if (!EXECUTE) {
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

  await executeRebalance(env, ctx, components, pagePdas, driftThresholdBps, now);
}

async function executeRebalance(env, ctx, components, pagePdas, driftThresholdBps, nowOnChain) {
  const { connection, program, payer } = env;
  log("  opening rebalance intent...");
  const intentPk = await openIntent(env, ctx, components, pagePdas, driftThresholdBps, nowOnChain);
  ctx.intent = intentPk;

  // Read the program's authoritative legs (it recomputes them from its own Switchboard quote).
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
    const buyEntries = [];
    for (const c of buyComponents) buyEntries.push(await buildLegEntry(connection, ctx, c, targets[c.globalIndex], "buy"));
    sellBatches = await packBatches(program, ctx, "executeRebalanceSellBatch", sellEntries);
    buyBatches = await packBatches(program, ctx, "executeRebalanceBuyBatch", buyEntries);
  } catch (error) {
    log(`  build/validation failed (${error.message ?? error}); cancelling the un-executed intent`);
    await cancelIntent(connection, program, payer, ctx, intentPk).catch((e) =>
      log(`  [warn] cancel failed, intent will need unwind after expiry: ${e.message ?? e}`),
    );
    await closeIntent(connection, program, payer, intentPk);
    throw error;
  }

  log("  sending sell batches...");
  await sendBatches(connection, program, payer, ctx, "executeRebalanceSellBatch", sellBatches);
  log("  sending buy batches...");
  await sendBatches(connection, program, payer, ctx, "executeRebalanceBuyBatch", buyBatches);

  log("  verifying legs...");
  for (const c of [...sellComponents, ...buyComponents]) await verifyLeg(env, ctx, c);

  log("  finalizing...");
  await finalize(env, ctx, components, pagePdas);
  await closeIntent(connection, program, payer, intentPk);
  log(`  DONE: rebalanced ${ctx.index.toBase58()}`);
}

// --- main --------------------------------------------------------------------

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
  const program = loadProgram(connection, payer);
  const crossbar = CrossbarClient.default();
  crossbar.setNetwork(CrossbarNetwork.SolanaMainnet);

  if (!program.programId.equals(PROGRAM_ID)) {
    throw new Error(`IDL program id ${program.programId.toBase58()} != ${PROGRAM_ID.toBase58()}`);
  }

  log(
    JSON.stringify(
      {
        mode: EXECUTE ? "EXECUTE (will send transactions)" : "dry-run (read-only)",
        rpc: RPC_URL,
        keeper: payer.publicKey.toBase58(),
        watch: WATCH,
        batchSize: BATCH_SIZE,
        slippageBps: SLIPPAGE_BPS,
        navToleranceBps: NAV_TOLERANCE_BPS,
        verifyOracleBps: Math.min(MAX_FIXED_WEIGHT_EXECUTION_SLIPPAGE_BPS, SLIPPAGE_BPS + VERIFY_ORACLE_BUFFER_BPS),
        ttlSeconds: INTENT_TTL_S,
      },
      null,
      2,
    ),
  );

  const env = { connection, program, crossbar, payer };
  if (WATCH) {
    // eslint-disable-next-line no-constant-condition
    while (true) {
      await runOnce(env).catch((e) => log(`run error: ${e.message ?? e}`));
      log(`\nsleeping ${WATCH_INTERVAL_S}s...\n`);
      await new Promise((r) => setTimeout(r, WATCH_INTERVAL_S * 1000));
    }
  } else {
    await runOnce(env);
  }
}

await main();
