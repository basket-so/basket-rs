import fs from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import anchor from '@coral-xyz/anchor';
import { Connection, Keypair, PublicKey } from '@solana/web3.js';
import { getMint, getAccount, TOKEN_PROGRAM_ID } from '@solana/spl-token';

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
const read = p => JSON.parse(fs.readFileSync(path.join(root, p), 'utf8'));
const catalog = read('docs/program-replacement/catalog.json');
const journal = read('docs/program-replacement/deployment.json');
const connection = new Connection(process.env.SOLANA_RPC_URL ?? JSON.parse(fs.readFileSync(path.join(root, '../basket-ui/public/mainnet-state.json'), 'utf8')).rpcUrl, 'confirmed');
// Verification does not require access to a signing key.
const program = new anchor.Program(read('target/idl/basket.json'), new anchor.AnchorProvider(connection, new anchor.Wallet(Keypair.generate()), {}));
if (program.programId.toBase58() !== catalog.programId) throw new Error('IDL/catalog mismatch');
const rows = [];
for (const [symbol, saved] of Object.entries(journal.baskets)) {
  const expected = catalog.baskets.find(b => b.symbol === symbol);
  const index = new PublicKey(saved.index);
  const state = await program.account.indexState.fetch(index);
  const [authority] = PublicKey.findProgramAddressSync([Buffer.from('vault-authority'), index.toBuffer()], program.programId);
  const mint = await getMint(connection, new PublicKey(saved.indexMint));
  const page = await program.account.largeBasketComponentPage.fetch(new PublicKey(saved.page));
  if (!state.largeBasketConfigured || !page.finalized || !page.index.equals(index) || state.largeBasketComponentCount !== saved.components.length ||
      state.symbol !== symbol || state.name !== expected.name || !(expected.kind in state.kind) ||
      state.fixedWeightDriftThresholdBps !== expected.driftThresholdBps ||
      state.fixedWeightRebalanceIntervalSeconds.toString() !== String(expected.rebalanceIntervalSeconds) ||
      !state.indexMint.equals(mint.address) || !mint.mintAuthority?.equals(authority) || mint.decimals !== 6 ||
      page.components.length !== saved.components.length) throw new Error(`${symbol}: index state mismatch`);
  let snapshotNav = 0;
  for (const [i, component] of page.components.entries()) {
    const e = saved.components[i];
    const vault = await getAccount(connection, new PublicKey(e.vault), 'confirmed', TOKEN_PROGRAM_ID);
    if (!component.mint.equals(new PublicKey(e.mint)) || !component.oraclePair.equals(new PublicKey(e.oraclePair)) ||
        component.unitsPerIndex.toString() !== e.unitsPerIndex || component.targetWeightBps !== e.targetWeightBps ||
        !vault.owner.equals(authority) || !vault.mint.equals(component.mint)) throw new Error(`${symbol}: component ${i} mismatch`);
    if (!component.accountedReserve.isZero() || vault.amount !== 0n || mint.supply !== 0n) throw new Error('New index must start empty');
    snapshotNav += Number(component.unitsPerIndex.toString()) / 10 ** e.decimals * e.price;
  }
  if (Math.abs(snapshotNav - 1) > catalog.navToleranceUsd) throw new Error(`${symbol}: initial NAV mismatch`);
  const metadataProgram = new PublicKey('metaqbxxUerdq28cj1RbAWkYQm3ybzjb6a8bt518x1s');
  const [metadata] = PublicKey.findProgramAddressSync([Buffer.from('metadata'), metadataProgram.toBuffer(), mint.address.toBuffer()], metadataProgram);
  const metadataAccount = await connection.getAccountInfo(metadata);
  if (!metadataAccount?.owner.equals(metadataProgram) || !metadataAccount.data.subarray(33,65).equals(mint.address.toBuffer()))
    throw new Error(`${symbol}: missing or invalid Metaplex metadata`);
  let cursor = 65;
  const metadataString = () => {
    const length = metadataAccount.data.readUInt32LE(cursor); cursor += 4;
    if (cursor + length > metadataAccount.data.length) throw new Error('Malformed metadata');
    const text = metadataAccount.data.subarray(cursor, cursor + length).toString('utf8').replace(/\0+$/, '');
    cursor += length;
    return text;
  };
  if (metadataString() !== expected.name || metadataString() !== symbol) throw new Error(`${symbol}: metadata name/symbol mismatch`);
  const lookup = (await connection.getAddressLookupTable(new PublicKey(saved.lookupTable))).value;
  if (!lookup || lookup.state.deactivationSlot !== 18446744073709551615n ||
      !lookup.state.addresses.some(k => k.equals(index))) throw new Error(`${symbol}: lookup table missing/deactivated`);
  saved.status = 'verified';
  saved.verifiedAt = new Date().toISOString();
  saved.supplyAtoms = mint.supply.toString();
  rows.push({ symbol, name: expected.name, kind: expected.kind, index: saved.index, mint: saved.indexMint,
    lookupTable: saved.lookupTable, initialNavUsd: snapshotNav, supplyAtoms: mint.supply.toString() });
  console.log(`[verified] ${symbol}: ${saved.indexMint}, initial NAV ${snapshotNav}, supply ${mint.supply}`);
}
fs.writeFileSync(path.join(root, 'docs/program-replacement/deployment.json'), JSON.stringify(journal, null, 2) + '\n');
const report = read('docs/program-replacement/readiness.json');
report.baskets = catalog.baskets.map(b => report.baskets.find(r => r.symbol === b.symbol) ??
  { symbol: b.symbol, status: b.blockers.length ? 'blocked' : 'not-checked', blockers: b.blockers });
for (const b of report.baskets) {
  const row = rows.find(r => r.symbol === b.symbol);
  if (row) { b.status = 'deployed-verified'; b.initialNavUsd = row.initialNavUsd; b.index = row.index; b.indexMint = row.mint; }
}
report.finalWalletLamports = await connection.getBalance(new PublicKey(report.payer));
report.finalVerifiedAt = new Date().toISOString();
fs.writeFileSync(path.join(root, 'docs/program-replacement/readiness.json'), JSON.stringify(report, null, 2) + '\n');
const lines = ['# Mainnet basket deployment', '', `Verified ${report.finalVerifiedAt}. Program: \`${catalog.programId}\`.`, '',
  'Initial NAV is valued at the recorded creation oracle snapshot. Zero supply means the index is configured and awaiting its first backed mint.', '',
  '| Basket | Policy | Initial NAV | Index token mint |', '| --- | --- | --- | --- |',
  ...rows.map(r => `| ${r.symbol} — ${r.name} | ${r.kind === 'fixedWeights' ? 'Fixed weights' : 'Fixed units (dynamic weights)'} | $${r.initialNavUsd.toFixed(9)} | [${r.mint}](https://solscan.io/token/${r.mint}) |`), '',

  'The deployment journal contains confirmed transaction signatures, component quantities, oracle feed IDs and per-index lookup tables.', ''];
fs.writeFileSync(path.join(root, 'docs/program-replacement/DEPLOYMENT.md'), lines.join('\n'));
console.log(`Verified ${rows.length} indexes; wallet balance ${report.finalWalletLamports / 1e9} SOL.`);
