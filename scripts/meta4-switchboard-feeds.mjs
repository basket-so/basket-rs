import anchor from "@coral-xyz/anchor";
import {
  Connection,
  Keypair,
  PublicKey,
  SYSVAR_INSTRUCTIONS_PUBKEY,
  SYSVAR_SLOT_HASHES_PUBKEY,
  SystemProgram,
  TransactionMessage,
  VersionedTransaction,
} from "@solana/web3.js";
import {
  ASSOCIATED_TOKEN_PROGRAM_ID,
  TOKEN_PROGRAM_ID,
  getAssociatedTokenAddressSync,
} from "@solana/spl-token";
import {
  CrossbarClient,
  CrossbarNetwork,
  OracleFeed,
  OracleJob,
} from "@switchboard-xyz/common";
import { OracleQuote, getDefaultQueue } from "@switchboard-xyz/on-demand";
import fs from "fs";
import path from "path";

const PROGRAM_ID = new PublicKey("5PYVGshoLQrcawa8zyUCe4qTCe1AQJt6Nxkk89yVgkxu");
const META4_SYMBOL = "META4";
const USDC_MINT = new PublicKey("EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v");
const JUPITER_V6 = new PublicKey("JUP6LkbZbjS1jKKwapdHNy74zcZ3tLUZoi5QNyVTaV4");
const SWITCHBOARD_MAX_AGE_SLOTS = 150;

const COMPONENTS = [
  {
    symbol: "META",
    mint: new PublicKey("METAwkXcqyXKy1AtsSgJ8JiUHwGCafnZL38n3vYmeta"),
    unitsPerIndex: 500_000,
  },
  {
    symbol: "OMFG",
    mint: new PublicKey("omfgRBnxHsNJh6YeGbGAmWenNkenzsXyBXm3WDhmeta"),
    unitsPerIndex: 6_000_000,
  },
  {
    symbol: "AVICI",
    mint: new PublicKey("BANKJmvhT8tiJRsBSS1n2HryMBPvT5Ze4HU95DUAmeta"),
    unitsPerIndex: 2_000_000,
  },
  {
    symbol: "UMBRA",
    mint: new PublicKey("PRVT6TB7uss3FrUd2D9xs2zqDBsa3GbMJMwCQsgmeta"),
    unitsPerIndex: 3_000_000,
  },
];

const args = new Set(process.argv.slice(2));
const applyMeta4 = args.has("--apply-meta4");
const updateQuoteOnly = args.has("--update-quote");
const rpcUrl = process.env.SOLANA_RPC_URL ?? "https://api.mainnet-beta.solana.com";
const walletPath = process.env.ANCHOR_WALLET ?? "deployer-keypair.json";
const priceUrlTemplate =
  process.env.META4_PRICE_URL_TEMPLATE ?? "https://lite-api.jup.ag/price/v3?ids={mint}";

function readJson(filePath) {
  return JSON.parse(fs.readFileSync(filePath, "utf8"));
}

function loadKeypair(filePath) {
  return Keypair.fromSecretKey(Uint8Array.from(readJson(filePath)));
}

function feedIdToPubkey(feedId) {
  const hex = feedId.replace(/^0x/, "");
  return new PublicKey(Buffer.from(hex, "hex"));
}

