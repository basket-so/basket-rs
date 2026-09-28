import { prepareRebalanceFixtures, testRebalanceMigration } from "./rebalance-migration-fixtures.mjs";
import assert from "node:assert/strict";
import { spawn, spawnSync } from "node:child_process";
import fs from "node:fs";
import net from "node:net";
import path from "node:path";
import { fileURLToPath } from "node:url";

import anchor from "@coral-xyz/anchor";
import {
  ASSOCIATED_TOKEN_PROGRAM_ID,
  createMint,
  getAccount,
  getAssociatedTokenAddressSync,
  getMint,
  getOrCreateAssociatedTokenAccount,
  MintLayout,
  mintTo,
  TOKEN_PROGRAM_ID,
} from "@solana/spl-token";
import {
  Keypair,
  LAMPORTS_PER_SOL,
  PublicKey,
  sendAndConfirmTransaction,
  SystemProgram,
  Transaction,
} from "@solana/web3.js";

const __dirname = path.dirname(fileURLToPath(import.meta.url));
const root = path.resolve(__dirname, "..");
const programId = new PublicKey("bskthjNMRWQ4ekDLxaAzA1e39ThPmEtUgHY3XHfs7qv");
const basketMint = new PublicKey("2rNBaMg5VAr1aMNCwAPdDZVgzzdTaNDebUnNqPFNmeta");
const usdcMint = new PublicKey("EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v");
const bpfLoaderUpgradeable = new PublicKey(
  "BPFLoaderUpgradeab1e11111111111111111111111",
);

const exe = (name) => (process.platform === "win32" ? `${name}.exe` : name);

function localSolanaBinary(name) {
  const candidate = path.join(
    root,
    ".agave-2.3",
    "releases",
    "2.3.0",
    "solana-release",
    "bin",
    exe(name),
  );
  return fs.existsSync(candidate) ? candidate : name;
}

const validator =
  process.env.BASKET_VALIDATOR_BIN ?? localSolanaBinary("solana-test-validator");

function keypairFromFile(file) {
  return Keypair.fromSecretKey(
    Uint8Array.from(JSON.parse(fs.readFileSync(path.join(root, file), "utf8"))),
  );
}

function writeMintAccountDump(file, pubkey, decimals) {
  const data = Buffer.alloc(MintLayout.span);
  MintLayout.encode(
    {
      mintAuthorityOption: 1,
      mintAuthority: keypairFromFile("deployer-keypair.json").publicKey,
      supply: 0n,
      decimals,
      isInitialized: true,
      freezeAuthorityOption: 0,
      freezeAuthority: PublicKey.default,
    },
    data,
  );

  fs.writeFileSync(
    file,
    JSON.stringify(
      {
        pubkey: pubkey.toBase58(),
        account: {
          lamports: 1_461_600,
          data: [data.toString("base64"), "base64"],
          owner: TOKEN_PROGRAM_ID.toBase58(),
          executable: false,
          rentEpoch: 0,
          space: MintLayout.span,
        },
      },
      null,
      2,
    ),
  );
}

async function freePort() {
  return await new Promise((resolve, reject) => {
    const server = net.createServer();
    server.listen(0, "127.0.0.1", () => {
      const { port } = server.address();
      server.close(() => resolve(port));
    });
    server.on("error", reject);
  });
}

async function portIsFree(port) {
  return await new Promise((resolve) => {
    const server = net.createServer();
    server.once("error", () => resolve(false));
    server.listen(port, "127.0.0.1", () => {
      server.close(() => resolve(true));
    });
  });
}

async function freeRpcPortPair() {
  for (let attempt = 0; attempt < 50; attempt += 1) {
    const rpcPort = await freePort();
    if (rpcPort < 65_535 && (await portIsFree(rpcPort + 1))) {
      return rpcPort;
    }
  }

  throw new Error("Could not find a free adjacent RPC/WebSocket port pair");
}

async function freePortOutside(excluded) {
  for (let attempt = 0; attempt < 50; attempt += 1) {
    const port = await freePort();
    if (!excluded.has(port)) {
      return port;
    }
  }

  throw new Error("Could not find a free distinct port");
}

