// Rehearses the wrapped-ORE upgrade on a local copy of mainnet, from a read-only snapshot
// (tests/wrapped-ore-snapshot.mjs): the SOV basket and the protocol accounts as they are,
// the basket program as deployed, and ORE's own staking programs (ore-lst, ore-stake) with
// their state. Jupiter is replaced by tests/mock-jupiter, which fills swaps at live prices
// from reserve accounts, behind a local copy of Jupiter's quote, swap and price APIs.
//
// Phase 1, on the deployed binary: swap and in-kind mints and redeems, then the upgrade (extend
// and deploy, as on mainnet), a check that it changed no account, the same flows again, and a
// live proposal moving SOV's ORE weight to stORE.
// Phase 2: the 3-day notice can't be waited out locally, so every account from phase 1 is moved
// into a new validator with only the proposal's two timestamps set 3 days back. The keeper
// (scripts/rebalance-bot.mjs, unchanged) applies the change and rebalances; then mints and
// redeems run through the new wrapped legs, in kind and through stORE pools, and every check a
// wrapped leg makes is attacked.
// Phase 3: the accounts move again, with SOV's XAUt0 vault frozen (as its issuer could) and its
// last rebalance a month back. A stranger creates a community basket and mints and redeems it
// through the swap path; then cbBTC's price moves 30% and the keeper rebalances both baskets,
// SOV around the frozen vault; then an SOV redeem skips XAUt0.
//
//   node tests/mock-jupiter build: cargo-build-sbf in tests/mock-jupiter (Agave 2.1, as anchor build)
//   BASKET_WSL=1 node tests/wrapped-ore-fork.mjs
import assert from "node:assert/strict";
import { spawn, spawnSync } from "node:child_process";
import { createHash } from "node:crypto";
import fs from "node:fs";
import http from "node:http";
import net from "node:net";
import path from "node:path";
import { fileURLToPath } from "node:url";

import anchor from "@coral-xyz/anchor";
import {
  AccountLayout,
  ASSOCIATED_TOKEN_PROGRAM_ID,
  createAssociatedTokenAccountIdempotentInstruction,
  getAssociatedTokenAddressSync,
  TOKEN_PROGRAM_ID,
} from "@solana/spl-token";
import {
  AddressLookupTableProgram,
  ComputeBudgetProgram,
  Connection,
  Keypair,
  LAMPORTS_PER_SOL,
  PublicKey,
  SystemProgram,
  TransactionMessage,
  VersionedTransaction,
} from "@solana/web3.js";

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");
const snapshotDir = path.join(root, ".anchor", "wrapped-ore-snapshot");
const manifest = JSON.parse(fs.readFileSync(path.join(snapshotDir, "manifest.json"), "utf8"));
const idl = JSON.parse(fs.readFileSync(path.join(root, "target/idl/basket.json"), "utf8"));
const NEW_BINARY = path.join(root, "target/deploy/basket.so");
const MOCK_JUPITER = path.join(root, "tests/mock-jupiter/target/deploy/mock_jupiter.so");
const CLI_BIN = path.join(root, ".agave-2.3/releases/2.3.0/solana-release/bin");
const exe = (name) => (process.platform === "win32" ? `${name}.exe` : name);
const WSL = Boolean(process.env.BASKET_WSL);
const WSL_VALIDATOR = "/home/gainsu/.cache/basket-validator/solana-release/bin/solana-test-validator";
const report = { startedAt: new Date().toISOString(), snapshotSlot: manifest.slot, steps: [], states: {}, measurements: {} };

const PROGRAM_ID = new PublicKey("bskthjNMRWQ4ekDLxaAzA1e39ThPmEtUgHY3XHfs7qv");
const JUP6 = new PublicKey("JUP6LkbZbjS1jKKwapdHNy74zcZ3tLUZoi5QNyVTaV4");
const ORE_LST = new PublicKey("storeD7bEkywTTMrje19WRoyrkEhbhrvyjVnLxWih6a");
const ORE_STAKE = new PublicKey("stakecNP3FpiExZPCgZfqRgumVzi6dNqnfrjwXyTgeH");
const ORE_MINT = new PublicKey("oreoU2P8bN6jkk3jbaiVxYnG1dCXcYxwhwyK9jSybcp");
const STORE_MINT = new PublicKey("storenSbvkfzircixnaosc5CbzNZVrHJ6S3EKrS1yqR");
const USDC = new PublicKey("EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v");
const SOV = new PublicKey(manifest.basket.index);
const LOADER = new PublicKey("BPFLoaderUpgradeab1e11111111111111111111111");
const ACCOUNT_LOCK_LIMIT_128_FEATURE = "9LZdXeKGeBV6hRLdxS1rHbHoEUsKqesCC2ZAPTPKJAbK";
const RESERVE = PublicKey.findProgramAddressSync([Buffer.from("reserve")], JUP6)[0];
const SYMBOLS = {
  [ORE_MINT.toBase58()]: "ORE",
  [STORE_MINT.toBase58()]: "stORE",
  [USDC.toBase58()]: "USDC",
  A7bdiYdS5GjqGFtxf17ppRHtDKPkkRqbKtR27dxvQXaS: "ZEC",
  AymATz4TCL9sWNEEV9Kvyz45CHVhDZ6kUgjTJPzLpU9P: "XAUt0",
  cbbtcf3aa214zXHbiAZQwf4122FBYbraNdFqgw4iMij: "cbBTC",
};
const DECIMALS = { [ORE_MINT.toBase58()]: 11, [STORE_MINT.toBase58()]: 11, [USDC.toBase58()]: 6 };
for (const c of manifest.basket.components) DECIMALS[c.mint] = c.decimals;
// What ORE's change proposes in this rehearsal: ORE's half moves to stORE, the rest stays.
const NEW_WEIGHTS = { ORE: 0, ZEC: 1500, XAUt0: 1500, cbBTC: 2000, USDC: 0 };
const STORE_WEIGHT = 5000;
// Mainnet's ProgramData holds 1,811,520 program bytes; the new binary needs more. The upgrade
// extends it by this much first (ExtendProgram takes at least 10,240 bytes).
// Sized for the stORE legs plus skipping stuck components (1,946,928 bytes), with room to spare.
const MAINNET_EXTEND_BYTES = 153_600;
const SWAP_FEE = 0.002;

const payer = Keypair.fromSecretKey(Uint8Array.from(JSON.parse(fs.readFileSync(path.join(root, "deployer-keypair.json"), "utf8"))));
const user = Keypair.generate();
const thief = Keypair.generate();
const oracle = Keypair.generate();
const bn = (n) => new anchor.BN(n.toString());
const meta = (pubkey, isWritable = false, isSigner = false) => ({ pubkey, isWritable, isSigner });
const pda = (...seeds) => PublicKey.findProgramAddressSync(seeds.map((s) => (typeof s === "string" ? Buffer.from(s) : s.toBuffer ? s.toBuffer() : s)), PROGRAM_ID)[0];
const ata = (mint, owner) => getAssociatedTokenAddressSync(mint, owner, true, TOKEN_PROGRAM_ID);
const u64 = (n) => { const b = Buffer.alloc(8); b.writeBigUInt64LE(BigInt(n)); return b; };
const symbol = (mint) => SYMBOLS[mint.toBase58?.() ?? mint] ?? String(mint).slice(0, 6);
const CBBTC = new PublicKey("cbbtcf3aa214zXHbiAZQwf4122FBYbraNdFqgw4iMij");
const communityConfig = PublicKey.findProgramAddressSync([Buffer.from("community-config")], PROGRAM_ID)[0];
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const vaultAuthority = new PublicKey(manifest.basket.vaultAuthority);
const feeRecipient = new PublicKey(manifest.basket.feeRecipient);
const stakingPool = pda("staking-pool");
const stakingAuthority = pda("staking-authority");
const protocolConfig = pda("protocol-config");
const priceOracle = pda("price-oracle");
const compositionChange = pda("composition-change", SOV);
const intentLock = pda("large-basket-intent-lock", SOV, user.publicKey);
const oreLstAccounts = (() => {
  const a = manifest.oreLst.wrap.accounts;
  return {
    oreLstVault: new PublicKey(a.vault), oreLstVaultTokens: new PublicKey(a.vaultTokens), oreStake: new PublicKey(a.stake),
    oreStakeTokens: new PublicKey(a.stakeTokens), oreStakeTreasury: new PublicKey(a.treasury), oreStakeTreasuryTokens: new PublicKey(a.treasuryTokens),
    oreStakeVesting: new PublicKey(a.vesting), oreLstProgram: ORE_LST, oreStakeProgram: ORE_STAKE,
  };
})();
function step(name, detail = {}) {
  report.steps.push({ name, ...detail });
  console.log(`  ok  ${name}${Object.keys(detail).length ? ` ${JSON.stringify(detail, (_, v) => (typeof v === "bigint" ? v.toString() : v))}` : ""}`);
}

// --- prices ------------------------------------------------------------------------------

// The mock fills at live prices: DexScreener's most liquid pair, which the keeper's oracle also
// checks against, else Jupiter's public price API. USD per whole token.
async function livePrices(mints) {
  const prices = new Map([[USDC.toBase58(), 1]]);
  const wanted = mints.map(String).filter((m) => m !== USDC.toBase58());
  try {
    const pairs = await (await fetch(`https://api.dexscreener.com/tokens/v1/solana/${wanted.join(",")}`, { signal: AbortSignal.timeout(15_000) })).json();
    const best = new Map();
    for (const pair of Array.isArray(pairs) ? pairs : []) {
      const mint = pair?.baseToken?.address;
      const usd = Number(pair?.priceUsd);
      const liquidity = Number(pair?.liquidity?.usd ?? 0);
      if (wanted.includes(mint) && usd > 0 && (!best.has(mint) || liquidity > best.get(mint).liquidity)) best.set(mint, { usd, liquidity });
    }
    for (const [mint, { usd }] of best) prices.set(mint, usd);
  } catch (error) {
    console.warn(`  DexScreener unavailable (${error.message}); using Jupiter's price API`);
  }
  const missing = wanted.filter((m) => !prices.has(m));
  if (missing.length) {
    const payload = await (await fetch(`https://lite-api.jup.ag/price/v3?ids=${missing.join(",")}`, { signal: AbortSignal.timeout(15_000) })).json();
    for (const m of missing) if (Number(payload?.[m]?.usdPrice) > 0) prices.set(m, Number(payload[m].usdPrice));
  }
  const still = wanted.filter((m) => !prices.has(m));
  if (still.length) throw new Error(`no live price for ${still.map(symbol).join(", ")}`);
  return prices;
}
let PRICES;
const atomUsd = (mint) => PRICES.get(mint.toBase58()) / 10 ** DECIMALS[mint.toBase58()];
const exactIn = (inMint, outMint, amount) => BigInt(Math.floor((Number(amount) * atomUsd(inMint)) / atomUsd(outMint) * (1 - SWAP_FEE)));
const exactOut = (inMint, outMint, amount) => BigInt(Math.ceil((Number(amount) * atomUsd(outMint)) / atomUsd(inMint) / (1 - SWAP_FEE)));

// --- mock Jupiter --------------------------------------------------------------------------

// tests/mock-jupiter's swap: it takes inAmount from source and pays outAmount to destination.
function mockSwap({ authority, source, destination, inputMint, outputMint, inAmount, outAmount }) {
  return {
    programId: JUP6,
    keys: [
      meta(authority, false, true), meta(source, true), meta(destination, true),
      meta(ata(inputMint, RESERVE), true), meta(ata(outputMint, RESERVE), true), meta(RESERVE),
      meta(inputMint), meta(outputMint), meta(TOKEN_PROGRAM_ID), meta(TOKEN_PROGRAM_ID),
    ],
    data: Buffer.concat([Buffer.from([0]), u64(inAmount), u64(outAmount)]),
  };
}

// A swap as the program's compact plan: each account is an index into `candidates`, followed by
// the accounts the route adds (passed as remaining accounts), one byte each.
function compact(swap, candidates) {
  const position = new Map();
  candidates.forEach((key, i) => { if (!position.has(key.toBase58())) position.set(key.toBase58(), i); });
  const route = [];
  const bytes = swap.keys.map((k) => {
    let i = position.get(k.pubkey.toBase58());
    if (i === undefined) {
      i = candidates.length + route.length;
      route.push(meta(k.pubkey, k.isWritable));
      position.set(k.pubkey.toBase58(), i);
    }
    assert.ok(i < 64, "route too wide to pack");
    return (i & 0x3f) | (k.isSigner ? 0x40 : 0) | (k.isWritable ? 0x80 : 0);
  });
  return { plan: { instructionData: Buffer.from(swap.data), accounts: Buffer.from(bytes) }, route };
}

