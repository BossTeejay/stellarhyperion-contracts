#!/usr/bin/env node
/**
 * Axelar source-note and destination-payment reconciliation CLI.
 *
 * Joins encoded note intent with candidate ITS payment events for development evidence.
 * Verifies exact agreement on recipient, token ID, amount, and full command identity.
 * Partial or mismatched receipts are reported unverified.
 * Reports retain original raw references without relying on a display nonce.
 *
 * Active deployment is Stellar Testnet outbound ITS/CCTP; EVM and other rail development
 * remain explicitly gated until independently validated.
 */

import { readFileSync } from "node:fs";
import { resolve, join } from "node:path";
import { pathToFileURL } from "node:url";

// Self-contained note parsing and reconciliation helpers
const NOTE_VERSION = 1;
const OUTBOUND_NOTE_LEN = 29;
const INBOUND_NOTE_MIN = 1 + 3 + 8;

export function fromHex(hex) {
  const clean = hex.startsWith("0x") ? hex.slice(2) : hex;
  if (clean.length % 2 !== 0) throw new Error("Invalid hex length");
  const bytes = new Uint8Array(clean.length / 2);
  for (let i = 0; i < bytes.length; i++) {
    bytes[i] = parseInt(clean.slice(i * 2, i * 2 + 2), 16);
  }
  return bytes;
}

export function toHex(bytes) {
  let hex = "0x";
  for (let i = 0; i < bytes.length; i++) {
    hex += bytes[i].toString(16).padStart(2, "0");
  }
  return hex;
}

export function bytesToBigInt(bytes) {
  let val = 0n;
  for (let i = 0; i < bytes.length; i++) {
    val = (val << 8n) | BigInt(bytes[i]);
  }
  return val;
}

export function bytesToAscii(bytes) {
  let str = "";
  for (let i = 0; i < bytes.length; i++) {
    str += String.fromCharCode(bytes[i]);
  }
  return str;
}

export function decodeOutboundNote(payload) {
  const bytes = typeof payload === "string" ? fromHex(payload) : payload;
  if (bytes.length !== OUTBOUND_NOTE_LEN) {
    throw new Error(`Outbound note must be exactly ${OUTBOUND_NOTE_LEN} bytes, got ${bytes.length}`);
  }
  if (bytes[0] !== NOTE_VERSION) {
    throw new Error(`Unsupported note version: ${bytes[0]}, expected ${NOTE_VERSION}`);
  }
  const recipient = toHex(bytes.slice(1, 21));
  if (bytesToBigInt(bytes.slice(1, 21)) === 0n) {
    throw new Error("ZeroAddressKey: recipient cannot be zero address");
  }
  const nonce = bytesToBigInt(bytes.slice(21, 29));
  return { recipient, nonce };
}

export function decodeInboundNote(payload) {
  const bytes = typeof payload === "string" ? fromHex(payload) : payload;
  if (bytes.length < INBOUND_NOTE_MIN) {
    throw new Error(`Inbound note is too short: ${bytes.length} bytes`);
  }
  if (bytes[0] !== NOTE_VERSION) {
    throw new Error(`Unsupported note version: ${bytes[0]}, expected ${NOTE_VERSION}`);
  }
  const kind = bytes[1];
  const len = bytes[2];
  if (bytes.length !== 3 + len + 8) {
    throw new Error(`Length byte (${len}) does not match note length (${bytes.length})`);
  }
  const strkey = bytesToAscii(bytes.slice(3, 3 + len));
  const nonce = bytesToBigInt(bytes.slice(3 + len, 3 + len + 8));
  return { kind, strkey, nonce };
}

export function normalizeHex32(value) {
  if (!value) return "";
  const cleaned = String(value).startsWith("0x") ? String(value).slice(2) : String(value);
  return cleaned.toLowerCase();
}

export function parseIntegerAmount(value) {
  if (value === undefined || value === null) return null;
  try {
    if (typeof value === "bigint") return value;
    if (typeof value === "number") {
      if (!Number.isInteger(value)) return null;
      return BigInt(value);
    }
    const str = String(value).trim();
    if (!/^-?\d+$/.test(str)) return null;
    return BigInt(str);
  } catch {
    return null;
  }
}

/**
 * Reconcile Axelar source note intent against candidate ITS payment evidence.
 *
 * @param {object} source Source note intent
 * @param {object|null} candidate Candidate ITS payment event
 * @returns {object} Reconciliation report
 */
