import fs from 'node:fs';
import path from 'node:path';
import { PublicKey } from '@solana/web3.js';

const read = p => JSON.parse(fs.readFileSync(p,'utf8'));
const old = read('docs/catalog/deployment.json');
const next = read('docs/program-replacement/deployment.json');
const deployed = read('docs/program-replacement/program.json');
const ui = path.resolve('../basket-ui');
if (Object.keys(next.baskets).length !== 12 || Object.values(next.baskets).some(b=>b.status!=='verified')) throw new Error('All twelve baskets must be verified');
const program = new PublicKey(deployed.newProgram);
const pda = (...seeds) => PublicKey.findProgramAddressSync(seeds.map(s=>typeof s==='string'?Buffer.from(s):s.toBuffer()),program)[0].toBase58();
const oldState=read(path.join(ui,'public/mainnet-state.json'));
const replacements = new Map([[deployed.oldProgram,deployed.newProgram],['5yTFbtAE5RDjxpiVpDfyWuzcCWgwh659CEu7a7ZQtSpk',deployed.stakingMint]]);
for (const [symbol,b] of Object.entries(next.baskets)) {
  replacements.set(old.baskets[symbol].index,b.index);
  replacements.set(old.baskets[symbol].indexMint,b.indexMint);
  replacements.set(old.baskets[symbol].lookupTable,b.lookupTable);
}
for (const [key,value] of Object.entries(deployed.accounts)) if(oldState.staking?.[key]) replacements.set(oldState.staking[key],value);
function replaceFile(file) {
  let text=fs.readFileSync(file,'utf8'),before=text;
  for(const [a,b] of replacements)text=text.split(a).join(b);
  if(text!==before)fs.writeFileSync(file,text);
}
function walk(dir) {
  for (const item of fs.readdirSync(dir,{withFileTypes:true})) {
    const full=path.join(dir,item.name);
    if(item.isDirectory())walk(full);
    else if(/\.(tsx?|m?js|json)$/.test(item.name))replaceFile(full);
  }
}
for(const dir of ['app','lib','tests'])walk(path.join(ui,dir));
for(const file of ['mainnet-config.json','mainnet-state.json'])replaceFile(path.join(ui,'public',file));
const state=read(path.join(ui,'public/mainnet-state.json'));
state.generatedAt=new Date().toISOString();
state.protocolConfig=deployed.accounts.protocolConfig;
state.tokens.BASKET.decimals=6;
state.staking={...state.staking,...Object.fromEntries(Object.entries(deployed.accounts).filter(([key])=>['stakingPool','stakingAuthority','stakeVault','rewardVault'].includes(key))),basketMint:deployed.stakingMint};
state.index={...state.index,index:next.baskets.SOLB.index,indexMint:next.baskets.SOLB.indexMint,vaultAuthority:pda('vault-authority',new PublicKey(next.baskets.SOLB.index)),lookupTables:[next.baskets.SOLB.lookupTable]};
state.indexLookupTables=Object.fromEntries(Object.values(next.baskets).map(b=>[b.index,[b.lookupTable]]));
fs.writeFileSync(path.join(ui,'public/mainnet-state.json'),JSON.stringify(state,null,2)+'\n');
const config=read(path.join(ui,'public/mainnet-config.json'));config.generatedAt=state.generatedAt;
fs.writeFileSync(path.join(ui,'public/mainnet-config.json'),JSON.stringify(config,null,2)+'\n');
const protocolRoute=path.join(ui,'app/api/protocol/route.ts');
fs.writeFileSync(protocolRoute,fs.readFileSync(protocolRoute,'utf8').replace('tokenForMintEntries(tokenEntries, fallbackBasketMint)?.decimals ?? 9','tokenForMintEntries(tokenEntries, fallbackBasketMint)?.decimals ?? 6'));
const routing=path.join(ui,'lib/index-routing.ts');
let routingText=fs.readFileSync(routing,'utf8');
routingText=routingText.replace(/export const DEFAULT_OMNINDEX_LOOKUP_TABLES = \[[\s\S]*?\] as const;/,'export const DEFAULT_OMNINDEX_LOOKUP_TABLES: readonly string[] = [];');
fs.writeFileSync(routing,routingText);
for(const d of ['src/generated/idl/omnindex.json','public/generated/idl/omnindex.json'])fs.copyFileSync('target/idl/basket.json',path.join(ui,d));
// The landing page's five featured baskets retain their selection and descriptions.
replaceFile('../omnindex-landing/lib/featured-baskets.mjs');
console.log('Updated app addresses, six-decimal staking, catalog descriptions, per-index lookup tables, and landing mint selection.');
