import test from "node:test";
import assert from "node:assert/strict";
import fs from "node:fs";
import http from "node:http";
import anchor from "@coral-xyz/anchor";
import {
  AddressLookupTableAccount,
  AddressLookupTableProgram,
  ComputeBudgetProgram,
  Ed25519Program,
  Keypair,
  PublicKey,
  SystemProgram,
  SYSVAR_INSTRUCTIONS_PUBKEY,
  TransactionInstruction,
} from "@solana/web3.js";
import { TOKEN_PROGRAM_ID } from "@solana/spl-token";
import {
  compiledSize, PROGRAM_ID, PRICE_ORACLE, sendV0, executeBatchIx, openIx, finalizeIx, packBatches, sendWithFreshPrices,
  waitForSlot, isStalePriceError, assertLegsWithinOracle, affordableBuys, buyAmount, intentPda, basketLookupAddresses,
  ensureBasketLookupTable, pricedStepFootprint, fitsOneTransaction, buyQuote, closeIntent, closeSettledRebalanceIntents,
  noLegExecuted, fetchWritten,
} from "../scripts/rebalance-bot.mjs";
import { OraclePriceError, PRICE_SCALE, USDC_MINT, fetchJson, jupiterApis, oraclePrices } from "../scripts/lib/price-oracle.mjs";
import {
  MAX_PRICE_AGE_SLOTS,
  quantizePrice,
  decodePriceMessage,
  encodePriceMessage,
  placeholderPriceInstruction,
  priceSignatureInstruction,
  rebalanceIntentAddress,
  signPriceMessage,
  verifyPriceSignature,
} from "../scripts/lib/signed-prices.mjs";

const idl = JSON.parse(fs.readFileSync("target/idl/basket.json"));
const program = new anchor.Program(idl, { connection: {}, publicKey: PublicKey.default });
const errorCode = (name) => idl.errors.find((e) => e.name.toLowerCase() === name.toLowerCase()).code;

test("keeper address matches program source and every priced step carries the price oracle and instructions sysvar", async () => {
  const source = fs.readFileSync("programs/basket/src/lib.rs", "utf8");
  assert.ok(source.includes(`declare_id!("${PROGRAM_ID}")`));
  assert.equal(idl.address, PROGRAM_ID.toBase58());
  assert.match(fs.readFileSync("programs/basket/src/constants.rs", "utf8"), new RegExp(`MAX_PRICE_AGE_SLOTS: u64 = ${MAX_PRICE_AGE_SLOTS};`));
  assert.ok(PRICE_ORACLE.equals(PublicKey.findProgramAddressSync([Buffer.from("price-oracle")], PROGRAM_ID)[0]));
  const pk = PublicKey.default;
  const ctx = { keeper: pk, index: pk, indexMint: pk, intent: pk, nonce: new anchor.BN(1), vaultAuthority: pk, vaultQuote: pk };
  const carries = (ix) => {
    assert.ok(ix.keys.some((k) => k.pubkey.equals(PRICE_ORACLE) && !k.isWritable));
    assert.ok(ix.keys.some((k) => k.pubkey.equals(SYSVAR_INSTRUCTIONS_PUBKEY) && !k.isWritable));
    return program.coder.instruction.decode(ix.data).data.args;
  };
  for (const method of ["executeRebalanceSellBatch", "executeRebalanceBuyBatch"]) {
    const args = carries(await executeBatchIx(program, ctx, method, []));
    assert.equal(args.maxPriceAgeSlots.toNumber(), MAX_PRICE_AGE_SLOTS);
    assert.equal(args.maxOracleSlippageBps, 400);
  }
  assert.equal(carries(await openIx(program, ctx, [], [], { expiresAt: 1, maxPostRebalanceDriftBps: 100 })).maxPriceAgeSlots.toNumber(), MAX_PRICE_AGE_SLOTS);
  assert.equal(carries(await finalizeIx(program, ctx, [], [])).maxPriceAgeSlots.toNumber(), MAX_PRICE_AGE_SLOTS);
});

