import test from "node:test";
import assert from "node:assert/strict";
import fs from "node:fs";
import anchor from "@coral-xyz/anchor";
import { Keypair, PublicKey, SystemProgram, TransactionInstruction } from "@solana/web3.js";
import { compiledSize, PROGRAM_ID, PRICE_BOARD, sendV0, executeBatchIx, postPricesIx, isStalePriceError, assertLegsWithinOracle, affordableBuys, buyAmount } from "../scripts/rebalance-bot.mjs";
import { OraclePriceError, PRICE_SCALE, USDC_MINT, oraclePrices } from "../scripts/lib/price-oracle.mjs";

test("keeper address matches program source and batch builds with current IDL", async () => {
  const source = fs.readFileSync("programs/basket/src/lib.rs", "utf8");
  assert.ok(source.includes(`declare_id!("${PROGRAM_ID}")`));
  const idl = JSON.parse(fs.readFileSync("target/idl/basket.json"));
  assert.equal(idl.address, PROGRAM_ID.toBase58());
  const pk = PublicKey.default;
  const program = new anchor.Program(idl, { connection: {}, publicKey: pk });
  const ctx = { keeper: pk, index: pk, intent: pk, vaultAuthority: pk, vaultQuote: pk };
  assert.ok(PRICE_BOARD.equals(PublicKey.findProgramAddressSync([Buffer.from("price-board")], PROGRAM_ID)[0]));
  for (const method of ["executeRebalanceSellBatch", "executeRebalanceBuyBatch"]) {
    const ix = await executeBatchIx(program, ctx, method, []);
    assert.ok(ix.keys.some((k) => k.pubkey.equals(PRICE_BOARD) && !k.isWritable));
    const decoded = program.coder.instruction.decode(ix.data);
    assert.equal(decoded.data.args.maxPriceAgeSlots.toNumber(), 150);
    assert.equal(decoded.data.args.maxOracleSlippageBps, 400);
  }
});

test("posted prices encode exactly, signed by the oracle and written to the board", async () => {
  const idl = JSON.parse(fs.readFileSync("target/idl/basket.json"));
  const program = new anchor.Program(idl, { connection: {}, publicKey: PublicKey.default });
  const oracle = Keypair.generate().publicKey;
  const mint = Keypair.generate().publicKey.toBase58();
  const scaled = 123_456_789_012_345_678_901n; // $123.456789012345678901
  const ix = await postPricesIx(program, oracle, [[mint, { scaled }]]);
  assert.ok(ix.keys.some((k) => k.pubkey.equals(oracle) && k.isSigner));
  assert.ok(ix.keys.some((k) => k.pubkey.equals(PRICE_BOARD) && k.isWritable));
  const decoded = program.coder.instruction.decode(ix.data);
  assert.equal(decoded.name, "postPrices");
  assert.equal(decoded.data.args.prices[0].mint.toBase58(), mint);
  assert.equal(decoded.data.args.prices[0].price.toString(), scaled.toString());
});

// --- price oracle --------------------------------------------------------------

const TOKEN = "Tok1111111111111111111111111111111111111111";
// A fake Jupiter/DexScreener: buying with $50 returns `bought` tokens (6 decimals), selling
// them returns `back` USDC atoms; the price API and DexScreener report the given prices.
function fakeMarket({ bought = 25_000_000n, back = 49_900_000n, jupiter = 2, jupiterSlot = 1_000, dexscreener = 2, failQuotes = false } = {}) {
  const calls = [];
  const fetchJson = async (url) => {
    calls.push(url);
    if (url.includes("/quote?")) {
      if (failQuotes) throw new Error("no route");
      const sell = url.includes(`inputMint=${TOKEN}`);
      return { outAmount: String(sell ? back : bought), contextSlot: 1_100 };
    }
    if (url.startsWith("https://price.test")) return jupiter ? { [TOKEN]: { usdPrice: jupiter, blockId: jupiterSlot } } : {};
    if (url.includes("dexscreener")) return dexscreener ? [{ baseToken: { address: TOKEN }, priceUsd: String(dexscreener), liquidity: { usd: 1e6 } }] : [];
    throw new Error(`unexpected ${url}`);
  };
  return { calls, env: { swapApi: "https://swap.test", priceApi: "https://price.test", fetchJson } };
}

