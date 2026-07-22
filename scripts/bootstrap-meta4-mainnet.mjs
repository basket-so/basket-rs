import anchor from "@coral-xyz/anchor";
import {
  Connection,
  Keypair,
  PublicKey,
  SystemProgram,
} from "@solana/web3.js";
import {
  ASSOCIATED_TOKEN_PROGRAM_ID,
  TOKEN_PROGRAM_ID,
  getAssociatedTokenAddressSync,
} from "@solana/spl-token";
import fs from "fs";
import path from "path";

const PROGRAM_ID = new PublicKey("gnnZfKE3rrrK6HFhSrmRafN1LfWUdfyRp7cBzNyPAdP");
const BPF_LOADER_UPGRADEABLE = new PublicKey(
  "BPFLoaderUpgradeab1e11111111111111111111111",
);
const BASKET_MINT = new PublicKey("5yTFbtAE5RDjxpiVpDfyWuzcCWgwh659CEu7a7ZQtSpk");
const USDC_MINT = new PublicKey("EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v");
const META4_SYMBOL = "META4";

const COMPONENTS = [
  {
    symbol: "META",
    mint: new PublicKey("METAwkXcqyXKy1AtsSgJ8JiUHwGCafnZL38n3vYmeta"),
    unitsPerIndex: 500_000,
    oraclePair: new PublicKey("Bb7R18nNobdKAdqWA7A34pYK6gwoGNw5w1YAiJW2wBh4"),
  },
  {
    symbol: "OMFG",
    mint: new PublicKey("omfgRBnxHsNJh6YeGbGAmWenNkenzsXyBXm3WDhmeta"),
    unitsPerIndex: 6_000_000,
    oraclePair: new PublicKey("G972Zgvn24tE2f6boCEhirGZtHRLW9gtq8x4NVTHee4Q"),
  },
  {
    symbol: "AVICI",
    mint: new PublicKey("BANKJmvhT8tiJRsBSS1n2HryMBPvT5Ze4HU95DUAmeta"),
    unitsPerIndex: 2_000_000,
    oraclePair: new PublicKey("9HjTqZZkf8RVbE464CL9AhqmvR1eq5i6m5CcSRhVZ6PY"),
  },
  {
    symbol: "UMBRA",
    mint: new PublicKey("PRVT6TB7uss3FrUd2D9xs2zqDBsa3GbMJMwCQsgmeta"),
    unitsPerIndex: 3_000_000,
    oraclePair: new PublicKey("9Jhi7DTXb6vkxLuH5KThjCfak2kgkvAxYsFnMvJMh5nx"),
  },
];

const args = new Set(process.argv.slice(2));
const dryRun = args.has("--dry-run");
const rpcUrl = process.env.SOLANA_RPC_URL ?? "https://api.mainnet-beta.solana.com";
const walletPath = process.env.ANCHOR_WALLET ?? "deployer-keypair.json";

function readJson(filePath) {
  return JSON.parse(fs.readFileSync(filePath, "utf8"));
}

function loadKeypair(filePath) {
  return Keypair.fromSecretKey(Uint8Array.from(readJson(filePath)));
}

async function accountExists(connection, address) {
  return Boolean(await connection.getAccountInfo(address, "confirmed"));
}

async function runStep(label, callback) {
  if (dryRun) {
    console.log(`[dry-run] ${label}`);
    return null;
  }
  console.log(`[send] ${label}`);
  const signature = await callback();
  console.log(`[ok] ${label}: ${signature}`);
  return signature;
}

function loadProgram(connection, payer) {
  const wallet = new anchor.Wallet(payer);
  const provider = new anchor.AnchorProvider(connection, wallet, {
    commitment: "confirmed",
    preflightCommitment: "confirmed",
  });
  anchor.setProvider(provider);
  const idl = readJson(path.join(process.cwd(), "target", "idl", "basket.json"));
  return new anchor.Program(idl, provider);
}

const connection = new Connection(rpcUrl, "confirmed");
const payer = loadKeypair(walletPath);
const program = loadProgram(connection, payer);

if (!program.programId.equals(PROGRAM_ID)) {
  throw new Error(`IDL program id ${program.programId.toBase58()} does not match ${PROGRAM_ID}`);
}

const [programData] = PublicKey.findProgramAddressSync(
  [PROGRAM_ID.toBuffer()],
  BPF_LOADER_UPGRADEABLE,
);
const [protocolConfig] = PublicKey.findProgramAddressSync(
  [Buffer.from("protocol-config")],
  PROGRAM_ID,
);
const [stakingPool] = PublicKey.findProgramAddressSync(
  [Buffer.from("staking-pool")],
  PROGRAM_ID,
);
const [stakingAuthority] = PublicKey.findProgramAddressSync(
  [Buffer.from("staking-authority")],
  PROGRAM_ID,
);
const [index] = PublicKey.findProgramAddressSync(
  [Buffer.from("index"), payer.publicKey.toBuffer(), Buffer.from(META4_SYMBOL)],
  PROGRAM_ID,
);
const [indexMint] = PublicKey.findProgramAddressSync(
  [Buffer.from("index-mint"), index.toBuffer()],
  PROGRAM_ID,
);
const [vaultAuthority] = PublicKey.findProgramAddressSync(
  [Buffer.from("vault-authority"), index.toBuffer()],
  PROGRAM_ID,
);

const stakeVault = getAssociatedTokenAddressSync(
  BASKET_MINT,
  stakingAuthority,
  true,
  TOKEN_PROGRAM_ID,
  ASSOCIATED_TOKEN_PROGRAM_ID,
);
const rewardVault = getAssociatedTokenAddressSync(
  USDC_MINT,
  stakingAuthority,
  true,
  TOKEN_PROGRAM_ID,
  ASSOCIATED_TOKEN_PROGRAM_ID,
);
const componentVaults = COMPONENTS.map((component) =>
  getAssociatedTokenAddressSync(
    component.mint,
    vaultAuthority,
    true,
    TOKEN_PROGRAM_ID,
    ASSOCIATED_TOKEN_PROGRAM_ID,
  ),
);

