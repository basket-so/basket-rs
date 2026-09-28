import { Gateway } from "@switchboard-xyz/common";
import { Oracle } from "@switchboard-xyz/on-demand";

// Discovery is advisory. Switchboard still verifies oracle signatures on-chain.
export function createGatewayResolver({ discover, registered, probe, create = url => new Gateway(url), report = () => {} }) {
  let cached;
  let expires = 0;
  return async () => {
    if (cached && Date.now() < expires) return cached;
    let advertised = [];
    try { advertised = await discover(); } catch { /* Try the on-chain queue. */ }
    for (const [source, load] of [["Crossbar", async () => advertised], ["on-chain queue", registered]]) {
      const urls = [...new Set(await load())].filter(url => {
        try { return new URL(url).protocol === "https:"; } catch { return false; }
      });
      if (!urls.length) continue;
      const results = await Promise.allSettled(urls.map(async url => {
        await probe(url);
        return url;
      }));
      const healthy = results.find(result => result.status === "fulfilled");
      if (healthy) {
        cached = create(healthy.value);
        expires = Date.now() + 60_000;
        report(`Switchboard gateway discovered via ${source}: ${healthy.value}`);
        return cached;
      }
    }
    throw new Error("No reachable Switchboard gateways from Crossbar or the on-chain queue");
  };
}

export function installGatewayFallback(crossbar, getQueue, report) {
  crossbar.fetchGateway = createGatewayResolver({
    discover: async () => {
      const response = await fetch(`${crossbar.crossbarUrl}/gateways?network=mainnet`, { signal: AbortSignal.timeout(10_000) });
      if (!response.ok) throw new Error(`Gateway discovery HTTP ${response.status}`);
      const urls = await response.json();
      if (!Array.isArray(urls)) throw new Error("Invalid gateway directory");
      return urls;
    },
    registered: async () => {
      const queue = await getQueue();
      const keys = await queue.fetchOracleKeys();
      const data = await Oracle.loadMany(queue.program, keys);
      return data.filter(Boolean).map(item => Buffer.from(item.gatewayUri).toString().replace(/\0/g, ""));
    },
    probe: async url => {
      const response = await fetch(`${url.replace(/\/$/, "")}/gateway/api/v1/test`, { signal: AbortSignal.timeout(10_000) });
      if (!response.ok) throw new Error(`Gateway health HTTP ${response.status}`);
      await response.text();
    },
    report,
  });
}
