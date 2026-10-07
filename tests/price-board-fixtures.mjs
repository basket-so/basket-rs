import fs from 'node:fs';
import assert from 'node:assert/strict';
import anchor from '@coral-xyz/anchor';
import { PublicKey, Keypair, SystemProgram } from '@solana/web3.js';
import { AccountLayout, MintLayout, TOKEN_PROGRAM_ID, ASSOCIATED_TOKEN_PROGRAM_ID, getAssociatedTokenAddressSync } from '@solana/spl-token';

// Rebalances read prices the protocol's oracle posts to the price board. A fixed-weight basket
// that is due for a timed rebalance is written straight into the validator at startup (as
// composition-change-fixtures does), so open/finalize run against real accounts; no swap is
// needed when the posted prices put it on target.
const usdc = new PublicKey('EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v');
const bn = n => new anchor.BN(n);
const meta = (pubkey, isWritable = false) => ({ pubkey, isWritable, isSigner: false });
const SUPPLY = 100_000_000n; // 100 index tokens, each backed by 1 A and 1 B
const PRICE_SCALE = 10n ** 18n;
const usd = dollars => bn((BigInt(Math.round(dollars * 1e6)) * PRICE_SCALE / 1_000_000n).toString());

export async function preparePriceBoardFixtures(dir, programId, payer) {
  const program = new anchor.Program(JSON.parse(fs.readFileSync('target/idl/basket.json')), { connection: {}, publicKey: payer });
  const types = program.idl.types;
  const encode = (name, value) => {
    const { layout, discriminator } = program.coder.accounts.accountLayouts.get(name);
    const data = Buffer.alloc(4096); const len = layout.encode(value, data);
    return Buffer.concat([Buffer.from(discriminator), data.subarray(0, len)]);
  };
  const zero = t => {
    if (t === 'pubkey') return PublicKey.default;
    if (t === 'bool') return false;
    if (t === 'string') return '';
    if (typeof t === 'string') return ['u64', 'i64', 'u128', 'i128'].includes(t) ? bn(0) : 0;
    if (t.array) return Array.from({ length: t.array[1] }, () => zero(t.array[0]));
    if (t.vec) return [];
    const def = types.find(x => x.name === t.defined.name).type;
    if (def.kind === 'enum') return { [def.variants[0].name]: {} };
    return Object.fromEntries(def.fields.map(f => [f.name, zero(f.type)]));
  };
  const pda = (seed, ...p) => PublicKey.findProgramAddressSync([Buffer.from(seed), ...p.map(x => x.toBuffer ? x.toBuffer() : x)], programId);
  const validatorArgs = [];
  const dump = (pk, data, owner, size = data.length) => {
    const padded = Buffer.alloc(size); data.copy(padded);
    const file = `${dir}/${pk}.json`;
    fs.writeFileSync(file, JSON.stringify({ pubkey: pk.toBase58(), account: { lamports: 100_000_000, data: [padded.toString('base64'), 'base64'], owner: owner.toBase58(), executable: false, rentEpoch: 0 } }));
    validatorArgs.push('--account', pk.toBase58(), file);
  };
  const mint = (pk, authority, supply) => {
    const data = Buffer.alloc(MintLayout.span);
    MintLayout.encode({ mintAuthorityOption: 1, mintAuthority: authority, supply, decimals: 6, isInitialized: true, freezeAuthorityOption: 0, freezeAuthority: PublicKey.default }, data);
    dump(pk, data, TOKEN_PROGRAM_ID);
  };
  const token = (m, owner, amount) => {
    const pk = getAssociatedTokenAddressSync(m, owner, true), data = Buffer.alloc(AccountLayout.span);
    AccountLayout.encode({ mint: m, owner, amount, delegateOption: 0, delegate: PublicKey.default, state: 1, isNativeOption: 0, isNative: 0n, delegatedAmount: 0n, closeAuthorityOption: 0, closeAuthority: PublicKey.default }, data);
    dump(pk, data, TOKEN_PROGRAM_ID); return pk;
  };

  // 60% A, 40% B and a USDC cash slot, rebalancing every second (last rebalanced at 0) and
  // with no oracle pairs: prices come from the board by mint.
  const symbol = 'PRICED';
  const [index, indexBump] = pda('index', payer, Buffer.from(symbol));
  const [indexMint, indexMintBump] = pda('index-mint', index);
  const [vaultAuthority, vaultAuthorityBump] = pda('vault-authority', index);
  const [page, pageBump] = pda('large-basket-component-page', index, Buffer.from([0]));
  mint(indexMint, vaultAuthority, SUPPLY);
  const vaultQuote = token(usdc, vaultAuthority, 0n);
  const components = [6000, 4000].map(weight => {
    const m = Keypair.generate().publicKey;
    mint(m, payer, SUPPLY);
    return { mint: m, vault: token(m, vaultAuthority, SUPPLY), tokenProgram: TOKEN_PROGRAM_ID, unitsPerIndex: bn(1_000_000), accountedReserve: bn(SUPPLY.toString()), targetWeightBps: weight, oraclePair: PublicKey.default, decimals: 6 };
  });
  components.push({ mint: usdc, vault: vaultQuote, tokenProgram: TOKEN_PROGRAM_ID, unitsPerIndex: bn(0), accountedReserve: bn(0), targetWeightBps: 0, oraclePair: PublicKey.default, decimals: 6 });
  const state = zero({ defined: { name: 'indexState' } });
  Object.assign(state, { authority: payer, creator: payer, feeRecipient: payer, indexMint, vaultAuthorityBump, indexBump, indexMintBump, decimals: 6, kind: { fixedWeights: {} }, largeBasketComponentCount: components.length, largeBasketPageCount: 1, largeBasketConfigured: true, fixedWeightQuoteMint: usdc, fixedWeightRebalanceIntervalSeconds: bn(1), fixedWeightDriftThresholdBps: 500, name: symbol, symbol });
  dump(index, encode('indexState', state), programId, 4096);
  dump(page, encode('largeBasketComponentPage', { index, pageIndex: 0, startComponentIndex: 0, componentCount: components.length, bump: pageBump, finalized: true, reserved: Array(32).fill(0), components }), programId, 1553);
  return { validatorArgs, fixtures: { index, indexMint, vaultAuthority, page, vaultQuote, components } };
}