test("oracle prices a token at its exact round-trip midpoint", async () => {
  const { env } = fakeMarket();
  const prices = await oraclePrices([{ mint: TOKEN, decimals: 6 }], env);
  const p = prices.get(TOKEN);
  // ($50 + $49.90) / (2 × 25 tokens) = $1.998
  assert.equal(p.scaled, (1_998n * PRICE_SCALE) / 1_000n);
  assert.equal(p.spreadBps, 20);
  assert.deepEqual(p.confirmedBy, ["jupiter-price", "dexscreener"]);
});

test("oracle refuses prices a reference disputes, and too-wide or gaining round trips", async () => {
  await assert.rejects(oraclePrices([{ mint: TOKEN, decimals: 6, label: "TOK" }], fakeMarket({ jupiter: 2.2, dexscreener: 1.7 }).env),
    (e) => e instanceof OraclePriceError && /TOK: round trip .* disagrees/.test(e.message));
  // One reference agreeing is not enough when the other disputes the price.
  await assert.rejects(oraclePrices([{ mint: TOKEN, decimals: 6 }], fakeMarket({ jupiter: 2, dexscreener: 2.2 }).env), /disagrees .* dexscreener \$2\.2/);
  await assert.rejects(oraclePrices([{ mint: TOKEN, decimals: 6 }], fakeMarket({ jupiter: 0, dexscreener: 0 }).env), /no reference price/);
  // A round trip costing over 4% could be priced but never traded within the per-leg bound.
  await assert.rejects(oraclePrices([{ mint: TOKEN, decimals: 6 }], fakeMarket({ back: 47_500_000n }).env), /spread 5\.00% is too wide/);
  // Gaining money on a round trip means a pool is off.
  await assert.rejects(oraclePrices([{ mint: TOKEN, decimals: 6 }], fakeMarket({ back: 50_400_000n }).env), /gains 0\.80%/);
  await assert.rejects(oraclePrices([{ mint: TOKEN, decimals: 6 }], fakeMarket({ failQuotes: true }).env), /no route/);
  await assert.rejects(oraclePrices([{ mint: USDC_MINT, decimals: 6 }], fakeMarket().env), /USDC is priced at \$1 on chain/);
});

test("a stale price API reading cannot confirm, but a fresh DexScreener one can", async () => {
  // Jupiter's price last updated over an hour of slots before the quote.
  const stale = { jupiter: 1.998, jupiterSlot: 1_100 - 9_001 };
  await assert.rejects(oraclePrices([{ mint: TOKEN, decimals: 6 }], fakeMarket({ ...stale, dexscreener: 0 }).env), /no reference price/);
  const prices = await oraclePrices([{ mint: TOKEN, decimals: 6 }], fakeMarket({ ...stale, dexscreener: 2.01 }).env);
  assert.deepEqual(prices.get(TOKEN).confirmedBy, ["dexscreener"]);
});

test("failed confirmation rejects instead of advancing keeper", async () => {
  const payer = Keypair.generate();
  const connection = {
    getLatestBlockhash: async () => ({ blockhash: PublicKey.default.toBase58(), lastValidBlockHeight: 1 }),
    sendTransaction: async () => "test-signature",
    confirmTransaction: async () => ({ value: { err: { InstructionError: [0, { Custom: 1 }] } } }),
  };
  await assert.rejects(sendV0(connection, payer, [SystemProgram.transfer({
    fromPubkey: payer.publicKey, toPubkey: Keypair.generate().publicKey, lamports: 1,
  })], "swap"), /transaction test-signature failed/);
});

