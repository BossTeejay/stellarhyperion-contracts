#!/usr/bin/env node
//
// Compare a desired deployment manifest with the live Soroban router and emit unsigned action
// proposals. This file never signs, sends, or submits anything: every read is a simulated view
// call (or a local fixture), and the output is a JSON description of what a reviewer would still
// have to queue and execute through the normal timelock.
//
// The manifest is a deployment record of the same shape as deployments/stellar-testnet-phase1.json.
// Integer amounts are carried as decimal strings end to end so a 14-digit USDC flow limit cannot be
// quietly rounded by a floating point parse on the way to a proposal.

import { execFileSync } from "node:child_process";
import { existsSync, readFileSync, writeFileSync } from "node:fs";
import path from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";

const HERE = path.dirname(fileURLToPath(import.meta.url));
const REPO_ROOT = path.resolve(HERE, "../..");
const DEFAULT_MANIFEST = path.join(
  REPO_ROOT,
  "deployments/stellar-testnet-phase1.json",
);

// Keep in step with script/stellar/lib.sh, HyperionTypes.sol RouteKind, and the protocol parity test.
export const ROUTE_IDS = Object.freeze({
  cctp: 0,
  "axelar-its": 1,
  "axelar-gmp": 2,
  allbridge: 3,
});

// From soroban/crates/hyperion-router/src/timelock.rs. The deployed router is unchanged by this
// tool, so these are reported as requirements rather than read from a newer contract view.
export const DELAY_REQUIREMENTS = Object.freeze({
  minDelaySeconds: "3600",
  maxDelaySeconds: "2592000",
  gracePeriodSeconds: "604800",
});

export const SUPPORTED_NETWORK_KEY = "stellar-testnet";
export const SUPPORTED_PASSPHRASE = "Test SDF Network ; September 2015";

export class ProposalAbort extends Error {
  constructor(message) {
    super(message);
    this.name = "ProposalAbort";
  }
}

function abort(message) {
  throw new ProposalAbort(message);
}

function readJson(file) {
  if (!existsSync(file)) abort(`no file at ${file}`);
  try {
    return JSON.parse(readFileSync(file, "utf8"));
  } catch (error) {
    abort(`cannot parse ${file}: ${error.message}`);
  }
}

/** Integer comparison without ever going through Number for i128/u64 amounts. */
export function exactInt(value, field) {
  if (value === undefined || value === null || String(value).trim() === "") {
    abort(`${field} is missing; refusing to guess an amount`);
  }
  if (typeof value === "number" && !Number.isSafeInteger(value))
    abort(`${field} must be a safe integer or exact decimal string`);
  const text = String(value).trim();
  if (!/^-?\d+$/.test(text)) abort(`${field} is not an exact integer: ${text}`);
  return BigInt(text);
}

const MAX_U32 = (1n << 32n) - 1n;
const MAX_I128 = (1n << 127n) - 1n;
function boundedInt(value, field, maximum, minimum = 0n) {
  const integer = exactInt(value, field);
  if (integer < minimum || integer > maximum)
    abort(`${field} is outside its contract integer range`);
  return integer;
}
function adapterHash(desired, slug) {
  return (
    desired.wasmHashes?.[`adapter-${slug}`] ??
    (slug === "axelar-its" ? desired.wasmHashes?.["adapter-axelar"] : undefined)
  );
}

function canonical(value) {
  if (Array.isArray(value)) return value.map(canonical);
  if (value && typeof value === "object") {
    return Object.fromEntries(
      Object.keys(value)
        .sort()
        .map((key) => [key, canonical(value[key])]),
    );
  }
  return typeof value === "number" ? String(value) : value;
}

const same = (left, right) =>
  JSON.stringify(canonical(left)) === JSON.stringify(canonical(right));
const address = (value) => String(value ?? "").trim();

