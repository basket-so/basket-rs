// Upgrade the mainnet basket program in place, keeping every basket paused while the new
// code goes live. Dry-run unless --execute. Each upgrade gets its own record directory.
//
//   node scripts/upgrade-program.mjs --dir docs/program-upgrade-2026-10-05            # preflight
//   node scripts/upgrade-program.mjs --dir docs/program-upgrade-2026-10-05 --execute --keep-paused
//   node scripts/upgrade-program.mjs --dir docs/program-upgrade-2026-10-05 --unpause   # after the app ships
//
// Mints and rebalances are paused during the upgrade (redeems can't be paused). Pausing runs
// against the program still deployed, so when the upgrade changes update_config's arguments,
// pass that program's IDL with --previous-idl <file>; unpausing uses target/idl/basket.json.
//
// The first dry-run records the binary hash in <dir>/preflight.json; --execute refuses any
// other binary. Without --keep-paused the original pause settings are restored right after
// the upgrade. Env: SOLANA_RPC_URL. Signs with deployer-keypair.json (upgrade + index authority).
import fs from 'node:fs';
import path from 'node:path';
import { spawn } from 'node:child_process';
import { createHash } from 'node:crypto';
import anchor from '@coral-xyz/anchor';
import { Connection, Keypair, PublicKey, SystemProgram, Transaction, TransactionInstruction, sendAndConfirmTransaction } from '@solana/web3.js';

const args = process.argv.slice(2);
const dir = args.includes('--dir') ? args[args.indexOf('--dir') + 1] : null;
if (!dir) throw new Error('Usage: node scripts/upgrade-program.mjs --dir <record dir> [--previous-idl <file>] [--execute [--keep-paused] | --unpause]');
const previousIdl = args.includes('--previous-idl') ? args[args.indexOf('--previous-idl') + 1] : null;
const execute = args.includes('--execute');
const keepPaused = args.includes('--keep-paused');
const unpause = args.includes('--unpause');
const MAINNET_GENESIS = '5eykt4UsFv8P8NJdTREpY1vzqKqZKvdpKuc147dw2N9d';

const rpc = process.env.SOLANA_RPC_URL ?? 'https://api.mainnet-beta.solana.com';
const signerPath = path.resolve('deployer-keypair.json');
const payer = Keypair.fromSecretKey(Uint8Array.from(JSON.parse(fs.readFileSync(signerPath, 'utf8'))));
const connection = new Connection(rpc, 'confirmed');
const provider = new anchor.AnchorProvider(connection, new anchor.Wallet(payer), { commitment: 'confirmed', preflightCommitment: 'confirmed' });
const program = new anchor.Program(JSON.parse(fs.readFileSync('target/idl/basket.json', 'utf8')), provider);
// The deployed program's client, for pausing before the upgrade.
const deployed = previousIdl ? new anchor.Program(JSON.parse(fs.readFileSync(previousIdl, 'utf8')), provider) : program;
const loader = new PublicKey('BPFLoaderUpgradeab1e11111111111111111111111');
const binary = fs.readFileSync('target/deploy/basket.so');
const sha = createHash('sha256').update(binary).digest('hex');
const [programData] = PublicKey.findProgramAddressSync([program.programId.toBuffer()], loader);
const PROGRAM_DATA_HEADER = 45;
// The loader refuses ExtendProgram calls under this many bytes (the CLI's auto-extend asks for
// the exact deficit and fails), so the script extends explicitly before uploading.
const MIN_EXTEND_BYTES = 10240;

fs.mkdirSync(dir, { recursive: true });
const preflightPath = path.join(dir, 'preflight.json');
const journalPath = path.join(dir, 'deployment.json');
const journal = fs.existsSync(journalPath) ? JSON.parse(fs.readFileSync(journalPath, 'utf8')) : { program: program.programId.toBase58(), binarySha256: sha, transactions: [], indexes: [] };
const save = () => fs.writeFileSync(journalPath, `${JSON.stringify(journal, null, 2)}\n`);
const redact = s => String(s).split(rpc).join('[RPC]');