test("signed price messages match the program's layout and round-trip exactly", () => {
  const intent = Keypair.generate().publicKey;
  const price = quantizePrice(123_456_789_012_345_678_901n); // $123.456789012345678901
  assert.equal(price, 123_456_789_000_000_000_000n, "kept to 10 significant digits");
  const message = encodePriceMessage({ intent, slot: 2n ** 40n + 3n, entries: [
    { componentIndex: 7, price: 1n },
    { componentIndex: 2, price },
  ] });
  assert.equal(message.length, 57 + 2 * 6);
  assert.equal(message.subarray(0, 16).toString(), "basket-prices-v1");
  assert.ok(new PublicKey(message.subarray(16, 48)).equals(intent));
  assert.equal(message.readBigUInt64LE(48), 2n ** 40n + 3n);
  assert.equal(message[56], 2);
  assert.equal(message[57], 2, "entries are sorted by component index");
  assert.equal(message.readUInt32LE(58), 1_234_567_890, "mantissa");
  assert.equal(message[62], 11, "exponent: $123.456789 = 1234567890 × 10^11 at 1e18");
  assert.deepEqual([...message.subarray(63, 69)], [7, 1, 0, 0, 0, 0]);
  assert.deepEqual(decodePriceMessage(message).entries, [{ componentIndex: 2, price }, { componentIndex: 7, price: 1n }]);
  const entry = (componentIndex, p) => () => encodePriceMessage({ intent, slot: 1, entries: [{ componentIndex, price: p }] });
  assert.throws(entry(0, 0n), /bad price/);
  assert.throws(entry(0, 2n ** 127n), /bad price/, "the program reads prices as i128");
  assert.throws(entry(0, 123_456_789_012n), /quantizePrice it first/, "12 significant digits do not fit");
  assert.throws(entry(256, 1n), /bad component index/);
  assert.throws(() => encodePriceMessage({ intent, slot: 1, entries: [{ componentIndex: 1, price: 1n }, { componentIndex: 1, price: 2n }] }), /twice/);
  // The intent PDA the oracle signs for is the one open creates.
  assert.ok(rebalanceIntentAddress(PROGRAM_ID, intent, 42n).equals(intentPda(intent, new anchor.BN(42))));
  // The Ed25519 instruction keeps key, signature and message in its own data; the stand-in
  // used for sizing is exactly as big.
  const oracle = Keypair.generate();
  const signature = signPriceMessage(oracle.secretKey, message);
  assert.ok(verifyPriceSignature(oracle.publicKey, message, signature));
  assert.ok(!verifyPriceSignature(Keypair.generate().publicKey, message, signature));
  const ix = priceSignatureInstruction({ oracle: oracle.publicKey, message, signature });
  assert.ok(ix.programId.equals(Ed25519Program.programId));
  assert.equal(ix.data[0], 1);
  for (const field of [1, 3, 6]) assert.equal(ix.data.readUInt16LE(2 + 2 * field), 0xffff);
  assert.ok(new PublicKey(ix.data.subarray(ix.data.readUInt16LE(6), ix.data.readUInt16LE(6) + 32)).equals(oracle.publicKey));
  assert.ok(ix.data.subarray(ix.data.readUInt16LE(10)).equals(message));
  assert.equal(placeholderPriceInstruction(2).data.length, ix.data.length);
});

test("compact prices keep at least 9 significant digits, far inside the oracle's bps-level checks", () => {
  // Every magnitude a token price could take, from 1e-15 dollars to $10^15, with many digits.
  let worst = 0;
  for (let exponent = 3; exponent <= 33; exponent += 1) {
    for (const digits of [123456789123456789n, 999999999999999999n, 100000000500000000n, 314159265358979323n]) {
      const price = (digits * 10n ** BigInt(exponent)) / 10n ** 17n || 1n;
      const q = quantizePrice(price);
      encodePriceMessage({ intent: PublicKey.default, slot: 1, entries: [{ componentIndex: 0, price: q }] }); // carried exactly
      const error = Number((q > price ? q - price : price - q) * 10n ** 12n / price) / 1e12;
      worst = Math.max(worst, error);
    }
  }
  // Rounding to a u32 mantissa of at least 429,496,730 is off by at most half a unit: 1.2e-9,
  // about a millionth of one basis point.
  assert.ok(worst <= 1.2e-9, `worst relative rounding ${worst}`);
  assert.equal(quantizePrice(4_294_967_295n), 4_294_967_295n, "a u32 mantissa is kept whole");
  assert.equal(quantizePrice(4_294_967_296n), 4_294_967_300n, "one more digit rounds to the nearest ten");
  assert.equal(quantizePrice(42_949_672_949n), 42_949_672_950n);
  assert.throws(() => quantizePrice(0n), /bad price/);
});

// --- a priced step on a fake connection ------------------------------------------------

const keeper = Keypair.generate();
const pk = () => Keypair.generate().publicKey;
const component = (globalIndex, mint, isQuote = false) => ({ globalIndex, mint, isQuote, retired: false, decimals: 6, vault: pk(), pagePda: pk(), tokenProgram: SystemProgram.programId });
const mintA = pk();
const mintB = pk();
const components = [component(0, mintA), component(1, USDC_MINT, true), component(2, mintB)];

function stepContext() {
  const index = pk();
  const nonce = new anchor.BN(99);
  return { keeper: keeper.publicKey, index, indexMint: pk(), vaultAuthority: pk(), vaultQuote: pk(), nonce, intent: intentPda(index, nonce) };
}

