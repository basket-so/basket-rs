// Read-only snapshot of the mainnet state the wrapped-ORE fork test loads into a local
// validator: ORE's staking programs (ore-lst, ore-stake) and the accounts their wrap and
// unwrap touch, and every account of the SOV basket and the protocol it uses. Nothing is sent.
//
//   node --env-file=.env.rpc tests/wrapped-ore-snapshot.mjs [--basket <index>]
//
// Writes .anchor/wrapped-ore-snapshot/: one validator account dump per account, the two
// program binaries, and manifest.json describing what each account is.
import { createHash } from "node:crypto";
import fs from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";

import anchor from "@coral-xyz/anchor";
import { getAssociatedTokenAddressSync, TOKEN_PROGRAM_ID } from "@solana/spl-token";
import { Connection, PublicKey } from "@solana/web3.js";

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");
const out = path.join(root, ".anchor", "wrapped-ore-snapshot");
const argv = process.argv.slice(2);
const flag = (name, fallback) => (argv.includes(name) ? argv[argv.indexOf(name) + 1] : fallback);

const PROGRAM_ID = new PublicKey("bskthjNMRWQ4ekDLxaAzA1e39ThPmEtUgHY3XHfs7qv");
const ORE_LST = new PublicKey("storeD7bEkywTTMrje19WRoyrkEhbhrvyjVnLxWih6a");
const ORE_STAKE = new PublicKey("stakecNP3FpiExZPCgZfqRgumVzi6dNqnfrjwXyTgeH");
const ORE_MINT = new PublicKey("oreoU2P8bN6jkk3jbaiVxYnG1dCXcYxwhwyK9jSybcp");
const STORE_MINT = new PublicKey("storenSbvkfzircixnaosc5CbzNZVrHJ6S3EKrS1yqR");
const USDC = new PublicKey("EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v");
const BASKET_MINT = new PublicKey("2rNBaMg5VAr1aMNCwAPdDZVgzzdTaNDebUnNqPFNmeta");
const SOV = new PublicKey(flag("--basket", "5odgX8DVHcfEybrUCRVMMFXyjjidZwY7zSKWLkoXHRy6"));
const LOADER = new PublicKey("BPFLoaderUpgradeab1e11111111111111111111111");
const WRAP = 3;
const UNWRAP = 2;
// Positions in ore-lst's wrap/unwrap account list.
const ORE_LST_ACCOUNTS = ["signer", "payer", "senderOre", "senderStore", "oreMint", "storeMint", "stake", "stakeTokens", "treasury", "treasuryTokens", "vault", "vaultTokens", "vesting", "system", "token", "associatedToken", "oreStake"];

if (!process.env.SOLANA_RPC_URL) throw new Error("SOLANA_RPC_URL is not set (node --env-file=.env.rpc ...)");
const connection = new Connection(process.env.SOLANA_RPC_URL, "confirmed");
const pda = (...seeds) => PublicKey.findProgramAddressSync(seeds.map((s) => (typeof s === "string" ? Buffer.from(s) : s.toBuffer ? s.toBuffer() : s)), PROGRAM_ID)[0];

fs.rmSync(out, { recursive: true, force: true });
fs.mkdirSync(out, { recursive: true });
const manifest = { takenAt: new Date().toISOString(), slot: await connection.getSlot("confirmed"), accounts: {}, programs: {} };

async function dump(label, pubkey) {
  const info = await connection.getAccountInfo(pubkey, "confirmed");
  if (!info) {
    manifest.accounts[pubkey.toBase58()] = { label, missing: true };
    return null;
  }
  fs.writeFileSync(
    path.join(out, `${pubkey.toBase58()}.json`),
    JSON.stringify({ pubkey: pubkey.toBase58(), account: { lamports: info.lamports, data: [info.data.toString("base64"), "base64"], owner: info.owner.toBase58(), executable: info.executable, rentEpoch: 0, space: info.data.length } }),
  );
  manifest.accounts[pubkey.toBase58()] = { label, owner: info.owner.toBase58(), size: info.data.length };
  return info;
}

