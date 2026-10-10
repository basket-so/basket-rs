import fs from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import anchor from '@coral-xyz/anchor';
import { AddressLookupTableProgram, Connection, ComputeBudgetProgram, Keypair, PublicKey,
  SystemProgram, SYSVAR_RENT_PUBKEY, TransactionMessage, VersionedTransaction } from '@solana/web3.js';
import { ASSOCIATED_TOKEN_PROGRAM_ID, ExtensionType, TOKEN_2022_PROGRAM_ID, TOKEN_PROGRAM_ID, getAssociatedTokenAddressSync,
  getExtensionTypes, getTransferHook, unpackMint, getAccountLenForMint } from '@solana/spl-token';
import { USDC, validateCatalog, sizeBasket, fetchJson } from './lib/catalog.mjs';
import { jupiterApis, oraclePrices } from './lib/price-oracle.mjs';

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
const read = file => JSON.parse(fs.readFileSync(path.join(root, file), 'utf8'));
const catalog = read('docs/program-replacement/catalog.json');
validateCatalog(catalog);
const argv = process.argv.slice(2);
const execute = argv.includes('--execute');
const symbolsArg = argv.find(a => a.startsWith('--symbols='));
const symbols = symbolsArg?.slice(10).split(',');
if (argv.some(a => a !== '--execute' && a !== '--dry-run' && a !== symbolsArg)) throw new Error('Unknown argument');
if (execute && argv.includes('--dry-run')) throw new Error('Conflicting modes');
if (symbols?.some(s => !catalog.baskets.some(b => b.symbol === s))) throw new Error('Unknown basket symbol');
const selected = catalog.baskets.filter(b => !symbols || symbols.includes(b.symbol));
if (symbols && selected.some(b => b.blockers.length)) throw new Error('Requested basket has unresolved catalog blockers');
const reportFile = path.join(root, 'docs/program-replacement/readiness.json');
const journalFile = path.join(root, 'docs/program-replacement/deployment.json');
const journal = fs.existsSync(journalFile) ? JSON.parse(fs.readFileSync(journalFile, 'utf8')) : { transactions: [], baskets: {} };
const saveJournal = () => fs.writeFileSync(journalFile, JSON.stringify(journal, null, 2) + '\n');
const connection = new Connection(process.env.SOLANA_RPC_URL ?? 'https://api.mainnet-beta.solana.com', 'confirmed');
const keyFile = process.env.ANCHOR_WALLET ?? path.join(root, 'deployer-keypair.json');
const payer = Keypair.fromSecretKey(Uint8Array.from(JSON.parse(fs.readFileSync(keyFile, 'utf8'))));
const provider = new anchor.AnchorProvider(connection, new anchor.Wallet(payer), { commitment: 'confirmed', preflightCommitment: 'confirmed' });
// Fresh deployment uses paged component storage for all basket sizes.
const program = new anchor.Program(read('target/idl/basket.json'), provider);
const programId = new PublicKey(catalog.programId);
if (!program.programId.equals(programId)) throw new Error('Catalog/IDL program mismatch');
if (await connection.getGenesisHash() !== '5eykt4UsFv8P8NJdTREpY1vzqKqZKvdpKuc147dw2N9d') throw new Error('Expected Solana mainnet');
if (execute && !(await connection.getAccountInfo(programId))?.executable) throw new Error('Program is not deployed');
const pda = (...seeds) => PublicKey.findProgramAddressSync(seeds.map(s => typeof s === 'string' ? Buffer.from(s) : s.toBuffer ? s.toBuffer() : s), programId)[0];
const protocolConfig = pda('protocol-config');
const config = await program.account.protocolConfig.fetchNullable(protocolConfig) ?? { authority: payer.publicKey, indexCreator: payer.publicKey, indexCreatorWhitelist: [], permissionlessIndexCreation: false };
if (!config.permissionlessIndexCreation && !config.indexCreator.equals(payer.publicKey) &&
    !config.indexCreatorWhitelist.some(k => k.equals(payer.publicKey))) throw new Error('Wallet is not an approved creator');