// Records every transaction sent; `outcomes` decides how each one confirms.
function fakeChain(outcomes = []) {
  const sent = [];
  let slot = 5_000;
  const connection = {
    getSlot: async () => (slot += 10),
    getLatestBlockhash: async () => ({ blockhash: PublicKey.default.toBase58(), lastValidBlockHeight: 1 }),
    sendTransaction: async (tx) => {
      if (!sent.includes(tx)) sent.push(tx);
      return `sig${sent.length}`;
    },
    confirmTransaction: async () => ({ value: { err: outcomes[sent.length - 1] ?? null } }),
  };
  const oracle = Keypair.generate();
  const env = {
    connection,
    program,
    payer: keeper,
    oracle,
    priceTokens: async (tokens) => new Map(tokens.map((t) => [t.mint, { scaled: t.mint === mintA.toBase58() ? 2n * PRICE_SCALE : PRICE_SCALE / 2n, usd: 1, spreadBps: 1, confirmedBy: [] }])),
  };
  return { env, sent, oracle };
}

function instructionsOf(tx) {
  const keys = tx.message.staticAccountKeys;
  return tx.message.compiledInstructions.map((ix) => ({ programId: keys[ix.programIdIndex], data: Buffer.from(ix.data) }));
}

test("a step goes out as [budget, Ed25519 signature, step], signed for its own intent", async () => {
  const { env, sent, oracle } = fakeChain();
  const ctx = stepContext();
  const prices = await sendWithFreshPrices(env, ctx, components, "finalize", () => finalizeIx(program, ctx, components, [pk()]), 600_000);
  assert.equal(prices.get(mintA.toBase58()).scaled, 2n * PRICE_SCALE);
  assert.equal(sent.length, 1);
  const ixs = instructionsOf(sent[0]);
  assert.deepEqual(ixs.map((ix) => ix.programId.toBase58()), [
    ComputeBudgetProgram.programId.toBase58(),
    ComputeBudgetProgram.programId.toBase58(),
    Ed25519Program.programId.toBase58(),
    PROGRAM_ID.toBase58(),
  ]);
  const data = ixs[2].data;
  const message = data.subarray(data.readUInt16LE(10), data.readUInt16LE(10) + data.readUInt16LE(12));
  const decoded = decodePriceMessage(message);
  assert.ok(decoded.intent.equals(ctx.intent));
  assert.equal(decoded.slot, 5_010);
  // Components by index; the USDC cash slot (1) is never signed.
  assert.deepEqual(decoded.entries, [{ componentIndex: 0, price: 2n * PRICE_SCALE }, { componentIndex: 2, price: PRICE_SCALE / 2n }]);
  const signature = data.subarray(data.readUInt16LE(2), data.readUInt16LE(2) + 64);
  assert.ok(verifyPriceSignature(oracle.publicKey, message, signature));
});

test("a step whose prices aged out before it landed is re-signed and resent", async () => {
  const stale = { InstructionError: [3, { Custom: errorCode("StaleOraclePrice") }] };
  const { env, sent } = fakeChain([stale, null]);
  const ctx = stepContext();
  await sendWithFreshPrices(env, ctx, components, "finalize", () => finalizeIx(program, ctx, components, [pk()]), 600_000);
  assert.equal(sent.length, 2);
  const slotOf = (tx) => decodePriceMessage(instructionsOf(tx)[2].data.subarray(112)).slot;
  assert.ok(slotOf(sent[1]) > slotOf(sent[0]), "the resend carries freshly signed prices");
  // Anything else is not retried.
  const other = fakeChain([{ InstructionError: [3, { Custom: errorCode("RebalanceNavMismatch") }] }]);
  await assert.rejects(sendWithFreshPrices(other.env, ctx, components, "finalize", () => finalizeIx(program, ctx, components, [pk()]), 600_000), /failed/);
  assert.equal(other.sent.length, 1);
});

test("a step waits for the keeper's RPC to reach the slot its prices carry", async () => {
  // An RPC two polls behind the oracle's.
  let slot = 98;
  const behind = { getSlot: async () => (slot += 1) };
  await waitForSlot(behind, 101, "step");
  assert.equal(slot, 101);
  // One that never catches up within the wait is sent to anyway; the resend re-signs.
  let calls = 0;
  const stuck = { getSlot: async () => ((calls += 1), 50) };
  const started = Date.now();
  await waitForSlot(stuck, 101, "step", 300);
  assert.ok(Date.now() - started < 2_000 && calls >= 1);
});

// A Jupiter quote for one token: it sells at `bid` and buys at `ask` USDC atoms per token atom,
// can't route ExactOut buys unless `exactOut`, and keeps `slippageBps` off each minimum.
function fakeJupiter({ bid = 0.1, ask = 0.1003, exactOut = false, slippageBps = 100 } = {}) {
  const calls = [];
  const quote = async ({ inputMint, amount, swapMode }) => {
    const buying = inputMint.toBase58() === USDC_MINT;
    calls.push(`${buying ? "buy" : "sell"} ${swapMode} ${amount}`);
    const atoms = BigInt(amount);
    if (buying && swapMode === "ExactOut") {
      if (!exactOut) throw new Error('https://api.jup.ag/swap/v1/quote?... -> 400: {"error":"No routes found","errorCode":"NO_ROUTES_FOUND"}');
      const inAmount = BigInt(Math.ceil(Number(atoms) * ask));
      return { swapMode, inAmount: String(inAmount), outAmount: String(atoms), otherAmountThreshold: String((inAmount * BigInt(10_000 + slippageBps)) / 10_000n) };
    }
    const out = BigInt(Math.floor(buying ? Number(atoms) / ask : Number(atoms) * bid));
    return { swapMode: "ExactIn", inAmount: String(atoms), outAmount: String(out), otherAmountThreshold: String((out * BigInt(10_000 - slippageBps)) / 10_000n) };
  };
  return { calls, quote };
}

