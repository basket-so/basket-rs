// Off-chain price oracle for rebalances.
//
// A token's price is the midpoint of a small Jupiter round trip: buy it with PROBE_USD of
// USDC, sell what that bought back to USDC. That is a price the basket can actually trade
// at, from whichever venues Jupiter routes through, for any token Jupiter can route. It is
// only signed if every available reference (Jupiter's price API, when recently updated, and
// DexScreener's most liquid pair) agrees within MAX_DEVIATION_BPS, at least one is available,
// and the round trip costs under MAX_SPREAD_BPS without gaining money. A token failing any
// check is not priced, so its basket does not rebalance until it can be.
//
// MAX_SPREAD_BPS must stay well under twice the per-leg execution bound the keeper passes
// (max_oracle_slippage_bps): a trade fills about half the spread away from the midpoint, so a
// wider token could be priced but never traded within the bound.
//
// Prices are signed for one rebalance intent by the oracle key (a key separate from the
// keeper's; scripts/lib/signed-prices.mjs), and each rebalance step checks that signature.

import { quantizePrice } from "./signed-prices.mjs";

export const USDC_MINT = "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v";
export const PRICE_SCALE = 10n ** 18n; // USD per whole token, as stored on chain
const USDC_DECIMALS = 6;
// Jupiter's price API reports the slot its price was last updated in; older is stale.
const MAX_REFERENCE_AGE_SLOTS = 9_000; // about an hour
const DEXSCREENER_TOKENS_API = "https://api.dexscreener.com/tokens/v1/solana";
const DEXSCREENER_BATCH = 30;

export const DEFAULT_ORACLE_CONFIG = {
  probeUsd: 50,
  maxSpreadBps: 400,
  maxDeviationBps: 300,
};
// A round trip that gains money means one leg priced off a stale or pushed pool.
const MIN_SPREAD_BPS = -50;
// Tokens priced at once. Open and finalize price every token of a basket, and the oracle stamps
// prices with the slot before pricing starts, so pricing must finish well within the program's
// price age limit.
const PRICE_CONCURRENCY = 8;
// One HTTP try is cut off after this, so a price source that hangs can't hold the caller.
const FETCH_TIMEOUT_MS = 10_000;

// Jupiter's keyed API. With JUPITER_API_KEY set, Jupiter calls go here and carry the key; the key
// is sent to this host only.
export const JUPITER_KEYED_HOST = "api.jup.ag";

export function jupiterApiKey() {
  return process.env.JUPITER_API_KEY?.trim() || null;
}

/** Jupiter's swap and price API bases: the keyed API with JUPITER_API_KEY, else the keyless one. */
export function jupiterApis() {
  const host = jupiterApiKey() ? JUPITER_KEYED_HOST : "lite-api.jup.ag";
  return {
    swapApi: process.env.JUPITER_SWAP_API ?? `https://${host}/swap/v1`,
    priceApi: process.env.JUPITER_PRICE_API ?? `https://${host}/price/v3`,
  };
}

/**
 * JSON from a price source (or Jupiter's swap API). No answer, a timeout, a rate limit or a server
 * error is retried with backoff; any other refusal (e.g. Jupiter finding no route) is final.
 * Aborting `init.signal` stops it altogether. Calls to Jupiter's keyed API carry JUPITER_API_KEY.
 */
export async function fetchJson(url, init = {}, attempts = 4) {
  const key = jupiterApiKey();
  const headers = key && new URL(url).hostname === JUPITER_KEYED_HOST ? { ...init.headers, "x-api-key": key } : init.headers;
  for (let attempt = 1; ; attempt += 1) {
    let res;
    try {
      const timeout = AbortSignal.timeout(FETCH_TIMEOUT_MS);
      res = await fetch(url, { ...init, headers, signal: init.signal ? AbortSignal.any([init.signal, timeout]) : timeout });
      if (res.ok) return await res.json();
      throw new Error(`${url} -> ${res.status}: ${(await res.text()).slice(0, 300)}`);
    } catch (error) {
      const retryable = !res || res.ok || res.status === 429 || res.status >= 500;
      if (!retryable || attempt >= attempts || init.signal?.aborted) throw error;
    }
    await new Promise((r) => setTimeout(r, attempt * 1_500));
  }
}

// `fn` over `items`, at most `limit` at a time, results in order.
async function mapConcurrently(items, limit, fn) {
  const results = new Array(items.length);
  let next = 0;
  const worker = async () => {
    while (next < items.length) {
      const i = next;
      next += 1;
      results[i] = await fn(items[i]);
    }
  };
  await Promise.all(Array.from({ length: Math.min(limit, items.length) }, worker));
  return results;
}

export class OraclePriceError extends Error {
  constructor(failures) {
    super(`could not price ${failures.map((f) => `${f.label}: ${f.reason}`).join("; ")}`);
    this.failures = failures;
  }
}

const bps = (a, b) => Math.abs(a / b - 1) * 10_000;
const toNumber = (scaled) => Number(scaled) / Number(PRICE_SCALE);

// Jupiter quote: amount in atoms, ExactIn. Throws if there is no route.
async function quote(env, inputMint, outputMint, amount) {
  const url =
    `${env.swapApi}/quote?inputMint=${inputMint}&outputMint=${outputMint}` +
    `&amount=${amount}&swapMode=ExactIn&slippageBps=100&restrictIntermediateTokens=true`;
  const result = await env.fetchJson(url, { headers: { accept: "application/json" }, signal: env.signal });
  if (!result?.outAmount || BigInt(result.outAmount) <= 0n) throw new Error("no Jupiter route");
  return result;
}