if (execute && !(await connection.getAccountInfo(pda('staking-pool')))) throw new Error('Initialize the protocol staking pool before creating baskets');
async function fetchCreatedIndex(address) {
  for (let attempt = 0; attempt < 10; attempt++) {
    const state = await program.account.indexState.fetchNullable(address, 'confirmed');
    if (state?.largeBasketConfigured) return state;
    await new Promise(resolve => setTimeout(resolve, 1000));
  }
  throw new Error(`Confirmed index not yet readable: ${address}`);
}
// Jupiter's keyed API with JUPITER_API_KEY (lib/catalog.mjs's fetchJson sends the key), else keyless.
const { priceApi, swapApi } = jupiterApis();
const metadataProgram = new PublicKey('metaqbxxUerdq28cj1RbAWkYQm3ybzjb6a8bt518x1s');
const assets = {};
const failures = {};
const candidates = selected.filter(b => !b.blockers.length);
const usedSymbols = [...new Set(candidates.flatMap(b => [...b.components.map(c => c.symbol), ...(b.usdcCashSlot ? ['USDC'] : [])]))];
const mintAccounts = await connection.getMultipleAccountsInfo(usedSymbols.map(s => new PublicKey(catalog.tokens[s].mint)));
const slot = await connection.getSlot();
const prices = await fetchJson(`${priceApi}?ids=${usedSymbols.map(s => catalog.tokens[s].mint).join(',')}`);

// The price the rebalance oracle would post (scripts/lib/price-oracle.mjs): the live midpoint
// of a Jupiter round trip, which every fresh reference (Jupiter's price API unless it has gone
// quiet for an hour, DexScreener) confirms. Fixed-weight baskets rebalance against it, so every
// component must be priceable by it; it also sizes every basket's starting units.
const oraclePrice = async (symbol, token) =>
  (await oraclePrices([{ mint: token.mint, decimals: token.decimals, label: symbol }],
    { swapApi, priceApi, fetchJson: url => fetchJson(url) })).get(token.mint);

