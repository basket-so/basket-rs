import fs from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";

import anchor from "@coral-xyz/anchor";
import {
  Keypair,
  PublicKey,
  SystemProgram,
} from "@solana/web3.js";
import {
  ASSOCIATED_TOKEN_PROGRAM_ID,
  getAccount,
  getAssociatedTokenAddressSync,
  getMint,
  TOKEN_PROGRAM_ID,
} from "@solana/spl-token";

const __dirname = path.dirname(fileURLToPath(import.meta.url));
const root = path.resolve(__dirname, "..");

const programId = new PublicKey("H6JKCZU82gCADQZ98Jmfj7UzpHTdbyfnDpt7AD5NZ3Lt");

const usdcMint = new PublicKey("EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v");
const umbraMint = new PublicKey("PRVT6TB7uss3FrUd2D9xs2zqDBsa3GbMJMwCQsgmeta");
const omfgMint = new PublicKey("omfgRBnxHsNJh6YeGbGAmWenNkenzsXyBXm3WDhmeta");
const metaMint = new PublicKey("METAwkXcqyXKy1AtsSgJ8JiUHwGCafnZL38n3vYmeta");
const aviciMint = new PublicKey("BANKJmvhT8tiJRsBSS1n2HryMBPvT5Ze4HU95DUAmeta");

const pairs = {
  UMBRA_USDC: new PublicKey("DLdMKytiJvVgivmQBhwfKEU4hgEWjhGURS16kTeEkjPP"),
  OMFG_USDC: new PublicKey("BZi9iPbcpLWDkHs8nNQe5pb9MxLQe8fbgHtypj2pL4UB"),
  META_USDC: new PublicKey("Cp2nGCWWfqkUmPR3pPKoR376Fti8wuYRFrSWJZq1a9SA"),
  AVICI_USDC: new PublicKey("7USpQsGyRh9awZXyCFft18Do7di8mraRrfqBcYXLok7C"),
};

const rpcUrl = process.env.SOLANA_RPC_URL ?? "https://api.mainnet-beta.solana.com";
const walletPath = path.resolve(
  process.env.ANCHOR_WALLET ?? path.join(root, "deployer-keypair.json"),
);
const dryRun = process.argv.includes("--dry-run");

const rebalanceIntervalSeconds = Number(
  process.env.META4_REBALANCE_INTERVAL_SECONDS ?? 86_400,
);
const driftThresholdBps = Number(process.env.META4_DRIFT_THRESHOLD_BPS ?? 500);
const spotEmaMaxDeviationBps = Number(
  process.env.META4_SPOT_EMA_MAX_DEVIATION_BPS ?? 1_000,
);

function keypairFromFile(file) {
  return Keypair.fromSecretKey(
    Uint8Array.from(JSON.parse(fs.readFileSync(file, "utf8"))),
  );
}

function pda(seeds) {
  return PublicKey.findProgramAddressSync(seeds, programId)[0];
}

async function accountExists(connection, pubkey) {
  return (await connection.getAccountInfo(pubkey, "confirmed")) !== null;
}

async function send(label, builder) {
  if (dryRun) {
    console.log(`[dry-run] would send ${label}`);
    return null;
  }

  const signature = await builder().rpc({
    commitment: "confirmed",
    preflightCommitment: "confirmed",
    maxRetries: 5,
  });
  console.log(`${label}: ${signature}`);
  return signature;
}

function assertKey(label, actual, expected) {
  if (!actual.equals(expected)) {
    throw new Error(`${label} mismatch: got ${actual.toBase58()}, expected ${expected.toBase58()}`);
  }
}

function assertNumber(label, actual, expected) {
  if (Number(actual) !== Number(expected)) {
    throw new Error(`${label} mismatch: got ${actual}, expected ${expected}`);
  }
}