function validatorStartupError(output) {
  if (/code:\s*1314|required privilege/i.test(output)) {
    return [
      "solana-test-validator exited before RPC became ready.",
      "On this Windows host it failed with privilege error 1314.",
      "Run the test from an elevated shell, enable Developer Mode/symlink privileges,",
      "or point BASKET_VALIDATOR_BIN at a validator binary that can start in this environment.",
    ].join(" ");
  }

  return [
    "solana-test-validator exited before RPC became ready.",
    output.trim(),
  ]
    .filter(Boolean)
    .join("\n");
}

async function waitForRpc(connection, validatorProcess, output) {
  let lastError;
  for (let attempt = 0; attempt < 80; attempt += 1) {
    if (validatorProcess.exitCode !== null) {
      throw new Error(validatorStartupError(output()));
    }

    try {
      await connection.getLatestBlockhash("confirmed");
      return;
    } catch (error) {
      lastError = error;
      await new Promise((resolve) => setTimeout(resolve, 500));
    }
  }

  throw lastError ?? new Error("Timed out waiting for validator RPC");
}

async function transferSol(connection, from, to, sol) {
  const transaction = new Transaction().add(
    SystemProgram.transfer({
      fromPubkey: from.publicKey,
      toPubkey: to,
      lamports: sol * LAMPORTS_PER_SOL,
    }),
  );
  await sendAndConfirmTransaction(connection, transaction, [from], {
    commitment: "confirmed",
  });
}

function sleep(ms) {
  return new Promise((resolve) => setTimeout(resolve, ms));
}

function processHasExited(child) {
  return child.exitCode !== null || child.signalCode !== null;
}

async function waitForProcessClose(child, timeoutMs) {
  if (processHasExited(child)) {
    return;
  }

  await new Promise((resolve) => {
    let settled = false;
    const finish = () => {
      if (settled) {
        return;
      }
      settled = true;
      clearTimeout(timeout);
      child.off("close", finish);
      resolve();
    };
    const timeout = setTimeout(finish, timeoutMs);
    child.once("close", finish);
  });
}

async function stopValidator(child) {
  if (processHasExited(child) || child.pid === undefined) {
    return;
  }

  if (process.platform === "win32") {
    spawnSync("taskkill", ["/pid", String(child.pid), "/T", "/F"], {
      stdio: "ignore",
      windowsHide: true,
    });
  } else {
    child.kill("SIGTERM");
  }

  await waitForProcessClose(child, 5_000);

  if (!processHasExited(child) && process.platform !== "win32") {
    child.kill("SIGKILL");
    await waitForProcessClose(child, 2_000);
  }
}

function closeRpcWebSocket(connection) {
  try {
    connection._rpcWebSocket?.close();
  } catch (error) {
    console.warn(`warning: could not close RPC websocket: ${error.message}`);
  }
}

async function removePathWithRetries(target) {
  if (!path.resolve(target).startsWith(path.resolve(root, ".anchor") + path.sep)) throw new Error("Unsafe cleanup path");
  for (let attempt = 0; attempt < 12; attempt += 1) {
    try {
      fs.rmSync(target, { recursive: true, force: true });
      return;
    } catch (error) {
      const retryable = ["EBUSY", "ENOTEMPTY", "EPERM"].includes(error.code);
      if (!retryable || attempt === 11) {
        console.warn(`warning: could not clean up ${target}: ${error.message}`);
        return;
      }
      await sleep(250 * (attempt + 1));
    }
  }
}

