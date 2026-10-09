// Price oracle service: the only place the oracle key lives.
//
// The rebalance keeper used to hold the oracle key next to its own, so whoever controlled the
// keeper's host could both set prices and trade against them. This service runs as its own Fly
// app (fly.oracle.toml) with its own key. Before each rebalance step the keeper asks it to sign
// prices for some mints of one rebalance intent; the service prices them itself
// (scripts/lib/price-oracle.mjs), maps each mint to its component index from the basket's pages
// on chain, and signs a price message for that intent (scripts/lib/signed-prices.mjs). The
// keeper sends the signature in an Ed25519 instruction right before the step, and the program
// checks it. The keeper chooses which mints and when, never the prices. The service sends no
// transactions and needs no SOL.
//
// Whatever the caller asks, the service:
//   - signs only for a FixedWeights basket, and only mints that basket holds;
//   - signs at most --max-signatures-per-hour messages;
//   - refuses a price more than --max-move-bps from the first price it signed for the same
//     intent (which pins a rebalance to its opening prices), or otherwise from the last price it
//     signed for that mint within --move-window-s, so a rebalance can't be walked along a
//     pushed pool;
//   - stamps prices with the slot read before pricing, and refuses to sign when pricing took
//     over half the program's price age window, so the age the program checks is the prices'.
//
// Until the program's price oracle account names this service's key
// (scripts/set-price-oracle.mjs --oracle <key>) it stands by: /health says so, dry runs and the
// periodic self-check work, signing is refused.
//
// HTTP, on Fly's private network only (fly.oracle.toml gives the app no public address):
//   GET  /health                                          status, no auth
//   POST /sign {"index", "nonce", "mints": [...], "dryRun"}  Authorization: Bearer $ORACLE_API_TOKEN
//     -> { slot, intent, message, signature (both base64), oracle, prices }
//
//   node scripts/oracle-service.mjs [--port 8080] [--max-signatures-per-hour 120]
//     [--max-move-bps 1500] [--move-window-s 900] [--self-check-interval-s 3600]
//     [--price-probe-usd 50] [--price-max-spread-bps 400] [--price-max-deviation-bps 300]
//
// Env: ORACLE_KEYPAIR (secret key JSON array) or ORACLE_WALLET (path to one), ORACLE_API_TOKEN
//      (32+ characters, shared with the keeper), SOLANA_RPC_URL (the keeper's provider, so the
//      slots the two read agree; must allow getProgramAccounts for the self-check),
//      JUPITER_API_KEY (optional: Jupiter's keyed API, api.jup.ag, instead of the keyless one),
//      JUPITER_SWAP_API, JUPITER_PRICE_API.

import crypto from "node:crypto";
import fs from "node:fs";
import http from "node:http";
import path from "node:path";
import { pathToFileURL } from "node:url";
import anchor from "@coral-xyz/anchor";
import { Connection, Keypair, PublicKey } from "@solana/web3.js";
import { OraclePriceError, USDC_MINT, fetchJson, jupiterApiKey, jupiterApis, oraclePrices } from "./lib/price-oracle.mjs";
import {
  MAX_PRICE_AGE_SLOTS,
  encodePriceMessage,
  priceOracleAddress,
  rebalanceIntentAddress,
  signPriceMessage,
} from "./lib/signed-prices.mjs";

const PROGRAM_ID = new PublicKey("bskthjNMRWQ4ekDLxaAzA1e39ThPmEtUgHY3XHfs7qv");
const PRICE_ORACLE = priceOracleAddress(PROGRAM_ID);
// Mirrors of the program's layout (programs/basket/src).
const INDEX_KIND_OFFSET = 172;
const INDEX_KIND_FIXED_WEIGHTS = 1;
// The most components a basket can hold (MAX_LARGE_BASKET_COMPONENTS in constants.rs), so a
// request can always name every token of the widest basket. The service only signs mints the
// basket holds, so this bounds nothing a basket doesn't.
const MAX_MINTS_PER_REQUEST = 50;
const MAX_BODY_BYTES = 16_384;
// Signed prices are kept per intent this long (past the program's 30-minute intent cap).
const INTENT_MEMORY_MS = 2 * 60 * 60_000;
// Prices carry the slot read before pricing starts, so the program's age limit bounds how old
// the prices are. Pricing may use at most half that window, leaving the keeper the rest to land
// the step; slower pricing is refused (the keeper retries) and stopped after PRICING_TIMEOUT_MS.
const MAX_PRICING_SLOTS = Math.floor(MAX_PRICE_AGE_SLOTS / 2);
const PRICING_TIMEOUT_MS = 10_000;
// One RPC call is cut off after this, so a hung RPC can't hold the queue of sign requests.
const RPC_TIMEOUT_MS = 20_000;