export function reconcileAxelarPayment(source, candidate) {
  const discrepancies = [];
  const agreements = [];

  let direction = source?.direction ?? "outbound";
  let expectedRecipient = "";
  let displayNonce = source?.nonce !== undefined ? String(source.nonce) : undefined;

  if (source?.note) {
    try {
      const noteBytes = typeof source.note === "string" ? fromHex(source.note) : source.note;
      if (source.direction === "inbound") {
        const decoded = decodeInboundNote(noteBytes);
        expectedRecipient = decoded.strkey;
        displayNonce = decoded.nonce.toString();
        direction = "inbound";
      } else if (source.direction === "outbound") {
        const decoded = decodeOutboundNote(noteBytes);
        expectedRecipient = decoded.recipient.toLowerCase();
        displayNonce = decoded.nonce.toString();
        direction = "outbound";
      } else {
        if (noteBytes.length === OUTBOUND_NOTE_LEN) {
          const decoded = decodeOutboundNote(noteBytes);
          expectedRecipient = decoded.recipient.toLowerCase();
          displayNonce = decoded.nonce.toString();
          direction = "outbound";
        } else {
          const decoded = decodeInboundNote(noteBytes);
          expectedRecipient = decoded.strkey;
          displayNonce = decoded.nonce.toString();
          direction = "inbound";
        }
      }
    } catch (err) {
      discrepancies.push(`malformed note payload: ${err.message}`);
    }
  }

  if (!expectedRecipient && source?.recipient) {
    expectedRecipient =
      direction === "outbound"
        ? String(source.recipient).toLowerCase()
        : String(source.recipient).trim();
  }

  if (!expectedRecipient && discrepancies.length === 0) {
    discrepancies.push("missing expected recipient in source intent or note");
  }

  const expectedToken = normalizeHex32(source?.tokenId);
  const expectedAmount = parseIntegerAmount(source?.amount);
  const expectedCommandId = source?.commandId ? normalizeHex32(source.commandId) : undefined;

  if (expectedAmount === null || expectedAmount <= 0n) {
    discrepancies.push(`invalid or non-positive source amount: ${String(source?.amount)}`);
  }

  let candidateRecipient;
  let candidateToken;
  let candidateAmount = null;
  let candidateCommandId;

  if (!candidate) {
    discrepancies.push("missing candidate payment");
  } else {
    // 1. Recipient agreement
    if (!candidate.recipient) {
      discrepancies.push("missing recipient on candidate payment");
    } else {
      candidateRecipient =
        direction === "outbound"
          ? String(candidate.recipient).toLowerCase()
          : String(candidate.recipient).trim();
      if (expectedRecipient && candidateRecipient !== expectedRecipient) {
        discrepancies.push(
          `recipient mismatch: expected ${expectedRecipient} but candidate received ${candidateRecipient}`,
        );
      } else if (expectedRecipient) {
        agreements.push("recipient");
      }
    }

    // 2. Token ID agreement
    if (!candidate.tokenId) {
      discrepancies.push("missing token ID on candidate payment");
    } else {
      candidateToken = normalizeHex32(candidate.tokenId);
      if (expectedToken && candidateToken !== expectedToken) {
        discrepancies.push(
          `token ID mismatch: expected ${expectedToken} but candidate received ${candidateToken}`,
        );
      } else if (expectedToken) {
        agreements.push("tokenId");
      }
    }

    // 3. Exact integer amount agreement
    candidateAmount = parseIntegerAmount(candidate.amount);
    if (candidateAmount === null) {
      discrepancies.push(`missing or invalid candidate amount: ${String(candidate.amount)}`);
    } else if (expectedAmount !== null) {
      if (candidateAmount < expectedAmount) {
        discrepancies.push(
          `partial receipt detected: expected amount ${expectedAmount.toString()} but candidate received ${candidateAmount.toString()}`,
        );
      } else if (candidateAmount > expectedAmount) {
        discrepancies.push(
          `amount mismatch: expected ${expectedAmount.toString()} but candidate received ${candidateAmount.toString()}`,
        );
      } else {
        agreements.push("amount");
      }
    }

    // 4. Full command identity agreement
    if (!candidate.commandId || String(candidate.commandId).trim() === "") {
      discrepancies.push(
        "missing candidate command ID (command identity must be non-empty and retained)",
      );
    } else {
      candidateCommandId = normalizeHex32(candidate.commandId);
      if (expectedCommandId && candidateCommandId !== expectedCommandId) {
        discrepancies.push(
          `command ID mismatch: expected ${expectedCommandId} but candidate received ${candidateCommandId}`,
        );
      } else {
        agreements.push("commandId");
      }
    }
  }

  // Preserve raw references without relying on a display nonce
  const rawReferences = {
    ...(source?.rawReferences ?? {}),
    ...(candidate?.rawReferences ?? {}),
  };
  if (source?.commandId && !rawReferences.sourceCommandId) {
    rawReferences.sourceCommandId = source.commandId;
  }
  if (candidate?.commandId && !rawReferences.candidateCommandId) {
    rawReferences.candidateCommandId = candidate.commandId;
  }
  if (source?.tokenId && !rawReferences.tokenId) {
    rawReferences.tokenId = source.tokenId;
  }
  if (source?.note && !rawReferences.rawNote) {
    rawReferences.rawNote = typeof source.note === "string" ? source.note : toHex(source.note);
  }

  const verified = discrepancies.length === 0;
  return {
    verified,
    status: verified ? "verified" : "unverified",
    direction,
    discrepancies,
    agreements,
    rawReferences,
    details: {
      expectedRecipient,
      candidateRecipient,
      expectedTokenId: expectedToken,
      candidateTokenId: candidateToken,
      expectedAmount: expectedAmount !== null ? expectedAmount.toString() : String(source?.amount),
      candidateAmount: candidateAmount !== null ? candidateAmount.toString() : undefined,
      expectedCommandId,
      candidateCommandId,
      displayNonce,
    },
  };
}

