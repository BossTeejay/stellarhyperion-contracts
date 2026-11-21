/**
 * The assets Hyperion carries, per chain.
 *
 * The same dollar has a different address, a different precision and a different way of being held
 * on every chain it exists on, and a bridge that treats those as interchangeable is a bridge that
 * eventually sends six decimals of something into a seven decimal slot. So each side is described
 * on its own terms and the two are tied together by a symbol rather than by an assumption.
 *
 * Provenance works the same way it does for the rail contracts: `confirmed` means a primary source
 * was read for that exact entry during this build. The deploy script reads `symbol()` and
 * `decimals()` on chain before registering anything regardless, because an address that is correct
 * today and a token that behaves as expected are two different claims.
 */
import { chain, isChainKey, type ChainKey } from "../chains.js";
import type { Hex } from "../bytes.js";
import { isStellarAddress } from "../addresses.js";
import {
  RouteKind,
  ROUTE_SLUGS,
  tryRouteFromSlug,
  type RouteKind as RouteKindType,
} from "../routes.js";

/** What every asset entry says about itself, whichever chain it lives on. */
interface AssetBase {
  readonly chain: ChainKey;
  /** The ticker, and the key that ties the two sides of a hop together. */
  readonly symbol: string;
  readonly name: string;
  readonly decimals: number;
  /** Which rails can carry this asset out of this chain. */
  readonly routes: readonly RouteKind[];
  readonly source: string;
  readonly confirmed: boolean;
}

export interface EvmAsset extends AssetBase {
  readonly family: "evm";
  readonly address: Hex;
}

export interface StellarAsset extends AssetBase {
  readonly family: "stellar";
  /** The classic asset code, four or twelve characters. */
  readonly code: string;
  /** The issuing account. Half of what makes a Stellar asset unique; the code is the other half. */
  readonly issuer: string;
  /**
   * The Stellar Asset Contract id, which Soroban actually calls into.
   *
   * Null here on purpose. It is derived from the code, the issuer and the network passphrase, so
   * the deploy script computes it with `stellar contract id asset` and writes it into the
   * deployment record rather than anybody typing it.
   */
  readonly sacId: string | null;
  readonly decimals: 7;
}

export type Asset = EvmAsset | StellarAsset;

/** Explicit provider-side token addresses, kept separate from canonical asset metadata. */
export type ProviderAddressOverrides = Readonly<
  Partial<Record<ChainKey, Readonly<Record<string, Partial<Record<RouteKindType, string>>>>>>
>;

export interface ProviderAddressPair {
  readonly sourceChain: ChainKey;
  readonly destinationChain: ChainKey;
  readonly symbol: string;
  readonly route: RouteKindType;
  readonly source: string;
  readonly destination: string;
}

/** Field-specific failure while reading or resolving provider address overrides. */
export class ProviderAddressError extends Error {
  constructor(
    readonly path: string,
    reason: string,
  ) {
    super(`provider address invalid at ${path}: ${reason}`);
    this.name = "ProviderAddressError";
  }
}

function providerAddress(value: unknown, path: string, chainKey: ChainKey): string {
  if (typeof value !== "string" || value.length === 0) {
    throw new ProviderAddressError(path, "expected a non empty address");
  }
  const network = chain(chainKey);
  if (network.family === "evm" && !/^0x[0-9a-fA-F]{40}$/.test(value)) {
    throw new ProviderAddressError(path, "expected a 20 byte EVM address");
  }
  if (network.family === "stellar" && (!value.startsWith("C") || !isStellarAddress(value))) {
    throw new ProviderAddressError(path, "expected a Stellar contract id");
  }
  return value;
}

/**
 * Validate address overrides supplied by an external config.
 *
 * This only records the addresses that a provider uses. It never writes to the declared asset
 * address, chain, or precision.
 */
