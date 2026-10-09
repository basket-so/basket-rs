import test from "node:test";
import assert from "node:assert/strict";
import http from "node:http";
import { Keypair, PublicKey } from "@solana/web3.js";
import {
  MoveGuard,
  RequestError,
  SignatureBudget,
  bearerMatches,
  createHandler,
  parseSignRequest,
  resolveComponents,
} from "../scripts/oracle-service.mjs";
import { OraclePriceError, USDC_MINT } from "../scripts/lib/price-oracle.mjs";
import {
  decodePriceMessage,
  priceSignatureInstruction,
  rebalanceIntentAddress,
  signPriceMessage,
  verifyPriceSignature,
} from "../scripts/lib/signed-prices.mjs";
import { PROGRAM_ID, oracleProblems, requestSignedPrices } from "../scripts/rebalance-bot.mjs";

const TOKEN = "t".repeat(40);
const oracleKey = Keypair.generate();
const ORACLE = oracleKey.publicKey.toBase58();
const INDEX = Keypair.generate().publicKey;
const mintA = Keypair.generate().publicKey.toBase58();
const mintB = Keypair.generate().publicKey.toBase58();
const SLOT = 1_000_000;
const scaledUsd = (usd) => BigInt(Math.round(usd * 1e6)) * 10n ** 12n;
const priced = (usd) => ({ scaled: scaledUsd(usd), usd, spreadBps: 20, confirmedBy: ["dexscreener"] });

// The service's handler over a fake chain. The basket holds mintA, the USDC cash slot and
// mintB, in that order; mintA trades at $2 and mintB at $0.50 unless a test moves them.
function fakeService(overrides = {}) {
  const signed = [];
  const market = new Map([[mintA, 2], [mintB, 0.5]]);
  const deps = {
    programId: PROGRAM_ID,
    oracle: ORACLE,
    token: TOKEN,
    config: { maxSignaturesPerHour: 3, maxMoveBps: 1500, moveWindowS: 900 },
    budget: new SignatureBudget(3),
    moves: new MoveGuard({ maxMoveBps: 1500, moveWindowS: 900 }),
    basket: async (index) => {
      if (!index.equals(INDEX)) throw new RequestError(403, `${index.toBase58()} is not a FixedWeights basket`);
      return {
        components: [
          { index: 0, mint: mintA, decimals: 6 },
          { index: 1, mint: USDC_MINT, decimals: 6 },
          { index: 2, mint: mintB, decimals: 9 },
        ],
      };
    },
    price: async (tokens) => new Map(tokens.map((t) => [t.mint, priced(market.get(t.mint))])),
    configuredOracle: async () => ORACLE,
    slot: async () => SLOT,
    sign: (message) => {
      signed.push(message);
      return signPriceMessage(oracleKey.secretKey, message);
    },
    status: async () => ({ ok: true, oracle: ORACLE, accepted: true, configuredOracle: ORACLE }),
    ...overrides,
  };
  return { deps, signed, market };
}

async function serve(deps, fn) {
  const server = http.createServer(createHandler(deps));
  await new Promise((resolve) => server.listen(0, "127.0.0.1", resolve));
  try {
    return await fn(`http://127.0.0.1:${server.address().port}`);
  } finally {
    server.close();
  }
}

const ask = (url, request, token = TOKEN) => requestSignedPrices(url, token, { index: INDEX, nonce: 7n, mints: [mintA], ...request });

test("only callers with the token can ask for signatures", async () => {
  const { deps, signed } = fakeService();
  await serve(deps, async (url) => {
    const bare = await fetch(`${url}/sign`, { method: "POST", body: JSON.stringify({ index: INDEX.toBase58(), nonce: "1", mints: [mintA] }) });
    assert.equal(bare.status, 401);
    await assert.rejects(ask(url, {}, "x".repeat(40)), (e) => e.status === 401);
    const health = await fetch(`${url}/health`).then((r) => r.json());
    assert.equal(health.oracle, ORACLE, "health needs no token");
    assert.equal((await fetch(`${url}/prices`, { method: "POST" })).status, 404, "nothing is posted any more");
  });
  assert.equal(signed.length, 0);
});

