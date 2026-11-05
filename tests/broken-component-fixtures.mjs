import fs from 'node:fs';
import assert from 'node:assert/strict';
import anchor from '@coral-xyz/anchor';
import { Keypair, PublicKey, SystemProgram, SYSVAR_INSTRUCTIONS_PUBKEY, Transaction } from '@solana/web3.js';
import { AccountLayout, MintLayout, TOKEN_PROGRAM_ID, TOKEN_2022_PROGRAM_ID, ASSOCIATED_TOKEN_PROGRAM_ID, createAccount, createAssociatedTokenAccountIdempotent, freezeAccount, getAccount, getAssociatedTokenAddressSync, thawAccount, transfer } from '@solana/spl-token';
import { encodePriceMessage, priceSignatureInstruction, signPriceMessage } from '../scripts/lib/signed-prices.mjs';

// A component its issuer stops (a frozen vault, a paused mint, a hook program, a drained vault)
// must never trap the rest of a basket. These baskets are written straight into the validator,
// with mints the test controls: classic mints whose freeze authority is the payer, and
// Token-2022 mints already paused or carrying a hook program, so their transfers fail.
const usdc = new PublicKey('EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v');
const bn = n => new anchor.BN(n);
const meta = (pubkey, isWritable = false) => ({ pubkey, isWritable, isSigner: false });
const SUPPLY = 100_000_000n; // 100 index tokens, each backed by 1 of every component
const PRICE_SCALE = 10n ** 18n;
const usd = dollars => BigInt(Math.round(dollars * 1e6)) * PRICE_SCALE / 1_000_000n;