async function assertClassicMint(connection, label, mint) {
  const account = await connection.getAccountInfo(mint, "confirmed");
  if (!account) {
    throw new Error(`${label} mint does not exist: ${mint.toBase58()}`);
  }
  assertKey(`${label} owner`, account.owner, TOKEN_PROGRAM_ID);
  const mintInfo = await getMint(connection, mint, "confirmed", TOKEN_PROGRAM_ID);
  if (mintInfo.decimals !== 6) {
    throw new Error(`${label} expected 6 decimals, got ${mintInfo.decimals}`);
  }
}

async function main() {
  const payer = keypairFromFile(walletPath);
  const connection = new anchor.web3.Connection(rpcUrl, "confirmed");
  const provider = new anchor.AnchorProvider(
    connection,
    new anchor.Wallet(payer),
    { commitment: "confirmed", preflightCommitment: "confirmed" },
  );
  anchor.setProvider(provider);

  const idl = JSON.parse(
    fs.readFileSync(path.join(root, "target", "idl", "omnindex.json"), "utf8"),
  );
  const program = new anchor.Program(idl, provider);

  const programInfo = await connection.getAccountInfo(programId, "confirmed");
  if (!programInfo?.executable) {
    throw new Error(`${programId.toBase58()} is not an executable mainnet program`);
  }

  const authority = payer.publicKey;
  const protocolConfig = pda([Buffer.from("protocol-config")]);
  const symbol = "META4";
  const name = "META4";
  const index = pda([Buffer.from("index"), authority.toBuffer(), Buffer.from(symbol)]);
  const indexMint = pda([Buffer.from("index-mint"), index.toBuffer()]);
  const vaultAuthority = pda([Buffer.from("vault-authority"), index.toBuffer()]);

  const components = [
    { symbol: "UMBRA", mint: umbraMint, pair: pairs.UMBRA_USDC },
    { symbol: "OMFG", mint: omfgMint, pair: pairs.OMFG_USDC },
    { symbol: "META", mint: metaMint, pair: pairs.META_USDC },
    { symbol: "AVICI", mint: aviciMint, pair: pairs.AVICI_USDC },
  ];
  const vaults = components.map(({ mint }) =>
    getAssociatedTokenAddressSync(
      mint,
      vaultAuthority,
      true,
      TOKEN_PROGRAM_ID,
      ASSOCIATED_TOKEN_PROGRAM_ID,
    ),
  );

  console.log(`rpc: ${rpcUrl}`);
  console.log(`authority: ${authority.toBase58()}`);
  console.log(`program: ${programId.toBase58()}`);
  console.log(`protocolConfig: ${protocolConfig.toBase58()}`);
  console.log(`index: ${index.toBase58()}`);
  console.log(`indexMint: ${indexMint.toBase58()}`);
  console.log(`vaultAuthority: ${vaultAuthority.toBase58()}`);
  for (const [i, component] of components.entries()) {
    console.log(`${component.symbol} vault: ${vaults[i].toBase58()}`);
  }

  for (const component of components) {
    await assertClassicMint(connection, component.symbol, component.mint);
  }
  await assertClassicMint(connection, "USDC", usdcMint);

  const protocolExists = await accountExists(connection, protocolConfig);
  if (!protocolExists) {
    throw new Error("protocol config is not initialized");
  }

  const indexExists = await accountExists(connection, index);
  const vaultExists = await Promise.all(
    vaults.map((vault) => accountExists(connection, vault)),
  );

  if (dryRun) {
    console.log("plan:", {
      createIndex: !indexExists,
      initializeVaults: vaultExists.some((exists) => !exists),
      indexArgs: {
        name,
        symbol,
        decimals: 6,
        kind: "fixedWeights",
        fixedWeightQuoteMint: usdcMint.toBase58(),
        fixedWeightRebalanceIntervalSeconds: rebalanceIntervalSeconds,
        fixedWeightDriftThresholdBps: driftThresholdBps,
        fixedWeightSpotEmaMaxDeviationBps: spotEmaMaxDeviationBps,
        components: components.map((component) => ({
          symbol: component.symbol,
          mint: component.mint.toBase58(),
          targetWeightBps: 2_500,
          oraclePair: component.pair.toBase58(),
        })),
      },
    });
    return;
  }

  if (!indexExists) {
    await send("createIndex META4", () =>
      program.methods
        .createIndex({
          name,
          symbol,
          metadataUri: "",
          decimals: 6,
          feeRecipient: authority,
          maxSupply: new anchor.BN(0),
          rebalanceDelaySeconds: new anchor.BN(0),
          kind: { fixedWeights: {} },
          fixedWeightQuoteMint: usdcMint,
          fixedWeightRebalanceIntervalSeconds: new anchor.BN(rebalanceIntervalSeconds),
          fixedWeightDriftThresholdBps: driftThresholdBps,
          fixedWeightSpotEmaMaxDeviationBps: spotEmaMaxDeviationBps,
          components: components.map((component) => ({
            mint: component.mint,
            unitsPerIndex: new anchor.BN(1),
            targetWeightBps: 2_500,
            oraclePair: component.pair,
          })),
        })
        .accounts({
          payer: authority,
          authority,
          protocolConfig,
          index,
          indexMint,
          vaultAuthority,
          tokenProgram: TOKEN_PROGRAM_ID,
          systemProgram: SystemProgram.programId,
        }),
    );
  } else {
    console.log("META4 index already exists; verifying");
  }

  const indexState = await program.account.indexState.fetch(index);
  assertKey("index authority", indexState.authority, authority);
  assertKey("index mint", indexState.indexMint, indexMint);
  assertKey("fixed-weight quote mint", indexState.fixedWeightQuoteMint, usdcMint);
  assertNumber("index decimals", indexState.decimals, 6);
  assertNumber(
    "fixed weight rebalance interval",
    indexState.fixedWeightRebalanceIntervalSeconds,
    rebalanceIntervalSeconds,
  );
  assertNumber("fixed weight drift threshold", indexState.fixedWeightDriftThresholdBps, driftThresholdBps);
  assertNumber(
    "fixed weight spot/EMA max deviation",
    indexState.fixedWeightSpotEmaMaxDeviationBps,
    spotEmaMaxDeviationBps,
  );
  if (!("fixedWeights" in indexState.kind)) {
    throw new Error(`index kind mismatch: ${JSON.stringify(indexState.kind)}`);
  }
  if (indexState.name !== name || indexState.symbol !== symbol) {
    throw new Error(`index name/symbol mismatch: ${indexState.name}/${indexState.symbol}`);
  }
  if (indexState.components.length !== components.length) {
    throw new Error(`component count mismatch: ${indexState.components.length}`);
  }
  for (const [i, component] of components.entries()) {
    assertKey(`${component.symbol} component mint`, indexState.components[i].mint, component.mint);
    assertNumber(`${component.symbol} units`, indexState.components[i].unitsPerIndex, 1);
    assertNumber(`${component.symbol} target weight`, indexState.components[i].targetWeightBps, 2_500);
    assertKey(`${component.symbol} oracle pair`, indexState.components[i].oraclePair, component.pair);
  }

  if (vaultExists.some((exists) => !exists)) {
    await send("initializeVaults META4", () =>
      program.methods
        .initializeVaults()
        .accounts({
          payer: authority,
          index,
          vaultAuthority,
          associatedTokenProgram: ASSOCIATED_TOKEN_PROGRAM_ID,
          tokenProgram: TOKEN_PROGRAM_ID,
          systemProgram: SystemProgram.programId,
        })
        .remainingAccounts(
          components.flatMap((component, i) => [
            { pubkey: component.mint, isWritable: false, isSigner: false },
            { pubkey: vaults[i], isWritable: true, isSigner: false },
          ]),
        ),
    );
  } else {
    console.log("META4 vaults already exist; verifying");
  }

  for (const [i, component] of components.entries()) {
    const vault = await getAccount(connection, vaults[i], "confirmed", TOKEN_PROGRAM_ID);
    assertKey(`${component.symbol} vault owner`, vault.owner, vaultAuthority);
    assertKey(`${component.symbol} vault mint`, vault.mint, component.mint);
  }

  console.log("META4 initialized and verified");
}

main().catch((error) => {
  console.error(error);
  process.exit(1);
});