test("a buy with no ExactOut route spends an exact input whose minimum output still covers the leg", async () => {
  const c = component(4, mintA);
  const leg = 606_823n;
  for (const ask of [0.1003, 0.103]) { // a tight market, and one 3% wider than the bid
    const jup = fakeJupiter({ ask });
    const q = await buyQuote(c, leg, 16, jup.quote);
    assert.equal(q.swapMode, "ExactIn");
    assert.ok(BigInt(q.otherAmountThreshold) >= leg, `ask ${ask}: guarantees the leg`);
    // It pays about what the leg costs plus the slippage margin, nowhere near the 4% bound.
    const cost = Number(leg) * ask;
    assert.ok(Number(q.inAmount) <= cost * 1.025, `ask ${ask}: ${q.inAmount} for a ${cost} leg`);
    assert.deepEqual(jup.calls.slice(0, 2), [`buy ExactOut ${leg}`, `sell ExactIn ${leg}`]);
  }
  // Where ExactOut routes, it is used as before, with no extra quotes.
  const routed = fakeJupiter({ exactOut: true });
  assert.equal((await buyQuote(c, leg, 16, routed.quote)).swapMode, "ExactOut");
  assert.deepEqual(routed.calls, [`buy ExactOut ${leg}`]);
  // Any other failure is not taken for a missing route.
  const down = async () => { throw new Error("https://api.jup.ag/swap/v1/quote -> 503: unavailable"); };
  await assert.rejects(buyQuote(c, leg, 16, down), /503/);
});

test("closing a just-cancelled intent retries while the RPC still sees it open, and the sweep closes leftovers", async () => {
  let refusals = 1;
  const sent = [];
  const connection = {
    getLatestBlockhash: async () => ({ blockhash: PublicKey.default.toBase58(), lastValidBlockHeight: 1 }),
    sendTransaction: async (tx) => {
      if (refusals > 0) {
        refusals -= 1;
        throw Object.assign(new Error("Simulation failed."), { logs: ["Program log: AnchorError caused by account: intent. Error Code: RebalanceIntentStillOpen."] });
      }
      sent.push(tx);
      return `sig${sent.length}`;
    },
    confirmTransaction: async () => ({ value: { err: null } }),
  };
  assert.equal(await closeIntent(connection, program, keeper, pk(), { retryDelayMs: 10 }), true);
  assert.equal(sent.length, 1);
  // A refusal that persists gives up, leaving the rent for the sweep.
  refusals = 10;
  assert.equal(await closeIntent(connection, program, keeper, pk(), { attempts: 3, retryDelayMs: 10 }), false);
  // The sweep closes this keeper's settled intents and leaves open ones alone.
  refusals = 0;
  sent.length = 0;
  let filter;
  const intents = [["open", pk()], ["cancelled", pk()], ["finalized", pk()]].map(([status, publicKey]) => ({ publicKey, account: { status: { [status]: {} } } }));
  const sweepProgram = { methods: program.methods, account: { rebalanceIntent: { all: async (filters) => ((filter = filters[0].memcmp), intents) } } };
  await closeSettledRebalanceIntents({ connection, program: sweepProgram, payer: keeper });
  assert.deepEqual(filter, { offset: 40, bytes: keeper.publicKey.toBase58() });
  assert.equal(sent.length, 2);
});

test("an intent whose legs never started is told apart, and a just-opened intent is read once visible", async () => {
  // Six components: sells at 0 and 2, a buy at 4. Components without a leg count as completed.
  const bitmap = (...bits) => [bits.reduce((byte, i) => byte | (1 << i), 0), 0, 0, 0, 0, 0, 0];
  const intent = (completedSells, completedBuys) => ({ componentCount: 6, sellLegBitmap: bitmap(0, 2), buyLegBitmap: bitmap(4), completedSells, completedBuys });
  assert.equal(noLegExecuted(intent(4, 5)), true, "as opened");
  assert.equal(noLegExecuted(intent(5, 5)), false, "one sell done");
  assert.equal(noLegExecuted(intent(4, 6)), false, "the buy done");
  // An RPC that doesn't have the account for two reads.
  let reads = 0;
  const account = { fetchNullable: async () => ((reads += 1) > 2 ? { status: { open: {} } } : null) };
  assert.deepEqual(await fetchWritten(account, pk(), { delayMs: 1 }), { status: { open: {} } });
  assert.equal(reads, 3);
  await assert.rejects(fetchWritten({ fetchNullable: async () => null }, pk(), { attempts: 2, delayMs: 1 }), /not visible/);
});