// Jupiter's quote, swap-instructions and price APIs over the mock, for the keeper.
function startMockJupiterApi(connection) {
  const calls = { quote: 0, swap: 0, price: 0 };
  const server = http.createServer(async (req, res) => {
    const url = new URL(req.url, "http://localhost");
    const reply = (status, body) => { res.writeHead(status, { "content-type": "application/json" }); res.end(JSON.stringify(body)); };
    try {
      if (url.pathname.endsWith("/quote")) {
        calls.quote += 1;
        const inputMint = new PublicKey(url.searchParams.get("inputMint"));
        const outputMint = new PublicKey(url.searchParams.get("outputMint"));
        const amount = BigInt(url.searchParams.get("amount"));
        const swapMode = url.searchParams.get("swapMode") ?? "ExactIn";
        const slippageBps = Number(url.searchParams.get("slippageBps") ?? 50);
        if (!PRICES.has(inputMint.toBase58()) || !PRICES.has(outputMint.toBase58())) return reply(400, { error: "COULD_NOT_FIND_ANY_ROUTE" });
        const inAmount = swapMode === "ExactOut" ? exactOut(inputMint, outputMint, amount) : amount;
        const outAmount = swapMode === "ExactOut" ? amount : exactIn(inputMint, outputMint, amount);
        const threshold = swapMode === "ExactOut" ? (inAmount * BigInt(10_000 + slippageBps)) / 10_000n : (outAmount * BigInt(10_000 - slippageBps)) / 10_000n;
        const slot = await connection.getSlot("confirmed");
        return reply(200, {
          inputMint: inputMint.toBase58(), inAmount: inAmount.toString(), outputMint: outputMint.toBase58(), outAmount: outAmount.toString(),
          otherAmountThreshold: threshold.toString(), swapMode, slippageBps, priceImpactPct: "0", contextSlot: slot, timeTaken: 0,
          routePlan: [{ percent: 100, swapInfo: { ammKey: RESERVE.toBase58(), label: "MockReserve", inputMint: inputMint.toBase58(), outputMint: outputMint.toBase58(), inAmount: inAmount.toString(), outAmount: outAmount.toString(), feeAmount: "0", feeMint: inputMint.toBase58() } }],
        });
      }
      if (url.pathname.endsWith("/swap-instructions")) {
        calls.swap += 1;
        const body = JSON.parse(await new Promise((r) => { let s = ""; req.on("data", (d) => (s += d)); req.on("end", () => r(s)); }));
        const q = body.quoteResponse;
        const taker = new PublicKey(body.userPublicKey);
        const inputMint = new PublicKey(q.inputMint);
        const outputMint = new PublicKey(q.outputMint);
        const swap = mockSwap({ authority: taker, source: ata(inputMint, taker), destination: ata(outputMint, taker), inputMint, outputMint, inAmount: q.inAmount, outAmount: q.outAmount });
        return reply(200, {
          computeBudgetInstructions: [], setupInstructions: [], cleanupInstruction: null, otherInstructions: [], addressLookupTableAddresses: [],
          swapInstruction: { programId: JUP6.toBase58(), accounts: swap.keys.map((k) => ({ pubkey: k.pubkey.toBase58(), isSigner: k.isSigner, isWritable: k.isWritable })), data: swap.data.toString("base64") },
        });
      }
      if (url.pathname.endsWith("/price/v3")) {
        calls.price += 1;
        const slot = await connection.getSlot("confirmed");
        const ids = (url.searchParams.get("ids") ?? "").split(",").filter(Boolean);
        return reply(200, Object.fromEntries(ids.filter((m) => PRICES.has(m)).map((m) => [m, { usdPrice: PRICES.get(m), blockId: slot, decimals: DECIMALS[m] }])));
      }
      reply(404, { error: "not found" });
    } catch (error) {
      reply(500, { error: error.message });
    }
  });
  return new Promise((resolve) => server.listen(0, "127.0.0.1", () => resolve({ server, calls, base: `http://127.0.0.1:${server.address().port}` })));
}

// --- validator ----------------------------------------------------------------------------

function tokenAccount(mint, owner, amount) {
  const data = Buffer.alloc(AccountLayout.span);
  AccountLayout.encode({ mint, owner, amount: BigInt(amount), delegateOption: 0, delegate: PublicKey.default, state: 1, isNativeOption: 0, isNative: 0n, delegatedAmount: 0n, closeAuthorityOption: 0, closeAuthority: PublicKey.default }, data);
  return { lamports: 2_039_280, data, owner: TOKEN_PROGRAM_ID };
}
// Mainnet's rent is now 5,080 lamports per byte at a threshold of 1; the local validator still
// charges 6,960, so accounts mainnet created recently look under-funded here. Only their
// lamports are raised to the local minimum; their data is loaded as is.
let toppedUp = 0;
function writeAccount(dir, pubkey, { lamports, data, owner, executable = false }) {
  const minimum = (128 + data.length) * 6_960;
  if (!executable && lamports < minimum) {
    lamports = minimum;
    toppedUp += 1;
  }
  fs.writeFileSync(path.join(dir, `${pubkey.toBase58()}.json`), JSON.stringify({ pubkey: pubkey.toBase58(), account: { lamports, data: [Buffer.from(data).toString("base64"), "base64"], owner: owner.toBase58(), executable, rentEpoch: 0, space: data.length } }));
}
async function freePort() {
  return new Promise((resolve, reject) => { const s = net.createServer(); s.listen(0, "127.0.0.1", () => { const { port } = s.address(); s.close(() => resolve(port)); }); s.on("error", reject); });
}
const wslPath = (p) => p.replace(/^([A-Za-z]):/, (_, d) => `/mnt/${d.toLowerCase()}`).replaceAll("\\", "/");

async function startValidator(label, programSo, accountDir) {
  let rpcPort;
  for (;;) { rpcPort = await freePort(); if (rpcPort < 60_000 && rpcPort > 20_000) break; }
  const ledger = path.join(root, ".anchor", `fork-ledger-${label}-${Date.now()}`);
  const args = [
    "--reset", "--quiet", "--mint", payer.publicKey.toBase58(),
    "--upgradeable-program", PROGRAM_ID.toBase58(), programSo, payer.publicKey.toBase58(),
    "--bpf-program", ORE_LST.toBase58(), path.join(snapshotDir, "ore_lst.so"),
    "--bpf-program", ORE_STAKE.toBase58(), path.join(snapshotDir, "ore_stake.so"),
    "--bpf-program", JUP6.toBase58(), MOCK_JUPITER,
    "--account-dir", accountDir,
    "--ledger", ledger, "--rpc-port", String(rpcPort), "--faucet-port", String(rpcPort + 2),
    "--deactivate-feature", ACCOUNT_LOCK_LIMIT_128_FEATURE,
  ];
  const child = WSL
    ? spawn("wsl.exe", ["-d", "Ubuntu", "--", WSL_VALIDATOR, ...args.map((a) => (/^[A-Za-z]:\\/.test(a) ? wslPath(a) : a))], { stdio: ["ignore", "pipe", "pipe"], windowsHide: true })
    : spawn(path.join(CLI_BIN, exe("solana-test-validator")), args, { stdio: ["ignore", "pipe", "pipe"], windowsHide: true });
  let output = "";
  child.stdout.on("data", (d) => (output += d));
  child.stderr.on("data", (d) => (output += d));
  const rpcUrl = `http://127.0.0.1:${rpcPort}`;
  const connection = new Connection(rpcUrl, { commitment: "confirmed", wsEndpoint: `ws://127.0.0.1:${rpcPort + 1}` });
  for (let i = 0; ; i += 1) {
    if (child.exitCode !== null) throw new Error(`validator ${label} exited: ${output.slice(-2000)}`);
    try { await connection.getLatestBlockhash("confirmed"); break; } catch (error) { if (i > 240) throw error; await sleep(500); }
  }
  // A fresh validator refuses transactions against its genesis slot ("no record of a prior
  // credit" for a funded payer), so wait until it is producing blocks.
  for (let i = 0; (await connection.getSlot("confirmed")) < 2; i += 1) {
    if (i > 240) throw new Error(`validator ${label} is not producing blocks`);
    await sleep(250);
  }
  const stop = async () => {
    try { connection._rpcWebSocket?.close(); } catch {}
    if (WSL) spawnSync("wsl.exe", ["-d", "Ubuntu", "--", "pkill", "-f", `rpc-port ${rpcPort}`], { stdio: "ignore" });
    if (child.exitCode === null) {
      if (process.platform === "win32") spawnSync("taskkill", ["/pid", String(child.pid), "/T", "/F"], { stdio: "ignore" });
      else child.kill("SIGKILL");
    }
    await sleep(1500);
    fs.rmSync(ledger, { recursive: true, force: true, maxRetries: 5 });
  };
  return { connection, rpcUrl, stop, output: () => output };
}

// --- transactions -------------------------------------------------------------------------

function makeFork(connection, rpcUrl) {
  const provider = new anchor.AnchorProvider(connection, new anchor.Wallet(payer), { commitment: "confirmed" });
  const program = new anchor.Program(idl, provider);
  const F = { connection, rpcUrl, provider, program, tables: [] };
  // A v0 transaction with the fork's lookup tables. Returns its size, accounts and compute.
  F.send = async (ixs, signers = [payer], { cu = 1_400_000, tables = F.tables } = {}) => {
    const latest = await connection.getLatestBlockhash("confirmed");
    const message = new TransactionMessage({ payerKey: signers[0].publicKey, recentBlockhash: latest.blockhash, instructions: [ComputeBudgetProgram.setComputeUnitLimit({ units: cu }), ...ixs] }).compileToV0Message(tables);
    const tx = new VersionedTransaction(message);
    tx.sign(signers);
    const bytes = tx.serialize().length;
    const accounts = message.staticAccountKeys.length + message.addressTableLookups.reduce((n, l) => n + l.writableIndexes.length + l.readonlyIndexes.length, 0);
    assert.ok(bytes <= 1232, `transaction is ${bytes} bytes`);
    assert.ok(accounts <= 64, `transaction locks ${accounts} accounts`);
    const signature = await connection.sendRawTransaction(tx.serialize(), { preflightCommitment: "confirmed" });
    const confirmation = await connection.confirmTransaction({ signature, ...latest }, "confirmed");
    if (confirmation.value.err) throw new Error(`transaction failed: ${JSON.stringify(confirmation.value.err)}`);
    const landed = await connection.getTransaction(signature, { commitment: "confirmed", maxSupportedTransactionVersion: 0 });
    return { signature, bytes, accounts, cu: landed?.meta?.computeUnitsConsumed, logs: landed?.meta?.logMessages ?? [] };
  };
  F.rejects = async (promise, name, why = name) => {
    let error;
    try { await promise; } catch (e) { error = e; }
    assert.ok(error, `expected ${name}: ${why}`);
    const text = `${error.message}\n${(error.transactionLogs ?? error.logs ?? []).join("\n")}`;
    const code = idl.errors.find((e) => e.name === name)?.code;
    const pattern = new RegExp(code === undefined ? name : `${name}|custom program error: 0x${code.toString(16)}\\b`);
    assert.ok(pattern.test(text), `${why}: expected ${name}, got ${text.slice(0, 1200)}`);
    step(`refused: ${why}`, { error: name });
  };
  F.lookupTable = async (addresses, { add = false } = {}) => {
    const slot = await connection.getSlot("confirmed");
    const [create, table] = AddressLookupTableProgram.createLookupTable({ authority: payer.publicKey, payer: payer.publicKey, recentSlot: slot - 1 });
    await F.send([create], [payer], { tables: [] });
    const unique = [...new Map(addresses.map((a) => [a.toBase58(), a])).values()];
    for (let i = 0; i < unique.length; i += 20) {
      await F.send([AddressLookupTableProgram.extendLookupTable({ lookupTable: table, authority: payer.publicKey, payer: payer.publicKey, addresses: unique.slice(i, i + 20) })], [payer], { tables: [] });
    }
    const target = (await connection.getSlot("confirmed")) + 1;
    while ((await connection.getSlot("confirmed")) <= target) await sleep(200);
    const loaded = (await connection.getAddressLookupTable(table)).value;
    F.tables = add ? [...F.tables, loaded] : [loaded];
  };
  return F;
}

