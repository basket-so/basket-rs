import fs from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";

import anchor from "@coral-xyz/anchor";
import {
  Keypair,
  PublicKey,
  SystemProgram,
  SYSVAR_RENT_PUBKEY,
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
const bpfLoaderUpgradeable = new PublicKey(
  "BPFLoaderUpgradeab1e11111111111111111111111",
);
const metadataProgram = new PublicKey(
  "metaqbxxUerdq28cj1RbAWkYQm3ybzjb6a8bt518x1s",
);

const basketMint = new PublicKey("5yTFbtAE5RDjxpiVpDfyWuzcCWgwh659CEu7a7ZQtSpk");
const usdcMint = new PublicKey("EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v");
const metaMint = new PublicKey("METAwkXcqyXKy1AtsSgJ8JiUHwGCafnZL38n3vYmeta");
const omfgMint = new PublicKey("omfgRBnxHsNJh6YeGbGAmWenNkenzsXyBXm3WDhmeta");

const rpcUrl = process.env.SOLANA_RPC_URL ?? "https://api.mainnet-beta.solana.com";
const walletPath = path.resolve(
  process.env.ANCHOR_WALLET ?? path.join(root, "deployer-keypair.json"),
);
const dryRun = process.argv.includes("--dry-run");
const createMetadata = process.argv.includes("--create-metadata");
const metadataUri = process.env.META2_METADATA_URI ?? "";

function keypairFromFile(file) {
  return Keypair.fromSecretKey(
    Uint8Array.from(JSON.parse(fs.readFileSync(file, "utf8"))),
  );
}

function pda(seeds) {
  return PublicKey.findProgramAddressSync(seeds, programId)[0];
}

function metadataPda(mint) {
  return PublicKey.findProgramAddressSync(
    [Buffer.from("metadata"), metadataProgram.toBuffer(), mint.toBuffer()],
    metadataProgram,
  )[0];
}

function displayComponent(component) {
  return {
    mint: component.mint.toBase58(),
    unitsPerIndex: component.unitsPerIndex.toString(),
    targetWeightBps: component.targetWeightBps,
    oraclePair: component.oraclePair.toBase58(),
  };
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
  const [programData] = PublicKey.findProgramAddressSync(
    [programId.toBuffer()],
    bpfLoaderUpgradeable,
  );
  const protocolConfig = pda([Buffer.from("protocol-config")]);
  const stakingPool = pda([Buffer.from("staking-pool")]);
  const stakingAuthority = pda([Buffer.from("staking-authority")]);
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

  const symbol = "META2";
  const name = "META2";
  const index = pda([Buffer.from("index"), authority.toBuffer(), Buffer.from(symbol)]);
  const indexMint = pda([Buffer.from("index-mint"), index.toBuffer()]);
  const vaultAuthority = pda([Buffer.from("vault-authority"), index.toBuffer()]);
  const metaVault = getAssociatedTokenAddressSync(
    metaMint,
    vaultAuthority,
    true,
    TOKEN_PROGRAM_ID,
    ASSOCIATED_TOKEN_PROGRAM_ID,
  );
  const omfgVault = getAssociatedTokenAddressSync(
    omfgMint,
    vaultAuthority,
    true,
    TOKEN_PROGRAM_ID,
    ASSOCIATED_TOKEN_PROGRAM_ID,
  );
  const metadata = metadataPda(indexMint);

  const meta = await getMint(connection, metaMint, "confirmed", TOKEN_PROGRAM_ID);
  const omfg = await getMint(connection, omfgMint, "confirmed", TOKEN_PROGRAM_ID);
  if (meta.decimals !== 6 || omfg.decimals !== 6) {
    throw new Error(`expected 6-decimal component mints, got META=${meta.decimals}, OMFG=${omfg.decimals}`);
  }

  const oneMeta = 10n ** BigInt(meta.decimals);
  const tenOmfg = 10n * 10n ** BigInt(omfg.decimals);
  if (oneMeta > BigInt(Number.MAX_SAFE_INTEGER) || tenOmfg > BigInt(Number.MAX_SAFE_INTEGER)) {
    throw new Error("component units exceed safe JavaScript integer range");
  }

  console.log(`rpc: ${rpcUrl}`);
  console.log(`authority: ${authority.toBase58()}`);
  console.log(`program: ${programId.toBase58()}`);
  console.log(`protocolConfig: ${protocolConfig.toBase58()}`);
  console.log(`stakingPool: ${stakingPool.toBase58()}`);
  console.log(`index: ${index.toBase58()}`);
  console.log(`indexMint: ${indexMint.toBase58()}`);
  console.log(`vaultAuthority: ${vaultAuthority.toBase58()}`);
  console.log(`META vault: ${metaVault.toBase58()}`);
  console.log(`OMFG vault: ${omfgVault.toBase58()}`);

  const protocolExists = await accountExists(connection, protocolConfig);
  const stakingExists = await accountExists(connection, stakingPool);
  const indexExists = await accountExists(connection, index);
  const metaVaultExists = await accountExists(connection, metaVault);
  const omfgVaultExists = await accountExists(connection, omfgVault);
  const metadataExists = await accountExists(connection, metadata);

  if (dryRun) {
    console.log("plan:", {
      initializeProtocol: !protocolExists,
      initializeStakingPool: !stakingExists,
      createIndex: !indexExists,
      initializeVaults: !(metaVaultExists && omfgVaultExists),
      createIndexMetadata: createMetadata && !metadataExists,
      indexArgs: {
        name,
        symbol,
        metadataUri,
        decimals: 6,
        kind: "fixedUnits",
        components: [
          { symbol: "META", mint: metaMint.toBase58(), unitsPerIndex: oneMeta.toString() },
          { symbol: "OMFG", mint: omfgMint.toBase58(), unitsPerIndex: tenOmfg.toString() },
        ],
      },
    });
    return;
  }

  if (!protocolExists) {
    await send("initializeProtocol", () =>
      program.methods
        .initializeProtocol({ indexCreator: authority })
        .accounts({
          payer: authority,
          authority,
          program: programId,
          programData,
          protocolConfig,
          systemProgram: SystemProgram.programId,
        }),
    );
  } else {
    console.log("protocolConfig already exists; verifying");
  }

  const protocol = await program.account.protocolConfig.fetch(protocolConfig);
  assertKey("protocol authority", protocol.authority, authority);
  assertKey("protocol index creator", protocol.indexCreator, authority);
  assertNumber("permissionless index creation", protocol.permissionlessIndexCreation ? 1 : 0, 0);

  if (!stakingExists) {
    await send("initializeStakingPool", () =>
      program.methods
        .initializeStakingPool()
        .accounts({
          payer: authority,
          authority,
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
        }),
    );
  } else {
    console.log("stakingPool already exists; verifying");
  }

  const staking = await program.account.stakingPool.fetch(stakingPool);
  assertKey("staking authority", staking.authority, authority);
  assertKey("staking basket mint", staking.basketMint, basketMint);
  assertKey("staking reward mint", staking.rewardMint, usdcMint);

  if (!indexExists) {
    await send("createIndex META2", () =>
      program.methods
        .createIndex({
          name,
          symbol,
          metadataUri,
          decimals: 6,
          feeRecipient: authority,
          maxSupply: new anchor.BN(0),
          rebalanceDelaySeconds: new anchor.BN(0),
          kind: { fixedUnits: {} },
          fixedWeightQuoteMint: PublicKey.default,
          fixedWeightRebalanceIntervalSeconds: new anchor.BN(0),
          fixedWeightDriftThresholdBps: 0,
          fixedWeightSpotEmaMaxDeviationBps: 0,
          components: [
            {
              mint: metaMint,
              unitsPerIndex: new anchor.BN(oneMeta.toString()),
              targetWeightBps: 0,
              oraclePair: PublicKey.default,
            },
            {
              mint: omfgMint,
              unitsPerIndex: new anchor.BN(tenOmfg.toString()),
              targetWeightBps: 0,
              oraclePair: PublicKey.default,
            },
          ],
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
    console.log("META2 index already exists; verifying");
  }

  const indexState = await program.account.indexState.fetch(index);
  assertKey("index authority", indexState.authority, authority);
  assertKey("index mint", indexState.indexMint, indexMint);
  assertNumber("index decimals", indexState.decimals, 6);
  if (!("fixedUnits" in indexState.kind)) {
    throw new Error(`index kind mismatch: ${JSON.stringify(indexState.kind)}`);
  }
  if (indexState.name !== name || indexState.symbol !== symbol) {
    throw new Error(`index name/symbol mismatch: ${indexState.name}/${indexState.symbol}`);
  }
  if (indexState.components.length !== 2) {
    throw new Error(`component count mismatch: ${indexState.components.length}`);
  }
  assertKey("META component mint", indexState.components[0].mint, metaMint);
  assertKey("OMFG component mint", indexState.components[1].mint, omfgMint);
  assertNumber("META units", indexState.components[0].unitsPerIndex, oneMeta);
  assertNumber("OMFG units", indexState.components[1].unitsPerIndex, tenOmfg);

  if (!metaVaultExists || !omfgVaultExists) {
    await send("initializeVaults META2", () =>
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
        .remainingAccounts([
          { pubkey: metaMint, isWritable: false, isSigner: false },
          { pubkey: metaVault, isWritable: true, isSigner: false },
          { pubkey: omfgMint, isWritable: false, isSigner: false },
          { pubkey: omfgVault, isWritable: true, isSigner: false },
        ]),
    );
  } else {
    console.log("META2 vaults already exist; verifying");
  }

  if (createMetadata) {
    if (!metadataExists) {
      await send("createIndexMetadata META2", () =>
        program.methods
          .createIndexMetadata({ uri: metadataUri })
          .accounts({
            payer: authority,
            authority,
            index,
            indexMint,
            metadata,
            vaultAuthority,
            metadataProgram,
            systemProgram: SystemProgram.programId,
            rent: SYSVAR_RENT_PUBKEY,
          }),
      );
    } else {
      console.log("META2 metadata account already exists; leaving it unchanged");
    }
  }

  const indexMintAccount = await getMint(connection, indexMint, "confirmed", TOKEN_PROGRAM_ID);
  assertNumber("index mint decimals", indexMintAccount.decimals, 6);
  assertKey("index mint authority", indexMintAccount.mintAuthority, vaultAuthority);
  assertKey("index freeze authority", indexMintAccount.freezeAuthority, vaultAuthority);

  const metaVaultAccount = await getAccount(connection, metaVault, "confirmed", TOKEN_PROGRAM_ID);
  const omfgVaultAccount = await getAccount(connection, omfgVault, "confirmed", TOKEN_PROGRAM_ID);
  assertKey("META vault owner", metaVaultAccount.owner, vaultAuthority);
  assertKey("OMFG vault owner", omfgVaultAccount.owner, vaultAuthority);
  assertKey("META vault mint", metaVaultAccount.mint, metaMint);
  assertKey("OMFG vault mint", omfgVaultAccount.mint, omfgMint);

  console.log("verified:", {
    protocol: {
      authority: protocol.authority.toBase58(),
      indexCreator: protocol.indexCreator.toBase58(),
      permissionlessIndexCreation: protocol.permissionlessIndexCreation,
    },
    stakingPool: stakingPool.toBase58(),
    index: index.toBase58(),
    indexMint: indexMint.toBase58(),
    vaultAuthority: vaultAuthority.toBase58(),
    components: indexState.components.map(displayComponent),
    vaults: {
      META: { address: metaVault.toBase58(), amount: metaVaultAccount.amount.toString() },
      OMFG: { address: omfgVault.toBase58(), amount: omfgVaultAccount.amount.toString() },
    },
    metadata: createMetadata ? metadata.toBase58() : "not requested",
  });
}

await main();
