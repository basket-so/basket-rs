import fs from 'node:fs';
import path from 'node:path';
import {spawn} from 'node:child_process';
import {createHash} from 'node:crypto';
import anchor from '@coral-xyz/anchor';
import {Connection,PublicKey,Keypair,SystemProgram} from '@solana/web3.js';
import {getAssociatedTokenAddressSync,ASSOCIATED_TOKEN_PROGRAM_ID,TOKEN_PROGRAM_ID} from '@solana/spl-token';
const rpc=process.env.SOLANA_RPC_URL ?? JSON.parse(fs.readFileSync('../basket-ui/public/mainnet-state.json')).rpcUrl;
const execute=process.argv.includes('--execute');
const journalPath='docs/program-upgrade/deployment.json';
const preflight=JSON.parse(fs.readFileSync('docs/program-upgrade/preflight.json'));
const signerPath=path.resolve('deployer-keypair.json');
const payer=Keypair.fromSecretKey(Uint8Array.from(JSON.parse(fs.readFileSync(signerPath))));
const c=new Connection(rpc,'confirmed');
const program=new anchor.Program(JSON.parse(fs.readFileSync('target/idl/basket.json')),new anchor.AnchorProvider(c,new anchor.Wallet(payer),{commitment:'confirmed',preflightCommitment:'confirmed'}));
const loader=new PublicKey('BPFLoaderUpgradeab1e11111111111111111111111');
const usdc=new PublicKey('EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v');
const binary=fs.readFileSync('target/deploy/basket.so');
const sha=createHash('sha256').update(binary).digest('hex');
const pda=(seed,...parts)=>PublicKey.findProgramAddressSync([Buffer.from(seed),...parts.map(x=>x.toBuffer?x.toBuffer():x)],program.programId)[0];
const [programData]=PublicKey.findProgramAddressSync([program.programId.toBuffer()],loader);
let journal=fs.existsSync(journalPath)?JSON.parse(fs.readFileSync(journalPath)):{program:program.programId.toBase58(),binarySha256:sha,transactions:[],indexes:[]};
const save=()=>fs.writeFileSync(journalPath,JSON.stringify(journal,null,2)+'\n');
async function record(action,builder){const signature=await builder.rpc();journal.transactions.push({action,signature,at:new Date().toISOString()});save();console.log(`${action}: ${signature}`);}
async function readData(){const x=await c.getAccountInfo(programData);if(!x||!x.owner.equals(loader)||x.data[12]!==1||!new PublicKey(x.data.subarray(13,45)).equals(payer.publicKey))throw new Error('ProgramData owner/upgrade authority mismatch');return x;}
// Pausing or unpausing redemptions restarts a pending composition change's notice: pass its PDA.
async function configure(pk,flags){const s=await program.account.indexState.fetch(pk);return program.methods.updateConfig({feeRecipient:s.feeRecipient,creatorFeeRecipient:s.creatorFeeRecipient,maxSupply:s.maxSupply,rebalanceDelaySeconds:s.rebalanceDelaySeconds,...flags}).accounts({authority:payer.publicKey,index:pk}).remainingAccounts([{pubkey:pda('composition-change',pk),isWritable:true,isSigner:false}]);}
try {
if(program.programId.toBase58()!==preflight.program||sha!==preflight.binarySha256||journal.binarySha256!==sha||payer.publicKey.toBase58()!==preflight.authority)throw new Error('Validated program/binary/authority changed');
if(await c.getGenesisHash()!==preflight.genesis)throw new Error('Not mainnet');
const executable=await c.getAccountInfo(program.programId);if(!executable?.executable||!executable.owner.equals(loader))throw new Error('Not upgradeable executable');
const data=await readData();
const alreadyDeployed=data.data.subarray(45,45+binary.length).equals(binary);
const indexes=await program.account.indexState.all();
for(const {publicKey,account:s} of indexes){if(s.largeBasketOperationInProgress)throw new Error(`Active operation: ${s.symbol}`);if(!s.authority.equals(payer.publicKey))throw new Error(`Cannot protect ${s.symbol} during upgrade`);}
const balance=await c.getBalance(payer.publicKey),rent=await c.getMinimumBalanceForRentExemption(binary.length+37);
console.log(JSON.stringify({walletSOL:balance/1e9,bufferRentSOL:rent/1e9,binarySha256:sha,alreadyDeployed,baskets:indexes.length}));
const existingBuffer=journal.buffer ? await c.getAccountInfo(new PublicKey(journal.buffer)) : null;
if(!alreadyDeployed&&balance<Math.max(0,rent-(existingBuffer?.lamports??0))+50_000_000)throw new Error('Insufficient buffer rent plus fee reserve');
if(!execute)process.exit(0);
if(!journal.indexes.length){journal.indexes=indexes.map(({publicKey,account:s})=>({address:publicKey.toBase58(),symbol:s.symbol,mintingPaused:s.mintingPaused,redeemingPaused:s.redeemingPaused,rebalancingPaused:s.rebalancingPaused}));journal.startingWalletLamports=balance;journal.startedAt=new Date().toISOString();save();}
fs.mkdirSync('target/program-upgrade',{recursive:true});
if(!fs.existsSync('target/program-upgrade/previous-program.so'))fs.writeFileSync('target/program-upgrade/previous-program.so',data.data.subarray(45));
for(const x of journal.indexes){const pk=new PublicKey(x.address),s=await program.account.indexState.fetch(pk);if(!s.mintingPaused||!s.redeemingPaused||!s.rebalancingPaused)await record(`pause ${x.symbol}`,await configure(pk,{mintingPaused:true,redeemingPaused:true,rebalancingPaused:true}));}
// Check again after pausing: an operation may have opened during the first scan.
for(const x of journal.indexes){if((await program.account.indexState.fetch(new PublicKey(x.address))).largeBasketOperationInProgress)throw new Error(`Operation raced with pause: ${x.symbol}`);}
if(!alreadyDeployed){
 const bufferPath=path.resolve('target/program-upgrade/buffer-keypair.json');
 if(!fs.existsSync(bufferPath))fs.writeFileSync(bufferPath,JSON.stringify([...Keypair.generate().secretKey]),{flag:'wx'});
 journal.buffer=Keypair.fromSecretKey(Uint8Array.from(JSON.parse(fs.readFileSync(bufferPath)))).publicKey.toBase58();journal.status='uploading';save();
 const cli=path.resolve('.agave-2.3/releases/2.3.0/solana-release/bin/solana.exe');
 const args=['program','deploy','target/deploy/basket.so','--program-id',program.programId.toBase58(),'--buffer',bufferPath,'--upgrade-authority',signerPath,'--use-rpc','--with-compute-unit-price','5000','--max-sign-attempts','5','--url',rpc,'--keypair',signerPath,'--output','json'];
 await new Promise((resolve,reject)=>{const child=spawn(cli,args,{windowsHide:true,stdio:['ignore','pipe','pipe']});let output='';const collect=d=>{const s=d.toString().split(rpc).join('[RPC]');output+=s;process.stdout.write(s);};child.stdout.on('data',collect);child.stderr.on('data',collect);child.on('error',reject);child.on('exit',code=>{journal.transactions.push({action:'program upgrade',exitCode:code,output,at:new Date().toISOString()});save();code===0?resolve():reject(new Error(`Upgrade CLI exited ${code}`));});});
}
const upgraded=await readData();if(!upgraded.data.subarray(45,45+binary.length).equals(binary))throw new Error('Deployed bytes do not match validated binary');
journal.status='migrating';journal.deployedSlot=upgraded.data.readBigUInt64LE(4).toString();save();
for(const x of journal.indexes){const index=new PublicKey(x.address),s=await program.account.indexState.fetch(index);if(!s.kind.fixedWeights)continue;const pages=Array.from({length:s.largeBasketPageCount},(_,i)=>pda('large-basket-component-page',index,Buffer.from([i])));const records=await Promise.all(pages.map(p=>program.account.largeBasketComponentPage.fetch(p)));if(records.some(p=>p.components.some(c=>c.mint.equals(usdc))))continue;
 const vaultAuthority=pda('vault-authority',index),vaultQuote=getAssociatedTokenAddressSync(usdc,vaultAuthority,true),quotePage=pda('large-basket-component-page',index,Buffer.from([Math.floor(s.largeBasketComponentCount/10)]));
 await record(`register USDC ${x.symbol}`,program.methods.registerRebalanceQuote().accounts({payer:payer.publicKey,index,indexMint:s.indexMint,vaultAuthority,quoteMint:usdc,vaultQuote,quotePage,associatedTokenProgram:ASSOCIATED_TOKEN_PROGRAM_ID,tokenProgram:TOKEN_PROGRAM_ID,systemProgram:SystemProgram.programId}).remainingAccounts(pages.map(pubkey=>({pubkey,isWritable:false,isSigner:false}))));
 const page=await program.account.largeBasketComponentPage.fetch(quotePage),cash=page.components.find(c=>c.mint.equals(usdc)),vault=await c.getAccountInfo(vaultQuote);
 if(!cash||cash.targetWeightBps!==0||cash.accountedReserve.toString()!==vault.data.readBigUInt64LE(64).toString())throw new Error(`Cash migration failed verification: ${x.symbol}`);
}
for(const x of journal.indexes){const pk=new PublicKey(x.address);await record(`restore pause settings ${x.symbol}`,await configure(pk,{mintingPaused:x.mintingPaused,redeemingPaused:x.redeemingPaused,rebalancingPaused:x.rebalancingPaused}));}
for(const x of journal.indexes){const s=await program.account.indexState.fetch(new PublicKey(x.address));if(s.mintingPaused!==x.mintingPaused||s.redeemingPaused!==x.redeemingPaused||s.rebalancingPaused!==x.rebalancingPaused||s.largeBasketOperationInProgress)throw new Error(`Final state mismatch: ${x.symbol}`);}
journal.status='verified';journal.finishedAt=new Date().toISOString();journal.endingWalletLamports=await c.getBalance(payer.publicKey);journal.bufferClosed=!(await c.getAccountInfo(new PublicKey(journal.buffer)));save();console.log(JSON.stringify({status:journal.status,slot:journal.deployedSlot,walletSOL:journal.endingWalletLamports/1e9,bufferClosed:journal.bufferClosed}));
}catch(e){journal.lastError=String(e.message).split(rpc).join('[RPC]');if(execute)save();console.error(journal.lastError);process.exitCode=1;}