const tokenAmount = async (F, account) => {
  const info = await F.connection.getAccountInfo(account, "confirmed");
  return info ? AccountLayout.decode(info.data).amount : 0n;
};
const mintSupply = async (F, mint) => (await F.connection.getAccountInfo(mint, "confirmed")).data.readBigUInt64LE(36);
const chainNow = async (F) => Number((await F.connection.getAccountInfo(new PublicKey("SysvarC1ock11111111111111111111111111111111"))).data.readBigInt64LE(32));
async function oreLstRate(F) {
  const stake = (await F.connection.getAccountInfo(oreLstAccounts.oreStake, "confirmed")).data;
  return { balance: stake.readBigUInt64LE(40), supply: await mintSupply(F, STORE_MINT) };
}

async function components(F) {
  const index = await F.program.account.indexState.fetch(SOV);
  const list = [];
  for (let p = 0; p < index.largeBasketPageCount; p += 1) {
    const page = pda("large-basket-component-page", SOV, Buffer.from([p]));
    const decoded = await F.program.account.largeBasketComponentPage.fetch(page);
    decoded.components.forEach((c, i) => list.push({ ...c, index: decoded.startComponentIndex + i, page, symbol: symbol(c.mint) }));
  }
  return { index, list, pages: [...new Set(list.map((c) => c.page.toBase58()))].map((k) => new PublicKey(k)) };
}

async function basketState(F, label) {
  const { index, list } = await components(F);
  const supply = await mintSupply(F, index.indexMint);
  const rows = [];
  for (const c of list) {
    const vault = await tokenAmount(F, c.vault);
    const usd = (Number(vault) / 10 ** c.decimals) * PRICES.get(c.mint.toBase58());
    rows.push({ symbol: c.symbol, vault: vault.toString(), accountedReserve: c.accountedReserve.toString(), unitsPerIndex: c.unitsPerIndex.toString(), targetWeightBps: c.targetWeightBps, usd });
  }
  const total = rows.reduce((s, r) => s + r.usd, 0);
  rows.forEach((r) => (r.weightBps = total ? Math.round((r.usd / total) * 10_000) : 0));
  const state = { label, supply: supply.toString(), tvlUsd: total, navUsd: Number(supply) ? total / (Number(supply) / 10 ** index.decimals) : 0, lastRebalancedAt: Number(index.fixedWeightLastRebalancedAt), openIntents: index.openIntentCount, components: rows };
  report.states[label] = state;
  return state;
}
function printState(s) {
  console.log(`\n  ${s.label}: supply ${(Number(s.supply) / 1e6).toFixed(6)} SOV, TVL $${s.tvlUsd.toFixed(2)}, NAV $${s.navUsd.toFixed(4)}`);
  for (const r of s.components) console.log(`    ${r.symbol.padEnd(6)} vault ${r.vault.padStart(16)}  accounted ${r.accountedReserve.padStart(16)}  units/index ${r.unitsPerIndex.padStart(10)}  target ${(r.targetWeightBps / 100).toFixed(2).padStart(6)}%  now ${(r.weightBps / 100).toFixed(2).padStart(6)}%  $${r.usd.toFixed(2)}`);
}

// --- mint and redeem ----------------------------------------------------------------------

let nextNonce = 1;
const intentPda = (owner, nonce) => pda("large-basket-intent", SOV, owner, bn(nonce).toArrayLike(Buffer, "le", 8));
const userUsdc = ata(USDC, user.publicKey);
const feeUsdc = ata(USDC, feeRecipient);
const rewardVault = ata(USDC, stakingAuthority);
const indexMint = new PublicKey(Object.entries(manifest.accounts).find(([, v]) => v.label === "SOV index mint")[0]);
const sovKeys = { index: SOV, indexMint, vaultAuthority };
const intentAccounts = (intent, owner = user.publicKey) => ({ owner, index: SOV, indexMint, intent, vaultAuthority, quoteMint: USDC, ownerQuoteTokenAccount: owner.equals(user.publicKey) ? userUsdc : ata(USDC, owner), jupiterProgram: JUP6, associatedTokenProgram: ASSOCIATED_TOKEN_PROGRAM_ID, quoteTokenProgram: TOKEN_PROGRAM_ID, systemProgram: SystemProgram.programId });
const feeAccounts = { feeRecipientQuoteTokenAccount: feeUsdc, creatorFeeRecipientQuoteTokenAccount: feeUsdc, stakingPool, stakingAuthority, stakingRewardVault: rewardVault };

async function openIntent(F, kind, amount, { inKind = false, owner = user } = {}) {
  const { list, pages, index } = await components(F);
  const nonce = nextNonce++;
  const intent = intentPda(owner.publicKey, nonce);
  const lock = pda("large-basket-intent-lock", SOV, owner.publicKey);
  const supply = await mintSupply(F, indexMint);
  // What the amount is worth now, to bound the mint's spend and the redeem's proceeds.
  let usd = 0;
  for (const c of list) usd += (Number(c.accountedReserve) * Number(amount)) / Number(supply) / 10 ** c.decimals * PRICES.get(c.mint.toBase58());
  const quote = BigInt(Math.round(usd * 1e6));
  const expiresAt = bn((await chainNow(F)) + 600);
  const ix = kind === "mint"
    ? await F.program.methods.openLargeBasketMintIntent({ indexAmountOut: bn(amount), maxQuoteIn: bn(inKind ? 0n : (quote * 105n) / 100n + 10_000n), nonce: bn(nonce), expiresAt, inKind })
      .accounts({ owner: owner.publicKey, index: SOV, indexMint, quoteMint: USDC, stakingPool, intent, intentLock: lock, systemProgram: SystemProgram.programId })
      .remainingAccounts(pages.map((p) => meta(p))).instruction()
    : await F.program.methods.openLargeBasketRedeemIntent({ indexAmountIn: bn(amount), minQuoteOut: bn(inKind ? 0n : (quote * 97n) / 100n), nonce: bn(nonce), expiresAt, inKind })
      .accounts({ owner: owner.publicKey, index: SOV, indexMint, ownerIndexTokenAccount: ata(indexMint, owner.publicKey), quoteMint: USDC, stakingPool, intent, intentLock: lock, tokenProgram: TOKEN_PROGRAM_ID, systemProgram: SystemProgram.programId })
      .remainingAccounts(pages.map((p) => meta(p, true))).instruction();
  await F.send([ix], [owner]);
  const state = await F.program.account.largeBasketIntent.fetch(intent);
  return { intent, nonce, list, pages, index, amounts: state.componentAmounts.map((a) => BigInt(a.toString())), quote };
}

function mintEntry(I, c) {
  const k = I.keys ?? sovKeys;
  const amount = I.amounts[c.index];
  const own = [meta(c.page), meta(c.mint), meta(c.vault, true), meta(c.tokenProgram)];
  if (amount === 0n) return { entry: { componentIndex: c.index, maxQuoteIn: bn(0), routeAccountCount: 0, swap: null }, remaining: own };
  if (c.mint.equals(USDC)) return { entry: { componentIndex: c.index, maxQuoteIn: bn((amount * 101n) / 100n + 10n), routeAccountCount: 0, swap: null }, remaining: own };
  const inAmount = exactOut(USDC, c.mint, amount);
  const swap = mockSwap({ authority: user.publicKey, source: userUsdc, destination: c.vault, inputMint: USDC, outputMint: c.mint, inAmount, outAmount: amount });
  const { plan, route } = compact(swap, [user.publicKey, k.index, k.indexMint, I.intent, k.vaultAuthority, USDC, userUsdc, JUP6, c.page, c.mint, c.vault, c.tokenProgram, ASSOCIATED_TOKEN_PROGRAM_ID, TOKEN_PROGRAM_ID, SystemProgram.programId]);
  return { entry: { componentIndex: c.index, maxQuoteIn: bn(inAmount), routeAccountCount: route.length, swap: plan }, remaining: [...own, ...route] };
}
function redeemEntry(I, c) {
  const k = I.keys ?? sovKeys;
  const amount = I.amounts[c.index];
  const own = [meta(c.page), meta(c.mint), meta(c.vault, true), meta(c.tokenProgram)];
  if (amount === 0n || c.mint.equals(USDC)) return { entry: { componentIndex: c.index, minQuoteOut: bn(0), routeAccountCount: 0, swap: null }, remaining: own };
  const outAmount = exactIn(c.mint, USDC, amount);
  const swap = mockSwap({ authority: k.vaultAuthority, source: c.vault, destination: userUsdc, inputMint: c.mint, outputMint: USDC, inAmount: amount, outAmount });
  const { plan, route } = compact(swap, [user.publicKey, k.index, k.indexMint, I.intent, k.vaultAuthority, USDC, userUsdc, JUP6, c.page, c.mint, c.vault, c.tokenProgram, TOKEN_PROGRAM_ID]);
  return { entry: { componentIndex: c.index, minQuoteOut: bn(outAmount), routeAccountCount: route.length, swap: plan }, remaining: [...own, ...route] };
}
const batches = (items, size = 2) => Array.from({ length: Math.ceil(items.length / size) }, (_, i) => items.slice(i * size, i * size + size));

// The wrapped legs. ORE for a mint is sized at the vault rate so the wrap mints at least the
// leg; a redeem sells the ORE the leg's stORE unwraps to.
async function wrappedMintIx(F, I, c, { swap, accounts = {}, maxQuoteIn, remaining } = {}) {
  const amount = I.amounts[c.index];
  const { balance, supply } = await oreLstRate(F);
  // A tenth of a percent over the vault rate covers yield the wrap compounds before it prices.
  const ore = ((amount * balance + supply - 1n) / supply) * 1001n / 1000n + 2n;
  const inAmount = exactOut(USDC, ORE_MINT, ore);
  const userOre = ata(ORE_MINT, user.publicKey);
  const built = compact(swap ?? mockSwap({ authority: user.publicKey, source: userUsdc, destination: userOre, inputMint: USDC, outputMint: ORE_MINT, inAmount, outAmount: ore }), [user.publicKey, userUsdc, USDC, userOre, ORE_MINT, TOKEN_PROGRAM_ID, ASSOCIATED_TOKEN_PROGRAM_ID, SystemProgram.programId, JUP6]);
  const ix = await F.program.methods.executeLargeBasketMintWrappedComponent({ componentIndex: c.index, maxQuoteIn: bn(maxQuoteIn ?? inAmount), swap: built.plan })
    .accounts({ ...intentAccounts(I.intent), componentPage: c.page, componentMint: c.mint, componentVault: c.vault, oreMint: ORE_MINT, ownerOreTokenAccount: userOre, ownerStoreTokenAccount: ata(STORE_MINT, user.publicKey), oreLst: oreLstAccounts, tokenProgram: TOKEN_PROGRAM_ID, ...accounts })
    .remainingAccounts(remaining ?? built.route).instruction();
  return { ix, ore, inAmount };
}
async function wrappedRedeemIx(F, I, c, { sellFraction = 1, minQuoteOut, accounts = {} } = {}) {
  const amount = I.amounts[c.index];
  const { balance, supply } = await oreLstRate(F);
  const ore = (amount * balance) / supply - 1n;
  const sold = BigInt(Math.floor(Number(ore) * sellFraction));
  const outAmount = exactIn(ORE_MINT, USDC, sold);
  const userOre = ata(ORE_MINT, user.publicKey);
  const built = compact(mockSwap({ authority: user.publicKey, source: userOre, destination: userUsdc, inputMint: ORE_MINT, outputMint: USDC, inAmount: sold, outAmount }), [user.publicKey, userUsdc, USDC, userOre, ORE_MINT, TOKEN_PROGRAM_ID, ASSOCIATED_TOKEN_PROGRAM_ID, SystemProgram.programId, JUP6]);
  const ix = await F.program.methods.executeLargeBasketRedeemWrappedComponent({ componentIndex: c.index, minQuoteOut: bn(minQuoteOut ?? outAmount), swap: built.plan })
    .accounts({ ...intentAccounts(I.intent), componentPage: c.page, componentMint: c.mint, componentVault: c.vault, oreMint: ORE_MINT, ownerOreTokenAccount: userOre, ownerStoreTokenAccount: ata(STORE_MINT, user.publicKey), oreLst: oreLstAccounts, tokenProgram: TOKEN_PROGRAM_ID, ...feeAccounts, intentLock, ...accounts })
    .remainingAccounts(built.route).instruction();
  return { ix, ore, outAmount };
}