test("batch packing counts the signature instruction each batch carries", async () => {
  const ctx = stepContext();
  const entry = (globalIndex, routeCount) => {
    const routeAccounts = Array.from({ length: routeCount }, () => ({ pubkey: pk(), isSigner: false, isWritable: true }));
    const c = component(globalIndex, pk());
    return {
      component: c,
      atoms: 1n,
      entry: { componentIndex: globalIndex, quoteLimit: new anchor.BN(1), routeAccountCount: routeCount, swap: { instructionData: Buffer.alloc(40), accounts: Buffer.alloc(routeCount + 10) } },
      remaining: [{ pubkey: c.pagePda, isSigner: false, isWritable: false }, { pubkey: c.mint, isSigner: false, isWritable: false }, { pubkey: c.vault, isSigner: false, isWritable: true }, { pubkey: c.tokenProgram, isSigner: false, isWritable: false }, ...routeAccounts],
      lookupTables: [],
    };
  };
  const sizeOf = async (group, withSignature) => compiledSize(keeper, [
    ComputeBudgetProgram.setComputeUnitLimit({ units: 1_400_000 }),
    ComputeBudgetProgram.setComputeUnitPrice({ microLamports: 5000 }),
    ...(withSignature ? [placeholderPriceInstruction(group.length)] : []),
    await executeBatchIx(program, ctx, "executeRebalanceSellBatch", group),
  ]);
  // Find route sizes where two legs fit one transaction only without the signature.
  let pair;
  for (let routes = 0; routes < 20 && !pair; routes += 1) {
    const candidate = [entry(0, routes), entry(2, routes)];
    if ((await sizeOf(candidate, false)) <= 1232 && (await sizeOf(candidate, true)) > 1232) pair = candidate;
  }
  assert.ok(pair, "found legs that fit two to a transaction only without the signature");
  const batches = await packBatches({}, program, ctx, "executeRebalanceSellBatch", pair);
  assert.deepEqual(batches.map((b) => b.length), [1, 1]);
  // The basket's lookup table takes the shared and per-leg accounts down to a byte each, so the
  // same legs fit two to a transaction again.
  const pages = pair.map((b) => b.component.pagePda);
  ctx.lookupTable = tableFor(basketLookupAddresses(ctx, pair.map((b) => b.component), pages));
  assert.deepEqual((await packBatches({}, program, ctx, "executeRebalanceSellBatch", pair)).map((b) => b.length), [2]);
});

// --- the basket's lookup table ---------------------------------------------------------------

function tableFor(addresses, key = pk()) {
  return new AddressLookupTableAccount({ key, state: { deactivationSlot: 2n ** 64n - 1n, lastExtendedSlot: 0, lastExtendedSlotStartIndex: 0, authority: keeper.publicKey, addresses } });
}

function basketFixture(componentCount) {
  const ctx = { keeper: keeper.publicKey, index: pk(), indexMint: pk(), vaultAuthority: pk(), vaultQuote: pk(), nonce: new anchor.BN(1), intent: pk() };
  const components = Array.from({ length: componentCount }, (_, i) => {
    const isQuote = i === componentCount - 1;
    return { globalIndex: i, mint: isQuote ? new PublicKey(USDC_MINT) : pk(), isQuote, retired: false, decimals: 6, vault: isQuote ? ctx.vaultQuote : pk(), tokenProgram: TOKEN_PROGRAM_ID };
  });
  const pagePdas = Array.from({ length: Math.ceil(componentCount / 10) }, pk);
  return { ctx, components, pagePdas };
}

