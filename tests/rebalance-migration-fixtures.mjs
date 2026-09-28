import fs from 'node:fs';
import assert from 'node:assert/strict';
import anchor from '@coral-xyz/anchor';
import { PublicKey, Keypair, SystemProgram } from '@solana/web3.js';
import { AccountLayout, MintLayout, TOKEN_PROGRAM_ID, ASSOCIATED_TOKEN_PROGRAM_ID, getAssociatedTokenAddressSync, getOrCreateAssociatedTokenAccount, getAccount, getMint, mintTo } from '@solana/spl-token';
const usdc = new PublicKey('EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v');
const bn = n => new anchor.BN(n);
const meta = (pubkey, isWritable=false) => ({ pubkey, isWritable, isSigner:false });
export async function prepareRebalanceFixtures(dir, programId, payer) {
  const program = new anchor.Program(JSON.parse(fs.readFileSync('target/idl/basket.json')), { connection: {}, publicKey: payer });
  const types = program.idl.types;
  function encode(name, value) {
    const {layout,discriminator}=program.coder.accounts.accountLayouts.get(name);
    const data=Buffer.alloc(4096);const len=layout.encode(value,data);
    return Buffer.concat([Buffer.from(discriminator),data.subarray(0,len)]);
  }
  function zero(t) {
    if (t === 'pubkey') return PublicKey.default;
    if (t === 'bool') return false;
    if (t === 'string') return '';
    if (typeof t === 'string') return ['u64','i64','u128','i128'].includes(t) ? bn(0) : 0;
    if (t.array) return Array.from({length:t.array[1]},()=>zero(t.array[0]));
    if (t.vec) return [];
    const def = types.find(x=>x.name===t.defined.name).type;
    if (def.kind==='enum') return {[def.variants[0].name]:{}};
    return Object.fromEntries(def.fields.map(f=>[f.name,zero(f.type)]));
  }
  const pda = (seed,...p) => PublicKey.findProgramAddressSync([Buffer.from(seed),...p.map(x=>x.toBuffer?x.toBuffer():x)],programId);
  const validatorArgs=[], fixtures=[];
  function dump(pk,data,owner,size=data.length) {
    const padded=Buffer.alloc(size);data.copy(padded);
    const file=`${dir}/${pk}.json`;
    fs.writeFileSync(file,JSON.stringify({pubkey:pk.toBase58(),account:{lamports:100_000_000,data:[padded.toString('base64'),'base64'],owner:owner.toBase58(),executable:false,rentEpoch:0}}));
    validatorArgs.push('--account',pk.toBase58(),file);
  }
  function mint(pk,authority,supply) {
    const data=Buffer.alloc(MintLayout.span);
    MintLayout.encode({mintAuthorityOption:1,mintAuthority:authority,supply,decimals:6,isInitialized:true,freezeAuthorityOption:0,freezeAuthority:PublicKey.default},data);
    dump(pk,data,TOKEN_PROGRAM_ID);
  }
  function token(mint,owner,amount) {
    const pk=getAssociatedTokenAddressSync(mint,owner,true),data=Buffer.alloc(AccountLayout.span);
    AccountLayout.encode({mint,owner,amount,delegateOption:0,delegate:PublicKey.default,state:1,isNativeOption:0,isNative:0n,delegatedAmount:0n,closeAuthorityOption:0,closeAuthority:PublicKey.default},data);
    dump(pk,data,TOKEN_PROGRAM_ID);return pk;
  }
  for(const count of [2,10]) {
    const [index,indexBump]=pda('index',payer,Buffer.from(`LEGACY${count}`));
    const [indexMint,indexMintBump]=pda('index-mint',index);
    const [vaultAuthority,vaultAuthorityBump]=pda('vault-authority',index);
    const [page,pageBump]=pda('large-basket-component-page',index,Buffer.from([0]));
    mint(indexMint,vaultAuthority,100_000_000n);
    const ownerIndex=token(indexMint,payer,100_000_000n);
    const vaultQuote=token(usdc,vaultAuthority,100_000_000n);
    const components=Array.from({length:count},()=>{
      const m=Keypair.generate().publicKey;mint(m,payer,2_000_000_000n);
      const vault=token(m,vaultAuthority,900_000_000n/BigInt(count));token(m,payer,1_000_000_000n);
      return {mint:m,vault,tokenProgram:TOKEN_PROGRAM_ID,unitsPerIndex:bn(9_000_000/count),accountedReserve:bn(900_000_000/count),targetWeightBps:10000/count,oraclePair:Keypair.generate().publicKey,decimals:6};
    });
    const state=zero({defined:{name:'indexState'}});
    Object.assign(state,{authority:payer,creator:payer,feeRecipient:payer,indexMint,vaultAuthorityBump,indexBump,indexMintBump,decimals:6,kind:{fixedWeights:{}},largeBasketComponentCount:count,largeBasketPageCount:1,largeBasketConfigured:true,fixedWeightQuoteMint:usdc,fixedWeightRebalanceIntervalSeconds:bn(86400),fixedWeightDriftThresholdBps:500,name:'Legacy',symbol:`LEGACY${count}`});
    dump(index,encode('indexState',state),programId,4096);
    dump(page,encode('largeBasketComponentPage',{index,pageIndex:0,startComponentIndex:0,componentCount:count,bump:pageBump,finalized:true,reserved:Array(32).fill(0),components}),programId,1553);
    fixtures.push({index,indexMint,vaultAuthority,page,vaultQuote,ownerIndex,count,components});
  }
  return {validatorArgs,fixtures};
}
export async function testRebalanceMigration(program,connection,payer,stakingPool,fixtures) {
  let nonce=5000;
  const pda=(seed,...p)=>PublicKey.findProgramAddressSync([Buffer.from(seed),...p.map(x=>x.toBuffer?x.toBuffer():x)],program.programId)[0];
  for(const f of fixtures) {
    const quotePage=pda('large-basket-component-page',f.index,Buffer.from([Math.floor(f.count/10)]));
    const migration=()=>program.methods.registerRebalanceQuote().accounts({payer:payer.publicKey,index:f.index,indexMint:f.indexMint,vaultAuthority:f.vaultAuthority,quoteMint:usdc,vaultQuote:f.vaultQuote,quotePage,associatedTokenProgram:ASSOCIATED_TOKEN_PROGRAM_ID,tokenProgram:TOKEN_PROGRAM_ID,systemProgram:SystemProgram.programId}).remainingAccounts([meta(f.page)]);
    const common={owner:payer.publicKey,index:f.index,indexMint:f.indexMint,stakingPool,quoteMint:usdc,vaultAuthority:f.vaultAuthority,intentLock:pda('large-basket-intent-lock',f.index,payer.publicKey),ownerIndexTokenAccount:f.ownerIndex,tokenProgram:TOKEN_PROGRAM_ID,systemProgram:SystemProgram.programId};
    const makeIntent=()=>pda('large-basket-intent',f.index,payer.publicKey,bn(++nonce).toArrayLike(Buffer,'le',8));
    const args=(amount)=>({nonce:bn(nonce),expiresAt:bn(Math.floor(Date.now()/1000)+600),inKind:true,indexAmountOut:bn(amount),maxQuoteIn:bn(0)});
    let intent=makeIntent();
    await assert.rejects(program.methods.openLargeBasketMintIntent(args(100_000_000)).accounts({...common,intent}).remainingAccounts([meta(f.page)]).rpc(),/InvalidFixedWeightConfig|6019|custom program error/);
    await migration().rpc();
    const pages=f.count===10?[f.page,quotePage]:[f.page];
    const page=await program.account.largeBasketComponentPage.fetch(quotePage);
    const cash=page.components.at(-1);
    assert.ok(cash.mint.equals(usdc));assert.equal(cash.targetWeightBps,0);assert.equal(cash.accountedReserve.toString(),'100000000');
    assert.equal((await program.account.indexState.fetch(f.index)).largeBasketComponentCount,f.count+1);
    await assert.rejects(migration().rpc());
    // Half redemption reserves exactly half the parked cash. Cancellation restores it.
    intent=makeIntent();
    await program.methods.openLargeBasketRedeemIntent({nonce:bn(nonce),expiresAt:bn(Math.floor(Date.now()/1000)+600),inKind:true,indexAmountIn:bn(50_000_000),minQuoteOut:bn(0)}).accounts({...common,intent}).remainingAccounts(pages.map(p=>meta(p,true))).rpc();
    assert.equal((await program.account.largeBasketIntent.fetch(intent)).componentAmounts.at(-1).toString(),'50000000');
    await program.methods.cancelUnfilledLargeBasketRedeemIntent().accounts({...common,intent}).remainingAccounts(pages.map(p=>meta(p,true))).rpc();
    assert.equal((await program.account.largeBasketComponentPage.fetch(quotePage)).components.at(-1).accountedReserve.toString(),'100000000');
    if(f.count!==2) continue;
    const components=[...f.components,cash];
    const userCash=await getOrCreateAssociatedTokenAccount(connection,payer,usdc,payer.publicKey);
    await mintTo(connection,payer,usdc,userCash.address,payer,100_000_000);
    for(const kind of ['mint','redeem']) {
      intent=makeIntent();
      const amount=kind==='mint'?100_000_000:200_000_000;
      const clock=await connection.getAccountInfo(new PublicKey('SysvarC1ock11111111111111111111111111111111'));
      const openArgs={nonce:bn(nonce),expiresAt:bn(Number(clock.data.readBigInt64LE(32))+(kind==='redeem'?5:600)),inKind:true,...(kind==='mint'?{indexAmountOut:bn(amount),maxQuoteIn:bn(0)}:{indexAmountIn:bn(amount),minQuoteOut:bn(0)})};
      await program.methods[kind==='mint'?'openLargeBasketMintIntent':'openLargeBasketRedeemIntent'](openArgs).accounts({...common,intent}).remainingAccounts(pages.map(p=>meta(p,true))).rpc();
      assert.equal((await program.account.largeBasketIntent.fetch(intent)).componentAmounts.at(-1).toString(),kind==='mint'?'100000000':'200000000');
      for(const [i,c] of (kind==='redeem'?components.slice(0,1):components).entries()) {
        const owner=getAssociatedTokenAddressSync(c.mint,payer.publicKey);
        await program.methods[kind==='mint'?'executeLargeBasketMintComponentInKind':'executeLargeBasketRedeemComponentInKind']({componentIndex:i}).accounts({...common,intent,componentPage:f.page,componentMint:c.mint,componentVault:c.vault,componentTokenProgram:TOKEN_PROGRAM_ID,ownerComponentTokenAccount:owner,protocolFeeComponentAccount:owner,creatorFeeComponentAccount:owner,associatedTokenProgram:ASSOCIATED_TOKEN_PROGRAM_ID}).rpc();
      }
      if(kind==='redeem') {
        // A partial expired redeem must return unfilled components, including cash.
        await new Promise(resolve=>setTimeout(resolve,6000));
        const before=(await getAccount(connection,userCash.address)).amount;
        await program.methods.cancelExpiredLargeBasketIntent().accounts({...common,intent}).remainingAccounts([
          ...pages.map(p=>meta(p,true)),
          ...components.slice(1).flatMap(c=>[meta(c.mint),meta(c.vault,true),meta(getAssociatedTokenAddressSync(c.mint,payer.publicKey),true),meta(TOKEN_PROGRAM_ID)]),
        ]).rpc();
        assert.equal((await getAccount(connection,userCash.address)).amount-before,200_000_000n);
      } else {
        await program.methods.finalizeLargeBasketMintIntent().accounts({...common,intent,associatedTokenProgram:ASSOCIATED_TOKEN_PROGRAM_ID}).remainingAccounts(pages.map(p=>meta(p,true))).rpc();
      }
      assert.equal((await getAccount(connection,f.vaultQuote)).amount,kind==='mint'?200_000_000n:0n);
    }
    assert.equal((await getMint(connection,f.indexMint)).supply,0n);
  }
  console.log('integration ok: legacy USDC migration, new page boundary, duplicate rejection, pro-rata cash mint/redeem and cancellation');
}
