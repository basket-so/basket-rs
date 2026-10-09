import fs from 'node:fs';
import assert from 'node:assert/strict';
import anchor from '@coral-xyz/anchor';
import { ComputeBudgetProgram, Keypair, PublicKey, SystemProgram, SYSVAR_INSTRUCTIONS_PUBKEY, Transaction, TransactionInstruction, TransactionMessage, VersionedTransaction } from '@solana/web3.js';
import { AccountLayout, MintLayout, TOKEN_PROGRAM_ID, ASSOCIATED_TOKEN_PROGRAM_ID, getAccount, getAssociatedTokenAddressSync, mintTo } from '@solana/spl-token';
import { encodePriceMessage, priceSignatureInstruction, signPriceMessage } from '../scripts/lib/signed-prices.mjs';
import { basketLookupAddresses, ensureBasketLookupTable, sendV0 } from '../scripts/rebalance-bot.mjs';

// Rebalances read prices the protocol's oracle signs for one intent, from a native Ed25519
// instruction right before each step. Fixed-weight baskets that are due for a timed rebalance
// are written straight into the validator at startup (as composition-change-fixtures does), so
// open/finalize run against real accounts; no swap is needed when the signed prices put them on
// target. PRICED is small; WIDE is the largest basket whose open fits one mainnet transaction
// (64 accounts), rebalanced through the lookup table the keeper creates for it.
const usdc = new PublicKey('EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v');
const bn = n => new anchor.BN(n);
const meta = (pubkey, isWritable = false) => ({ pubkey, isWritable, isSigner: false });
const SUPPLY = 100_000_000n; // 100 index tokens, each backed by 1 of every component
const PRICE_SCALE = 10n ** 18n;
const usd = dollars => BigInt(Math.round(dollars * 1e6)) * PRICE_SCALE / 1_000_000n;
// The program caps baskets at 40 components (MAX_BASKET_COMPONENTS). WIDE is written straight
// into the validator at 45, the most a rebalance open fits, to show the cap leaves that margin.
export const WIDE_COMPONENTS = 45;
// The feature that would raise the per-transaction account limit from 64 to 128 is inactive on
// mainnet; the lifecycle test deactivates it so the validator enforces 64 too.
export const ACCOUNT_LOCK_LIMIT_128_FEATURE = '9LZdXeKGeBV6hRLdxS1rHbHoEUsKqesCC2ZAPTPKJAbK';

export async function prepareSignedPriceFixtures(dir, programId, payer) {
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
  // Small baskets' accounts are passed one by one; a wide one's go in a directory (--account-dir)
  // to keep the validator's command line short.
  const writer = accountDir => (pk, data, owner, size = data.length) => {
    const padded = Buffer.alloc(size); data.copy(padded);
    const file = `${accountDir ?? dir}/${pk}.json`;
    fs.writeFileSync(file, JSON.stringify({ pubkey: pk.toBase58(), account: { lamports: 100_000_000, data: [padded.toString('base64'), 'base64'], owner: owner.toBase58(), executable: false, rentEpoch: 0 } }));
    if (!accountDir) validatorArgs.push('--account', pk.toBase58(), file);
  };

  // A due fixed-weight basket: one SUPPLY of each token in its vault (1 per index token), the
  // USDC cash slot last, rebalancing every second (last rebalanced at 0), no oracle pairs.
  const basket = (symbol, weights, accountDir) => {
    const dump = writer(accountDir);
    if (accountDir) { fs.mkdirSync(accountDir, { recursive: true }); validatorArgs.push('--account-dir', accountDir); }
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
    const [index, indexBump] = pda('index', payer, Buffer.from(symbol));
    const [indexMint, indexMintBump] = pda('index-mint', index);
    const [vaultAuthority, vaultAuthorityBump] = pda('vault-authority', index);
    mint(indexMint, vaultAuthority, SUPPLY);
    const vaultQuote = token(usdc, vaultAuthority, 0n);
    const components = weights.map(weight => {
      const m = Keypair.generate().publicKey;
      mint(m, payer, SUPPLY);
      return { mint: m, vault: token(m, vaultAuthority, SUPPLY), tokenProgram: TOKEN_PROGRAM_ID, unitsPerIndex: bn(1_000_000), accountedReserve: bn(SUPPLY.toString()), targetWeightBps: weight, oraclePair: PublicKey.default, decimals: 6 };
    });
    components.push({ mint: usdc, vault: vaultQuote, tokenProgram: TOKEN_PROGRAM_ID, unitsPerIndex: bn(0), accountedReserve: bn(0), targetWeightBps: 0, oraclePair: PublicKey.default, decimals: 6 });
    const pages = [];
    for (let start = 0; start < components.length; start += 10) {
      const pageIndex = start / 10;
      const [page, pageBump] = pda('large-basket-component-page', index, Buffer.from([pageIndex]));
      const slice = components.slice(start, start + 10);
      dump(page, encode('largeBasketComponentPage', { index, pageIndex, startComponentIndex: start, componentCount: slice.length, bump: pageBump, finalized: true, reserved: Array(32).fill(0), components: slice }), programId, 1553);
      pages.push(page);
    }
    const state = zero({ defined: { name: 'indexState' } });
    Object.assign(state, { authority: payer, creator: payer, feeRecipient: payer, indexMint, vaultAuthorityBump, indexBump, indexMintBump, decimals: 6, kind: { fixedWeights: {} }, largeBasketComponentCount: components.length, largeBasketPageCount: pages.length, largeBasketConfigured: true, fixedWeightQuoteMint: usdc, fixedWeightRebalanceIntervalSeconds: bn(1), fixedWeightDriftThresholdBps: 500, name: symbol, symbol });
    dump(index, encode('indexState', state), programId, 4096);
    return { index, indexMint, vaultAuthority, page: pages[0], pages, vaultQuote, components };
  };

  // PRICED: 60% A, 40% B. WIDE: 44 tokens (43 at 2.27%, one at 2.39%), so at $1 per 100 bps of
  // weight every token sits on its target.
  const priced = basket('PRICED', [6000, 4000]);
  const wideWeights = Array.from({ length: WIDE_COMPONENTS - 1 }, (_, i) => (i === WIDE_COMPONENTS - 2 ? 239 : 227));
  const wide = basket('WIDE', wideWeights, `${dir}/wide-basket`);
  return { validatorArgs, fixtures: { ...priced, wide: { ...wide, weights: wideWeights } } };
}

