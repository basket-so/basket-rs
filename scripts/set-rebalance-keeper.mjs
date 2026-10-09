// Designate the keeper wallet allowed (besides the authority) to request and open rebalances on
// every fixed-weight basket. Dry-run unless --execute. Signs with deployer-keypair.json (index
// authority). Env: SOLANA_RPC_URL.
//
//   node scripts/set-rebalance-keeper.mjs --keeper <pubkey>            # show what would change
//   node scripts/set-rebalance-keeper.mjs --keeper <pubkey> --execute
import fs from 'node:fs';
import anchor from '@coral-xyz/anchor';
import { Connection, Keypair, PublicKey } from '@solana/web3.js';

const args = process.argv.slice(2);
const keeperArg = args.includes('--keeper') ? args[args.indexOf('--keeper') + 1] : null;
if (!keeperArg) throw new Error('Usage: node scripts/set-rebalance-keeper.mjs --keeper <pubkey> [--execute]');
const keeper = new PublicKey(keeperArg);
const execute = args.includes('--execute');
const MAINNET_GENESIS = '5eykt4UsFv8P8NJdTREpY1vzqKqZKvdpKuc147dw2N9d';

const rpc = process.env.SOLANA_RPC_URL ?? 'https://api.mainnet-beta.solana.com';
const authority = Keypair.fromSecretKey(Uint8Array.from(JSON.parse(fs.readFileSync('deployer-keypair.json', 'utf8'))));
const connection = new Connection(rpc, 'confirmed');
const program = new anchor.Program(JSON.parse(fs.readFileSync('target/idl/basket.json', 'utf8')), new anchor.AnchorProvider(connection, new anchor.Wallet(authority), { commitment: 'confirmed', preflightCommitment: 'confirmed' }));

if (await connection.getGenesisHash() !== MAINNET_GENESIS) throw new Error('Not mainnet');
const baskets = (await program.account.indexState.all()).filter(({ account: s }) => 'fixedWeights' in s.kind);
for (const { publicKey, account: s } of baskets) {
  const current = s.rebalanceKeeper.toBase58();
  if (s.rebalanceKeeper.equals(keeper)) { console.log(`${s.symbol}: keeper already ${current}`); continue; }
  if (!s.authority.equals(authority.publicKey)) { console.log(`${s.symbol}: skipped, authority is ${s.authority.toBase58()}`); continue; }
  if (!execute) { console.log(`${s.symbol}: would set keeper ${current} -> ${keeper.toBase58()}`); continue; }
  const signature = await program.methods.setRebalanceKeeper({ keeper }).accounts({ authority: authority.publicKey, index: publicKey }).rpc();
  const after = await program.account.indexState.fetch(publicKey);
  if (!after.rebalanceKeeper.equals(keeper)) throw new Error(`${s.symbol}: keeper not set`);
  console.log(`${s.symbol}: keeper set to ${keeper.toBase58()} (${signature})`);
}
