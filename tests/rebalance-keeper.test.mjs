import test from "node:test";
import assert from "node:assert/strict";
import fs from "node:fs";
import anchor from "@coral-xyz/anchor";
import { Keypair, PublicKey, SystemProgram, TransactionInstruction } from "@solana/web3.js";
import { compiledSize, PROGRAM_ID, sendV0, executeBatchIx, affordableBuys, buyAmount } from "../scripts/rebalance-bot.mjs";

test("keeper address matches program source and batch builds with current IDL", async () => {
  const source = fs.readFileSync("programs/basket/src/lib.rs", "utf8");
  assert.ok(source.includes(`declare_id!("${PROGRAM_ID}")`));
  const idl = JSON.parse(fs.readFileSync("target/idl/basket.json"));
  assert.equal(idl.address, PROGRAM_ID.toBase58());
  const pk = PublicKey.default;
  const program = new anchor.Program(idl, { connection: {}, publicKey: pk });
  const ctx = { keeper: pk, index: pk, intent: pk, vaultAuthority: pk, vaultQuote: pk };
  const oracle = { queue: { pubkey: Keypair.generate().publicKey }, quoteAccount: Keypair.generate().publicKey };
  for (const method of ["executeRebalanceSellBatch", "executeRebalanceBuyBatch"]) {
    const ix = await executeBatchIx(program, ctx, method, [], oracle);
    assert.ok(ix.keys.some((k) => k.pubkey.equals(oracle.quoteAccount)));
    const decoded = program.coder.instruction.decode(ix.data);
    assert.equal(decoded.data.args.switchboardMaxAgeSlots.toNumber(), 150);
    assert.equal(decoded.data.args.maxOracleSlippageBps, 400);
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


const { createGatewayResolver } = await import("../scripts/lib/switchboard-gateway.mjs");
for (const failure of ["empty", "unavailable", "unhealthy"]) {
  test(`gateway discovery falls back when Crossbar is ${failure}`, async () => {
    let reads = 0;
    const resolve = createGatewayResolver({
      discover: async () => { if (failure === "unavailable") throw new Error("503"); return failure === "empty" ? [] : ["https://dead.example"]; },
      registered: async () => { reads++; return ["https://dead.example", "https://working.example"]; },
      probe: async url => { if (url.includes("dead")) throw new Error("503"); },
      create: url => ({ url }),
    });
    assert.equal((await resolve()).url, "https://working.example");
    assert.equal((await resolve()).url, "https://working.example");
    assert.equal(reads, 1);
  });
}
test("gateway fallback fails closed if every registered gateway is unreachable", async () => {
  const resolve = createGatewayResolver({ discover: async () => [], registered: async () => ["https://dead.example"], probe: async () => { throw new Error("timeout"); } });
  await assert.rejects(resolve(), /No reachable Switchboard gateways/);
});