export async function testSignedPrices(program, connection, payer, user, f) {
  const pda = (...seeds) => PublicKey.findProgramAddressSync(seeds.map(s => typeof s === 'string' ? Buffer.from(s) : s.toBuffer ? s.toBuffer() : s), program.programId)[0];
  const codeOf = name => program.idl.errors.find(e => e.name.toLowerCase() === name.toLowerCase()).code;
  // A refused transaction names the error in its logs (preflight) or carries its code.
  const rejectsWith = async (promise, name, why = name) => {
    let error;
    try { await promise; } catch (e) { error = e; }
    assert.ok(error, `expected ${name}: ${why}`);
    const text = `${error.message}\n${(error.logs ?? error.transactionLogs ?? []).join('\n')}`;
    assert.ok(new RegExp(`Error Code: ${name}\\.|custom program error: 0x${codeOf(name).toString(16)}\\b|"Custom":${codeOf(name)}\\b`).test(text), `${why}: expected ${name}, got ${text.slice(0, 600)}`);
  };
  const protocolConfig = pda('protocol-config');
  const priceOracle = pda('price-oracle');
  const oracle = Keypair.generate();
  const waitForSlot = async n => { while (await connection.getSlot('confirmed') < n) await new Promise(r => setTimeout(r, 200)); };
  await waitForSlot(120); // room to sign at a slot older than the 50-slot window

  // --- Only the protocol authority sets the oracle; doing so creates its account ---
  const setOracle = (key, authority = payer) => program.methods.setPriceOracle({ oracle: key })
    .accounts({ authority: authority.publicKey, protocolConfig, priceOracle, systemProgram: SystemProgram.programId })
    .signers(authority === payer ? [] : [authority]).rpc();
  await rejectsWith(setOracle(oracle.publicKey, user), 'UnauthorizedAuthority');
  await rejectsWith(setOracle(PublicKey.default), 'InvalidAuthority');
  await setOracle(oracle.publicKey);
  assert.ok((await program.account.priceOracle.fetch(priceOracle)).oracle.equals(oracle.publicKey));

  // --- Steps carry the oracle's Ed25519 signature instruction right before them ---
  const nonces = new Map();
  const nextNonce = b => { const n = (nonces.get(b.index.toBase58()) ?? 0) + 1; nonces.set(b.index.toBase58(), n); return n; };
  const intentOf = (b, n) => pda('rebalance-intent', b.index, bn(n).toArrayLike(Buffer, 'le', 8));
  const chainNow = async () => Number((await connection.getAccountInfo(new PublicKey('SysvarC1ock11111111111111111111111111111111'))).data.readBigInt64LE(32));
  const prices = (a, b) => [{ componentIndex: 0, price: usd(a) }, { componentIndex: 1, price: usd(b) }];
  const signed = async ({ intent, entries, signer = oracle, slotOffset = 0 }) => {
    const message = encodePriceMessage({ intent, slot: (await connection.getSlot('confirmed')) + slotOffset, entries });
    return { message, ix: priceSignatureInstruction({ oracle: signer.publicKey, message, signature: signPriceMessage(signer.secretKey, message) }) };
  };
  const send = ixs => program.provider.sendAndConfirm(new Transaction().add(...ixs));
  const openIx = async (b, n, maxPriceAgeSlots = 50, initiator = payer.publicKey) => program.methods.openRebalanceIntent({ nonce: bn(n), expiresAt: bn((await chainNow()) + 600), maxPriceAgeSlots: bn(maxPriceAgeSlots), navToleranceBps: 50, maxPostRebalanceDriftBps: 100 })
    .accounts({ initiator, index: b.index, indexMint: b.indexMint, vaultAuthority: b.vaultAuthority, quoteMint: usdc, vaultQuoteTokenAccount: b.vaultQuote, intent: intentOf(b, n), priceOracle, instructionsSysvar: SYSVAR_INSTRUCTIONS_PUBKEY, associatedTokenProgram: ASSOCIATED_TOKEN_PROGRAM_ID, quoteTokenProgram: TOKEN_PROGRAM_ID, systemProgram: SystemProgram.programId })
    .remainingAccounts([...b.pages.map(p => meta(p)), ...b.components.map(c => meta(c.vault))]).instruction();
  const finalizeIx = (b, intent) => program.methods.finalizeRebalance({ maxPriceAgeSlots: bn(50) })
    .accounts({ keeper: payer.publicKey, index: b.index, intent, vaultAuthority: b.vaultAuthority, quoteMint: usdc, vaultQuoteTokenAccount: b.vaultQuote, priceOracle, instructionsSysvar: SYSVAR_INSTRUCTIONS_PUBKEY, quoteTokenProgram: TOKEN_PROGRAM_ID })
    .remainingAccounts([...b.pages.map(p => meta(p, true)), ...b.components.map(c => meta(c.vault))]).instruction();
  const open = async (a, b) => {
    const n = nextNonce(f);
    await send([(await signed({ intent: intentOf(f, n), entries: prices(a, b) })).ix, await openIx(f, n)]);
    return intentOf(f, n);
  };
  const cancel = (b, intent) => program.methods.cancelRebalance().accounts({ initiator: payer.publicKey, index: b.index, intent }).rpc();

  const next = (nonces.get(f.index.toBase58()) ?? 0) + 1;
  const good = () => signed({ intent: intentOf(f, next), entries: prices(2, 1) });
  await rejectsWith(send([await openIx(f, next)]), 'MissingPriceSignature', 'no signature instruction at all');
  await rejectsWith(send([ComputeBudgetProgram.setComputeUnitLimit({ units: 400_000 }), await openIx(f, next)]), 'MissingPriceSignature', 'the instruction before is not the Ed25519 program');
  await rejectsWith(send([(await good()).ix, ComputeBudgetProgram.setComputeUnitLimit({ units: 400_000 }), await openIx(f, next)]), 'MissingPriceSignature', 'the signature is not immediately before');
  await rejectsWith(send([(await signed({ intent: intentOf(f, next), entries: prices(2, 1), signer: user })).ix, await openIx(f, next)]), 'InvalidPriceSignature', 'signed by another key');
  await rejectsWith(send([(await signed({ intent: intentOf(f, next + 1), entries: prices(2, 1) })).ix, await openIx(f, next)]), 'SignedPricesMismatch', 'signed for another intent');
  await rejectsWith(send([(await signed({ intent: intentOf(f, next), entries: prices(2, 1), slotOffset: -60 })).ix, await openIx(f, next)]), 'StaleOraclePrice', 'older than 50 slots');
  await rejectsWith(send([(await signed({ intent: intentOf(f, next), entries: prices(2, 1), slotOffset: 1_000 })).ix, await openIx(f, next)]), 'SignedPriceSlotInFuture', 'from a future slot');
  await rejectsWith(send([(await signed({ intent: intentOf(f, next), entries: prices(2, 1).slice(0, 1) })).ix, await openIx(f, next)]), 'MissingOraclePrice', 'B was never signed');
  await rejectsWith(send([(await good()).ix, await openIx(f, next, 51)]), 'InvalidOraclePriceAge', 'a looser age than the cap');
  // A zero mantissa signs fine but is no price.
  const zeroPrice = await good();
  const at = zeroPrice.ix.data.readUInt16LE(10) + 57; // the first entry, after the message header
  zeroPrice.ix.data.writeUInt32LE(0, at + 1);
  const resigned = zeroPrice.ix.data.subarray(zeroPrice.ix.data.readUInt16LE(10));
  signPriceMessage(oracle.secretKey, resigned).copy(zeroPrice.ix.data, zeroPrice.ix.data.readUInt16LE(2));
  await rejectsWith(send([zeroPrice.ix, await openIx(f, next)]), 'InvalidOraclePrice', 'a zero price');

  // The known Ed25519 trap: an instruction whose offsets point into ANOTHER instruction is
  // verified by the runtime against that instruction's bytes. Here the attacker signs forged
  // prices with their own key (instruction 0, valid on its own), and instruction 1 claims the
  // oracle's key and the same forged message in its own data while its offsets make the runtime
  // check instruction 0. The runtime accepts both; the program must refuse.
  const forged = await signed({ intent: intentOf(f, next), entries: prices(1_000, 1), signer: user });
  const claim = Buffer.from(forged.ix.data);
  for (const field of [1, 3, 6]) claim.writeUInt16LE(0, 2 + 2 * field);
  oracle.publicKey.toBuffer().copy(claim, claim.readUInt16LE(6));
  const trap = new TransactionInstruction({ keys: [], programId: forged.ix.programId, data: claim });
  await rejectsWith(send([forged.ix, trap, await openIx(f, next)]), 'InvalidPriceSignature', 'offsets into another instruction');

  // --- With good signatures: at A $2 / B $1 the basket is 2/3 A, so the legs sell 10 A and buy
  // 20 B toward 60/40 of $300 ---
  const offTarget = await open(2, 1);
  const legs = await program.account.rebalanceIntent.fetch(offTarget);
  assert.deepEqual(legs.componentTargetAmounts.map(x => x.toNumber()), [10_000_000, 20_000_000, 0]);
  assert.equal(legs.oldNavNad.toString(), '300000000000'); // $300 at 1e9
  await rejectsWith(send([(await signed({ intent: offTarget, entries: prices(2, 1) })).ix, await finalizeIx(f, offTarget)]), 'LargeBasketComponentNotFilled');
  await cancel(f, offTarget);

  // --- The keeper's lookup table: found by its authority and contents, created once, extended
  // when the basket gains accounts, and usable once warm ---
  const env = { connection, payer };
  const keeperView = b => ({
    ctx: { index: b.index, indexMint: b.indexMint, vaultAuthority: b.vaultAuthority, vaultQuote: b.vaultQuote },
    components: b.components.map(c => ({ vault: c.vault, mint: c.mint, tokenProgram: c.tokenProgram })),
    pages: b.pages,
  });
  const small = keeperView(f);
  assert.equal(await ensureBasketLookupTable(env, small.ctx, small.components, small.pages, { execute: false }), null, 'a dry run creates nothing');
  const table = await ensureBasketLookupTable(env, small.ctx, small.components, small.pages, { execute: true });
  const wanted = basketLookupAddresses(small.ctx, small.components, small.pages).map(a => a.toBase58());
  assert.deepEqual(table.state.addresses.map(a => a.toBase58()).sort(), [...wanted].sort());
  assert.ok(table.state.authority.equals(payer.publicKey));
  const again = await ensureBasketLookupTable(env, small.ctx, small.components, small.pages, { execute: true });
  assert.ok(again.key.equals(table.key), 'found again rather than created twice');
  const added = { vault: Keypair.generate().publicKey, mint: Keypair.generate().publicKey, tokenProgram: TOKEN_PROGRAM_ID };
  const extended = await ensureBasketLookupTable(env, small.ctx, [...small.components, added], small.pages, { execute: true });
  assert.ok(extended.key.equals(table.key));
  assert.equal(extended.state.addresses.length, table.state.addresses.length + 2, 'only the new component\'s vault and mint are added');

  // At A $1.50 / B $1 it is exactly 60/40: nothing to swap, so it finalizes straight away, again
  // against fresh signed prices, and a signature for the earlier intent is no good here. Open and
  // finalize go out as v0 transactions through the basket's table.
  const sendWithTable = (ixs, label) => sendV0(connection, payer, ixs, label, [extended]);
  const n = nextNonce(f);
  const onTarget = intentOf(f, n);
  await sendWithTable([(await signed({ intent: onTarget, entries: prices(1.5, 1) })).ix, await openIx(f, n)], 'open PRICED through its table');
  assert.deepEqual((await program.account.rebalanceIntent.fetch(onTarget)).componentTargetAmounts.map(x => x.toNumber()), [0, 0, 0]);
  await rejectsWith(sendWithTable([(await signed({ intent: onTarget, entries: prices(1.5, 1), slotOffset: -60 })).ix, await finalizeIx(f, onTarget)], 'stale finalize'), 'StaleOraclePrice');
  await rejectsWith(sendWithTable([(await signed({ intent: offTarget, entries: prices(1.5, 1) })).ix, await finalizeIx(f, onTarget)], 'finalize for another intent'), 'SignedPricesMismatch');
  await sendWithTable([(await signed({ intent: onTarget, entries: prices(1.5, 1) })).ix, await finalizeIx(f, onTarget)], 'finalize PRICED through its table');
  const after = await program.account.indexState.fetch(f.index);
  assert.equal(after.largeBasketOperationInProgress, false);
  assert.ok(after.fixedWeightLastRebalancedAt.toNumber() > 0);
  assert.equal((await program.account.rebalanceIntent.fetch(onTarget)).newNavNad.toString(), '250000000000');

  // --- WIDE: 45 components, 44 signed prices, through the table the keeper creates for it. Open
  // names 64 accounts, the most one mainnet transaction may lock; one more is refused. The program
  // lets no basket past 40 components, so real baskets stay well inside this ---
  const w = f.wide;
  const wideView = keeperView(w);
  const wideTable = await ensureBasketLookupTable(env, wideView.ctx, wideView.components, wideView.pages, { execute: true });
  const widePrices = w.weights.map((weight, componentIndex) => ({ componentIndex, price: usd(weight / 100) }));
  const budget = ComputeBudgetProgram.setComputeUnitLimit({ units: 1_400_000 });
  const wn = nextNonce(w);
  const wideIntent = intentOf(w, wn);
  const wideOpen = await openIx(w, wn);
  const footprint = message => message.staticAccountKeys.length + message.addressTableLookups.reduce((s, l) => s + l.writableIndexes.length + l.readonlyIndexes.length, 0);
  const compile = ixs => new TransactionMessage({ payerKey: payer.publicKey, recentBlockhash: PublicKey.default.toBase58(), instructions: ixs }).compileToV0Message([wideTable]);
  const wideSigned = await signed({ intent: wideIntent, entries: widePrices });
  assert.equal(footprint(compile([budget, wideSigned.ix, wideOpen])), 64);
  // One more account than mainnet allows is refused before the program runs.
  const overfull = new TransactionInstruction({ ...wideOpen, keys: [...wideOpen.keys, meta(Keypair.generate().publicKey)] });
  const latest = await connection.getLatestBlockhash('confirmed');
  const tooMany = new VersionedTransaction(new TransactionMessage({ payerKey: payer.publicKey, recentBlockhash: latest.blockhash, instructions: [budget, wideSigned.ix, overfull] }).compileToV0Message([wideTable]));
  tooMany.sign([payer]);
  await assert.rejects(connection.sendTransaction(tooMany), /too many account locks|TooManyAccountLocks|locked too many accounts/i);
  const computeOf = async sig => (await connection.getTransaction(sig, { commitment: 'confirmed', maxSupportedTransactionVersion: 0 })).meta.computeUnitsConsumed;
  const openSig = await sendV0(connection, payer, [budget, (await signed({ intent: wideIntent, entries: widePrices })).ix, await openIx(w, wn)], 'open WIDE through its table', [wideTable]);
  assert.deepEqual((await program.account.rebalanceIntent.fetch(wideIntent)).componentTargetAmounts.map(x => x.toNumber()), Array(WIDE_COMPONENTS).fill(0));
  const finalizeSig = await sendV0(connection, payer, [budget, (await signed({ intent: wideIntent, entries: widePrices })).ix, await finalizeIx(w, wideIntent)], 'finalize WIDE through its table', [wideTable]);
  assert.equal((await program.account.rebalanceIntent.fetch(wideIntent)).newNavNad.toString(), '10000000000000'); // $10,000 at 1e9
  console.log(`integration: WIDE (${WIDE_COMPONENTS} components) open used ${await computeOf(openSig)} CU, finalize ${await computeOf(finalizeSig)} CU`);

  // --- A new oracle key takes over at once; the old one's signatures stop verifying ---
  const rotated = Keypair.generate();
  await setOracle(rotated.publicKey);
  // The 1-second rebalance interval passes again.
  while (await chainNow() < after.fixedWeightLastRebalancedAt.toNumber() + 1) await new Promise(r => setTimeout(r, 200));
  const rn = nextNonce(f);
  await rejectsWith(send([(await signed({ intent: intentOf(f, rn), entries: prices(1.5, 1) })).ix, await openIx(f, rn)]), 'InvalidPriceSignature', 'the previous key');
  await send([(await signed({ intent: intentOf(f, rn), entries: prices(1.5, 1), signer: rotated })).ix, await openIx(f, rn)]);
  await cancel(f, intentOf(f, rn));

  // --- The vaults are public, so tokens can arrive mid-rebalance. Finalize judges what the legs
  // left, not the live balances: $5 of USDC and 10 A ($15) sent to the $250 basket after open
  // would otherwise be 2% excess quote and 6% drift, past the 1% bounds. They are booked to
  // holders once it finalizes ---
  const dn = nextNonce(f);
  const gifted = intentOf(f, dn);
  await send([(await signed({ intent: gifted, entries: prices(1.5, 1), signer: rotated })).ix, await openIx(f, dn)]);
  await mintTo(connection, payer, usdc, f.vaultQuote, payer, 5_000_000);
  await mintTo(connection, payer, f.components[0].mint, f.components[0].vault, payer, 10_000_000);
  await send([(await signed({ intent: gifted, entries: prices(1.5, 1), signer: rotated })).ix, await finalizeIx(f, gifted)]);
  assert.equal((await program.account.rebalanceIntent.fetch(gifted)).newNavNad.toString(), '250000000000');
  const booked = (await program.account.largeBasketComponentPage.fetch(f.page)).components;
  assert.equal(booked[0].accountedReserve.toString(), (SUPPLY + 10_000_000n).toString());
  assert.equal(booked[2].accountedReserve.toString(), '5000000');
  assert.equal((await getAccount(connection, f.vaultQuote)).amount, 5_000_000n);

  // --- After an abandoned rebalance the keeper waits out the request spacing before opening
  // again, so it cannot hold mints and redeems back by reopening each time one is unwound; the
  // authority, which appoints the keeper, is not held back ---
  await program.methods.setRebalanceKeeper({ keeper: user.publicKey }).accounts({ authority: payer.publicKey, index: f.index }).rpc();
  const lastRebalanced = (await program.account.indexState.fetch(f.index)).fixedWeightLastRebalancedAt.toNumber();
  while (await chainNow() <= lastRebalanced + 1) await new Promise(r => setTimeout(r, 200));
  const abandoned = nextNonce(f);
  await send([(await signed({ intent: intentOf(f, abandoned), entries: prices(1.5, 1), signer: rotated })).ix, await openIx(f, abandoned)]);
  await cancel(f, intentOf(f, abandoned));
  const kn = nextNonce(f);
  const keeperOpen = async () => program.provider.sendAndConfirm(new Transaction().add((await signed({ intent: intentOf(f, kn), entries: prices(1.5, 1), signer: rotated })).ix, await openIx(f, kn, 50, user.publicKey)), [user]);
  await rejectsWith(keeperOpen(), 'RebalanceRequestCooldown', 'the keeper reopening right after a cancel');
  await rejectsWith(program.methods.requestRebalance().accounts({ operator: user.publicKey, index: f.index }).signers([user]).rpc(), 'RebalanceRequestCooldown', 'the keeper requesting right after a cancel');
  await send([(await signed({ intent: intentOf(f, kn), entries: prices(1.5, 1), signer: rotated })).ix, await openIx(f, kn)]);
  await cancel(f, intentOf(f, kn));
  await program.methods.setRebalanceKeeper({ keeper: PublicKey.default }).accounts({ authority: payer.publicKey, index: f.index }).rpc();
  console.log('integration ok: price oracle set/rotate, Ed25519 signature required right before the step, wrong key/intent/age/slot/missing or zero price refused, cross-instruction offsets refused, legs from signed prices, keeper lookup tables created/found/extended, on-target rebalances finalized through them up to 45 components and 64 accounts, mid-rebalance gifts neither block finalize nor count toward its bounds, keeper opens spaced after an abandoned rebalance');
}