// Round-trip midpoint, exact in integers: (USDC in + USDC back) / (2 × tokens bought).
async function roundTrip(env, token) {
  const probeAtoms = BigInt(Math.round(env.probeUsd * 10 ** USDC_DECIMALS));
  const buy = await quote(env, USDC_MINT, token.mint, probeAtoms);
  const tokens = BigInt(buy.outAmount);
  const sell = await quote(env, token.mint, USDC_MINT, tokens);
  const back = BigInt(sell.outAmount);
  const tokenScale = 10n ** BigInt(token.decimals);
  const usdcScale = 10n ** BigInt(USDC_DECIMALS);
  const scaled = ((probeAtoms + back) * tokenScale * PRICE_SCALE) / (2n * tokens * usdcScale);
  // Buy price over sell price: what a round trip costs.
  const spreadBps = Number(((probeAtoms - back) * 10_000n) / probeAtoms);
  return { scaled, spreadBps, slot: Number(buy.contextSlot ?? 0) };
}

async function jupiterReferences(env, mints) {
  const out = new Map();
  if (!mints.length) return out;
  const payload = await env.fetchJson(`${env.priceApi}?ids=${mints.join(",")}`, {
    headers: { accept: "application/json" },
    signal: env.signal,
  });
  for (const mint of mints) {
    const entry = payload?.[mint];
    const usd = Number(entry?.usdPrice);
    if (Number.isFinite(usd) && usd > 0) out.set(mint, { usd, slot: Number(entry.blockId ?? 0) });
  }
  return out;
}

async function dexscreenerReferences(env, mints) {
  const out = new Map();
  for (let i = 0; i < mints.length; i += DEXSCREENER_BATCH) {
    const batch = mints.slice(i, i + DEXSCREENER_BATCH);
    const pairs = await env.fetchJson(`${DEXSCREENER_TOKENS_API}/${batch.join(",")}`, {
      headers: { accept: "application/json" },
      signal: env.signal,
    });
    for (const pair of Array.isArray(pairs) ? pairs : []) {
      const mint = pair?.baseToken?.address;
      const usd = Number(pair?.priceUsd);
      const liquidity = Number(pair?.liquidity?.usd ?? 0);
      if (!batch.includes(mint) || !(usd > 0)) continue;
      if (!out.has(mint) || liquidity > out.get(mint).liquidity) out.set(mint, { usd, liquidity });
    }
  }
  return out;
}

/**
 * Prices `tokens` ([{ mint, decimals, label? }], USDC excluded) for signing.
 * Returns Map(mint -> { scaled: bigint (PRICE_SCALE), usd, spreadBps, confirmedBy: [sources],
 * references: [{ source, usd }] }).
 * Throws OraclePriceError naming every token it could not price.
 *
 * env: { swapApi, priceApi, fetchJson(url, init), probeUsd?, maxSpreadBps?, maxDeviationBps?,
 *   signal? (an AbortSignal that stops pricing) }
 */
export async function oraclePrices(tokens, env) {
  const cfg = { ...DEFAULT_ORACLE_CONFIG, ...env };
  const mints = tokens.map((t) => t.mint);
  // References are only vetoes; an unavailable one just can't confirm. They load while the round
  // trips run, and the round trips run several tokens at a time.
  const references = Promise.all([
    jupiterReferences(cfg, mints).catch(() => new Map()),
    dexscreenerReferences(cfg, mints).catch(() => new Map()),
  ]);
  const trips = await mapConcurrently(tokens, PRICE_CONCURRENCY, (token) =>
    token.mint === USDC_MINT
      ? { error: new Error("USDC is priced at $1 on chain") }
      : roundTrip(cfg, token).then((trip) => ({ trip }), (error) => ({ error })));
  const [jupiter, dexscreener] = await references;
  const prices = new Map();
  const failures = [];
  for (const [i, token] of tokens.entries()) {
    const label = token.label ?? token.mint;
    try {
      const { trip, error } = trips[i];
      if (error) throw error;
      if (trip.spreadBps > cfg.maxSpreadBps) {
        throw new Error(`round-trip spread ${(trip.spreadBps / 100).toFixed(2)}% is too wide to price`);
      }
      if (trip.spreadBps < MIN_SPREAD_BPS) {
        throw new Error(`round trip gains ${(-trip.spreadBps / 100).toFixed(2)}%, so a pool is off`);
      }
      const usd = toNumber(trip.scaled);
      const references = [];
      const jup = jupiter.get(token.mint);
      if (jup && !(trip.slot && jup.slot && trip.slot - jup.slot > MAX_REFERENCE_AGE_SLOTS)) {
        references.push({ source: "jupiter-price", usd: jup.usd });
      }
      const dex = dexscreener.get(token.mint);
      if (dex) references.push({ source: "dexscreener", usd: dex.usd });
      if (!references.length) throw new Error(`round trip $${usd} has no reference price to confirm it`);
      const disagreeing = references.filter((r) => bps(r.usd, usd) > cfg.maxDeviationBps);
      if (disagreeing.length) {
        const seen = disagreeing.map((r) => `${r.source} $${r.usd}`).join(", ");
        throw new Error(`round trip $${usd} disagrees by over ${cfg.maxDeviationBps / 100}% with ${seen}`);
      }
      const confirmedBy = references;
      prices.set(token.mint, {
        // The price a signed message carries (9 to 10 significant digits), so what the keeper
        // checks legs against is exactly what the program reads.
        scaled: quantizePrice(trip.scaled),
        usd,
        spreadBps: trip.spreadBps,
        confirmedBy: confirmedBy.map((r) => r.source),
        references,
      });
    } catch (error) {
      failures.push({ mint: token.mint, label, reason: error.message ?? String(error) });
    }
  }
  if (failures.length) throw new OraclePriceError(failures);
  return prices;
}
