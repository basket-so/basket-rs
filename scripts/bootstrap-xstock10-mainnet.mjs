import anchor from "@coral-xyz/anchor";
import {
  AddressLookupTableProgram,
  Connection,
  ComputeBudgetProgram,
  Keypair,
  PublicKey,
  SystemProgram,
  TransactionMessage,
  VersionedTransaction,
} from "@solana/web3.js";
import {
  ASSOCIATED_TOKEN_PROGRAM_ID,
  TOKEN_2022_PROGRAM_ID,
  TOKEN_PROGRAM_ID,
  getAssociatedTokenAddressSync,
} from "@solana/spl-token";
import {
  CrossbarClient,
  CrossbarNetwork,
  OracleFeed,
  OracleJob,
} from "@switchboard-xyz/common";
import fs from "fs";
import path from "path";

const PROGRAM_ID = new PublicKey("9LNEoShrH93XekWQTFmZBdUdMu8ugJxBr5cbfqJQC1mw");
const BPF_LOADER_UPGRADEABLE = new PublicKey(
  "BPFLoaderUpgradeab1e11111111111111111111111",
);
const INDEX_NAME = "xStocks US Top 10";
const INDEX_SYMBOL = "USTOP10";
const METADATA_URI = "https://omnipair.fi/xstock10.json";
const TARGET_COMPONENT_USD = 100;
const XSTOCKS_API = "https://api.xstocks.fi/api/v2";
const DEXSCREENER_TOKENS_API = "https://api.dexscreener.com/latest/dex/tokens";
// Each Switchboard feed uses a three-source fallback chain (xStocks -> DexScreener
// -> Jupiter) via nested conditionalTask, so no single source rate-limiting or
// going down can kill a feed. The original single-source (xStocks-only) feeds
// died whenever that one source failed for a symbol. xStocks is primary (true
// equity quote); DexScreener and Jupiter are on-chain DEX-price fallbacks.
const JUPITER_PRICE_API = process.env.JUPITER_PRICE_API ?? "https://lite-api.jup.ag/price/v3";

