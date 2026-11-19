#!/usr/bin/env node
/**
 * Reproducible Soroban storage TTL and restoration cost projector for HyperionRouter.
 *
 * Estimates ledger entry byte footprints, periodic TTL upkeep (`extend_ttl` / `keep_alive`),
 * and worst-case archival restoration (`RestoreFootprintOp`) costs from a deployment record
 * and/or contract footprint counts.
 *
 * Usage:
 *   node script/stellar/storage-cost.mjs [options]
 *
 * Options:
 *   --record <path>          Deployment record JSON (extracts token/adapter counts)
 *   --footprint <path>       JSON file matching `get_storage_footprint` output
 *   --out-nonce <n>          Number of Transfer(u64) entries (default: 0)
 *   --claim-count <n>        Total Claim(u64) entries ever created (default: 0)
 *   --archived-claims <n>    Archived settled claims removed via cleanup (default: 0)
 *   --queue-count <n>        Number of Queued(u64) entries (default: 0)
 *   --processed-count <n>    Number of Processed(RouteKind, BytesN<32>) entries (default: 0)
 *   --tokens <n>             Registered token count (overridden by --record if present)
 *   --routes <n>             Enabled route count (overridden by --record if present)
 *   --bump-ledgers <n>       Ledgers extended per maintenance cycle (default: 518400, ~30d)
 *   --json                   Emit JSON instead of human-readable report
 */

import fs from "node:fs";

// Approximate XDR-serialized ledger key + entry sizes (bytes) for HyperionRouter DataKeys.
const ENTRY_BYTES = {
  instance: 1450,          // Config, OutNonce, ClaimSeq, QueueSeq, ArchivedClaimCount, Adapters, RailReceivers, RouteEnabled
  tokenConfig: 128,        // DataKey::Token(Address) -> TokenConfig
  routeConfig: 80,         // DataKey::RouteConfig(RouteKind) -> RouteConfig
  routeFlowLimit: 112,     // DataKey::RouteFlowLimit(Address, RouteKind) -> i128
  flowWindow: 144,         // DataKey::Flow(Address, RouteKind) -> FlowWindow
  transferRecord: 320,     // DataKey::Transfer(u64) -> TransferRecord
  pendingClaim: 288,       // DataKey::Claim(u64) -> PendingClaim
  claimSettledAt: 72,      // DataKey::ClaimSettledAt(u64) -> u64
  processedReplay: 96,     // DataKey::Processed(RouteKind, BytesN<32>) -> bool
  queuedAction: 224,       // DataKey::Queued(u64) -> QueuedAction
};

// Conservative Soroban network resource pricing defaults (stroops).
// Operators can override via flags/env if network fee parameters change.
const DEFAULTS = {
  bumpLedgers: 518_400,              // BUMP_TO (~30 days at ~5s/ledger)
  bumpThreshold: 17_280,             // BUMP_THRESHOLD (~1 day)
  ledgersPerYear: 6_307_200,         // ~5s per ledger
  persistentRentStroopsPerKb1M: 210, // ~210 stroops per KB per 1M ledgers (persistent)
  instanceRentStroopsPerKb1M: 420,   // ~420 stroops per KB per 1M ledgers (instance)
  writeFeeStroopsPerKb: 1_200,       // RestoreFootprintOp write fee per KB
  txBaseFeeStroops: 100,             // Base inclusion fee per tx
  keysPerMaintenanceTx: 40,          // Conservative footprint key batch per tx
};

function parseArgs(argv) {
  const opts = {
    record: null,
    footprint: null,
    outNonce: 0,
    claimCount: 0,
    archivedClaims: 0,
    queueCount: 0,
    processedCount: 0,
    tokens: 1,
    routes: 2,
    bumpLedgers: DEFAULTS.bumpLedgers,
    json: false,
  };
  for (let i = 0; i < argv.length; i++) {
    const a = argv[i];
    const next = () => {
      if (i + 1 >= argv.length) throw new Error(`missing value for ${a}`);
      return argv[++i];
    };
    if (a === "--record") opts.record = next();
    else if (a === "--footprint") opts.footprint = next();
    else if (a === "--out-nonce") opts.outNonce = Number(next());
    else if (a === "--claim-count") opts.claimCount = Number(next());
    else if (a === "--archived-claims") opts.archivedClaims = Number(next());
    else if (a === "--queue-count") opts.queueCount = Number(next());
    else if (a === "--processed-count") opts.processedCount = Number(next());
    else if (a === "--tokens") opts.tokens = Number(next());
    else if (a === "--routes") opts.routes = Number(next());
    else if (a === "--bump-ledgers") opts.bumpLedgers = Number(next());
    else if (a === "--json") opts.json = true;
    else throw new Error(`unknown option: ${a}`);
  }
  return opts;
}

