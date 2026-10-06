// Snapshot the Switchboard feed definitions behind every catalog basket component.
//
//   node scripts/vendor-switchboard-feeds.mjs [--out <file>]...
//
// With the full definition, a price update goes straight to an oracle gateway instead of asking
// Crossbar to resolve the feed hash, so Crossbar outages (crossbar.switchboard.xyz lost its DNS
// record on 2026-10-05) cannot block mints, redeems or rebalances. Each definition is accepted
// only if it hashes to the feed id stored on-chain. Re-run after adding baskets or changing feeds.
// Env: SOLANA_RPC_URL, SWITCHBOARD_CROSSBAR_URL (tried first).
import fs from 'node:fs';
import anchor from '@coral-xyz/anchor';
import { Connection, Keypair, PublicKey } from '@solana/web3.js';
import { CrossbarClient, FeedHash } from '@switchboard-xyz/common';

const args = process.argv.slice(2);
const outputs = args.flatMap((arg, i) => (arg === '--out' ? [args[i + 1]] : []));
if (!outputs.length) outputs.push('scripts/switchboard-feeds.json');
const hosts = [process.env.SWITCHBOARD_CROSSBAR_URL, 'https://crossbar.switchboard.xyz', 'https://crossbar.switchboardlabs.xyz']
  .filter((url, i, all) => url && all.indexOf(url) === i);

const connection = new Connection(process.env.SOLANA_RPC_URL ?? 'https://api.mainnet-beta.solana.com', 'confirmed');
const idl = JSON.parse(fs.readFileSync('docs/program-upgrade/basket.idl.json', 'utf8'));
const program = new anchor.Program(idl, new anchor.AnchorProvider(connection, new anchor.Wallet(Keypair.generate()), {}));
const baskets = JSON.parse(fs.readFileSync('docs/program-replacement/deployment.json', 'utf8')).baskets;
const pda = (index, page) => PublicKey.findProgramAddressSync([Buffer.from('large-basket-component-page'), index.toBuffer(), Buffer.from([page])], program.programId)[0];

const feedIds = new Map(); // feed id -> symbols that use it
for (const [symbol, { index: address }] of Object.entries(baskets)) {
  const index = new PublicKey(address);
  const state = await program.account.indexState.fetch(index);
  for (let page = 0; page < state.largeBasketPageCount; page += 1) {
    for (const component of (await program.account.largeBasketComponentPage.fetch(pda(index, page))).components) {
      if (component.oraclePair.equals(PublicKey.default)) continue; // USDC is priced at $1 without a feed
      const id = `0x${component.oraclePair.toBuffer().toString('hex')}`;
      feedIds.set(id, [...(feedIds.get(id) ?? []), symbol]);
    }
  }
}

async function definition(id) {
  const errors = [];
  for (const host of hosts) {
    try {
      const { feed } = await new CrossbarClient(host).fetchOracleFeed(id);
      const computed = `0x${FeedHash.computeOracleFeedId(feed).toString('hex')}`;
      if (computed !== id) throw new Error(`definition hashes to ${computed}`);
      return feed;
    } catch (error) {
      errors.push(`${host}: ${error.message ?? error}`);
    }
  }
  throw new Error(`No verified definition for ${id} (${feedIds.get(id).join(', ')}): ${errors.join('; ')}`);
}

const feeds = {};
for (const id of [...feedIds.keys()].sort()) feeds[id] = await definition(id);
const json = `${JSON.stringify(feeds, null, 1)}\n`;
for (const file of outputs) fs.writeFileSync(file, json);
console.log(`Verified ${Object.keys(feeds).length} feed definitions across ${Object.keys(baskets).length} baskets -> ${outputs.join(', ')}`);
