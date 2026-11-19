import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import { readFileSync } from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";
import test from "node:test";

import { planProposals, ProposalAbort, DELAY_REQUIREMENTS } from "../propose.mjs";

const HERE = path.dirname(fileURLToPath(import.meta.url));
const REPO_ROOT = path.resolve(HERE, "../../..");
const FIXTURES = path.join(HERE, "fixtures");
const CLI = path.join(REPO_ROOT, "script/stellar/propose.mjs");

function fixture(name) {
  const dir = path.join(FIXTURES, name);
  return {
    dir,
    manifest: JSON.parse(readFileSync(path.join(dir, "manifest.json"), "utf8")),
    // Guard-only fixtures carry just the live snapshot; the settled manifest is the desired one.
    live: JSON.parse(readFileSync(path.join(dir, "live.json"), "utf8")),
  };
}

function settled() {
  return fixture("settled");
}

function actions(plan) {
  return plan.proposals.map((proposal) => proposal.action);
}

test("a settled manifest produces an empty diff and submits nothing", () => {
  const { manifest, live } = settled();
  const plan = planProposals(manifest, live);

  assert.deepEqual(plan.differences, []);
  assert.deepEqual(plan.proposals, []);
  assert.deepEqual(plan.alreadyQueued, []);
  assert.equal(plan.signed, false);
  assert.equal(plan.submitted, false);
});

test("drift emits only actual differences with their delay requirements", () => {
  const { manifest, live } = fixture("drift");
  const plan = planProposals(manifest, live);

  assert.equal(plan.differences.length, plan.proposals.length);
  assert.deepEqual(actions(plan), [
    { SetAdmin: "GNEWADMINPLACEHOLDERXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXX" },
    { SetFeeBps: 35 },
    { SetFlowWindow: 900 },
    { RaiseTokenFlowLimit: ["CBIELTK6YBZJU5UP2WWQEUCYKLPU6AUNZ2BQ4WWFEIE3USCIHMXQDAMA", "20000000000000"] },
    { SetAdapter: [1, "GDESIREDNAMEPLACEHOLDERXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXX"] },
    { SetRailReceiver: [1, "GDESIREDNAMEPLACEHOLDERXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXX"] },
    { SetRouteConfig: [0, { max_single_transfer: "500000000" }] },
  ]);
  for (const proposal of plan.proposals) {
    assert.equal(proposal.delay.delaySeconds, "3600");
    assert.equal(proposal.delay.minDelaySeconds, DELAY_REQUIREMENTS.minDelaySeconds);
    assert.equal(proposal.delay.maxDelaySeconds, DELAY_REQUIREMENTS.maxDelaySeconds);
    assert.equal(proposal.delay.gracePeriodSeconds, DELAY_REQUIREMENTS.gracePeriodSeconds);
    assert.deepEqual(proposal.submission, { signed: false, submitted: false });
  }
  assert.deepEqual(plan.proposals.map((proposal) => proposal.sequence), [1, 2, 3, 4, 5, 6, 7]);
});

test("large i128 amounts survive as exact decimal strings", () => {
  const { manifest, live } = settled();
  manifest.networks["stellar-testnet"].tokens.USDC.flowLimit = "123456789012345678901234";
  const plan = planProposals(manifest, live);

  assert.deepEqual(actions(plan), [
    {
      RaiseTokenFlowLimit: [
        "CBIELTK6YBZJU5UP2WWQEUCYKLPU6AUNZ2BQ4WWFEIE3USCIHMXQDAMA",
        "123456789012345678901234",
      ],
    },
  ]);
});

test("an already queued matching action is not emitted again", () => {
  const { manifest, live } = settled();
  manifest.networks["stellar-testnet"].feeBps = 31;
  live.queue = [
    { id: 7, action: { SetFeeBps: 31 } },
    { id: 8, action: { SetFeeBps: 30 } },
  ];
  const plan = planProposals(manifest, live);

  assert.deepEqual(plan.proposals, []);
  assert.equal(plan.differences.length, 1);
  assert.equal(plan.alreadyQueued.length, 1);
  assert.equal(plan.alreadyQueued[0].id, 7);
});