function loadInputs(opts) {
  let networkKey = "custom";
  let router = null;
  let tokens = opts.tokens;
  let routes = opts.routes;

  if (opts.record) {
    const rec = JSON.parse(fs.readFileSync(opts.record, "utf8"));
    const keys = Object.keys(rec.networks ?? {});
    if (keys.length > 0) {
      networkKey = keys[0];
      const net = rec.networks[networkKey];
      router = net.router ?? null;
      tokens = Object.keys(net.tokens ?? {}).length || tokens;
      routes = Object.keys(net.adapters ?? {}).length || routes;
    }
  }

  let outNonce = opts.outNonce;
  let claimCount = opts.claimCount;
  let archivedClaims = opts.archivedClaims;
  let queueCount = opts.queueCount;
  let bumpLedgers = opts.bumpLedgers;

  if (opts.footprint) {
    const fp = JSON.parse(fs.readFileSync(opts.footprint, "utf8"));
    outNonce = Number(fp.out_nonce ?? outNonce);
    claimCount = Number(fp.claim_count ?? claimCount);
    archivedClaims = Number(fp.archived_claims ?? archivedClaims);
    queueCount = Number(fp.queue_count ?? queueCount);
    if (fp.bump_to) bumpLedgers = Number(fp.bump_to);
  }

  const activeClaims = Math.max(0, claimCount - archivedClaims);
  const processedCount = opts.processedCount > 0 ? opts.processedCount : claimCount;

  return {
    networkKey,
    router,
    tokens,
    routes,
    outNonce,
    claimCount,
    archivedClaims,
    activeClaims,
    queueCount,
    processedCount,
    bumpLedgers,
  };
}

export function projectStorageCosts(input) {
  const flowEntries = input.tokens * input.routes;
  const categories = [
    { name: "Instance (Config + Seq + Adapters)", storage: "instance", count: 1, bytesEach: ENTRY_BYTES.instance },
    { name: "Token(Address)", storage: "persistent", count: input.tokens, bytesEach: ENTRY_BYTES.tokenConfig },
    { name: "RouteConfig(RouteKind)", storage: "persistent", count: input.routes, bytesEach: ENTRY_BYTES.routeConfig },
    { name: "RouteFlowLimit(Address,RouteKind)", storage: "persistent", count: flowEntries, bytesEach: ENTRY_BYTES.routeFlowLimit },
    { name: "Flow(Address,RouteKind)", storage: "persistent", count: flowEntries, bytesEach: ENTRY_BYTES.flowWindow },
    { name: "Transfer(u64)", storage: "persistent", count: input.outNonce, bytesEach: ENTRY_BYTES.transferRecord },
    { name: "Claim(u64) [active]", storage: "persistent", count: input.activeClaims, bytesEach: ENTRY_BYTES.pendingClaim },
    { name: "Processed(RouteKind,BytesN<32>)", storage: "persistent", count: input.processedCount, bytesEach: ENTRY_BYTES.processedReplay },
    { name: "Queued(u64)", storage: "persistent", count: input.queueCount, bytesEach: ENTRY_BYTES.queuedAction },
  ];

  let instanceBytes = 0;
  let persistentBytes = 0;
  let persistentEntries = 0;

  for (const c of categories) {
    c.totalBytes = c.count * c.bytesEach;
    if (c.storage === "instance") {
      instanceBytes += c.totalBytes;
    } else {
      persistentBytes += c.totalBytes;
      persistentEntries += c.count;
    }
  }

  const cyclesPerYear = DEFAULTS.ledgersPerYear / input.bumpLedgers;
  const instanceRentPerCycle = Math.ceil(
    (instanceBytes / 1024) * (input.bumpLedgers / 1_000_000) * DEFAULTS.instanceRentStroopsPerKb1M
  );
  const persistentRentPerCycle = Math.ceil(
    (persistentBytes / 1024) * (input.bumpLedgers / 1_000_000) * DEFAULTS.persistentRentStroopsPerKb1M
  );

  const maintenanceTxsPerCycle = Math.max(1, Math.ceil((persistentEntries + 1) / DEFAULTS.keysPerMaintenanceTx));
  const txInclusionPerCycle = maintenanceTxsPerCycle * DEFAULTS.txBaseFeeStroops;
  const totalTtlCostPerCycleStroops = instanceRentPerCycle + persistentRentPerCycle + txInclusionPerCycle;
  const annualTtlCostStroops = Math.ceil(totalTtlCostPerCycleStroops * cyclesPerYear);

  const totalBytes = instanceBytes + persistentBytes;
  const fullRestoreWriteFeeStroops = Math.ceil((totalBytes / 1024) * DEFAULTS.writeFeeStroopsPerKb);
  const fullRestoreTotalStroops =
    fullRestoreWriteFeeStroops + instanceRentPerCycle + persistentRentPerCycle + txInclusionPerCycle;

  const savedClaimBytes = input.archivedClaims * (ENTRY_BYTES.pendingClaim + ENTRY_BYTES.claimSettledAt);
  const savedAnnualStroops = Math.ceil(
    (savedClaimBytes / 1024) * (DEFAULTS.ledgersPerYear / 1_000_000) * DEFAULTS.persistentRentStroopsPerKb1M
  );

  return {
    input,
    categories,
    totals: {
      instanceBytes,
      persistentBytes,
      totalBytes,
      persistentEntries,
      maintenanceTxsPerCycle,
      ttlCostPerCycleStroops: totalTtlCostPerCycleStroops,
      ttlCostPerCycleXlm: (totalTtlCostPerCycleStroops / 1e7).toFixed(7),
      annualTtlCostStroops,
      annualTtlCostXlm: (annualTtlCostStroops / 1e7).toFixed(7),
      fullRestoreStroops: fullRestoreTotalStroops,
      fullRestoreXlm: (fullRestoreTotalStroops / 1e7).toFixed(7),
      archivedClaimsSavedBytes: savedClaimBytes,
      archivedClaimsSavedAnnualStroops: savedAnnualStroops,
    },
  };
}