/** Pure planner. `live` is either read from the CLI or loaded from a local fixture. */
export function planProposals(manifest, live, options = {}) {
  const networkKey = options.networkKey ?? SUPPORTED_NETWORK_KEY;
  const networks = manifest?.networks;
  if (!networks || typeof networks !== "object")
    abort("the manifest has no networks object");

  if (networkKey !== SUPPORTED_NETWORK_KEY)
    abort("only Stellar Testnet proposal generation is validated");
  const desired = networks[networkKey];
  if (!desired) abort(`the manifest has no ${networkKey} network`);
  if (desired.family !== "stellar") {
    abort(
      `family ${String(desired.family)} is not supported by this command. EVM and other rail ` +
        "proposal generation stays gated until that side is independently validated.",
    );
  }

  // Guard one: the manifest and the RPC must agree on which network is in front of us.
  const livePassphrase = String(live?.networkPassphrase ?? "").trim();
  const manifestPassphrase = String(desired.networkPassphrase ?? "").trim();
  if (livePassphrase !== SUPPORTED_PASSPHRASE) {
    abort(
      `wrong network: the live passphrase is ${livePassphrase || "unknown"}, expected ${SUPPORTED_PASSPHRASE}`,
    );
  }
  if (manifestPassphrase !== livePassphrase) {
    abort(
      `wrong network: the manifest names ${manifestPassphrase || "no passphrase"}, the chain has ${livePassphrase}`,
    );
  }

  // Guard two: a config diff is only meaningful for the code whose config it is.
  const expectedHash = String(desired.wasmHashes?.router ?? "")
    .trim()
    .toLowerCase();
  const actualHash = String(live?.routerCodeHash ?? "")
    .trim()
    .toLowerCase();
  if (!/^[0-9a-f]{64}$/.test(expectedHash))
    abort(
      "the manifest has no valid wasmHashes.router; cannot verify the deployed code",
    );
  if (expectedHash !== actualHash) {
    abort(
      `unexpected code hash: router is ${actualHash || "unknown"}, manifest records ${expectedHash}`,
    );
  }
  for (const slug of Object.keys(desired.adapters ?? {})) {
    if (slug !== "cctp" && slug !== "axelar-its")
      abort(
        `route ${slug} proposal generation stays gated until independently validated`,
      );
    const expected = String(adapterHash(desired, slug) ?? "")
      .trim()
      .toLowerCase();
    const actual = String(live?.adapterCodeHashes?.[slug] ?? "")
      .trim()
      .toLowerCase();
    if (!/^[0-9a-f]{64}$/.test(expected) || expected !== actual) {
      abort(
        `unexpected code hash: ${slug} adapter is ${actual || "unknown"}, manifest records ${expected || "no hash"}`,
      );
    }
  }

  const delaySeconds = exactInt(desired.timelockDelay, "timelockDelay");
  if (
    delaySeconds < BigInt(DELAY_REQUIREMENTS.minDelaySeconds) ||
    delaySeconds > BigInt(DELAY_REQUIREMENTS.maxDelaySeconds)
  ) {
    abort(
      `timelockDelay ${delaySeconds} is outside the router's accepted range`,
    );
  }
  const currentDelay = boundedInt(
    live?.config?.timelock_delay,
    "live timelockDelay",
    BigInt(DELAY_REQUIREMENTS.maxDelaySeconds),
    BigInt(DELAY_REQUIREMENTS.minDelaySeconds),
  );
  const requirements = {
    delaySeconds: currentDelay.toString(),
    ...DELAY_REQUIREMENTS,
  };
  const now = boundedInt(
    live?.ledgerTimestamp,
    "ledgerTimestamp",
    (1n << 64n) - 1n,
  );

  // Never hand a reviewer an action the router would refuse at queue time.
  if (desired.feeBps !== undefined) {
    const fee = exactInt(desired.feeBps, "feeBps");
    if (fee < 0n || fee > 100n)
      abort(`feeBps ${fee} is outside the router's accepted range`);
  }
  if (
    desired.flowWindow !== undefined &&
    boundedInt(desired.flowWindow, "flowWindow", MAX_U32) <= 0n
  ) {
    abort("flowWindow must be greater than zero");
  }

  const differences = [];
  const proposals = [];
  const alreadyQueued = [];
  const guardianActions = [];
  const unsupported = [];

  const record = (field, liveValue, desiredValue, authority) => {
    differences.push({
      field,
      live:
        liveValue === undefined || liveValue === null
          ? null
          : String(liveValue),
      desired:
        desiredValue === undefined || desiredValue === null
          ? null
          : String(desiredValue),
      authority,
    });
  };

  const propose = (
    field,
    liveValue,
    desiredValue,
    action,
    description,
    authority = "admin/timelocked",
  ) => {
    record(field, liveValue, desiredValue, authority);
    proposals.push({
      sequence: proposals.length + 1,
      field,
      description,
      action,
      authority,
      delay: { ...requirements },
      submission: { signed: false, submitted: false },
    });
  };

  const guardian = (field, liveValue, desiredValue, action, description) => {
    record(field, liveValue, desiredValue, "guardian/immediate");
    guardianActions.push({
      field,
      description,
      action,
      note: "Narrows risk; no timelock and no signature from this tool.",
    });
  };

  // The config fields the router admin changes through queue_action/execute_action.
  const configFields = [
    ["admin", "admin", (v) => ({ SetAdmin: address(v) })],
    ["guardian", "guardian", (v) => ({ SetGuardian: address(v) })],
    ["treasury", "treasury", (v) => ({ SetTreasury: address(v) })],
    [
      "feeBps",
      "fee_bps",
      (v) => ({ SetFeeBps: Number(exactInt(v, "feeBps")) }),
    ],
    [
      "flowWindow",
      "flow_window_ledgers",
      (v) => ({
        SetFlowWindow: Number(boundedInt(v, "flowWindow", MAX_U32, 1n)),
      }),
    ],
    [
      "timelockDelay",
      "timelock_delay",
      (v) => ({ SetTimelockDelay: exactInt(v, "timelockDelay").toString() }),
    ],
  ];
  for (const [field, liveField, makeAction] of configFields) {
    const want = desired[field];
    if (want === undefined) continue;
    const got = live?.config?.[liveField];
    if (
      field === "feeBps" ||
      field === "flowWindow" ||
      field === "timelockDelay"
    ) {
      if (exactInt(want, field) === exactInt(got, field)) continue;
      propose(field, got, want, makeAction(want), `set ${field} to ${want}`);
    } else if (address(got) !== address(want)) {
      propose(field, got, want, makeAction(want), `set ${field} to ${want}`);
    }
  }

  // Tokens. Decimals are immutable on the deployed router: a second RegisterToken overwrites them,
  // so a changed decimal count is a migration decision, never an automatically generated action.
  for (const [symbol, token] of Object.entries(desired.tokens ?? {})) {
    const sac = address(token.sacId);
    if (!sac) abort(`token ${symbol} has no sacId`);
    const wantDecimals = Number(
      boundedInt(token.decimals, `${symbol}.decimals`, 38n),
    );
    const wantLimit = boundedInt(
      token.flowLimit,
      `${symbol}.flowLimit`,
      MAX_I128,
    ).toString();
    const got = live?.tokens?.[sac];

    if (!got) {
      propose(
        `tokens.${symbol}`,
        null,
        wantLimit,
        { RegisterToken: [sac, wantDecimals, wantLimit] },
        `register ${symbol}`,
      );
      continue;
    }

    const gotDecimals = Number(
      boundedInt(got.decimals, `${symbol} live decimals`, 38n),
    );
    if (gotDecimals !== wantDecimals) {
      abort(
        `token ${symbol} decimals changed from ${gotDecimals} to ${wantDecimals}. Re-registration ` +
          "would overwrite them; migrate deliberately instead of generating a proposal.",
      );
    }
    if (got.enabled === false) {
      propose(
        `tokens.${symbol}.enabled`,
        "false",
        "true",
        { RegisterToken: [sac, wantDecimals, wantLimit] },
        `re-register ${symbol}`,
      );
      continue;
    }
    const gotLimit = boundedInt(
      got.flow_limit,
      `${symbol} live flow_limit`,
      MAX_I128,
    ).toString();
    if (gotLimit === wantLimit) continue;
    if (BigInt(wantLimit) > BigInt(gotLimit)) {
      propose(
        `tokens.${symbol}.flowLimit`,
        gotLimit,
        wantLimit,
        { RaiseTokenFlowLimit: [sac, wantLimit] },
        `raise ${symbol} flow limit`,
      );
    } else {
      guardian(
        `tokens.${symbol}.flowLimit`,
        gotLimit,
        wantLimit,
        { lower_token_flow_limit: [sac, wantLimit] },
        `lower ${symbol} flow limit`,
      );
    }
  }

  // Rails. Router-side wiring is timelocked; the adapter's own admin/link setters are a separate
  // authority and are deliberately not represented as router AdminActions.
  const enabledRoutes = new Set(desired.enabledRoutes ?? []);
  for (const token of Object.values(desired.tokens ?? {})) {
    for (const slug of token.routes ?? []) enabledRoutes.add(slug);
  }
  for (const slug of enabledRoutes) {
    if (ROUTE_IDS[slug] === undefined || !desired.adapters?.[slug])
      abort(`enabled route ${slug} has no desired adapter`);
  }
  const receivers = desired.railReceivers ?? {};
  const routeConfigs = desired.routeConfigs ?? {};

  for (const [slug, addr] of Object.entries(desired.adapters ?? {})) {
    const route = ROUTE_IDS[slug];
    if (route === undefined) abort(`unknown route slug ${slug}`);
    const got = live?.routes?.[route] ?? {};
    const wantReceiver = address(receivers[slug] ?? addr);

    if (address(got.adapter) !== address(addr)) {
      propose(
        `routes.${slug}.adapter`,
        got.adapter ?? null,
        addr,
        { SetAdapter: [route, address(addr)] },
        `set ${slug} adapter`,
      );
    }
    if (address(got.receiver) !== wantReceiver) {
      propose(
        `routes.${slug}.receiver`,
        got.receiver ?? null,
        wantReceiver,
        { SetRailReceiver: [route, wantReceiver] },
        `set ${slug} rail receiver`,
      );
    }
    const wantEnabled = enabledRoutes.has(slug);
    if (wantEnabled && got.enabled !== true) {
      propose(
        `routes.${slug}.enabled`,
        String(got.enabled),
        "true",
        { EnableRoute: route },
        `enable ${slug}`,
      );
    } else if (!wantEnabled && got.enabled === true) {
      guardian(
        `routes.${slug}.enabled`,
        "true",
        "false",
        { disable_route: route },
        `disable ${slug}`,
      );
    }
  }

  for (const [slug, routeConfig] of Object.entries(routeConfigs)) {
    const route = ROUTE_IDS[slug];
    if (route === undefined) abort(`unknown route slug ${slug}`);
    const wantMax = boundedInt(
      routeConfig.maxSingleTransfer,
      `routes.${slug}.maxSingleTransfer`,
      MAX_I128,
    ).toString();
    const gotMax = live?.routes?.[route]?.config?.max_single_transfer;
    if (
      gotMax === undefined ||
      exactInt(gotMax, `routes.${slug} live maxSingleTransfer`).toString() !==
        wantMax
    ) {
      propose(
        `routes.${slug}.maxSingleTransfer`,
        gotMax ?? null,
        wantMax,
        { SetRouteConfig: [route, { max_single_transfer: wantMax }] },
        `set ${slug} max single transfer`,
      );
    }
  }

  // A matching action already waiting on the clock is not a new difference to sign.
  const pending = (live?.queue ?? []).filter((entry) => {
    if (!entry || entry.action === undefined) abort("malformed pending action");
    return (
      boundedInt(entry.expires_at, "pending expires_at", (1n << 64n) - 1n) >=
      now
    );
  });
  const remaining = [];
  for (const proposal of proposals) {
    const hit = pending.find((entry) => same(entry.action, proposal.action));
    if (hit) {
      alreadyQueued.push({
        id: hit.id,
        description: proposal.description,
        action: proposal.action,
        delay: { ...requirements },
      });
    } else {
      remaining.push(proposal);
    }
  }
  for (const proposal of remaining)
    proposal.sequence = remaining.indexOf(proposal) + 1;

  return {
    schemaVersion: 1,
    network: networkKey,
    networkPassphrase: livePassphrase,
    router: address(desired.router),
    routerCodeHash: actualHash,
    delay: requirements,
    differences,
    proposals: remaining,
    alreadyQueued,
    guardianActions,
    unsupported,
    signed: false,
    submitted: false,
  };
}

