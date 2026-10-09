// Rebalance prices signed by the protocol's price oracle, shared by the oracle service
// (scripts/oracle-service.mjs) and the keeper (scripts/rebalance-bot.mjs). The program side is
// programs/basket/src/utils/signed_prices.rs.
//
// The oracle signs one message per rebalance step. The keeper sends the native Ed25519
// program's instruction carrying that signature immediately before the step; the runtime
// verifies it and the step reads the key and message back through the instructions sysvar.
//
// Message, little-endian:
//   16-byte tag "basket-prices-v1", rebalance intent address (for open, the PDA it creates from
//   index + nonce), u64 slot the oracle signed at, u8 entry count, then 6 bytes per entry: a u8
//   global component index, a u32 mantissa and a u8 exponent, the price (USD per whole token ×
//   1e18) being mantissa × 10^exponent.
// The intent is a PDA of the program, so it already names the program. Entries name components
// by index, and prices are decimal floating point (quantizePrice keeps 9 to 10 significant
// digits); all of it keeps every step within one transaction. Slots are append-only and each
// mint holds one, so the oracle maps mints to indexes from the basket's pages on chain itself.

import crypto from "node:crypto";
import { Ed25519Program, PublicKey } from "@solana/web3.js";

export const PRICE_MESSAGE_TAG = Buffer.from("basket-prices-v1");
// Mirror of the program's MAX_PRICE_AGE_SLOTS (programs/basket/src/constants.rs).
export const MAX_PRICE_AGE_SLOTS = 50;
const HEADER_LEN = 16 + 32 + 8 + 1;
const ENTRY_LEN = 1 + 4 + 1;
const MAX_ENTRIES = 255;
const MANTISSA_LIMIT = 1n << 32n;
const I128_MAX = (1n << 127n) - 1n;

/** The nearest price (1e18 scale) a message can carry: the mantissa as large as fits a u32. */
export function quantizePrice(price) {
  if (typeof price !== "bigint" || price <= 0n) throw new Error(`bad price ${price}`);
  let scale = 1n;
  while ((price + scale / 2n) / scale >= MANTISSA_LIMIT) scale *= 10n;
  return ((price + scale / 2n) / scale) * scale;
}

/** { mantissa, exponent } of a price a message can carry exactly, or throws. */
export function compactPrice(price) {
  if (typeof price !== "bigint" || price <= 0n || price > I128_MAX) throw new Error(`bad price ${price}`);
  for (let exponent = 0, scale = 1n; exponent <= 38; exponent += 1, scale *= 10n) {
    if (price % scale === 0n && price / scale < MANTISSA_LIMIT) return { mantissa: Number(price / scale), exponent };
  }
  throw new Error(`price ${price} has more significant digits than a message carries; quantizePrice it first`);
}

// DER prefixes that wrap a raw 32-byte Ed25519 seed / public key for node:crypto.
const PKCS8_ED25519_PREFIX = Buffer.from("302e020100300506032b657004220420", "hex");
const SPKI_ED25519_PREFIX = Buffer.from("302a300506032b6570032100", "hex");

export function priceOracleAddress(programId) {
  return PublicKey.findProgramAddressSync([Buffer.from("price-oracle")], programId)[0];
}

export function rebalanceIntentAddress(programId, index, nonce) {
  const le = Buffer.alloc(8);
  le.writeBigUInt64LE(BigInt(nonce.toString()));
  return PublicKey.findProgramAddressSync([Buffer.from("rebalance-intent"), index.toBuffer(), le], programId)[0];
}

/**
 * entries: [{ componentIndex, price: bigint }], sorted by component index in the message. Each
 * price must be one a message carries exactly (see quantizePrice).
 */
export function encodePriceMessage({ intent, slot, entries }) {
  if (entries.length > MAX_ENTRIES) throw new Error(`at most ${MAX_ENTRIES} prices per message`);
  const sorted = [...entries].sort((a, b) => a.componentIndex - b.componentIndex);
  const message = Buffer.alloc(HEADER_LEN + ENTRY_LEN * sorted.length);
  PRICE_MESSAGE_TAG.copy(message, 0);
  new PublicKey(intent).toBuffer().copy(message, 16);
  message.writeBigUInt64LE(BigInt(slot), 48);
  message.writeUInt8(sorted.length, 56);
  let at = HEADER_LEN;
  let previous = -1;
  for (const { componentIndex, price } of sorted) {
    if (!Number.isInteger(componentIndex) || componentIndex < 0 || componentIndex > 0xff) throw new Error(`bad component index ${componentIndex}`);
    if (componentIndex === previous) throw new Error(`component ${componentIndex} is priced twice`);
    const { mantissa, exponent } = compactPrice(price);
    message.writeUInt8(componentIndex, at);
    message.writeUInt32LE(mantissa, at + 1);
    message.writeUInt8(exponent, at + 5);
    at += ENTRY_LEN;
    previous = componentIndex;
  }
  return message;
}

export function decodePriceMessage(message) {
  const buf = Buffer.from(message);
  if (buf.length < HEADER_LEN || !buf.subarray(0, 16).equals(PRICE_MESSAGE_TAG)) throw new Error("not a signed price message");
  const count = buf.readUInt8(56);
  if (buf.length !== HEADER_LEN + ENTRY_LEN * count) throw new Error("signed price message has the wrong length");
  const entries = [];
  for (let at = HEADER_LEN; at < buf.length; at += ENTRY_LEN) {
    entries.push({
      componentIndex: buf.readUInt8(at),
      price: BigInt(buf.readUInt32LE(at + 1)) * 10n ** BigInt(buf.readUInt8(at + 5)),
    });
  }
  return {
    intent: new PublicKey(buf.subarray(16, 48)),
    slot: Number(buf.readBigUInt64LE(48)),
    entries,
  };
}

/** Ed25519 signature (64 bytes) over `message` with a Solana secret key (64-byte seed+public). */
export function signPriceMessage(secretKey, message) {
  const key = crypto.createPrivateKey({
    key: Buffer.concat([PKCS8_ED25519_PREFIX, Buffer.from(secretKey).subarray(0, 32)]),
    format: "der",
    type: "pkcs8",
  });
  return crypto.sign(null, Buffer.from(message), key);
}

export function verifyPriceSignature(publicKey, message, signature) {
  const key = crypto.createPublicKey({
    key: Buffer.concat([SPKI_ED25519_PREFIX, new PublicKey(publicKey).toBuffer()]),
    format: "der",
    type: "spki",
  });
  return crypto.verify(null, Buffer.from(message), key, Buffer.from(signature));
}

/** The Ed25519 program instruction carrying the signature; every offset points into itself. */
export function priceSignatureInstruction({ oracle, message, signature }) {
  return Ed25519Program.createInstructionWithPublicKey({
    publicKey: new PublicKey(oracle).toBytes(),
    message: Buffer.from(message),
    signature: Buffer.from(signature),
  });
}

/** A same-size stand-in for sizing a transaction before the real prices are signed. */
export function placeholderPriceInstruction(priceCount) {
  return priceSignatureInstruction({
    oracle: PublicKey.default,
    message: Buffer.alloc(HEADER_LEN + ENTRY_LEN * priceCount),
    signature: Buffer.alloc(64),
  });
}