test("signs its own prices for the intent the index and nonce name, by component index", async () => {
  const { deps, signed } = fakeService();
  await serve(deps, async (url) => {
    const answer = await ask(url, { mints: [mintB, mintA, mintB] });
    const intent = rebalanceIntentAddress(PROGRAM_ID, INDEX, 7n);
    assert.ok(answer.intent.equals(intent));
    assert.equal(answer.slot, SLOT);
    assert.equal(answer.oracle.toBase58(), ORACLE);
    assert.equal(answer.prices.get(mintA).scaled, scaledUsd(2));
    assert.deepEqual(answer.prices.get(mintA).confirmedBy, ["dexscreener"]);
    // The message names the intent, at the slot the service read before pricing, and indexes
    // the service resolved itself (mintB is component 2, past the USDC slot).
    const message = decodePriceMessage(answer.message);
    assert.ok(message.intent.equals(intent));
    assert.equal(message.slot, SLOT);
    assert.deepEqual(message.entries, [
      { componentIndex: 0, price: scaledUsd(2) },
      { componentIndex: 2, price: scaledUsd(0.5) },
    ]);
    assert.ok(verifyPriceSignature(oracleKey.publicKey, answer.message, answer.signature));
    // The keeper's Ed25519 instruction reads everything from its own data.
    const ix = priceSignatureInstruction(answer);
    assert.equal(ix.data[0], 1);
    for (const field of [1, 3, 6]) assert.equal(ix.data.readUInt16LE(2 + 2 * field), 0xffff);
  });
  assert.equal(signed.length, 1);
  assert.equal(deps.budget.remaining(), 2);
});

test("prices are stamped with the slot before pricing, and slow pricing is refused as retryable", async () => {
  // The chain advances `perPricing` slots while the service prices.
  const timed = (perPricing) => {
    let slot = SLOT;
    const reads = [];
    const service = fakeService({
      slot: async () => {
        reads.push(slot);
        return slot;
      },
    });
    const price = service.deps.price;
    service.deps.price = async (tokens, options) => {
      assert.ok(options.signal instanceof AbortSignal, "pricing can be stopped");
      slot += perPricing;
      return price(tokens);
    };
    return { ...service, reads };
  };
  const quick = timed(20);
  await serve(quick.deps, async (url) => {
    const answer = await ask(url, {});
    assert.equal(decodePriceMessage(answer.message).slot, SLOT, "the slot before pricing, not after");
  });
  assert.deepEqual(quick.reads, [SLOT, SLOT + 20]);
  // Pricing that used over half the 50-slot window leaves the keeper too little to land the step.
  const slow = timed(26);
  await serve(slow.deps, async (url) => {
    await assert.rejects(ask(url, {}), (e) => e.status === 503 && /pricing took 26 slots, more than the 25/.test(e.message));
    const dry = await ask(url, { dryRun: true });
    assert.match(dry.blockers.join(), /pricing took 26 slots/);
  });
  assert.equal(slow.signed.length, 0);
  assert.equal(slow.deps.budget.remaining(), 3);
});

test("stands by while the program accepts another key: dry runs work, signing is refused", async () => {
  const other = Keypair.generate().publicKey.toBase58();
  const { deps, signed } = fakeService({ configuredOracle: async () => other });
  await serve(deps, async (url) => {
    const dry = await ask(url, { dryRun: true });
    assert.equal(dry.prices.get(mintA).scaled, scaledUsd(2));
    assert.equal(dry.signature, undefined);
    assert.equal(dry.message, undefined);
    assert.match(dry.blockers.join(), new RegExp(`accepts prices signed by ${other}`));
    await assert.rejects(ask(url, {}), (e) => e.status === 409);
  });
  const unset = fakeService({ configuredOracle: async () => null });
  await serve(unset.deps, async (url) => {
    await assert.rejects(ask(url, {}), (e) => e.status === 409 && /not set yet/.test(e.message));
  });
  assert.equal(signed.length + unset.signed.length, 0);
  assert.equal(deps.budget.remaining(), 3, "dry runs spend nothing");
});