export async function prepareBrokenComponentFixtures(dir, programId, payer) {
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
  // One --account-dir keeps the validator's command line under Windows' length limit.
  const accountDir = `${dir}/broken-components`;
  fs.mkdirSync(accountDir, { recursive: true });
  const validatorArgs = ['--account-dir', accountDir];
  const dump = (pk, data, owner, size = data.length) => {
    const padded = Buffer.alloc(size); data.copy(padded);
    fs.writeFileSync(`${accountDir}/${pk}.json`, JSON.stringify({ pubkey: pk.toBase58(), account: { lamports: 100_000_000, data: [padded.toString('base64'), 'base64'], owner: owner.toBase58(), executable: false, rentEpoch: 0 } }));
  };
  const mintBase = (authority, supply, freezeAuthority) => {
    const data = Buffer.alloc(MintLayout.span);
    MintLayout.encode({ mintAuthorityOption: 1, mintAuthority: authority, supply, decimals: 6, isInitialized: true, freezeAuthorityOption: freezeAuthority ? 1 : 0, freezeAuthority: freezeAuthority ?? PublicKey.default }, data);
    return data;
  };
  // A classic mint whose freeze authority is the payer, so the test can freeze its accounts.
  const freezableMint = () => {
    const pk = Keypair.generate().publicKey;
    dump(pk, mintBase(payer, SUPPLY * 3n, payer), TOKEN_PROGRAM_ID);
    return { pk, program: TOKEN_PROGRAM_ID };
  };
  // A Token-2022 mint with one extension (type, value): padded to a token account's length,
  // then the mint account-type byte, then the extension.
  const token2022Mint = (type, value) => {
    const pk = Keypair.generate().publicKey;
    const header = Buffer.alloc(4); header.writeUInt16LE(type, 0); header.writeUInt16LE(value.length, 2);
    const data = Buffer.concat([mintBase(payer, SUPPLY * 3n, null), Buffer.alloc(165 - MintLayout.span), Buffer.from([1]), header, value]);
    dump(pk, data, TOKEN_2022_PROGRAM_ID);
    return { pk, program: TOKEN_2022_PROGRAM_ID };
  };
  const hooked = () => token2022Mint(14, Buffer.concat([payer.toBuffer(), Keypair.generate().publicKey.toBuffer()]));
  const paused = () => token2022Mint(26, Buffer.concat([payer.toBuffer(), Buffer.from([1])]));
  const token = (m, owner, amount, tokenProgram = TOKEN_PROGRAM_ID, delegatedAmount = 0n) => {
    const pk = getAssociatedTokenAddressSync(m, owner, true, tokenProgram), data = Buffer.alloc(AccountLayout.span);
    AccountLayout.encode({ mint: m, owner, amount, delegateOption: delegatedAmount ? 1 : 0, delegate: delegatedAmount ? payer : PublicKey.default, state: 1, isNativeOption: 0, isNative: 0n, delegatedAmount, closeAuthorityOption: 0, closeAuthority: PublicKey.default }, data);
    dump(pk, data, tokenProgram); return pk;
  };
  // A Token-2022 mint whose freeze authority is the payer and whose new accounts start frozen
  // (DefaultAccountState, type 6, set to frozen).
  const defaultFrozenMint = () => {
    const pk = Keypair.generate().publicKey;
    const header = Buffer.alloc(4); header.writeUInt16LE(6, 0); header.writeUInt16LE(1, 2);
    dump(pk, Buffer.concat([mintBase(payer, SUPPLY * 3n, payer), Buffer.alloc(165 - MintLayout.span), Buffer.from([1]), header, Buffer.from([2])]), TOKEN_2022_PROGRAM_ID);
    return { pk, program: TOKEN_2022_PROGRAM_ID };
  };
  const feeWallet = Keypair.generate().publicKey;

  // components: [{ mint: {pk, program}, weight, vaultAmount, reserve, feeAccount }]
  const basket = (symbol, kind, components, fees = {}) => {
    const [index, indexBump] = pda('index', payer, Buffer.from(symbol));
    const [indexMint, indexMintBump] = pda('index-mint', index);
    const [vaultAuthority, vaultAuthorityBump] = pda('vault-authority', index);
    dump(indexMint, mintBase(vaultAuthority, SUPPLY, null), TOKEN_PROGRAM_ID);
    const ownerIndex = token(indexMint, payer, SUPPLY);
    const [page, pageBump] = pda('large-basket-component-page', index, Buffer.from([0]));
    const entries = components.map(c => {
      const isUsdc = c.mint.pk.equals(usdc);
      const reserve = c.reserve ?? SUPPLY;
      const vault = token(c.mint.pk, vaultAuthority, c.vaultAmount ?? reserve, c.mint.program, c.delegatedAmount ?? 0n);
      if (!isUsdc && c.mint.program.equals(TOKEN_PROGRAM_ID)) token(c.mint.pk, feeWallet, 0n);
      return { mint: c.mint.pk, vault, tokenProgram: c.mint.program, unitsPerIndex: bn((reserve * 1_000_000n / SUPPLY).toString()), accountedReserve: bn(reserve.toString()), targetWeightBps: c.weight ?? 0, oraclePair: PublicKey.default, decimals: 6 };
    });
    dump(page, encode('largeBasketComponentPage', { index, pageIndex: 0, startComponentIndex: 0, componentCount: entries.length, bump: pageBump, finalized: true, reserved: Array(32).fill(0), components: entries }), programId, 1553);
    const state = zero({ defined: { name: 'indexState' } });
    Object.assign(state, { authority: payer, creator: payer, feeRecipient: fees.recipient ?? payer, indexMint, vaultAuthorityBump, indexBump, indexMintBump, decimals: 6, kind: { [kind]: {} }, largeBasketComponentCount: entries.length, largeBasketPageCount: 1, largeBasketConfigured: true, fixedWeightQuoteMint: kind === 'fixedWeights' ? usdc : PublicKey.default, fixedWeightRebalanceIntervalSeconds: bn(kind === 'fixedWeights' ? 1 : 0), fixedWeightDriftThresholdBps: kind === 'fixedWeights' ? 500 : 0, redeemFeeBps: fees.redeemBps ?? 0, name: symbol, symbol });
    dump(index, encode('indexState', state), programId, 4096);
    const vaultQuote = getAssociatedTokenAddressSync(usdc, vaultAuthority, true);
    return { index, indexMint, vaultAuthority, page, pages: [page], ownerIndex, vaultQuote, components: entries };
  };
  const usdcMint = { pk: usdc, program: TOKEN_PROGRAM_ID };
  token(usdc, feeWallet, 0n);

  // BROKE: a frozen-to-be vault (X), a hooked mint (H), a paused mint (P), a token whose fee
  // account gets frozen (Y), and USDC; redeems pay 0.3% to feeWallet.
  const broke = basket('BROKE', 'fixedWeights', [
    { mint: freezableMint(), weight: 3000 },
    { mint: hooked(), weight: 2000 },
    { mint: paused(), weight: 1000 },
    { mint: freezableMint(), weight: 2000 },
    { mint: usdcMint, weight: 2000 },
  ], { recipient: feeWallet, redeemBps: 30 });
  // DRAIN: its vault holds 60 of the 100 its books say.
  const drain = basket('DRAIN', 'fixedUnits', [{ mint: freezableMint(), vaultAmount: 60_000_000n }]);
  // RBX: due for a timed rebalance; with X2 frozen and P2 paused left out, A and USDC are $100
  // each against 50/50, so the rest is already on target and no swap is needed.
  // A's vault has a delegate, as an issuer's permanent delegate would, to take tokens mid-rebalance.
  const rbx = basket('RBX', 'fixedWeights', [
    { mint: freezableMint(), weight: 4000 },
    { mint: paused(), weight: 2000 },
    { mint: freezableMint(), weight: 2000, delegatedAmount: 1_000n },
    { mint: usdcMint, weight: 2000 },
  ]);
  // DFRZ: a token whose issuer makes every new account start frozen.
  const dfrz = basket('DFRZ', 'fixedWeights', [
    { mint: defaultFrozenMint(), weight: 5000 },
    { mint: usdcMint, weight: 5000 },
  ]);
  return { validatorArgs, fixtures: { broke, drain, rbx, dfrz, feeWallet } };
}

