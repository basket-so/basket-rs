import fs from 'node:fs';
import path from 'node:path';
import { spawn } from 'node:child_process';
import { createHash } from 'node:crypto';
import anchor from '@coral-xyz/anchor';
import { Connection, Keypair, PublicKey, SystemProgram } from '@solana/web3.js';
import { ASSOCIATED_TOKEN_PROGRAM_ID, TOKEN_PROGRAM_ID, getAssociatedTokenAddressSync, getMint } from '@solana/spl-token';

const OLD = new PublicKey('9LNEoShrH93XekWQTFmZBdUdMu8ugJxBr5cbfqJQC1mw');
const NEW = new PublicKey('bskthjNMRWQ4ekDLxaAzA1e39ThPmEtUgHY3XHfs7qv');
const BASKET = new PublicKey('2rNBaMg5VAr1aMNCwAPdDZVgzzdTaNDebUnNqPFNmeta');
const USDC = new PublicKey('EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v');
const loader = new PublicKey('BPFLoaderUpgradeab1e11111111111111111111111');
const rpc = JSON.parse(fs.readFileSync('../basket-ui/public/mainnet-state.json','utf8')).rpcUrl;
const connection = new Connection(rpc, 'confirmed');
const signerPath = path.resolve('deployer-keypair.json');
const signer = Keypair.fromSecretKey(Uint8Array.from(JSON.parse(fs.readFileSync(signerPath,'utf8'))));
if (signer.publicKey.toBase58() !== 'HF6Qk5JnBTfa4MyL4RX2KJUaD9nRG7maNt7xJH9QUVk2') throw new Error('Unexpected signer');
if (await connection.getGenesisHash() !== '5eykt4UsFv8P8NJdTREpY1vzqKqZKvdpKuc147dw2N9d') throw new Error('Expected mainnet');
const binary = fs.readFileSync('target/deploy/basket.so');
const binarySha256 = createHash('sha256').update(binary).digest('hex');
const journalPath = 'docs/program-replacement/program.json';
const journal = fs.existsSync(journalPath) ? JSON.parse(fs.readFileSync(journalPath,'utf8')) : { oldProgram: OLD.toBase58(), newProgram: NEW.toBase58(), stakingMint: BASKET.toBase58(), transactions: [] };
const save = () => fs.writeFileSync(journalPath, JSON.stringify(journal,null,2)+'\n');
const solana = path.resolve('.agave-2.3/releases/2.3.0/solana-release/bin/solana.exe');
async function cli(args) {
  await new Promise((resolve,reject) => {
    const child = spawn(solana,[...args,'--url',rpc,'--keypair',signerPath],{windowsHide:true,stdio:['ignore','pipe','pipe']});
    let output='';
    child.stdout.on('data',d=>{ const s=d.toString().split(rpc).join('[RPC]');output+=s;process.stdout.write(s); });
    child.stderr.on('data',d=>{const s=d.toString().split(rpc).join('[RPC]');output+=s;process.stderr.write(s);});
    child.on('error',reject);
    child.on('exit',code=>{ journal.transactions.push({ action:args.slice(0,2).join(' '), output, exitCode:code, at:new Date().toISOString() });save();code===0?resolve():reject(new Error(`Solana CLI exited ${code}`)); });
  });
}
const [oldDataAddress] = PublicKey.findProgramAddressSync([OLD.toBuffer()],loader);
const oldData = await connection.getAccountInfo(oldDataAddress);
const requiredRent = await connection.getMinimumBalanceForRentExemption(binary.length+45);
const available = await connection.getBalance(signer.publicKey) + (oldData?.lamports ?? 0);
if (available < requiredRent + 1_000_000_000) throw new Error('Insufficient deployment + catalog funding');
console.log(JSON.stringify({binarySha256,bytes:binary.length,requiredRent,available}));
if (!process.argv.includes('--execute')) process.exit(0);
if (oldData) {
  if (!oldData.owner.equals(loader) || oldData.data[12] !== 1 || !new PublicKey(oldData.data.subarray(13,45)).equals(signer.publicKey)) throw new Error('Old upgrade authority mismatch');
  const report = JSON.parse(fs.readFileSync('docs/program-replacement/preflight.json','utf8'));
  const approved = JSON.parse(fs.readFileSync('docs/program-replacement/approved-stranded-vaults.json','utf8'));
  if (report.programId !== OLD.toBase58() || Date.now()-Date.parse(report.checkedAt)>300_000 || report.accounts.length<40 || report.mints.length!==14) throw new Error('Fresh comprehensive preflight required');
  if (report.blockers.length !== approved.blockers.length || report.blockers.some(x=>!approved.blockers.includes(x))) throw new Error('Unapproved preflight blocker');
  fs.writeFileSync('docs/program-replacement/previous-programdata.bin',oldData.data);
  journal.approvedStranding = approved; journal.binarySha256=binarySha256;save();
  await cli(['program','close',OLD.toBase58(),'--authority',signerPath,'--recipient',signer.publicKey.toBase58(),'--bypass-warning']);
  if (await connection.getAccountInfo(oldDataAddress)) throw new Error('Old ProgramData still exists after close');
  journal.oldClosedAt=new Date().toISOString();save();
}
if (!(await connection.getAccountInfo(NEW))?.executable) {
  const key = Keypair.fromSecretKey(Uint8Array.from(JSON.parse(fs.readFileSync('target/deploy/basket-keypair.json','utf8'))));
  if (!key.publicKey.equals(NEW)) throw new Error('Program key mismatch');
  const bufferPath = 'target/vanity/deployment-buffer-keypair.json';
  if (!fs.existsSync(bufferPath)) fs.writeFileSync(bufferPath,JSON.stringify([...Keypair.generate().secretKey]));
  await cli(['program','deploy','target/deploy/basket.so','--program-id','target/deploy/basket-keypair.json','--buffer',bufferPath,'--upgrade-authority',signerPath,'--max-len',String(binary.length),'--use-rpc','--with-compute-unit-price','5000','--max-sign-attempts','10']);
}
const [programData] = PublicKey.findProgramAddressSync([NEW.toBuffer()],loader);
const deployed = await connection.getAccountInfo(programData);
if (!deployed || !deployed.data.subarray(45,45+binary.length).equals(binary) || !new PublicKey(deployed.data.subarray(13,45)).equals(signer.publicKey)) throw new Error('Deployed binary/authority verification failed');
journal.binarySha256=binarySha256;journal.deployedAt=new Date().toISOString();save();
const idl = JSON.parse(fs.readFileSync('target/idl/basket.json','utf8'));
if (idl.address !== NEW.toBase58()) throw new Error('IDL mismatch');
const program = new anchor.Program(idl,new anchor.AnchorProvider(connection,new anchor.Wallet(signer),{commitment:'confirmed'}));
const pda = seed => PublicKey.findProgramAddressSync([Buffer.from(seed)],NEW)[0];
const protocolConfig=pda('protocol-config'),stakingPool=pda('staking-pool'),stakingAuthority=pda('staking-authority');
async function initialize(label,method) {const signature=await method.rpc();journal.transactions.push({action:label,signature,at:new Date().toISOString()});save();console.log(`${label}: ${signature}`);}
if (!await program.account.protocolConfig.fetchNullable(protocolConfig)) await initialize('initialize protocol',program.methods.initializeProtocol({indexCreator:signer.publicKey}).accounts({payer:signer.publicKey,authority:signer.publicKey,program:NEW,programData,protocolConfig,systemProgram:SystemProgram.programId}));
const mint = await getMint(connection,BASKET);
if (!mint.isInitialized || mint.decimals!==6) throw new Error('Invalid staking mint');
const stakeVault=getAssociatedTokenAddressSync(BASKET,stakingAuthority,true),rewardVault=getAssociatedTokenAddressSync(USDC,stakingAuthority,true);
if (!await program.account.stakingPool.fetchNullable(stakingPool)) await initialize('initialize staking pool',program.methods.initializeStakingPool().accounts({payer:signer.publicKey,authority:signer.publicKey,protocolConfig,stakingPool,stakingAuthority,basketMint:BASKET,rewardMint:USDC,stakeVault,rewardVault,associatedTokenProgram:ASSOCIATED_TOKEN_PROGRAM_ID,tokenProgram:TOKEN_PROGRAM_ID,systemProgram:SystemProgram.programId}));
const pool=await program.account.stakingPool.fetch(stakingPool);
if (!pool.basketMint.equals(BASKET) || !pool.rewardMint.equals(USDC)) throw new Error('Staking verification failed');
journal.accounts={programData,protocolConfig,stakingPool,stakingAuthority,stakeVault,rewardVault};save();
console.log('Verified new executable, protocol configuration, and staking pool.');