test("pins a rebalance to its opening prices and refuses jumps from recent signatures", async () => {
  const { deps, signed, market } = fakeService({ budget: new SignatureBudget(10) });
  await serve(deps, async (url) => {
    await ask(url, { nonce: 1n });
    market.set(mintA, 2.5); // +25%
    await assert.rejects(ask(url, { nonce: 1n }), (e) => e.status === 422 && /moved 25\.00% from the price first signed for this rebalance/.test(e.message));
    await assert.rejects(ask(url, { nonce: 2n }), (e) => e.status === 422 && /signed recently/.test(e.message));
    market.set(mintA, 2.2); // +10%: within the 15% bound of the opening price
    await ask(url, { nonce: 1n });
    // A walk of small steps stays pinned to the opening price, not the last one.
    market.set(mintA, 2.4);
    await ask(url, { nonce: 3n }); // +9% from the last signature, a new intent: fine
    await assert.rejects(ask(url, { nonce: 1n }), (e) => e.status === 422 && /moved 20\.00%/.test(e.message));
  });
  assert.equal(signed.length, 3);
});

test("move guard: the opening price binds for the intent's life, recent prices only within the window", () => {
  let now = 0;
  const guard = new MoveGuard({ maxMoveBps: 1000, moveWindowS: 60 }, () => now);
  const at = (usd) => new Map([[mintA, priced(usd)]]);
  guard.record("open", at(1));
  assert.deepEqual(guard.check("open", at(1.11)), [{ mint: mintA, bps: 1100, against: "intent" }]);
  assert.deepEqual(guard.check("other", at(1.11)), [{ mint: mintA, bps: 1100, against: "recent" }]);
  assert.deepEqual(guard.check("other", at(0.91)), [], "9% is within 10%");
  now = 61_000;
  assert.deepEqual(guard.check("other", at(1.5)), [], "the last price is older than the window");
  assert.deepEqual(guard.check("open", at(1.5)), [{ mint: mintA, bps: 5000, against: "intent" }]);
  now = 3 * 3_600_000;
  assert.deepEqual(guard.check("open", at(1.5)), [], "intents are forgotten long after they expire");
});

test("signs only for FixedWeights baskets and mints they hold, and reports pricing failures as retryable", async () => {
  const stranger = Keypair.generate().publicKey.toBase58();
  const { deps } = fakeService();
  await serve(deps, async (url) => {
    await assert.rejects(ask(url, { mints: [mintA, stranger] }), (e) => e.status === 403 && e.message.includes(stranger));
    await assert.rejects(ask(url, { index: Keypair.generate().publicKey }), (e) => e.status === 403 && /not a FixedWeights basket/.test(e.message));
  });
  const failing = fakeService({
    price: async () => {
      throw new OraclePriceError([{ mint: mintA, label: "mintA", reason: "no Jupiter route" }]);
    },
  });
  await serve(failing.deps, async (url) => {
    await assert.rejects(ask(url, {}), (e) => e.status === 503 && /no Jupiter route/.test(e.message));
  });
  assert.throws(() => resolveComponents([{ index: 0, mint: mintA }], [mintB], "basket"), /does not hold/);
});

test("the hourly signature budget caps what callers can get", async () => {
  let now = 0;
  const budget = new SignatureBudget(2, () => now);
  budget.spend();
  budget.spend();
  assert.equal(budget.remaining(), 0);
  now = 3_600_001;
  assert.equal(budget.remaining(), 2);

  const spent = new SignatureBudget(1);
  spent.spend();
  const { deps, signed } = fakeService({ budget: spent });
  await serve(deps, async (url) => {
    await assert.rejects(ask(url, {}), (e) => e.status === 429);
  });
  assert.equal(signed.length, 0);
});