// A swap-path mint; with `wrapped`, its stORE leg goes through ORE's staking program.
async function mintSwap(F, amount, { wrapped = false } = {}) {
  const I = await openIntent(F, "mint", amount);
  const ordered = [...I.list.filter((c) => !c.mint.equals(USDC)), ...I.list.filter((c) => c.mint.equals(USDC))];
  const viaBatch = ordered.filter((c) => !(wrapped && c.mint.equals(STORE_MINT) && I.amounts[c.index] > 0n));
  const sent = {};
  if (wrapped) {
    const c = I.list.find((x) => x.mint.equals(STORE_MINT));
    const before = { vault: await tokenAmount(F, c.vault), stake: (await oreLstRate(F)).balance, ore: await tokenAmount(F, ata(ORE_MINT, user.publicKey)), store: await tokenAmount(F, ata(STORE_MINT, user.publicKey)) };
    const { ix, ore } = await wrappedMintIx(F, I, c);
    sent.wrapped = await F.send([ix], [user]);
    const delivered = (await tokenAmount(F, c.vault)) - before.vault;
    assert.ok(delivered >= I.amounts[c.index], "the wrapped leg delivered the leg's stORE");
    // Wrapping first compounds the vault's unclaimed yield, so the stake grows by a little more.
    const staked = (await oreLstRate(F)).balance - before.stake;
    assert.ok(staked >= ore, "ore-lst staked the ORE bought");
    assert.equal(await tokenAmount(F, ata(ORE_MINT, user.publicKey)), before.ore, "no ORE left with the minter");
    assert.equal(await tokenAmount(F, ata(STORE_MINT, user.publicKey)), before.store, "no stORE left with the minter");
    sent.storeLeg = { required: I.amounts[c.index], delivered, oreWrapped: ore, yieldCompounded: staked - ore };
  }
  for (const group of batches(viaBatch.map((c) => mintEntry(I, c)))) {
    const ix = await F.program.methods.executeLargeBasketMintBatch({ entries: group.map((g) => g.entry) }).accounts(intentAccounts(I.intent)).remainingAccounts(group.flatMap((g) => g.remaining)).instruction();
    await F.send([ix], [user]);
  }
  const collect = await F.program.methods.collectLargeBasketIntentFees().accounts({ owner: user.publicKey, intent: I.intent, quoteMint: USDC, ownerQuoteTokenAccount: userUsdc, ...feeAccounts, tokenProgram: TOKEN_PROGRAM_ID }).instruction();
  const finalize = await F.program.methods.finalizeLargeBasketMintIntent().accounts({ owner: user.publicKey, index: SOV, indexMint, vaultAuthority, intent: I.intent, intentLock, ownerIndexTokenAccount: ata(indexMint, user.publicKey), associatedTokenProgram: ASSOCIATED_TOKEN_PROGRAM_ID, tokenProgram: TOKEN_PROGRAM_ID, systemProgram: SystemProgram.programId }).remainingAccounts(I.pages.map((p) => meta(p, true))).instruction();
  const before = await tokenAmount(F, ata(indexMint, user.publicKey));
  await F.send([collect, finalize], [user]);
  const minted = (await tokenAmount(F, ata(indexMint, user.publicKey))) - before;
  assert.ok(minted >= BigInt(amount), "minted at least the requested amount");
  return { minted, ...sent };
}

async function redeemSwap(F, amount, { wrapped = false } = {}) {
  const I = await openIntent(F, "redeem", amount);
  const watched = [userUsdc, feeUsdc, rewardVault];
  const before = await Promise.all(watched.map((a) => tokenAmount(F, a)));
  const sent = {};
  const store = I.list.find((x) => x.mint.equals(STORE_MINT));
  const viaWrap = wrapped && store && I.amounts[store.index] > 0n;
  for (const group of batches(I.list.filter((c) => !(viaWrap && c === store)).map((c) => redeemEntry(I, c)))) {
    const ix = await F.program.methods.executeLargeBasketRedeemBatch({ entries: group.map((g) => g.entry) }).accounts({ ...intentAccounts(I.intent), ...feeAccounts, intentLock }).remainingAccounts(group.flatMap((g) => g.remaining)).instruction();
    await F.send([ix], [user]);
  }
  if (viaWrap) {
    const vaultBefore = await tokenAmount(F, store.vault);
    const stakeBefore = (await oreLstRate(F)).balance;
    const oreBefore = await tokenAmount(F, ata(ORE_MINT, user.publicKey));
    const { ix, ore } = await wrappedRedeemIx(F, I, store);
    sent.wrapped = await F.send([ix], [user]);
    assert.equal(vaultBefore - (await tokenAmount(F, store.vault)), I.amounts[store.index], "the vault paid exactly the leg's stORE");
    sent.storeLeg = { amount: I.amounts[store.index], stakeDecrease: stakeBefore - (await oreLstRate(F)).balance, oreSold: ore, oreLeftWithRedeemer: (await tokenAmount(F, ata(ORE_MINT, user.publicKey))) - oreBefore };
  }
  const settled = await F.program.account.largeBasketIntent.fetch(I.intent);
  assert.ok(settled.status.finalized, "the last leg finalized the redeem");
  const [proceeds, protocolFee, stakingFee] = (await Promise.all(watched.map((a) => tokenAmount(F, a)))).map((a, i) => a - before[i]);
  const gross = BigInt(settled.quoteAtomsExecuted.toString());
  // SOV charges 5 bps to the protocol and 5 to stakers on redeems, each rounded up.
  assert.equal(protocolFee, (gross * 5n + 9_999n) / 10_000n, "protocol fee");
  assert.equal(stakingFee, (gross * 5n + 9_999n) / 10_000n, "staking fee");
  assert.equal(proceeds + protocolFee + stakingFee, gross, "the redeemer got the rest");
  return { gross, proceeds, protocolFee, stakingFee, ...sent };
}

async function redeemInKind(F, amount) {
  const I = await openIntent(F, "redeem", amount, { inKind: true });
  const received = {};
  for (const group of batches(I.list, 2)) {
    const ixs = [];
    for (const c of group) {
      const own = ata(c.mint, user.publicKey);
      received[c.symbol] = await tokenAmount(F, own);
      ixs.push(await F.program.methods.executeLargeBasketRedeemComponentInKind({ componentIndex: c.index }).accounts({ owner: user.publicKey, index: SOV, indexMint, intent: I.intent, vaultAuthority, componentPage: c.page, componentMint: c.mint, componentVault: c.vault, componentTokenProgram: c.tokenProgram, ownerComponentTokenAccount: own, protocolFeeComponentAccount: ata(c.mint, feeRecipient), creatorFeeComponentAccount: ata(c.mint, feeRecipient), associatedTokenProgram: ASSOCIATED_TOKEN_PROGRAM_ID, systemProgram: SystemProgram.programId }).instruction());
    }
    await F.send(ixs, [user]);
  }
  await F.send([await F.program.methods.finalizeLargeBasketRedeemIntent().accounts({ index: SOV, intent: I.intent, intentLock }).instruction()], [user]);
  for (const c of I.list) received[c.symbol] = ((await tokenAmount(F, ata(c.mint, user.publicKey))) - received[c.symbol]).toString();
  return received;
}

// Accounts every user flow touches, so transactions fit through one lookup table.
async function userLookupTable(F) {
  const { list, pages } = await components(F);
  await F.lookupTable([
    SOV, indexMint, vaultAuthority, USDC, userUsdc, JUP6, ASSOCIATED_TOKEN_PROGRAM_ID, TOKEN_PROGRAM_ID, SystemProgram.programId,
    stakingPool, stakingAuthority, rewardVault, feeUsdc, intentLock, ata(indexMint, user.publicKey), RESERVE, ata(USDC, RESERVE),
    ...pages, ...list.flatMap((c) => [c.mint, c.vault, ata(c.mint, RESERVE), ata(c.mint, user.publicKey), ata(c.mint, feeRecipient)]),
    ORE_MINT, STORE_MINT, ata(ORE_MINT, user.publicKey), ata(STORE_MINT, user.publicKey), ata(ORE_MINT, RESERVE), ata(STORE_MINT, RESERVE), ...Object.values(oreLstAccounts),
    ComputeBudgetProgram.programId,
  ]);
}
async function createAtas(F, mints, owners) {
  const ixs = mints.flatMap((m) => owners.map((o) => createAssociatedTokenAccountIdempotentInstruction(payer.publicKey, ata(m, o), o, m, TOKEN_PROGRAM_ID)));
  for (const group of batches(ixs, 6)) await F.send(group, [payer], { tables: [] });
}

// Every account a later phase needs, as validator account dumps.
async function carryOver(F, dir) {
  fs.mkdirSync(dir, { recursive: true });
  const infos = new Map();
  for (const { pubkey, account } of await F.connection.getProgramAccounts(PROGRAM_ID, "confirmed")) infos.set(pubkey.toBase58(), account);
  for (const owner of [vaultAuthority, user.publicKey, thief.publicKey, RESERVE, feeRecipient, stakingAuthority, payer.publicKey]) {
    for (const { pubkey, account } of (await F.connection.getTokenAccountsByOwner(owner, { programId: TOKEN_PROGRAM_ID }, "confirmed")).value) infos.set(pubkey.toBase58(), account);
  }
  const extra = [indexMint, ...new Set(manifest.basket.components.map((c) => c.mint)), ORE_MINT.toBase58(), STORE_MINT.toBase58(), "2rNBaMg5VAr1aMNCwAPdDZVgzzdTaNDebUnNqPFNmeta", user.publicKey, thief.publicKey,
    ...["oreLstVault", "oreLstVaultTokens", "oreStake", "oreStakeTokens", "oreStakeTreasury", "oreStakeTreasuryTokens", "oreStakeVesting"].map((k) => oreLstAccounts[k])].map((k) => new PublicKey(k));
  const fetched = await F.connection.getMultipleAccountsInfo(extra, "confirmed");
  extra.forEach((k, i) => fetched[i] && infos.set(k.toBase58(), fetched[i]));
  for (const [key, info] of infos) writeAccount(dir, new PublicKey(key), { lamports: info.lamports, data: info.data, owner: info.owner, executable: info.executable });
  return infos.size;
}

// --- the rehearsal ------------------------------------------------------------------------

