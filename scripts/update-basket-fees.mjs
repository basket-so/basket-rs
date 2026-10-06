// Set mint/redeem fee splits on the mainnet catalog baskets. Dry-run (simulation) unless --execute.
//
//   node scripts/update-basket-fees.mjs --protocol-bps 5 --staking-bps 5 --exclude USDX
//   node scripts/update-basket-fees.mjs --protocol-bps 5 --staking-bps 5 --exclude USDX --execute
//
// The same split applies to mint and redeem. Each basket's fees are independent on-chain;
// rates are snapshotted when an intent opens, so in-flight mints/redeems keep their old fees.
// Env: SOLANA_RPC_URL, ANCHOR_WALLET (index authority; default deployer-keypair.json).
import fs from 'node:fs';
import anchor from '@coral-xyz/anchor';
import { Connection, Keypair, PublicKey } from '@solana/web3.js';

const args = process.argv.slice(2);
const flag = (name, fallback) => (args.includes(name) ? args[args.indexOf(name) + 1] : fallback);
const bps = name => {
  const value = Number(flag(name, '0'));
  if (!Number.isInteger(value) || value < 0) throw new Error(`${name} must be a whole number of basis points`);
  return value;
};
const protocolBps = bps('--protocol-bps');
const creatorBps = bps('--creator-bps');
const stakingBps = bps('--staking-bps');
if (protocolBps + creatorBps + stakingBps > 1000) throw new Error('Total fee per direction is capped at 1000 bps (10%)');
const exclude = new Set((flag('--exclude', '') || '').split(',').filter(Boolean).map(s => s.toUpperCase()));
const execute = args.includes('--execute');

const idl = JSON.parse(fs.readFileSync('docs/program-upgrade/basket.idl.json', 'utf8'));
const authority = Keypair.fromSecretKey(Uint8Array.from(JSON.parse(fs.readFileSync(process.env.ANCHOR_WALLET ?? 'deployer-keypair.json', 'utf8'))));
const connection = new Connection(process.env.SOLANA_RPC_URL ?? 'https://api.mainnet-beta.solana.com', 'confirmed');
const program = new anchor.Program(idl, new anchor.AnchorProvider(connection, new anchor.Wallet(authority), { commitment: 'confirmed' }));
const baskets = JSON.parse(fs.readFileSync('docs/program-replacement/deployment.json', 'utf8')).baskets;

const target = {
  mintFeeBps: protocolBps, redeemFeeBps: protocolBps,
  creatorMintFeeBps: creatorBps, creatorRedeemFeeBps: creatorBps,
  stakingMintFeeBps: stakingBps, stakingRedeemFeeBps: stakingBps,
};
const describe = s => `mint ${s.mintFeeBps}/${s.creatorMintFeeBps}/${s.stakingMintFeeBps} redeem ${s.redeemFeeBps}/${s.creatorRedeemFeeBps}/${s.stakingRedeemFeeBps}`;
console.log(`${execute ? 'EXECUTE' : 'Dry-run'}: protocol/creator/staking = ${protocolBps}/${creatorBps}/${stakingBps} bps each way` + (exclude.size ? `, excluding ${[...exclude].join(', ')}` : ''));

for (const [symbol, { index: address }] of Object.entries(baskets)) {
  if (exclude.has(symbol)) { console.log(`${symbol.padEnd(5)} skipped (excluded)`); continue; }
  const index = new PublicKey(address);
  const state = await program.account.indexState.fetch(index);
  if (!state.authority.equals(authority.publicKey)) throw new Error(`${symbol}: signer is not the index authority (${state.authority.toBase58()})`);
  if (Object.entries(target).every(([k, v]) => state[k] === v)) { console.log(`${symbol.padEnd(5)} already ${describe(state)}`); continue; }
  const builder = program.methods.updateFees(target).accounts({ authority: authority.publicKey, index });
  if (!execute) {
    const sim = await builder.simulate();
    console.log(`${symbol.padEnd(5)} ${describe(state)} -> ${describe(target)} (simulated OK, ${sim.raw.length} log lines)`);
    continue;
  }
  const signature = await builder.rpc();
  const updated = await program.account.indexState.fetch(index);
  if (!Object.entries(target).every(([k, v]) => updated[k] === v)) throw new Error(`${symbol}: verification failed after ${signature}`);
  console.log(`${symbol.padEnd(5)} ${describe(updated)}  ${signature}`);
}