for (const [i, symbol] of usedSymbols.entries()) {
  try {
    const token = catalog.tokens[symbol];
    const mint = new PublicKey(token.mint);
    const info = mintAccounts[i];
    if (!info || info.owner.toBase58() !== token.tokenProgram) throw new Error('Mint owner mismatch');
    const parsed = unpackMint(mint, info, info.owner);
    if (info.owner.equals(TOKEN_2022_PROGRAM_ID)) {
      // Only extensions that leave transfers plain: metadata, and a transfer hook with no program
      // set (PUMP's). A fee, delegate, pause, freeze default or live hook could take from or
      // block the vault. The program itself refuses the transfer-fee extension too.
      const allowed = new Set([ExtensionType.MetadataPointer, ExtensionType.TokenMetadata, ExtensionType.TransferHook]);
      const refused = getExtensionTypes(parsed.tlvData).filter(e => !allowed.has(e));
      if (refused.length) throw new Error(`Token-2022 extensions not accepted: ${refused.map(e => ExtensionType[e]).join(', ')}`);
      const hook = getTransferHook(parsed);
      if (hook && !hook.programId.equals(PublicKey.default)) throw new Error('Token-2022 transfer hook has a program set');
    } else if (!info.owner.equals(TOKEN_PROGRAM_ID)) throw new Error('Unsupported token program');
    if (!parsed.isInitialized || parsed.decimals !== token.decimals) throw new Error('Mint decimals/initialization mismatch');
    const quote = prices[token.mint];
    // Jupiter's price API only refreshes on trades it sees, so tokens that trade mostly on their
    // own venue (MetaDAO's AMM) can read an hour old while liquid; the live round trip below
    // prices them instead.
    if (!quote || !Number.isFinite(quote.usdPrice) || quote.usdPrice <= 0) throw new Error('Missing Jupiter price');
    if (!(quote.liquidity >= 10000)) throw new Error('Less than $10,000 reported liquidity');
    // Confirm both directions at $10 per component, not merely the existence of a mint.
    let routePrice;
    if (token.mint !== USDC) {
      const buy = await fetchJson(`${swapApi}/quote?inputMint=${USDC}&outputMint=${token.mint}&amount=10000000&slippageBps=100`);
      const sell = await fetchJson(`${swapApi}/quote?inputMint=${token.mint}&outputMint=${USDC}&amount=${buy.outAmount}&slippageBps=100`);
      if (!buy.routePlan?.length || !sell.routePlan?.length || !(Number(sell.outAmount) >= 9700000))
        throw new Error('No liquid two-way $10 route within 3% round-trip loss');
      const tokenAmount = Number(buy.outAmount) / 10 ** parsed.decimals;
      routePrice = (10 + Number(sell.outAmount) / 1e6) / (2 * tokenAmount);
    }
    let price = quote.usdPrice;
    if (token.mint !== USDC) {
      const oracle = await oraclePrice(symbol, token);
      price = oracle.usd;
      // The oracle already holds every fresh reference within 3%; sizing also needs one within 1%.
      if (!oracle.references.some(ref => Math.abs(price / ref.usd - 1) <= 0.01))
        throw new Error(`Oracle price is more than 1% from every reference (${oracle.references.map(r => `${r.source} $${r.usd}`).join(', ')})`);
      if (Math.abs(price / routePrice - 1) > 0.02) throw new Error('Oracle price differs from current two-way route midpoint by more than 2%');
    } else price = 1; // The deployed protocol itself values native USDC at $1.
    assets[symbol] = { ...token, price, priceObservedAt: new Date().toISOString(), liquidityUsd: quote.liquidity,
      vaultSize: getAccountLenForMint(parsed) };
    console.log(`[checked] ${symbol}: $${price}`);
  } catch (error) {
    failures[symbol] = error.message;
    console.log(`[blocked] ${symbol}: ${error.message}`);
  }
}

const report = { checkedAt: new Date().toISOString(), program: catalog.programId, payer: payer.publicKey.toBase58(),
  walletLamports: await connection.getBalance(payer.publicKey), assets, baskets: [], estimatedRequiredLamports: 0 };
