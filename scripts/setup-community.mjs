// Prepare mainnet for community baskets. Dry-run (simulation) unless --execute.
//
//   node scripts/setup-community.mjs --claim-symbols [--execute]
//   node scripts/setup-community.mjs --config --treasury <pubkey> --keeper <pubkey> \
//     --creation-fee-sol 0.2 --protocol-step-bps 4 --creator-step-bps 1 --staking-step-bps 5 [--execute]
//   node scripts/setup-community.mjs --config --creation-fee-sol 0.5 [--execute]
//   node scripts/setup-community.mjs --creation open|closed [--execute]
//
// --claim-symbols registers the symbol of every basket made before symbols were claimed, so no
// community basket can take one. Do it before turning on permissionless creation.
// --config sets (or changes) the terms new community baskets get: the treasury that receives the
// creation fee and the protocol fee share, the keeper that runs their rebalances, the creation
// fee, and the fee step. A creator picks each side's fee as a whole number of steps; one step
// splits into the protocol, creator and staking shares given (4 + 1 + 5 is a 0.10% step split
// 40/10/50). Existing community baskets keep the split they were created with. Once the config
// exists, any setting left out keeps its current value, so the fee can change on its own. The
// program caps the creation fee at 1 SOL.
// --creation open lets any wallet create a community basket (the protocol config's
// permissionless_index_creation); closed limits it to the index creator and the whitelist again.
// Opening refuses until the community terms are set and every existing symbol is claimed.
// Env: SOLANA_RPC_URL, ANCHOR_WALLET (protocol authority; default deployer-keypair.json).
import fs from 'node:fs';
import anchor from '@coral-xyz/anchor';
import { Connection, Keypair, LAMPORTS_PER_SOL, PublicKey, SystemProgram } from '@solana/web3.js';

const args = process.argv.slice(2);
const value = name => (args.includes(name) ? args[args.indexOf(name) + 1] : undefined);
const execute = args.includes('--execute');
const claimSymbols = args.includes('--claim-symbols');
const setConfig = args.includes('--config');
const creation = value('--creation');
if ([claimSymbols, setConfig, creation !== undefined].filter(Boolean).length !== 1) {
  throw new Error('Pass exactly one of --claim-symbols, --config or --creation open|closed');
}
if (creation !== undefined && creation !== 'open' && creation !== 'closed') throw new Error('--creation takes open or closed');

const idl = JSON.parse(fs.readFileSync('target/idl/basket.json', 'utf8'));
const authority = Keypair.fromSecretKey(Uint8Array.from(JSON.parse(fs.readFileSync(process.env.ANCHOR_WALLET ?? 'deployer-keypair.json', 'utf8'))));
const connection = new Connection(process.env.SOLANA_RPC_URL ?? 'https://api.mainnet-beta.solana.com', 'confirmed');
const program = new anchor.Program(idl, new anchor.AnchorProvider(connection, new anchor.Wallet(authority), { commitment: 'confirmed' }));
const pda = (...seeds) => PublicKey.findProgramAddressSync(seeds.map(s => (typeof s === 'string' ? Buffer.from(s) : s)), program.programId)[0];
const protocolConfig = pda('protocol-config');
const protocol = await program.account.protocolConfig.fetch(protocolConfig);
// A symbol is claimed under its key: 0 and 1 folded into O and I.
const claimKey = symbol => symbol.replace(/0/g, 'O').replace(/1/g, 'I');
const unclaimed = async () => {
  const indexes = await program.account.indexState.all();
  const missing = [];
  for (const { account } of indexes) if (!(await program.account.symbolClaim.fetchNullable(pda('symbol', claimKey(account.symbol))))) missing.push(account.symbol);
  return missing;
};
if (!protocol.authority.equals(authority.publicKey)) throw new Error(`Signer is not the protocol authority (${protocol.authority.toBase58()})`);

async function send(label, builder) {
  if (!execute) {
    await builder.simulate();
    console.log(`[dry-run] ${label} (simulated OK)`);
    return;
  }
  console.log(`${label}: ${await builder.rpc()}`);
}

