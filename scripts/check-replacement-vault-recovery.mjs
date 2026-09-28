import fs from 'node:fs';
import anchor from '@coral-xyz/anchor';
import { Connection, Keypair, PublicKey, TransactionMessage, VersionedTransaction } from '@solana/web3.js';
import { getMint, TOKEN_PROGRAM_ID } from '@solana/spl-token';
const config = JSON.parse(fs.readFileSync('../basket-ui/public/mainnet-state.json', 'utf8'));
const connection = new Connection(config.rpcUrl, 'confirmed');
const idl = JSON.parse(fs.readFileSync('docs/program-replacement/previous-idl.json', 'utf8'));
const payer = Keypair.fromSecretKey(Uint8Array.from(JSON.parse(fs.readFileSync('deployer-keypair.json', 'utf8'))));
const program = new anchor.Program(idl, new anchor.AnchorProvider(connection, new anchor.Wallet(payer), {}));
const index = new PublicKey('EGumJABbMjKDZRLDeEq17QvZbc8yzABnKpyXVzdXJfSh');
const ix = await program.methods.claimFees().accountsStrict({ authority: payer.publicKey, index, indexMint: new PublicKey('CAojhMsaxzHFKzvkjde3kP4ERezfBnqxTGCoYHUYWr6U'), vaultAuthority: new PublicKey('2tDtM1e16ogxrVxV7xorz2WgpRtL2ByXwwjZb4dd1X8Y'), tokenProgram: TOKEN_PROGRAM_ID }).instruction();
const tx = new VersionedTransaction(new TransactionMessage({ payerKey: payer.publicKey, recentBlockhash: (await connection.getLatestBlockhash()).blockhash, instructions: [ix] }).compileToV0Message());
// Read-only simulation. Nothing is signed or broadcast.
const simulated = await connection.simulateTransaction(tx, { sigVerify: false });
console.log(JSON.stringify({ recoverySimulation: simulated.value.err, logs: simulated.value.logs }, null, 2));
const report = JSON.parse(fs.readFileSync('docs/program-replacement/preflight.json', 'utf8'));
for (const vault of report.tokenAccounts.filter(x => x.amount !== '0')) {
  const mintAddress = new PublicKey(vault.mint);
  const mintInfo = await connection.getAccountInfo(mintAddress);
  const mint = await getMint(connection, mintAddress, 'confirmed', mintInfo.owner);
  console.log(JSON.stringify({ mint: vault.mint, decimals: mint.decimals, tokens: Number(vault.amount) / 10 ** mint.decimals }));
}