// An upgradeable program's binary, without the 45-byte ProgramData header.
async function program(label, id) {
  const account = await connection.getAccountInfo(id, "confirmed");
  const programData = new PublicKey(account.data.subarray(4, 36));
  const data = (await connection.getAccountInfo(programData, "confirmed")).data;
  const lastDeploySlot = Number(data.readBigUInt64LE(4));
  const authority = data[12] === 1 ? new PublicKey(data.subarray(13, 45)).toBase58() : null;
  const file = `${label}.so`;
  fs.writeFileSync(path.join(out, file), data.subarray(45));
  manifest.programs[id.toBase58()] = { label, file, programData: programData.toBase58(), programDataSize: data.length, lastDeploySlot, upgradeAuthority: authority };
}

// The 17 accounts of a recent mainnet wrap and unwrap, and which programs they called.
async function recentOreLstCalls() {
  const found = {};
  let before;
  for (let page = 0; page < 5 && (!found.wrap || !found.unwrap); page += 1) {
    const sigs = await connection.getSignaturesForAddress(ORE_LST, { limit: 50, before });
    if (!sigs.length) break;
    before = sigs.at(-1).signature;
    for (const { signature, err } of sigs) {
      if (err || (found.wrap && found.unwrap)) continue;
      const tx = await connection.getTransaction(signature, { maxSupportedTransactionVersion: 0, commitment: "confirmed" });
      if (!tx) continue;
      const keys = tx.transaction.message.getAccountKeys({ accountKeysFromLookups: tx.meta.loadedAddresses });
      tx.transaction.message.compiledInstructions.forEach((ix, position) => {
        if (!keys.get(ix.programIdIndex).equals(ORE_LST)) return;
        const kind = ix.data[0] === WRAP ? "wrap" : ix.data[0] === UNWRAP ? "unwrap" : null;
        if (!kind || found[kind] || ix.accountKeyIndexes.length !== 17) return;
        const inner = (tx.meta.innerInstructions ?? []).find((i) => i.index === position);
        found[kind] = {
          signature,
          slot: tx.slot,
          dataLength: ix.data.length,
          accounts: Object.fromEntries(ix.accountKeyIndexes.map((k, i) => [ORE_LST_ACCOUNTS[i], keys.get(k).toBase58()])),
          innerPrograms: [...new Set((inner?.instructions ?? []).map((i) => keys.get(i.programIdIndex).toBase58()))],
        };
      });
    }
  }
  return found;
}

const calls = await recentOreLstCalls();
if (!calls.wrap || !calls.unwrap) throw new Error(`no recent ${calls.wrap ? "unwrap" : "wrap"} on ore-lst to read its accounts from`);
manifest.oreLst = calls;
for (const name of ["stake", "stakeTokens", "treasury", "treasuryTokens", "vault", "vaultTokens", "vesting"]) {
  if (calls.wrap.accounts[name] !== calls.unwrap.accounts[name]) throw new Error(`wrap and unwrap disagree on ${name}`);
  await dump(`ore-lst ${name}`, new PublicKey(calls.wrap.accounts[name]));
}
await program("ore_lst", ORE_LST);
await program("ore_stake", ORE_STAKE);
await dump("ORE mint", ORE_MINT);
await dump("stORE mint", STORE_MINT);

