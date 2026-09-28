import fs from 'node:fs';
import anchor from '@coral-xyz/anchor';
import { Connection, PublicKey } from '@solana/web3.js';
import { getMint, TOKEN_PROGRAM_ID, TOKEN_2022_PROGRAM_ID } from '@solana/spl-token';

const idl = JSON.parse(fs.readFileSync('docs/program-replacement/previous-idl.json', 'utf8'));
const rpc = JSON.parse(fs.readFileSync('../basket-ui/public/mainnet-state.json', 'utf8')).rpcUrl;
const connection = new Connection(rpc, 'finalized');
const programId = new PublicKey(idl.address);
const coder = new anchor.BorshAccountsCoder(idl);
if (programId.toBase58() !== '9LNEoShrH93XekWQTFmZBdUdMu8ugJxBr5cbfqJQC1mw') throw new Error('Unexpected previous program');
const accounts = await connection.getProgramAccounts(programId);
if (!accounts.length) throw new Error('No previous program accounts found; cannot establish empty state');
const report = { checkedAt: new Date().toISOString(), programId: programId.toBase58(), accounts: [], tokenAccounts: [], mints: [], blockers: [] };
const authorities = new Set();
const serialize = value => JSON.parse(JSON.stringify(value));
for (const account of accounts) {
  const definition = idl.accounts.find(a => Buffer.from(a.discriminator).equals(account.account.data.subarray(0, 8)));
  if (!definition) { report.blockers.push(`Unknown program account ${account.pubkey}`); continue; }
  const state = coder.decode(definition.name, account.account.data);
  report.accounts.push({ address: account.pubkey.toBase58(), type: definition.name, state: serialize(state) });
  if (definition.name === 'IndexState') {
    const mint = await getMint(connection, state.index_mint);
    report.mints.push({ symbol: state.symbol, address: mint.address.toBase58(), supply: mint.supply.toString(), decimals: mint.decimals });
    if (mint.supply !== 0n) report.blockers.push(`${state.symbol} has nonzero supply: ${mint.supply}`);
    if (state.large_basket_operation_in_progress || state.pending_component_count) report.blockers.push(`${state.symbol} has an active operation`);
    authorities.add(PublicKey.findProgramAddressSync([Buffer.from('vault-authority'), account.pubkey.toBuffer()], programId)[0].toBase58());
  }
  if (definition.name === 'StakingPool') {
    for (const key of ['total_staked', 'unallocated_rewards', 'reward_remainder_scaled']) if (!state[key].isZero()) report.blockers.push(`Staking ${key} is nonzero`);
    authorities.add(PublicKey.findProgramAddressSync([Buffer.from('staking-authority')], programId)[0].toBase58());
  }
  if (definition.name === 'StakePosition') for (const key of ['amount_staked', 'pending_rewards', 'pending_rewards_scaled']) if (!state[key].isZero()) report.blockers.push(`Position ${account.pubkey} ${key} is nonzero`);
  if (definition.name === 'LargeBasketIntent') {
    authorities.add(account.pubkey.toBase58());
    if ('Open' in state.status || 'open' in state.status) report.blockers.push(`Open intent ${account.pubkey}`);
  }
}
for (const authority of authorities) for (const tokenProgram of [TOKEN_PROGRAM_ID, TOKEN_2022_PROGRAM_ID]) {
  const tokens = await connection.getParsedTokenAccountsByOwner(new PublicKey(authority), { programId: tokenProgram });
  for (const token of tokens.value) {
    const info = token.account.data.parsed.info;
    report.tokenAccounts.push({ address: token.pubkey.toBase58(), authority, mint: info.mint, amount: info.tokenAmount.amount });
    if (BigInt(info.tokenAmount.amount) !== 0n) report.blockers.push(`Vault ${token.pubkey} contains ${info.tokenAmount.amount} atoms of ${info.mint}`);
  }
}
fs.mkdirSync('docs/program-replacement', { recursive: true });
fs.writeFileSync('docs/program-replacement/preflight.json', JSON.stringify(report, null, 2) + '\n');
console.log(JSON.stringify({ accountCount: report.accounts.length, mints: report.mints, tokenAccountsChecked: report.tokenAccounts.length, blockers: report.blockers }, null, 2));
if (report.blockers.length) process.exitCode = 2;