test("fee-bearing buys fit proceeds through bounded target adjustment", async () => {
  const components = [{ globalIndex: 0 }];
  const build = async (c, atoms) => ({ atoms, entry: { quoteLimit: new anchor.BN(((atoms * 1003n + 999n) / 1000n).toString()) } });
  const entries = await affordableBuys(components, [100_000_000n], [400_000_000n], 50, 99_700_000n, build);
  assert.ok(BigInt(entries[0].entry.quoteLimit.toString()) <= 99_700_000n);
  assert.ok(entries[0].atoms >= buyAmount(400_000_000n, 100_000_000n, 50));
  assert.ok(entries[0].atoms < 100_000_000n);
  await assert.rejects(affordableBuys(components, [100_000_000n], [400_000_000n], 50, 90_000_000n, build), /Buy budget exceeds/);
});

test("affordable full targets are preserved and tiny buy legs stay nonzero", async () => {
  const entries = await affordableBuys([{ globalIndex: 0 }], [100n], [400n], 50, 100n,
    async (c, atoms) => ({ atoms, entry: { quoteLimit: atoms } }));
  assert.equal(entries[0].atoms, 100n);
  assert.equal(buyAmount(1_000_000n, 1n, 50), 1n);
});


test("oversized transaction candidates are rejected without aborting batch packing", () => {
  const payer = Keypair.generate();
  const ix = new TransactionInstruction({ programId: SystemProgram.programId, keys: [], data: Buffer.alloc(2000) });
  assert.equal(compiledSize(payer, [ix]), Infinity);
  assert.ok(compiledSize(payer, [SystemProgram.transfer({ fromPubkey: payer.publicKey, toPubkey: PublicKey.default, lamports: 1 })]) < 1232);
});

test("a price that aged out is retried whether it failed in preflight or after landing", () => {
  const idl = JSON.parse(fs.readFileSync("target/idl/basket.json"));
  const program = new anchor.Program(idl, { connection: {}, publicKey: PublicKey.default });
  const code = (name) => idl.errors.find((e) => e.name.toLowerCase() === name.toLowerCase()).code;
  const stale = code("StaleOraclePrice"), missing = code("MissingOraclePrice");
  assert.ok(isStalePriceError(program, new Error("Simulation failed. Error Code: StaleOraclePrice. Error Number: 6066.")));
  assert.ok(isStalePriceError(program, new Error(`finalize rebalance: transaction x failed: {"InstructionError":[2,{"Custom":${stale}}]}`)));
  assert.ok(isStalePriceError(program, new Error(`custom program error: 0x${missing.toString(16)}`)));
  assert.ok(!isStalePriceError(program, new Error(`{"InstructionError":[2,{"Custom":${code("ExecutionPriceOutsideOracleTolerance")}}]}`)));
});

test("legs are checked against the posted price at their worst allowed fill before any swap", () => {
  const mint = Keypair.generate().publicKey;
  const component = { mint, decimals: 6, globalIndex: 0 };
  const prices = new Map([[mint.toBase58(), { scaled: 2n * PRICE_SCALE }]]); // $2
  const leg = (quoteLimit) => ({ component, atoms: 10_000_000n, entry: { quoteLimit: new anchor.BN(quoteLimit) } });
  // 10 tokens at $2 with a 4% bound: sells must get at least $19.20, buys pay at most $20.80.
  assertLegsWithinOracle([leg(19_200_000)], prices, "sell");
  assert.throws(() => assertLegsWithinOracle([leg(19_190_000)], prices, "sell"), /sell could fill at \$1\.919.*outside 4\.00%/);
  assertLegsWithinOracle([leg(20_800_000)], prices, "buy");
  assert.throws(() => assertLegsWithinOracle([leg(20_810_000)], prices, "buy"), /buy could fill/);
  assert.throws(() => assertLegsWithinOracle([leg(20_000_000)], new Map(), "buy"), /no posted price/);
});