// The basket and the protocol accounts its mints, redeems and rebalances read.
const idl = JSON.parse(fs.readFileSync(path.join(root, "target/idl/basket.json"), "utf8"));
const { coder: { accounts: coder } } = new anchor.Program(idl, { connection });
const indexInfo = await dump("SOV index", SOV);
const index = coder.decode("indexState", indexInfo.data);
manifest.basket = { index: SOV.toBase58(), symbol: index.symbol, authority: index.authority.toBase58(), rebalanceKeeper: index.rebalanceKeeper.toBase58(), feeRecipient: index.feeRecipient.toBase58(), creatorFeeRecipient: index.creatorFeeRecipient.toBase58(), components: [] };
await dump("SOV index mint", index.indexMint);
const vaultAuthority = pda("vault-authority", SOV);
manifest.basket.vaultAuthority = vaultAuthority.toBase58();
for (let p = 0; p < index.largeBasketPageCount; p += 1) {
  const page = pda("large-basket-component-page", SOV, Buffer.from([p]));
  const decoded = coder.decode("largeBasketComponentPage", (await dump(`SOV page ${p}`, page)).data);
  for (const c of decoded.components) {
    manifest.basket.components.push({ page: page.toBase58(), mint: c.mint.toBase58(), vault: c.vault.toBase58(), tokenProgram: c.tokenProgram.toBase58(), unitsPerIndex: c.unitsPerIndex.toString(), accountedReserve: c.accountedReserve.toString(), targetWeightBps: c.targetWeightBps, decimals: c.decimals });
    await dump(`SOV component mint ${c.mint.toBase58()}`, c.mint);
    await dump(`SOV vault for ${c.mint.toBase58()}`, c.vault);
  }
}
for (const [label, key] of [
  ["protocol config", pda("protocol-config")],
  ["staking pool", pda("staking-pool")],
  ["price oracle", pda("price-oracle")],
  ["community config", pda("community-config")],
  ["SOV composition change", pda("composition-change", SOV)],
  ["SOV fee change", pda("fee-change", SOV)],
  ["BASKET mint", BASKET_MINT],
]) await dump(label, key);
const stakingAuthority = pda("staking-authority");
await dump("staking reward vault", getAssociatedTokenAddressSync(USDC, stakingAuthority, true, TOKEN_PROGRAM_ID));
await dump("staking stake vault", getAssociatedTokenAddressSync(BASKET_MINT, stakingAuthority, true, TOKEN_PROGRAM_ID));
for (const [label, owner] of [["fee recipient USDC", index.feeRecipient], ["creator fee recipient USDC", index.creatorFeeRecipient]]) {
  if (!owner.equals(PublicKey.default)) await dump(label, getAssociatedTokenAddressSync(USDC, owner, true, TOKEN_PROGRAM_ID));
}
// The basket program as deployed, to start the fork from and to size the upgrade against.
// ProgramData pads the ELF with zeros; the ELF ends with its section header table.
const basketProgram = await connection.getAccountInfo(PROGRAM_ID, "confirmed");
const basketProgramData = new PublicKey(basketProgram.data.subarray(4, 36));
const basketData = await connection.getAccountInfo(basketProgramData, "confirmed");
const elf = basketData.data.subarray(45);
const elfLength = Number(elf.readBigUInt64LE(0x28)) + elf.readUInt16LE(0x3c) * elf.readUInt16LE(0x3a);
fs.writeFileSync(path.join(out, "basket.so"), elf.subarray(0, elfLength));
manifest.basketProgram = {
  file: "basket.so",
  sha256: createHash("sha256").update(elf.subarray(0, elfLength)).digest("hex"),
  programData: basketProgramData.toBase58(),
  programDataSize: basketData.data.length,
  maxProgramBytes: basketData.data.length - 45,
  elfLength,
  upgradeAuthority: basketData.data[12] === 1 ? new PublicKey(basketData.data.subarray(13, 45)).toBase58() : null,
  lastDeploySlot: Number(basketData.data.readBigUInt64LE(4)),
};

fs.writeFileSync(path.join(out, "manifest.json"), JSON.stringify(manifest, null, 2));
const missing = Object.values(manifest.accounts).filter((a) => a.missing).map((a) => a.label);
console.log(`snapshot at slot ${manifest.slot}: ${Object.keys(manifest.accounts).length - missing.length} accounts, 2 programs${missing.length ? `; absent on mainnet: ${missing.join(", ")}` : ""}`);