if (creation !== undefined) {
  const open = creation === 'open';
  if (open) {
    const terms = await program.account.communityConfig.fetchNullable(pda('community-config'));
    if (!terms) throw new Error('Set the community terms first (--config)');
    const missing = await unclaimed();
    if (missing.length) throw new Error(`Claim these symbols first (--claim-symbols): ${missing.join(', ')}`);
  }
  console.log(`creation is ${protocol.permissionlessIndexCreation ? 'open' : 'closed'}; index creator ${protocol.indexCreator.toBase58()}, whitelist ${protocol.indexCreatorWhitelist.filter(k => !k.equals(PublicKey.default)).length}`);
  if (protocol.permissionlessIndexCreation === open) {
    console.log(`creation is already ${creation}; nothing to do`);
  } else {
    // The authority and the index creator stay as they are; only the switch changes.
    await send(`${open ? 'open' : 'close'} community basket creation`,
      program.methods.updateProtocolConfig({ authority: protocol.authority, indexCreator: protocol.indexCreator, permissionlessIndexCreation: open })
        .accounts({ authority: authority.publicKey, protocolConfig }));
  }
} else if (claimSymbols) {
  const indexes = await program.account.indexState.all();
  for (const { publicKey: index, account: state } of indexes.sort((a, b) => a.account.symbol.localeCompare(b.account.symbol))) {
    const symbolClaim = pda('symbol', claimKey(state.symbol));
    const claim = await program.account.symbolClaim.fetchNullable(symbolClaim);
    if (claim) {
      console.log(`${state.symbol.padEnd(8)} already claimed by ${claim.index.equals(index) ? 'itself' : claim.index.toBase58()}`);
      continue;
    }
    await send(`claim ${state.symbol}`, program.methods.claimIndexSymbol({ symbolClaimKey: claimKey(state.symbol) }).accounts({ payer: authority.publicKey, authority: authority.publicKey, protocolConfig, index, symbolClaim, systemProgram: SystemProgram.programId }));
  }
} else {
  // Community creation opens once this config exists, so every existing symbol must be claimed first.
  const missing = await unclaimed();
  if (missing.length) throw new Error(`Claim these symbols first (--claim-symbols): ${missing.join(', ')}`);
  const communityConfig = pda('community-config');
  const current = await program.account.communityConfig.fetchNullable(communityConfig);
  // A setting left out keeps the current config's value; without a config, all are required.
  const kept = (name, field) => {
    if (current) return current[field];
    throw new Error(`${name} is required`);
  };
  const key = (name, field) => (value(name) ? new PublicKey(value(name)) : kept(name, field));
  const bps = (name, field) => {
    if (value(name) === undefined) return kept(name, field);
    const v = Number(value(name));
    if (!Number.isInteger(v) || v < 0) throw new Error(`${name} must be a whole number of basis points`);
    return v;
  };
  let lamports;
  if (value('--creation-fee-sol') === undefined) {
    lamports = BigInt(kept('--creation-fee-sol', 'creationFeeLamports').toString());
  } else {
    const sol = Number(value('--creation-fee-sol'));
    if (!(sol >= 0)) throw new Error('--creation-fee-sol must be a number of SOL');
    lamports = BigInt(Math.round(sol * LAMPORTS_PER_SOL));
  }
  if (lamports > BigInt(LAMPORTS_PER_SOL)) throw new Error('The creation fee can be at most 1 SOL');
  const sol = Number(lamports) / LAMPORTS_PER_SOL;
  const config = {
    treasury: key('--treasury', 'treasury'),
    keeper: key('--keeper', 'keeper'),
    creationFeeLamports: new anchor.BN(lamports.toString()),
    protocolStepBps: bps('--protocol-step-bps', 'protocolStepBps'),
    creatorStepBps: bps('--creator-step-bps', 'creatorStepBps'),
    stakingStepBps: bps('--staking-step-bps', 'stakingStepBps'),
  };
  const step = config.protocolStepBps + config.creatorStepBps + config.stakingStepBps;
  if (step === 0 || step > 100) throw new Error('A fee step must be between 1 and 100 bps (1%)');
  console.log(`current: ${current ? JSON.stringify({ ...current, creationFeeLamports: current.creationFeeLamports.toString(), treasury: current.treasury.toBase58(), keeper: current.keeper.toBase58(), reserved: undefined }) : 'none'}`);
  await send(`set community config: treasury ${config.treasury.toBase58()}, keeper ${config.keeper.toBase58()}, creation fee ${sol} SOL, fee step ${step} bps split ${config.protocolStepBps}/${config.creatorStepBps}/${config.stakingStepBps} (protocol/creator/staking)`,
    program.methods.setCommunityConfig(config).accounts({ payer: authority.publicKey, authority: authority.publicKey, protocolConfig, communityConfig, systemProgram: SystemProgram.programId }));
}