const COMPONENTS = [
  {
    symbol: "NVDAx",
    name: "NVIDIA xStock",
    underlyingSymbol: "NVDA",
    mint: new PublicKey("Xsc9qvGR1efVDFGLrVsmkzv3qi45LTBjeUKSPmx9qEh"),
  },
  {
    symbol: "GOOGLx",
    name: "Alphabet xStock",
    underlyingSymbol: "GOOGL",
    mint: new PublicKey("XsCPL9dNWBMvFtTmwcCA5v3xWPSMEBCszbQdiLLq6aN"),
  },
  {
    symbol: "AAPLx",
    name: "Apple xStock",
    underlyingSymbol: "AAPL",
    mint: new PublicKey("XsbEhLAtcf6HdfpFZ5xEMdqW8nfAvcsP5bdudRLJzJp"),
  },
  {
    symbol: "MSFTx",
    name: "Microsoft xStock",
    underlyingSymbol: "MSFT",
    mint: new PublicKey("XspzcW1PRtgf6Wj92HCiZdjzKCyFekVD8P5Ueh3dRMX"),
  },
  {
    symbol: "AMZNx",
    name: "Amazon xStock",
    underlyingSymbol: "AMZN",
    mint: new PublicKey("Xs3eBt7uRfJX8QUs4suhyU8p2M6DoUDrJyWBa8LLZsg"),
  },
  {
    symbol: "MSTRx",
    name: "MicroStrategy xStock",
    underlyingSymbol: "MSTR",
    mint: new PublicKey("XsP7xzNPvEHS1m6qfanPUGjNmdnmsLKEoNAnHjdxxyZ"),
  },
  {
    symbol: "AVGOx",
    name: "Broadcom xStock",
    underlyingSymbol: "AVGO",
    mint: new PublicKey("XsgSaSvNSqLTtFuyWPBhK9196Xb9Bbdyjj4fH3cPJGo"),
  },
  {
    symbol: "TSLAx",
    name: "Tesla xStock",
    underlyingSymbol: "TSLA",
    mint: new PublicKey("XsDoVfqeBukxuZHWhdvWHBhgEHjGNst4MLodqsJHzoB"),
  },
  {
    symbol: "METAx",
    name: "Meta xStock",
    underlyingSymbol: "META",
    mint: new PublicKey("Xsa62P5mvPszXL1krVUnU5ar38bBSVcWAB6fmPCo5Zu"),
  },
  {
    symbol: "BRK.Bx",
    name: "Berkshire Hathaway xStock",
    underlyingSymbol: "BRK.B",
    mint: new PublicKey("Xs6B6zawENwAbWVi7w92rjazLuAr5Az59qgWKcNb45x"),
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

async function fetchJson(url, attempts = 4) {
  let lastError;

  for (let attempt = 1; attempt <= attempts; attempt += 1) {
    try {
      const response = await fetch(url, {
        headers: { accept: "application/json" },
      });

      if (response.ok) {
        return response.json();
      }

      const body = await response.text();
      lastError = new Error(`${url} returned ${response.status}: ${body}`);
      if (response.status < 500 || attempt === attempts) {
        throw lastError;
      }
    } catch (error) {
      lastError = error;
      if (attempt === attempts) {
        throw error;
      }
    }

    await new Promise((resolve) => setTimeout(resolve, attempt * 2_000));
  }

  throw lastError;
}

async function accountExists(connection, address) {
  return Boolean(await connection.getAccountInfo(address, "confirmed"));
}

function feedIdToPubkey(feedId) {
  const hex = feedId.replace(/^0x/, "");
  return new PublicKey(Buffer.from(hex, "hex"));
}

function jupiterPriceUrl(mint) {
  return `${JUPITER_PRICE_API}?ids=${encodeURIComponent(mint)}`;
}

function xstocksPriceUrl(symbol) {
  return `${XSTOCKS_API}/public/assets/${encodeURIComponent(symbol)}/price-data`;
}

function dexscreenerTokenUrl(mint) {
  return `${DEXSCREENER_TOKENS_API}/${encodeURIComponent(mint)}`;
}

// One source = an httpTask + jsonParseTask pair that resolves to a USD price.
function xstocksTasks(symbol) {
  return [
    { httpTask: { url: xstocksPriceUrl(symbol) } },
    { jsonParseTask: { path: "$.quote" } },
  ];
}

function dexscreenerTasks(mint) {
  return [
    { httpTask: { url: dexscreenerTokenUrl(mint) } },
    // Switchboard's JsonParseTask does NOT support JSONPath filter predicates
    // (e.g. [?(@.quoteToken.symbol=='USDC')] resolves to empty). Instead take the
    // MEDIAN price across all pairs, which is robust to a single off-market pair.
    { jsonParseTask: { path: "$.pairs[*].priceUsd", aggregationMethod: "MEDIAN" } },
  ];
}

function jupiterTasks(mint) {
  return [
    { httpTask: { url: jupiterPriceUrl(mint) } },
    { jsonParseTask: { path: `$['${mint}'].usdPrice` } },
  ];
}

function makeXstockFeed(component) {
  const mint = component.mint.toBase58();
  // Ordered fallback: try xStocks; on failure try DexScreener; on failure Jupiter.
  // ConditionalTask runs `attempt`; if it throws, runs `onFailure`.
  const job = OracleJob.fromObject({
    tasks: [
      {
        conditionalTask: {
          attempt: xstocksTasks(component.symbol),
          onFailure: [
            {
              conditionalTask: {
                attempt: dexscreenerTasks(mint),
                onFailure: jupiterTasks(mint),
              },
            },
          ],
        },
      },
    ],
  });

  return OracleFeed.create({
    name: `${component.symbol}/USD`,
    jobs: [job],
    minOracleSamples: 1,
    minJobResponses: 1,
    maxJobRangePct: 0,
  });
}

async function resolveXstockAsset(component) {
  const mint = component.mint.toBase58();
  const payload = await fetchJson(jupiterPriceUrl(mint));
  const price = Number(payload?.[mint]?.usdPrice);
  if (!Number.isFinite(price) || price <= 0) {
    throw new Error(`${component.symbol} Jupiter usdPrice is invalid.`);
  }

  return {
    ...component,
    price,
  };
}

async function resolveMintInfo(connection, mint) {
  const account = await connection.getAccountInfo(mint, "confirmed");
  if (!account) {
    throw new Error(`Mint account ${mint.toBase58()} does not exist.`);
  }

  const parsed = await connection.getParsedAccountInfo(mint, "confirmed");
  const decimals = parsed.value?.data?.parsed?.info?.decimals;
  if (!Number.isInteger(decimals)) {
    throw new Error(`Mint account ${mint.toBase58()} did not parse with decimals.`);
  }

  return {
    decimals,
    tokenProgram: account.owner,
  };
}

async function resolveSwitchboardFeed(crossbar, component, attempts = 5) {
  const feed = makeXstockFeed(component);
  // simulateFeed drives the Switchboard oracle's own fetch of the data source.
  // The free lite-api.jup.ag tier rate-limits bursts, so a feed can come back
  // with no result purely transiently. Retry with backoff before giving up.
  let lastDetail = "no result";
  for (let attempt = 1; attempt <= attempts; attempt += 1) {
    let simulation;
    try {
      simulation = await crossbar.simulateFeed(feed, true, {}, "mainnet");
    } catch (error) {
      lastDetail = error instanceof Error ? error.message : String(error);
      simulation = null;
    }

    if (simulation && !simulation.error && simulation.results?.length) {
      const stored = await crossbar.storeOracleFeed(feed);
      return {
        cid: stored.cid,
        feedId: stored.feedId,
        oraclePair: feedIdToPubkey(stored.feedId),
        simulatedPrice: simulation.results[0],
      };
    }

    lastDetail = simulation?.error ?? lastDetail;
    if (attempt < attempts) {
      const backoff = Math.min(8_000, 750 * 2 ** (attempt - 1));
      console.log(
        `[retry] ${component.symbol} feed simulation (${attempt}/${attempts}): ${lastDetail} — waiting ${backoff}ms`,
      );
      await new Promise((resolve) => setTimeout(resolve, backoff));
    }
  }

  throw new Error(
    `${component.symbol} Switchboard simulation failed after ${attempts} attempts: ${lastDetail}`,
  );
}

function unitsForEqualWeight(price, decimals) {
  const atoms = Math.round((TARGET_COMPONENT_USD / price) * 10 ** decimals);
  if (!Number.isSafeInteger(atoms) || atoms <= 0) {
    throw new Error(`Calculated unit amount is unsafe for price ${price} and decimals ${decimals}.`);
  }
  return BigInt(atoms);
}

function uiAmount(atoms, decimals) {
  const scale = 10n ** BigInt(decimals);
  const whole = atoms / scale;
  const fraction = atoms % scale;

  if (fraction === 0n) {
    return whole.toString();
  }

  return `${whole}.${fraction.toString().padStart(decimals, "0").replace(/0+$/, "")}`;
}

function uniquePublicKeys(keys) {
  const seen = new Set();
  const unique = [];

  for (const key of keys) {
    const value = key.toBase58();
    if (!seen.has(value)) {
      seen.add(value);
      unique.push(key);
    }
  }

  return unique;
}

async function sendV0(connection, payer, instructions, label, lookupTables = []) {
  const latest = await connection.getLatestBlockhash("confirmed");
  const message = new TransactionMessage({
    payerKey: payer.publicKey,
    recentBlockhash: latest.blockhash,
    instructions,
  }).compileToV0Message(lookupTables);
  const transaction = new VersionedTransaction(message);
  transaction.sign([payer]);

  console.log(`[send] ${label} (${transaction.serialize().length} bytes)`);
  const signature = await connection.sendTransaction(transaction, {
    skipPreflight: false,
    preflightCommitment: "confirmed",
    maxRetries: 10,
  });
  await connection.confirmTransaction(
    {
      signature,
      blockhash: latest.blockhash,
      lastValidBlockHeight: latest.lastValidBlockHeight,
    },
    "confirmed",
  );
  console.log(`[ok] ${label}: ${signature}`);
  return signature;
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

async function createLookupTableForInstruction(connection, payer, instruction) {
  const recentSlot = await connection.getSlot("finalized");
  const [createIx, lookupTableAddress] = AddressLookupTableProgram.createLookupTable({
    authority: payer.publicKey,
    payer: payer.publicKey,
    recentSlot,
  });
  await sendV0(connection, payer, [createIx], `create ${INDEX_SYMBOL} lookup table`);

  const lookupAddresses = uniquePublicKeys(
    instruction.keys
      .filter((meta) => !meta.isSigner)
      .map((meta) => meta.pubkey),
  );

  for (let i = 0; i < lookupAddresses.length; i += 20) {
    const batch = lookupAddresses.slice(i, i + 20);
    const extendIx = AddressLookupTableProgram.extendLookupTable({
      authority: payer.publicKey,
      lookupTable: lookupTableAddress,
      payer: payer.publicKey,
      addresses: batch,
    });
    await sendV0(
      connection,
      payer,
      [extendIx],
      `extend ${INDEX_SYMBOL} lookup table ${i / 20 + 1}`,
    );
  }

  const minUsableSlot = (await connection.getSlot("confirmed")) + 1;
  while ((await connection.getSlot("confirmed")) <= minUsableSlot) {
    await new Promise((resolve) => setTimeout(resolve, 500));
  }

  const lookup = await connection.getAddressLookupTable(lookupTableAddress, {
    commitment: "confirmed",
  });
  if (!lookup.value) {
    throw new Error(`Lookup table ${lookupTableAddress.toBase58()} was not readable.`);
  }

  return lookup.value;
}

const connection = new Connection(rpcUrl, "confirmed");
const payer = loadKeypair(walletPath);
const program = loadProgram(connection, payer);
const crossbar = CrossbarClient.default();
crossbar.setNetwork(CrossbarNetwork.SolanaMainnet);

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
const [index] = PublicKey.findProgramAddressSync(
  [Buffer.from("index"), payer.publicKey.toBuffer(), Buffer.from(INDEX_SYMBOL)],
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
const [page] = PublicKey.findProgramAddressSync(
  [
    Buffer.from("large-basket-component-page"),
    index.toBuffer(),
    Buffer.from([0]),
  ],
  PROGRAM_ID,
);

// Staking pool PDAs. The large-basket mint/redeem flow collects the staking
// fee into this pool, so it must exist before the first USTOP10 mint. These
// match the program's hardcoded BASKET_MINT / USDC_MINT constants.
const STAKING_BASKET_MINT = new PublicKey("5yTFbtAE5RDjxpiVpDfyWuzcCWgwh659CEu7a7ZQtSpk");
const USDC_MINT = new PublicKey("EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v");
const [stakingPool] = PublicKey.findProgramAddressSync(
  [Buffer.from("staking-pool")],
  PROGRAM_ID,
);
const [stakingAuthority] = PublicKey.findProgramAddressSync(
  [Buffer.from("staking-authority")],
  PROGRAM_ID,
);
const stakeVault = getAssociatedTokenAddressSync(
  STAKING_BASKET_MINT,
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

const components = [];
for (const [i, component] of COMPONENTS.entries()) {
  // Pace feed resolution so we don't burst the free Jupiter lite-API tier that
  // the Switchboard oracle fetches from during simulateFeed.
  if (i > 0) {
    await new Promise((resolve) => setTimeout(resolve, 1_200));
  }
  const asset = await resolveXstockAsset(component);
  const mintInfo = await resolveMintInfo(connection, asset.mint);
  const feed = await resolveSwitchboardFeed(crossbar, asset);
  const unitsPerIndex = unitsForEqualWeight(asset.price, mintInfo.decimals);
  const vault = getAssociatedTokenAddressSync(
    asset.mint,
    vaultAuthority,
    true,
    mintInfo.tokenProgram,
    ASSOCIATED_TOKEN_PROGRAM_ID,
  );

  components.push({
    ...asset,
    ...mintInfo,
    ...feed,
    unitsPerIndex,
    vault,
  });
}

const totalTargetValue = components.reduce(
  (sum, component) =>
    sum + Number(component.unitsPerIndex) / 10 ** component.decimals * component.price,
  0,
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
      page: page.toBase58(),
      targetComponentUsd: TARGET_COMPONENT_USD,
      estimatedIndexNavUsd: totalTargetValue,
      components: components.map((component) => ({
        symbol: component.symbol,
        underlyingSymbol: component.underlyingSymbol,
        mint: component.mint.toBase58(),
        tokenProgram: component.tokenProgram.toBase58(),
        decimals: component.decimals,
        price: component.price,
        unitsPerIndex: component.unitsPerIndex.toString(),
        unitsPerIndexUi: uiAmount(component.unitsPerIndex, component.decimals),
        estimatedValueUsd:
          Number(component.unitsPerIndex) / 10 ** component.decimals * component.price,
        estimatedWeight:
          (Number(component.unitsPerIndex) / 10 ** component.decimals * component.price) /
          totalTargetValue,
        oraclePair: component.oraclePair.toBase58(),
        feedId: component.feedId,
        simulatedPrice: component.simulatedPrice,
        vault: component.vault.toBase58(),
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
        basketMint: STAKING_BASKET_MINT,
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

if (!(await accountExists(connection, index))) {
  await runStep(`create ${INDEX_SYMBOL} large fixed-units index`, () =>
    program.methods
      .createLargeBasketIndex({
        name: INDEX_NAME,
        symbol: INDEX_SYMBOL,
        metadataUri: METADATA_URI,
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
        componentCount: components.length,
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
  console.log(`[skip] ${INDEX_SYMBOL} index already exists: ${index.toBase58()}`);
}

if (!(await accountExists(connection, page))) {
  const initializePageIx = await program.methods
    .initializeLargeBasketComponentPage({
      pageIndex: 0,
      startComponentIndex: 0,
      components: components.map((component) => ({
        mint: component.mint,
        unitsPerIndex: new anchor.BN(component.unitsPerIndex.toString()),
        targetWeightBps: 0,
        oraclePair: component.oraclePair,
      })),
    })
    .accounts({
      payer: payer.publicKey,
      authority: payer.publicKey,
      index,
      indexMint,
      vaultAuthority,
      page,
      associatedTokenProgram: ASSOCIATED_TOKEN_PROGRAM_ID,
      systemProgram: SystemProgram.programId,
    })
    .remainingAccounts(
      components.flatMap((component) => [
        { pubkey: component.mint, isWritable: false, isSigner: false },
        { pubkey: component.vault, isWritable: true, isSigner: false },
        { pubkey: component.tokenProgram, isWritable: false, isSigner: false },
      ]),
    )
    .instruction();

  if (dryRun) {
    console.log(`[dry-run] initialize ${INDEX_SYMBOL} component page`);
  } else {
    const lookupTable = await createLookupTableForInstruction(connection, payer, initializePageIx);
    await sendV0(
      connection,
      payer,
      [
        ComputeBudgetProgram.setComputeUnitLimit({ units: 1_400_000 }),
        ComputeBudgetProgram.setComputeUnitPrice({ microLamports: 5_000 }),
        initializePageIx,
      ],
      `initialize ${INDEX_SYMBOL} component page`,
      [lookupTable],
    );
    console.log(`[ok] ${INDEX_SYMBOL} lookup table: ${lookupTable.key.toBase58()}`);
  }
} else {
  console.log(`[skip] ${INDEX_SYMBOL} component page already exists: ${page.toBase58()}`);
}

if (dryRun) {
  process.exit(0);
}

const finalIndexBeforeFinalize = await program.account.indexState.fetch(index);
if (!finalIndexBeforeFinalize.largeBasketConfigured) {
  await runStep(`finalize ${INDEX_SYMBOL} large basket config`, () =>
    program.methods
      .finalizeLargeBasketConfig()
      .accounts({
        authority: payer.publicKey,
        index,
      })
      .remainingAccounts([
        { pubkey: page, isWritable: true, isSigner: false },
      ])
      .rpc(),
  );
} else {
  console.log(`[skip] ${INDEX_SYMBOL} large basket config already finalized`);
}

const finalIndex = await program.account.indexState.fetch(index);
const finalPage = await program.account.largeBasketComponentPage.fetch(page);
const vaultBalances = await Promise.all(
  components.map((component) => connection.getTokenAccountBalance(component.vault, "confirmed")
    .catch(() => null)),
);

console.log(
  JSON.stringify(
    {
      index: {
        address: index.toBase58(),
        indexMint: finalIndex.indexMint.toBase58(),
        authority: finalIndex.authority.toBase58(),
        symbol: finalIndex.symbol,
        decimals: finalIndex.decimals,
        componentCount: finalIndex.componentCount,
        largeBasketComponentCount: finalIndex.largeBasketComponentCount,
        largeBasketPageCount: finalIndex.largeBasketPageCount,
        largeBasketConfigured: finalIndex.largeBasketConfigured,
      },
      page: {
        address: page.toBase58(),
        pageIndex: finalPage.pageIndex,
        startComponentIndex: finalPage.startComponentIndex,
        componentCount: finalPage.componentCount,
        finalized: finalPage.finalized,
      },
      components: components.map((component, i) => ({
        symbol: component.symbol,
        mint: component.mint.toBase58(),
        tokenProgram: component.tokenProgram.toBase58(),
        unitsPerIndex: component.unitsPerIndex.toString(),
        unitsPerIndexUi: uiAmount(component.unitsPerIndex, component.decimals),
        oraclePair: component.oraclePair.toBase58(),
        vault: component.vault.toBase58(),
        vaultAmount: vaultBalances[i]?.value?.amount ?? "0",
      })),
    },
    null,
    2,
  ),
);
