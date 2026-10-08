// Pause or unpause one catalog basket's minting/redeeming. Dry-run (simulation) unless --execute.
//
//   node scripts/set-basket-pauses.mjs --basket MDAO --minting paused --redeeming paused
//   node scripts/set-basket-pauses.mjs --basket MDAO --minting paused --redeeming paused --execute
//
// update_config rewrites every config field at once, so the basket's fee recipients, supply cap,
// rebalance delay and any pause flag not named here are read back and passed through unchanged.
// Toggling redemptions restarts a pending composition change's notice, so its PDA is passed.
// Env: SOLANA_RPC_URL, ANCHOR_WALLET (index authority; default deployer-keypair.json).
import fs from 'node:fs';
import anchor from '@coral-xyz/anchor';
import { Connection, Keypair, PublicKey } from '@solana/web3.js';

const args = process.argv.slice(2);
const value = name => (args.includes(name) ? args[args.indexOf(name) + 1] : undefined);
const symbol = value('--basket');
if (!symbol) throw new Error('Usage: node scripts/set-basket-pauses.mjs --basket SYM [--minting paused|open] [--redeeming paused|open] [--execute]');
const flag = name => {
  const v = value(name);
  if (v === undefined) return undefined;
  if (v !== 'paused' && v !== 'open') throw new Error(`${name} must be paused or open`);
  return v === 'paused';
};
const changes = { mintingPaused: flag('--minting'), redeemingPaused: flag('--redeeming') };
const execute = args.includes('--execute');

const idl = JSON.parse(fs.readFileSync('docs/program-upgrade/basket.idl.json', 'utf8'));
const authority = Keypair.fromSecretKey(Uint8Array.from(JSON.parse(fs.readFileSync(process.env.ANCHOR_WALLET ?? 'deployer-keypair.json', 'utf8'))));
const connection = new Connection(process.env.SOLANA_RPC_URL ?? JSON.parse(fs.readFileSync('../basket-ui/public/mainnet-state.json', 'utf8')).rpcUrl, 'confirmed');
const program = new anchor.Program(idl, new anchor.AnchorProvider(connection, new anchor.Wallet(authority), { commitment: 'confirmed' }));
const basket = JSON.parse(fs.readFileSync('docs/program-replacement/deployment.json', 'utf8')).baskets[symbol];
if (!basket) throw new Error(`${symbol} is not in docs/program-replacement/deployment.json`);

const index = new PublicKey(basket.index);
const state = await program.account.indexState.fetch(index);
if (!state.authority.equals(authority.publicKey)) throw new Error(`Signer is not ${symbol}'s authority (${state.authority.toBase58()})`);
const preserved = ['feeRecipient', 'creatorFeeRecipient', 'maxSupply', 'rebalanceDelaySeconds', 'mintingPaused', 'redeemingPaused', 'rebalancingPaused'];
const config = Object.fromEntries(preserved.map(key => [key, state[key]]));
for (const [key, v] of Object.entries(changes)) if (v !== undefined) config[key] = v;
const describe = s => `minting ${s.mintingPaused ? 'paused' : 'open'}, redeeming ${s.redeemingPaused ? 'paused' : 'open'}`;
if (preserved.every(key => config[key] === state[key] || config[key]?.equals?.(state[key]) || config[key]?.eq?.(state[key]))) {
  console.log(`${symbol}: already ${describe(state)}`);
  process.exit(0);
}
const compositionChange = PublicKey.findProgramAddressSync([Buffer.from('composition-change'), index.toBuffer()], program.programId)[0];
const builder = program.methods.updateConfig(config).accounts({ authority: authority.publicKey, index })
  .remainingAccounts([{ pubkey: compositionChange, isWritable: true, isSigner: false }]);
if (!execute) {
  await builder.simulate();
  console.log(`[dry-run] ${symbol}: ${describe(state)} -> ${describe(config)} (simulated OK); pass --execute`);
  process.exit(0);
}
const signature = await builder.rpc();
const after = await program.account.indexState.fetch(index);
if (after.mintingPaused !== config.mintingPaused || after.redeemingPaused !== config.redeemingPaused) throw new Error(`${symbol}: verification failed after ${signature}`);
console.log(`${symbol}: ${describe(after)} (${signature})`);
