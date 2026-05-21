import { createHash } from "node:crypto";
import { mkdir, readFile, rename, writeFile } from "node:fs/promises";
import { basename, isAbsolute, join, normalize } from "node:path";
import type { DashboardManifest } from "../lib/dashboard-types";

const defaultManifest = "https://ocellus.minhuw.dev/dashboards/index.json";

type Args = {
  manifest: string;
  output?: string;
  dashboardBaseUrl?: string;
  intervalSeconds?: number;
  dryRun: boolean;
};

function parseArgs(argv: string[]): Args {
  const args: Args = {
    manifest: defaultManifest,
    dryRun: false,
  };

  for (let index = 0; index < argv.length; index += 1) {
    const arg = argv[index];
    const next = argv[index + 1];

    if (arg === "--manifest" && next) {
      args.manifest = next;
      index += 1;
    } else if (arg === "--output" && next) {
      args.output = next;
      index += 1;
    } else if (arg === "--dashboard-base-url" && next) {
      args.dashboardBaseUrl = next;
      index += 1;
    } else if (arg === "--interval-seconds" && next) {
      args.intervalSeconds = Number.parseInt(next, 10);
      index += 1;
    } else if (arg === "--dry-run") {
      args.dryRun = true;
    } else if (arg === "--help" || arg === "-h") {
      usage(0);
    } else {
      throw new Error(`unknown or incomplete argument: ${arg}`);
    }
  }

  if (!args.output) {
    throw new Error("--output is required");
  }

  if (
    args.intervalSeconds !== undefined &&
    (!Number.isInteger(args.intervalSeconds) || args.intervalSeconds < 1)
  ) {
    throw new Error("--interval-seconds must be greater than 0");
  }

  return args;
}

function usage(code: number): never {
  const output = code === 0 ? console.log : console.error;
  output(`Usage: ocellus-sync-dashboards --output DIR [options]

Options:
  --manifest URL             Manifest URL. Defaults to ${defaultManifest}
  --dashboard-base-url URL   Override dashboard URLs from the manifest.
  --interval-seconds N       Run continuously and sync every N seconds.
  --dry-run                  Fetch and verify without writing files.
`);
  process.exit(code);
}

async function fetchBytes(url: string): Promise<Buffer> {
  if (url.startsWith("file://")) {
    return readFile(new URL(url));
  }

  const response = await fetch(url);
  if (!response.ok) {
    throw new Error(`${url}: HTTP ${response.status}`);
  }
  return Buffer.from(await response.arrayBuffer());
}

async function fetchJson<T>(url: string): Promise<T> {
  return JSON.parse((await fetchBytes(url)).toString("utf8")) as T;
}

function sha256(payload: Buffer): string {
  return createHash("sha256").update(payload).digest("hex");
}

function dashboardUrl(file: string, manifestUrl: string, dashboardBaseUrl?: string): string {
  if (!dashboardBaseUrl) {
    return manifestUrl;
  }
  return new URL(file, dashboardBaseUrl.endsWith("/") ? dashboardBaseUrl : `${dashboardBaseUrl}/`).toString();
}

function safeDashboardFile(file: string): string {
  const normalized = normalize(file);
  if (
    file !== basename(file) ||
    normalized !== file ||
    isAbsolute(file) ||
    file.includes("/") ||
    file.includes("\\") ||
    file === "." ||
    file === ".." ||
    !/^[A-Za-z0-9._-]+\.json$/.test(file)
  ) {
    throw new Error(`${file}: invalid dashboard filename`);
  }
  return file;
}

async function syncOnce(args: Args): Promise<void> {
  const manifest = await fetchJson<DashboardManifest>(args.manifest);
  const output = args.output;
  if (!output) {
    throw new Error("--output is required");
  }

  for (const item of manifest.dashboards) {
    const file = safeDashboardFile(item.file);
    const url = dashboardUrl(file, item.url, args.dashboardBaseUrl);
    const payload = await fetchBytes(url);
    const actualSha = sha256(payload);
    if (actualSha !== item.sha256) {
      throw new Error(`${file}: sha256 mismatch, expected ${item.sha256}, got ${actualSha}`);
    }

    const destination = join(output, file);
    let current: Buffer | null = null;
    try {
      current = await readFile(destination);
    } catch {
      current = null;
    }

    if (current && current.equals(payload)) {
      console.log(`unchanged: ${destination}`);
      continue;
    }

    if (args.dryRun) {
      console.log(`would_update: ${destination}`);
      continue;
    }

    await mkdir(output, { recursive: true });
    const temporary = join(output, `.${file}.${process.pid}.tmp`);
    await writeFile(temporary, payload);
    await rename(temporary, destination);
    console.log(`updated: ${destination}`);
  }
}

function sleep(ms: number): Promise<void> {
  return new Promise((resolve) => {
    setTimeout(resolve, ms);
  });
}

async function main(): Promise<number> {
  let args: Args;
  try {
    args = parseArgs(process.argv.slice(2));
  } catch (error) {
    console.error(`error: ${(error as Error).message}`);
    usage(1);
  }

  while (true) {
    try {
      await syncOnce(args);
    } catch (error) {
      console.error(`error: ${(error as Error).message}`);
      if (args.intervalSeconds === undefined) {
        return 1;
      }
    }

    if (args.intervalSeconds === undefined) {
      return 0;
    }
    await sleep(args.intervalSeconds * 1000);
  }
}

process.exitCode = await main();
