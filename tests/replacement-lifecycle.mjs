import { prepareRebalanceFixtures, testRebalanceMigration } from "./rebalance-migration-fixtures.mjs";
import { prepareCompositionFixtures, testCompositionChange } from "./composition-change-fixtures.mjs";
import { ACCOUNT_LOCK_LIMIT_128_FEATURE, prepareSignedPriceFixtures, testSignedPrices } from "./signed-prices-fixtures.mjs";
import assert from "node:assert/strict";
import { spawn, spawnSync } from "node:child_process";
import fs from "node:fs";
import net from "node:net";
import path from "node:path";
import { fileURLToPath } from "node:url";

import anchor from "@coral-xyz/anchor";
import {
  ASSOCIATED_TOKEN_PROGRAM_ID,
  createInitializeMintInstruction,
  createInitializeTransferFeeConfigInstruction,
  createMint,
  ExtensionType,
  getAccount,
  getAssociatedTokenAddressSync,
  getMint,
  getMintLen,
  getOrCreateAssociatedTokenAccount,
  MintLayout,
  mintTo,
  TOKEN_2022_PROGRAM_ID,
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
  const compositionFixtures = await prepareCompositionFixtures(fixtureDir, programId, payer.publicKey);
  const signedPriceFixtures = await prepareSignedPriceFixtures(fixtureDir, programId, payer.publicKey);
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
  validatorArgs.push(...rebalanceFixtures.validatorArgs, ...compositionFixtures.validatorArgs, ...signedPriceFixtures.validatorArgs);
  // Enforce mainnet's 64 accounts per transaction (the 128 feature is inactive there).
  validatorArgs.push("--deactivate-feature", ACCOUNT_LOCK_LIMIT_128_FEATURE);
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
    await testCompositionChange(program, connection, payer, user, stakingPool, compositionFixtures.fixtures);
    await testSignedPrices(program, connection, payer, user, signedPriceFixtures.fixtures);
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
      const createIndex = (componentCount) => program.methods.createLargeBasketIndex({ name: symbol + ' test', symbol, metadataUri: '', decimals: 6, feeRecipient: payer.publicKey, creatorFeeRecipient: PublicKey.default, maxSupply: bn(10_000_000), rebalanceDelaySeconds: bn(0), kind: fixedWeights ? { fixedWeights: {} } : { fixedUnits: {} }, fixedWeightQuoteMint: fixedWeights ? usdcMint : PublicKey.default, fixedWeightRebalanceIntervalSeconds: bn(fixedWeights ? 86400 : 0), fixedWeightDriftThresholdBps: fixedWeights ? 500 : 0, fixedWeightSpotEmaMaxDeviationBps: fixedWeights ? 500 : 0, componentCount }).accounts({ payer: payer.publicKey, authority: payer.publicKey, protocolConfig, index, indexMint, vaultAuthority, tokenProgram: TOKEN_PROGRAM_ID, systemProgram: SystemProgram.programId }).rpc();
      // Every basket kind is capped at 40 components.
      await assert.rejects(createIndex(41), /InvalidComponentCount/);
      await createIndex(2);
      await program.methods.initializeLargeBasketComponentPage({ pageIndex: 0, startComponentIndex: 0, components: components.map(mint => ({ mint, unitsPerIndex: bn(fixedWeights && mint.equals(usdcMint) ? 0 : 500_000), targetWeightBps: fixedWeights && !mint.equals(usdcMint) ? 10000 : 0, oraclePair: mint.equals(usdcMint) ? PublicKey.default : Keypair.generate().publicKey })) }).accounts({ payer: payer.publicKey, authority: payer.publicKey, index, indexMint, vaultAuthority, page, associatedTokenProgram: ASSOCIATED_TOKEN_PROGRAM_ID, systemProgram: SystemProgram.programId }).remainingAccounts(components.flatMap((mint,i) => [meta(mint), meta(vaults[i],true), meta(TOKEN_PROGRAM_ID)])).rpc();
      await program.methods.finalizeLargeBasketConfig().accounts({ authority: payer.publicKey, index }).remainingAccounts([meta(page,true)]).rpc();
      const chainNow = async () => connection.getBlockTime(await connection.getSlot('confirmed'));
      const indexState = () => program.account.indexState.fetch(index);
      const ownerContext = (kp, tokenAccounts, signers) => ({ kp, tokenAccounts, signers, nonce: 0, common: { owner: kp.publicKey, index, indexMint, stakingPool, quoteMint: usdcMint, vaultAuthority, intentLock: pda('large-basket-intent-lock', index, kp.publicKey), ownerIndexTokenAccount: getAssociatedTokenAddressSync(indexMint, kp.publicKey), tokenProgram: TOKEN_PROGRAM_ID, systemProgram: SystemProgram.programId } });
      async function newOwner() {
        const kp = Keypair.generate();
        await transferSol(connection, payer, kp.publicKey, 1);
        const tokenAccounts = await Promise.all(components.map(m => getOrCreateAssociatedTokenAccount(connection, payer, m, kp.publicKey)));
        for (const [i, m] of components.entries()) await mintTo(connection, payer, m, tokenAccounts[i].address, payer, 10_000_000);
        return ownerContext(kp, tokenAccounts, [kp]);
      }
      const me = ownerContext(payer, owners, []);
      async function open(o, kind, amount, expiresIn = 600) {
        const nonce = bn(++o.nonce);
        const intent = pda('large-basket-intent', index, o.kp.publicKey, nonce.toArrayLike(Buffer, 'le', 8));
        const expiresAt = (await chainNow()) + expiresIn;
        const args = { nonce, expiresAt: bn(expiresAt), inKind: true, ...(kind === 'mint' ? { indexAmountOut: bn(amount), maxQuoteIn: bn(0) } : { indexAmountIn: bn(amount), minQuoteOut: bn(0) }) };
        await program.methods[kind === 'mint' ? 'openLargeBasketMintIntent' : 'openLargeBasketRedeemIntent'](args).accounts({ ...o.common, intent }).remainingAccounts([meta(page, kind === 'redeem')]).signers(o.signers).rpc();
        return { o, kind, intent, expiresAt };
      }
      async function fill(h) {
        for (let i = 0; i < components.length; i++) {
          await program.methods[h.kind === 'mint' ? 'executeLargeBasketMintComponentInKind' : 'executeLargeBasketRedeemComponentInKind']({ componentIndex: i }).accounts({ ...h.o.common, intent: h.intent, componentPage: page, componentMint: components[i], componentVault: vaults[i], componentTokenProgram: TOKEN_PROGRAM_ID, ownerComponentTokenAccount: h.o.tokenAccounts[i].address, protocolFeeComponentAccount: h.o.tokenAccounts[i].address, creatorFeeComponentAccount: h.o.tokenAccounts[i].address, associatedTokenProgram: ASSOCIATED_TOKEN_PROGRAM_ID }).signers(h.o.signers).rpc();
        }
      }
      async function finalize(h) {
        if (h.kind === 'mint') await program.methods.finalizeLargeBasketMintIntent().accounts({ ...h.o.common, intent: h.intent, associatedTokenProgram: ASSOCIATED_TOKEN_PROGRAM_ID }).remainingAccounts([meta(page, true)]).signers(h.o.signers).rpc();
        else await program.methods.finalizeLargeBasketRedeemIntent().accounts({ index, intent: h.intent, intentLock: h.o.common.intentLock }).rpc();
      }
      async function cancelUnfilled(h) {
        await program.methods[h.kind === 'mint' ? 'cancelUnfilledLargeBasketMintIntent' : 'cancelUnfilledLargeBasketRedeemIntent']().accounts({ ...h.o.common, intent: h.intent }).remainingAccounts(h.kind === 'redeem' ? [meta(page, true)] : []).signers(h.o.signers).rpc();
      }
      async function operation(o, kind, amount) {
        const h = await open(o, kind, amount);
        await fill(h);
        await finalize(h);
      }
      // Anyone may close a settled intent (the payer sends these); its rent goes back to its owner.
      const close = (h, owner = h.o.kp.publicKey) => program.methods.closeLargeBasketIntent().accounts({ owner, intent: h.intent, intentLock: h.o.common.intentLock }).rpc();
      const lamportsOf = async (key) => (await connection.getAccountInfo(key, 'confirmed'))?.lamports ?? 0;
      const supplyOf = async () => (await getMint(connection, indexMint)).supply;
      await operation(me, 'mint', 1_000_000);
      await operation(me, 'mint', 1_000_000);
      assert.equal(await supplyOf(), 2_000_000n);
      for (const [i,v] of vaults.entries()) assert.equal((await getAccount(connection,v)).amount, fixedWeights && components[i].equals(usdcMint) ? 0n : 1_000_000n);
      const unfilledMint = await open(me, 'mint', 1_000_000);
      await assert.rejects(close(unfilledMint), /LargeBasketIntentNotClosable/);
      await cancelUnfilled(unfilledMint);
      await close(unfilledMint);
      assert.equal(await lamportsOf(unfilledMint.intent), 0);
      await cancelUnfilled(await open(me, 'redeem', 1_000_000));
      assert.equal(await supplyOf(), 2_000_000n);

      // Intents from different owners run side by side: a second owner mints and settles
      // while the first owner's redeem is still mid-flight, and neither waits on the other.
      const alice = await newOwner();
      const redeemInFlight = await open(me, 'redeem', 500_000);
      const mintAlongside = await open(alice, 'mint', 1_000_000);
      assert.equal((await indexState()).openIntentCount, 2);
      await fill(mintAlongside);
      await finalize(mintAlongside);
      // Someone other than alice closes her settled mint: the rent goes to alice, and naming
      // anyone else as the owner is refused.
      const aliceRent = await lamportsOf(mintAlongside.intent);
      const aliceBefore = await lamportsOf(alice.kp.publicKey);
      await assert.rejects(close(mintAlongside, payer.publicKey), /InvalidLargeBasketIntent/);
      await close(mintAlongside);
      assert.equal(await lamportsOf(alice.kp.publicKey), aliceBefore + aliceRent);
      assert.equal(await lamportsOf(mintAlongside.intent), 0);
      await fill(redeemInFlight);
      await finalize(redeemInFlight);
      assert.equal((await indexState()).openIntentCount, 0);
      assert.equal(await supplyOf(), 2_500_000n);
      await operation(alice, 'redeem', 1_000_000);
      assert.equal(await supplyOf(), 1_500_000n);

      if (fixedWeights) {
        // Only the authority or its keeper may hold intents back or open a rebalance, and
        // a rebalance waits for open intents to settle instead of cutting them off.
        const keeper = Keypair.generate();
        await transferSol(connection, payer, keeper.publicKey, 1);
        const request = (kp) => program.methods.requestRebalance().accounts({ operator: kp.publicKey, index }).signers([kp]).rpc();
        const rebalanceNonce = bn(1);
        const openRebalance = async (kp) => program.methods.openRebalanceIntent({ nonce: rebalanceNonce, expiresAt: bn((await chainNow()) + 600), maxPriceAgeSlots: bn(50), navToleranceBps: 50, maxPostRebalanceDriftBps: 100 }).accounts({ initiator: kp.publicKey, index, indexMint, vaultAuthority, quoteMint: usdcMint, vaultQuoteTokenAccount: vaults[1], intent: pda('rebalance-intent', index, rebalanceNonce.toArrayLike(Buffer, 'le', 8)), priceOracle: pda('price-oracle'), associatedTokenProgram: ASSOCIATED_TOKEN_PROGRAM_ID, quoteTokenProgram: TOKEN_PROGRAM_ID, systemProgram: SystemProgram.programId }).signers([kp]).rpc();
        await assert.rejects(request(alice.kp), /NotRebalanceOperator/);
        await assert.rejects(request(keeper), /NotRebalanceOperator/);
        await assert.rejects(program.methods.setRebalanceKeeper({ keeper: keeper.publicKey }).accounts({ authority: alice.kp.publicKey, index }).signers([alice.kp]).rpc(), /UnauthorizedAuthority/);
        await program.methods.setRebalanceKeeper({ keeper: keeper.publicKey }).accounts({ authority: payer.publicKey, index }).rpc();
        const openedBeforeRequest = await open(alice, 'mint', 1_000_000);
        await assert.rejects(openRebalance(alice.kp), /NotRebalanceOperator/);
        await assert.rejects(openRebalance(keeper), /IntentsStillOpen/);
        await request(keeper);
        assert.equal((await indexState()).rebalanceRequested, true);
        await assert.rejects(open(me, 'mint', 1_000_000), /RebalancePending/);
        // The intent opened before the request still settles normally.
        await fill(openedBeforeRequest);
        await finalize(openedBeforeRequest);
        assert.equal((await indexState()).openIntentCount, 0);
        await program.methods.cancelRebalanceRequest().accounts({ operator: keeper.publicKey, index }).signers([keeper]).rpc();
        assert.equal((await indexState()).rebalanceRequested, false);
        // Requests are spaced out, so even the keeper cannot hold intents back continuously.
        await assert.rejects(request(keeper), /RebalanceRequestCooldown/);
        await operation(alice, 'redeem', 1_000_000);
      } else {
        await assert.rejects(program.methods.requestRebalance().accounts({ operator: payer.publicKey, index }).rpc(), /InvalidIndexKind/);
      }
      await operation(me, 'redeem', 1_500_000);
      assert.equal(await supplyOf(), 0n);
      for (const v of vaults) assert.equal((await getAccount(connection, v)).amount, 0n);

      // An empty basket: mints opened from two owners are both priced by units_per_index
      // and both settle; the first starts a new supply era and the second joins it.
      const eraBefore = (await indexState()).supplyEra;
      const first = await open(me, 'mint', 1_000_000);
      const second = await open(alice, 'mint', 1_000_000);
      await fill(second);
      await finalize(second);
      await fill(first);
      await finalize(first);
      assert.equal((await indexState()).supplyEra, eraBefore + 1);
      assert.equal(await supplyOf(), 2_000_000n);

      // A ratio-priced mint cannot settle into a basket that emptied after it opened; once
      // it expires anyone can cancel it and the owner gets the deposits back.
      const bob = await newOwner();
      const bobBefore = await Promise.all(bob.tokenAccounts.map(a => getAccount(connection, a.address).then(x => x.amount)));
      const stranded = await open(bob, 'mint', 500_000, 30);
      await fill(stranded);
      await operation(me, 'redeem', 1_000_000);
      await operation(alice, 'redeem', 1_000_000);
      assert.equal(await supplyOf(), 0n);
      await assert.rejects(finalize(stranded), /MintBasisChanged/);
      while ((await chainNow()) <= stranded.expiresAt) await sleep(1_000);
      // Anyone may return the deposits, one component per call: the intent stays Refunding
      // (still open) until all have left the vaults, and nothing can be paid out twice. The
      // first goes to the refund escrow, as the keeper sends it, and the rest straight to bob.
      const amounts = (await program.account.largeBasketIntent.fetch(stranded.intent)).componentAmounts;
      const refundEscrow = pda('refund-escrow', index);
      const escrowAccounts = await Promise.all(components.map(m => getOrCreateAssociatedTokenAccount(connection, payer, m, refundEscrow, true)));
      const owed = components.flatMap((m, i) => amounts[i].isZero() ? [] : [i]);
      const refundGroup = (i, viaEscrow) => [meta(components[i]), meta(vaults[i], true), meta(viaEscrow ? escrowAccounts[i].address : bob.tokenAccounts[i].address, true), meta(TOKEN_PROGRAM_ID)];
      const refund = (group) => program.methods.cancelExpiredLargeBasketIntent().accounts({ index, indexMint, vaultAuthority, intent: stranded.intent, intentLock: bob.common.intentLock, ownerIndexTokenAccount: bob.common.ownerIndexTokenAccount, tokenProgram: TOKEN_PROGRAM_ID }).remainingAccounts([meta(page, true), ...group]).rpc();
      await assert.rejects(refund([]), /InvalidRemainingAccounts/);
      for (const [n, i] of owed.entries()) {
        const group = refundGroup(i, n === 0);
        await refund(group);
        const after = await program.account.largeBasketIntent.fetch(stranded.intent);
        const last = n === owed.length - 1;
        assert.ok(last ? after.status.cancelled : after.status.refunding);
        assert.equal((await indexState()).openIntentCount, last ? 0 : 1);
        if (!last) {
          await assert.rejects(refund(group), /InvalidRemainingAccounts/);
          await assert.rejects(finalize(stranded), /InvalidLargeBasketIntent/);
          await assert.rejects(close(stranded), /LargeBasketIntentNotClosable/);
        }
      }
      // The escrowed component waits for bob, whose lock keeps pointing at the intent until he
      // claims it; only bob's own new intents wait meanwhile.
      const lockOf = async () => (await program.account.largeBasketIntentLock.fetch(bob.common.intentLock)).activeIntent.toBase58();
      assert.equal(await lockOf(), stranded.intent.toBase58());
      // Cancelled, but with a component still in escrow it stays open for bob's claim.
      await assert.rejects(close(stranded), /LargeBasketIntentNotClosable/);
      assert.equal((await getAccount(connection, bob.tokenAccounts[owed[0]].address)).amount, bobBefore[owed[0]] - BigInt(amounts[owed[0]].toString()));
      const claim = (kp) => program.methods.claimLargeBasketRefund().accounts({ owner: kp.publicKey, index, intent: stranded.intent, intentLock: bob.common.intentLock, refundEscrow }).remainingAccounts([meta(page), ...refundGroup(owed[0], true).map((m, k) => (k === 1 ? meta(escrowAccounts[owed[0]].address, true) : k === 2 ? meta(bob.tokenAccounts[owed[0]].address, true) : m))]).signers([kp]).rpc();
      await assert.rejects(claim(alice.kp));
      await claim(bob.kp);
      await assert.rejects(claim(bob.kp), /InvalidRemainingAccounts/);
      assert.equal(await lockOf(), PublicKey.default.toBase58());
      // Claimed in full, it closes, and bob gets its rent back.
      const bobRent = await lamportsOf(stranded.intent);
      const bobSol = await lamportsOf(bob.kp.publicKey);
      await close(stranded);
      assert.equal(await lamportsOf(bob.kp.publicKey), bobSol + bobRent);
      for (const [i, a] of bob.tokenAccounts.entries()) assert.equal((await getAccount(connection, a.address)).amount, bobBefore[i]);
      for (const a of escrowAccounts) assert.equal((await getAccount(connection, a.address)).amount, 0n);
      for (const v of vaults) assert.equal((await getAccount(connection, v)).amount, 0n);
      const settled = await indexState();
      assert.equal(settled.openIntentCount, 0);
      assert.equal(settled.largeBasketOperationInProgress, false);

      if (fixedWeights) {
        // The zero-weight USDC component is a zero amount in every mint and redeem. A mint
        // that fills only it, or a redeem that leaves only it unfilled, owes its owner nothing
        // once expired: no transfer group can name it, and the pages alone settle the intent,
        // so no one can hold the basket's rebalances back with one.
        await operation(me, 'mint', 1_000_000);
        const fillOne = (h, i) => program.methods[h.kind === 'mint' ? 'executeLargeBasketMintComponentInKind' : 'executeLargeBasketRedeemComponentInKind']({ componentIndex: i }).accounts({ ...h.o.common, intent: h.intent, componentPage: page, componentMint: components[i], componentVault: vaults[i], componentTokenProgram: TOKEN_PROGRAM_ID, ownerComponentTokenAccount: h.o.tokenAccounts[i].address, protocolFeeComponentAccount: h.o.tokenAccounts[i].address, creatorFeeComponentAccount: h.o.tokenAccounts[i].address, associatedTokenProgram: ASSOCIATED_TOKEN_PROGRAM_ID }).signers(h.o.signers).rpc();
        const zeroMint = await open(alice, 'mint', 1_000_000, 30);
        const zeroRedeem = await open(me, 'redeem', 500_000, 30);
        const mintAmounts = (await program.account.largeBasketIntent.fetch(zeroMint.intent)).componentAmounts;
        const redeemAmounts = (await program.account.largeBasketIntent.fetch(zeroRedeem.intent)).componentAmounts;
        assert.ok(mintAmounts[1].isZero() && redeemAmounts[1].isZero() && !redeemAmounts[0].isZero());
        await fillOne(zeroMint, 1);
        await fillOne(zeroRedeem, 0);
        const componentBefore = (await getAccount(connection, owners[0].address)).amount;
        assert.equal((await indexState()).openIntentCount, 2);
        while ((await chainNow()) <= Math.max(zeroMint.expiresAt, zeroRedeem.expiresAt)) await sleep(1_000);
        const cancelExpired = (h, transfers = []) => program.methods.cancelExpiredLargeBasketIntent().accounts({ index, indexMint, vaultAuthority, intent: h.intent, intentLock: h.o.common.intentLock, ownerIndexTokenAccount: h.o.common.ownerIndexTokenAccount, tokenProgram: TOKEN_PROGRAM_ID }).remainingAccounts([meta(page, true), ...transfers]).rpc();
        // A component that is not owed still cannot be named: the mint's zero fill, the
        // redeem's delivered share.
        await assert.rejects(cancelExpired(zeroMint, [meta(components[1]), meta(vaults[1], true), meta(alice.tokenAccounts[1].address, true), meta(TOKEN_PROGRAM_ID)]), /InvalidRemainingAccounts/);
        await assert.rejects(cancelExpired(zeroRedeem, [meta(components[0]), meta(vaults[0], true), meta(owners[0].address, true), meta(TOKEN_PROGRAM_ID)]), /InvalidRemainingAccounts/);
        for (const h of [zeroMint, zeroRedeem]) {
          await cancelExpired(h);
          assert.ok((await program.account.largeBasketIntent.fetch(h.intent)).status.cancelled);
          assert.equal((await program.account.largeBasketIntentLock.fetch(h.o.common.intentLock)).activeIntent.toBase58(), PublicKey.default.toBase58());
          await close(h);
        }
        assert.equal((await indexState()).openIntentCount, 0);
        // The redeem's burn and its delivered component stand; nothing moved for the mint.
        assert.equal(await supplyOf(), 500_000n);
        assert.equal((await getAccount(connection, owners[0].address)).amount, componentBefore);
        await operation(me, 'redeem', 500_000);
        assert.equal(await supplyOf(), 0n);
        for (const v of vaults) assert.equal((await getAccount(connection, v)).amount, 0n);
      }
    }
    // A Token-2022 mint that charges a transfer fee cannot become a component: the fee would
    // leave the basket's books above what its vault holds. One without the extension can.
    {
      const index = pda('index', payer.publicKey, 'TWOK');
      const indexMint = pda('index-mint', index);
      const vaultAuthority = pda('vault-authority', index);
      const page = pda('large-basket-component-page', index, Buffer.from([0]));
      await program.methods.createLargeBasketIndex({ name: 'TWOK test', symbol: 'TWOK', metadataUri: '', decimals: 6, feeRecipient: payer.publicKey, creatorFeeRecipient: PublicKey.default, maxSupply: bn(10_000_000), rebalanceDelaySeconds: bn(0), kind: { fixedUnits: {} }, fixedWeightQuoteMint: PublicKey.default, fixedWeightRebalanceIntervalSeconds: bn(0), fixedWeightDriftThresholdBps: 0, fixedWeightSpotEmaMaxDeviationBps: 0, componentCount: 1 }).accounts({ payer: payer.publicKey, authority: payer.publicKey, protocolConfig, index, indexMint, vaultAuthority, tokenProgram: TOKEN_PROGRAM_ID, systemProgram: SystemProgram.programId }).rpc();
      const token2022Mint = async (transferFee) => {
        const mint = Keypair.generate();
        const space = getMintLen(transferFee ? [ExtensionType.TransferFeeConfig] : []);
        const tx = new Transaction().add(SystemProgram.createAccount({ fromPubkey: payer.publicKey, newAccountPubkey: mint.publicKey, space, lamports: await connection.getMinimumBalanceForRentExemption(space), programId: TOKEN_2022_PROGRAM_ID }));
        if (transferFee) tx.add(createInitializeTransferFeeConfigInstruction(mint.publicKey, payer.publicKey, payer.publicKey, 50, 1_000_000n, TOKEN_2022_PROGRAM_ID));
        tx.add(createInitializeMintInstruction(mint.publicKey, 6, payer.publicKey, null, TOKEN_2022_PROGRAM_ID));
        await sendAndConfirmTransaction(connection, tx, [payer, mint]);
        return mint.publicKey;
      };
      const initializePage = (mint) => program.methods.initializeLargeBasketComponentPage({ pageIndex: 0, startComponentIndex: 0, components: [{ mint, unitsPerIndex: bn(1_000_000), targetWeightBps: 0, oraclePair: PublicKey.default }] }).accounts({ payer: payer.publicKey, authority: payer.publicKey, index, indexMint, vaultAuthority, page, associatedTokenProgram: ASSOCIATED_TOKEN_PROGRAM_ID, systemProgram: SystemProgram.programId }).remainingAccounts([meta(mint), meta(getAssociatedTokenAddressSync(mint, vaultAuthority, true, TOKEN_2022_PROGRAM_ID), true), meta(TOKEN_2022_PROGRAM_ID)]).rpc();
      await assert.rejects(initializePage(await token2022Mint(true)), /InvalidTokenMint/);
      const plain = await token2022Mint(false);
      await initializePage(plain);
      assert.ok((await program.account.largeBasketComponentPage.fetch(page)).components[0].tokenProgram.equals(TOKEN_2022_PROGRAM_ID));
    }

    // Exercise the exact swap-path batch interface used by the UI, with a USDC component so
    // this test needs no external liquidity or oracle. Mint fees settle in a separate collect
    // step; redeem fees are charged with each leg and the last leg finalizes the redeem.
    {
      const index = pda('index', payer.publicKey, 'CASH');
      const indexMint = pda('index-mint', index);
      const vaultAuthority = pda('vault-authority', index);
      const page = pda('large-basket-component-page', index, Buffer.from([0]));
      const vault = getAssociatedTokenAddressSync(usdcMint, vaultAuthority, true);
      // A fee wallet distinct from the redeemer, so every fee share has to actually move.
      const feeWallet = Keypair.generate();
      const feeUsdc = (await getOrCreateAssociatedTokenAccount(connection, payer, usdcMint, feeWallet.publicKey)).address;
      await program.methods.createLargeBasketIndex({ name: 'Cash lifecycle', symbol: 'CASH', metadataUri: '', decimals: 6, feeRecipient: feeWallet.publicKey, creatorFeeRecipient: PublicKey.default, maxSupply: bn(2_000_000), rebalanceDelaySeconds: bn(0), kind: { fixedUnits: {} }, fixedWeightQuoteMint: PublicKey.default, fixedWeightRebalanceIntervalSeconds: bn(0), fixedWeightDriftThresholdBps: 0, fixedWeightSpotEmaMaxDeviationBps: 0, componentCount: 1 }).accounts({ payer: payer.publicKey, authority: payer.publicKey, protocolConfig, index, indexMint, vaultAuthority, tokenProgram: TOKEN_PROGRAM_ID, systemProgram: SystemProgram.programId }).rpc();
      await program.methods.initializeLargeBasketComponentPage({ pageIndex: 0, startComponentIndex: 0, components: [{ mint: usdcMint, unitsPerIndex: bn(1_000_000), targetWeightBps: 0, oraclePair: PublicKey.default }] }).accounts({ payer: payer.publicKey, authority: payer.publicKey, index, indexMint, vaultAuthority, page, associatedTokenProgram: ASSOCIATED_TOKEN_PROGRAM_ID, systemProgram: SystemProgram.programId }).remainingAccounts([meta(usdcMint),meta(vault,true),meta(TOKEN_PROGRAM_ID)]).rpc();
      await program.methods.finalizeLargeBasketConfig().accounts({ authority: payer.publicKey,index }).remainingAccounts([meta(page,true)]).rpc();
      await program.methods.updateFees({ mintFeeBps: 10, redeemFeeBps: 10, creatorMintFeeBps: 0, creatorRedeemFeeBps: 0, stakingMintFeeBps: 10, stakingRedeemFeeBps: 10 }).accounts({ authority: payer.publicKey,index }).rpc();
      const ownerIndexTokenAccount = getAssociatedTokenAddressSync(indexMint,payer.publicKey);
      const intentLock = pda('large-basket-intent-lock',index,payer.publicKey);
      const common = { owner: payer.publicKey,index,indexMint,stakingPool,stakingAuthority,quoteMint: usdcMint,vaultAuthority,intentLock,ownerIndexTokenAccount,ownerQuoteTokenAccount:userUsdc.address,quoteTokenProgram:TOKEN_PROGRAM_ID,tokenProgram:TOKEN_PROGRAM_ID,systemProgram:SystemProgram.programId,associatedTokenProgram:ASSOCIATED_TOKEN_PROGRAM_ID };
      const feeAccounts = { feeRecipientQuoteTokenAccount:feeUsdc,creatorFeeRecipientQuoteTokenAccount:feeUsdc,stakingRewardVault:rewardVault };
      const jupiterProgram = new PublicKey('JUP6LkbZbjS1jKKwapdHNy74zcZ3tLUZoi5QNyVTaV4');
      const legAccounts = [meta(page),meta(usdcMint),meta(vault,true),meta(TOKEN_PROGRAM_ID)];
      const usdcOf = async (account) => (await getAccount(connection,account)).amount;
      // An in-kind mint skips the USDC fee step, so it must not be fillable through a swap.
      {
        const nonce = bn(99);
        const intent = pda('large-basket-intent',index,payer.publicKey,nonce.toArrayLike(Buffer,'le',8));
        await program.methods.openLargeBasketMintIntent({ nonce,expiresAt:bn(Math.floor(Date.now()/1000)+600),inKind:true,indexAmountOut:bn(1_000_000),maxQuoteIn:bn(0) }).accounts({...common,intent}).remainingAccounts([meta(page)]).rpc();
        const entries = [{ componentIndex:0,routeAccountCount:0,swap:null,maxQuoteIn:bn(1_000_000) }];
        await assert.rejects(program.methods.executeLargeBasketMintBatch({entries}).accounts({...common,intent,jupiterProgram}).remainingAccounts(legAccounts).rpc(), /InvalidLargeBasketIntent/);
        await program.methods.cancelUnfilledLargeBasketMintIntent().accounts({...common,intent}).rpc();
      }
      for (const [n,kind] of ['mint','redeem'].entries()) {
        const nonce = bn(n+1);
        const intent = pda('large-basket-intent',index,payer.publicKey,nonce.toArrayLike(Buffer,'le',8));
        const args = { nonce,expiresAt:bn(Math.floor(Date.now()/1000)+600),inKind:false,...(kind==='mint'?{indexAmountOut:bn(1_000_000),maxQuoteIn:bn(1_002_000)}:{indexAmountIn:bn(1_000_000),minQuoteOut:bn(998_000)}) };
        if (kind === 'mint') await assert.rejects(program.methods.openLargeBasketMintIntent({...args,indexAmountOut:bn(3_000_000)}).accounts({...common,intent}).remainingAccounts([meta(page)]).rpc(), /SupplyCapExceeded|0x177c/);
        await program.methods[kind==='mint'?'openLargeBasketMintIntent':'openLargeBasketRedeemIntent'](args).accounts({...common,intent}).remainingAccounts([meta(page,kind==='redeem')]).rpc();
        if (kind === 'mint') {
          const entries = [{ componentIndex:0,routeAccountCount:0,swap:null,maxQuoteIn:bn(1_000_000) }];
          await program.methods.executeLargeBasketMintBatch({entries}).accounts({...common,intent,jupiterProgram}).remainingAccounts(legAccounts).rpc();
          await program.methods.collectLargeBasketIntentFees().accounts({...common,intent,...feeAccounts}).rpc();
          await program.methods.finalizeLargeBasketMintIntent().accounts({...common,intent}).remainingAccounts([meta(page,true)]).rpc();
          continue;
        }
        // The fee-less single-leg path and the separate collect step are closed for redeems.
        await assert.rejects(program.methods.executeLargeBasketRedeemComponent({ componentIndex:0,minQuoteOut:bn(0),maxOracleSlippageBps:0,swap:null,switchboardMaxAgeSlots:bn(0) }).accounts({...common,intent,jupiterProgram,componentPage:page,componentMint:usdcMint,componentVault:vault,componentTokenProgram:TOKEN_PROGRAM_ID}).rpc(), /RedeemRequiresBatchExecution/);
        await assert.rejects(program.methods.collectLargeBasketIntentFees().accounts({...common,intent,...feeAccounts}).rpc(), /InvalidLargeBasketIntent/);
        const before = { user: await usdcOf(userUsdc.address), fee: await usdcOf(feeUsdc), staking: await usdcOf(rewardVault) };
        const entries = [{ componentIndex:0,routeAccountCount:0,swap:null,minQuoteOut:bn(1_000_000) }];
        await program.methods.executeLargeBasketRedeemBatch({entries}).accounts({...common,intent,jupiterProgram,...feeAccounts}).remainingAccounts(legAccounts).rpc();
        // Proceeds and both fee shares moved in the same instruction: 10 bps protocol + 10 bps staking.
        assert.equal(await usdcOf(userUsdc.address) - before.user, 998_000n);
        assert.equal(await usdcOf(feeUsdc) - before.fee, 1_000n);
        assert.equal(await usdcOf(rewardVault) - before.staking, 1_000n);
        // The last leg finalized the redeem and released the basket and owner locks.
        const settled = await program.account.largeBasketIntent.fetch(intent);
        assert.ok(settled.status.finalized && settled.feesCollected);
        assert.equal((await program.account.indexState.fetch(index)).openIntentCount, 0);
        assert.equal((await program.account.largeBasketIntentLock.fetch(intentLock)).activeIntent.toBase58(), PublicKey.default.toBase58());
        await assert.rejects(program.methods.finalizeLargeBasketRedeemIntent().accounts({...common,intent}).rpc(), /InvalidLargeBasketIntent/);
      }
      assert.equal((await getMint(connection,indexMint)).supply,0n);
      assert.equal((await getAccount(connection,vault)).amount,0n);
      assert.equal(await usdcOf(feeUsdc),2_000n);
      assert.equal((await getAccount(connection,rewardVault)).amount,2_000n);
    }

    // Creator fees go to a wallet other than the authority, which gains no power over the
    // basket by receiving them. The split is the one planned for BUYB: 10 bps each way, 4 to
    // the treasury, 1 to the creator and 5 to stakers. Swap-path fees move in USDC and
    // in-kind fees in the component itself.
    {
      const fees = { mintFeeBps: 4, redeemFeeBps: 4, creatorMintFeeBps: 1, creatorRedeemFeeBps: 1, stakingMintFeeBps: 5, stakingRedeemFeeBps: 5 };
      const treasury = Keypair.generate();
      const creator = Keypair.generate();
      const jupiterProgram = new PublicKey('JUP6LkbZbjS1jKKwapdHNy74zcZ3tLUZoi5QNyVTaV4');
      const expiresAt = () => bn(Math.floor(Date.now() / 1000) + 600);
      const balance = async (account) => (await getAccount(connection, account)).amount;
      // How much each account's balance moved across one action.
      const deltas = async (accounts, action) => {
        const before = await Promise.all(accounts.map(balance));
        await action();
        return (await Promise.all(accounts.map(balance))).map((after, i) => after - before[i]);
      };
      const createBasket = async (symbol, mint) => {
        const index = pda('index', payer.publicKey, symbol);
        const indexMint = pda('index-mint', index);
        const vaultAuthority = pda('vault-authority', index);
        const page = pda('large-basket-component-page', index, Buffer.from([0]));
        const vault = getAssociatedTokenAddressSync(mint, vaultAuthority, true);
        await program.methods.createLargeBasketIndex({ name: symbol + ' test', symbol, metadataUri: '', decimals: 6, feeRecipient: treasury.publicKey, creatorFeeRecipient: creator.publicKey, maxSupply: bn(10_000_000), rebalanceDelaySeconds: bn(0), kind: { fixedUnits: {} }, fixedWeightQuoteMint: PublicKey.default, fixedWeightRebalanceIntervalSeconds: bn(0), fixedWeightDriftThresholdBps: 0, fixedWeightSpotEmaMaxDeviationBps: 0, componentCount: 1 }).accounts({ payer: payer.publicKey, authority: payer.publicKey, protocolConfig, index, indexMint, vaultAuthority, tokenProgram: TOKEN_PROGRAM_ID, systemProgram: SystemProgram.programId }).rpc();
        await program.methods.initializeLargeBasketComponentPage({ pageIndex: 0, startComponentIndex: 0, components: [{ mint, unitsPerIndex: bn(1_000_000), targetWeightBps: 0, oraclePair: PublicKey.default }] }).accounts({ payer: payer.publicKey, authority: payer.publicKey, index, indexMint, vaultAuthority, page, associatedTokenProgram: ASSOCIATED_TOKEN_PROGRAM_ID, systemProgram: SystemProgram.programId }).remainingAccounts([meta(mint), meta(vault, true), meta(TOKEN_PROGRAM_ID)]).rpc();
        await program.methods.finalizeLargeBasketConfig().accounts({ authority: payer.publicKey, index }).remainingAccounts([meta(page, true)]).rpc();
        await program.methods.updateFees(fees).accounts({ authority: payer.publicKey, index }).rpc();
        const common = { owner: payer.publicKey, index, indexMint, stakingPool, stakingAuthority, quoteMint: usdcMint, vaultAuthority, intentLock: pda('large-basket-intent-lock', index, payer.publicKey), ownerIndexTokenAccount: getAssociatedTokenAddressSync(indexMint, payer.publicKey), ownerQuoteTokenAccount: userUsdc.address, quoteTokenProgram: TOKEN_PROGRAM_ID, tokenProgram: TOKEN_PROGRAM_ID, systemProgram: SystemProgram.programId, associatedTokenProgram: ASSOCIATED_TOKEN_PROGRAM_ID };
        const intentFor = (nonce) => pda('large-basket-intent', index, payer.publicKey, bn(nonce).toArrayLike(Buffer, 'le', 8));
        return { index, indexMint, page, vault, common, intentFor };
      };

      // Swap path, on a basket whose one component is USDC so no liquidity is needed.
      {
        const b = await createBasket('CRTR', usdcMint);
        const state = await program.account.indexState.fetch(b.index);
        assert.equal(state.authority.toBase58(), payer.publicKey.toBase58());
        assert.equal(state.creatorFeeRecipient.toBase58(), creator.publicKey.toBase58());
        // The recipients' USDC accounts, as the app creates them on the first fee-paying mint.
        const treasuryUsdc = (await getOrCreateAssociatedTokenAccount(connection, payer, usdcMint, treasury.publicKey)).address;
        const creatorUsdc = (await getOrCreateAssociatedTokenAccount(connection, payer, usdcMint, creator.publicKey)).address;
        const feeAccounts = { feeRecipientQuoteTokenAccount: treasuryUsdc, creatorFeeRecipientQuoteTokenAccount: creatorUsdc, stakingRewardVault: rewardVault };
        const misdirected = { ...feeAccounts, creatorFeeRecipientQuoteTokenAccount: treasuryUsdc };
        const legAccounts = [meta(b.page), meta(usdcMint), meta(b.vault, true), meta(TOKEN_PROGRAM_ID)];
        const watched = [userUsdc.address, treasuryUsdc, creatorUsdc, rewardVault];

        const mintIntent = b.intentFor(1);
        await program.methods.openLargeBasketMintIntent({ nonce: bn(1), expiresAt: expiresAt(), inKind: false, indexAmountOut: bn(1_000_000), maxQuoteIn: bn(1_001_000) }).accounts({ ...b.common, intent: mintIntent }).remainingAccounts([meta(b.page)]).rpc();
        await program.methods.executeLargeBasketMintBatch({ entries: [{ componentIndex: 0, routeAccountCount: 0, swap: null, maxQuoteIn: bn(1_000_000) }] }).accounts({ ...b.common, intent: mintIntent, jupiterProgram }).remainingAccounts(legAccounts).rpc();
        // Whoever sends the collect step cannot route the creator's share to another account.
        const collect = (accounts) => program.methods.collectLargeBasketIntentFees().accounts({ ...b.common, intent: mintIntent, ...accounts }).rpc();
        await assert.rejects(collect(misdirected), /InvalidCreatorFeeRecipientTokenAccount/);
        // 10 bps of 1 USDC is 1,000 atoms: 400 treasury, 100 creator, 500 stakers.
        assert.deepEqual(await deltas(watched, () => collect(feeAccounts)), [-1_000n, 400n, 100n, 500n]);
        await program.methods.finalizeLargeBasketMintIntent().accounts({ ...b.common, intent: mintIntent }).remainingAccounts([meta(b.page, true)]).rpc();

        const redeemIntent = b.intentFor(2);
        await program.methods.openLargeBasketRedeemIntent({ nonce: bn(2), expiresAt: expiresAt(), inKind: false, indexAmountIn: bn(1_000_000), minQuoteOut: bn(999_000) }).accounts({ ...b.common, intent: redeemIntent }).remainingAccounts([meta(b.page, true)]).rpc();
        const redeemLeg = (accounts) => program.methods.executeLargeBasketRedeemBatch({ entries: [{ componentIndex: 0, routeAccountCount: 0, swap: null, minQuoteOut: bn(1_000_000) }] }).accounts({ ...b.common, intent: redeemIntent, jupiterProgram, ...accounts }).remainingAccounts(legAccounts).rpc();
        await assert.rejects(redeemLeg(misdirected), /InvalidCreatorFeeRecipientTokenAccount/);
        assert.deepEqual(await deltas(watched, () => redeemLeg(feeAccounts)), [999_000n, 400n, 100n, 500n]);
        assert.ok((await program.account.largeBasketIntent.fetch(redeemIntent)).status.finalized);
        assert.equal((await getMint(connection, b.indexMint)).supply, 0n);

        // Receiving fees gives the recipient no say over the basket: fees, recipients, pauses
        // and the authority all stay with the authority.
        await assert.rejects(program.methods.updateFees({ ...fees, creatorMintFeeBps: 100 }).accounts({ authority: creator.publicKey, index: b.index }).signers([creator]).rpc(), /UnauthorizedAuthority/);
        await assert.rejects(program.methods.updateConfig({ feeRecipient: creator.publicKey, creatorFeeRecipient: creator.publicKey, maxSupply: bn(0), rebalanceDelaySeconds: bn(0), mintingPaused: true, redeemingPaused: true, rebalancingPaused: true }).accounts({ authority: creator.publicKey, index: b.index }).signers([creator]).rpc(), /UnauthorizedAuthority/);
        await assert.rejects(program.methods.updateAuthority({ newAuthority: creator.publicKey }).accounts({ authority: creator.publicKey, index: b.index }).signers([creator]).rpc(), /UnauthorizedAuthority/);
      }

      // In kind, on a basket of a plain token: the fee is taken in that token. Staking rewards
      // are paid in USDC only, so the stakers' share goes to the treasury on this path.
      {
        const token = await createMint(connection, payer, payer.publicKey, null, 6);
        const b = await createBasket('CRTK', token);
        const own = (await getOrCreateAssociatedTokenAccount(connection, payer, token, payer.publicKey)).address;
        await mintTo(connection, payer, token, own, payer, 2_000_000);
        const treasuryToken = (await getOrCreateAssociatedTokenAccount(connection, payer, token, treasury.publicKey)).address;
        const creatorToken = (await getOrCreateAssociatedTokenAccount(connection, payer, token, creator.publicKey)).address;
        const watched = [own, b.vault, treasuryToken, creatorToken, rewardVault];
        const fill = (kind, intent, creatorAccount) => program.methods[kind === 'mint' ? 'executeLargeBasketMintComponentInKind' : 'executeLargeBasketRedeemComponentInKind']({ componentIndex: 0 }).accounts({ ...b.common, intent, componentPage: b.page, componentMint: token, componentVault: b.vault, componentTokenProgram: TOKEN_PROGRAM_ID, ownerComponentTokenAccount: own, protocolFeeComponentAccount: treasuryToken, creatorFeeComponentAccount: creatorAccount }).rpc();

        const mintIntent = b.intentFor(1);
        await program.methods.openLargeBasketMintIntent({ nonce: bn(1), expiresAt: expiresAt(), inKind: true, indexAmountOut: bn(1_000_000), maxQuoteIn: bn(0) }).accounts({ ...b.common, intent: mintIntent }).remainingAccounts([meta(b.page)]).rpc();
        await assert.rejects(fill('mint', mintIntent, own), /InvalidFeeRecipientTokenAccount/);
        // 10 bps of 1,000,000 atoms is 1,000: 100 to the creator, 900 to the treasury.
        assert.deepEqual(await deltas(watched, () => fill('mint', mintIntent, creatorToken)), [-1_001_000n, 1_000_000n, 900n, 100n, 0n]);
        await program.methods.finalizeLargeBasketMintIntent().accounts({ ...b.common, intent: mintIntent }).remainingAccounts([meta(b.page, true)]).rpc();

        const redeemIntent = b.intentFor(2);
        await program.methods.openLargeBasketRedeemIntent({ nonce: bn(2), expiresAt: expiresAt(), inKind: true, indexAmountIn: bn(1_000_000), minQuoteOut: bn(0) }).accounts({ ...b.common, intent: redeemIntent }).remainingAccounts([meta(b.page, true)]).rpc();
        await assert.rejects(fill('redeem', redeemIntent, own), /InvalidFeeRecipientTokenAccount/);
        assert.deepEqual(await deltas(watched, () => fill('redeem', redeemIntent, creatorToken)), [999_000n, -1_000_000n, 900n, 100n, 0n]);
        await program.methods.finalizeLargeBasketRedeemIntent().accounts({ index: b.index, intent: redeemIntent, intentLock: b.common.intentLock }).rpc();
        assert.equal((await getMint(connection, b.indexMint)).supply, 0n);
      }
    }
    console.log('integration ok: six-decimal staking, rewards, unstaking; fixed-unit and fixed-weight paged creation, initial/pro-rata mint, cancellation, complete redemption; concurrent intents from several owners, keeper-only spaced rebalance requests, empty-basket eras, incremental expiry refunds and escrow claims; settled intents closed with the rent back to the owner, unsettled ones refused; 40-component cap; zero-amount intents settled once expired; transfer-fee Token-2022 components refused; swap-path mint fee collection; redeem fees charged with proceeds, auto-finalize, closed bypasses; creator fees paid to a separate recipient in USDC and in kind, unredirectable, with no authority');
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