function formatReport(report) {
  const { input, categories, totals } = report;
  const lines = [];
  lines.push(`HyperionRouter Soroban Storage Cost Projection (${input.networkKey})`);
  if (input.router) lines.push(`Router: ${input.router}`);
  lines.push(`Bump window: ${input.bumpLedgers} ledgers (~${(input.bumpLedgers / 17280).toFixed(1)} days)`);
  lines.push("");
  lines.push("Category                              Count    Bytes/ea    Total Bytes");
  lines.push("------------------------------------  -------  ----------  -----------");
  for (const c of categories) {
    lines.push(
      `${c.name.padEnd(36)}  ${String(c.count).padStart(7)}  ${String(c.bytesEach).padStart(10)}  ${String(c.totalBytes).padStart(11)}`
    );
  }
  lines.push("------------------------------------  -------  ----------  -----------");
  lines.push(`Total footprint: ${totals.totalBytes} bytes (${totals.persistentEntries} persistent entries + 1 instance)`);
  lines.push(`Maintenance txs per cycle: ${totals.maintenanceTxsPerCycle}`);
  lines.push(`Projected TTL upkeep / cycle: ${totals.ttlCostPerCycleStroops} stroops (${totals.ttlCostPerCycleXlm} XLM)`);
  lines.push(`Projected annual TTL upkeep:  ${totals.annualTtlCostStroops} stroops (${totals.annualTtlCostXlm} XLM)`);
  lines.push(`Worst-case full restoration:  ${totals.fullRestoreStroops} stroops (${totals.fullRestoreXlm} XLM)`);
  lines.push(
    `Cleanup savings (${input.archivedClaims} archived claims): ${totals.archivedClaimsSavedBytes} bytes (${totals.archivedClaimsSavedAnnualStroops} stroops/yr)`
  );
  return lines.join("\n");
}

if (import.meta.url === `file://${process.argv[1]}`) {
  const opts = parseArgs(process.argv.slice(2));
  const input = loadInputs(opts);
  const report = projectStorageCosts(input);
  if (opts.json) {
    process.stdout.write(JSON.stringify(report, null, 2) + "\n");
  } else {
    process.stdout.write(formatReport(report) + "\n");
  }
}