const plans = [];
const rentBySize = new Map();
for (const basket of selected) {
  const blockers = [...basket.blockers, ...basket.components.filter(c => failures[c.symbol]).map(c => `${c.symbol}: ${failures[c.symbol]}`)];
  if (blockers.length) { report.baskets.push({ symbol: basket.symbol, status: 'blocked', blockers }); continue; }
  try {
    const plan = sizeBasket(basket, assets, catalog.navToleranceUsd);
    const index = pda('index', payer.publicKey, basket.symbol);
    const indexMint = pda('index-mint', index);
    const vaultAuthority = pda('vault-authority', index);
    const metadata = PublicKey.findProgramAddressSync([Buffer.from('metadata'), metadataProgram.toBuffer(), indexMint.toBuffer()], metadataProgram)[0];
    const existing = await program.account.indexState.fetchNullable(index);
    if (existing) {
      // Never claim an unrelated existing index is this deployment or resize it.
      const saved = journal.baskets[basket.symbol];
      if (!saved || saved.index !== index.toBase58()) throw new Error('Index already exists without this deployment journal');
      if (saved.status === 'verified' && existing.symbol === basket.symbol && existing.indexMint.toBase58() === saved.indexMint) {
        report.baskets.push({ symbol: basket.symbol, status: 'already-deployed', index: saved.index, indexMint: saved.indexMint });
        continue;
      }
      throw new Error('Index already exists; inspect deployment journal and on-chain state before resuming');
    }
    const components = plan.components.map(c => ({ ...c, vault: getAssociatedTokenAddressSync(new PublicKey(c.mint), vaultAuthority, true, new PublicKey(c.tokenProgram)).toBase58() }));
    // Conservative upper bounds: inline index allocation, SPL mint, Metaplex
    // account, address lookup table, each component ATA, transaction buffer.
    let rent = 0;
    for (const size of [2200, 1800, 82, 679, 56 + (components.length * 2 + 8) * 32, ...components.map(c => c.vaultSize)])
      {
        if (!rentBySize.has(size)) rentBySize.set(size, await connection.getMinimumBalanceForRentExemption(size));
        rent += rentBySize.get(size);
      }
    rent += 1000000;
    const addresses = { index, indexMint, vaultAuthority, metadata };
    plans.push({ ...plan, components, addresses });
    report.baskets.push({ symbol: basket.symbol, status: 'ready-for-funding', initialNavUsd: plan.initialNavUsd,
      index: index.toBase58(), indexMint: indexMint.toBase58(), estimatedLamports: rent,
      components: components.map(({ symbol, mint, unitsPerIndex, price, targetWeightBps }) => ({ symbol, mint, unitsPerIndex, price, targetWeightBps })) });
    report.estimatedRequiredLamports += rent;
  } catch (error) { report.baskets.push({ symbol: basket.symbol, status: 'blocked', blockers: [error.message] }); }
}
report.fundingShortfallLamports = Math.max(0, report.estimatedRequiredLamports - report.walletLamports);
fs.writeFileSync(reportFile, JSON.stringify(report, null, 2) + '\n');
console.log(JSON.stringify({ baskets: report.baskets.map(({ symbol, status, blockers, initialNavUsd }) => ({ symbol, status, blockers, initialNavUsd })),
  walletSol: report.walletLamports / 1e9, requiredSol: report.estimatedRequiredLamports / 1e9, shortfallSol: report.fundingShortfallLamports / 1e9 }, null, 2));
if (!execute) process.exit(0);
if (!plans.length) throw new Error('No deployable baskets');
if (symbols && report.baskets.some(b => b.status === 'blocked')) throw new Error('Selected basket failed preflight');
if (report.fundingShortfallLamports) throw new Error('Insufficient SOL for the complete eligible batch; no transactions sent');

async function send(instructions, label, lookupTables = []) {
  for (let attempt = 0; attempt < 3; attempt++) {
    const latest = await connection.getLatestBlockhash();
    const tx = new VersionedTransaction(new TransactionMessage({ payerKey: payer.publicKey, recentBlockhash: latest.blockhash,
      instructions: [ComputeBudgetProgram.setComputeUnitLimit({ units: 1400000 }),
        ComputeBudgetProgram.setComputeUnitPrice({ microLamports: 5000 }), ...instructions] }).compileToV0Message(lookupTables));
    if (tx.serialize().length > 1232) throw new Error(`${label}: transaction too large`);
    tx.sign([payer]);
    const signature = await connection.sendTransaction(tx, { skipPreflight: false, maxRetries: 3 });
    journal.transactions.push({ label, signature, status: 'sent' }); saveJournal();
    let result;
    try { result = await connection.confirmTransaction({ ...latest, signature }, 'confirmed'); }
    catch (error) {
      if (!/block height exceeded/i.test(error.message)) throw error;
      const status = (await connection.getSignatureStatuses([signature], { searchTransactionHistory: true })).value[0];
      if (status && !status.err && ['confirmed', 'finalized'].includes(status.confirmationStatus)) result = { value: { err: null } };
      else if (!status && attempt < 2) {
        journal.transactions.at(-1).status = 'expired-unlanded'; saveJournal();
        console.log(`[retry] ${label}: expired without landing; rebuilding with a fresh blockhash`);
        continue;
      } else throw error;
    }
    if (result.value.err) throw new Error(`${label}: ${JSON.stringify(result.value.err)}`);
    journal.transactions.at(-1).status = 'confirmed'; saveJournal();
    console.log(`[confirmed] ${label}: ${signature}`);
    return;
  }
}