const argv = process.argv.slice(2);
function flagValue(name, fallback) {
  const idx = argv.indexOf(name);
  return idx >= 0 && idx + 1 < argv.length ? argv[idx + 1] : fallback;
}

const CONFIG = {
  port: Number(flagValue("--port", process.env.PORT ?? "8080")),
  maxSignaturesPerHour: Number(flagValue("--max-signatures-per-hour", "120")),
  maxMoveBps: Number(flagValue("--max-move-bps", "1500")),
  moveWindowS: Number(flagValue("--move-window-s", "900")),
  selfCheckIntervalS: Number(flagValue("--self-check-interval-s", "3600")),
  pricing: {
    probeUsd: Number(flagValue("--price-probe-usd", "50")),
    maxSpreadBps: Number(flagValue("--price-max-spread-bps", "400")),
    maxDeviationBps: Number(flagValue("--price-max-deviation-bps", "300")),
  },
};

const RPC_URL = process.env.SOLANA_RPC_URL ?? "https://api.mainnet-beta.solana.com";
const { swapApi: JUPITER_SWAP_API, priceApi: JUPITER_PRICE_API } = jupiterApis();

function log(...args) {
  console.log(new Date().toISOString(), ...args);
}

export class RequestError extends Error {
  constructor(status, message, extra = {}) {
    super(message);
    this.status = status;
    this.extra = extra;
  }
}

// --- request rules (pure, tested in tests/oracle-service.test.mjs) ----------------------

export function bearerMatches(header, token) {
  const given = Buffer.from(String(header ?? "").replace(/^Bearer\s+/i, ""));
  const expected = Buffer.from(String(token ?? ""));
  return expected.length > 0 && given.length === expected.length && crypto.timingSafeEqual(given, expected);
}

function parsePublicKey(value, what) {
  try {
    return new PublicKey(value);
  } catch {
    throw new RequestError(400, `${what} ${value} is not a public key`);
  }
}

export function parseSignRequest(body) {
  if (!body || typeof body !== "object") throw new RequestError(400, "request body must be a JSON object");
  const index = parsePublicKey(body.index, "index");
  const nonce = String(body.nonce ?? "");
  if (!/^\d{1,20}$/.test(nonce) || BigInt(nonce) >= 1n << 64n) {
    throw new RequestError(400, "nonce must be the rebalance intent's u64 nonce");
  }
  if (!Array.isArray(body.mints) || !body.mints.length) {
    throw new RequestError(400, "mints must be a non-empty array");
  }
  const mints = [...new Set(body.mints.map(String))];
  if (mints.length > MAX_MINTS_PER_REQUEST) {
    throw new RequestError(400, `at most ${MAX_MINTS_PER_REQUEST} mints per request`);
  }
  for (const mint of mints) {
    parsePublicKey(mint, "mint");
    if (mint === USDC_MINT) throw new RequestError(400, "USDC is always $1 on chain and is never signed");
  }
  return { index, nonce: BigInt(nonce), mints, dryRun: body.dryRun === true };
}

/**
 * Remembers signed prices so a rebalance can't be walked along a pushed pool: each price is
 * compared with the first one signed for the same intent, or else with the last one signed for
 * that mint within `moveWindowS`.
 */
export class MoveGuard {
  constructor({ maxMoveBps, moveWindowS }, now = () => Date.now()) {
    this.maxMoveBps = maxMoveBps;
    this.windowMs = moveWindowS * 1000;
    this.now = now;
    this.intents = new Map(); // intent -> { at, first: Map(mint -> bigint) }
    this.last = new Map(); // mint -> { price: bigint, at }
  }

