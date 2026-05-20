import fs from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";

import anchor from "@coral-xyz/anchor";
import { Keypair, PublicKey } from "@solana/web3.js";
import { getMint, TOKEN_PROGRAM_ID } from "@solana/spl-token";

const __dirname = path.dirname(fileURLToPath(import.meta.url));
const root = path.resolve(__dirname, "..");

const programId = new PublicKey("H6JKCZU82gCADQZ98Jmfj7UzpHTdbyfnDpt7AD5NZ3Lt");
const index = new PublicKey("BGfKiQCSyBzqkG2nKnAV5V72R57gwvYNAiPfBcELqMz");
const indexMint = new PublicKey("5fFBHceUp33Su7Bu2QkFTqGDyVtbUJsM6dNRUJyrgw4R");
const usdcMint = new PublicKey("EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v");

const components = [
  {
    symbol: "UMBRA",
    mint: new PublicKey("PRVT6TB7uss3FrUd2D9xs2zqDBsa3GbMJMwCQsgmeta"),
    pair: new PublicKey("DLdMKytiJvVgivmQBhwfKEU4hgEWjhGURS16kTeEkjPP"),
  },
  {
    symbol: "OMFG",
    mint: new PublicKey("omfgRBnxHsNJh6YeGbGAmWenNkenzsXyBXm3WDhmeta"),
    pair: new PublicKey("BZi9iPbcpLWDkHs8nNQe5pb9MxLQe8fbgHtypj2pL4UB"),
  },
  {
    symbol: "META",
    mint: new PublicKey("METAwkXcqyXKy1AtsSgJ8JiUHwGCafnZL38n3vYmeta"),
    pair: new PublicKey("Cp2nGCWWfqkUmPR3pPKoR376Fti8wuYRFrSWJZq1a9SA"),
  },
  {
    symbol: "AVICI",
    mint: new PublicKey("BANKJmvhT8tiJRsBSS1n2HryMBPvT5Ze4HU95DUAmeta"),
    pair: new PublicKey("7USpQsGyRh9awZXyCFft18Do7di8mraRrfqBcYXLok7C"),
  },
];

const rpcUrl = process.env.SOLANA_RPC_URL ?? "https://api.mainnet-beta.solana.com";
const walletPath = path.resolve(
  process.env.ANCHOR_WALLET ?? path.join(root, "deployer-keypair.json"),
);
const dryRun = process.argv.includes("--dry-run");
const targetNavUsdc = Number(process.env.META4_TARGET_NAV_USDC ?? 1);

function keypairFromFile(file) {
  return Keypair.fromSecretKey(
    Uint8Array.from(JSON.parse(fs.readFileSync(file, "utf8"))),
  );
}

async function fetchPairPrices() {
  const response = await fetch("https://api.indexer.omnipair.fi/api/v1/pools");
  if (!response.ok) {
    throw new Error(`Omnipair indexer request failed: ${response.status}`);
  }
  const data = await response.json();
  const pools = data.data.pools;

  return components.map((component) => {
    const pool = pools.find((candidate) => candidate.pair_address === component.pair.toBase58());
    if (!pool) {
      throw new Error(`missing Omnipair indexer pool for ${component.symbol}`);
    }
    const componentIsToken0 = pool.token0.address === component.mint.toBase58();
    const price = Number(componentIsToken0 ? pool.spot_prices.token0 : pool.spot_prices.token1);
    if (!Number.isFinite(price) || price <= 0) {
      throw new Error(`invalid ${component.symbol} price: ${price}`);
    }
    const unitsPerIndex = Math.max(
      1,
      Math.ceil(((targetNavUsdc / components.length) / price) * 1_000_000),
    );
    return { ...component, price, unitsPerIndex };
  });
}

function componentInputs(pricedComponents) {
  return pricedComponents.map((component) => ({
    mint: component.mint,
    unitsPerIndex: new anchor.BN(component.unitsPerIndex),
    targetWeightBps: 2_500,
    oraclePair: component.pair,
  }));
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

  const mint = await getMint(connection, indexMint, "confirmed", TOKEN_PROGRAM_ID);
  if (mint.supply !== 0n) {
    throw new Error(`META4 supply is already nonzero: ${mint.supply.toString()}`);
  }

  const state = await program.account.indexState.fetch(index);
  if (state.symbol !== "META4") {
    throw new Error(`unexpected index symbol: ${state.symbol}`);
  }

  const pricedComponents = await fetchPairPrices();
  const estimatedNav = pricedComponents.reduce(
    (total, component) => total + (component.unitsPerIndex / 1_000_000) * component.price,
    0,
  );

  console.log(`authority: ${payer.publicKey.toBase58()}`);
  console.log(`index: ${index.toBase58()}`);
  console.log(`indexMint supply: ${mint.supply.toString()}`);
  console.log(`target NAV USDC: ${targetNavUsdc}`);
  console.log(`estimated seeded NAV USDC: ${estimatedNav}`);
  for (const component of pricedComponents) {
    console.log(
      `${component.symbol}: price=${component.price} units=${component.unitsPerIndex} human=${component.unitsPerIndex / 1_000_000}`,
    );
  }

  if (dryRun) {
    console.log("[dry-run] would send updateFixedWeightConfig META4");
    return;
  }

  const signature = await program.methods
    .updateFixedWeightConfig({
      fixedWeightQuoteMint: usdcMint,
      fixedWeightRebalanceIntervalSeconds: state.fixedWeightRebalanceIntervalSeconds,
      fixedWeightDriftThresholdBps: state.fixedWeightDriftThresholdBps,
      fixedWeightSpotEmaMaxDeviationBps: state.fixedWeightSpotEmaMaxDeviationBps,
      components: componentInputs(pricedComponents),
    })
    .accounts({
      authority: payer.publicKey,
      index,
      indexMint,
    })
    .rpc({
      commitment: "confirmed",
      preflightCommitment: "confirmed",
      maxRetries: 5,
    });
  console.log(`updateFixedWeightConfig META4: ${signature}`);

  const updated = await program.account.indexState.fetch(index);
  for (const [i, component] of pricedComponents.entries()) {
    const updatedComponent = updated.components[i];
    if (!updatedComponent.mint.equals(component.mint)) {
      throw new Error(`${component.symbol} mint mismatch after update`);
    }
    if (updatedComponent.unitsPerIndex.toString() !== String(component.unitsPerIndex)) {
      throw new Error(`${component.symbol} units mismatch after update`);
    }
    if (updatedComponent.targetWeightBps !== 2_500) {
      throw new Error(`${component.symbol} target weight mismatch after update`);
    }
    if (!updatedComponent.oraclePair.equals(component.pair)) {
      throw new Error(`${component.symbol} pair mismatch after update`);
    }
  }
  console.log("META4 seed units updated and verified");
}

main().catch((error) => {
  console.error(error);
  process.exit(1);
});
