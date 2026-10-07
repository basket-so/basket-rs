import fs from 'node:fs';
import assert from 'node:assert/strict';
import anchor from '@coral-xyz/anchor';
import { PublicKey, Keypair, SystemProgram, Transaction, sendAndConfirmTransaction } from '@solana/web3.js';
import { AccountLayout, MintLayout, TOKEN_PROGRAM_ID, TOKEN_2022_PROGRAM_ID, ASSOCIATED_TOKEN_PROGRAM_ID, getAssociatedTokenAddressSync, getAccount, getMint } from '@solana/spl-token';

// Composition changes have a 3-day notice period, which a local validator cannot wait out.
// Baskets and proposals that are already due are written straight into the validator at
// startup (as rebalance-migration-fixtures does), so applying runs against real accounts.
const usdc = new PublicKey('EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v');
const bn = n => new anchor.BN(n);
const meta = (pubkey, isWritable = false) => ({ pubkey, isWritable, isSigner: false });
const SUPPLY = 100_000_000n;
const DAY = 86_400;

export async function prepareCompositionFixtures(dir, programId, payer) {
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
  const mint = (pk, authority, supply, owner = TOKEN_PROGRAM_ID) => {
    const data = Buffer.alloc(MintLayout.span);
    MintLayout.encode({ mintAuthorityOption: 1, mintAuthority: authority, supply, decimals: 6, isInitialized: true, freezeAuthorityOption: 0, freezeAuthority: PublicKey.default }, data);
    dump(pk, data, owner);
  };
  const token = (m, owner, amount) => {
    const pk = getAssociatedTokenAddressSync(m, owner, true), data = Buffer.alloc(AccountLayout.span);
    AccountLayout.encode({ mint: m, owner, amount, delegateOption: 0, delegate: PublicKey.default, state: 1, isNativeOption: 0, isNative: 0n, delegatedAmount: 0n, closeAuthorityOption: 0, closeAuthority: PublicKey.default }, data);
    dump(pk, data, TOKEN_PROGRAM_ID); return pk;
  };
  // Components the payer can mint in kind; additions the payer holds none of.
  const newMint = (ownerAccount = true) => { const m = Keypair.generate().publicKey; mint(m, payer, 2_000_000_000n); if (ownerAccount) token(m, payer, 1_000_000_000n); return m; };
  const now = Math.floor(Date.now() / 1000);

  // A fixed-weight basket with `weights` for its non-USDC components (plus a USDC cash
  // slot unless withCash is false), each holding 1 token per index token, and an optional
  // composition change that is already due.
  function basket(symbol, weights, { withCash = true, change, overrides = {} } = {}) {
    const [index, indexBump] = pda('index', payer, Buffer.from(symbol));
    const [indexMint, indexMintBump] = pda('index-mint', index);
    const [vaultAuthority, vaultAuthorityBump] = pda('vault-authority', index);
    const [page, pageBump] = pda('large-basket-component-page', index, Buffer.from([0]));
    mint(indexMint, vaultAuthority, SUPPLY);
    const ownerIndex = token(indexMint, payer, SUPPLY);
    const vaultQuote = token(usdc, vaultAuthority, 0n);
    const components = weights.map(weight => {
      const m = newMint();
      const vault = token(m, vaultAuthority, SUPPLY);
      return { mint: m, vault, tokenProgram: TOKEN_PROGRAM_ID, unitsPerIndex: bn(1_000_000), accountedReserve: bn(SUPPLY.toString()), targetWeightBps: weight, oraclePair: Keypair.generate().publicKey, decimals: 6 };
    });
    if (withCash) components.push({ mint: usdc, vault: vaultQuote, tokenProgram: TOKEN_PROGRAM_ID, unitsPerIndex: bn(0), accountedReserve: bn(0), targetWeightBps: 0, oraclePair: PublicKey.default, decimals: 6 });
    const state = zero({ defined: { name: 'indexState' } });
    Object.assign(state, { authority: payer, creator: payer, feeRecipient: payer, indexMint, vaultAuthorityBump, indexBump, indexMintBump, decimals: 6, kind: { fixedWeights: {} }, largeBasketComponentCount: components.length, largeBasketPageCount: 1, largeBasketConfigured: true, fixedWeightQuoteMint: usdc, fixedWeightRebalanceIntervalSeconds: bn(86400), fixedWeightDriftThresholdBps: 500, name: symbol, symbol, ...overrides });
    dump(index, encode('indexState', state), programId, 4096);
    dump(page, encode('largeBasketComponentPage', { index, pageIndex: 0, startComponentIndex: 0, componentCount: components.length, bump: pageBump, finalized: true, reserved: Array(32).fill(0), components }), programId, 1553);
    const [compositionChange, changeBump] = pda('composition-change', index);
    let additions = [];
    if (change) {
      additions = change.additions.map(weight => ({ mint: newMint(false), oraclePair: Keypair.generate().publicKey, targetWeightBps: weight }));
      dump(compositionChange, encode('compositionChange', { index, proposer: payer, proposedAt: bn(0), effectiveAt: bn(change.effectiveAt ?? now - 60), bump: changeBump, redeemFeeBps: 0, targetWeightsBps: change.weights, additions }), programId);
    }
    return { symbol, index, indexMint, vaultAuthority, page, vaultQuote, ownerIndex, components, compositionChange, additions };
  }

  const token2022Mint = Keypair.generate().publicKey;
  mint(token2022Mint, payer, 1_000_000n, TOKEN_2022_PROGRAM_ID);

  const fixtures = {
    // Removes the second component and adds a new one, all within page 0.
    swap: basket('COMPA', [6000, 4000], { change: { weights: [5000, 0, 0], additions: [5000] } }),
    // Eight slots (7 + cash): of three additions, two fill page 0 and one opens page 1.
    overflow: basket('COMPB', [4000, ...Array(6).fill(1000)], { change: { weights: [3000, ...Array(6).fill(1000), 0], additions: [400, 300, 300] } }),
    // A USDC cash slot registered after the proposal makes it stale.
    stale: basket('COMPC', [5000, 5000], { withCash: false, change: { weights: [6000, 4000], additions: [] } }),
    // No proposal yet: propose, validation and cancel run live.
    fresh: basket('COMPD', [5000, 5000]),
    // Due eight days ago: past the seven-day apply window.
    expired: basket('COMPE', [5000, 5000], { change: { weights: [6000, 4000, 0], additions: [], effectiveAt: now - 8 * DAY } }),
    // Due, but redemptions are paused / cost more than when proposed.
    paused: basket('COMPF', [5000, 5000], { change: { weights: [6000, 4000, 0], additions: [] }, overrides: { redeemingPaused: true } }),
    feeRaised: basket('COMPG', [5000, 5000], { change: { weights: [6000, 4000, 0], additions: [] }, overrides: { redeemFeeBps: 50 } }),
    token2022Mint,
  };
  return { validatorArgs, fixtures };
}