  prune() {
    const now = this.now();
    for (const [intent, entry] of this.intents) if (now - entry.at > INTENT_MEMORY_MS) this.intents.delete(intent);
    for (const [mint, entry] of this.last) if (now - entry.at > this.windowMs) this.last.delete(mint);
  }

  /** `prices`: Map(mint -> { scaled: bigint }). Returns the prices that moved too far. */
  check(intent, prices) {
    this.prune();
    const opening = this.intents.get(intent)?.first;
    const moves = [];
    for (const [mint, p] of prices) {
      const first = opening?.get(mint);
      const reference = first ?? this.last.get(mint)?.price;
      if (reference === undefined || reference <= 0n) continue;
      const diff = p.scaled > reference ? p.scaled - reference : reference - p.scaled;
      const bps = Number((diff * 10_000n) / reference);
      if (bps > this.maxMoveBps) moves.push({ mint, bps, against: first !== undefined ? "intent" : "recent" });
    }
    return moves;
  }

  record(intent, prices) {
    const now = this.now();
    let entry = this.intents.get(intent);
    if (!entry) {
      entry = { at: now, first: new Map() };
      this.intents.set(intent, entry);
    }
    for (const [mint, p] of prices) {
      if (!entry.first.has(mint)) entry.first.set(mint, p.scaled);
      this.last.set(mint, { price: p.scaled, at: now });
    }
  }
}

// Signatures made in the last hour, so a misbehaving caller can only get so many.
export class SignatureBudget {
  constructor(perHour, now = () => Date.now()) {
    this.perHour = perHour;
    this.now = now;
    this.made = [];
  }

  remaining() {
    const cutoff = this.now() - 3_600_000;
    this.made = this.made.filter((t) => t > cutoff);
    return this.perHour - this.made.length;
  }

  spend() {
    this.made.push(this.now());
  }
}

/** The component index of each requested mint in `components` ({ index, mint, decimals }). */
export function resolveComponents(components, mints, label) {
  const byMint = new Map(components.map((c) => [c.mint, c]));
  const missing = mints.filter((m) => !byMint.has(m));
  if (missing.length) throw new RequestError(403, `${label} does not hold ${missing.join(", ")}`, { mints: missing });
  return mints.map((m) => byMint.get(m));
}

function serializePrices(prices) {
  return Object.fromEntries(
    [...prices].map(([mint, p]) => [
      mint,
      { scaled: p.scaled.toString(), usd: p.usd, spreadBps: p.spreadBps, confirmedBy: p.confirmedBy },
    ]),
  );
}

function describePrices(prices) {
  return [...prices].map(([mint, p]) => `${mint.slice(0, 6)}.. $${p.usd.toPrecision(6)}`).join(", ");
}

// --- HTTP ----------------------------------------------------------------------------------

function reply(res, status, body) {
  res.writeHead(status, { "content-type": "application/json" });
  res.end(JSON.stringify(body));
}

async function readJsonBody(req) {
  const chunks = [];
  let size = 0;
  for await (const chunk of req) {
    size += chunk.length;
    if (size > MAX_BODY_BYTES) throw new RequestError(413, "request body too large");
    chunks.push(chunk);
  }
  try {
    return JSON.parse(Buffer.concat(chunks).toString("utf8"));
  } catch {
    throw new RequestError(400, "request body is not JSON");
  }
}

// Why a signature can't be made now: the program doesn't accept this key, a price moved too
// far, or the hourly budget is spent.
async function signBlockers(deps, intent, prices) {
  const blockers = [];
  const configured = await deps.configuredOracle();
  if (!configured) {
    blockers.push({ status: 409, message: "the program's price oracle is not set yet" });
  } else if (configured !== deps.oracle) {
    blockers.push({ status: 409, message: `the program accepts prices signed by ${configured}, not this service's key ${deps.oracle}` });
  }
  for (const move of deps.moves.check(intent, prices)) {
    blockers.push({
      status: 422,
      message: `${move.mint} moved ${(move.bps / 100).toFixed(2)}% from the price ${move.against === "intent" ? "first signed for this rebalance" : "signed recently"}`,
    });
  }
  if (deps.budget.remaining() < 1) {
    blockers.push({ status: 429, message: `the budget of ${deps.config.maxSignaturesPerHour} signatures an hour is spent` });
  }
  return blockers;
}

