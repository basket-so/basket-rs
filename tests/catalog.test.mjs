import test from 'node:test';
import assert from 'node:assert/strict';
import fs from 'node:fs';
import { rational, unitsForDollarNav, sizeBasket, validateCatalog } from '../scripts/lib/catalog.mjs';

const catalog = JSON.parse(fs.readFileSync(new URL('../docs/catalog/baskets.json', import.meta.url)));
test('catalog preserves all six landing compositions and adds nine researched baskets', () => {
  validateCatalog(catalog);
  assert.equal(catalog.baskets.filter(b => b.origin === 'research').length, 9);
  const expected = {
    AIDX: [28,22,18,16,10,6], SOLB: [40,18,16,14,7,5], DPIN: [26,20,16,14,12,12],
    MEME: [22,20,16,14,14,14], RWAX: [36,28,16,12,8], LSTY: [30,22,18,16,14],
  };
  for (const [symbol, weights] of Object.entries(expected)) {
    const b = catalog.baskets.find(b => b.symbol === symbol);
    assert.equal(b.origin, 'landing');
    assert.deepEqual(b.components.map(c => c.weightBps), weights.map(w => w * 100));
  }
});
test('sizes $1 total NAV, not $1 per constituent, across extreme prices', () => {
  const basket = catalog.baskets.find(b => b.symbol === 'MEME');
  const assets = Object.fromEntries(basket.components.map((c,i) => [c.symbol, { price: [0.2, 0.000000003, 0.05, 0.0004, 0.17, 0.05][i], decimals: 9 }]));
  const result = sizeBasket(basket, assets);
  assert.ok(Math.abs(result.initialNavUsd - 1) < 1e-8);
  assert.ok(result.components.every(c => c.targetWeightBps === 0));
  assert.equal(unitsForDollarNav(2000, '3e-9', 9), '66666666666666667');
});
test('fixed weights retain target bps and initialize nonzero quantities', () => {
  const b = catalog.baskets.find(b => b.symbol === 'SOLC');
  const r = sizeBasket(b, { SOL: { price: 100, decimals: 9 }, USDC: { price: 1, decimals: 6 }, JitoSOL: { price: 125, decimals: 9 } });
  assert.equal(r.initialNavUsd, 1);
  assert.deepEqual(r.components.map(c => c.unitsPerIndex), ['4000000','400000','1600000']);
  assert.deepEqual(r.components.map(c => c.targetWeightBps), [4000,4000,2000]);
});
test('rejects invalid prices, zero-atom positions, overflow and excessive NAV rounding', () => {
  for (const price of [0,-1,NaN,Infinity,'1e200']) assert.throws(() => rational(price));
  assert.throws(() => unitsForDollarNav(1000, 100000, 0), /zero/);
  assert.throws(() => unitsForDollarNav(10000, '1e-30', 18), /u64/);
  assert.throws(() => sizeBasket({ symbol:'X',kind:'fixedUnits',components:[{symbol:'A',weightBps:10000}] }, { A:{price:0.6,decimals:0} }), /tolerance/);
});
test('rejects accidental duplicate mints and allocations that do not sum to 100%', () => {
  const clone = structuredClone(catalog);
  clone.baskets[1].components[1].symbol = clone.baskets[1].components[0].symbol;
  assert.throws(() => validateCatalog(clone), /Duplicate component mint/);
  const badWeight = structuredClone(catalog);
  badWeight.baskets[1].components[0].weightBps++;
  assert.throws(() => validateCatalog(badWeight), /10000/);
});

test('majors reserve absorbs BTC rounding in either direction while preserving $1 NAV', () => {
  const b = catalog.baskets.find(b => b.symbol === 'MAJR');
  for (const btc of [79072.34913670164, 80001, 79999, 100000]) {
    const assets = Object.fromEntries(b.components.map(c => [c.symbol, {
      ...catalog.tokens[c.symbol], price: { cbBTC: btc, ETH: 2489.5277, SOL: 110.4567, USDC: 1 }[c.symbol],
    }]));
    const result = sizeBasket(b, assets);
    assert.ok(Math.abs(result.initialNavUsd - 1) <= 0.0000005);
    const reserve = result.components.find(c => c.symbol === 'USDC');
    assert.ok(Math.abs(Number(reserve.unitsPerIndex) - 50000) <= 500);
    assert.ok(result.components.every(c => c.targetWeightBps === 0));
  }
});

test('rounding reserve rejects excessive correction, wrong asset and invalid valuation', () => {
  const b = catalog.baskets.find(b => b.symbol === 'MAJR');
  const assets = Object.fromEntries(b.components.map(c => [c.symbol, {
    ...catalog.tokens[c.symbol], price: { cbBTC: 70000000, ETH: 2500, SOL: 100, USDC: 1 }[c.symbol],
  }]));
  assert.throws(() => sizeBasket(b, assets), /5 bps/);
  assets.cbBTC.price = 80000;
  assets.USDC.price = 0.99;
  assert.throws(() => sizeBasket(b, assets), /valued at/);
  const clone = structuredClone(catalog);
  clone.baskets.find(b => b.symbol === 'MAJR').navRoundingReserve = 'SOL';
  assert.throws(() => validateCatalog(clone), /native USDC/);
});
