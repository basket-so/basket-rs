// Close every settled mint/redeem intent and return its rent to its owner
// (close_large_basket_intent). Settled means finalized, or cancelled with nothing left in the
// refund escrow, and no longer its owner's active intent. Anyone may close one: the rent always
// goes to the intent's owner, and the wallet running this pays only the transaction fees.
// Dry-run unless --execute. Needs the program build with close_large_basket_intent deployed.
// Env: SOLANA_RPC_URL (must allow getProgramAccounts), ANCHOR_WALLET (fee payer, default
// deployer-keypair.json).
//
//   node scripts/close-settled-intents.mjs [--execute]
import fs from 'node:fs';
import anchor from '@coral-xyz/anchor';
import { Connection, Keypair, PublicKey, Transaction, sendAndConfirmTransaction } from '@solana/web3.js';

const execute = process.argv.includes('--execute');
// Closes per transaction: each names three accounts, so this stays far inside the size limits.
const PER_TX = 8;
const readKey = file => Keypair.fromSecretKey(Uint8Array.from(JSON.parse(fs.readFileSync(file, 'utf8'))));

const rpc = process.env.SOLANA_RPC_URL ?? 'https://api.mainnet-beta.solana.com';
const payer = readKey(process.env.ANCHOR_WALLET ?? 'deployer-keypair.json');
const connection = new Connection(rpc, 'confirmed');
const program = new anchor.Program(JSON.parse(fs.readFileSync('target/idl/basket.json', 'utf8')), new anchor.AnchorProvider(connection, new anchor.Wallet(payer), { commitment: 'confirmed', preflightCommitment: 'confirmed' }));
const lockOf = (index, owner) => PublicKey.findProgramAddressSync([Buffer.from('large-basket-intent-lock'), index.toBuffer(), owner.toBuffer()], program.programId)[0];

// The same rule the program enforces (require_closable), so nothing sent here is refused.
const intents = await program.account.largeBasketIntent.all();
const settled = intents.filter(({ account }) => (account.status.finalized || account.status.cancelled) && account.escrowedBitmap.every(byte => byte === 0));
const locks = await program.account.largeBasketIntentLock.fetchMultiple(settled.map(({ account }) => lockOf(account.index, account.owner)));
const closable = settled.filter(({ publicKey }, i) => locks[i] && !locks[i].activeIntent.equals(publicKey));
const rent = (await connection.getMultipleAccountsInfo(closable.map(x => x.publicKey))).reduce((sum, info) => sum + (info?.lamports ?? 0), 0);
const owners = new Set(closable.map(({ account }) => account.owner.toBase58()));
console.log(`${intents.length} intent(s) on chain; ${closable.length} settled and closable, returning ${(rent / 1e9).toFixed(6)} SOL of rent to ${owners.size} owner(s)`);
if (!closable.length) process.exit(0);

const closeIx = ({ publicKey, account }) => program.methods.closeLargeBasketIntent()
  .accounts({ owner: account.owner, intent: publicKey, intentLock: lockOf(account.index, account.owner) })
  .instruction();
const batches = [];
for (let i = 0; i < closable.length; i += PER_TX) batches.push(closable.slice(i, i + PER_TX));
const txOf = async batch => new Transaction().add(...await Promise.all(batch.map(closeIx)));

if (!execute) {
  // A deployed program without the instruction refuses this simulation.
  const tx = await txOf(batches[0]);
  tx.feePayer = payer.publicKey;
  const simulation = await connection.simulateTransaction(tx, [payer]);
  if ((simulation.value.logs ?? []).some(line => line.includes('InstructionFallbackNotFound'))) {
    console.log('[dry-run] the deployed program has no close_large_basket_intent yet; run this after the upgrade');
    process.exit(0);
  }
  if (simulation.value.err) throw new Error(`simulating the first batch failed: ${JSON.stringify(simulation.value.err)} ${(simulation.value.logs ?? []).slice(-3).join(' | ')}`);
  console.log(`[dry-run] ${batches.length} transaction(s) of up to ${PER_TX} closes, fees paid by ${payer.publicKey.toBase58()}; the first simulates cleanly. Pass --execute to send them.`);
  process.exit(0);
}
let closed = 0;
for (const batch of batches) {
  const signature = await sendAndConfirmTransaction(connection, await txOf(batch), [payer], { commitment: 'confirmed' });
  closed += batch.length;
  console.log(`closed ${closed}/${closable.length}: ${signature}`);
}
console.log(`done: returned ${(rent / 1e9).toFixed(6)} SOL of rent to ${owners.size} owner(s)`);