async function priceAndSign(deps, { index, nonce, mints, dryRun }) {
  const basket = await deps.basket(index);
  const components = resolveComponents(basket.components, mints, `basket ${index.toBase58()}`);
  const intent = rebalanceIntentAddress(deps.programId, index, nonce).toBase58();
  // The message carries the slot read before pricing starts, so the program's age limit bounds
  // how old the prices themselves are, not just how long ago they were signed.
  const slot = await deps.slot();
  const deadline = AbortSignal.timeout(PRICING_TIMEOUT_MS);
  let prices;
  try {
    prices = await deps.price(components.map((c) => ({ mint: c.mint, decimals: c.decimals, label: c.mint.slice(0, 6) })), { signal: deadline });
  } catch (error) {
    if (deadline.aborted) throw new RequestError(503, `pricing took over ${PRICING_TIMEOUT_MS / 1000}s and was stopped`);
    throw error;
  }
  const blockers = await signBlockers(deps, intent, prices);
  const pricingSlots = (await deps.slot()) - slot;
  if (pricingSlots > MAX_PRICING_SLOTS) {
    blockers.push({
      status: 503,
      message: `pricing took ${pricingSlots} slots, more than the ${MAX_PRICING_SLOTS} that leave the keeper time to land the step`,
    });
  }
  const result = { ok: true, dryRun, intent, oracle: deps.oracle, prices: serializePrices(prices) };
  if (dryRun) return { ...result, blockers: blockers.map((b) => b.message) };
  if (blockers.length) {
    throw new RequestError(blockers[0].status, blockers.map((b) => b.message).join("; "));
  }
  const message = encodePriceMessage({
    intent,
    slot,
    entries: components.map((c) => ({ componentIndex: c.index, price: prices.get(c.mint).scaled })),
  });
  const signature = deps.sign(message);
  deps.budget.spend();
  deps.moves.record(intent, prices);
  deps.log?.(`[signed] intent ${intent.slice(0, 8)}.. slot ${slot}: ${describePrices(prices)}`);
  return { ...result, slot, message: message.toString("base64"), signature: Buffer.from(signature).toString("base64") };
}

/**
 * The service's request handler, over injected chain access so tests can run it without one.
 * deps: { programId, oracle (base58), token, config, budget: SignatureBudget, moves: MoveGuard,
 *   log?, basket(index) -> { components: [{ index, mint, decimals }] }, price(tokens) -> Map,
 *   configuredOracle() -> base58 | null, slot() -> number, sign(message) -> signature,
 *   status() -> object }
 */
export function createHandler(deps) {
  // One request at a time, so concurrent callers can't overrun the budget or the move guard.
  let queue = Promise.resolve();
  const serially = (fn) => {
    const run = queue.then(fn);
    queue = run.catch(() => {});
    return run;
  };
  return async (req, res) => {
    try {
      const { pathname } = new URL(req.url, "http://oracle");
      if (req.method === "GET" && pathname === "/health") return reply(res, 200, await deps.status());
      if (req.method !== "POST" || pathname !== "/sign") throw new RequestError(404, "not found");
      if (!bearerMatches(req.headers.authorization, deps.token)) throw new RequestError(401, "unauthorized");
      const request = parseSignRequest(await readJsonBody(req));
      reply(res, 200, await serially(() => priceAndSign(deps, request)));
    } catch (error) {
      if (error instanceof RequestError) {
        return reply(res, error.status, { ok: false, error: error.message, ...error.extra });
      }
      if (error instanceof OraclePriceError) {
        return reply(res, 503, { ok: false, error: error.message, failures: error.failures });
      }
      deps.log?.(`[error] ${error.stack ?? error}`);
      reply(res, 500, { ok: false, error: error.message ?? String(error) });
    }
  };
}

// --- chain ---------------------------------------------------------------------------------