export function parseProviderAddressOverrides(value: unknown): ProviderAddressOverrides {
  if (typeof value !== "object" || value === null || Array.isArray(value)) {
    throw new ProviderAddressError("$", "expected an object keyed by chain");
  }
  const output: Partial<Record<ChainKey, Record<string, Partial<Record<RouteKindType, string>>>>> =
    {};
  for (const [chainName, chainValue] of Object.entries(value)) {
    const chainPath = `$.${chainName}`;
    if (!isChainKey(chainName)) throw new ProviderAddressError(chainPath, "unknown chain key");
    if (typeof chainValue !== "object" || chainValue === null || Array.isArray(chainValue)) {
      throw new ProviderAddressError(chainPath, "expected an object keyed by asset symbol");
    }
    const chainOverrides: Record<string, Partial<Record<RouteKindType, string>>> = {};
    for (const [symbol, symbolValue] of Object.entries(chainValue as Record<string, unknown>)) {
      const symbolPath = `${chainPath}.${symbol}`;
      const asset = assetOn(chainName, symbol);
      if (asset === null) throw new ProviderAddressError(symbolPath, "unknown asset on chain");
      if (typeof symbolValue !== "object" || symbolValue === null || Array.isArray(symbolValue)) {
        throw new ProviderAddressError(symbolPath, "expected an object keyed by provider rail");
      }
      const routeOverrides: Partial<Record<RouteKindType, string>> = {};
      for (const [slug, addressValue] of Object.entries(symbolValue as Record<string, unknown>)) {
        const path = `${symbolPath}.${slug}`;
        const route = tryRouteFromSlug(slug);
        if (route === null) throw new ProviderAddressError(path, "unknown provider rail slug");
        if (!asset.routes.includes(route)) {
          throw new ProviderAddressError(path, "asset does not support this provider rail");
        }
        routeOverrides[route] = providerAddress(addressValue, path, chainName);
      }
      chainOverrides[symbol] = routeOverrides;
    }
    output[chainName] = chainOverrides;
  }
  return output;
}

/**
 * Require an explicit provider address for both ends of a supported same-network route.
 *
 * Provider addresses are deliberately not inferred from canonical token addresses: providers may
 * represent the same asset differently. The returned pair contains no chain or decimal overrides.
 */
export function resolveProviderAddressPair(
  sourceChain: ChainKey,
  destinationChain: ChainKey,
  symbol: string,
  route: RouteKindType,
  overrides: ProviderAddressOverrides,
): ProviderAddressPair {
  const sourcePath = `providerAddresses.${sourceChain}.${symbol}.${ROUTE_SLUGS[route]}`;
  const destinationPath = `providerAddresses.${destinationChain}.${symbol}.${ROUTE_SLUGS[route]}`;
  if (chain(sourceChain).network !== chain(destinationChain).network) {
    throw new ProviderAddressError(
      "destinationChain",
      "source and destination must use the same network",
    );
  }
  const sourceAsset = assetOn(sourceChain, symbol);
  if (!sourceAsset?.routes.includes(route)) {
    throw new ProviderAddressError("sourceChain", "asset or provider capability is unavailable");
  }
  const destinationAsset = assetOn(destinationChain, symbol);
  if (!destinationAsset?.routes.includes(route)) {
    throw new ProviderAddressError(
      "destinationChain",
      "asset or provider capability is unavailable",
    );
  }
  const source = overrides[sourceChain]?.[symbol]?.[route];
  if (source === undefined) {
    throw new ProviderAddressError(sourcePath, "required provider address is missing");
  }
  const destination = overrides[destinationChain]?.[symbol]?.[route];
  if (destination === undefined) {
    throw new ProviderAddressError(destinationPath, "required provider address is missing");
  }
  return {
    sourceChain,
    destinationChain,
    symbol,
    route,
    source: providerAddress(source, sourcePath, sourceChain),
    destination: providerAddress(destination, destinationPath, destinationChain),
  };
}

const STELLAR_DOCS = "developers.stellar.org, read during this build";
const ARC_DOCS = "docs.arc.io, read during this build";
const CIRCLE_PUBLISHED =
  "Circle's published USDC deployment for this chain. The deploy script reads symbol and decimals on chain before registering it.";