export async function testBrokenComponents(program, connection, payer, user, stakingPool, f) {
  const pda = (...seeds) => PublicKey.findProgramAddressSync(seeds.map(s => typeof s === 'string' ? Buffer.from(s) : s.toBuffer ? s.toBuffer() : s), program.programId)[0];
  const codeOf = name => program.idl.errors.find(e => e.name.toLowerCase() === name.toLowerCase()).code;
  const rejectsWith = async (promise, name, why = name) => {
    let error;
    try { await promise; } catch (e) { error = e; }
    assert.ok(error, `expected ${name}: ${why}`);
    const text = `${error.message}\n${(error.logs ?? error.transactionLogs ?? []).join('\n')}`;
    assert.ok(new RegExp(`Error Code: ${name}\\.|custom program error: 0x${codeOf(name).toString(16)}\\b|"Custom":${codeOf(name)}\\b`).test(text), `${why}: expected ${name}, got ${text.slice(0, 600)}`);
  };
  const chainNow = async () => Number((await connection.getAccountInfo(new PublicKey('SysvarC1ock11111111111111111111111111111111'))).data.readBigInt64LE(32));
  const balance = async (account, tokenProgram = TOKEN_PROGRAM_ID) => (await getAccount(connection, account, 'confirmed', tokenProgram)).amount;
  const reserves = async b => (await program.account.largeBasketComponentPage.fetch(b.page)).components.map(c => BigInt(c.accountedReserve.toString()));

  // ---------------------------------------------------------------- BROKE: redeems get out
  const b = f.broke;
  const [X, H, P, Y, CASH] = b.components;
  const feeUsdc = getAssociatedTokenAddressSync(usdc, f.feeWallet);
  await freezeAccount(connection, payer, X.vault, X.mint, payer);
  const feeY = getAssociatedTokenAddressSync(Y.mint, f.feeWallet);
  await freezeAccount(connection, payer, feeY, Y.mint, payer);

  let nonce = 7000;
  const intentLock = pda('large-basket-intent-lock', b.index, payer.publicKey);
  const common = { owner: payer.publicKey, index: b.index, indexMint: b.indexMint, stakingPool, quoteMint: usdc, vaultAuthority: b.vaultAuthority, intentLock, ownerIndexTokenAccount: b.ownerIndex, tokenProgram: TOKEN_PROGRAM_ID, systemProgram: SystemProgram.programId };
  const openRedeem = async (amount, ttl) => {
    const intent = pda('large-basket-intent', b.index, payer.publicKey, bn(++nonce).toArrayLike(Buffer, 'le', 8));
    await program.methods.openLargeBasketRedeemIntent({ nonce: bn(nonce), expiresAt: bn((await chainNow()) + ttl), inKind: true, indexAmountIn: bn(amount), minQuoteOut: bn(0) })
      .accounts({ ...common, intent }).remainingAccounts(b.pages.map(p => meta(p, true))).rpc();
    return intent;
  };
  const ownerAta = c => getAssociatedTokenAddressSync(c.mint, payer.publicKey, false, c.tokenProgram);
  const leg = (intent, i, feeAccount) => {
    const c = b.components[i];
    return program.methods.executeLargeBasketRedeemComponentInKind({ componentIndex: i }).accounts({ ...common, intent, componentPage: b.page, componentMint: c.mint, componentVault: c.vault, componentTokenProgram: c.tokenProgram, ownerComponentTokenAccount: ownerAta(c), protocolFeeComponentAccount: feeAccount ?? getAssociatedTokenAddressSync(c.mint, f.feeWallet, false, c.tokenProgram), creatorFeeComponentAccount: ownerAta(c), associatedTokenProgram: ASSOCIATED_TOKEN_PROGRAM_ID }).rpc();
  };
  const skip = (intent, i, owner = payer) => {
    const c = b.components[i];
    return program.methods.skipLargeBasketRedeemComponent({ componentIndex: i }).accounts({ owner: owner.publicKey, index: b.index, intent, intentLock, componentPage: b.page, componentMint: c.mint, componentVault: c.vault, componentTokenProgram: c.tokenProgram }).signers(owner === payer ? [] : [owner]).rpc();
  };

  // 10 of 100 index tokens: 10 of every component is owed.
  const r1 = await openRedeem(10_000_000, 600);
  assert.deepEqual(await reserves(b), [90_000_000n, 90_000_000n, 90_000_000n, 90_000_000n, 90_000_000n]);
  await leg(r1, 4);
  assert.equal(await balance(feeUsdc), 30_000n, '0.3% of 10 USDC to the fee recipient');
  // Only the recipient's own frozen account waives its fee: any other frozen account, even one
  // the recipient owns, is refused, so a redeemer can't dodge the fee by passing it.
  await rejectsWith(leg(r1, 3, X.vault), 'InvalidFeeRecipientTokenAccount', 'a frozen account of another token');
  const strayY = await createAccount(connection, payer, Y.mint, f.feeWallet, Keypair.generate(), { commitment: 'confirmed' }, Y.tokenProgram);
  await freezeAccount(connection, payer, strayY, Y.mint, payer);
  await rejectsWith(leg(r1, 3, strayY), 'InvalidFeeRecipientTokenAccount', 'a frozen account of the recipient that is not its associated one');
  // Y's fee account is frozen: the fee is waived and the redeemer gets all 10.
  await leg(r1, 3, feeY);
  assert.equal(await balance(ownerAta(Y)), 10_000_000n);
  assert.equal(await balance(feeY), 0n);
  // The frozen vault, the hooked mint and the paused mint can't pay out.
  for (const i of [0, 1, 2]) await assert.rejects(leg(r1, i), `component ${i} cannot move`);
  // Only the redeemer may give up their own components.
  await assert.rejects(skip(r1, 0, user), 'someone else skipping');
  await skip(r1, 0);
  await skip(r1, 1);
  let intent = await program.account.largeBasketIntent.fetch(r1);
  assert.ok(intent.status.open, 'still open until the last component');
  await skip(r1, 2);
  intent = await program.account.largeBasketIntent.fetch(r1);
  assert.ok(intent.status.finalized, 'skipping the last component settles the redeem');
  let state = await program.account.indexState.fetch(b.index);
  assert.equal(state.openIntentCount, 0, 'nothing left open to hold rebalances');
  assert.ok((await program.account.largeBasketIntentLock.fetch(intentLock)).activeIntent.equals(PublicKey.default), 'the owner can redeem again');
  // What X, H and P owed went back into the books; Y and USDC were paid out.
  assert.deepEqual(await reserves(b), [100_000_000n, 100_000_000n, 100_000_000n, 90_000_000n, 90_000_000n]);
  assert.equal((await program.account.largeBasketIntent.fetch(r1)).minQuoteOut.toString(), '0');

  // An expired redeem, cancelled by anyone: what can move comes back, what can't is forfeited
  // on the chain's own evidence, and the intent closes instead of staying open forever.
  const r2 = await openRedeem(9_000_000, 5); // 10% of the 90 left: X/H/P owe 10, Y/USDC owe 9
  await leg(r2, 4);
  await new Promise(resolve => setTimeout(resolve, 6_500));
  await assert.rejects(skip(r2, 0), 'skips end with the redeem window');
  const yBefore = await balance(ownerAta(Y));
  const ownerCash = getAssociatedTokenAddressSync(usdc, payer.publicKey);
  const group = (c, owner) => [meta(c.mint), meta(c.vault, true), meta(owner, true), meta(c.tokenProgram)];
  await program.methods.cancelExpiredLargeBasketIntent().accounts({ ...common, intent: r2 }).remainingAccounts([
    ...b.pages.map(p => meta(p, true)),
    ...group(X, ownerCash), ...group(H, ownerCash), ...group(P, ownerCash), ...group(Y, ownerAta(Y)),
  ]).rpc();
  intent = await program.account.largeBasketIntent.fetch(r2);
  assert.ok(intent.status.cancelled, 'the expired redeem closes');
  // Y came back in kind, less the 0.3% in-kind redeem fee, which stays in the basket: letting
  // a part-filled redeem expire is no way around the fee.
  assert.equal(await balance(ownerAta(Y)) - yBefore, 8_973_000n, 'Y came back in kind, less the fee');
  state = await program.account.indexState.fetch(b.index);
  assert.equal(state.openIntentCount, 0);
  // X, H and P are whole again in the books; Y came back to its owner, its fee booked back,
  // and USDC was paid out.
  assert.deepEqual(await reserves(b), [100_000_000n, 100_000_000n, 100_000_000n, 81_027_000n, 81_000_000n]);

  // The swap path the app uses: proceeds and fees in USDC, a floor on the proceeds, and the
  // leg that fills the last component settling the redeem. USDC needs no swap, so no route.
  const stakingAuthority = pda('staking-authority');
  const rewardVault = getAssociatedTokenAddressSync(usdc, stakingAuthority, true);
  const swapCommon = { ...common, stakingAuthority, ownerQuoteTokenAccount: ownerCash, quoteTokenProgram: TOKEN_PROGRAM_ID, associatedTokenProgram: ASSOCIATED_TOKEN_PROGRAM_ID };
  const swapFees = { feeRecipientQuoteTokenAccount: feeUsdc, creatorFeeRecipientQuoteTokenAccount: feeUsdc, stakingRewardVault: rewardVault };
  const jupiterProgram = new PublicKey('JUP6LkbZbjS1jKKwapdHNy74zcZ3tLUZoi5QNyVTaV4');
  const openSwapRedeem = async (amount, minQuoteOut) => {
    const intent = pda('large-basket-intent', b.index, payer.publicKey, bn(++nonce).toArrayLike(Buffer, 'le', 8));
    await program.methods.openLargeBasketRedeemIntent({ nonce: bn(nonce), expiresAt: bn((await chainNow()) + 600), inKind: false, indexAmountIn: bn(amount), minQuoteOut: bn(minQuoteOut) })
      .accounts({ ...common, intent }).remainingAccounts(b.pages.map(p => meta(p, true))).rpc();
    return intent;
  };
  const cashLeg = intent => program.methods.executeLargeBasketRedeemBatch({ entries: [{ componentIndex: 4, routeAccountCount: 0, swap: null, minQuoteOut: bn(0) }] })
    .accounts({ ...swapCommon, intent, jupiterProgram, ...swapFees })
    .remainingAccounts([meta(b.page), meta(CASH.mint), meta(CASH.vault, true), meta(TOKEN_PROGRAM_ID)]).rpc();

  // Skips first: the floor set on the full proceeds no longer applies, and the USDC leg, now
  // the last, settles the redeem and pays its 0.3% fee. 10% of the 81 left: USDC owes 8.1.
  const s1 = await openSwapRedeem(8_100_000, 8_000_000);
  for (const i of [0, 1, 2, 3]) await skip(s1, i);
  const cashBefore = await balance(ownerCash), feeBefore = await balance(feeUsdc);
  await cashLeg(s1);
  intent = await program.account.largeBasketIntent.fetch(s1);
  assert.ok(intent.status.finalized && intent.feesCollected, 'the last swap leg settles after skips');
  assert.equal(await balance(ownerCash) - cashBefore, 8_075_700n, '8.1 USDC less the 0.3% fee');
  assert.equal(await balance(feeUsdc) - feeBefore, 24_300n);
  assert.equal((await program.account.indexState.fetch(b.index)).openIntentCount, 0);
  // Y could move: the redeemer gave it up anyway, so its 8.1 went back into the books too.
  assert.deepEqual(await reserves(b), [100_000_000n, 100_000_000n, 100_000_000n, 81_027_000n, 72_900_000n]);

  // The other order: the USDC leg first, then skips. The skip that fills the last component
  // settles the redeem; the fee was already charged with the USDC leg.
  const s2 = await openSwapRedeem(7_290_000, 7_000_000);
  await cashLeg(s2);
  assert.ok((await program.account.largeBasketIntent.fetch(s2)).status.open);
  for (const i of [0, 1, 2]) await skip(s2, i);
  await skip(s2, 3);
  intent = await program.account.largeBasketIntent.fetch(s2);
  assert.ok(intent.status.finalized && intent.feesCollected && intent.minQuoteOut.isZero(), 'a skip settles a swap redeem too');
  assert.equal((await program.account.largeBasketIntentLock.fetch(intentLock)).activeIntent.toBase58(), PublicKey.default.toBase58());

  // A refund parked in escrow whose account then gets frozen: it stays claimable, but the
  // owner's lock is released once everything left there is stuck, so they can redeem again.
  const escrow = pda('refund-escrow', b.index);
  const escrowY = await createAssociatedTokenAccountIdempotent(connection, payer, Y.mint, escrow, {}, TOKEN_PROGRAM_ID, ASSOCIATED_TOKEN_PROGRAM_ID, true);
  const r5 = await openRedeem(6_561_000, 5);
  await leg(r5, 4);
  await new Promise(resolve => setTimeout(resolve, 6_500));
  await program.methods.cancelExpiredLargeBasketIntent().accounts({ ...common, intent: r5 }).remainingAccounts([
    ...b.pages.map(p => meta(p, true)),
    ...group(X, ownerCash), ...group(H, ownerCash), ...group(P, ownerCash), ...group(Y, escrowY),
  ]).rpc();
  intent = await program.account.largeBasketIntent.fetch(r5);
  assert.ok(intent.status.cancelled && intent.escrowedBitmap[0] === 0b1000, 'Y waits in escrow');
  assert.ok((await program.account.largeBasketIntentLock.fetch(intentLock)).activeIntent.equals(r5), 'the lock points at the claim');
  const escrowed = await balance(escrowY);
  await freezeAccount(connection, payer, escrowY, Y.mint, payer);
  const claim = () => program.methods.claimLargeBasketRefund().accounts({ owner: payer.publicKey, index: b.index, intent: r5, intentLock, refundEscrow: escrow })
    .remainingAccounts([...b.pages.map(p => meta(p)), meta(Y.mint), meta(escrowY, true), meta(ownerAta(Y), true), meta(TOKEN_PROGRAM_ID)]).rpc();
  await claim();
  intent = await program.account.largeBasketIntent.fetch(r5);
  assert.equal(intent.escrowedBitmap[0], 0b1000, 'still escrowed, still claimable');
  assert.ok((await program.account.largeBasketIntentLock.fetch(intentLock)).activeIntent.equals(PublicKey.default), 'the owner can redeem again');
  await thawAccount(connection, payer, escrowY, Y.mint, payer);
  const yBeforeClaim = await balance(ownerAta(Y));
  await claim();
  assert.equal(await balance(ownerAta(Y)) - yBeforeClaim, escrowed, 'claimed once it thawed');
  assert.equal((await program.account.largeBasketIntent.fetch(r5)).escrowedBitmap[0], 0);

  // ---------------------------------------------------------------- DRAIN: books meet the vault
  const d = f.drain;
  const sync = () => program.methods.syncLargeBasketReserves().accounts({ index: d.index, indexMint: d.indexMint }).remainingAccounts([meta(d.page, true), meta(d.components[0].vault)]).rpc();
  await sync();
  let drained = (await program.account.largeBasketComponentPage.fetch(d.page)).components[0];
  assert.equal(drained.accountedReserve.toString(), '60000000', 'written down to what the vault holds');
  assert.equal(drained.unitsPerIndex.toString(), '600000');
  await sync(); // nothing more to write down
  drained = (await program.account.largeBasketComponentPage.fetch(d.page)).components[0];
  assert.equal(drained.accountedReserve.toString(), '60000000');

  // ---------------------------------------------------------------- RBX: rebalances work around
  const r = f.rbx;
  const [X2, P2] = r.components;
  await freezeAccount(connection, payer, X2.vault, X2.mint, payer);
  const priceOracle = pda('price-oracle');
  const oracle = Keypair.generate();
  await program.methods.setPriceOracle({ oracle: oracle.publicKey }).accounts({ authority: payer.publicKey, protocolConfig: pda('protocol-config'), priceOracle, systemProgram: SystemProgram.programId }).rpc();
  const signed = async (intent, entries) => {
    const message = encodePriceMessage({ intent, slot: await connection.getSlot('confirmed'), entries });
    return priceSignatureInstruction({ oracle: oracle.publicKey, message, signature: signPriceMessage(oracle.secretKey, message) });
  };
  const send = ixs => program.provider.sendAndConfirm(new Transaction().add(...ixs));
  const rebalanceIntent = n => pda('rebalance-intent', r.index, bn(n).toArrayLike(Buffer, 'le', 8));
  const openIx = async (n, proofMints) => program.methods.openRebalanceIntent({ nonce: bn(n), expiresAt: bn((await chainNow()) + 600), maxPriceAgeSlots: bn(50), navToleranceBps: 50, maxPostRebalanceDriftBps: 100 })
    .accounts({ initiator: payer.publicKey, index: r.index, indexMint: r.indexMint, vaultAuthority: r.vaultAuthority, quoteMint: usdc, vaultQuoteTokenAccount: r.vaultQuote, intent: rebalanceIntent(n), priceOracle, instructionsSysvar: SYSVAR_INSTRUCTIONS_PUBKEY, associatedTokenProgram: ASSOCIATED_TOKEN_PROGRAM_ID, quoteTokenProgram: TOKEN_PROGRAM_ID, systemProgram: SystemProgram.programId })
    .remainingAccounts([...r.pages.map(p => meta(p)), ...r.components.map(c => meta(c.vault)), ...proofMints.map(m => meta(m))]).instruction();
  const onlyA = [{ componentIndex: 2, price: usd(1) }];
  // Without the paused mint shown, P2 is in the rebalance and needs a price nobody signed.
  await rejectsWith(send([await signed(rebalanceIntent(1), onlyA), await openIx(1, [])]), 'MissingOraclePrice', 'P2 not shown to be paused');
  // A mint that is not one of the basket's is no proof.
  await rejectsWith(send([await signed(rebalanceIntent(1), onlyA), await openIx(1, [usdc, Keypair.generate().publicKey])]), 'InvalidRemainingAccounts');
  await send([await signed(rebalanceIntent(1), onlyA), await openIx(1, [P2.mint])]);
  const opened = await program.account.rebalanceIntent.fetch(rebalanceIntent(1));
  assert.equal(opened.excludedBitmap[0], 0b0011, 'X2 (frozen) and P2 (paused) are left out');
  assert.deepEqual(opened.componentTargetAmounts.map(x => x.toNumber()), [0, 0, 0, 0], 'A and USDC already split the rest 50/50');
  // Mid-rebalance, A's issuer takes one atom through its delegate. That used to leave
  // finalize impossible; now the shortfall is booked as lost and the rebalance finishes.
  const A = r.components[2];
  const payerA = await createAssociatedTokenAccountIdempotent(connection, payer, A.mint, payer.publicKey);
  await transfer(connection, payer, A.vault, payerA, payer, 1);
  await send([await signed(rebalanceIntent(1), onlyA), await program.methods.finalizeRebalance({ maxPriceAgeSlots: bn(50) })
    .accounts({ keeper: payer.publicKey, index: r.index, intent: rebalanceIntent(1), vaultAuthority: r.vaultAuthority, quoteMint: usdc, vaultQuoteTokenAccount: r.vaultQuote, priceOracle, instructionsSysvar: SYSVAR_INSTRUCTIONS_PUBKEY, quoteTokenProgram: TOKEN_PROGRAM_ID })
    .remainingAccounts([...r.pages.map(p => meta(p, true)), ...r.components.map(c => meta(c.vault))]).instruction()]);
  assert.ok((await program.account.rebalanceIntent.fetch(rebalanceIntent(1))).status.finalized, 'finalized without pricing the stuck components');
  const after = await program.account.indexState.fetch(r.index);
  assert.ok(after.fixedWeightLastRebalancedAt.toNumber() > 0 && !after.largeBasketOperationInProgress);
  assert.deepEqual(await reserves(r), [100_000_000n, 100_000_000n, 99_999_999n, 100_000_000n], 'the atom the issuer took is gone from the books');

  // ---------------------------------------------------------------- DFRZ: a refund with nowhere to go
  // An expired redeem owes D, but its issuer made every new D account start frozen and the
  // refund escrow's D account is frozen too: no transfer can ever pay it. It is forfeited
  // instead, so the intent closes and stops holding the basket's rebalances.
  const z = f.dfrz;
  const [D, ZCASH] = z.components;
  const zLock = pda('large-basket-intent-lock', z.index, payer.publicKey);
  const zCommon = { owner: payer.publicKey, index: z.index, indexMint: z.indexMint, stakingPool, quoteMint: usdc, vaultAuthority: z.vaultAuthority, intentLock: zLock, ownerIndexTokenAccount: z.ownerIndex, tokenProgram: TOKEN_PROGRAM_ID, systemProgram: SystemProgram.programId };
  const z1 = pda('large-basket-intent', z.index, payer.publicKey, bn(++nonce).toArrayLike(Buffer, 'le', 8));
  await program.methods.openLargeBasketRedeemIntent({ nonce: bn(nonce), expiresAt: bn((await chainNow()) + 5), inKind: true, indexAmountIn: bn(10_000_000), minQuoteOut: bn(0) })
    .accounts({ ...zCommon, intent: z1 }).remainingAccounts([meta(z.page, true)]).rpc();
  const zCash = getAssociatedTokenAddressSync(usdc, payer.publicKey);
  await program.methods.executeLargeBasketRedeemComponentInKind({ componentIndex: 1 }).accounts({ ...zCommon, intent: z1, componentPage: z.page, componentMint: ZCASH.mint, componentVault: ZCASH.vault, componentTokenProgram: ZCASH.tokenProgram, ownerComponentTokenAccount: zCash, protocolFeeComponentAccount: zCash, creatorFeeComponentAccount: zCash, associatedTokenProgram: ASSOCIATED_TOKEN_PROGRAM_ID }).rpc();
  await new Promise(resolve => setTimeout(resolve, 6_500));
  const payerD = await createAssociatedTokenAccountIdempotent(connection, payer, D.mint, payer.publicKey, { commitment: 'confirmed' }, TOKEN_2022_PROGRAM_ID);
  const escrowD = await createAssociatedTokenAccountIdempotent(connection, payer, D.mint, pda('refund-escrow', z.index), { commitment: 'confirmed' }, TOKEN_2022_PROGRAM_ID, ASSOCIATED_TOKEN_PROGRAM_ID, true);
  assert.ok((await getAccount(connection, payerD, 'confirmed', TOKEN_2022_PROGRAM_ID)).isFrozen && (await getAccount(connection, escrowD, 'confirmed', TOKEN_2022_PROGRAM_ID)).isFrozen, 'new D accounts start frozen');
  const cancelZ = dest => program.methods.cancelExpiredLargeBasketIntent().accounts({ ...zCommon, intent: z1 }).remainingAccounts([meta(z.page, true), meta(D.mint), meta(D.vault, true), meta(dest, true), meta(TOKEN_2022_PROGRAM_ID)]).rpc();
  // The owner's frozen account can't take it, and that alone proves nothing.
  await assert.rejects(cancelZ(payerD), 'a frozen owner account takes no refund');
  await cancelZ(escrowD);
  intent = await program.account.largeBasketIntent.fetch(z1);
  assert.ok(intent.status.cancelled, 'the refund with nowhere to go is forfeited and the intent closes');
  assert.equal((await program.account.indexState.fetch(z.index)).openIntentCount, 0, 'nothing left open to hold rebalances');
  assert.deepEqual(await reserves(z), [100_000_000n, 90_000_000n], "D's share went back into the books; USDC was paid out");
  assert.equal(await balance(D.vault, TOKEN_2022_PROGRAM_ID), 100_000_000n);

  console.log('integration ok: a frozen vault, hooked mint and paused mint skipped on in-kind and swap redeems, in either order, the redeem settled and its share booked back; a refund stuck in a frozen escrow account stays claimable and no longer holds up its owner; a frozen fee account waived, only when it belongs to the recipient; an expired redeem forfeits only components the chain shows stuck, or whose refund has nowhere to go, and closes; a drained vault written down; a rebalance leaves stuck components unpriced and out, and finishes when an issuer takes tokens mid-rebalance');
}