async function record(action, builder) {
  const signature = await builder.rpc();
  journal.transactions.push({ action, signature, at: new Date().toISOString() });
  save();
  console.log(`${action}: ${signature}`);
}
async function readProgramData() {
  const account = await connection.getAccountInfo(programData);
  if (!account || !account.owner.equals(loader) || account.data[12] !== 1 || !new PublicKey(account.data.subarray(13, 45)).equals(payer.publicKey)) {
    throw new Error('ProgramData owner/upgrade authority mismatch');
  }
  return account;
}
// update_config rewrites every field, so carry the live values and change only the mint and
// rebalance pause flags. A program that still has a redeem pause gets its current value back
// unchanged, so no composition change's notice restarts (its PDA is passed for programs that
// would want it; programs that predate composition changes ignore the extra account).
async function setPauses(client, index, flags) {
  const s = await client.account.indexState.fetch(index);
  const compositionChange = PublicKey.findProgramAddressSync([Buffer.from('composition-change'), index.toBuffer()], client.programId)[0];
  const redeemPause = 'redeemingPaused' in s ? { redeemingPaused: s.redeemingPaused } : {};
  // update_config was renamed update_basket_config when the redeem pause was removed.
  const updateConfig = client.methods.updateBasketConfig ?? client.methods.updateConfig;
  return updateConfig({ feeRecipient: s.feeRecipient, creatorFeeRecipient: s.creatorFeeRecipient, maxSupply: s.maxSupply, rebalanceDelaySeconds: s.rebalanceDelaySeconds, ...redeemPause, ...flags }).accounts({ authority: payer.publicKey, index })
    .remainingAccounts([{ pubkey: compositionChange, isWritable: true, isSigner: false }]);
}
async function restorePauses() {
  for (const x of journal.indexes) await record(`restore pause settings ${x.symbol}`, await setPauses(program, new PublicKey(x.address), { mintingPaused: x.mintingPaused, rebalancingPaused: x.rebalancingPaused }));
  for (const x of journal.indexes) {
    const s = await program.account.indexState.fetch(new PublicKey(x.address));
    if (s.mintingPaused !== x.mintingPaused || s.rebalancingPaused !== x.rebalancingPaused || s.largeBasketOperationInProgress) throw new Error(`Final state mismatch: ${x.symbol}`);
  }
  journal.status = 'verified';
  journal.finishedAt = new Date().toISOString();
  journal.endingWalletLamports = await connection.getBalance(payer.publicKey);
  save();
  console.log(JSON.stringify({ status: journal.status, walletSOL: journal.endingWalletLamports / 1e9 }));
}

