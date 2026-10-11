// Set mint/redeem fee splits on the mainnet catalog baskets. Dry-run (simulation) unless --execute.
//
//   node scripts/update-basket-fees.mjs --protocol-bps 5 --staking-bps 5 --exclude USDX
//   node scripts/update-basket-fees.mjs --protocol-bps 4 --creator-bps 1 --staking-bps 5 --only BUYB --execute
//   node scripts/update-basket-fees.mjs --apply [--only SYM] [--execute]    # apply due increases
//   node scripts/update-basket-fees.mjs --cancel --only SYM [--execute]     # withdraw a pending increase
//
// The same split applies to mint and redeem, capped at 100 bps (1%) in total each way. Lower
// fees take effect at once. Raising any fee on a basket with holders needs three days' notice:
// the script proposes the increase, and --apply applies it once due (within seven days). A
// basket with no supply yet takes any change at once. Rates are snapshotted when an intent
// opens, so in-flight mints/redeems keep their old fees.
// Env: SOLANA_RPC_URL, ANCHOR_WALLET (index authority; default deployer-keypair.json).
import fs from 'node:fs';
import anchor from '@coral-xyz/anchor';
import { getMint } from '@solana/spl-token';
import { Connection, Keypair, PublicKey } from '@solana/web3.js';

const args = process.argv.slice(2);
const flag = (name, fallback) => (args.includes(name) ? args[args.indexOf(name) + 1] : fallback);
const bps = name => {
  const value = Number(flag(name, '0'));
  if (!Number.isInteger(value) || value < 0) throw new Error(`${name} must be a whole number of basis points`);
  return value;
};
const applyDue = args.includes('--apply');
const cancel = args.includes('--cancel');
const protocolBps = bps('--protocol-bps');
const creatorBps = bps('--creator-bps');
const stakingBps = bps('--staking-bps');
if (protocolBps + creatorBps + stakingBps > 100) throw new Error('Total fee per direction is capped at 100 bps (1%)');
const exclude = new Set((flag('--exclude', '') || '').split(',').filter(Boolean).map(s => s.toUpperCase()));
// --only SYM,... limits the change to those baskets (a new basket with its own split).
const only = new Set((flag('--only', '') || '').split(',').filter(Boolean).map(s => s.toUpperCase()));
if (cancel && !only.size) throw new Error('--cancel needs --only');
const execute = args.includes('--execute');

const idl = JSON.parse(fs.readFileSync('docs/program-upgrade/basket.idl.json', 'utf8'));
const authority = Keypair.fromSecretKey(Uint8Array.from(JSON.parse(fs.readFileSync(process.env.ANCHOR_WALLET ?? 'deployer-keypair.json', 'utf8'))));
const connection = new Connection(process.env.SOLANA_RPC_URL ?? 'https://api.mainnet-beta.solana.com', 'confirmed');
const program = new anchor.Program(idl, new anchor.AnchorProvider(connection, new anchor.Wallet(authority), { commitment: 'confirmed' }));
const baskets = JSON.parse(fs.readFileSync('docs/program-replacement/deployment.json', 'utf8')).baskets;
const pda = (seed, index) => PublicKey.findProgramAddressSync([Buffer.from(seed), index.toBuffer()], program.programId)[0];

const target = {
  mintFeeBps: protocolBps, redeemFeeBps: protocolBps,
  creatorMintFeeBps: creatorBps, creatorRedeemFeeBps: creatorBps,
  stakingMintFeeBps: stakingBps, stakingRedeemFeeBps: stakingBps,
};
const describe = s => `mint ${s.mintFeeBps}/${s.creatorMintFeeBps}/${s.stakingMintFeeBps} redeem ${s.redeemFeeBps}/${s.creatorRedeemFeeBps}/${s.stakingRedeemFeeBps}`;
const when = seconds => new Date(seconds * 1000).toISOString();
const mode = applyDue ? 'apply due increases' : cancel ? 'cancel pending increases' : `protocol/creator/staking = ${protocolBps}/${creatorBps}/${stakingBps} bps each way`;
console.log(`${execute ? 'EXECUTE' : 'Dry-run'}: ${mode}` + (exclude.size ? `, excluding ${[...exclude].join(', ')}` : ''));

