// Propose, inspect or cancel a fixed-weight basket's composition change.
//
//   node scripts/composition-change.mjs --basket MDAO
//       show the composition and any pending change
//   node scripts/composition-change.mjs --basket MDAO --weights META=3000,CRED=2000,... [--add OMFG=500] [--execute]
//       propose new target weights (every non-USDC component; 0 removes it) and new components
//   node scripts/composition-change.mjs --basket MDAO --cancel [--execute]
//
// Without --execute nothing is sent: the transaction is simulated. A proposal applies
// COMPOSITION_CHANGE_DELAY_SECONDS (3 days) later, within a 7-day window, while redemptions
// are open at fees no higher than when proposed: the rebalance bot applies it once the
// basket's intents have settled, then rebalances onto it. New components must be in
// docs/program-replacement/catalog.json `tokens` first. The rebalance that switches the
// basket over prices every component it still holds or targets (removed ones until they are
// sold), so this checks the rebalance oracle (scripts/lib/price-oracle.mjs) can price them all.
// Env: SOLANA_RPC_URL, ANCHOR_WALLET (the basket authority), JUPITER_API_KEY (Jupiter's keyed
// API; keyless without it), JUPITER_SWAP_API, JUPITER_PRICE_API.
import fs from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import anchor from '@coral-xyz/anchor';
import { Connection, Keypair, PublicKey, SystemProgram } from '@solana/web3.js';
import { USDC, fetchJson } from './lib/catalog.mjs';
import { OraclePriceError, jupiterApis, oraclePrices } from './lib/price-oracle.mjs';

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
const read = file => JSON.parse(fs.readFileSync(path.join(root, file), 'utf8'));
const argv = process.argv.slice(2);
const flag = name => argv.includes(name);
const value = name => { const i = argv.indexOf(name); return i >= 0 ? argv[i + 1] : undefined; };
const values = name => argv.flatMap((arg, i) => (arg === name ? [argv[i + 1]] : []));
const execute = flag('--execute');
const basketArg = value('--basket');
if (!basketArg) throw new Error('Pass --basket <symbol or index address>');

const catalog = read('docs/program-replacement/catalog.json');
const deployment = read('docs/program-replacement/deployment.json');
const symbolByMint = new Map(Object.entries(catalog.tokens).map(([symbol, token]) => [token.mint, symbol]));
const { priceApi, swapApi } = jupiterApis();

const connection = new Connection(process.env.SOLANA_RPC_URL ?? 'https://api.mainnet-beta.solana.com', 'confirmed');
if (await connection.getGenesisHash() !== '5eykt4UsFv8P8NJdTREpY1vzqKqZKvdpKuc147dw2N9d') throw new Error('Expected Solana mainnet');
const keyFile = process.env.ANCHOR_WALLET ?? path.join(root, 'deployer-keypair.json');
const authority = Keypair.fromSecretKey(Uint8Array.from(JSON.parse(fs.readFileSync(keyFile, 'utf8'))));
const provider = new anchor.AnchorProvider(connection, new anchor.Wallet(authority), { commitment: 'confirmed', preflightCommitment: 'confirmed' });
const program = new anchor.Program(read('target/idl/basket.json'), provider);
const pda = (...seeds) => PublicKey.findProgramAddressSync(seeds.map(s => typeof s === 'string' ? Buffer.from(s) : s.toBuffer ? s.toBuffer() : s), program.programId)[0];

const index = new PublicKey(deployment.baskets[basketArg]?.index ?? basketArg);
const state = await program.account.indexState.fetch(index);
if (!('fixedWeights' in state.kind)) throw new Error(`${state.symbol} is not a fixed-weight basket`);
const pages = Array.from({ length: state.largeBasketPageCount }, (_, i) => pda('large-basket-component-page', index, Buffer.from([i])));
const components = (await Promise.all(pages.map(p => program.account.largeBasketComponentPage.fetch(p)))).flatMap(p => p.components);
const changePk = pda('composition-change', index);
const pending = await program.account.compositionChange.fetchNullable(changePk);
const label = mint => symbolByMint.get(mint.toBase58()) ?? mint.toBase58();
const pct = bps => `${(bps / 100).toFixed(2)}%`;

console.log(`${state.symbol} (${index.toBase58()})`);
for (const c of components) {
  const retired = !c.mint.equals(new PublicKey(USDC)) && c.targetWeightBps === 0 && c.accountedReserve.isZero();
  console.log(`  ${label(c.mint).padEnd(12)} target ${pct(c.targetWeightBps).padStart(7)}${retired ? '  (removed)' : ''}`);
}
if (pending) {
  console.log(`pending change, applies from ${new Date(Number(pending.effectiveAt) * 1000).toISOString()}:`);
  pending.targetWeightsBps.forEach((w, i) => { if (w !== components[i]?.targetWeightBps) console.log(`  ${label(components[i].mint)}: ${pct(components[i].targetWeightBps)} -> ${pct(w)}`); });
  for (const a of pending.additions) console.log(`  + ${label(a.mint)}: ${pct(a.targetWeightBps)}`);
  if (pending.targetWeightsBps.length !== components.length) console.log('  STALE: a component was registered after this proposal; cancel and propose again');
}