try {
  if (await connection.getGenesisHash() !== MAINNET_GENESIS) throw new Error('Not mainnet');
  if (unpause) {
    if (journal.status !== 'upgraded-paused') throw new Error(`Nothing to unpause (status: ${journal.status ?? 'none'})`);
    const live = await readProgramData();
    if (!live.data.subarray(PROGRAM_DATA_HEADER, PROGRAM_DATA_HEADER + binary.length).equals(binary)) throw new Error('Deployed bytes do not match this binary');
    await restorePauses();
    process.exit(0);
  }

  if (!fs.existsSync(preflightPath)) {
    fs.writeFileSync(preflightPath, `${JSON.stringify({ program: program.programId.toBase58(), binarySha256: sha, authority: payer.publicKey.toBase58(), genesis: MAINNET_GENESIS, recordedAt: new Date().toISOString() }, null, 2)}\n`);
    console.log(`Recorded preflight for binary ${sha}`);
  }
  const preflight = JSON.parse(fs.readFileSync(preflightPath, 'utf8'));
  if (program.programId.toBase58() !== preflight.program || sha !== preflight.binarySha256 || journal.binarySha256 !== sha || payer.publicKey.toBase58() !== preflight.authority) {
    throw new Error('Program, binary or authority differs from the recorded preflight');
  }
  const executable = await connection.getAccountInfo(program.programId);
  if (!executable?.executable || !executable.owner.equals(loader)) throw new Error('Not an upgradeable executable');
  const data = await readProgramData();
  const allocated = data.data.length - PROGRAM_DATA_HEADER;
  const alreadyDeployed = data.data.subarray(PROGRAM_DATA_HEADER, PROGRAM_DATA_HEADER + binary.length).equals(binary);
  const indexes = await deployed.account.indexState.all();
  for (const { account: s } of indexes) {
    if (s.largeBasketOperationInProgress) throw new Error(`Active operation: ${s.symbol}`);
    if (!s.authority.equals(payer.publicKey)) throw new Error(`Cannot pause ${s.symbol} during the upgrade`);
  }
  const balance = await connection.getBalance(payer.publicKey);
  const bufferRent = await connection.getMinimumBalanceForRentExemption(binary.length + 37);
  // ProgramData is extended first when the new binary is larger; that rent is permanent.
  const extendBytes = binary.length > allocated ? Math.max(binary.length - allocated, MIN_EXTEND_BYTES) : 0;
  const extendRent = extendBytes
    ? (await connection.getMinimumBalanceForRentExemption(data.data.length + extendBytes)) - data.lamports
    : 0;
  const existingBuffer = journal.buffer ? await connection.getAccountInfo(new PublicKey(journal.buffer)) : null;
  const required = alreadyDeployed ? 50_000_000 : Math.max(0, bufferRent - (existingBuffer?.lamports ?? 0)) + Math.max(0, extendRent) + 50_000_000;
  console.log(JSON.stringify({ walletSOL: balance / 1e9, requiredSOL: required / 1e9, bufferRentSOL: bufferRent / 1e9, bufferRefunded: true, extendBytes, extendRentSOL: Math.max(0, extendRent) / 1e9, binarySha256: sha, alreadyDeployed, baskets: indexes.length }));
  if (balance < required) throw new Error(`Fund ${payer.publicKey.toBase58()} with at least ${((required - balance) / 1e9).toFixed(3)} more SOL`);
  if (!execute) process.exit(0);

  if (!journal.indexes.length) {
    journal.indexes = indexes.map(({ publicKey, account: s }) => ({ address: publicKey.toBase58(), symbol: s.symbol, mintingPaused: s.mintingPaused, rebalancingPaused: s.rebalancingPaused }));
    journal.startingWalletLamports = balance;
    journal.startedAt = new Date().toISOString();
    save();
  }
  fs.mkdirSync(path.join(dir, 'backup'), { recursive: true });
  const backup = path.join(dir, 'backup', 'previous-program.so');
  if (!fs.existsSync(backup)) fs.writeFileSync(backup, data.data.subarray(PROGRAM_DATA_HEADER));
  // Pause through the program still deployed; once it's replaced, the new one is in charge.
  if (!alreadyDeployed) {
    for (const x of journal.indexes) {
      const s = await deployed.account.indexState.fetch(new PublicKey(x.address));
      if (!s.mintingPaused || !s.rebalancingPaused) await record(`pause ${x.symbol}`, await setPauses(deployed, new PublicKey(x.address), { mintingPaused: true, rebalancingPaused: true }));
    }
  }
  // Check again after pausing: a rebalance may have opened during the first scan.
  for (const x of journal.indexes) if ((await program.account.indexState.fetch(new PublicKey(x.address))).largeBasketOperationInProgress) throw new Error(`Operation raced with pause: ${x.symbol}`);

  if (!alreadyDeployed && extendBytes > 0) {
    const current = await readProgramData();
    if (current.data.length - PROGRAM_DATA_HEADER < binary.length) {
      const extendData = Buffer.alloc(8);
      extendData.writeUInt32LE(6, 0); // UpgradeableLoaderInstruction::ExtendProgram
      extendData.writeUInt32LE(extendBytes, 4);
      const extend = new TransactionInstruction({ programId: loader, data: extendData, keys: [
        { pubkey: programData, isSigner: false, isWritable: true },
        { pubkey: program.programId, isSigner: false, isWritable: true },
        { pubkey: SystemProgram.programId, isSigner: false, isWritable: false },
        { pubkey: payer.publicKey, isSigner: true, isWritable: true },
      ] });
      const signature = await sendAndConfirmTransaction(connection, new Transaction().add(extend), [payer], { commitment: 'confirmed' });
      journal.transactions.push({ action: `extend program data by ${extendBytes} bytes`, signature, at: new Date().toISOString() });
      save();
      console.log(`extended program data by ${extendBytes} bytes: ${signature}`);
    }
  }
  if (!alreadyDeployed) {
    const bufferPath = path.resolve(dir, 'backup', 'buffer-keypair.json');
    if (!fs.existsSync(bufferPath)) fs.writeFileSync(bufferPath, JSON.stringify([...Keypair.generate().secretKey]), { flag: 'wx' });
    journal.buffer = Keypair.fromSecretKey(Uint8Array.from(JSON.parse(fs.readFileSync(bufferPath, 'utf8')))).publicKey.toBase58();
    journal.status = 'uploading';
    save();
    const cli = path.resolve('.agave-2.3/releases/2.3.0/solana-release/bin/solana.exe');
    const cliArgs = ['program', 'deploy', 'target/deploy/basket.so', '--program-id', program.programId.toBase58(), '--buffer', bufferPath, '--upgrade-authority', signerPath, '--use-rpc', '--with-compute-unit-price', '5000', '--max-sign-attempts', '5', '--url', rpc, '--keypair', signerPath, '--output', 'json'];
    await new Promise((resolve, reject) => {
      const child = spawn(cli, cliArgs, { windowsHide: true, stdio: ['ignore', 'pipe', 'pipe'] });
      let output = '';
      const collect = d => { const s = redact(d.toString()); output += s; process.stdout.write(s); };
      child.stdout.on('data', collect);
      child.stderr.on('data', collect);
      child.on('error', reject);
      child.on('exit', code => {
        journal.transactions.push({ action: 'program upgrade', exitCode: code, output, at: new Date().toISOString() });
        save();
        code === 0 ? resolve() : reject(new Error(`Upgrade CLI exited ${code}`));
      });
    });
  }
  const upgraded = await readProgramData();
  if (!upgraded.data.subarray(PROGRAM_DATA_HEADER, PROGRAM_DATA_HEADER + binary.length).equals(binary)) throw new Error('Deployed bytes do not match the validated binary');
  journal.deployedSlot = upgraded.data.readBigUInt64LE(4).toString();
  journal.bufferClosed = journal.buffer ? !(await connection.getAccountInfo(new PublicKey(journal.buffer))) : true;
  if (keepPaused) {
    journal.status = 'upgraded-paused';
    save();
    console.log(JSON.stringify({ status: journal.status, slot: journal.deployedSlot, bufferClosed: journal.bufferClosed, next: 'deploy the app, then rerun with --unpause' }));
  } else {
    await restorePauses();
  }
} catch (e) {
  journal.lastError = redact(e.message);
  if (execute || unpause) save();
  console.error(journal.lastError);
  process.exitCode = 1;
}