async function phase1() {
  const dir = path.join(root, ".anchor", `fork-accounts-1-${Date.now()}`);
  fs.mkdirSync(dir, { recursive: true });
  for (const file of fs.readdirSync(snapshotDir).filter((f) => f.endsWith(".json") && f !== "manifest.json")) {
    const { pubkey, account } = JSON.parse(fs.readFileSync(path.join(snapshotDir, file), "utf8"));
    writeAccount(dir, new PublicKey(pubkey), { lamports: account.lamports, data: Buffer.from(account.data[0], "base64"), owner: new PublicKey(account.owner), executable: account.executable });
  }
  report.measurements.accountsToppedUpToLocalRent = toppedUp;
  // A tester with USDC, and the mock's reserves, which only exist on this validator.
  writeAccount(dir, userUsdc, tokenAccount(USDC, user.publicKey, 5_000_000_000n));
  const reserves = { [USDC.toBase58()]: 10_000_000_000_000n, [ORE_MINT.toBase58()]: 100_000n * 10n ** 11n, [STORE_MINT.toBase58()]: 100_000n * 10n ** 11n };
  for (const c of manifest.basket.components) reserves[c.mint] ??= 10n ** BigInt(c.decimals) * 1_000_000n;
  for (const [mint, amount] of Object.entries(reserves)) writeAccount(dir, ata(new PublicKey(mint), RESERVE), tokenAccount(new PublicKey(mint), RESERVE, amount));

  console.log("\nphase 1: the deployed binary on mainnet's state");
  const v = await startValidator("1", path.join(snapshotDir, manifest.basketProgram.file), dir);
  try {
    const F = makeFork(v.connection, v.rpcUrl);
    const programData = PublicKey.findProgramAddressSync([PROGRAM_ID.toBuffer()], LOADER)[0];
    const deployed = (await F.connection.getAccountInfo(programData)).data.subarray(45);
    assert.equal(createHash("sha256").update(deployed).digest("hex"), manifest.basketProgram.sha256);
    step("validator runs mainnet's basket binary", { sha256: manifest.basketProgram.sha256.slice(0, 16) });
    await F.send([SystemProgram.transfer({ fromPubkey: payer.publicKey, toPubkey: user.publicKey, lamports: 20 * LAMPORTS_PER_SOL }), SystemProgram.transfer({ fromPubkey: payer.publicKey, toPubkey: thief.publicKey, lamports: LAMPORTS_PER_SOL })], [payer], { tables: [] });
    const mints = manifest.basket.components.map((c) => new PublicKey(c.mint));
    await createAtas(F, [...mints, ORE_MINT, STORE_MINT], [user.publicKey, feeRecipient]);
    await userLookupTable(F);
    printState(await basketState(F, "mainnet now"));

    const flows = async (when) => {
      const minted = await mintSwap(F, 10_000_000);
      step(`${when}: swap mint of 10 SOV`, { minted: minted.minted });
      const redeemed = await redeemSwap(F, 5_000_000);
      step(`${when}: swap redeem of 5 SOV`, { usdc: redeemed.proceeds, fees: redeemed.protocolFee + redeemed.stakingFee });
      const inKind = await redeemInKind(F, 5_000_000);
      step(`${when}: in-kind redeem of 5 SOV`, inKind);
    };
    await flows("deployed binary");
    // The wrapped legs don't exist before the upgrade.
    const I = await openIntent(F, "mint", 1_000_000);
    const ore = I.list.find((c) => c.mint.equals(ORE_MINT));
    await F.rejects(F.send([(await wrappedMintIx(F, I, ore)).ix], [user]), "InstructionFallbackNotFound", "wrapped mint before the upgrade");
    for (const group of batches(I.list.map((c) => mintEntry(I, c)))) await F.send([await F.program.methods.executeLargeBasketMintBatch({ entries: group.map((g) => g.entry) }).accounts(intentAccounts(I.intent)).remainingAccounts(group.flatMap((g) => g.remaining)).instruction()], [user]);
    await F.send([
      await F.program.methods.collectLargeBasketIntentFees().accounts({ owner: user.publicKey, intent: I.intent, quoteMint: USDC, ownerQuoteTokenAccount: userUsdc, ...feeAccounts, tokenProgram: TOKEN_PROGRAM_ID }).instruction(),
      await F.program.methods.finalizeLargeBasketMintIntent().accounts({ owner: user.publicKey, index: SOV, indexMint, vaultAuthority, intent: I.intent, intentLock, ownerIndexTokenAccount: ata(indexMint, user.publicKey), associatedTokenProgram: ASSOCIATED_TOKEN_PROGRAM_ID, tokenProgram: TOKEN_PROGRAM_ID, systemProgram: SystemProgram.programId }).remainingAccounts(I.pages.map((p) => meta(p, true))).instruction(),
    ], [user]);
    await redeemSwap(F, 1_000_000);
    printState(await basketState(F, "before the upgrade"));

    // The upgrade, as on mainnet: extend ProgramData, then deploy over it.
    const stateBefore = new Map();
    const watch = async () => {
      const keys = [...(await F.connection.getProgramAccounts(PROGRAM_ID, "confirmed")).map((a) => a.pubkey), ...(await F.connection.getTokenAccountsByOwner(vaultAuthority, { programId: TOKEN_PROGRAM_ID })).value.map((a) => a.pubkey), indexMint];
      const infos = await F.connection.getMultipleAccountsInfo(keys, "confirmed");
      return new Map(keys.map((k, i) => [k.toBase58(), createHash("sha256").update(infos[i].data).update(String(infos[i].lamports)).digest("hex")]));
    };
    for (const [k, h] of await watch()) stateBefore.set(k, h);
    const localSize = (await F.connection.getAccountInfo(programData)).data.length;
    const extendBy = manifest.basketProgram.programDataSize - localSize + MAINNET_EXTEND_BYTES;
    const newSize = fs.statSync(NEW_BINARY).size;
    assert.ok(newSize > manifest.basketProgram.maxProgramBytes, "the new binary needs a larger ProgramData");
    assert.ok(newSize <= manifest.basketProgram.maxProgramBytes + MAINNET_EXTEND_BYTES, "the planned extension fits it");
    const cli = (...args) => {
      const out = spawnSync(path.join(CLI_BIN, exe("solana")), [...args, "--url", v.rpcUrl, "--keypair", path.join(root, "deployer-keypair.json"), "--commitment", "confirmed"], { cwd: root, encoding: "utf8", timeout: 600_000 });
      if (out.status !== 0) throw new Error(`solana ${args[0]} ${args[1]}: ${out.stdout}\n${out.stderr}`);
      return out.stdout.trim();
    };
    const balanceBefore = await F.connection.getBalance(payer.publicKey);
    cli("program", "extend", PROGRAM_ID.toBase58(), String(extendBy));
    const extendCost = balanceBefore - (await F.connection.getBalance(payer.publicKey));
    cli("program", "deploy", NEW_BINARY, "--program-id", PROGRAM_ID.toBase58(), "--upgrade-authority", path.join(root, "deployer-keypair.json"), "--use-rpc", "--max-sign-attempts", "20");
    const after = await F.connection.getAccountInfo(programData);
    assert.equal(after.data.length, manifest.basketProgram.programDataSize + MAINNET_EXTEND_BYTES, "ProgramData ends at mainnet's size plus the extension");
    assert.equal(createHash("sha256").update(after.data.subarray(45, 45 + newSize)).digest("hex"), createHash("sha256").update(fs.readFileSync(NEW_BINARY)).digest("hex"));
    report.measurements.upgrade = { mainnetProgramBytes: manifest.basketProgram.maxProgramBytes, newBinaryBytes: newSize, extendBytes: MAINNET_EXTEND_BYTES, extendRentLamports: extendCost, headroomAfterBytes: manifest.basketProgram.maxProgramBytes + MAINNET_EXTEND_BYTES - newSize };
    step("upgraded: ProgramData extended and the new binary deployed", report.measurements.upgrade);
    const target = (await F.connection.getSlot("confirmed")) + 2;
    while ((await F.connection.getSlot("confirmed")) < target) await sleep(200);
    const changed = [...(await watch())].filter(([k, h]) => stateBefore.get(k) !== h).map(([k]) => k);
    assert.deepEqual(changed, [], "the upgrade changed no basket account");
    step("the upgrade left every SOV and protocol account byte-identical", { accounts: stateBefore.size });
    printState(await basketState(F, "right after the upgrade"));

    await flows("upgraded binary");
    // SOV holds ORE, not stORE, until its composition changes.
    const J = await openIntent(F, "mint", 1_000_000);
    await F.rejects(F.send([(await wrappedMintIx(F, J, J.list.find((c) => c.mint.equals(ORE_MINT)))).ix], [user]), "WrappedComponentNotStore", "wrapped mint of an ORE component");
    await F.send([await F.program.methods.cancelUnfilledLargeBasketMintIntent().accounts({ owner: user.publicKey, index: SOV, intent: J.intent, intentLock }).instruction()], [user]);
    const open = (await F.program.account.indexState.fetch(SOV)).openIntentCount;
    if (open) {
      for (const group of batches(J.list.map((c) => mintEntry(J, c)))) await F.send([await F.program.methods.executeLargeBasketMintBatch({ entries: group.map((g) => g.entry) }).accounts(intentAccounts(J.intent)).remainingAccounts(group.flatMap((g) => g.remaining)).instruction()], [user]);
      await F.send([
        await F.program.methods.collectLargeBasketIntentFees().accounts({ owner: user.publicKey, intent: J.intent, quoteMint: USDC, ownerQuoteTokenAccount: userUsdc, ...feeAccounts, tokenProgram: TOKEN_PROGRAM_ID }).instruction(),
        await F.program.methods.finalizeLargeBasketMintIntent().accounts({ owner: user.publicKey, index: SOV, indexMint, vaultAuthority, intent: J.intent, intentLock, ownerIndexTokenAccount: ata(indexMint, user.publicKey), associatedTokenProgram: ASSOCIATED_TOKEN_PROGRAM_ID, tokenProgram: TOKEN_PROGRAM_ID, systemProgram: SystemProgram.programId }).remainingAccounts(J.pages.map((p) => meta(p, true))).instruction(),
      ], [user]);
      await redeemSwap(F, 1_000_000);
    }
    assert.equal((await F.program.account.indexState.fetch(SOV)).openIntentCount, 0);

    // The proposal, as scripts/composition-change.mjs sends it.
    const { list, pages } = await components(F);
    const weights = list.map((c) => NEW_WEIGHTS[c.symbol]);
    await F.send([await F.program.methods.proposeCompositionChange({ targetWeightsBps: weights, additions: [{ mint: STORE_MINT, oraclePair: PublicKey.default, targetWeightBps: STORE_WEIGHT }] })
      .accounts({ authority: payer.publicKey, index: SOV, compositionChange, systemProgram: SystemProgram.programId })
      .remainingAccounts([...pages.map((p) => meta(p)), meta(STORE_MINT)]).instruction()], [payer]);
    const change = await F.program.account.compositionChange.fetch(compositionChange);
    const notice = Number(change.effectiveAt) - Number(change.proposedAt);
    assert.ok(notice >= 3 * 86_400 && notice <= 3 * 86_400 + 60, `three days of notice, got ${notice}s`);
    step("composition change proposed: ORE 50% -> stORE 50%", { effectiveAt: new Date(Number(change.effectiveAt) * 1000).toISOString(), weights: Object.fromEntries(list.map((c, i) => [c.symbol, weights[i]])), add: { stORE: STORE_WEIGHT } });
    await F.rejects(F.send([await F.program.methods.applyCompositionChange().accounts({ operator: payer.publicKey, index: SOV, vaultAuthority, compositionChange, proposer: payer.publicKey, associatedTokenProgram: ASSOCIATED_TOKEN_PROGRAM_ID, systemProgram: SystemProgram.programId })
      .remainingAccounts([...pages.map((p) => meta(p, true)), meta(STORE_MINT), meta(ata(STORE_MINT, vaultAuthority), true), meta(TOKEN_PROGRAM_ID)]).instruction()], [payer]), "CompositionChangeNotReady", "applying before the notice ends");
    printState(await basketState(F, "end of phase 1"));

    const next = path.join(root, ".anchor", `fork-accounts-2-${Date.now()}`);
    const count = await carryOver(F, next);
    // The 3-day wait: the proposal's timestamps move back, nothing else changes.
    const file = path.join(next, `${compositionChange.toBase58()}.json`);
    const dump = JSON.parse(fs.readFileSync(file, "utf8"));
    const now = Math.floor(Date.now() / 1000);
    const encoded = await F.program.coder.accounts.encode("compositionChange", { ...change, proposedAt: bn(now - 3 * 86_400 - 120), effectiveAt: bn(now - 120) });
    dump.account.data[0] = Buffer.concat([encoded, Buffer.alloc(Buffer.from(dump.account.data[0], "base64").length - encoded.length)]).toString("base64");
    fs.writeFileSync(file, JSON.stringify(dump));
    step("carried every account into phase 2, with the proposal 3 days older", { accounts: count });
    fs.rmSync(dir, { recursive: true, force: true });
    return next;
  } finally {
    await v.stop();
  }
}

// The keeper as deployed, against the local validator, the mock APIs and the local oracle key.
const keeperLog = path.join(root, ".anchor", "wrapped-ore-fork-keeper.log");
function keeperRunner(v, api) {
  const env = { ...process.env, SOLANA_RPC_URL: v.rpcUrl, ANCHOR_WALLET: "deployer-keypair.json", ORACLE_KEYPAIR: JSON.stringify([...oracle.secretKey]), JUPITER_SWAP_API: `${api.base}/swap/v1`, JUPITER_PRICE_API: `${api.base}/price/v3` };
  for (const k of ["ORACLE_URL", "ORACLE_API_TOKEN", "JUPITER_API_KEY", "KEEPER_KEYPAIR", "ORACLE_WALLET"]) delete env[k];
  return (index, flags) => new Promise((resolve) => {
    const child = spawn(process.execPath, ["scripts/rebalance-bot.mjs", "--index", index.toBase58(), "--execute", ...flags], { cwd: root, env, stdio: ["ignore", "pipe", "pipe"], windowsHide: true });
    let out = "";
    child.stdout.on("data", (d) => (out += d));
    child.stderr.on("data", (d) => (out += d));
    child.on("close", (code) => {
      fs.appendFileSync(keeperLog, `
===== rebalance-bot --index ${index.toBase58()} ${flags.join(" ")} =====
${out}`);
      assert.equal(code, 0, `keeper exited ${code}: ${out.slice(-3000)}`);
      resolve(out);
    });
  });
}

