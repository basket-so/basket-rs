// One-time accounting migration. Dry-run unless --execute is passed.
import fs from 'node:fs';
import anchor from '@coral-xyz/anchor';
import { Connection, Keypair, PublicKey, SystemProgram } from '@solana/web3.js';
import { getAssociatedTokenAddressSync, ASSOCIATED_TOKEN_PROGRAM_ID, TOKEN_PROGRAM_ID } from '@solana/spl-token';
const args = process.argv.slice(2);
const indexArg = args[args.indexOf('--index') + 1];
if (!args.includes('--index') || !indexArg) throw new Error('Usage: node scripts/register-rebalance-quote.mjs --index <address> [--execute]');
const idl = JSON.parse(fs.readFileSync('target/idl/basket.json', 'utf8'));
const payer = Keypair.fromSecretKey(Uint8Array.from(JSON.parse(fs.readFileSync(process.env.ANCHOR_WALLET ?? 'deployer-keypair.json', 'utf8'))));
const connection = new Connection(process.env.SOLANA_RPC_URL ?? 'https://api.mainnet-beta.solana.com', 'confirmed');
const program = new anchor.Program(idl, new anchor.AnchorProvider(connection, new anchor.Wallet(payer), { commitment: 'confirmed' }));
const index = new PublicKey(indexArg);
const state = await program.account.indexState.fetch(index);
const usdc = new PublicKey('EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v');
const pda = (seed, ...parts) => PublicKey.findProgramAddressSync([Buffer.from(seed), ...parts.map(p => p.toBuffer ? p.toBuffer() : p)], program.programId)[0];
const pages = Array.from({ length: state.largeBasketPageCount }, (_, i) => pda('large-basket-component-page', index, Buffer.from([i])));
const records = await Promise.all(pages.map(p => program.account.largeBasketComponentPage.fetch(p)));
if (!state.kind.fixedWeights) throw new Error('Only FixedWeights baskets need this migration');
if (records.some(p => p.components.some(c => c.mint.equals(usdc)))) {
  console.log('USDC is already accounted as a component; no migration needed.');
} else {
  if (state.largeBasketOperationInProgress) throw new Error('Finish or cancel the active operation before migrating');
  if (state.largeBasketComponentCount >= 50) throw new Error('No free component slot; this basket requires a separate capacity migration');
  const vaultAuthority = pda('vault-authority', index);
  const vaultQuote = getAssociatedTokenAddressSync(usdc, vaultAuthority, true);
  const quotePage = pda('large-basket-component-page', index, Buffer.from([Math.floor(state.largeBasketComponentCount / 10)]));
  const builder = program.methods.registerRebalanceQuote().accounts({
    payer: payer.publicKey, index, indexMint: state.indexMint, vaultAuthority,
    quoteMint: usdc, vaultQuote, quotePage, associatedTokenProgram: ASSOCIATED_TOKEN_PROGRAM_ID,
    tokenProgram: TOKEN_PROGRAM_ID, systemProgram: SystemProgram.programId,
  }).remainingAccounts(pages.map(pubkey => ({ pubkey, isWritable: false, isSigner: false })));
  if (args.includes('--execute')) {
    const signature = await builder.rpc();
    const updated = await program.account.largeBasketComponentPage.fetch(quotePage);
    const cash = updated.components.find(c => c.mint.equals(usdc));
    if (!cash || cash.targetWeightBps !== 0) throw new Error('Migration verification failed');
    console.log(`Registered USDC reserve: ${cash.accountedReserve} atoms. Transaction: ${signature}`);
  } else {
    await builder.instruction();
    console.log(`Dry-run: append USDC reserve at component ${state.largeBasketComponentCount} for ${index}. No transactions sent.`);
  }
}