/** Everything Hyperion can move. USDC first, because USDC is why CCTP exists. */
export const ASSETS: readonly Asset[] = [
  {
    family: "stellar",
    chain: "stellar",
    symbol: "USDC",
    name: "USD Coin",
    code: "USDC",
    issuer: "GA5ZSEJYB37JRC5AVCIA5MOP4RHTM335X2KGX3IHOJAPP5RE34K4KZVN",
    sacId: null,
    decimals: 7,
    routes: [RouteKind.Cctp, RouteKind.AxelarIts, RouteKind.Allbridge],
    source: STELLAR_DOCS,
    confirmed: true,
  },
  {
    family: "stellar",
    chain: "stellar-testnet",
    symbol: "USDC",
    name: "USD Coin",
    code: "USDC",
    issuer: "GBBD47IF6LWK7P7MDEVSCWR7DPUWV3NY3DTQEVFL4NAT4AQH3ZLLFLA5",
    sacId: null,
    decimals: 7,
    routes: [RouteKind.Cctp, RouteKind.AxelarIts],
    source: STELLAR_DOCS,
    confirmed: true,
  },
  {
    family: "evm",
    chain: "arc",
    symbol: "USDC",
    name: "USD Coin",
    // Arc's USDC lives at a fixed system address rather than a deployed contract, and it is also
    // the gas token. Six decimals on the ERC-20 even though the chain accounts for gas in
    // eighteen, which is the single most expensive detail on this chain to get wrong.
    address: "0x3600000000000000000000000000000000000000",
    decimals: 6,
    routes: [RouteKind.Cctp],
    source: ARC_DOCS,
    confirmed: true,
  },
  {
    family: "evm",
    chain: "arc-testnet",
    symbol: "USDC",
    name: "USD Coin",
    address: "0x3600000000000000000000000000000000000000",
    decimals: 6,
    routes: [RouteKind.Cctp],
    source: ARC_DOCS,
    confirmed: true,
  },
  {
    family: "evm",
    chain: "arc",
    symbol: "EURC",
    name: "Euro Coin",
    address: "0xbEf5f6d51CB62b58e6A8f77868681825C6fe21c1",
    decimals: 6,
    routes: [RouteKind.AxelarIts],
    source: ARC_DOCS,
    confirmed: true,
  },
  {
    family: "evm",
    chain: "arc-testnet",
    symbol: "EURC",
    name: "Euro Coin",
    address: "0x89B50855Aa3bE2F677cD6303Cec089B5F319D72a",
    decimals: 6,
    routes: [RouteKind.AxelarIts],
    source: ARC_DOCS,
    confirmed: true,
  },
  {
    family: "evm",
    chain: "ethereum",
    symbol: "USDC",
    name: "USD Coin",
    address: "0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48",
    decimals: 6,
    routes: [RouteKind.Cctp, RouteKind.AxelarIts, RouteKind.Allbridge],
    source: CIRCLE_PUBLISHED,
    confirmed: false,
  },
  {
    family: "evm",
    chain: "sepolia",
    symbol: "USDC",
    name: "USD Coin",
    address: "0x1c7D4B196Cb0C7B01d743Fbc6116a902379C7238",
    decimals: 6,
    routes: [RouteKind.Cctp, RouteKind.AxelarIts],
    source: CIRCLE_PUBLISHED,
    confirmed: false,
  },
  {
    family: "evm",
    chain: "base",
    symbol: "USDC",
    name: "USD Coin",
    address: "0x833589fCD6eDb6E08f4c7C32D4f71b54bdA02913",
    decimals: 6,
    routes: [RouteKind.Cctp, RouteKind.AxelarIts, RouteKind.Allbridge],
    source: CIRCLE_PUBLISHED,
    confirmed: false,
  },
  {
    family: "evm",
    chain: "base-sepolia",
    symbol: "USDC",
    name: "USD Coin",
    address: "0x036CbD53842c5426634e7929541eC2318f3dCF7e",
    decimals: 6,
    routes: [RouteKind.Cctp, RouteKind.AxelarIts],
    source: CIRCLE_PUBLISHED,
    confirmed: false,
  },
];

export function assetsOn(chainKey: ChainKey): readonly Asset[] {
  return ASSETS.filter((asset) => asset.chain === chainKey);
}

export function assetOn(chainKey: ChainKey, symbol: string): Asset | null {
  return ASSETS.find((asset) => asset.chain === chainKey && asset.symbol === symbol) ?? null;
}

export function isEvmAsset(asset: Asset): asset is EvmAsset {
  return asset.family === "evm";
}

export function isStellarAsset(asset: Asset): asset is StellarAsset {
  return asset.family === "stellar";
}

/**
 * Whether a symbol exists on both ends of a hop, which is the first thing a route planner needs to
 * know and the cheapest check it can do.
 */
export function pairExists(from: ChainKey, to: ChainKey, symbol: string): boolean {
  return assetOn(from, symbol) !== null && assetOn(to, symbol) !== null;
}

/** Every rail that can carry a symbol from one chain to another, in no particular order. */
export function routesForPair(from: ChainKey, to: ChainKey, symbol: string): readonly RouteKind[] {
  const source = assetOn(from, symbol);
  const destination = assetOn(to, symbol);
  if (source === null || destination === null) return [];
  return source.routes.filter((route) => destination.routes.includes(route));
}