function parseCliValue(raw) {
  const text = String(raw ?? "").trim();
  if (!text) return null;
  try {
    return JSON.parse(text);
  } catch {
    if (/^"[\s\S]*"$/.test(text)) {
      try {
        return JSON.parse(text);
      } catch {}
    }
    if (text === "true") return true;
    if (text === "false") return false;
    return text;
  }
}

function cliJson(args, execute = execFileSync) {
  return parseCliValue(
    execute("stellar", args, {
      encoding: "utf8",
      stdio: ["ignore", "pipe", "pipe"],
      timeout: 30_000,
      maxBuffer: 4 * 1024 * 1024,
    }),
  );
}

export function readLiveFromCli(
  network,
  router,
  manifest,
  execute = execFileSync,
) {
  const read = (args, absentCode) => {
    try {
      return cliJson(args, execute);
    } catch (error) {
      const diagnostic = String(error.stderr ?? "");
      if (
        absentCode &&
        new RegExp(`Error\\(Contract, #${absentCode}\\)`).test(diagnostic)
      )
        return null;
      abort(
        `read failed for ${args.includes("--") ? args[args.indexOf("--") + 1] : args.slice(0, 3).join(" ")}; no proposal generated`,
      );
    }
  };
  const identity = process.env.STELLAR_IDENTITY ?? "hyperion-deploy";
  const invoke = (fn, args = []) =>
    read([
      "contract",
      "invoke",
      "--id",
      router,
      "--source-account",
      identity,
      "--network",
      network,
      "--send=no",
      "--",
      fn,
      ...args,
    ]);

  const networkInfo = read([
    "network",
    "info",
    "--network",
    network,
    "--output",
    "json",
  ]);
  const networkPassphrase =
    networkInfo?.passphrase ?? networkInfo?.result?.passphrase;
  if (
    networkPassphrase !== SUPPORTED_PASSPHRASE ||
    manifest?.networks?.[SUPPORTED_NETWORK_KEY]?.networkPassphrase !==
      networkPassphrase
  )
    abort("wrong network; no contract read attempted");
  const routerCodeHash = String(
    read([
      "contract",
      "info",
      "hash",
      "--contract-id",
      router,
      "--network",
      network,
    ]) ?? "",
  ).trim();
  const latest = read([
    "ledger",
    "latest",
    "--network",
    network,
    "--output",
    "json",
  ]);
  const sequence = boundedInt(
    latest?.sequence,
    "latest ledger sequence",
    MAX_U32,
    1n,
  ).toString();
  const fetched = read([
    "ledger",
    "fetch",
    sequence,
    "--network",
    network,
    "--output",
    "json",
    "--xdr-format",
    "xdr",
  ]);
  const ledgerTimestamp = fetched?.latestLedgerCloseTime;
  const config = invoke("get_config");
  const tokens = {};
  for (const token of Object.values(
    manifest?.networks?.[SUPPORTED_NETWORK_KEY]?.tokens ?? {},
  )) {
    const sac = address(token?.sacId);
    if (sac)
      tokens[sac] = read(
        [...invokeArgs(router, identity, network), "get_token", "--token", sac],
        31,
      );
  }

  const routes = {};
  for (const route of Object.values(ROUTE_IDS)) {
    routes[route] = {
      adapter: read(
        [
          ...invokeArgs(router, identity, network),
          "get_adapter",
          "--route",
          String(route),
        ],
        7,
      ),
      receiver: read(
        [
          ...invokeArgs(router, identity, network),
          "get_rail_receiver",
          "--route",
          String(route),
        ],
        7,
      ),
      enabled: read([
        ...invokeArgs(router, identity, network),
        "is_route_enabled",
        "--route",
        String(route),
      ]),
      config: read([
        ...invokeArgs(router, identity, network),
        "get_route_config",
        "--route",
        String(route),
      ]),
    };
  }

  const adapterCodeHashes = {};
  for (const [slug, contract] of Object.entries(
    manifest?.networks?.[SUPPORTED_NETWORK_KEY]?.adapters ?? {},
  )) {
    adapterCodeHashes[slug] = String(
      read([
        "contract",
        "info",
        "hash",
        "--contract-id",
        contract,
        "--network",
        network,
      ]) ?? "",
    ).trim();
  }

  const count = Number(
    boundedInt(invoke("queue_count"), "queue_count", 10_000n),
  );
  const queue = [];
  for (let id = 1; id <= count; id += 1) {
    const entry = read(
      [
        ...invokeArgs(router, identity, network),
        "get_queued",
        "--id",
        String(id),
      ],
      22,
    );
    if (entry) queue.push(entry);
  }

  return {
    networkPassphrase,
    routerCodeHash,
    adapterCodeHashes,
    config,
    tokens,
    routes,
    queue,
    ledgerTimestamp,
  };
}