// A chain that keeps lookup tables: it applies the create and extend instructions the keeper
// sends, records every transaction, and serves tables by authority (getProgramAccounts) or key.
function tableChain({ failSearch = false } = {}) {
  const tables = new Map(); // key -> { authority, addresses, lastExtendedSlot }
  const sent = [];
  let slot = 1_000;
  const accountData = (t) => {
    const data = Buffer.alloc(56 + 32 * t.addresses.length);
    data.writeUInt32LE(1, 0);
    data.writeBigUInt64LE(2n ** 64n - 1n, 4);
    data.writeBigUInt64LE(BigInt(t.lastExtendedSlot), 12);
    data[21] = 1;
    t.authority.toBuffer().copy(data, 22);
    t.addresses.forEach((a, i) => a.toBuffer().copy(data, 56 + 32 * i));
    return data;
  };
  const apply = (tx) => {
    const keys = tx.message.staticAccountKeys;
    for (const ix of tx.message.compiledInstructions) {
      if (!keys[ix.programIdIndex].equals(AddressLookupTableProgram.programId)) continue;
      const data = Buffer.from(ix.data);
      const table = keys[ix.accountKeyIndexes[0]].toBase58();
      if (data.readUInt32LE(0) === 0) tables.set(table, { authority: keys[ix.accountKeyIndexes[1]], addresses: [], lastExtendedSlot: slot });
      if (data.readUInt32LE(0) === 2) {
        const count = Number(data.readBigUInt64LE(4));
        for (let i = 0; i < count; i += 1) tables.get(table).addresses.push(new PublicKey(data.subarray(12 + 32 * i, 44 + 32 * i)));
        tables.get(table).lastExtendedSlot = slot;
      }
    }
  };
  const connection = {
    getSlot: async () => (slot += 1),
    getProgramAccounts: async (programId, { filters }) => {
      assert.ok(programId.equals(AddressLookupTableProgram.programId));
      assert.equal(filters[0].memcmp.offset, 22);
      if (failSearch) throw new Error("getProgramAccounts is disabled on this RPC");
      return [...tables]
        .filter(([, t]) => t.authority.toBase58() === filters[0].memcmp.bytes)
        .map(([key, t]) => ({ pubkey: new PublicKey(key), account: { data: accountData(t) } }));
    },
    getAddressLookupTable: async (address) => {
      const t = tables.get(address.toBase58());
      return { value: t ? new AddressLookupTableAccount({ key: address, state: AddressLookupTableAccount.deserialize(accountData(t)) }) : null };
    },
    getMinimumBalanceForRentExemption: async (bytes) => (bytes + 128) * 6960,
    getLatestBlockhash: async () => ({ blockhash: PublicKey.default.toBase58(), lastValidBlockHeight: 1 }),
    sendTransaction: async (tx) => {
      if (!sent.includes(tx)) {
        sent.push(tx);
        apply(tx);
      }
      return `sig${sent.length}`;
    },
    confirmTransaction: async () => ({ value: { err: null } }),
  };
  return { connection, tables, sent };
}

const lookupTableIxs = (tx) => tx.message.compiledInstructions
  .filter((ix) => tx.message.staticAccountKeys[ix.programIdIndex].equals(AddressLookupTableProgram.programId))
  .map((ix) => Buffer.from(ix.data).readUInt32LE(0));

test("a basket's lookup table holds every account its steps name, but no signer, intent or invoked program", () => {
  const { ctx, components, pagePdas } = basketFixture(12);
  const held = basketLookupAddresses(ctx, components, pagePdas).map((a) => a.toBase58());
  assert.equal(new Set(held).size, held.length, "no duplicates");
  for (const key of [ctx.index, ctx.indexMint, ctx.vaultAuthority, ctx.vaultQuote, PRICE_ORACLE, SYSVAR_INSTRUCTIONS_PUBKEY, TOKEN_PROGRAM_ID, ...pagePdas, ...components.flatMap((c) => [c.vault, c.mint])]) {
    assert.ok(held.includes(key.toBase58()), key.toBase58());
  }
  for (const key of [ctx.keeper, ctx.intent, PROGRAM_ID, ComputeBudgetProgram.programId, Ed25519Program.programId]) {
    assert.ok(!held.includes(key.toBase58()), key.toBase58());
  }
  // 11 shared accounts, 2 pages, and 11 vaults and mints besides the USDC slot's (its vault is the
  // shared quote account, its mint USDC).
  assert.equal(held.length, 11 + 2 + 11 + 11);
});

test("the keeper creates a basket's table once, finds it again, and extends it for new components", async () => {
  const { connection, sent } = tableChain();
  const env = { connection, payer: keeper };
  const { ctx, components, pagePdas } = basketFixture(30);
  const wanted = basketLookupAddresses(ctx, components, pagePdas);
  // A dry run only says what it would do.
  assert.equal(await ensureBasketLookupTable(env, ctx, components, pagePdas, { execute: false }), null);
  assert.equal(sent.length, 0);
  // Created with the first 20 addresses in one transaction, the rest 20 at a time, and returned
  // only once warm.
  const table = await ensureBasketLookupTable(env, ctx, components, pagePdas, { execute: true });
  assert.equal(wanted.length, 72);
  assert.deepEqual(sent.map(lookupTableIxs), [[0, 2], [2], [2], [2]]);
  assert.deepEqual(table.state.addresses.map((a) => a.toBase58()).sort(), wanted.map((a) => a.toBase58()).sort());
  assert.ok(table.state.authority.equals(keeper.publicKey));
  assert.ok(sent.every((tx) => tx.message.staticAccountKeys[0].equals(keeper.publicKey)), "the keeper pays");
  // Nothing more to do while it holds everything.
  assert.ok((await ensureBasketLookupTable(env, ctx, components, pagePdas, { execute: true })).key.equals(table.key));
  assert.equal(sent.length, 4);
  // A composition change adds a component: only its vault and mint are appended.
  const added = { vault: pk(), mint: pk(), tokenProgram: TOKEN_PROGRAM_ID };
  const extended = await ensureBasketLookupTable(env, ctx, [...components, added], pagePdas, { execute: true });
  assert.ok(extended.key.equals(table.key));
  assert.deepEqual(sent.slice(4).map(lookupTableIxs), [[2]]);
  assert.deepEqual(extended.state.addresses.slice(-2).map((a) => a.toBase58()), [added.vault.toBase58(), added.mint.toBase58()]);
});

