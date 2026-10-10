import { PublicKey } from '@solana/web3.js';

export const USDC = 'EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v';
export const TOKEN_PROGRAM = 'TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA';
export const TOKEN_2022 = 'TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb';

// Decimal arithmetic stays integer throughout sizing; JSON prices can use exponents.
export function rational(value) {
  const match = String(value).match(/^(\d+)(?:\.(\d+))?(?:e([+-]?\d+))?$/i);
  if (!match) throw new Error(`Invalid positive decimal: ${value}`);
  const scale = (match[2]?.length ?? 0) - Number(match[3] ?? 0);
  if (Math.abs(scale) > 100) throw new Error('Decimal exponent out of range');
  let numerator = BigInt(match[1] + (match[2] ?? ''));
  let denominator = 1n;
  if (scale >= 0) denominator = 10n ** BigInt(scale);
  else numerator *= 10n ** BigInt(-scale);
  if (!numerator) throw new Error('Price must be positive');
  return { numerator, denominator };
}

export function unitsForDollarNav(weightBps, price, decimals) {
  if (!Number.isInteger(weightBps) || weightBps <= 0 || weightBps > 10000)
    throw new Error('Invalid weight');
  if (!Number.isInteger(decimals) || decimals < 0 || decimals > 18)
    throw new Error('Invalid decimals');
  const { numerator, denominator } = rational(price);
  const n = BigInt(weightBps) * 10n ** BigInt(decimals) * denominator;
  const d = 10000n * numerator;
  const atoms = (n + d / 2n) / d;
  if (atoms <= 0n || atoms > 18446744073709551615n)
    throw new Error('Initial component units are zero or exceed u64');
  return atoms.toString();
}

export function validateCatalog(catalog) {
  new PublicKey(catalog.programId);
  const symbols = new Set();
  for (const b of catalog.baskets) {
    if (!/^[A-Z0-9]{1,10}$/.test(b.symbol) || symbols.has(b.symbol))
      throw new Error(`Invalid or duplicate symbol: ${b.symbol}`);
    symbols.add(b.symbol);
    if (!b.name || Buffer.byteLength(b.name) > 32 || b.startingNavUsd !== 1)
      throw new Error(`${b.symbol}: invalid name or initial NAV`);
    if (!['fixedUnits', 'fixedWeights'].includes(b.kind)) throw new Error('Invalid kind');
    if (!b.components.length || b.components.length > 10) throw new Error('A basket is created on one component page, so at most ten components');
    if (b.components.some(c => !Number.isInteger(c.weightBps) || c.weightBps <= 0) ||
        b.components.reduce((n, c) => n + c.weightBps, 0) !== 10000)
      throw new Error(`${b.symbol}: weights must sum to 10000 bps`);
    if (b.kind === 'fixedUnits' && (b.rebalanceIntervalSeconds || b.driftThresholdBps))
      throw new Error(`${b.symbol}: fixed units must not rebalance`);
    if (b.navRoundingReserve !== undefined && (b.navRoundingReserve !== 'USDC' ||
        catalog.tokens.USDC?.mint !== USDC || !b.components.some(c => c.symbol === 'USDC')))
      throw new Error(`${b.symbol}: NAV rounding reserve must be native USDC in the basket`);
    // A fixed-weight basket must hold a USDC slot (finalize refuses one without it); this adds
    // it at zero weight and zero units, as the cash its rebalances route through.
    if (b.usdcCashSlot !== undefined && (b.usdcCashSlot !== true || b.kind !== 'fixedWeights' ||
        catalog.tokens.USDC?.mint !== USDC || b.components.some(c => c.symbol === 'USDC') || b.components.length > 9))
      throw new Error(`${b.symbol}: USDC cash slot needs a fixed-weight basket without USDC and at most nine other components`);
    for (const key of ['feeRecipient', 'creatorFeeRecipient'])
      if (b[key] !== undefined && new PublicKey(b[key]).equals(PublicKey.default)) throw new Error(`${b.symbol}: ${key} must be set or omitted`);
    const mints = new Set();
    for (const component of b.components) {
      const token = catalog.tokens[component.symbol];
      if (!token) {
        if (!b.blockers.length) throw new Error(`Missing mint: ${component.symbol}`);
        continue;
      }
      new PublicKey(token.mint);
      if (mints.has(token.mint)) throw new Error('Duplicate component mint');
      mints.add(token.mint);
      if (![TOKEN_PROGRAM, TOKEN_2022].includes(token.tokenProgram)) throw new Error('Unsupported token program');
    }
  }
}

