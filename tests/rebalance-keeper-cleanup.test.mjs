import test from "node:test";
import assert from "node:assert/strict";
import fs from "node:fs";
import anchor from "@coral-xyz/anchor";
import { Keypair, PublicKey, SystemProgram } from "@solana/web3.js";
import { MintLayout, TOKEN_PROGRAM_ID, getAssociatedTokenAddressSync } from "@solana/spl-token";
import { PROGRAM_ID, settleExpiredIntents } from "../scripts/rebalance-bot.mjs";

const idl = JSON.parse(fs.readFileSync("target/idl/basket.json", "utf8"));

function mintAccount() {
  const data = Buffer.alloc(MintLayout.span);
  MintLayout.encode({ mintAuthorityOption: 0, mintAuthority: PublicKey.default, supply: 0n, decimals: 6, isInitialized: true, freezeAuthorityOption: 0, freezeAuthority: PublicKey.default }, data);
  return { data, owner: TOKEN_PROGRAM_ID, lamports: 1, executable: false };
}

// Settles a basket's expired intents the way the keeper does before a rebalance. `refuse`
// decides whether a sent transaction fails (as when the owner sabotaged their own account).
// Returns every landed transaction's program instructions, decoded, plus its other programs.
async function settle(intent, { refuse = () => false } = {}) {
  const payer = Keypair.generate();
  const index = Keypair.generate().publicKey;
  const indexMint = Keypair.generate().publicKey;
  const page = Keypair.generate().publicKey;
  const components = [0, 1, 2, 3, 4].map((globalIndex) => ({ globalIndex, mint: Keypair.generate().publicKey, vault: Keypair.generate().publicKey, tokenProgram: TOKEN_PROGRAM_ID }));
  const landed = [];
  const connection = {
    getAccountInfo: async () => mintAccount(),
    getLatestBlockhash: async () => ({ blockhash: PublicKey.default.toBase58(), lastValidBlockHeight: 1 }),
    sendTransaction: async (tx) => {
      if (refuse(tx)) throw new Error("simulation failed: owner account refused");
      landed.push(tx);
      return "sig";
    },
    confirmTransaction: async () => ({ value: { err: null } }),
    getMinimumBalanceForRentExemption: async () => 1,
  };
  const program = new anchor.Program(idl, { connection, publicKey: payer.publicKey });
  program.account.largeBasketIntent.all = async (filters) => (filters[1].memcmp.bytes === "1" ? [{ publicKey: Keypair.generate().publicKey, account: intent }] : []);
  await settleExpiredIntents({ connection, program, payer }, index, { indexMint }, components, [page], 1_000);
  const txs = landed.map((tx) => {
    const keys = tx.message.staticAccountKeys;
    const programs = tx.message.compiledInstructions.map((ix) => keys[ix.programIdIndex]);
    const ixs = tx.message.compiledInstructions
      .filter((ix) => keys[ix.programIdIndex].equals(PROGRAM_ID))
      .map((ix) => ({ name: program.coder.instruction.decode(Buffer.from(ix.data)).name, accounts: ix.accountKeyIndexes.map((i) => keys[i]) }));
    return { programs, ixs };
  });
  const escrow = PublicKey.findProgramAddressSync([Buffer.from("refund-escrow"), index.toBuffer()], PROGRAM_ID)[0];
  const escrowAccount = (c) => getAssociatedTokenAddressSync(c.mint, escrow, true, TOKEN_PROGRAM_ID);
  return { txs, components, page, escrowAccount };
}

const base = (overrides) => ({
  owner: Keypair.generate().publicKey,
  kind: { mint: {} },
  expiresAt: new anchor.BN(500),
  componentCount: 5,
  completedComponents: 0,
  componentFillBitmap: [0, 0, 0, 0, 0, 0, 0],
  refundedBitmap: [0, 0, 0, 0, 0, 0, 0],
  componentAmounts: [new anchor.BN(5), new anchor.BN(0), new anchor.BN(7), new anchor.BN(2), new anchor.BN(9)],
  ...overrides,
});

test("expired partly-filled mint moves only the filled, nonzero components into the escrow", async () => {
  // Components 0 and 1 filled; 1 owes nothing, 2-4 were never filled.
  const { txs, components, page, escrowAccount } = await settle(base({ completedComponents: 2, componentFillBitmap: [0b00011, 0, 0, 0, 0, 0, 0] }));
  assert.equal(txs.length, 1);
  const [cancel] = txs[0].ixs;
  assert.equal(cancel.name, "cancelExpiredLargeBasketIntent");
  const remaining = cancel.accounts.slice(7);
  assert.equal(remaining.length, 5);
  assert.ok(remaining[0].equals(page) && remaining[1].equals(components[0].mint) && remaining[2].equals(components[0].vault) && remaining[4].equals(TOKEN_PROGRAM_ID));
  assert.ok(remaining[3].equals(escrowAccount(components[0])));
});

test("escrow refunds are packed several to a transaction and skip what is back already", async () => {
  // Redeem: components 0, 2, 3, 4 unfilled and owed; 0 already returned.
  const { txs, components, escrowAccount } = await settle(base({ kind: { redeem: {} }, completedComponents: 1, componentFillBitmap: [0b00010, 0, 0, 0, 0, 0, 0], componentAmounts: [5, 3, 7, 2, 9].map((n) => new anchor.BN(n)), refundedBitmap: [0b00001, 0, 0, 0, 0, 0, 0] }));
  assert.equal(txs.length, 1);
  const groups = txs[0].ixs[0].accounts.slice(8);
  assert.equal(groups.length, 12);
  for (const [k, c] of [components[2], components[3], components[4]].entries()) {
    assert.ok(groups[k * 4].equals(c.mint) && groups[k * 4 + 2].equals(escrowAccount(c)));
  }
});

test("expired unfilled mint sends no remaining accounts", async () => {
  const { txs } = await settle(base({}));
  assert.equal(txs[0].ixs[0].accounts.length, 7);
});

test("expired zero-fill redeem restores reservation through the pages only", async () => {
  const { txs, page } = await settle(base({ kind: { redeem: {} } }));
  assert.deepEqual(txs[0].ixs[0].accounts.slice(7).map(String), [page.toBase58()]);
});

test("expired fully-filled redeem is finalized, not cancelled", async () => {
  const { txs } = await settle(base({ kind: { redeem: {} }, completedComponents: 5, componentFillBitmap: [0b11111, 0, 0, 0, 0, 0, 0] }));
  assert.equal(txs[0].ixs[0].name, "finalizeLargeBasketRedeemIntent");
});

test("a refused owner index account is replaced by one created in the same transaction", async () => {
  const createdInTx = (tx) => tx.message.compiledInstructions.some((ix) => tx.message.staticAccountKeys[ix.programIdIndex].equals(SystemProgram.programId));
  const { txs } = await settle(base({ kind: { redeem: {} } }), { refuse: (tx) => !createdInTx(tx) });
  assert.equal(txs.length, 1);
  assert.ok(txs[0].programs.some((p) => p.equals(SystemProgram.programId)));
  assert.equal(txs[0].ixs[0].name, "cancelExpiredLargeBasketIntent");
});