test("a keeper that restarted finds its table by authority, and a failed search never creates one", async () => {
  const { connection, sent } = tableChain();
  const { ctx, components, pagePdas } = basketFixture(5);
  const first = await ensureBasketLookupTable({ connection, payer: keeper }, ctx, components, pagePdas, { execute: true });
  // Another basket's table by the same keeper is not this basket's.
  const other = basketFixture(5);
  await ensureBasketLookupTable({ connection, payer: keeper }, other.ctx, other.components, other.pagePdas, { execute: true });
  const count = sent.length;
  // A fresh process (no cache) for a basket whose index the cache never saw: same chain, found
  // by getProgramAccounts on the authority and picked by holding the basket's index.
  const restarted = await import(`../scripts/rebalance-bot.mjs?restart=${Date.now()}`);
  const found = await restarted.ensureBasketLookupTable({ connection, payer: keeper }, ctx, components, pagePdas, { execute: true });
  assert.ok(found.key.equals(first.key));
  assert.equal(sent.length, count, "nothing sent");
  // An RPC that cannot search must not lead to a second table.
  const blind = tableChain({ failSearch: true });
  const fresh = await import(`../scripts/rebalance-bot.mjs?blind=${Date.now()}`);
  await assert.rejects(fresh.ensureBasketLookupTable({ connection: blind.connection, payer: keeper }, ctx, components, pagePdas, { execute: true }), /getProgramAccounts is disabled/);
  assert.equal(blind.sent.length, 0);
});