console.log(
  JSON.stringify(
    {
      rpcUrl,
      dryRun,
      payer: payer.publicKey.toBase58(),
      program: PROGRAM_ID.toBase58(),
      programData: programData.toBase58(),
      protocolConfig: protocolConfig.toBase58(),
      index: index.toBase58(),
      indexMint: indexMint.toBase58(),
      vaultAuthority: vaultAuthority.toBase58(),
      stakingPool: stakingPool.toBase58(),
      stakingAuthority: stakingAuthority.toBase58(),
      stakeVault: stakeVault.toBase58(),
      rewardVault: rewardVault.toBase58(),
      components: COMPONENTS.map((component, index) => ({
        symbol: component.symbol,
        mint: component.mint.toBase58(),
        unitsPerIndex: component.unitsPerIndex,
        oraclePair: component.oraclePair.toBase58(),
        vault: componentVaults[index].toBase58(),
      })),
    },
    null,
    2,
  ),
);

if (!(await accountExists(connection, protocolConfig))) {
  await runStep("initialize protocol", () =>
    program.methods
      .initializeProtocol({ indexCreator: payer.publicKey })
      .accounts({
        payer: payer.publicKey,
        authority: payer.publicKey,
        program: PROGRAM_ID,
        programData,
        protocolConfig,
        systemProgram: SystemProgram.programId,
      })
      .rpc(),
  );
} else {
  console.log(`[skip] protocol already initialized: ${protocolConfig.toBase58()}`);
}

if (!(await accountExists(connection, index))) {
  await runStep("create META4 fixed-units index", () =>
    program.methods
      .createIndex({
        name: "META4",
        symbol: META4_SYMBOL,
        metadataUri: "https://omnipair.fi/meta4.json",
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
        components: COMPONENTS.map((component) => ({
          mint: component.mint,
          unitsPerIndex: new anchor.BN(component.unitsPerIndex),
          targetWeightBps: 0,
          oraclePair: component.oraclePair,
        })),
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
      .rpc(),
  );
} else {
  console.log(`[skip] META4 index already exists: ${index.toBase58()}`);
}

const vaultInfos = await Promise.all(
  componentVaults.map((vault) => accountExists(connection, vault)),
);
if (vaultInfos.some((exists) => !exists)) {
  const remainingAccounts = COMPONENTS.flatMap((component, i) => [
    { pubkey: component.mint, isWritable: false, isSigner: false },
    { pubkey: componentVaults[i], isWritable: true, isSigner: false },
  ]);
  await runStep("initialize META4 component vaults", () =>
    program.methods
      .initializeVaults()
      .accounts({
        payer: payer.publicKey,
        index,
        vaultAuthority,
        associatedTokenProgram: ASSOCIATED_TOKEN_PROGRAM_ID,
        tokenProgram: TOKEN_PROGRAM_ID,
        systemProgram: SystemProgram.programId,
      })
      .remainingAccounts(remainingAccounts)
      .rpc(),
  );
} else {
  console.log("[skip] all META4 component vaults already exist");
}

if (!(await accountExists(connection, stakingPool))) {
  await runStep("initialize staking pool", () =>
    program.methods
      .initializeStakingPool()
      .accounts({
        payer: payer.publicKey,
        authority: payer.publicKey,
        protocolConfig,
        stakingPool,
        stakingAuthority,
        basketMint: BASKET_MINT,
        rewardMint: USDC_MINT,
        stakeVault,
        rewardVault,
        associatedTokenProgram: ASSOCIATED_TOKEN_PROGRAM_ID,
        tokenProgram: TOKEN_PROGRAM_ID,
        systemProgram: SystemProgram.programId,
      })
      .rpc(),
  );
} else {
  console.log(`[skip] staking pool already exists: ${stakingPool.toBase58()}`);
}

if (dryRun) {
  process.exit(0);
}

const finalIndex = await program.account.indexState.fetch(index);
const finalProtocol = await program.account.protocolConfig.fetch(protocolConfig);
const finalStakingPool = await program.account.stakingPool.fetch(stakingPool);

console.log(
  JSON.stringify(
    {
      protocol: {
        address: protocolConfig.toBase58(),
        authority: finalProtocol.authority.toBase58(),
        indexCreator: finalProtocol.indexCreator.toBase58(),
        permissionlessIndexCreation: finalProtocol.permissionlessIndexCreation,
      },
      index: {
        address: index.toBase58(),
        indexMint: finalIndex.indexMint.toBase58(),
        authority: finalIndex.authority.toBase58(),
        symbol: finalIndex.symbol,
        decimals: finalIndex.decimals,
        componentCount: finalIndex.componentCount,
        largeBasketComponentCount: finalIndex.largeBasketComponentCount,
        components: finalIndex.components.map((component, i) => ({
          symbol: COMPONENTS[i]?.symbol,
          mint: component.mint.toBase58(),
          unitsPerIndex: component.unitsPerIndex.toString(),
          oraclePair: component.oraclePair.toBase58(),
          vault: componentVaults[i]?.toBase58(),
        })),
      },
      stakingPool: {
        address: stakingPool.toBase58(),
        authority: finalStakingPool.authority.toBase58(),
        basketMint: finalStakingPool.basketMint.toBase58(),
        rewardMint: finalStakingPool.rewardMint.toBase58(),
        stakeVault: stakeVault.toBase58(),
        rewardVault: rewardVault.toBase58(),
      },
    },
    null,
    2,
  ),
);
