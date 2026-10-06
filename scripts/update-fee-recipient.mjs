// Point the catalog baskets' protocol fee recipient at a new address. Dry-run (simulation) unless --execute.
//
//   node scripts/update-fee-recipient.mjs --recipient <address>
//   node scripts/update-fee-recipient.mjs --recipient <address> --execute
//
// update_config rewrites every config field at once, so each basket's current creator recipient,
// supply cap, rebalance delay and pause flags are read back and passed through unchanged.
// The recipient only receives fees; it gains no authority over the baskets.
// Env: SOLANA_RPC_URL, ANCHOR_WALLET (index authority; default deployer-keypair.json).
import fs from 'node:fs';
import anchor from '@coral-xyz/anchor';
import { Connection, Keypair, PublicKey } from '@solana/web3.js';
import { getAssociatedTokenAddressSync } from '@solana/spl-token';

const args = process.argv.slice(2);
if (!args.includes('--recipient')) throw new Error('Usage: node scripts/update-fee-recipient.mjs --recipient <address> [--exclude SYM,...] [--execute]');
const recipient = new PublicKey(args[args.indexOf('--recipient') + 1]);
const exclude = new Set((args.includes('--exclude') ? args[args.indexOf('--exclude') + 1] : '').split(',').filter(Boolean).map(s => s.toUpperCase()));
const execute = args.includes('--execute');

const idl = JSON.parse(fs.readFileSync('docs/program-upgrade/basket.idl.json', 'utf8'));
const authority = Keypair.fromSecretKey(Uint8Array.from(JSON.parse(fs.readFileSync(process.env.ANCHOR_WALLET ?? 'deployer-keypair.json', 'utf8'))));
const connection = new Connection(process.env.SOLANA_RPC_URL ?? 'https://api.mainnet-beta.solana.com', 'confirmed');
const program = new anchor.Program(idl, new anchor.AnchorProvider(connection, new anchor.Wallet(authority), { commitment: 'confirmed' }));
const baskets = JSON.parse(fs.readFileSync('docs/program-replacement/deployment.json', 'utf8')).baskets;

// Swap-path fees are paid in USDC, so the recipient must already hold a USDC account.
const usdc = new PublicKey('EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v');
if (!(await connection.getAccountInfo(getAssociatedTokenAddressSync(usdc, recipient, true)))) {
  throw new Error(`${recipient.toBase58()} has no USDC token account; create it before redirecting fees`);
}

const preserved = ['creatorFeeRecipient', 'maxSupply', 'rebalanceDelaySeconds', 'mintingPaused', 'redeemingPaused', 'rebalancingPaused'];
const same = (a, b) => (a?.equals ? a.equals(b) : a?.eq ? a.eq(b) : a === b);
console.log(`${execute ? 'EXECUTE' : 'Dry-run'}: fee recipient -> ${recipient.toBase58()}` + (exclude.size ? `, excluding ${[...exclude].join(', ')}` : ''));

for (const [symbol, { index: address }] of Object.entries(baskets)) {
  if (exclude.has(symbol)) { console.log(`${symbol.padEnd(5)} skipped (excluded)`); continue; }
  const index = new PublicKey(address);
  const state = await program.account.indexState.fetch(index);
  if (!state.authority.equals(authority.publicKey)) throw new Error(`${symbol}: signer is not the index authority (${state.authority.toBase58()})`);
  if (state.feeRecipient.equals(recipient)) { console.log(`${symbol.padEnd(5)} already ${recipient.toBase58()}`); continue; }
  const config = {feeRecipient: recipient};
  for (const key of preserved) config[key] = state[key];
  const pauses = `paused mint/redeem/rebalance ${+state.mintingPaused}/${+state.redeemingPaused}/${+state.rebalancingPaused}`;
  const builder = program.methods.updateConfig(config).accounts({ authority: authority.publicKey, index });
  if (!execute) {
    await builder.simulate();
    console.log(`${symbol.padEnd(5)} ${state.feeRecipient.toBase58().slice(0, 8)}… -> ${recipient.toBase58().slice(0, 8)}… (${pauses}; simulated OK)`);
    continue;
  }
  const signature = await builder.rpc();
  const updated = await program.account.indexState.fetch(index);
  if (!updated.feeRecipient.equals(recipient) || !preserved.every(key => same(updated[key], state[key]))) {
    throw new Error(`${symbol}: verification failed after ${signature}`);
  }
  console.log(`${symbol.padEnd(5)} fee recipient ${recipient.toBase58()} (${pauses} unchanged)  ${signature}`);
}