for (const plan of plans) {
  // Refresh sizing after long service retries instead of locking later baskets
  // to the beginning of a slow batch. Existing on-chain quantities are never reset.
  if (plan.components.some(c => Date.now() - Date.parse(c.priceObservedAt) > 120000)) {
    console.log(`[refresh] ${plan.symbol}: refresh initial NAV snapshot`);
    for (const component of plan.components) {
      if (component.mint === USDC) continue;
      const price = await oraclePrice(component.symbol, component);
      if (Math.abs(price / component.price - 1) > 0.02)
        throw new Error('Oracle price moved >2%; rerun route preflight');
      assets[component.symbol] = { ...assets[component.symbol], price, priceObservedAt: new Date().toISOString() };
    }
    // Size from the catalog entry: the plan's components already include any USDC cash slot.
    const refreshed = sizeBasket(selected.find(b => b.symbol === plan.symbol), assets, catalog.navToleranceUsd);
    plan.components = refreshed.components.map((c,i) => ({ ...c, vault: plan.components[i].vault }));
    plan.initialNavUsd = refreshed.initialNavUsd;
  }
  const { index, indexMint, vaultAuthority, metadata } = plan.addresses;
  // Rebalances price components from the oracle's signed prices; oracle pairs are unused.
  const onchain = plan.components.map(component => ({ ...component, oraclePair: PublicKey.default }));
  const accounts = { payer: payer.publicKey, authority: payer.publicKey, index, indexMint, vaultAuthority };
  const create = await program.methods.createLargeBasketIndex({ name: plan.name, symbol: plan.symbol, metadataUri: '', decimals: 6,
    feeRecipient: plan.feeRecipient ? new PublicKey(plan.feeRecipient) : config.authority,
    creatorFeeRecipient: plan.creatorFeeRecipient ? new PublicKey(plan.creatorFeeRecipient) : PublicKey.default,
    maxSupply: new anchor.BN(0), rebalanceDelaySeconds: new anchor.BN(0),
    kind: { [plan.kind]: {} }, fixedWeightQuoteMint: plan.kind === 'fixedWeights' ? new PublicKey(USDC) : PublicKey.default,
    fixedWeightRebalanceIntervalSeconds: new anchor.BN(plan.rebalanceIntervalSeconds), fixedWeightDriftThresholdBps: plan.driftThresholdBps,
    fixedWeightSpotEmaMaxDeviationBps: plan.kind === 'fixedWeights' ? 500 : 0,
    componentCount: onchain.length,
  }).accounts({ ...accounts, protocolConfig, tokenProgram: TOKEN_PROGRAM_ID, systemProgram: SystemProgram.programId }).instruction();
  const page = pda('large-basket-component-page', index, Buffer.from([0]));
  const init = await program.methods.initializeLargeBasketComponentPage({ pageIndex: 0, startComponentIndex: 0,
    components: onchain.map(c => ({ mint: new PublicKey(c.mint), unitsPerIndex: new anchor.BN(c.unitsPerIndex), targetWeightBps: c.targetWeightBps, oraclePair: c.oraclePair })) })
    .accounts({ ...accounts, page, associatedTokenProgram: ASSOCIATED_TOKEN_PROGRAM_ID, systemProgram: SystemProgram.programId })
    .remainingAccounts(onchain.flatMap(c => [
      { pubkey: new PublicKey(c.mint), isWritable: false, isSigner: false },
      { pubkey: new PublicKey(c.vault), isWritable: true, isSigner: false },
      { pubkey: new PublicKey(c.tokenProgram), isWritable: false, isSigner: false },
    ])).instruction();
  const finalize = await program.methods.finalizeLargeBasketConfig().accounts({ authority: payer.publicKey, index })
    .remainingAccounts([{ pubkey: page, isWritable: true, isSigner: false }]).instruction();
  const meta = await program.methods.createIndexMetadata({ uri: '' }).accounts({ ...accounts, metadata,
    metadataProgram, systemProgram: SystemProgram.programId, rent: SYSVAR_RENT_PUBKEY }).instruction();
  const priorLookup = journal.baskets[plan.symbol]?.lookupTable;
  journal.baskets[plan.symbol] = { index: index.toBase58(), indexMint: indexMint.toBase58(), initialNavUsd: plan.initialNavUsd, page: page.toBase58(),
    components: onchain.map(c => ({ ...c, oraclePair: c.oraclePair.toBase58() })), status: 'prepared' }; saveJournal();
  const [createLookup, lookupAddress] = AddressLookupTableProgram.createLookupTable({ authority: payer.publicKey, payer: payer.publicKey, recentSlot: await connection.getSlot('finalized') });
  const tableAddress = priorLookup ? new PublicKey(priorLookup) : lookupAddress;
  if (!priorLookup) await send([createLookup], `${plan.symbol}: create lookup table`);
  journal.baskets[plan.symbol].lookupTable = tableAddress.toBase58(); saveJournal();
  const priorTable = priorLookup ? (await connection.getAddressLookupTable(tableAddress)).value : null;
  const keys = [...new Map([create, init, finalize, meta].flatMap(ix => ix.keys).filter(k => !k.isSigner).map(k => [k.pubkey.toBase58(), k.pubkey])).values()]
    .filter(k => !priorTable?.state.addresses.some(p => p.equals(k)));
  for (let i = 0; i < keys.length; i += 20) await send([AddressLookupTableProgram.extendLookupTable({ authority: payer.publicKey,
    payer: payer.publicKey, lookupTable: tableAddress, addresses: keys.slice(i, i + 20) })], `${plan.symbol}: extend lookup table`);
  let lookup;
  for (let attempt = 0; attempt < 120; attempt++) {
    lookup = (await connection.getAddressLookupTable(tableAddress)).value;
    if (lookup && await connection.getSlot('finalized') > lookup.state.lastExtendedSlot) break;
    await new Promise(resolve => setTimeout(resolve, 500));
  }
  if (!lookup || await connection.getSlot('finalized') <= lookup.state.lastExtendedSlot) throw new Error('Lookup table did not finalize');
  // Construct and size every transaction before creating the index.
  for (const instructions of [[create, meta], [init, finalize]]) {
    const tx = new VersionedTransaction(new TransactionMessage({ payerKey: payer.publicKey, recentBlockhash: PublicKey.default.toBase58(),
      instructions: [ComputeBudgetProgram.setComputeUnitLimit({ units: 1400000 }),
        ComputeBudgetProgram.setComputeUnitPrice({ microLamports: 5000 }), ...instructions] }).compileToV0Message([lookup]));
    if (tx.serialize().length > 1232) throw new Error('Catalog transaction exceeds packet size');
  }
  await send([create, meta], `${plan.symbol}: create index and metadata`, [lookup]);
  await send([init, finalize], `${plan.symbol}: initialize component page and finalize`, [lookup]);
  const state = await fetchCreatedIndex(index);
  const pageState = await program.account.largeBasketComponentPage.fetch(page);
  if (!state.largeBasketConfigured || state.largeBasketComponentCount !== onchain.length || pageState.components.length !== onchain.length ||
      state.name !== plan.name || !state.feeRecipient.equals(plan.feeRecipient ? new PublicKey(plan.feeRecipient) : config.authority) ||
      !state.creatorFeeRecipient.equals(plan.creatorFeeRecipient ? new PublicKey(plan.creatorFeeRecipient) : PublicKey.default) ||
      pageState.components.some((c, i) => c.unitsPerIndex.toString() !== onchain[i].unitsPerIndex || !c.mint.equals(new PublicKey(onchain[i].mint)) || c.targetWeightBps !== onchain[i].targetWeightBps))
    throw new Error('Post-deployment verification failed');
  journal.baskets[plan.symbol].status = 'verified'; saveJournal();
}
