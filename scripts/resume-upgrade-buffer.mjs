import { createHash } from 'node:crypto';
import fs from 'node:fs';
import { Connection, Keypair, PublicKey, TransactionInstruction, TransactionMessage, VersionedTransaction, ComputeBudgetProgram } from '@solana/web3.js';

// Fills in buffer chunks a failed `solana program deploy` left unwritten. With --dir <record
// dir> it resumes an upgrade-program.mjs run; without it, the 2026-09-21 upgrade's buffer.
const rpc=process.env.SOLANA_RPC_URL ?? 'https://api.mainnet-beta.solana.com';
const connection=new Connection(rpc,'confirmed');
const readKey=p=>Keypair.fromSecretKey(Uint8Array.from(JSON.parse(fs.readFileSync(p,'utf8'))));
const dir=process.argv.includes('--dir')?process.argv[process.argv.indexOf('--dir')+1]:null;
const preflightPath=dir?`${dir}/preflight.json`:'docs/program-upgrade/preflight.json';
const bufferKeyPath=dir?`${dir}/backup/buffer-keypair.json`:'target/program-upgrade/buffer-keypair.json';
const expectedBuffer=dir?JSON.parse(fs.readFileSync(`${dir}/deployment.json`,'utf8')).buffer:'2vdnSYfAk1uTLNLkDMDgqZQZMqBzKj22cDHBRPtcMLdt';
const payer=readKey('deployer-keypair.json'),buffer=readKey(bufferKeyPath);
const loader=new PublicKey('BPFLoaderUpgradeab1e11111111111111111111111');
const binary=fs.readFileSync('target/deploy/basket.so');
if(createHash('sha256').update(binary).digest('hex')!==JSON.parse(fs.readFileSync(preflightPath)).binarySha256)throw new Error('Validated binary changed');
if(payer.publicKey.toBase58()!=='HF6Qk5JnBTfa4MyL4RX2KJUaD9nRG7maNt7xJH9QUVk2'||buffer.publicKey.toBase58()!==expectedBuffer)throw new Error('Unexpected authority/buffer');
if(await connection.getGenesisHash()!=='5eykt4UsFv8P8NJdTREpY1vzqKqZKvdpKuc147dw2N9d')throw new Error('Expected mainnet');
async function missingChunks(){
  const info=await connection.getAccountInfo(buffer.publicKey);
  if(!info||!info.owner.equals(loader)||info.data.readUInt32LE(0)!==1||info.data[4]!==1||!new PublicKey(info.data.subarray(5,37)).equals(payer.publicKey)||info.data.length!==binary.length+37)throw new Error('Buffer identity/authority/size mismatch');
  const chunks=[];
  for(let offset=0;offset<binary.length;offset+=960){const bytes=binary.subarray(offset,Math.min(offset+960,binary.length));if(!info.data.subarray(37+offset,37+offset+bytes.length).equals(bytes))chunks.push({offset,bytes});}
  return chunks;
}
function transaction(chunk,blockhash){
  // UpgradeableLoaderInstruction::Write, bincode encoding (u32 variant, u32
  // offset, u64 Vec length). Matches the CLI's confirmed write instruction.
  const data=Buffer.alloc(16+chunk.bytes.length);data.writeUInt32LE(1,0);data.writeUInt32LE(chunk.offset,4);data.writeBigUInt64LE(BigInt(chunk.bytes.length),8);chunk.bytes.copy(data,16);
  const write=new TransactionInstruction({programId:loader,keys:[{pubkey:buffer.publicKey,isSigner:false,isWritable:true},{pubkey:payer.publicKey,isSigner:true,isWritable:false}],data});
  const tx=new VersionedTransaction(new TransactionMessage({payerKey:payer.publicKey,recentBlockhash:blockhash,instructions:[write,ComputeBudgetProgram.setComputeUnitPrice({microLamports:10000}),ComputeBudgetProgram.setComputeUnitLimit({units:10000})]}).compileToLegacyMessage());
  if(tx.serialize().length>1232)throw new Error('Write exceeds packet size');tx.sign([payer]);return tx;
}
let pending=await missingChunks();console.log(`Missing ${pending.length} of ${Math.ceil(binary.length/960)} chunks.`);
if(pending.length){const latest=await connection.getLatestBlockhash();const sim=await connection.simulateTransaction(transaction(pending[0],latest.blockhash),{sigVerify:true});if(sim.value.err)throw new Error(JSON.stringify({error:sim.value.err,logs:sim.value.logs}));}
for(let round=0;pending.length&&round<8;round++){
  for(let start=0;start<pending.length;start+=32){
    const batch=pending.slice(start,start+32),latest=await connection.getLatestBlockhash();
    const signatures=[];
    for(let j=0;j<batch.length;j+=2){
      await new Promise(r=>setTimeout(r,1000));
      const results=await Promise.allSettled(batch.slice(j,j+2).map(chunk=>connection.sendTransaction(transaction(chunk,latest.blockhash),{skipPreflight:false,maxRetries:2})));
      for(const result of results){if(result.status==='fulfilled')signatures.push(result.value);else console.log(`Write will retry: ${String(result.reason).split(rpc).join('[RPC]').slice(0,160)}`);}
    }
    for(let poll=0;signatures.length&&poll<20;poll++){
      const statuses=(await connection.getSignatureStatuses(signatures)).value;
      if(statuses.every(s=>s&&(s.err||s.confirmationStatus==='confirmed'||s.confirmationStatus==='finalized')))break;
      await new Promise(r=>setTimeout(r,500));
    }
    console.log(`Round ${round+1}: submitted ${Math.min(start+32,pending.length)}/${pending.length} missing chunks`);
  }
  pending=await missingChunks();console.log(`Remaining missing chunks: ${pending.length}`);
}
if(pending.length)throw new Error('Upload incomplete; safely rerun to resume');
console.log('Complete buffer matches final binary byte-for-byte.');