async function send(builder, what) {
  if (!execute) {
    const simulation = await builder.simulate().catch(error => ({ error }));
    if (simulation.error) throw new Error(`${what} would fail: ${simulation.error.message ?? simulation.error}\n${(simulation.error.simulationResponse?.logs ?? simulation.error.logs ?? []).join('\n')}`);
    console.log(`[dry-run] ${what} simulates cleanly; pass --execute to send it`);
    return;
  }
  console.log(`${what}: ${await builder.rpc()}`);
}

if (flag('--cancel')) {
  if (!pending) throw new Error('No pending composition change');
  await send(program.methods.cancelCompositionChange().accounts({ authority: authority.publicKey, index, compositionChange: changePk, proposer: pending.proposer }), 'cancel composition change');
} else if (value('--weights') || values('--add').length) {
  if (pending) throw new Error('A composition change is already pending; cancel it first');
  const weightArgs = new Map((value('--weights') ?? '').split(',').filter(Boolean).map(pair => {
    const [key, bps] = pair.split('=');
    return [key, Number(bps)];
  }));
  const targetWeightsBps = components.map(c => {
    if (c.mint.equals(new PublicKey(USDC))) return c.targetWeightBps; // the cash slot keeps its weight
    const key = [label(c.mint), c.mint.toBase58()].find(k => weightArgs.has(k));
    if (!key) throw new Error(`--weights must give every component a weight (0 removes it); missing ${label(c.mint)}`);
    const bps = weightArgs.get(key);
    weightArgs.delete(key);
    return bps;
  });
  if (weightArgs.size) throw new Error(`Not components of ${state.symbol}: ${[...weightArgs.keys()].join(', ')}`);

  const additions = [];
  const tokens = [];
  for (const arg of values('--add')) {
    const [symbol, bps] = arg.split('=');
    const token = catalog.tokens[symbol];
    if (!token) throw new Error(`${symbol} is not in catalog.json tokens`);
    // The program only adds classic SPL tokens (Token-2022 extensions break vault accounting).
    if (token.tokenProgram !== 'TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA') throw new Error(`${symbol} is not a classic SPL token`);
    // Rebalances price components from the oracle's signed prices; oracle pairs are unused.
    additions.push({ mint: new PublicKey(token.mint), oraclePair: PublicKey.default, targetWeightBps: Number(bps) });
    tokens.push({ mint: token.mint, decimals: token.decimals, label: symbol, bps: Number(bps) });
  }
  // The switchover rebalance prices every component it holds or targets, plus the additions.
  components.forEach((c, i) => {
    if (c.mint.equals(new PublicKey(USDC)) || (targetWeightsBps[i] === 0 && c.accountedReserve.isZero())) return;
    tokens.push({ mint: c.mint.toBase58(), decimals: c.decimals, label: label(c.mint), existing: true });
  });
  try {
    const prices = await oraclePrices(tokens, { swapApi, priceApi, fetchJson: url => fetchJson(url) });
    for (const t of tokens) console.log(`  ${t.existing ? ' ' : '+'} ${t.label}${t.existing ? '' : ` ${pct(t.bps)}`}: oracle prices it at $${prices.get(t.mint).usd.toPrecision(6)}`);
  } catch (error) {
    if (!(error instanceof OraclePriceError)) throw error;
    throw new Error(`The rebalance oracle cannot price ${error.failures.map(f => `${f.label} (${f.reason})`).join(', ')}`);
  }
  const total = [...targetWeightsBps, ...additions.map(a => a.targetWeightBps)].reduce((sum, w) => sum + w, 0);
  if (total !== 10_000) throw new Error(`Weights sum to ${pct(total)}, not 100%`);
  components.forEach((c, i) => { if (targetWeightsBps[i] !== c.targetWeightBps) console.log(`  ${label(c.mint)}: ${pct(c.targetWeightBps)} -> ${pct(targetWeightsBps[i])}`); });

  await send(program.methods.proposeCompositionChange({ targetWeightsBps, additions })
    .accounts({ authority: authority.publicKey, index, compositionChange: changePk, systemProgram: SystemProgram.programId })
    .remainingAccounts([...pages.map(pubkey => ({ pubkey, isWritable: false, isSigner: false })),
      ...additions.map(a => ({ pubkey: a.mint, isWritable: false, isSigner: false }))]), 'propose composition change');
  if (execute) {
    const created = await program.account.compositionChange.fetch(changePk);
    console.log(`Applies from ${new Date(Number(created.effectiveAt) * 1000).toISOString()}.`);
  }
}