function chainDeps({ connection, program, oracle, config }) {
  const budget = new SignatureBudget(config.maxSignaturesPerHour);
  const moves = new MoveGuard(config);
  let selfCheck = null;

  // The program's own account at `address`, decoded, or null.
  function decodeOwned(name, address, info) {
    if (!info) return null;
    if (!info.owner.equals(PROGRAM_ID)) throw new RequestError(403, `${address.toBase58()} is not the program's account`);
    return program.coder.accounts.decode(name, info.data);
  }

  // One FixedWeights basket's components in global order, from its pages on chain.
  async function basket(index) {
    const state = decodeOwned("indexState", index, await connection.getAccountInfo(index, "confirmed"));
    if (!state || !("fixedWeights" in state.kind)) {
      throw new RequestError(403, `${index.toBase58()} is not a FixedWeights basket`);
    }
    const pageAddresses = Array.from({ length: state.largeBasketPageCount }, (_, i) =>
      PublicKey.findProgramAddressSync([Buffer.from("large-basket-component-page"), index.toBuffer(), Buffer.from([i])], PROGRAM_ID)[0]);
    const infos = await connection.getMultipleAccountsInfo(pageAddresses, "confirmed");
    const pages = infos.map((info, i) => {
      const page = decodeOwned("largeBasketComponentPage", pageAddresses[i], info);
      if (!page || !page.index.equals(index)) throw new Error(`page ${i} of ${index.toBase58()} is missing`);
      return page;
    });
    pages.sort((a, b) => a.startComponentIndex - b.startComponentIndex);
    const components = pages.flatMap((page) => page.components.map((c, i) => ({
      index: page.startComponentIndex + i,
      mint: c.mint.toBase58(),
      decimals: c.decimals,
    })));
    return { components };
  }

  // Every token a FixedWeights basket would price in a rebalance now (held, not retired), for
  // the self-check; a pending composition change's additions are priced too.
  async function rebalancedTokens() {
    const indexes = await program.account.indexState.all([
      {
        memcmp: {
          offset: INDEX_KIND_OFFSET,
          bytes: anchor.utils.bytes.bs58.encode(Buffer.from([INDEX_KIND_FIXED_WEIGHTS])),
        },
      },
    ]);
    const fixedWeights = new Set(indexes.map((i) => i.publicKey.toBase58()));
    const [pages, changes] = await Promise.all([
      program.account.largeBasketComponentPage.all(),
      program.account.compositionChange.all(),
    ]);
    const tokens = new Map();
    for (const { account } of pages) {
      if (!fixedWeights.has(account.index.toBase58())) continue;
      for (const c of account.components) {
        const mint = c.mint.toBase58();
        if (mint === USDC_MINT || (c.targetWeightBps === 0 && c.accountedReserve.isZero())) continue;
        tokens.set(mint, { mint, decimals: c.decimals, label: mint.slice(0, 6) });
      }
    }
    const added = [
      ...new Set(
        changes
          .filter(({ account }) => fixedWeights.has(account.index.toBase58()))
          .flatMap(({ account }) => account.additions.map((a) => a.mint.toBase58())),
      ),
    ].filter((mint) => mint !== USDC_MINT && !tokens.has(mint));
    if (added.length) {
      const infos = await connection.getMultipleAccountsInfo(added.map((m) => new PublicKey(m)), "confirmed");
      // SPL mint layout: decimals is the byte at offset 44.
      infos.forEach((info, i) => {
        if (info && info.data.length >= 45) tokens.set(added[i], { mint: added[i], decimals: info.data[44], label: added[i].slice(0, 6) });
      });
    }
    return [...tokens.values()];
  }

  function price(tokenList, { signal } = {}) {
    return oraclePrices(tokenList, {
      ...config.pricing,
      swapApi: JUPITER_SWAP_API,
      priceApi: JUPITER_PRICE_API,
      fetchJson,
      signal,
    });
  }

  async function configuredOracle() {
    const account = await program.account.priceOracle.fetchNullable(PRICE_ORACLE);
    return account ? account.oracle.toBase58() : null;
  }

  async function status() {
    const base = {
      oracle: oracle.publicKey.toBase58(),
      priceOracle: PRICE_ORACLE.toBase58(),
      signaturesLeftThisHour: budget.remaining(),
      selfCheck,
    };
    try {
      const configured = await configuredOracle();
      return { ok: true, ...base, accepted: configured === base.oracle, configuredOracle: configured };
    } catch (error) {
      return { ok: false, ...base, error: error.message ?? String(error) };
    }
  }

  // Prices every token a rebalance would need now, without signing, and records the outcome
  // for /health and the logs.
  async function runSelfCheck() {
    const at = new Date().toISOString();
    try {
      const prices = await price(await rebalancedTokens());
      selfCheck = { at, ok: true, priced: prices.size };
      log(`[self-check] priced ${prices.size} token(s): ${describePrices(prices) || "none needed"}`);
    } catch (error) {
      const failures = error instanceof OraclePriceError ? error.failures : undefined;
      selfCheck = { at, ok: false, error: error.message ?? String(error), failures };
      log(`[self-check] FAILED: ${error.message ?? error}`);
    }
  }

  return {
    budget,
    moves,
    basket,
    price,
    configuredOracle,
    slot: () => connection.getSlot("confirmed"),
    sign: (message) => signPriceMessage(oracle.secretKey, message),
    status,
    runSelfCheck,
  };
}