async function phase2(dir) {
  console.log("\nphase 2: the upgraded binary, 3 days later");
  const v = await startValidator("2", NEW_BINARY, dir);
  const api = await startMockJupiterApi(v.connection);
  try {
    const F = makeFork(v.connection, v.rpcUrl);
    await F.send([await F.program.methods.setPriceOracle({ oracle: oracle.publicKey }).accounts({ authority: payer.publicKey, protocolConfig, priceOracle, systemProgram: SystemProgram.programId }).instruction()], [payer], { tables: [] });
    step("price oracle pointed at a local test key (the real one stays on Fly)");
    printState(await basketState(F, "phase 2 start"));

    fs.writeFileSync(keeperLog, "");
    const keeper = keeperRunner(v, api);
    const runKeeper = (flags) => keeper(SOV, flags);
    // First as deployed on Fly (default 1% slippage, 0.5% NAV tolerance).
    const deployed = await runKeeper(["--priority-fee", "50000"]);
    assert.ok(/composition change applied/.test(deployed), `keeper applied the change: ${deployed.slice(-3000)}`);
    let index = await F.program.account.indexState.fetch(SOV);
    assert.equal(await F.program.account.compositionChange.fetchNullable(compositionChange), null, "the proposal is consumed");
    if (index.compositionRebalanceDue) {
      const reason = deployed.match(/ERROR processing [^:]+: (.*)/)?.[1] ?? "unknown";
      assert.ok(!index.largeBasketOperationInProgress && index.activeRebalanceIntent.equals(PublicKey.default), "a refused rebalance leaves no intent open");
      report.measurements.keeperAsDeployed = { applied: true, rebalanced: false, reason };
      step("keeper as deployed applied the change but refused the switchover rebalance", { reason });
      printState(await basketState(F, "change applied, rebalance refused"));
      // The switchover trades half the basket; a one-off pass with tighter swap slippage and the
      // program's widest NAV tolerance budgets its buys within the sells' guaranteed proceeds.
      const operator = ["--slippage-bps", "30", "--nav-tolerance-bps", "100"];
      const second = await runKeeper(operator);
      assert.ok(!/ERROR processing/.test(second), `operator pass failed: ${second.slice(-3000)}`);
      report.measurements.keeperSwitchoverFlags = operator.join(" ");
      step("one-off keeper pass rebalanced the switchover", { flags: operator.join(" ") });
    } else {
      report.measurements.keeperAsDeployed = { applied: true, rebalanced: true };
    }
    index = await F.program.account.indexState.fetch(SOV);
    assert.ok(!index.compositionRebalanceDue, "the switchover rebalance finished");
    step("keeper applied the change and rebalanced", { mockJupiterCalls: api.calls, log: path.relative(root, keeperLog) });
    const rebalanced = await basketState(F, "after the change and rebalance");
    printState(rebalanced);
    const store = rebalanced.components.find((r) => r.symbol === "stORE");
    assert.ok(Math.abs(store.weightBps - STORE_WEIGHT) <= 150, `stORE at ${store.weightBps} bps`);
    assert.equal(rebalanced.components.find((r) => r.symbol === "ORE").accountedReserve, "0", "ORE fully sold");
    const before = report.states["phase 2 start"];
    report.measurements.rebalanceNavChangeBps = Math.round((rebalanced.navUsd / before.navUsd - 1) * 10_000);
    step("NAV across the switchover", { beforeUsd: before.navUsd, afterUsd: rebalanced.navUsd, changeBps: report.measurements.rebalanceNavChangeBps });

    await userLookupTable(F);
    await createAtas(F, [ORE_MINT, STORE_MINT], [thief.publicKey]);

    // The new legs on the live basket.
    const minted = await mintSwap(F, 10_000_000, { wrapped: true });
    report.measurements.wrappedMint = { bytes: minted.wrapped.bytes, accounts: minted.wrapped.accounts, computeUnits: minted.wrapped.cu, ...minted.storeLeg };
    step("mint of 10 SOV with its stORE leg wrapped from ORE", report.measurements.wrappedMint);
    const redeemed = await redeemSwap(F, 5_000_000, { wrapped: true });
    report.measurements.wrappedRedeem = { bytes: redeemed.wrapped.bytes, accounts: redeemed.wrapped.accounts, computeUnits: redeemed.wrapped.cu, ...redeemed.storeLeg, usdc: redeemed.proceeds, protocolFee: redeemed.protocolFee, stakingFee: redeemed.stakingFee };
    step("redeem of 5 SOV with its stORE leg unwrapped to ORE", report.measurements.wrappedRedeem);
    step("in-kind redeem of 2 SOV, stORE included", await redeemInKind(F, 2_000_000));
    const viaPools = await mintSwap(F, 2_000_000);
    step("swap mint of 2 SOV buying stORE through a pool, as before", { minted: viaPools.minted });
    step("swap redeem of 2 SOV selling stORE through a pool, as before", { usdc: (await redeemSwap(F, 2_000_000)).proceeds });

    await attacks(F);
    const end = await basketState(F, "end of the rehearsal");
    printState(end);
    // Room left for a real Jupiter route: the mock's route adds 3 accounts of its own.
    for (const leg of ["wrappedMint", "wrappedRedeem"]) report.measurements[leg].accountsForARealRoute = 64 - (report.measurements[leg].accounts - 3);

    const next = path.join(root, ".anchor", `fork-accounts-3-${Date.now()}`);
    const count = await carryOver(F, next);
    const patch = (key, change) => {
      const file = path.join(next, `${key.toBase58()}.json`);
      const dump = JSON.parse(fs.readFileSync(file, "utf8"));
      const data = Buffer.from(dump.account.data[0], "base64");
      change(data);
      dump.account.data[0] = data.toString("base64");
      fs.writeFileSync(file, JSON.stringify(dump));
    };
    // XAUt0's vault frozen: an SPL token account's state byte (1 initialized, 2 frozen).
    const xaut = (await components(F)).list.find((c) => c.symbol === "XAUt0");
    patch(xaut.vault, (data) => { data[108] = 2; });
    // SOV's last rebalance a month and an hour back, so its time trigger is due.
    const sov = await F.program.account.indexState.fetch(SOV);
    const back = Math.floor(Date.now() / 1000) - Number(sov.fixedWeightRebalanceIntervalSeconds) - 3_600;
    const encoded = await F.program.coder.accounts.encode("indexState", { ...sov, fixedWeightLastRebalancedAt: bn(back) });
    patch(SOV, (data) => encoded.copy(data));
    // The tester gets more USDC for the community basket.
    patch(userUsdc, (data) => data.writeBigUInt64LE(5_000_000_000n, 64));
    step("carried every account into phase 3, with SOV's XAUt0 vault frozen and its rebalance due", { accounts: count });
    return next;
  } finally {
    api.server.close();
    await v.stop();
    fs.rmSync(dir, { recursive: true, force: true });
  }
}

// --- a community basket -------------------------------------------------------------------

// A community basket's keys and fee accounts, for the generic legs above.
function communityKeys(symbolText, treasury) {
  const index = pda("index", user.publicKey, symbolText);
  return {
    symbol: symbolText, index, indexMint: pda("index-mint", index), vaultAuthority: pda("vault-authority", index),
    page: pda("large-basket-component-page", index, Buffer.from([0])), lock: pda("large-basket-intent-lock", index, user.publicKey),
    treasury, creatorFeeRecipient: thief.publicKey,
  };
}
const communityAccounts = (B, intent) => ({ owner: user.publicKey, index: B.index, indexMint: B.indexMint, intent, vaultAuthority: B.vaultAuthority, quoteMint: USDC, ownerQuoteTokenAccount: userUsdc, jupiterProgram: JUP6, associatedTokenProgram: ASSOCIATED_TOKEN_PROGRAM_ID, quoteTokenProgram: TOKEN_PROGRAM_ID, systemProgram: SystemProgram.programId });
const communityFees = (B) => ({ feeRecipientQuoteTokenAccount: ata(USDC, B.treasury), creatorFeeRecipientQuoteTokenAccount: ata(USDC, B.creatorFeeRecipient), stakingPool, stakingAuthority, stakingRewardVault: rewardVault });

// Created as the app's create page does it: the basket, its page, then finalize. `weights` are
// [mint, bps]; one index token is worth `nav` dollars at today's prices.
async function createCommunityBasket(F, symbolText, weights, nav = 10) {
  const config = await F.program.account.communityConfig.fetch(communityConfig);
  const B = communityKeys(symbolText, config.treasury);
  const claimKey = symbolText.replace(/0/g, "O").replace(/1/g, "I");
  const create = await F.program.methods.createCommunityBasketIndex({ name: `${symbolText} fork test`, symbol: symbolText, metadataUri: "", decimals: 6, creatorFeeRecipient: B.creatorFeeRecipient, mintFeeBps: 10, redeemFeeBps: 10, maxSupply: bn(0), kind: { fixedWeights: {} }, fixedWeightRebalanceIntervalSeconds: bn(86_400), fixedWeightDriftThresholdBps: 500, componentCount: weights.length, symbolClaimKey: claimKey })
    .accounts({ payer: user.publicKey, authority: user.publicKey, protocolConfig, communityConfig, treasury: config.treasury, index: B.index, symbolClaim: pda("symbol", claimKey), indexMint: B.indexMint, vaultAuthority: B.vaultAuthority, tokenProgram: TOKEN_PROGRAM_ID, systemProgram: SystemProgram.programId }).instruction();
  // Creation is still closed on mainnet: only whitelisted wallets may create.
  await F.rejects(F.send([create], [user], { tables: [] }), "UnauthorizedIndexCreator", "a community basket from a wallet the protocol has not allowed");
  await F.send([await F.program.methods.updateIndexCreatorWhitelist({ creator: user.publicKey, whitelisted: true }).accounts({ authority: payer.publicKey, protocolConfig }).instruction()], [payer], { tables: [] });
  const treasuryBefore = await F.connection.getBalance(config.treasury);
  await F.send([create], [user], { tables: [] });
  assert.equal(BigInt((await F.connection.getBalance(config.treasury)) - treasuryBefore), BigInt(config.creationFeeLamports.toString()), "the creation fee went to the treasury");
  const components = weights.map(([mint, weight]) => ({ mint, unitsPerIndex: bn(BigInt(Math.round((nav * weight) / 10_000 / atomUsd(mint)))), targetWeightBps: weight, oraclePair: PublicKey.default }));
  await F.send([await F.program.methods.initializeLargeBasketComponentPage({ pageIndex: 0, startComponentIndex: 0, components })
    .accounts({ payer: user.publicKey, authority: user.publicKey, index: B.index, indexMint: B.indexMint, vaultAuthority: B.vaultAuthority, page: B.page, associatedTokenProgram: ASSOCIATED_TOKEN_PROGRAM_ID, systemProgram: SystemProgram.programId })
    .remainingAccounts(weights.flatMap(([mint]) => [meta(mint), meta(ata(mint, B.vaultAuthority), true), meta(TOKEN_PROGRAM_ID)])).instruction()], [user], { tables: [] });
  await F.send([await F.program.methods.finalizeLargeBasketConfig().accounts({ authority: user.publicKey, index: B.index }).remainingAccounts([meta(B.page, true)]).instruction()], [user], { tables: [] });
  const state = await F.program.account.indexState.fetch(B.index);
  assert.ok(state.community && state.largeBasketConfigured && state.feeRecipient.equals(config.treasury) && state.rebalanceKeeper.equals(config.keeper), "a community basket on the protocol's terms");
  await createAtas(F, [USDC], [config.treasury, B.creatorFeeRecipient]);
  await F.lookupTable([
    B.index, B.indexMint, B.vaultAuthority, B.page, B.lock, ata(B.indexMint, user.publicKey), ata(USDC, B.treasury), ata(USDC, B.creatorFeeRecipient),
    ...weights.flatMap(([mint]) => [mint, ata(mint, B.vaultAuthority), ata(mint, RESERVE), ata(mint, user.publicKey)]),
  ], { add: true });
  return B;
}