test("sign requests are validated before anything is priced", () => {
  const index = INDEX.toBase58();
  assert.throws(() => parseSignRequest(null), /JSON object/);
  assert.throws(() => parseSignRequest({ nonce: "1", mints: [mintA] }), /index .* is not a public key/);
  assert.throws(() => parseSignRequest({ index: "nope", nonce: "1", mints: [mintA] }), /not a public key/);
  for (const nonce of [undefined, "", "-1", "1.5", "0x10", (1n << 64n).toString()]) {
    assert.throws(() => parseSignRequest({ index, nonce, mints: [mintA] }), /u64 nonce/, String(nonce));
  }
  assert.throws(() => parseSignRequest({ index, nonce: "1", mints: [] }), /non-empty/);
  assert.throws(() => parseSignRequest({ index, nonce: "1", mints: ["not-a-key"] }), /not a public key/);
  assert.throws(() => parseSignRequest({ index, nonce: "1", mints: [USDC_MINT] }), /never signed/);
  // Every token of the widest basket fits one request.
  const widest = Array.from({ length: 50 }, () => Keypair.generate().publicKey.toBase58());
  assert.equal(parseSignRequest({ index, nonce: "1", mints: widest }).mints.length, 50);
  assert.throws(() => parseSignRequest({ index, nonce: "1", mints: [...widest, mintA] }), /at most 50/);
  const parsed = parseSignRequest({ index, nonce: 18446744073709551615n.toString(), mints: [mintA, mintA], dryRun: "yes" });
  assert.ok(parsed.index.equals(INDEX));
  assert.equal(parsed.nonce, 18446744073709551615n);
  assert.deepEqual(parsed.mints, [mintA]);
  assert.equal(parsed.dryRun, false);
  assert.equal(parseSignRequest({ index, nonce: 5, mints: [mintA] }).nonce, 5n);
  assert.equal(bearerMatches(`Bearer ${TOKEN}`, TOKEN), true);
  assert.equal(bearerMatches(TOKEN, TOKEN), true);
  assert.equal(bearerMatches("Bearer nope", TOKEN), false);
  assert.equal(bearerMatches(undefined, TOKEN), false);
  assert.equal(bearerMatches("Bearer ", ""), false, "an empty token never matches");
});

test("the keeper checks the program accepts the service's key and it is not the keeper's own", async () => {
  const keeper = Keypair.generate().publicKey;
  const programAccepting = (key) => ({
    account: { priceOracle: { fetchNullable: async () => (key ? { oracle: new PublicKey(key) } : null) } },
  });
  const envFor = (url, accepted = ORACLE) => ({ oracleUrl: url, payer: { publicKey: keeper }, program: programAccepting(accepted) });
  await serve(fakeService().deps, async (url) => {
    assert.equal(await oracleProblems(envFor(url)), null);
    assert.match(await oracleProblems(envFor(url, null)), /price oracle is not set yet/);
    assert.match(await oracleProblems(envFor(url, mintB)), new RegExp(`accepts prices signed by ${mintB}, not the oracle service's ${ORACLE}`));
  });
  const sameKey = fakeService({ status: async () => ({ ok: true, oracle: keeper.toBase58(), accepted: true }) });
  await serve(sameKey.deps, async (url) => {
    assert.match(await oracleProblems(envFor(url, keeper.toBase58())), /keeper's own key/);
  });
  assert.match(await oracleProblems(envFor("http://127.0.0.1:1")), /not healthy/);
  // Without the service, the bot's local key must be the one the program accepts.
  const local = Keypair.generate();
  const localEnv = (accepted) => ({ oracle: local, payer: { publicKey: keeper }, program: programAccepting(accepted) });
  assert.equal(await oracleProblems(localEnv(local.publicKey.toBase58())), null);
  assert.match(await oracleProblems(localEnv(ORACLE)), /not this oracle key/);
  assert.match(await oracleProblems({ payer: { publicKey: keeper }, program: programAccepting(ORACLE) }), /no oracle key/);
});