test("tightening a limit becomes a guardian action, not a timelocked proposal", () => {
  const { manifest, live } = settled();
  manifest.networks["stellar-testnet"].tokens.USDC.flowLimit = "9000000000";
  const plan = planProposals(manifest, live);

  assert.deepEqual(plan.proposals, []);
  assert.equal(plan.guardianActions.length, 1);
  assert.deepEqual(plan.guardianActions[0].action, {
    lower_token_flow_limit: ["CBIELTK6YBZJU5UP2WWQEUCYKLPU6AUNZ2BQ4WWFEIE3USCIHMXQDAMA", "9000000000"],
  });
});

test("wrong live network aborts before any diff is produced", () => {
  const { manifest } = settled();
  const live = JSON.parse(readFileSync(path.join(FIXTURES, "wrong-network/live.json"), "utf8"));
  assert.throws(() => planProposals(manifest, live), (error) => error instanceof ProposalAbort && /wrong network/.test(error.message));
});

test("a passphrase mismatch between manifest and chain aborts", () => {
  const { manifest, live } = settled();
  manifest.networks["stellar-testnet"].networkPassphrase = "Test SDF Future Network ; October 2022";
  assert.throws(() => planProposals(manifest, live), /wrong network/);
});

test("unexpected deployed code hash aborts", () => {
  const { manifest } = settled();
  const live = JSON.parse(readFileSync(path.join(FIXTURES, "wrong-hash/live.json"), "utf8"));
  assert.throws(() => planProposals(manifest, live), /unexpected code hash/);
});

test("a changed adapter code hash aborts too", () => {
  const { manifest, live } = settled();
  live.adapterCodeHashes.cctp = "00".repeat(32);
  assert.throws(() => planProposals(manifest, live), /unexpected code hash.*cctp/);
});

test("changing decimals aborts instead of overwriting them via re-registration", () => {
  const { manifest, live } = settled();
  manifest.networks["stellar-testnet"].tokens.USDC.decimals = 6;
  assert.throws(() => planProposals(manifest, live), /decimals changed/);
});

test("EVM and other families stay gated", () => {
  const { manifest, live } = settled();
  manifest.networks["stellar-testnet"].family = "evm";
  assert.throws(() => planProposals(manifest, live), /gated/);
});

test("an out-of-range desired delay aborts", () => {
  const { manifest, live } = settled();
  manifest.networks["stellar-testnet"].timelockDelay = 60;
  assert.throws(() => planProposals(manifest, live), /outside the router's accepted range/);
});

test("an out-of-range fee aborts instead of emitting an action the router would refuse", () => {
  const { manifest, live } = settled();
  manifest.networks["stellar-testnet"].feeBps = 101;
  assert.throws(() => planProposals(manifest, live), /feeBps 101 is outside/);
});

test("the CLI produces JSON from a fixture without touching Stellar", () => {
  const { dir, manifest } = fixture("settled");
  const result = spawnSync(process.execPath, [CLI, path.join(dir, "manifest.json"), "--fixture", dir], {
    encoding: "utf8",
    env: { ...process.env, PATH: "/nonexistent" },
  });
  assert.equal(result.status, 0, result.stderr);
  assert.deepEqual(JSON.parse(result.stdout).proposals, []);
  assert.match(result.stderr, /no configuration differences/);
});

test("the CLI exits non-zero and prints no proposal JSON when a guard fails", () => {
  const { dir } = settled();
  const guardDir = path.join(FIXTURES, "wrong-hash");
  const result = spawnSync(process.execPath, [CLI, path.join(dir, "manifest.json"), "--fixture", guardDir], {
    encoding: "utf8",
  });
  assert.equal(result.status, 1);
  assert.equal(result.stdout, "");
  assert.match(result.stderr, /unexpected code hash/);
});