async function communityIntent(F, B, kind, amount) {
  const page = await F.program.account.largeBasketComponentPage.fetch(B.page);
  const list = page.components.map((c, i) => ({ ...c, index: page.startComponentIndex + i, page: B.page, symbol: symbol(c.mint) }));
  const supply = await mintSupply(F, B.indexMint);
  let usd = 0;
  for (const c of list) usd += (supply ? (Number(c.accountedReserve) * Number(amount)) / Number(supply) : (Number(c.unitsPerIndex) * Number(amount)) / 1e6) * atomUsd(c.mint);
  const quote = BigInt(Math.round(usd * 1e6));
  const nonce = nextNonce++;
  const intent = pda("large-basket-intent", B.index, user.publicKey, bn(nonce).toArrayLike(Buffer, "le", 8));
  const expiresAt = bn((await chainNow(F)) + 600);
  const ix = kind === "mint"
    ? await F.program.methods.openLargeBasketMintIntent({ indexAmountOut: bn(amount), maxQuoteIn: bn((quote * 105n) / 100n + 10_000n), nonce: bn(nonce), expiresAt, inKind: false })
      .accounts({ owner: user.publicKey, index: B.index, indexMint: B.indexMint, quoteMint: USDC, stakingPool, intent, intentLock: B.lock, systemProgram: SystemProgram.programId }).remainingAccounts([meta(B.page)]).instruction()
    : await F.program.methods.openLargeBasketRedeemIntent({ indexAmountIn: bn(amount), minQuoteOut: bn((quote * 97n) / 100n), nonce: bn(nonce), expiresAt, inKind: false })
      .accounts({ owner: user.publicKey, index: B.index, indexMint: B.indexMint, ownerIndexTokenAccount: ata(B.indexMint, user.publicKey), quoteMint: USDC, stakingPool, intent, intentLock: B.lock, tokenProgram: TOKEN_PROGRAM_ID, systemProgram: SystemProgram.programId }).remainingAccounts([meta(B.page, true)]).instruction();
  await F.send([ix], [user]);
  const state = await F.program.account.largeBasketIntent.fetch(intent);
  return { intent, keys: B, list, amounts: state.componentAmounts.map((a) => BigInt(a.toString())), quote };
}

// Each fee part is rounded up on its own, as the program charges it.
const feePart = (gross, bps) => (gross * BigInt(bps) + 9_999n) / 10_000n;

async function communityMint(F, B, amount) {
  const I = await communityIntent(F, B, "mint", amount);
  const ordered = [...I.list.filter((c) => !c.mint.equals(USDC)), ...I.list.filter((c) => c.mint.equals(USDC))];
  const watched = [userUsdc, ata(USDC, B.treasury), ata(USDC, B.creatorFeeRecipient), rewardVault];
  const before = await Promise.all(watched.map((a) => tokenAmount(F, a)));
  for (const group of batches(ordered.map((c) => mintEntry(I, c)))) {
    await F.send([await F.program.methods.executeLargeBasketMintBatch({ entries: group.map((g) => g.entry) }).accounts(communityAccounts(B, I.intent)).remainingAccounts(group.flatMap((g) => g.remaining)).instruction()], [user]);
  }
  const collect = await F.program.methods.collectLargeBasketIntentFees().accounts({ owner: user.publicKey, intent: I.intent, quoteMint: USDC, ownerQuoteTokenAccount: userUsdc, ...communityFees(B), tokenProgram: TOKEN_PROGRAM_ID }).instruction();
  const finalize = await F.program.methods.finalizeLargeBasketMintIntent().accounts({ owner: user.publicKey, index: B.index, indexMint: B.indexMint, vaultAuthority: B.vaultAuthority, intent: I.intent, intentLock: B.lock, ownerIndexTokenAccount: ata(B.indexMint, user.publicKey), associatedTokenProgram: ASSOCIATED_TOKEN_PROGRAM_ID, tokenProgram: TOKEN_PROGRAM_ID, systemProgram: SystemProgram.programId }).remainingAccounts([meta(B.page, true)]).instruction();
  const held = await tokenAmount(F, ata(B.indexMint, user.publicKey));
  await F.send([collect, finalize], [user]);
  const minted = (await tokenAmount(F, ata(B.indexMint, user.publicKey))) - held;
  assert.ok(minted >= BigInt(amount), "minted at least the requested amount");
  const [spent, protocolFee, creatorFee, stakingFee] = (await Promise.all(watched.map((a) => tokenAmount(F, a)))).map((a, i) => a - before[i]);
  // Community fees: one 0.10% step, split 4 protocol / 1 creator / 5 stakers.
  assert.ok(protocolFee > 0n && creatorFee > 0n && stakingFee > 0n, "every fee part paid on the mint");
  assert.ok(protocolFee >= 4n * creatorFee - 4n && protocolFee <= 4n * creatorFee + 4n && stakingFee >= 5n * creatorFee - 5n && stakingFee <= 5n * creatorFee + 5n, "the mint fee split 4/1/5");
  for (const c of (await F.program.account.largeBasketComponentPage.fetch(B.page)).components) {
    assert.ok((await tokenAmount(F, c.vault)) >= BigInt(c.accountedReserve.toString()), "each vault holds what the books say");
  }
  return { minted, spent: -spent, protocolFee, creatorFee, stakingFee };
}

async function communityRedeem(F, B, amount) {
  const I = await communityIntent(F, B, "redeem", amount);
  const watched = [userUsdc, ata(USDC, B.treasury), ata(USDC, B.creatorFeeRecipient), rewardVault];
  const before = await Promise.all(watched.map((a) => tokenAmount(F, a)));
  for (const group of batches(I.list.map((c) => redeemEntry(I, c)))) {
    await F.send([await F.program.methods.executeLargeBasketRedeemBatch({ entries: group.map((g) => g.entry) }).accounts({ ...communityAccounts(B, I.intent), ...communityFees(B), intentLock: B.lock }).remainingAccounts(group.flatMap((g) => g.remaining)).instruction()], [user]);
  }
  const settled = await F.program.account.largeBasketIntent.fetch(I.intent);
  assert.ok(settled.status.finalized, "the last leg finalized the redeem");
  const [proceeds, protocolFee, creatorFee, stakingFee] = (await Promise.all(watched.map((a) => tokenAmount(F, a)))).map((a, i) => a - before[i]);
  const gross = BigInt(settled.quoteAtomsExecuted.toString());
  assert.equal(protocolFee, feePart(gross, 4), "protocol fee");
  assert.equal(creatorFee, feePart(gross, 1), "creator fee");
  assert.equal(stakingFee, feePart(gross, 5), "staking fee");
  assert.equal(proceeds + protocolFee + creatorFee + stakingFee, gross, "the redeemer got the rest");
  return { gross, proceeds, protocolFee, creatorFee, stakingFee };
}

// Value shares of the components a rebalance could trade, against their targets rescaled over
// what's left once the excluded ones are set aside.
function activeDrift(state, excluded = []) {
  const active = state.components.filter((r) => !excluded.includes(r.symbol));
  const activeUsd = active.reduce((sum, r) => sum + r.usd, 0);
  const activeWeight = active.reduce((sum, r) => sum + r.targetWeightBps, 0);
  return Math.max(...active.filter((r) => r.targetWeightBps > 0).map((r) => Math.abs(Math.round((r.usd / activeUsd) * 10_000) - Math.round((r.targetWeightBps * 10_000) / activeWeight))));
}

async function communityState(F, B, label) {
  const page = await F.program.account.largeBasketComponentPage.fetch(B.page);
  const supply = await mintSupply(F, B.indexMint);
  const rows = [];
  for (const c of page.components) {
    const vault = await tokenAmount(F, c.vault);
    rows.push({ symbol: symbol(c.mint), vault: vault.toString(), accountedReserve: c.accountedReserve.toString(), unitsPerIndex: c.unitsPerIndex.toString(), targetWeightBps: c.targetWeightBps, usd: Number(vault) * atomUsd(c.mint) });
  }
  const total = rows.reduce((sum, r) => sum + r.usd, 0);
  rows.forEach((r) => (r.weightBps = total ? Math.round((r.usd / total) * 10_000) : 0));
  const state = { label, supply: supply.toString(), tvlUsd: total, navUsd: Number(supply) ? total / (Number(supply) / 1e6) : 0, lastRebalancedAt: Number((await F.program.account.indexState.fetch(B.index)).fixedWeightLastRebalancedAt), components: rows };
  report.states[label] = state;
  return state;
}