// --- main ----------------------------------------------------------------------------------

function loadOracleKeypair() {
  if (process.env.ORACLE_KEYPAIR) return Keypair.fromSecretKey(Uint8Array.from(JSON.parse(process.env.ORACLE_KEYPAIR)));
  if (process.env.ORACLE_WALLET) {
    return Keypair.fromSecretKey(Uint8Array.from(JSON.parse(fs.readFileSync(process.env.ORACLE_WALLET, "utf8"))));
  }
  throw new Error("set ORACLE_KEYPAIR (secret key JSON array) or ORACLE_WALLET (path to one)");
}

async function main() {
  const oracle = loadOracleKeypair();
  const token = process.env.ORACLE_API_TOKEN ?? "";
  if (token.length < 32) throw new Error("set ORACLE_API_TOKEN (32+ characters) so only the keeper can ask for signatures");
  const idl = JSON.parse(fs.readFileSync(path.join(process.cwd(), "target", "idl", "basket.json"), "utf8"));
  if (idl.address !== PROGRAM_ID.toBase58()) throw new Error(`IDL program id ${idl.address} != ${PROGRAM_ID.toBase58()}`);
  const connection = new Connection(RPC_URL, {
    commitment: "confirmed",
    fetch: (url, init) => fetch(url, { ...init, signal: AbortSignal.timeout(RPC_TIMEOUT_MS) }),
  });
  const program = new anchor.Program(idl, { connection, publicKey: oracle.publicKey });

  const chain = chainDeps({ connection, program, oracle, config: CONFIG });
  const handler = createHandler({
    ...chain,
    programId: PROGRAM_ID,
    oracle: oracle.publicKey.toBase58(),
    token,
    config: CONFIG,
    log,
  });
  const server = http.createServer(handler);
  // "::" also accepts IPv4; Fly's private network (<app>.internal) is IPv6.
  await new Promise((resolve) => server.listen(CONFIG.port, "::", resolve));
  log(JSON.stringify({
    listening: CONFIG.port,
    rpc: new URL(RPC_URL).origin,
    jupiter: new URL(JUPITER_SWAP_API).origin,
    jupiterKey: Boolean(jupiterApiKey()),
    oracle: oracle.publicKey.toBase58(),
    priceOracle: PRICE_ORACLE.toBase58(),
    config: CONFIG,
  }));
  const health = await chain.status();
  if (!health.ok) log(`could not read the price oracle account yet: ${health.error}`);
  else if (health.accepted) log("the program accepts this key: signing is live");
  else log(`standing by: the program accepts ${health.configuredOracle ?? "no key (the price oracle is not set yet)"}, not this key; dry runs only`);

  await chain.runSelfCheck();
  const timer = setInterval(() => chain.runSelfCheck(), CONFIG.selfCheckIntervalS * 1000);
  const stop = (signal) => {
    log(`${signal} received; stopping`);
    clearInterval(timer);
    server.close(() => process.exit(0));
    setTimeout(() => process.exit(0), 10_000).unref();
  };
  process.once("SIGTERM", () => stop("SIGTERM"));
  process.once("SIGINT", () => stop("SIGINT"));
}

if (process.argv[1] && import.meta.url === pathToFileURL(path.resolve(process.argv[1])).href) {
  await main();
}