async function main() {
  const rpcPort = await freeRpcPortPair();
  const wsPort = rpcPort + 1;
  const faucetPort = await freePortOutside(new Set([rpcPort, wsPort]));
  const rpcUrl = `http://127.0.0.1:${rpcPort}`;
  const wsUrl = `ws://127.0.0.1:${wsPort}`;
  const ledgerDir = path.join(root, ".anchor", `integration-ledger-${Date.now()}`);
  const fixtureDir = path.join(root, ".anchor", `integration-fixtures-${Date.now()}`);
  fs.mkdirSync(fixtureDir, { recursive: true });
  const basketMintDump = path.join(fixtureDir, "basket-mint.json");
  const usdcMintDump = path.join(fixtureDir, "usdc-mint.json");
  writeMintAccountDump(basketMintDump, basketMint, 6);
  writeMintAccountDump(usdcMintDump, usdcMint, 6);
  const payer = keypairFromFile("deployer-keypair.json");

  const rebalanceFixtures = await prepareRebalanceFixtures(fixtureDir, programId, payer.publicKey);
  const validatorArgs = [
      "--reset",
      "--quiet",
      "--mint",
      payer.publicKey.toBase58(),
      "--upgradeable-program",
      programId.toBase58(),
      path.join(root, "target", "deploy", "basket.so"),
      payer.publicKey.toBase58(),
      "--ledger",
      ledgerDir,
      "--rpc-port",
      String(rpcPort),
      "--faucet-port",
      String(faucetPort),
      "--account",
      basketMint.toBase58(),
      basketMintDump,
      "--account",
      usdcMint.toBase58(),
      usdcMintDump,
    ];
  validatorArgs.push(...rebalanceFixtures.validatorArgs);
  const wslPath = p => p.replace(/^([A-Za-z]):/, (_, drive) => '/mnt/' + drive.toLowerCase()).replaceAll('\\','/');
  const validatorProcess = spawn(process.env.BASKET_WSL ? 'wsl.exe' : validator,
    process.env.BASKET_WSL ? ['-d','Ubuntu','--','/home/gainsu/.cache/basket-validator/solana-release/bin/solana-test-validator', ...validatorArgs.map(wslPath)] : validatorArgs,
    {
      cwd: root,
      stdio: ["ignore", "pipe", "pipe"],
      windowsHide: true,
    },
  );

  let validatorOutput = "";
  validatorProcess.stdout.on("data", (data) => {
    validatorOutput += data.toString();
  });
  validatorProcess.stderr.on("data", (data) => {
    validatorOutput += data.toString();
  });

  const user = Keypair.generate();
  const connection = new anchor.web3.Connection(rpcUrl, {
    commitment: "confirmed",
    wsEndpoint: wsUrl,
  });
  const provider = new anchor.AnchorProvider(
    connection,
    new anchor.Wallet(payer),
    { commitment: "confirmed" },
  );
  anchor.setProvider(provider);

  try {
    await waitForRpc(connection, validatorProcess, () => validatorOutput);
    const programAccount = await connection.getAccountInfo(programId, "confirmed");
    assert.ok(programAccount?.executable, `${programId.toBase58()} was not loaded into the validator`);
    await transferSol(connection, payer, user.publicKey, 5);

    const idl = JSON.parse(fs.readFileSync(path.join(root, "target/idl/basket.json"), "utf8"));
    const program = new anchor.Program(idl, provider);

    const [protocolConfig] = PublicKey.findProgramAddressSync(
      [Buffer.from("protocol-config")],
      programId,
    );
    const [programData] = PublicKey.findProgramAddressSync(
      [programId.toBuffer()],
      bpfLoaderUpgradeable,
    );

    await program.methods
      .initializeProtocol({ indexCreator: payer.publicKey })
      .accounts({
        payer: payer.publicKey,
        authority: payer.publicKey,
        program: programId,
        programData,
        protocolConfig,
        systemProgram: SystemProgram.programId,
      })
      .rpc();

    const protocol = await program.account.protocolConfig.fetch(protocolConfig);
    assert.equal(protocol.authority.toBase58(), payer.publicKey.toBase58());
    assert.equal(protocol.indexCreator.toBase58(), payer.publicKey.toBase58());
    assert.equal(protocol.permissionlessIndexCreation, false);

    const [stakingPool] = PublicKey.findProgramAddressSync(
      [Buffer.from("staking-pool")],
      programId,
    );
    const [stakingAuthority] = PublicKey.findProgramAddressSync(
      [Buffer.from("staking-authority")],
      programId,
    );
    const stakeVault = getAssociatedTokenAddressSync(
      basketMint,
      stakingAuthority,
      true,
      TOKEN_PROGRAM_ID,
      ASSOCIATED_TOKEN_PROGRAM_ID,
    );
    const rewardVault = getAssociatedTokenAddressSync(
      usdcMint,
      stakingAuthority,
      true,
      TOKEN_PROGRAM_ID,
      ASSOCIATED_TOKEN_PROGRAM_ID,
    );

    await program.methods
      .initializeStakingPool()
      .accounts({
        payer: payer.publicKey,
        authority: payer.publicKey,
        protocolConfig,
        stakingPool,
        stakingAuthority,
        basketMint,
        rewardMint: usdcMint,
        stakeVault,
        rewardVault,
        associatedTokenProgram: ASSOCIATED_TOKEN_PROGRAM_ID,
        tokenProgram: TOKEN_PROGRAM_ID,
        systemProgram: SystemProgram.programId,
      })
      .rpc();

    const staking = await program.account.stakingPool.fetch(stakingPool);
    assert.equal(staking.authority.toBase58(), payer.publicKey.toBase58());
    assert.equal(staking.basketMint.toBase58(), basketMint.toBase58());
    assert.equal(staking.rewardMint.toBase58(), usdcMint.toBase58());

    const pda = (...seeds) => PublicKey.findProgramAddressSync(seeds.map(s => typeof s === 'string' ? Buffer.from(s) : s.toBuffer ? s.toBuffer() : s), programId)[0];
    const bn = n => new anchor.BN(n);
    const meta = (pubkey, isWritable = false) => ({ pubkey, isWritable, isSigner: false });
    const userBasket = await getOrCreateAssociatedTokenAccount(connection, payer, basketMint, payer.publicKey);
    const userUsdc = await getOrCreateAssociatedTokenAccount(connection, payer, usdcMint, payer.publicKey);
    await mintTo(connection, payer, basketMint, userBasket.address, payer, 10_000_000);
    await mintTo(connection, payer, usdcMint, userUsdc.address, payer, 10_000_000);
    const stakePosition = pda('stake-position', stakingPool, payer.publicKey);
    const stakeAccounts = { owner: payer.publicKey, stakingPool, stakingAuthority, stakePosition, basketMint, ownerBasketTokenAccount: userBasket.address, stakeVault, tokenProgram: TOKEN_PROGRAM_ID, systemProgram: SystemProgram.programId };
    await program.methods.stakeBasket({ amount: bn(2_000_000) }).accounts(stakeAccounts).rpc();
    assert.equal((await getAccount(connection, stakeVault)).amount, 2_000_000n);
    await program.methods.fundStakingRewards({ amount: bn(1_000_000) }).accounts({ funder: payer.publicKey, stakingPool, stakingAuthority, funderRewardTokenAccount: userUsdc.address, rewardVault, tokenProgram: TOKEN_PROGRAM_ID }).rpc();
    await program.methods.claimStakingRewards().accounts({ owner: payer.publicKey, stakingPool, stakingAuthority, stakePosition, rewardMint: usdcMint, ownerRewardTokenAccount: userUsdc.address, rewardVault, associatedTokenProgram: ASSOCIATED_TOKEN_PROGRAM_ID, tokenProgram: TOKEN_PROGRAM_ID, systemProgram: SystemProgram.programId }).rpc();
    assert.equal((await getAccount(connection, userUsdc.address)).amount, 10_000_000n);
    await program.methods.unstakeBasket({ amount: bn(2_000_000) }).accounts(stakeAccounts).rpc();
    assert.equal((await getAccount(connection, stakeVault)).amount, 0n);

    await testRebalanceMigration(program, connection, payer, stakingPool, rebalanceFixtures.fixtures);
    for (const fixedWeights of [false, true]) {
      const symbol = fixedWeights ? 'FIXED' : 'UNITS';
      const index = pda('index', payer.publicKey, symbol);
      const indexMint = pda('index-mint', index);
      const vaultAuthority = pda('vault-authority', index);
      const page = pda('large-basket-component-page', index, Buffer.from([0]));
      const components = [await createMint(connection, payer, payer.publicKey, null, 6), usdcMint];
      const vaults = components.map(m => getAssociatedTokenAddressSync(m, vaultAuthority, true));
      const owners = await Promise.all(components.map(m => getOrCreateAssociatedTokenAccount(connection, payer, m, payer.publicKey)));
      await mintTo(connection, payer, components[0], owners[0].address, payer, 10_000_000);
      await program.methods.createLargeBasketIndex({ name: symbol + ' test', symbol, metadataUri: '', decimals: 6, feeRecipient: payer.publicKey, creatorFeeRecipient: PublicKey.default, maxSupply: bn(10_000_000), rebalanceDelaySeconds: bn(0), kind: fixedWeights ? { fixedWeights: {} } : { fixedUnits: {} }, fixedWeightQuoteMint: fixedWeights ? usdcMint : PublicKey.default, fixedWeightRebalanceIntervalSeconds: bn(fixedWeights ? 86400 : 0), fixedWeightDriftThresholdBps: fixedWeights ? 500 : 0, fixedWeightSpotEmaMaxDeviationBps: fixedWeights ? 500 : 0, componentCount: 2 }).accounts({ payer: payer.publicKey, authority: payer.publicKey, protocolConfig, index, indexMint, vaultAuthority, tokenProgram: TOKEN_PROGRAM_ID, systemProgram: SystemProgram.programId }).rpc();
      await program.methods.initializeLargeBasketComponentPage({ pageIndex: 0, startComponentIndex: 0, components: components.map(mint => ({ mint, unitsPerIndex: bn(fixedWeights && mint.equals(usdcMint) ? 0 : 500_000), targetWeightBps: fixedWeights && !mint.equals(usdcMint) ? 10000 : 0, oraclePair: mint.equals(usdcMint) ? PublicKey.default : Keypair.generate().publicKey })) }).accounts({ payer: payer.publicKey, authority: payer.publicKey, index, indexMint, vaultAuthority, page, associatedTokenProgram: ASSOCIATED_TOKEN_PROGRAM_ID, systemProgram: SystemProgram.programId }).remainingAccounts(components.flatMap((mint,i) => [meta(mint), meta(vaults[i],true), meta(TOKEN_PROGRAM_ID)])).rpc();
      await program.methods.finalizeLargeBasketConfig().accounts({ authority: payer.publicKey, index }).remainingAccounts([meta(page,true)]).rpc();
      const ownerIndexTokenAccount = getAssociatedTokenAddressSync(indexMint, payer.publicKey);
      const intentLock = pda('large-basket-intent-lock', index, payer.publicKey);
      const common = { owner: payer.publicKey, index, indexMint, stakingPool, quoteMint: usdcMint, vaultAuthority, intentLock, ownerIndexTokenAccount, tokenProgram: TOKEN_PROGRAM_ID, systemProgram: SystemProgram.programId };
      let nonce = 0;
      async function operation(kind, amount, cancel = false) {
        const intent = pda('large-basket-intent', index, payer.publicKey, bn(++nonce).toArrayLike(Buffer, 'le', 8));
        const args = { nonce: bn(nonce), expiresAt: bn(Math.floor(Date.now()/1000)+600), inKind: true, ...(kind === 'mint' ? { indexAmountOut: bn(amount), maxQuoteIn: bn(0) } : { indexAmountIn: bn(amount), minQuoteOut: bn(0) }) };
        await program.methods[kind === 'mint' ? 'openLargeBasketMintIntent' : 'openLargeBasketRedeemIntent'](args).accounts({ ...common, intent }).remainingAccounts([meta(page,kind === 'redeem')]).rpc();
        if (cancel) {
          await program.methods[kind === 'mint' ? 'cancelUnfilledLargeBasketMintIntent' : 'cancelUnfilledLargeBasketRedeemIntent']().accounts({ ...common, intent }).remainingAccounts(kind === 'redeem' ? [meta(page,true)] : []).rpc();
          return;
        }
        for (let i = 0; i < components.length; i++) {
          await program.methods[kind === 'mint' ? 'executeLargeBasketMintComponentInKind' : 'executeLargeBasketRedeemComponentInKind']({ componentIndex: i }).accounts({ ...common, intent, componentPage: page, componentMint: components[i], componentVault: vaults[i], componentTokenProgram: TOKEN_PROGRAM_ID, ownerComponentTokenAccount: owners[i].address, protocolFeeComponentAccount: owners[i].address, creatorFeeComponentAccount: owners[i].address, associatedTokenProgram: ASSOCIATED_TOKEN_PROGRAM_ID }).rpc();
        }
        await program.methods[kind === 'mint' ? 'finalizeLargeBasketMintIntent' : 'finalizeLargeBasketRedeemIntent']().accounts({ ...common, intent, associatedTokenProgram: ASSOCIATED_TOKEN_PROGRAM_ID }).remainingAccounts(kind === 'mint' ? [meta(page,true)] : []).rpc();
      }
      await operation('mint', 1_000_000);
      await operation('mint', 1_000_000);
      assert.equal((await getMint(connection, indexMint)).supply, 2_000_000n);
      for (const [i,v] of vaults.entries()) assert.equal((await getAccount(connection,v)).amount, fixedWeights && components[i].equals(usdcMint) ? 0n : 1_000_000n);
      await operation('mint', 1_000_000, true);
      await operation('redeem', 1_000_000, true);
      assert.equal((await getMint(connection,indexMint)).supply, 2_000_000n);
      await operation('redeem', 2_000_000);
      assert.equal((await getMint(connection,indexMint)).supply, 0n);
      for (const v of vaults) assert.equal((await getAccount(connection,v)).amount, 0n);
      assert.equal((await program.account.indexState.fetch(index)).largeBasketOperationInProgress, false);
    }
    // Exercise the exact swap-path batch/collect/finalize interface used by the UI,
    // with a USDC component so this test needs no external liquidity or oracle.
    {
      const index = pda('index', payer.publicKey, 'CASH');
      const indexMint = pda('index-mint', index);
      const vaultAuthority = pda('vault-authority', index);
      const page = pda('large-basket-component-page', index, Buffer.from([0]));
      const vault = getAssociatedTokenAddressSync(usdcMint, vaultAuthority, true);
      await program.methods.createLargeBasketIndex({ name: 'Cash lifecycle', symbol: 'CASH', metadataUri: '', decimals: 6, feeRecipient: payer.publicKey, creatorFeeRecipient: PublicKey.default, maxSupply: bn(2_000_000), rebalanceDelaySeconds: bn(0), kind: { fixedUnits: {} }, fixedWeightQuoteMint: PublicKey.default, fixedWeightRebalanceIntervalSeconds: bn(0), fixedWeightDriftThresholdBps: 0, fixedWeightSpotEmaMaxDeviationBps: 0, componentCount: 1 }).accounts({ payer: payer.publicKey, authority: payer.publicKey, protocolConfig, index, indexMint, vaultAuthority, tokenProgram: TOKEN_PROGRAM_ID, systemProgram: SystemProgram.programId }).rpc();
      await program.methods.initializeLargeBasketComponentPage({ pageIndex: 0, startComponentIndex: 0, components: [{ mint: usdcMint, unitsPerIndex: bn(1_000_000), targetWeightBps: 0, oraclePair: PublicKey.default }] }).accounts({ payer: payer.publicKey, authority: payer.publicKey, index, indexMint, vaultAuthority, page, associatedTokenProgram: ASSOCIATED_TOKEN_PROGRAM_ID, systemProgram: SystemProgram.programId }).remainingAccounts([meta(usdcMint),meta(vault,true),meta(TOKEN_PROGRAM_ID)]).rpc();
      await program.methods.finalizeLargeBasketConfig().accounts({ authority: payer.publicKey,index }).remainingAccounts([meta(page,true)]).rpc();
      await program.methods.updateFees({ mintFeeBps: 10, redeemFeeBps: 10, creatorMintFeeBps: 0, creatorRedeemFeeBps: 0, stakingMintFeeBps: 10, stakingRedeemFeeBps: 10 }).accounts({ authority: payer.publicKey,index }).rpc();
      const ownerIndexTokenAccount = getAssociatedTokenAddressSync(indexMint,payer.publicKey);
      const intentLock = pda('large-basket-intent-lock',index,payer.publicKey);
      const common = { owner: payer.publicKey,index,indexMint,stakingPool,stakingAuthority,quoteMint: usdcMint,vaultAuthority,intentLock,ownerIndexTokenAccount,ownerQuoteTokenAccount:userUsdc.address,quoteTokenProgram:TOKEN_PROGRAM_ID,tokenProgram:TOKEN_PROGRAM_ID,systemProgram:SystemProgram.programId,associatedTokenProgram:ASSOCIATED_TOKEN_PROGRAM_ID };
      for (const [n,kind] of ['mint','redeem'].entries()) {
        const nonce = bn(n+1);
        const intent = pda('large-basket-intent',index,payer.publicKey,nonce.toArrayLike(Buffer,'le',8));
        const args = { nonce,expiresAt:bn(Math.floor(Date.now()/1000)+600),inKind:false,...(kind==='mint'?{indexAmountOut:bn(1_000_000),maxQuoteIn:bn(1_002_000)}:{indexAmountIn:bn(1_000_000),minQuoteOut:bn(998_000)}) };
        if (kind === 'mint') await assert.rejects(program.methods.openLargeBasketMintIntent({...args,indexAmountOut:bn(3_000_000)}).accounts({...common,intent}).remainingAccounts([meta(page)]).rpc(), /SupplyCapExceeded|0x177c/);
        await program.methods[kind==='mint'?'openLargeBasketMintIntent':'openLargeBasketRedeemIntent'](args).accounts({...common,intent}).remainingAccounts([meta(page,kind==='redeem')]).rpc();
        const entries = [{ componentIndex:0,routeAccountCount:0,swap:null,...(kind==='mint'?{maxQuoteIn:bn(1_000_000)}:{minQuoteOut:bn(1_000_000)}) }];
        await program.methods[kind==='mint'?'executeLargeBasketMintBatch':'executeLargeBasketRedeemBatch']({entries}).accounts({...common,intent,jupiterProgram:new PublicKey('JUP6LkbZbjS1jKKwapdHNy74zcZ3tLUZoi5QNyVTaV4')}).remainingAccounts([meta(page),meta(usdcMint),meta(vault,true),meta(TOKEN_PROGRAM_ID)]).rpc();
        await program.methods.collectLargeBasketIntentFees().accounts({...common,intent,feeRecipientQuoteTokenAccount:userUsdc.address,creatorFeeRecipientQuoteTokenAccount:userUsdc.address,stakingRewardVault:rewardVault}).rpc();
        await program.methods[kind==='mint'?'finalizeLargeBasketMintIntent':'finalizeLargeBasketRedeemIntent']().accounts({...common,intent}).remainingAccounts(kind==='mint'?[meta(page,true)]:[]).rpc();
      }
      assert.equal((await getMint(connection,indexMint)).supply,0n);
      assert.equal((await getAccount(connection,vault)).amount,0n);
      assert.equal((await getAccount(connection,rewardVault)).amount,2_000n);
    }
    console.log('integration ok: six-decimal staking, rewards, unstaking; fixed-unit and fixed-weight paged creation, initial/pro-rata mint, cancellation, complete redemption; swap-path batch and USDC fee collection');
  } catch (error) {
    if (!/privilege error 1314/i.test(error.message)) {
      console.error(validatorOutput);
    }
    throw error;
  } finally {
    closeRpcWebSocket(connection);
    await stopValidator(validatorProcess);
    await removePathWithRetries(ledgerDir);
    await removePathWithRetries(fixtureDir);
  }
}

await main();