export async function testPriceBoard(program, connection, payer, user, f) {
  const pda = (...seeds) => PublicKey.findProgramAddressSync(seeds.map(s => typeof s === 'string' ? Buffer.from(s) : s.toBuffer ? s.toBuffer() : s), program.programId)[0];
  const errorCode = name => new RegExp(`Error Code: ${name}\\.`);
  const protocolConfig = pda('protocol-config');
  const priceBoard = pda('price-board');
  const [a, b] = f.components;
  const oracle = Keypair.generate();
  const waitSlots = async n => { const start = await connection.getSlot('confirmed'); while (await connection.getSlot('confirmed') < start + n) await new Promise(r => setTimeout(r, 200)); };

  // --- Only the protocol authority sets the oracle; doing so creates the board ---
  const setOracle = (key, authority = payer) => program.methods.setPriceOracle({ oracle: key })
    .accounts({ authority: authority.publicKey, protocolConfig, priceBoard, systemProgram: SystemProgram.programId })
    .signers(authority === payer ? [] : [authority]).rpc();
  await assert.rejects(setOracle(oracle.publicKey, user), errorCode('UnauthorizedAuthority'));
  await assert.rejects(setOracle(PublicKey.default), errorCode('InvalidAuthority'));
  await setOracle(oracle.publicKey);
  const board = () => program.account.priceBoard.fetch(priceBoard);
  assert.ok((await board()).oracle.equals(oracle.publicKey));

  // --- Only the oracle posts, and only usable prices ---
  const post = (prices, signer = oracle) => program.methods.postPrices({ prices: prices.map(([mint, price]) => ({ mint, price })) })
    .accounts({ oracle: signer.publicKey, priceBoard }).signers([signer]).rpc();
  await assert.rejects(post([[a.mint, usd(1.5)]], user), errorCode('UnauthorizedAuthority'), 'not the oracle');
  await assert.rejects(post([]), errorCode('InvalidOraclePrice'), 'empty post');
  await assert.rejects(post([[a.mint, bn(0)]]), errorCode('InvalidOraclePrice'), 'zero price');
  await assert.rejects(post([[usdc, usd(1)]]), errorCode('InvalidOraclePrice'), 'USDC is $1 on chain');
  await post([[a.mint, usd(1.5)]]);

  // --- Open reads every held component's price, fresh ---
  let nonce = 0;
  const chainNow = async () => Number((await connection.getAccountInfo(new PublicKey('SysvarC1ock11111111111111111111111111111111'))).data.readBigInt64LE(32));
  const intentOf = n => pda('rebalance-intent', f.index, bn(n).toArrayLike(Buffer, 'le', 8));
  const vaults = f.components.map(c => meta(c.vault));
  const open = async (maxPriceAgeSlots = 150) => {
    nonce += 1;
    await program.methods.openRebalanceIntent({ nonce: bn(nonce), expiresAt: bn((await chainNow()) + 600), maxPriceAgeSlots: bn(maxPriceAgeSlots), navToleranceBps: 50, maxPostRebalanceDriftBps: 100 })
      .accounts({ initiator: payer.publicKey, index: f.index, indexMint: f.indexMint, vaultAuthority: f.vaultAuthority, quoteMint: usdc, vaultQuoteTokenAccount: f.vaultQuote, intent: intentOf(nonce), priceBoard, associatedTokenProgram: ASSOCIATED_TOKEN_PROGRAM_ID, quoteTokenProgram: TOKEN_PROGRAM_ID, systemProgram: SystemProgram.programId })
      .remainingAccounts([meta(f.page), ...vaults]).rpc();
    return intentOf(nonce);
  };
  const finalize = (intent, maxPriceAgeSlots = 150) => program.methods.finalizeRebalance({ maxPriceAgeSlots: bn(maxPriceAgeSlots) })
    .accounts({ keeper: payer.publicKey, index: f.index, intent, vaultAuthority: f.vaultAuthority, quoteMint: usdc, vaultQuoteTokenAccount: f.vaultQuote, priceBoard, quoteTokenProgram: TOKEN_PROGRAM_ID })
    .remainingAccounts([meta(f.page, true), ...vaults]).rpc();
  const cancel = intent => program.methods.cancelRebalance().accounts({ initiator: payer.publicKey, index: f.index, intent }).rpc();
  await assert.rejects(open(), errorCode('MissingOraclePrice'), 'B was never posted');
  await post([[a.mint, usd(1.5)], [b.mint, usd(1)]]);
  await assert.rejects(open(151), errorCode('InvalidOraclePriceAge'));
  await waitSlots(2);
  await assert.rejects(open(1), errorCode('StaleOraclePrice'));
  // Reposting replaces a mint's price rather than adding an entry.
  assert.equal((await board()).prices.length, 2);

  // At A $2 / B $1 the basket is 2/3 A: legs sell 10 A and buy 20 B toward 60/40 of $300.
  await post([[a.mint, usd(2)], [b.mint, usd(1)]]);
  const offTarget = await open();
  const legs = await program.account.rebalanceIntent.fetch(offTarget);
  assert.deepEqual(legs.componentTargetAmounts.map(x => x.toNumber()), [10_000_000, 20_000_000, 0]);
  assert.equal(legs.oldNavNad.toString(), '300000000000'); // $300 at 1e9
  await assert.rejects(finalize(offTarget), errorCode('LargeBasketComponentNotFilled'));
  await cancel(offTarget);

  // At A $1.50 / B $1 it is exactly 60/40: nothing to swap, so it finalizes straight away,
  // again against fresh prices.
  await post([[a.mint, usd(1.5)], [b.mint, usd(1)]]);
  const onTarget = await open();
  const opened = await program.account.rebalanceIntent.fetch(onTarget);
  assert.deepEqual(opened.componentTargetAmounts.map(x => x.toNumber()), [0, 0, 0]);
  await waitSlots(2);
  await assert.rejects(finalize(onTarget, 1), errorCode('StaleOraclePrice'));
  await finalize(onTarget);
  const after = await program.account.indexState.fetch(f.index);
  assert.equal(after.largeBasketOperationInProgress, false);
  assert.ok(after.fixedWeightLastRebalancedAt.toNumber() > 0);
  assert.equal((await program.account.rebalanceIntent.fetch(onTarget)).newNavNad.toString(), '250000000000');

  // --- A new oracle key starts from an empty board; the old one can no longer post ---
  const next = Keypair.generate();
  await setOracle(next.publicKey);
  assert.equal((await board()).prices.length, 0);
  await assert.rejects(post([[a.mint, usd(1.5)]]), errorCode('UnauthorizedAuthority'));
  await post([[a.mint, usd(1.5)]], next);
  assert.equal((await board()).prices.length, 1);
  console.log('integration ok: price board oracle set/rotate, post authorization and validation, missing/stale/age-capped prices at open and finalize, legs from posted prices, on-target rebalance finalized');
}