/**
 * Reconcile evidence payload containing source and candidate objects.
 *
 * @param {object} evidence Object with { sourceIntent, candidatePayment } or { source, candidate }
 * @returns {object} Reconciliation report
 */
export function reconcileAxelarEvidence(evidence) {
  const source = evidence.sourceIntent ?? evidence.source;
  const candidate = evidence.candidatePayment ?? evidence.candidate;
  if (!source) {
    return {
      verified: false,
      status: "unverified",
      direction: "outbound",
      discrepancies: ["missing source intent in evidence"],
      agreements: [],
      rawReferences: evidence.rawReferences ?? {},
      details: {},
    };
  }
  return reconcileAxelarPayment(source, candidate);
}

function printUsage() {
  console.log(`
Usage:
  node script/stellar/reconcile-axelar.mjs [options]

Options:
  --evidence <path>      Path to JSON file containing source and candidate payment
  --source <path>        Path to source intent JSON file
  --candidate <path>     Path to candidate payment JSON file
  --fixture <dir>        Path to fixture directory containing evidence.json (or source.json and candidate.json)
  --strict               Exit with code 1 if reconciliation is unverified
  --pretty               Output formatted JSON (default: true)
  -h, --help             Show this help message

Input Format (JSON):
  {
    "sourceIntent": {
      "direction": "outbound",
      "note": "0x01...",
      "tokenId": "0x...",
      "amount": "10000000",
      "commandId": "0x...",
      "rawReferences": { "txHash": "...", "block": 12345 }
    },
    "candidatePayment": {
      "recipient": "0x...",
      "tokenId": "0x...",
      "amount": "10000000",
      "commandId": "0x...",
      "rawReferences": { "txHash": "...", "block": 67890 }
    }
  }
`);
}

function parseCliArgs(argv) {
  const args = {
    evidencePath: null,
    sourcePath: null,
    candidatePath: null,
    fixtureDir: null,
    strict: false,
    pretty: true,
  };

  for (let i = 2; i < argv.length; i++) {
    const arg = argv[i];
    if (arg === "--help" || arg === "-h") {
      printUsage();
      process.exit(0);
    } else if (arg === "--evidence" && i + 1 < argv.length) {
      args.evidencePath = argv[++i];
    } else if (arg === "--source" && i + 1 < argv.length) {
      args.sourcePath = argv[++i];
    } else if (arg === "--candidate" && i + 1 < argv.length) {
      args.candidatePath = argv[++i];
    } else if (arg === "--fixture" && i + 1 < argv.length) {
      args.fixtureDir = argv[++i];
    } else if (arg === "--strict") {
      args.strict = true;
    } else if (arg === "--pretty") {
      args.pretty = true;
    } else if (arg === "--no-pretty") {
      args.pretty = false;
    }
  }
  return args;
}

export function runCli(argv = process.argv) {
  const args = parseCliArgs(argv);

  let evidence = null;

  if (args.fixtureDir) {
    const fixturePath = resolve(args.fixtureDir);
    const evidenceFile = join(fixturePath, "evidence.json");
    try {
      evidence = JSON.parse(readFileSync(evidenceFile, "utf8"));
    } catch {
      const srcFile = join(fixturePath, "source.json");
      const candFile = join(fixturePath, "candidate.json");
      const source = JSON.parse(readFileSync(srcFile, "utf8"));
      let candidate = null;
      try {
        candidate = JSON.parse(readFileSync(candFile, "utf8"));
      } catch {
        // candidate file may be omitted or empty
      }
      evidence = { sourceIntent: source, candidatePayment: candidate };
    }
  } else if (args.evidencePath) {
    const content = readFileSync(resolve(args.evidencePath), "utf8");
    evidence = JSON.parse(content);
  } else if (args.sourcePath) {
    const source = JSON.parse(readFileSync(resolve(args.sourcePath), "utf8"));
    let candidate = null;
    if (args.candidatePath) {
      candidate = JSON.parse(readFileSync(resolve(args.candidatePath), "utf8"));
    }
    evidence = { sourceIntent: source, candidatePayment: candidate };
  } else if (!process.stdin.isTTY) {
    // Read from stdin
    const input = readFileSync(0, "utf8");
    evidence = JSON.parse(input);
  } else {
    printUsage();
    process.exit(1);
  }

  const report = reconcileAxelarEvidence(evidence);
  const jsonOutput = args.pretty ? JSON.stringify(report, null, 2) : JSON.stringify(report);
  console.log(jsonOutput);

  if (args.strict && !report.verified) {
    process.exit(1);
  }
  return report;
}

const isMain = process.argv[1] && pathToFileURL(resolve(process.argv[1])).href === import.meta.url;
if (isMain) {
  runCli(process.argv);
}