function makeFeed(component) {
  const mint = component.mint.toBase58();
  const url = priceUrlTemplate.replaceAll("{mint}", mint);
  const job = OracleJob.fromObject({
    tasks: [
      {
        httpTask: {
          url,
        },
      },
      {
        jsonParseTask: {
          path: `$['${mint}'].usdPrice`,
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

async function resolveFeeds(crossbar) {
  const feeds = [];

  for (const component of COMPONENTS) {
    const feed = makeFeed(component);
    const simulation = await crossbar.simulateFeed(feed, true, {}, "mainnet");
    if (simulation.error || !simulation.results?.length) {
      throw new Error(
        `${component.symbol} simulation failed: ${simulation.error ?? "no result"}`,
      );
    }

    const stored = await crossbar.storeOracleFeed(feed);
    const oraclePair = feedIdToPubkey(stored.feedId);
    feeds.push({
      ...component,
      feed,
      feedId: stored.feedId,
      cid: stored.cid,
      oraclePair,
      simulatedPrice: simulation.results[0],
    });
  }

  return feeds;
}

async function sendV0(connection, payer, instructions, label) {
  const latest = await connection.getLatestBlockhash("confirmed");
  const message = new TransactionMessage({
    payerKey: payer.publicKey,
    recentBlockhash: latest.blockhash,
    instructions,
  }).compileToV0Message();
  const transaction = new VersionedTransaction(message);
  transaction.sign([payer]);

  const signature = await connection.sendTransaction(transaction, {
    skipPreflight: false,
    preflightCommitment: "confirmed",
    maxRetries: 3,
  });
  await connection.confirmTransaction(
    {
      signature,
      blockhash: latest.blockhash,
      lastValidBlockHeight: latest.lastValidBlockHeight,
    },
    "confirmed",
  );
  console.log(`${label}: ${signature}`);
  return signature;
}

async function managedUpdateContext(connection, crossbar, payer, feedIds) {
  const queue = await getDefaultQueue(connection.rpcEndpoint);
  const [quoteAccount] = OracleQuote.getCanonicalPubkey(queue.pubkey, feedIds);
  const instructions = await queue.fetchManagedUpdateIxs(crossbar, feedIds, {
    payer: payer.publicKey,
    numSignatures: 1,
    instructionIdx: 0,
  });

  return {
    queue,
    quoteAccount,
    instructions,
  };
}

function loadProgram(connection, payer) {
  const wallet = new anchor.Wallet(payer);
  const provider = new anchor.AnchorProvider(connection, wallet, {
    commitment: "confirmed",
    preflightCommitment: "confirmed",
  });
  anchor.setProvider(provider);
  const idlPath = path.join(process.cwd(), "target", "idl", "basket.json");
  const idl = readJson(idlPath);
  return new anchor.Program(idl, provider);
}

async function applyMeta4OraclePairs(connection, crossbar, payer, feeds) {
  const program = loadProgram(connection, payer);
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

  const feedIds = feeds.map((feed) => feed.feedId);
  const components = feeds.map((feed) => ({
    mint: feed.mint,
    unitsPerIndex: new anchor.BN(feed.unitsPerIndex),
    targetWeightBps: 0,
    oraclePair: feed.oraclePair,
  }));
  const prices = feeds.map((feed) => ({
    mint: feed.mint,
    priceNad: null,
  }));
  const mintAccountMetas = feeds.map((feed) => ({
    pubkey: feed.mint,
    isSigner: false,
    isWritable: false,
  }));

  const proposeUpdate = await managedUpdateContext(connection, crossbar, payer, feedIds);
  await sendV0(
    connection,
    payer,
    proposeUpdate.instructions,
    "update Switchboard quote for propose",
  );
  const proposeIx = await program.methods
    .proposeRebalance({
      components,
      quoteMint: USDC_MINT,
      prices,
      oraclePriceToleranceBps: 0,
      navToleranceBps: 0,
      switchboardMaxAgeSlots: new anchor.BN(SWITCHBOARD_MAX_AGE_SLOTS),
    })
    .accounts({
      authority: payer.publicKey,
      index,
      indexMint,
      switchboardQueue: proposeUpdate.queue.pubkey,
      switchboardQuote: proposeUpdate.quoteAccount,
      slothashes: SYSVAR_SLOT_HASHES_PUBKEY,
      instructionsSysvar: SYSVAR_INSTRUCTIONS_PUBKEY,
      systemProgram: SystemProgram.programId,
    })
    .remainingAccounts(mintAccountMetas)
    .instruction();

  await sendV0(
    connection,
    payer,
    [proposeIx],
    "propose META4 oracle rebalance",
  );

  const executeUpdate = await managedUpdateContext(connection, crossbar, payer, feedIds);
  await sendV0(
    connection,
    payer,
    executeUpdate.instructions,
    "update Switchboard quote for execute",
  );
  const executeRemainingAccounts = feeds.flatMap((feed) => {
    const vault = getAssociatedTokenAddressSync(
      feed.mint,
      vaultAuthority,
      true,
      TOKEN_PROGRAM_ID,
      ASSOCIATED_TOKEN_PROGRAM_ID,
    );
    return [
      { pubkey: feed.mint, isSigner: false, isWritable: false },
      { pubkey: vault, isSigner: false, isWritable: true },
      { pubkey: TOKEN_PROGRAM_ID, isSigner: false, isWritable: false },
    ];
  });
  const executeIx = await program.methods
    .executeRebalance({
      swaps: [],
      prices,
      switchboardMaxAgeSlots: new anchor.BN(SWITCHBOARD_MAX_AGE_SLOTS),
    })
    .accounts({
      authority: payer.publicKey,
      index,
      indexMint,
      vaultAuthority,
      jupiterProgram: JUPITER_V6,
      switchboardQueue: executeUpdate.queue.pubkey,
      switchboardQuote: executeUpdate.quoteAccount,
      slothashes: SYSVAR_SLOT_HASHES_PUBKEY,
      instructionsSysvar: SYSVAR_INSTRUCTIONS_PUBKEY,
      associatedTokenProgram: ASSOCIATED_TOKEN_PROGRAM_ID,
      tokenProgram: TOKEN_PROGRAM_ID,
      systemProgram: SystemProgram.programId,
    })
    .remainingAccounts(executeRemainingAccounts)
    .instruction();

  await sendV0(
    connection,
    payer,
    [executeIx],
    "execute META4 oracle rebalance",
  );
}

const connection = new Connection(rpcUrl, "confirmed");
const payer = loadKeypair(walletPath);
const crossbar = CrossbarClient.default();
crossbar.setNetwork(CrossbarNetwork.SolanaMainnet);

const feeds = await resolveFeeds(crossbar);
const feedIds = feeds.map((feed) => feed.feedId);
const queue = await getDefaultQueue(connection.rpcEndpoint);
const [quoteAccount] = OracleQuote.getCanonicalPubkey(queue.pubkey, feedIds);

console.log(
  JSON.stringify(
    {
      rpcUrl,
      payer: payer.publicKey.toBase58(),
      switchboardQueue: queue.pubkey.toBase58(),
      switchboardQuoteForAllFeeds: quoteAccount.toBase58(),
      feeds: feeds.map((feed) => ({
        symbol: feed.symbol,
        mint: feed.mint.toBase58(),
        simulatedPrice: feed.simulatedPrice,
        feedId: feed.feedId,
        oraclePair: feed.oraclePair.toBase58(),
        cid: feed.cid,
      })),
    },
    null,
    2,
  ),
);

if (updateQuoteOnly) {
  const update = await managedUpdateContext(connection, crossbar, payer, feedIds);
  await sendV0(connection, payer, update.instructions, "update Switchboard quote");
}

if (applyMeta4) {
  await applyMeta4OraclePairs(connection, crossbar, payer, feeds);
}