test("open and finalize fit one transaction through the table up to 45 components; 46 lock too many accounts", async () => {
  for (const [componentCount, fits] of [[8, true], [45, true], [46, false]]) {
    const { ctx, components, pagePdas } = basketFixture(componentCount);
    ctx.lookupTable = tableFor(basketLookupAddresses(ctx, components, pagePdas));
    const open = await pricedStepFootprint(keeper, ctx, components, () => openIx(program, ctx, components, pagePdas, { expiresAt: 1, maxPostRebalanceDriftBps: 100 }), 400_000);
    const finalize = await pricedStepFootprint(keeper, ctx, components, () => finalizeIx(program, ctx, components, pagePdas), 600_000);
    assert.equal(fitsOneTransaction(open), fits, `open of ${componentCount}: ${JSON.stringify(open)}`);
    assert.ok(fitsOneTransaction(finalize), `finalize of ${componentCount}: ${JSON.stringify(finalize)}`);
    assert.ok(open.bytes < 1000, "bytes no longer bind");
  }
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

test("oracle prices several tokens at once, each at its own round trip", async () => {
  // Token i trades at $(i + 1): $50 buys 50 / (i + 1) of it, and selling that returns $49.90.
  const tokens = Array.from({ length: 20 }, () => ({ mint: Keypair.generate().publicKey.toBase58(), decimals: 6 }));
  const usdOf = new Map(tokens.map((t, i) => [t.mint, i + 1]));
  let inFlight = 0;
  let maxInFlight = 0;
  const fetchJson = async (url) => {
    const u = new URL(url);
    if (u.pathname.endsWith("/quote")) {
      inFlight += 1;
      maxInFlight = Math.max(maxInFlight, inFlight);
      await new Promise((r) => setTimeout(r, 20));
      inFlight -= 1;
      const buying = u.searchParams.get("inputMint") === USDC_MINT;
      const usd = usdOf.get(u.searchParams.get(buying ? "outputMint" : "inputMint"));
      return { outAmount: String(buying ? Math.round(50e6 / usd) : 49_900_000), contextSlot: 1_100 };
    }
    if (url.startsWith("https://price.test")) return {};
    const mints = u.pathname.split("/").pop().split(",");
    return mints.map((mint) => ({ baseToken: { address: mint }, priceUsd: String(usdOf.get(mint)), liquidity: { usd: 1e6 } }));
  };
  const prices = await oraclePrices(tokens, { swapApi: "https://swap.test", priceApi: "https://price.test", fetchJson });
  assert.equal(prices.size, 20);
  for (const t of tokens) assert.ok(Math.abs(prices.get(t.mint).usd / usdOf.get(t.mint) - 0.999) < 1e-3, t.mint);
  assert.ok(maxInFlight > 1 && maxInFlight <= 8, `at most 8 tokens at once, saw ${maxInFlight}`);
});

test("price sources: a refusal is final, rate limits and server errors are retried, and a hang is cut off", async () => {
  // Statuses the server answers with, in order (200 once they run out); "hang" never answers.
  const replies = [];
  let calls = 0;
  const server = http.createServer((req, res) => {
    calls += 1;
    const status = replies.shift() ?? 200;
    if (status === "hang") return;
    res.writeHead(status, { "content-type": "application/json" });
    res.end(JSON.stringify(status === 200 ? { ok: true } : { error: "nope" }));
  });
  await new Promise((resolve) => server.listen(0, "127.0.0.1", resolve));
  const url = `http://127.0.0.1:${server.address().port}/quote`;
  try {
    replies.push(400);
    await assert.rejects(fetchJson(url), /-> 400/);
    assert.equal(calls, 1, "a refusal (Jupiter finding no route) is not retried");
    calls = 0;
    replies.push(429, 503);
    assert.deepEqual(await fetchJson(url), { ok: true });
    assert.equal(calls, 3, "a rate limit and a server error are retried");
    calls = 0;
    replies.push("hang");
    const started = Date.now();
    await assert.rejects(fetchJson(url, { signal: AbortSignal.timeout(200) }));
    assert.ok(Date.now() - started < 2_000, "the caller's signal stops it without retrying");
    assert.equal(calls, 1);
  } finally {
    server.closeAllConnections();
    server.close();
  }
});

test("with JUPITER_API_KEY, Jupiter calls use the keyed API and only it gets the key", async () => {
  const realFetch = globalThis.fetch;
  const saved = { key: process.env.JUPITER_API_KEY, swap: process.env.JUPITER_SWAP_API, price: process.env.JUPITER_PRICE_API };
  const seen = [];
  globalThis.fetch = async (url, init) => {
    seen.push({ host: new URL(url).hostname, key: init.headers?.["x-api-key"], accept: init.headers?.accept });
    return new Response("{}", { status: 200 });
  };
  try {
    delete process.env.JUPITER_SWAP_API;
    delete process.env.JUPITER_PRICE_API;
    delete process.env.JUPITER_API_KEY;
    assert.deepEqual(jupiterApis(), { swapApi: "https://lite-api.jup.ag/swap/v1", priceApi: "https://lite-api.jup.ag/price/v3" });
    process.env.JUPITER_API_KEY = " test-key \r";
    assert.deepEqual(jupiterApis(), { swapApi: "https://api.jup.ag/swap/v1", priceApi: "https://api.jup.ag/price/v3" });
    const init = { headers: { accept: "application/json" } };
    await fetchJson("https://api.jup.ag/swap/v1/quote?amount=1", init);
    await fetchJson("https://lite-api.jup.ag/swap/v1/quote?amount=1", init);
    await fetchJson("https://api.dexscreener.com/tokens/v1/solana/x", init);
    assert.deepEqual(seen, [
      { host: "api.jup.ag", key: "test-key", accept: "application/json" },
      { host: "lite-api.jup.ag", key: undefined, accept: "application/json" },
      { host: "api.dexscreener.com", key: undefined, accept: "application/json" },
    ]);
    assert.equal(init.headers["x-api-key"], undefined, "the caller's headers are not changed");
  } finally {
    globalThis.fetch = realFetch;
    for (const [name, value] of [["JUPITER_API_KEY", saved.key], ["JUPITER_SWAP_API", saved.swap], ["JUPITER_PRICE_API", saved.price]]) {
      if (value === undefined) delete process.env[name];
      else process.env[name] = value;
    }
  }
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

test("prices that aged out are re-signed whether the step failed in preflight or after landing", () => {
  const stale = errorCode("StaleOraclePrice"), future = errorCode("SignedPriceSlotInFuture");
  assert.ok(isStalePriceError(program, new Error("Simulation failed. Error Code: StaleOraclePrice. Error Number: 6066.")));
  assert.ok(isStalePriceError(program, new Error(`finalize rebalance: transaction x failed: {"InstructionError":[3,{"Custom":${stale}}]}`)));
  assert.ok(isStalePriceError(program, new Error(`custom program error: 0x${future.toString(16)}`)));
  // Re-signing cannot fix a missing price, a bad signature or a bad fill.
  for (const name of ["MissingOraclePrice", "InvalidPriceSignature", "ExecutionPriceOutsideOracleTolerance"]) {
    assert.ok(!isStalePriceError(program, new Error(`{"InstructionError":[3,{"Custom":${errorCode(name)}}]}`)), name);
  }
});

test("legs are checked against the signed price at their worst allowed fill before any swap", () => {
  const mint = Keypair.generate().publicKey;
  const component = { mint, decimals: 6, globalIndex: 0 };
  const prices = new Map([[mint.toBase58(), { scaled: 2n * PRICE_SCALE }]]); // $2
  const leg = (quoteLimit) => ({ component, atoms: 10_000_000n, entry: { quoteLimit: new anchor.BN(quoteLimit) } });
  // 10 tokens at $2 with a 4% bound: sells must get at least $19.20, buys pay at most $20.80.
  assertLegsWithinOracle([leg(19_200_000)], prices, "sell");
  assert.throws(() => assertLegsWithinOracle([leg(19_190_000)], prices, "sell"), /sell could fill at \$1\.919.*outside 4\.00%/);
  assertLegsWithinOracle([leg(20_800_000)], prices, "buy");
  assert.throws(() => assertLegsWithinOracle([leg(20_810_000)], prices, "buy"), /buy could fill/);
  assert.throws(() => assertLegsWithinOracle([leg(20_000_000)], new Map(), "buy"), /no signed price/);
});