async function send(symbol, label, builder, verify) {
  if (!execute) {
    const sim = await builder.simulate();
    console.log(`${symbol.padEnd(5)} ${label} (simulated OK, ${sim.raw.length} log lines)`);
    return;
  }
  const signature = await builder.rpc();
  await verify();
  console.log(`${symbol.padEnd(5)} ${label}  ${signature}`);
}

for (const [symbol, { index: address }] of Object.entries(baskets)) {
  if (exclude.has(symbol) || (only.size && !only.has(symbol))) { console.log(`${symbol.padEnd(5)} skipped (excluded)`); continue; }
  const index = new PublicKey(address);
  const state = await program.account.indexState.fetch(index);
  if (!state.authority.equals(authority.publicKey)) throw new Error(`${symbol}: signer is not the index authority (${state.authority.toBase58()})`);
  const feeChange = pda('fee-change', index);
  const pending = await program.account.feeChange.fetchNullable(feeChange);
  const changeAccounts = { authority: authority.publicKey, index, feeChange, proposer: pending?.proposer ?? authority.publicKey };
  // A redeem fee change restarts a pending composition change's notice, which needs its PDA.
  const notice = [{ pubkey: pda('composition-change', index), isWritable: true, isSigner: false }];

  if (applyDue || cancel) {
    if (!pending) { console.log(`${symbol.padEnd(5)} no pending increase`); continue; }
    if (cancel) {
      await send(symbol, `cancelled pending ${describe(pending.fees)}`, program.methods.cancelFeeChange().accounts(changeAccounts),
        async () => { if (await connection.getAccountInfo(feeChange)) throw new Error(`${symbol}: still pending`); });
      continue;
    }
    const now = Math.floor(Date.now() / 1000);
    if (now < pending.effectiveAt.toNumber()) { console.log(`${symbol.padEnd(5)} ${describe(pending.fees)} not due until ${when(pending.effectiveAt.toNumber())}`); continue; }
    await send(symbol, `${describe(state)} -> ${describe(pending.fees)}`, program.methods.applyFeeChange().accounts(changeAccounts).remainingAccounts(notice),
      async () => { const s = await program.account.indexState.fetch(index); if (!Object.entries(pending.fees).every(([k, v]) => s[k] === v)) throw new Error(`${symbol}: verification failed`); });
    continue;
  }

  if (Object.entries(target).every(([k, v]) => state[k] === v)) { console.log(`${symbol.padEnd(5)} already ${describe(state)}`); continue; }
  const raises = Object.entries(target).some(([k, v]) => v > state[k]);
  const supply = (await getMint(connection, state.indexMint)).supply;
  if (!raises || supply === 0n) {
    await send(symbol, `${describe(state)} -> ${describe(target)}`, program.methods.updateFees(target).accounts({ authority: authority.publicKey, index, indexMint: state.indexMint }).remainingAccounts(notice),
      async () => { const s = await program.account.indexState.fetch(index); if (!Object.entries(target).every(([k, v]) => s[k] === v)) throw new Error(`${symbol}: verification failed`); });
    continue;
  }
  if (pending) { console.log(`${symbol.padEnd(5)} an increase to ${describe(pending.fees)} is already pending (due ${when(pending.effectiveAt.toNumber())}); cancel it first`); continue; }
  await send(symbol, `proposed ${describe(state)} -> ${describe(target)}, applies in 3 days (--apply)`, program.methods.proposeFeeChange(target).accounts({ authority: authority.publicKey, index, feeChange }),
    async () => { if (!(await program.account.feeChange.fetchNullable(feeChange))) throw new Error(`${symbol}: proposal missing`); });
}
