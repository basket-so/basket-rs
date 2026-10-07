// Propose, inspect or cancel a fixed-weight basket's composition change.
//
//   node scripts/composition-change.mjs --basket MDAO
//       show the composition and any pending change
//   node scripts/composition-change.mjs --basket MDAO --weights META=3000,CRED=2000,... [--add OMFG=500] [--execute]
//       propose new target weights (every non-USDC component; 0 removes it) and new components
//   node scripts/composition-change.mjs --basket MDAO --cancel [--execute]
//
// Without --execute nothing is stored or sent: feeds are simulated and the transaction is
// simulated. A proposal applies COMPOSITION_CHANGE_DELAY_SECONDS (3 days) later, within a
// 7-day window, while redemptions are open at fees no higher than when proposed: the
// rebalance bot applies it once the basket's intents have settled, then rebalances onto it.
// New components must be in docs/program-replacement/catalog.json `tokens` first. After
// proposing, re-run scripts/vendor-switchboard-feeds.mjs so the app and bot carry their feeds.
// Env: SOLANA_RPC_URL, ANCHOR_WALLET (the basket authority), SWITCHBOARD_CROSSBAR_URL.
import fs from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import anchor from '@coral-xyz/anchor';
import { Connection, Keypair, PublicKey, SystemProgram } from '@solana/web3.js';
import { CrossbarClient, CrossbarNetwork, FeedHash } from '@switchboard-xyz/common';
import { USDC, catalogPriceFeed, fetchJson } from './lib/catalog.mjs';

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
const priceApi = process.env.JUPITER_PRICE_API ?? 'https://lite-api.jup.ag/price/v3';

const connection = new Connection(process.env.SOLANA_RPC_URL ?? read('../basket-ui/public/mainnet-state.json').rpcUrl, 'confirmed');
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

const crossbar = new CrossbarClient(process.env.SWITCHBOARD_CROSSBAR_URL ?? 'https://crossbar.switchboardlabs.xyz');
crossbar.setNetwork(CrossbarNetwork.SolanaMainnet);

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
  const feeds = [];
  for (const arg of values('--add')) {
    const [symbol, bps] = arg.split('=');
    const token = catalog.tokens[symbol];
    if (!token) throw new Error(`${symbol} is not in catalog.json tokens`);
    // The program only adds classic SPL tokens (Token-2022 extensions break vault accounting).
    if (token.tokenProgram !== 'TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA') throw new Error(`${symbol} is not a classic SPL token`);
    const quote = (await fetchJson(`${priceApi}?ids=${token.mint}`))[token.mint];
    if (!(quote?.usdPrice > 0)) throw new Error(`No Jupiter price for ${symbol}`);
    const feed = await catalogPriceFeed(symbol, token.mint, quote.usdPrice, priceApi);
    const simulation = await crossbar.simulateFeed(feed, true, {}, 'mainnet');
    const prices = simulation.results?.map(Number).filter(p => Number.isFinite(p) && p > 0) ?? [];
    if (simulation.error || !prices.length) throw new Error(`${symbol} feed simulation failed: ${simulation.error ?? 'no results'}`);
    const median = prices.sort((a, b) => a - b)[Math.floor(prices.length / 2)];
    if (Math.abs(median / quote.usdPrice - 1) > 0.01) throw new Error(`${symbol} feed ($${median}) and Jupiter ($${quote.usdPrice}) disagree by more than 1%`);
    const oraclePair = new PublicKey(FeedHash.computeOracleFeedId(feed));
    console.log(`  + ${symbol} ${pct(Number(bps))}: feed ${oraclePair.toBase58()} simulates $${median}`);
    additions.push({ mint: new PublicKey(token.mint), oraclePair, targetWeightBps: Number(bps) });
    feeds.push({ symbol, feed, oraclePair });
  }
  const total = [...targetWeightsBps, ...additions.map(a => a.targetWeightBps)].reduce((sum, w) => sum + w, 0);
  if (total !== 10_000) throw new Error(`Weights sum to ${pct(total)}, not 100%`);
  components.forEach((c, i) => { if (targetWeightsBps[i] !== c.targetWeightBps) console.log(`  ${label(c.mint)}: ${pct(c.targetWeightBps)} -> ${pct(targetWeightsBps[i])}`); });

  if (execute) {
    for (const { symbol, feed, oraclePair } of feeds) {
      const stored = await crossbar.storeOracleFeed(feed);
      if (!new PublicKey(Buffer.from(stored.feedId.replace(/^0x/, ''), 'hex')).equals(oraclePair)) throw new Error(`Crossbar stored ${symbol}'s feed under a different id`);
    }
  }
  await send(program.methods.proposeCompositionChange({ targetWeightsBps, additions })
    .accounts({ authority: authority.publicKey, index, compositionChange: changePk, systemProgram: SystemProgram.programId })
    .remainingAccounts([...pages.map(pubkey => ({ pubkey, isWritable: false, isSigner: false })),
      ...additions.map(a => ({ pubkey: a.mint, isWritable: false, isSigner: false }))]), 'propose composition change');
  if (execute) {
    const created = await program.account.compositionChange.fetch(changePk);
    console.log(`Applies from ${new Date(Number(created.effectiveAt) * 1000).toISOString()}. Re-run vendor-switchboard-feeds.mjs and redeploy the app and bot${additions.length ? ' so they carry the new feeds' : ''}.`);
  }
}