export async function testCompositionChange(program, connection, payer, user, stakingPool, fixtures) {
  const pda = (seed, ...p) => PublicKey.findProgramAddressSync([Buffer.from(seed), ...p.map(x => x.toBuffer ? x.toBuffer() : x)], program.programId)[0];
  const errorCode = name => new RegExp(`Error Code: ${name}\\.`);
  const applyAccounts = (f, operator = payer.publicKey) => ({ operator, index: f.index, vaultAuthority: f.vaultAuthority, compositionChange: f.compositionChange, proposer: payer.publicKey, associatedTokenProgram: ASSOCIATED_TOKEN_PROGRAM_ID, systemProgram: SystemProgram.programId });
  const additionAccounts = f => f.additions.flatMap(a => [meta(a.mint), meta(getAssociatedTokenAddressSync(a.mint, f.vaultAuthority, true), true), meta(TOKEN_PROGRAM_ID)]);
  const cancel = f => program.methods.cancelCompositionChange().accounts({ authority: payer.publicKey, index: f.index, compositionChange: f.compositionChange, proposer: payer.publicKey });
  // Pausing/unpausing redemptions or changing the redeem fee restarts a pending change's notice.
  const noticeAccount = f => [meta(f.compositionChange, true)];
  const config = (f, redeemingPaused, remaining = noticeAccount(f)) => program.methods.updateConfig({ feeRecipient: payer.publicKey, creatorFeeRecipient: PublicKey.default, maxSupply: bn(0), rebalanceDelaySeconds: bn(0), mintingPaused: false, redeemingPaused, rebalancingPaused: false }).accounts({ authority: payer.publicKey, index: f.index }).remainingAccounts(remaining);
  const fees = (f, redeemFeeBps, mintFeeBps = 0, remaining = noticeAccount(f)) => program.methods.updateFees({ mintFeeBps, redeemFeeBps, creatorMintFeeBps: 0, creatorRedeemFeeBps: 0, stakingMintFeeBps: 0, stakingRedeemFeeBps: 0 }).accounts({ authority: payer.publicKey, index: f.index }).remainingAccounts(remaining);
  const effectiveAt = async f => (await program.account.compositionChange.fetch(f.compositionChange)).effectiveAt.toNumber();
  const chainNow = async () => Number((await connection.getAccountInfo(new PublicKey('SysvarC1ock11111111111111111111111111111111'))).data.readBigInt64LE(32));
  let nonce = 9000;

  // --- Propose, validate and cancel on a live basket ---
  const f = fixtures.fresh;
  const newComponent = fixtures.swap.additions[0].mint; // any existing SPL mint
  const propose = (weights, additions, authority = payer) => program.methods
    .proposeCompositionChange({ targetWeightsBps: weights, additions: additions.map(a => ({ mint: a.mint, oraclePair: a.oraclePair ?? Keypair.generate().publicKey, targetWeightBps: a.weight })) })
    .accounts({ authority: authority.publicKey, index: f.index, compositionChange: f.compositionChange, systemProgram: SystemProgram.programId })
    .remainingAccounts([meta(f.page), ...additions.map(a => meta(a.mint))])
    .signers(authority === payer ? [] : [authority]);
  await assert.rejects(propose([5000, 4000, 0], []).rpc(), errorCode('InvalidCompositionChange'), 'weights must sum to 100%');
  await assert.rejects(propose([5000, 4000, 1000], []).rpc(), errorCode('InvalidCompositionChange'), 'USDC cash weight is fixed');
  await assert.rejects(propose([5000, 5000, 0], []).rpc(), errorCode('InvalidCompositionChange'), 'no-op change');
  await assert.rejects(propose([5000, 4990, 0], [{ mint: newComponent, weight: 10 }]).rpc(), errorCode('InvalidCompositionChange'), 'weight below the minimum');
  await assert.rejects(propose([5000, 4000, 0], [{ mint: f.components[0].mint, weight: 1000 }]).rpc(), errorCode('DuplicateComponentMint'), 'existing mint');
  await assert.rejects(propose([5000, 4000, 0], [{ mint: usdc, weight: 1000 }]).rpc(), errorCode('InvalidComponentMint'), 'USDC addition');
  await assert.rejects(propose([5000, 4000, 0], [{ mint: fixtures.token2022Mint, weight: 1000 }]).rpc(), errorCode('InvalidTokenProgram'), 'Token-2022 addition');
  await assert.rejects(propose([5000, 4000, 0], [{ mint: newComponent, weight: 1000 }], user).rpc(), errorCode('UnauthorizedAuthority'), 'only the authority proposes');
  const before = await chainNow();
  await propose([5000, 4000, 0], [{ mint: newComponent, weight: 1000 }]).rpc();
  const proposed = await program.account.compositionChange.fetch(f.compositionChange);
  assert.deepEqual(proposed.targetWeightsBps, [5000, 4000, 0]);
  assert.equal(proposed.additions.length, 1);
  assert.ok(proposed.additions[0].mint.equals(newComponent));
  const notice = proposed.effectiveAt.toNumber() - before;
  assert.ok(notice >= 3 * DAY && notice <= 3 * DAY + 60, `three days of notice, got ${notice}s`);
  assert.equal(proposed.redeemFeeBps, 0);
  await assert.rejects(propose([4000, 6000, 0], []).rpc(), /already in use/, 'one pending change at a time');
  // Changing the exit terms restarts the notice, so holders always get three days at the
  // terms in force when the change applies. The proposal must be passed to do it.
  await assert.rejects(config(f, true, []).rpc(), errorCode('InvalidRemainingAccounts'), 'pausing redemptions needs the proposal');
  await assert.rejects(config(f, true, [meta(f.page, true)]).rpc(), errorCode('InvalidRemainingAccounts'), 'only the basket\'s proposal');
  await new Promise(r => setTimeout(r, 2000));
  await config(f, true).rpc();
  const afterPause = await effectiveAt(f);
  assert.ok(afterPause > proposed.effectiveAt.toNumber() && afterPause - (await chainNow()) > 3 * DAY - 60, 'pausing restarts the notice');
  await new Promise(r => setTimeout(r, 2000));
  await config(f, false).rpc();
  assert.ok((await effectiveAt(f)) > afterPause, 'unpausing restarts it again');
  await assert.rejects(fees(f, 50, 0, []).rpc(), errorCode('InvalidRemainingAccounts'), 'a redeem fee change needs the proposal');
  const beforeFee = await effectiveAt(f);
  await new Promise(r => setTimeout(r, 2000));
  await fees(f, 50).rpc();
  assert.ok((await effectiveAt(f)) > beforeFee, 'a redeem fee change restarts the notice');
  await fees(f, 0).rpc();
  // Mint fees and other settings leave the notice alone and need no extra account.
  const beforeMintFee = await effectiveAt(f);
  await fees(f, 0, 25, []).rpc();
  await config(f, false, []).rpc();
  assert.equal(await effectiveAt(f), beforeMintFee);
  await fees(f, 0, 0, []).rpc();
  const freshApply = program.methods.applyCompositionChange().accounts(applyAccounts(f))
    .remainingAccounts([meta(f.page, true), meta(newComponent), meta(getAssociatedTokenAddressSync(newComponent, f.vaultAuthority, true), true), meta(TOKEN_PROGRAM_ID)]);
  await assert.rejects(freshApply.rpc(), errorCode('CompositionChangeNotReady'), 'notice period');
  await assert.rejects(program.methods.applyCompositionChange().accounts(applyAccounts(f, user.publicKey)).remainingAccounts([meta(f.page, true)]).signers([user]).rpc(), errorCode('NotRebalanceOperator'));
  await assert.rejects(program.methods.cancelCompositionChange().accounts({ authority: user.publicKey, index: f.index, compositionChange: f.compositionChange, proposer: payer.publicKey }).signers([user]).rpc(), errorCode('UnauthorizedAuthority'));
  await cancel(f).rpc();
  assert.equal(await connection.getAccountInfo(f.compositionChange), null);

  // --- Applying needs redemptions open at no higher fees than when proposed ---
  for (const g of [fixtures.paused, fixtures.feeRaised]) {
    await assert.rejects(program.methods.applyCompositionChange().accounts(applyAccounts(g)).remainingAccounts([meta(g.page, true)]).rpc(), errorCode('CompositionChangeExitRestricted'), g.symbol);
    await cancel(g).rpc();
  }

  // --- A due change lapses after the apply window ---
  const e = fixtures.expired;
  await assert.rejects(program.methods.applyCompositionChange().accounts(applyAccounts(e)).remainingAccounts([meta(e.page, true)]).rpc(), errorCode('CompositionChangeExpired'));
  await cancel(e).rpc();

  // --- Apply: remove a component and add one ---
  const s = fixtures.swap;
  const common = { owner: payer.publicKey, index: s.index, indexMint: s.indexMint, stakingPool, quoteMint: usdc, vaultAuthority: s.vaultAuthority, intentLock: pda('large-basket-intent-lock', s.index, payer.publicKey), ownerIndexTokenAccount: s.ownerIndex, tokenProgram: TOKEN_PROGRAM_ID, systemProgram: SystemProgram.programId };
  const makeIntent = () => pda('large-basket-intent', s.index, payer.publicKey, bn(++nonce).toArrayLike(Buffer, 'le', 8));
  const redeemArgs = (amount, ttl = 600) => ({ nonce: bn(nonce), expiresAt: bn(Math.floor(Date.now() / 1000) + ttl), inKind: true, indexAmountIn: bn(amount), minQuoteOut: bn(0) });
  // An open intent settles against the current layout, so applying waits for it.
  let intent = makeIntent();
  await program.methods.openLargeBasketRedeemIntent(redeemArgs(10_000_000)).accounts({ ...common, intent }).remainingAccounts([meta(s.page, true)]).rpc();
  const apply = program.methods.applyCompositionChange().accounts(applyAccounts(s)).remainingAccounts([meta(s.page, true), ...additionAccounts(s)]);
  await assert.rejects(apply.rpc(), errorCode('IntentsStillOpen'));
  await program.methods.cancelUnfilledLargeBasketRedeemIntent().accounts({ ...common, intent }).remainingAccounts([meta(s.page, true)]).rpc();
  await assert.rejects(program.methods.applyCompositionChange().accounts(applyAccounts(s)).remainingAccounts([meta(s.page, true)]).rpc(), errorCode('InvalidRemainingAccounts'));
  await apply.rpc();
  const page = await program.account.largeBasketComponentPage.fetch(s.page);
  assert.deepEqual(page.components.map(c => c.targetWeightBps), [5000, 0, 0, 5000]);
  assert.equal(page.componentCount, 4);
  const added = page.components[3];
  assert.ok(added.mint.equals(s.additions[0].mint) && added.oraclePair.equals(s.additions[0].oraclePair));
  assert.equal(added.unitsPerIndex.toString(), '0');
  assert.equal(added.accountedReserve.toString(), '0');
  assert.equal(added.decimals, 6);
  const addedVault = getAssociatedTokenAddressSync(added.mint, s.vaultAuthority, true);
  assert.ok(added.vault.equals(addedVault));
  assert.equal((await getAccount(connection, addedVault)).amount, 0n);
  let state = await program.account.indexState.fetch(s.index);
  assert.equal(state.largeBasketComponentCount, 4);
  assert.equal(state.largeBasketPageCount, 1);
  assert.equal(state.pageGeneration.toString(), '1');
  assert.equal(state.compositionRebalanceDue, true);
  assert.equal(await connection.getAccountInfo(s.compositionChange), null);
  await assert.rejects(apply.rpc(), /AccountNotInitialized/, 'a change applies once');

  // Until a rebalance buys it in, the new component moves a zero amount in mints and redeems,
  // and the removed one is still held pro rata.
  const components = page.components;
  intent = makeIntent();
  const mintArgs = { nonce: bn(nonce), expiresAt: bn(Math.floor(Date.now() / 1000) + 600), inKind: true, indexAmountOut: bn(10_000_000), maxQuoteIn: bn(0) };
  await program.methods.openLargeBasketMintIntent(mintArgs).accounts({ ...common, intent }).remainingAccounts([meta(s.page, true)]).rpc();
  assert.deepEqual((await program.account.largeBasketIntent.fetch(intent)).componentAmounts.map(a => a.toString()), ['10000000', '10000000', '0', '0']);
  const ownerAccount = c => getAssociatedTokenAddressSync(c.mint, payer.publicKey);
  const inKind = (method, c, i) => program.methods[method]({ componentIndex: i }).accounts({ ...common, intent, componentPage: s.page, componentMint: c.mint, componentVault: c.vault, componentTokenProgram: TOKEN_PROGRAM_ID, ownerComponentTokenAccount: ownerAccount(c), protocolFeeComponentAccount: ownerAccount(c), creatorFeeComponentAccount: ownerAccount(c), associatedTokenProgram: ASSOCIATED_TOKEN_PROGRAM_ID });
  for (const [i, c] of components.entries()) await inKind('executeLargeBasketMintComponentInKind', c, i).rpc();
  await program.methods.finalizeLargeBasketMintIntent().accounts({ ...common, intent, associatedTokenProgram: ASSOCIATED_TOKEN_PROGRAM_ID }).remainingAccounts([meta(s.page, true)]).rpc();
  assert.equal((await getMint(connection, s.indexMint)).supply, SUPPLY + 10_000_000n);
  assert.equal((await getAccount(connection, addedVault)).amount, 0n);
  // A zero-amount in-kind redeem leg sends nothing, and opens no token account for it.
  intent = makeIntent();
  await program.methods.openLargeBasketRedeemIntent(redeemArgs(10_000_000, 5)).accounts({ ...common, intent }).remainingAccounts([meta(s.page, true)]).rpc();
  await inKind('executeLargeBasketRedeemComponentInKind', added, 3).rpc();
  assert.equal(await connection.getAccountInfo(ownerAccount(added)), null);
  while ((await chainNow()) <= Number((await program.account.largeBasketIntent.fetch(intent)).expiresAt)) await new Promise(r => setTimeout(r, 1000));
  await program.methods.cancelExpiredLargeBasketIntent().accounts({ ...common, intent }).remainingAccounts([
    meta(s.page, true),
    // Only owed components: the two held ones (the cash slot is empty, the new one was filled).
    ...components.slice(0, 2).flatMap(c => [meta(c.mint), meta(c.vault, true), meta(ownerAccount(c), true), meta(TOKEN_PROGRAM_ID)]),
  ]).rpc();
  state = await program.account.indexState.fetch(s.index);
  assert.equal(state.openIntentCount, 0);

  // --- Apply across a page boundary, at a pre-funded page address ---
  const o = fixtures.overflow;
  const nextPage = pda('large-basket-component-page', o.index, Buffer.from([1]));
  await sendAndConfirmTransaction(connection, new Transaction().add(SystemProgram.transfer({ fromPubkey: payer.publicKey, toPubkey: nextPage, lamports: 1_000_000 })), [payer], { commitment: 'confirmed' });
  await program.methods.applyCompositionChange().accounts(applyAccounts(o)).remainingAccounts([meta(o.page, true), meta(nextPage, true), ...additionAccounts(o)]).rpc();
  const first = await program.account.largeBasketComponentPage.fetch(o.page);
  assert.equal(first.componentCount, 10);
  assert.deepEqual(first.components.map(c => c.targetWeightBps), [3000, 1000, 1000, 1000, 1000, 1000, 1000, 0, 400, 300]);
  assert.ok(first.components[8].mint.equals(o.additions[0].mint) && first.components[9].mint.equals(o.additions[1].mint));
  const second = await program.account.largeBasketComponentPage.fetch(nextPage);
  assert.equal(second.pageIndex, 1);
  assert.equal(second.startComponentIndex, 10);
  assert.equal(second.finalized, true);
  assert.equal(second.components.length, 1);
  assert.ok(second.components[0].mint.equals(o.additions[2].mint));
  assert.equal(second.components[0].targetWeightBps, 300);
  state = await program.account.indexState.fetch(o.index);
  assert.equal(state.largeBasketComponentCount, 11);
  assert.equal(state.largeBasketPageCount, 2);
  // Intents read both pages: the new components carry zero amounts.
  const oCommon = { ...common, index: o.index, indexMint: o.indexMint, vaultAuthority: o.vaultAuthority, intentLock: pda('large-basket-intent-lock', o.index, payer.publicKey), ownerIndexTokenAccount: o.ownerIndex };
  intent = pda('large-basket-intent', o.index, payer.publicKey, bn(++nonce).toArrayLike(Buffer, 'le', 8));
  await program.methods.openLargeBasketRedeemIntent(redeemArgs(10_000_000)).accounts({ ...oCommon, intent }).remainingAccounts([meta(o.page, true), meta(nextPage, true)]).rpc();
  const amounts = (await program.account.largeBasketIntent.fetch(intent)).componentAmounts.map(a => a.toString());
  assert.equal(amounts.length, 11);
  assert.deepEqual(amounts.slice(7), ['0', '0', '0', '0']);
  await program.methods.cancelUnfilledLargeBasketRedeemIntent().accounts({ ...oCommon, intent }).remainingAccounts([meta(o.page, true), meta(nextPage, true)]).rpc();

  // --- A component registered after the proposal makes it stale ---
  const st = fixtures.stale;
  await program.methods.registerRebalanceQuote().accounts({ payer: payer.publicKey, index: st.index, indexMint: st.indexMint, vaultAuthority: st.vaultAuthority, quoteMint: usdc, vaultQuote: st.vaultQuote, quotePage: st.page, associatedTokenProgram: ASSOCIATED_TOKEN_PROGRAM_ID, tokenProgram: TOKEN_PROGRAM_ID, systemProgram: SystemProgram.programId }).remainingAccounts([meta(st.page)]).rpc();
  await assert.rejects(program.methods.applyCompositionChange().accounts(applyAccounts(st)).remainingAccounts([meta(st.page, true)]).rpc(), errorCode('CompositionChangeStale'));
  await cancel(st).rpc();

  console.log('integration ok: composition change propose/validate/cancel, notice period and its restarts, apply window, exit-restriction, open-intent and stale guards, removal + addition applied, zero-amount in-kind mint and redeem of the new component, partial page overflow onto a pre-funded new page');
}