function invokeArgs(router, identity, network) {
  return [
    "contract",
    "invoke",
    "--id",
    router,
    "--source-account",
    identity,
    "--network",
    network,
    "--send=no",
    "--",
  ];
}

export function parseArgs(argv) {
  const result = {
    manifest: DEFAULT_MANIFEST,
    network: "testnet",
    fixture: null,
    out: null,
  };
  for (let i = 0; i < argv.length; i += 1) {
    const arg = argv[i];
    if (arg === "--fixture") result.fixture = argv[++i];
    else if (arg.startsWith("--fixture="))
      result.fixture = arg.slice("--fixture=".length);
    else if (arg === "--out") result.out = argv[++i];
    else if (arg.startsWith("--out=")) result.out = arg.slice("--out=".length);
    else if (arg === "--network") result.network = argv[++i];
    else if (arg.startsWith("--network="))
      result.network = arg.slice("--network=".length);
    else if (arg === "--help" || arg === "-h") result.help = true;
    else if (!arg.startsWith("-") && result.manifest === DEFAULT_MANIFEST)
      result.manifest = arg;
    else abort(`unknown argument ${arg}`);
  }
  return result;
}

export function main(argv = process.argv.slice(2)) {
  const args = parseArgs(argv);
  if (args.help) {
    process.stdout.write(
      "Usage: script/stellar/propose.sh [manifest.json] [--network testnet] [--fixture dir] [--out file.json]\n" +
        "Read-only: compares the manifest with live router state and prints unsigned proposals as JSON.\n",
    );
    return 0;
  }

  const manifest = readJson(path.resolve(args.manifest));
  const router = manifest?.networks?.[SUPPORTED_NETWORK_KEY]?.router;
  if (!router) abort(`the manifest has no ${SUPPORTED_NETWORK_KEY}.router`);
  const live = args.fixture
    ? readJson(path.resolve(args.fixture, "live.json"))
    : readLiveFromCli(args.network, router, manifest);

  const plan = planProposals(manifest, live);
  const json = `${JSON.stringify(plan, null, 2)}\n`;
  if (args.out) writeFileSync(path.resolve(args.out), json);
  else process.stdout.write(json);

  const note = plan.differences.length
    ? `${plan.proposals.length} unsigned proposal(s), ${plan.differences.length} difference(s)`
    : "no configuration differences; nothing to queue or sign";
  process.stderr.write(`${note}\n`);
  if (plan.alreadyQueued.length)
    process.stderr.write(
      `${plan.alreadyQueued.length} difference(s) already queued on the router\n`,
    );
  if (plan.guardianActions.length)
    process.stderr.write(
      `${plan.guardianActions.length} difference(s) need the guardian's immediate action, not a timelocked proposal\n`,
    );
  return 0;
}

if (import.meta.url === pathToFileURL(process.argv[1] ?? "").href) {
  try {
    process.exitCode = main();
  } catch (error) {
    if (error instanceof ProposalAbort) {
      process.stderr.write(
        `refusing to generate proposals: ${error.message}\n`,
      );
      process.exitCode = 1;
    } else {
      throw error;
    }
  }
}
