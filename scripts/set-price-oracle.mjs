// Set the key whose signed prices rebalances accept, creating the program's price oracle account
// on first use (the authority pays its rent). Prices the previous key signed stop verifying at
// once. Dry-run unless --execute. Signs with deployer-keypair.json (protocol authority).
// Env: SOLANA_RPC_URL.
//
//   node scripts/set-price-oracle.mjs                      # oracle-keypair.json's public key
//   node scripts/set-price-oracle.mjs --oracle <pubkey> [--execute]
//
// For the oracle service (fly.oracle.toml) pass its public key (GET /health shows it). The
// oracle key must not be the keeper's (scripts/rebalance-bot.mjs refuses to run that way).
import fs from 'node:fs';
import anchor from '@coral-xyz/anchor';
import { Connection, Keypair, PublicKey, SystemProgram } from '@solana/web3.js';

const args = process.argv.slice(2);
const oracleArg = args.includes('--oracle') ? args[args.indexOf('--oracle') + 1] : null;
const execute = args.includes('--execute');
const MAINNET_GENESIS = '5eykt4UsFv8P8NJdTREpY1vzqKqZKvdpKuc147dw2N9d';
const readKey = file => Keypair.fromSecretKey(Uint8Array.from(JSON.parse(fs.readFileSync(file, 'utf8'))));

const oracle = oracleArg ? new PublicKey(oracleArg) : readKey('oracle-keypair.json').publicKey;
const rpc = process.env.SOLANA_RPC_URL ?? 'https://api.mainnet-beta.solana.com';
const authority = readKey('deployer-keypair.json');
const connection = new Connection(rpc, 'confirmed');
const program = new anchor.Program(JSON.parse(fs.readFileSync('target/idl/basket.json', 'utf8')), new anchor.AnchorProvider(connection, new anchor.Wallet(authority), { commitment: 'confirmed', preflightCommitment: 'confirmed' }));
if (await connection.getGenesisHash() !== MAINNET_GENESIS) throw new Error('Not mainnet');

const pda = seed => PublicKey.findProgramAddressSync([Buffer.from(seed)], program.programId)[0];
const protocolConfig = pda('protocol-config');
const priceOracle = pda('price-oracle');
const config = await program.account.protocolConfig.fetch(protocolConfig);
if (!config.authority.equals(authority.publicKey)) throw new Error(`Protocol authority is ${config.authority.toBase58()}, not ${authority.publicKey.toBase58()}`);
if (fs.existsSync('keeper-keypair.json') && readKey('keeper-keypair.json').publicKey.equals(oracle)) throw new Error('The oracle key must not be the keeper key');

const current = await program.account.priceOracle.fetchNullable(priceOracle);
console.log(`price oracle ${priceOracle.toBase58()}: ${current ? `accepts ${current.oracle.toBase58()}` : 'not created yet'}`);
if (current?.oracle.equals(oracle)) {
  console.log('already set');
  process.exit(0);
}
const builder = program.methods.setPriceOracle({ oracle })
  .accounts({ authority: authority.publicKey, protocolConfig, priceOracle, systemProgram: SystemProgram.programId });
if (!execute) {
  await builder.simulate();
  console.log(`[dry-run] would set the oracle to ${oracle.toBase58()}${current ? '' : ' and create the account'}; simulates cleanly, pass --execute`);
  process.exit(0);
}
const signature = await builder.rpc();
const after = await program.account.priceOracle.fetch(priceOracle);
if (!after.oracle.equals(oracle)) throw new Error('Oracle not set');
console.log(`oracle set to ${oracle.toBase58()} (${signature})`);
