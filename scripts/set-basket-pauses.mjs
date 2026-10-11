// Pause or unpause one catalog basket's minting/rebalancing. Dry-run (simulation) unless --execute.
// Redeems can't be paused.
//
//   node scripts/set-basket-pauses.mjs --basket MDAO --minting paused
//   node scripts/set-basket-pauses.mjs --basket MDAO --minting paused --rebalancing paused --execute
//
// update_config rewrites every config field at once, so the basket's fee recipients, supply cap,
// rebalance delay and any pause flag not named here are read back and passed through unchanged.
// Env: SOLANA_RPC_URL, ANCHOR_WALLET (index authority; default deployer-keypair.json).
import fs from 'node:fs';
import anchor from '@coral-xyz/anchor';
import { Connection, Keypair, PublicKey } from '@solana/web3.js';

const args = process.argv.slice(2);
const value = name => (args.includes(name) ? args[args.indexOf(name) + 1] : undefined);
const symbol = value('--basket');
if (!symbol) throw new Error('Usage: node scripts/set-basket-pauses.mjs --basket SYM [--minting paused|open] [--rebalancing paused|open] [--execute]');
const flag = name => {
  const v = value(name);
  if (v === undefined) return undefined;
  if (v !== 'paused' && v !== 'open') throw new Error(`${name} must be paused or open`);
  return v === 'paused';
};
if (args.includes('--redeeming')) throw new Error('Redeems can no longer be paused');
const changes = { mintingPaused: flag('--minting'), rebalancingPaused: flag('--rebalancing') };
const execute = args.includes('--execute');

const idl = JSON.parse(fs.readFileSync('docs/program-upgrade/basket.idl.json', 'utf8'));
const authority = Keypair.fromSecretKey(Uint8Array.from(JSON.parse(fs.readFileSync(process.env.ANCHOR_WALLET ?? 'deployer-keypair.json', 'utf8'))));
const connection = new Connection(process.env.SOLANA_RPC_URL ?? 'https://api.mainnet-beta.solana.com', 'confirmed');
const program = new anchor.Program(idl, new anchor.AnchorProvider(connection, new anchor.Wallet(authority), { commitment: 'confirmed' }));
const basket = JSON.parse(fs.readFileSync('docs/program-replacement/deployment.json', 'utf8')).baskets[symbol];
if (!basket) throw new Error(`${symbol} is not in docs/program-replacement/deployment.json`);

const index = new PublicKey(basket.index);
const state = await program.account.indexState.fetch(index);
if (!state.authority.equals(authority.publicKey)) throw new Error(`Signer is not ${symbol}'s authority (${state.authority.toBase58()})`);
const preserved = ['feeRecipient', 'creatorFeeRecipient', 'maxSupply', 'rebalanceDelaySeconds', 'mintingPaused', 'rebalancingPaused'];
const config = Object.fromEntries(preserved.map(key => [key, state[key]]));
for (const [key, v] of Object.entries(changes)) if (v !== undefined) config[key] = v;
const describe = s => `minting ${s.mintingPaused ? 'paused' : 'open'}, rebalancing ${s.rebalancingPaused ? 'paused' : 'open'}`;
if (preserved.every(key => config[key] === state[key] || config[key]?.equals?.(state[key]) || config[key]?.eq?.(state[key]))) {
  console.log(`${symbol}: already ${describe(state)}`);
  process.exit(0);
}
const builder = program.methods.updateBasketConfig(config).accounts({ authority: authority.publicKey, index });
if (!execute) {
  await builder.simulate();
  console.log(`[dry-run] ${symbol}: ${describe(state)} -> ${describe(config)} (simulated OK); pass --execute`);
  process.exit(0);
}
const signature = await builder.rpc();
const after = await program.account.indexState.fetch(index);
if (after.mintingPaused !== config.mintingPaused || after.rebalancingPaused !== config.rebalancingPaused) throw new Error(`${symbol}: verification failed after ${signature}`);
console.log(`${symbol}: ${describe(after)} (${signature})`);
