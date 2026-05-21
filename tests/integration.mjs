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
const programId = new PublicKey("H6JKCZU82gCADQZ98Jmfj7UzpHTdbyfnDpt7AD5NZ3Lt");
const basketMint = new PublicKey("5yTFbtAE5RDjxpiVpDfyWuzcCWgwh659CEu7a7ZQtSpk");
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
      mintAuthority: PublicKey.default,
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
  fs.rmSync(ledgerDir, { recursive: true, force: true });
  fs.rmSync(fixtureDir, { recursive: true, force: true });
  fs.mkdirSync(fixtureDir, { recursive: true });
  const basketMintDump = path.join(fixtureDir, "basket-mint.json");
  const usdcMintDump = path.join(fixtureDir, "usdc-mint.json");
  writeMintAccountDump(basketMintDump, basketMint, 9);
  writeMintAccountDump(usdcMintDump, usdcMint, 6);
  const payer = keypairFromFile("deployer-keypair.json");

  const validatorProcess = spawn(
    validator,
    [
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
    ],
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

    const componentA = await createMint(connection, payer, payer.publicKey, null, 6);
    const componentB = await createMint(connection, payer, payer.publicKey, null, 6);
    const symbol = "TST";
    const [index] = PublicKey.findProgramAddressSync(
      [Buffer.from("index"), payer.publicKey.toBuffer(), Buffer.from(symbol)],
      programId,
    );
    const [indexMint] = PublicKey.findProgramAddressSync(
      [Buffer.from("index-mint"), index.toBuffer()],
      programId,
    );
    const [vaultAuthority] = PublicKey.findProgramAddressSync(
      [Buffer.from("vault-authority"), index.toBuffer()],
      programId,
    );

    await program.methods
      .createIndex({
        name: "Test Index",
        symbol,
        metadataUri: "https://example.invalid/test-index.json",
        decimals: 6,
        feeRecipient: payer.publicKey,
        creatorFeeRecipient: PublicKey.default,
        maxSupply: new anchor.BN(0),
        rebalanceDelaySeconds: new anchor.BN(0),
        kind: { fixedUnits: {} },
        fixedWeightQuoteMint: PublicKey.default,
        fixedWeightRebalanceIntervalSeconds: new anchor.BN(0),
        fixedWeightDriftThresholdBps: 0,
        fixedWeightSpotEmaMaxDeviationBps: 0,
        components: [
          {
            mint: componentA,
            unitsPerIndex: new anchor.BN(1_000_000),
            targetWeightBps: 0,
            oraclePair: PublicKey.default,
          },
          {
            mint: componentB,
            unitsPerIndex: new anchor.BN(2_000_000),
            targetWeightBps: 0,
            oraclePair: PublicKey.default,
          },
        ],
      })
      .accounts({
        payer: payer.publicKey,
        authority: payer.publicKey,
        protocolConfig,
        index,
        indexMint,
        vaultAuthority,
        tokenProgram: TOKEN_PROGRAM_ID,
        systemProgram: SystemProgram.programId,
      })
      .rpc();

    const createdIndex = await program.account.indexState.fetch(index);
    assert.equal(createdIndex.authority.toBase58(), payer.publicKey.toBase58());
    assert.equal(createdIndex.indexMint.toBase58(), indexMint.toBase58());
    assert.equal(createdIndex.creatorFeeRecipient.toBase58(), PublicKey.default.toBase58());
    assert.equal(createdIndex.componentCount, 2);

    const fixedWeightSymbol = "FWX";
    const [fixedWeightIndex] = PublicKey.findProgramAddressSync(
      [Buffer.from("index"), payer.publicKey.toBuffer(), Buffer.from(fixedWeightSymbol)],
      programId,
    );
    const [fixedWeightIndexMint] = PublicKey.findProgramAddressSync(
      [Buffer.from("index-mint"), fixedWeightIndex.toBuffer()],
      programId,
    );
    const [fixedWeightVaultAuthority] = PublicKey.findProgramAddressSync(
      [Buffer.from("vault-authority"), fixedWeightIndex.toBuffer()],
      programId,
    );
    const fixedWeightComponentAPair = Keypair.generate().publicKey;
    const fixedWeightComponentBPair = Keypair.generate().publicKey;

    await program.methods
      .createIndex({
        name: "Fixed Weight External Quote",
        symbol: fixedWeightSymbol,
        metadataUri: "https://example.invalid/fixed-weight-external-quote.json",
        decimals: 6,
        feeRecipient: payer.publicKey,
        creatorFeeRecipient: PublicKey.default,
        maxSupply: new anchor.BN(0),
        rebalanceDelaySeconds: new anchor.BN(0),
        kind: { fixedWeights: {} },
        fixedWeightQuoteMint: usdcMint,
        fixedWeightRebalanceIntervalSeconds: new anchor.BN(60),
        fixedWeightDriftThresholdBps: 0,
        fixedWeightSpotEmaMaxDeviationBps: 500,
        components: [
          {
            mint: componentA,
            unitsPerIndex: new anchor.BN(1_250_000),
            targetWeightBps: 5_000,
            oraclePair: fixedWeightComponentAPair,
          },
          {
            mint: componentB,
            unitsPerIndex: new anchor.BN(2_500_000),
            targetWeightBps: 5_000,
            oraclePair: fixedWeightComponentBPair,
          },
        ],
      })
      .accounts({
        payer: payer.publicKey,
        authority: payer.publicKey,
        protocolConfig,
        index: fixedWeightIndex,
        indexMint: fixedWeightIndexMint,
        vaultAuthority: fixedWeightVaultAuthority,
        tokenProgram: TOKEN_PROGRAM_ID,
        systemProgram: SystemProgram.programId,
      })
      .rpc();

    let fixedWeightState = await program.account.indexState.fetch(fixedWeightIndex);
    assert.equal(fixedWeightState.indexMint.toBase58(), fixedWeightIndexMint.toBase58());
    assert.equal(fixedWeightState.fixedWeightQuoteMint.toBase58(), usdcMint.toBase58());
    assert.equal(fixedWeightState.components.length, 2);
    assert.equal(fixedWeightState.components[0].unitsPerIndex.toString(), "1250000");
    assert.equal(fixedWeightState.components[1].unitsPerIndex.toString(), "2500000");
    assert.equal(fixedWeightState.components[0].targetWeightBps, 5_000);
    assert.equal(fixedWeightState.components[1].targetWeightBps, 5_000);

    await program.methods
      .updateFixedWeightConfig({
        fixedWeightQuoteMint: usdcMint,
        fixedWeightRebalanceIntervalSeconds: new anchor.BN(120),
        fixedWeightDriftThresholdBps: 250,
        fixedWeightSpotEmaMaxDeviationBps: 750,
        components: [
          {
            mint: componentA,
            unitsPerIndex: new anchor.BN(123_456),
            targetWeightBps: 5_000,
            oraclePair: fixedWeightComponentAPair,
          },
          {
            mint: componentB,
            unitsPerIndex: new anchor.BN(654_321),
            targetWeightBps: 5_000,
            oraclePair: fixedWeightComponentBPair,
          },
        ],
      })
      .accounts({
        authority: payer.publicKey,
        index: fixedWeightIndex,
        indexMint: fixedWeightIndexMint,
      })
      .rpc();

    fixedWeightState = await program.account.indexState.fetch(fixedWeightIndex);
    assert.equal(fixedWeightState.fixedWeightRebalanceIntervalSeconds.toString(), "120");
    assert.equal(fixedWeightState.fixedWeightDriftThresholdBps, 250);
    assert.equal(fixedWeightState.fixedWeightSpotEmaMaxDeviationBps, 750);
    assert.equal(fixedWeightState.components[0].unitsPerIndex.toString(), "123456");
    assert.equal(fixedWeightState.components[1].unitsPerIndex.toString(), "654321");

    const vaultA = getAssociatedTokenAddressSync(
      componentA,
      vaultAuthority,
      true,
      TOKEN_PROGRAM_ID,
      ASSOCIATED_TOKEN_PROGRAM_ID,
    );
    const vaultB = getAssociatedTokenAddressSync(
      componentB,
      vaultAuthority,
      true,
      TOKEN_PROGRAM_ID,
      ASSOCIATED_TOKEN_PROGRAM_ID,
    );

    await program.methods
      .initializeVaults()
      .accounts({
        payer: payer.publicKey,
        index,
        vaultAuthority,
        associatedTokenProgram: ASSOCIATED_TOKEN_PROGRAM_ID,
        tokenProgram: TOKEN_PROGRAM_ID,
        systemProgram: SystemProgram.programId,
      })
      .remainingAccounts([
        { pubkey: componentA, isWritable: false, isSigner: false },
        { pubkey: vaultA, isWritable: true, isSigner: false },
        { pubkey: componentB, isWritable: false, isSigner: false },
        { pubkey: vaultB, isWritable: true, isSigner: false },
      ])
      .rpc();

    const userA = await getOrCreateAssociatedTokenAccount(
      connection,
      payer,
      componentA,
      user.publicKey,
    );
    const userB = await getOrCreateAssociatedTokenAccount(
      connection,
      payer,
      componentB,
      user.publicKey,
    );
    await mintTo(connection, payer, componentA, userA.address, payer, 10_000_000);
    await mintTo(connection, payer, componentB, userB.address, payer, 10_000_000);

    const userIndexTokenAccount = getAssociatedTokenAddressSync(
      indexMint,
      user.publicKey,
      false,
      TOKEN_PROGRAM_ID,
      ASSOCIATED_TOKEN_PROGRAM_ID,
    );

    await program.methods
      .mintIndex({ amount: new anchor.BN(1_000_000) })
      .accounts({
        depositor: user.publicKey,
        index,
        indexMint,
        vaultAuthority,
        depositorIndexTokenAccount: userIndexTokenAccount,
        associatedTokenProgram: ASSOCIATED_TOKEN_PROGRAM_ID,
        tokenProgram: TOKEN_PROGRAM_ID,
        systemProgram: SystemProgram.programId,
      })
      .remainingAccounts([
        { pubkey: userA.address, isWritable: true, isSigner: false },
        { pubkey: vaultA, isWritable: true, isSigner: false },
        { pubkey: userB.address, isWritable: true, isSigner: false },
        { pubkey: vaultB, isWritable: true, isSigner: false },
      ])
      .signers([user])
      .rpc();

    assert.equal((await getMint(connection, indexMint)).supply, 1_000_000n);
    assert.equal((await getAccount(connection, vaultA)).amount, 1_000_000n);
    assert.equal((await getAccount(connection, vaultB)).amount, 2_000_000n);
    assert.equal((await getAccount(connection, userIndexTokenAccount)).amount, 1_000_000n);

    await program.methods
      .redeemIndex({ amount: new anchor.BN(400_000) })
      .accounts({
        redeemer: user.publicKey,
        index,
        indexMint,
        vaultAuthority,
        redeemerIndexTokenAccount: userIndexTokenAccount,
        tokenProgram: TOKEN_PROGRAM_ID,
      })
      .remainingAccounts([
        { pubkey: vaultA, isWritable: true, isSigner: false },
        { pubkey: userA.address, isWritable: true, isSigner: false },
        { pubkey: vaultB, isWritable: true, isSigner: false },
        { pubkey: userB.address, isWritable: true, isSigner: false },
      ])
      .signers([user])
      .rpc();

    assert.equal((await getMint(connection, indexMint)).supply, 600_000n);
    assert.equal((await getAccount(connection, vaultA)).amount, 600_000n);
    assert.equal((await getAccount(connection, vaultB)).amount, 1_200_000n);
    assert.equal((await getAccount(connection, userIndexTokenAccount)).amount, 600_000n);
    assert.equal((await getAccount(connection, userA.address)).amount, 9_400_000n);
    assert.equal((await getAccount(connection, userB.address)).amount, 8_800_000n);

    console.log("integration ok: protocol, staking pool, index, vaults, direct mint/redeem");
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