export function sizeBasket(basket, assets, tolerance = 0.00001) {
  const components = basket.components.map(c => {
    const asset = assets[c.symbol];
    if (!asset || !Number.isFinite(asset.price) || asset.price <= 0) throw new Error(`Missing price: ${c.symbol}`);
    const unitsPerIndex = unitsForDollarNav(c.weightBps, asset.price, asset.decimals);
    return { ...c, ...asset, unitsPerIndex,
      targetWeightBps: basket.kind === 'fixedWeights' ? c.weightBps : 0 };
  });
  // A BTC atom can be worth more than the entire NAV tolerance. An explicit
  // USDC allocation absorbs that rounding dust, with at most 5 bps adjustment.
  if (basket.navRoundingReserve !== undefined) {
    const reserve = components.find(c => c.symbol === basket.navRoundingReserve);
    if (basket.navRoundingReserve !== 'USDC' || !reserve || reserve.mint !== USDC ||
        reserve.decimals !== 6 || reserve.price !== 1)
      throw new Error('NAV rounding reserve must be native USDC valued at $1');
    let numerator = 0n, denominator = 1n;
    for (const c of components.filter(c => c !== reserve)) {
      const price = rational(c.price);
      const d = 10n ** BigInt(c.decimals) * price.denominator;
      numerator = numerator * d + BigInt(c.unitsPerIndex) * price.numerator * denominator;
      denominator *= d;
    }
    const residual = denominator - numerator;
    const atoms = (residual * 1000000n + denominator / 2n) / denominator;
    const adjustment = atoms - BigInt(reserve.unitsPerIndex);
    if (residual <= 0n || atoms <= 0n || atoms > 18446744073709551615n ||
        adjustment < -500n || adjustment > 500n)
      throw new Error('NAV rounding reserve adjustment exceeds 5 bps or is nonpositive');
    reserve.unitsPerIndex = atoms.toString();
  }
  if (basket.usdcCashSlot) {
    const usdc = assets.USDC;
    if (!usdc || usdc.mint !== USDC) throw new Error('Missing USDC for the cash slot');
    components.push({ symbol: 'USDC', weightBps: 0, ...usdc, unitsPerIndex: '0', targetWeightBps: 0 });
  }
  const nav = components.reduce((n, c) => n + Number(c.unitsPerIndex) / 10 ** c.decimals * c.price, 0);
  if (Math.abs(nav - 1) > tolerance) throw new Error(`${basket.symbol}: rounded NAV ${nav} exceeds $1 tolerance`);
  return { ...basket, components, initialNavUsd: nav };
}

export async function fetchJson(url, attempts = 4) {
  for (let i = 0; i < attempts; i++) {
    const key = process.env.JUPITER_API_KEY?.trim();
    const headers = new URL(url).hostname === 'api.jup.ag' && key ? { 'x-api-key': key } : {};
    const r = await fetch(url, { headers, signal: AbortSignal.timeout(20000) });
    if (r.ok) return r.json();
    if (i === attempts - 1 || (r.status !== 429 && r.status < 500))
      throw new Error(`HTTP ${r.status} from ${new URL(url).origin}`);
    await new Promise(resolve => setTimeout(resolve, 3000 * (i + 1)));
  }
}