async function phase3(dir) {
  console.log("\nphase 3: a frozen component, a community basket and a price move");
  const v = await startValidator("3", NEW_BINARY, dir);
  const api = await startMockJupiterApi(v.connection);
  try {
    const F = makeFork(v.connection, v.rpcUrl);
    const keeper = keeperRunner(v, api);
    await userLookupTable(F);
    const xaut = (await components(F)).list.find((c) => c.symbol === "XAUt0");
    assert.equal((await F.connection.getAccountInfo(xaut.vault)).data[108], 2, "XAUt0's vault is frozen");

    // A stranger's community basket: half ORE, 30% cbBTC, 20% USDC, through the swap path.
    const B = await createCommunityBasket(F, "FRKC", [[ORE_MINT, 5000], [CBBTC, 3000], [USDC, 2000]]);
    step("community basket created by a whitelisted wallet, for the creation fee", { index: B.index.toBase58() });
    const minted = await communityMint(F, B, 50_000_000);
    step("community basket: swap mint of 50 FRKC (about $500)", minted);
    const redeemed = await communityRedeem(F, B, 10_000_000);
    step("community basket: swap redeem of 10 FRKC", redeemed);
    const communityStart = await communityState(F, B, "community basket after its first mint and redeem");
    assert.ok(activeDrift(communityStart) <= 50, "the first mint bought the target weights");
    // The keeper here signs as the protocol authority, so it becomes the basket's keeper, as the
    // protocol authority may set for a community basket.
    await F.send([await F.program.methods.updateCommunityBasket({ feeRecipient: B.treasury, keeper: payer.publicKey, maxSupply: bn(0), mintingPaused: false, rebalancingPaused: false }).accounts({ authority: payer.publicKey, protocolConfig, index: B.index }).instruction()], [payer], { tables: [] });

    // cbBTC rises 30%. The mock fills at the new price; the oracle's live DexScreener check
    // would still see the real one, so the keeper allows a wider gap from it here.
    PRICES.set(CBBTC.toBase58(), PRICES.get(CBBTC.toBase58()) * 1.3);
    report.measurements.phase3CbbtcPrice = PRICES.get(CBBTC.toBase58());
    const flags = ["--priority-fee", "50000", "--price-max-deviation-bps", "4000"];

    // SOV, due by time, with XAUt0 frozen: the keeper leaves XAUt0 out and trades the rest.
    const sovBefore = await basketState(F, "phase 3: cbBTC up 30%, XAUt0 frozen");
    printState(sovBefore);
    const lastBefore = sovBefore.lastRebalancedAt;
    const out = await keeper(SOV, flags);
    assert.ok(!/ERROR processing/.test(out), `SOV rebalance failed: ${out.slice(-3000)}`);
    assert.ok(/can't move \(VaultFrozen\): left out of rebalances/.test(out), "the keeper saw XAUt0 frozen");
    const sovAfter = await basketState(F, "phase 3: SOV rebalanced around frozen XAUt0");
    printState(sovAfter);
    assert.ok(sovAfter.lastRebalancedAt > lastBefore, "the SOV rebalance finished");
    const xBefore = sovBefore.components.find((r) => r.symbol === "XAUt0");
    const xAfter = sovAfter.components.find((r) => r.symbol === "XAUt0");
    assert.equal(xAfter.vault, xBefore.vault, "XAUt0's vault untouched");
    assert.equal(xAfter.accountedReserve, xBefore.accountedReserve, "XAUt0's books untouched");
    assert.ok(BigInt(sovAfter.components.find((r) => r.symbol === "cbBTC").accountedReserve) < BigInt(sovBefore.components.find((r) => r.symbol === "cbBTC").accountedReserve), "cbBTC sold");
    const sovDrift = { before: activeDrift(sovBefore, ["XAUt0"]), after: activeDrift(sovAfter, ["XAUt0"]) };
    assert.ok(sovDrift.after <= 150 && sovDrift.after < sovDrift.before, `the rest back on target: ${JSON.stringify(sovDrift)}`);
    const sovNavBps = Math.round((sovAfter.navUsd / sovBefore.navUsd - 1) * 10_000);
    assert.ok(Math.abs(sovNavBps) <= 100, `NAV moved ${sovNavBps} bps`);
    report.measurements.frozenRebalance = { driftBps: sovDrift, navChangeBps: sovNavBps };
    step("keeper rebalanced SOV around its frozen XAUt0 vault", report.measurements.frozenRebalance);

    // The community basket, off target by drift, rebalanced by the same keeper.
    const commBefore = await communityState(F, B, "community basket: cbBTC up 30%");
    const commOut = await keeper(B.index, flags);
    assert.ok(!/ERROR processing/.test(commOut), `community rebalance failed: ${commOut.slice(-3000)}`);
    assert.ok(/rebalance TRIGGERED \(drift=true/.test(commOut), "drift triggered the community rebalance");
    const commAfter = await communityState(F, B, "community basket rebalanced");
    assert.ok(commAfter.lastRebalancedAt > commBefore.lastRebalancedAt, "the community rebalance finished");
    const commDrift = { before: activeDrift(commBefore), after: activeDrift(commAfter) };
    assert.ok(commDrift.before >= 500 && commDrift.after <= 150, `community weights: ${JSON.stringify(commDrift)}`);
    const commNavBps = Math.round((commAfter.navUsd / commBefore.navUsd - 1) * 10_000);
    assert.ok(Math.abs(commNavBps) <= 100, `community NAV moved ${commNavBps} bps`);
    report.measurements.communityRebalance = { driftBps: commDrift, navChangeBps: commNavBps };
    step("keeper rebalanced the community basket by drift", report.measurements.communityRebalance);
    const commRedeem = await communityRedeem(F, B, 5_000_000);
    step("community basket: swap redeem of 5 FRKC after its rebalance", { usdc: commRedeem.proceeds });

    // An SOV swap redeem while XAUt0 can't move: the redeemer skips it, as the app offers, and
    // the other legs settle the redeem with XAUt0's share booked back to the basket.
    const R = await openIntent(F, "redeem", 1_000_000);
    const rx = R.list.find((c) => c.symbol === "XAUt0");
    const reserveAtOpen = BigInt((await components(F)).list.find((c) => c.symbol === "XAUt0").accountedReserve.toString());
    await F.send([await F.program.methods.skipLargeBasketRedeemComponent({ componentIndex: rx.index }).accounts({ owner: user.publicKey, index: SOV, intent: R.intent, intentLock, componentPage: rx.page, componentMint: rx.mint, componentVault: rx.vault, componentTokenProgram: rx.tokenProgram }).instruction()], [user]);
    const usdcBefore = await tokenAmount(F, userUsdc);
    for (const group of batches(R.list.filter((c) => c !== rx).map((c) => redeemEntry(R, c)))) {
      await F.send([await F.program.methods.executeLargeBasketRedeemBatch({ entries: group.map((g) => g.entry) }).accounts({ ...intentAccounts(R.intent), ...feeAccounts, intentLock }).remainingAccounts(group.flatMap((g) => g.remaining)).instruction()], [user]);
    }
    const skipped = await F.program.account.largeBasketIntent.fetch(R.intent);
    assert.ok(skipped.status.finalized && skipped.minQuoteOut.isZero(), "the redeem settled without XAUt0");
    const reserveAfter = BigInt((await components(F)).list.find((c) => c.symbol === "XAUt0").accountedReserve.toString());
    assert.equal(reserveAfter, reserveAtOpen + R.amounts[rx.index], "XAUt0's share went back into the books");
    assert.equal((await F.program.account.indexState.fetch(SOV)).openIntentCount, 0);
    step("SOV swap redeem of 1 SOV with frozen XAUt0 skipped", { usdc: (await tokenAmount(F, userUsdc)) - usdcBefore, xautBookedBack: R.amounts[rx.index] });
    printState(await basketState(F, "end of phase 3"));
  } finally {
    api.server.close();
    await v.stop();
    fs.rmSync(dir, { recursive: true, force: true });
  }
}

async function attacks(F) {
  const I = await openIntent(F, "mint", 1_000_000);
  const store = I.list.find((c) => c.mint.equals(STORE_MINT));
  const zec = I.list.find((c) => c.symbol === "ZEC");
  const userOre = ata(ORE_MINT, user.publicKey);
  const bought = async (destination) => {
    const { balance, supply } = await oreLstRate(F);
    const ore = ((I.amounts[store.index] * balance + supply - 1n) / supply) * 1001n / 1000n + 2n;
    return mockSwap({ authority: user.publicKey, source: userUsdc, destination, inputMint: USDC, outputMint: ORE_MINT, inAmount: exactOut(USDC, ORE_MINT, ore), outAmount: ore });
  };
  const tryMint = async (options, error, why) => F.rejects(F.send([(await wrappedMintIx(F, I, options.component ?? store, options)).ix], [options.signer ?? user]), error, why);
  await tryMint({ component: zec }, "WrappedComponentNotStore", "a wrapped leg for a component that is not stORE");
  await tryMint({ accounts: { oreLst: { ...oreLstAccounts, oreLstProgram: JUP6 } } }, "InvalidOreStakingAccount", "a stand-in for ORE's staking program");
  await tryMint({ accounts: { oreMint: USDC } }, "InvalidOreStakingAccount", "a mint other than ORE");
  const stray = Keypair.generate();
  await F.send([SystemProgram.createAccount({ fromPubkey: payer.publicKey, newAccountPubkey: stray.publicKey, lamports: 2_039_280, space: 165, programId: TOKEN_PROGRAM_ID }),
    (await import("@solana/spl-token")).createInitializeAccount3Instruction(stray.publicKey, ORE_MINT, user.publicKey)], [payer, stray], { tables: [] });
  await tryMint({ accounts: { ownerOreTokenAccount: stray.publicKey } }, "InvalidUserTokenAccount", "an ORE account that is not the owner's associated account");
  await tryMint({ swap: await bought(ata(ORE_MINT, thief.publicKey)) }, "InvalidJupiterRoute", "a route that pays the ORE to someone else");
  const intoVault = mockSwap({ authority: user.publicKey, source: userUsdc, destination: store.vault, inputMint: USDC, outputMint: STORE_MINT, inAmount: 1_000_000n, outAmount: I.amounts[store.index] });
  await tryMint({ swap: intoVault }, "InvalidJupiterRoute", "a route that touches the basket's vault");
  const asVault = await bought(userOre);
  asVault.keys[0] = meta(vaultAuthority, false, true);
  await tryMint({ swap: asVault }, "InvalidJupiterRoute", "a route signed as the vault authority");
  await tryMint({ maxQuoteIn: 1n }, "QuoteBudgetExceeded", "spending over the leg's budget");
  const other = Keypair.generate();
  await F.send([SystemProgram.transfer({ fromPubkey: payer.publicKey, toPubkey: other.publicKey, lamports: LAMPORTS_PER_SOL })], [payer], { tables: [] });
  await tryMint({ signer: other, accounts: { owner: other.publicKey, ownerQuoteTokenAccount: userUsdc, ownerOreTokenAccount: ata(ORE_MINT, other.publicKey), ownerStoreTokenAccount: ata(STORE_MINT, other.publicKey) } }, "InvalidLargeBasketIntent", "filling someone else's intent");
  const first = await F.send([(await wrappedMintIx(F, I, store)).ix], [user]);
  await tryMint({}, "LargeBasketComponentAlreadyFilled", "filling the stORE leg twice");
  step("wrapped mint leg after the refusals", { cu: first.cu });
  for (const group of batches(I.list.filter((c) => c !== store).map((c) => mintEntry(I, c)))) await F.send([await F.program.methods.executeLargeBasketMintBatch({ entries: group.map((g) => g.entry) }).accounts(intentAccounts(I.intent)).remainingAccounts(group.flatMap((g) => g.remaining)).instruction()], [user]);
  await F.send([
    await F.program.methods.collectLargeBasketIntentFees().accounts({ owner: user.publicKey, intent: I.intent, quoteMint: USDC, ownerQuoteTokenAccount: userUsdc, ...feeAccounts, tokenProgram: TOKEN_PROGRAM_ID }).instruction(),
    await F.program.methods.finalizeLargeBasketMintIntent().accounts({ owner: user.publicKey, index: SOV, indexMint, vaultAuthority, intent: I.intent, intentLock, ownerIndexTokenAccount: ata(indexMint, user.publicKey), associatedTokenProgram: ASSOCIATED_TOKEN_PROGRAM_ID, tokenProgram: TOKEN_PROGRAM_ID, systemProgram: SystemProgram.programId }).remainingAccounts(I.pages.map((p) => meta(p, true))).instruction(),
  ], [user]);

  // In kind takes no wrapped leg.
  const K = await openIntent(F, "mint", 1_000_000, { inKind: true });
  await F.rejects(F.send([(await wrappedMintIx(F, K, store)).ix], [user]), "InvalidLargeBasketIntent", "a wrapped leg on an in-kind mint");
  await F.send([await F.program.methods.cancelUnfilledLargeBasketMintIntent().accounts({ owner: user.publicKey, index: SOV, intent: K.intent, intentLock }).instruction()], [user]);

  const R = await openIntent(F, "redeem", 1_000_000);
  const rstore = R.list.find((c) => c.mint.equals(STORE_MINT));
  const tryRedeem = async (options, error, why) => F.rejects(F.send([(await wrappedRedeemIx(F, R, rstore, options)).ix], [user]), error, why);
  await tryRedeem({ sellFraction: 0.5 }, "UnwrappedOreNotSold", "selling half the unwrapped ORE");
  await tryRedeem({ minQuoteOut: 10n ** 12n }, "QuoteBudgetExceeded", "proceeds under the leg's minimum");
  await tryRedeem({ accounts: { feeRecipientQuoteTokenAccount: userUsdc } }, "InvalidFeeRecipientTokenAccount", "routing the protocol's fee to the redeemer");
  await tryRedeem({ accounts: { ownerStoreTokenAccount: ata(STORE_MINT, thief.publicKey) } }, "InvalidUserTokenAccount", "paying the leg's stORE to someone else");
  for (const group of batches(R.list.filter((c) => c !== rstore).map((c) => redeemEntry(R, c)))) await F.send([await F.program.methods.executeLargeBasketRedeemBatch({ entries: group.map((g) => g.entry) }).accounts({ ...intentAccounts(R.intent), ...feeAccounts, intentLock }).remainingAccounts(group.flatMap((g) => g.remaining)).instruction()], [user]);
  await F.send([(await wrappedRedeemIx(F, R, rstore)).ix], [user]);
  assert.ok((await F.program.account.largeBasketIntent.fetch(R.intent)).status.finalized, "the wrapped leg finalized the redeem as the last leg");
  step("a wrapped redeem leg that comes last finalizes the redeem");
  assert.equal((await F.program.account.indexState.fetch(SOV)).openIntentCount, 0);
}

async function main() {
  for (const file of [NEW_BINARY, MOCK_JUPITER]) assert.ok(fs.existsSync(file), `missing ${file}`);
  PRICES = await livePrices([...new Set([...manifest.basket.components.map((c) => c.mint), ORE_MINT.toBase58(), STORE_MINT.toBase58()])]);
  report.prices = Object.fromEntries([...PRICES].map(([m, p]) => [symbol(m), p]));
  console.log("live prices:", report.prices);
  const next = await phase1();
  const third = await phase2(next);
  await phase3(third);
  report.finishedAt = new Date().toISOString();
  report.passed = true;
  const out = path.join(root, ".anchor", "wrapped-ore-fork-report.json");
  fs.writeFileSync(out, JSON.stringify(report, (_, v) => (typeof v === "bigint" ? v.toString() : v), 2));
  console.log(`\nwrapped-ORE fork rehearsal passed (${report.steps.length} checks); report in ${path.relative(root, out)}`);
}

main().catch((error) => {
  report.passed = false;
  report.error = error.stack ?? String(error);
  fs.writeFileSync(path.join(root, ".anchor", "wrapped-ore-fork-report.json"), JSON.stringify(report, (_, v) => (typeof v === "bigint" ? v.toString() : v), 2));
  console.error(error);
  process.exit(1);
});
