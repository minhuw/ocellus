import { createHash } from "node:crypto";
import { mkdir, readFile, readdir, rename, unlink, writeFile } from "node:fs/promises";
import { basename, isAbsolute, join, normalize } from "node:path";
import type { DashboardManifest } from "../lib/dashboard-types";

const defaultManifest = "https://ocellus.minhuw.dev/dashboards/index.json";
const defaultDatasourceUid = "Prometheus";
const datasourcePlaceholder = "${DS_PROMETHEUS}";

type Args = {
  manifest: string;
  output?: string;
  dashboardBaseUrl?: string;
  format: "v2" | "classic";
  datasourceName?: string;
  datasourceUid: string;
  intervalSeconds?: number;
  dryRun: boolean;
  prune: boolean;
};

type PlannedDashboard = {
  file: string;
  destination: string;
  payload: Buffer;
};

function parseArgs(argv: string[]): Args {
  const args: Args = {
    manifest: defaultManifest,
    format: "v2",
    datasourceUid: defaultDatasourceUid,
    dryRun: false,
    prune: false,
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
    } else if (arg === "--format" && next) {
      if (next !== "v2" && next !== "classic") {
        throw new Error("--format must be v2 or classic");
      }
      args.format = next;
      index += 1;
    } else if (arg === "--datasource-uid" && next) {
      args.datasourceUid = next;
      index += 1;
    } else if (arg === "--datasource-name" && next) {
      args.datasourceName = next;
      index += 1;
    } else if (arg === "--interval-seconds" && next) {
      args.intervalSeconds = Number.parseInt(next, 10);
      index += 1;
    } else if (arg === "--dry-run") {
      args.dryRun = true;
    } else if (arg === "--prune") {
      args.prune = true;
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
  --format v2|classic        Dashboard format to sync. Defaults to v2.
  --datasource-name NAME     Rewrite Prometheus datasource names to NAME.
  --datasource-uid UID       Classic provisioning datasource UID. Defaults to Prometheus.
  --interval-seconds N       Run continuously and sync every N seconds.
  --prune                    Remove old *.json files that are not in the manifest.
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

function isRecord(value: unknown): value is Record<string, unknown> {
  return value !== null && typeof value === "object" && !Array.isArray(value);
}

function shouldRewriteDatasource(args: Args): boolean {
  return args.datasourceName !== undefined || args.datasourceUid !== defaultDatasourceUid;
}

function rewriteDatasourceReferences(
  value: unknown,
  args: Args,
  parentKey?: string,
): unknown {
  if (typeof value === "string") {
    return value === datasourcePlaceholder ? args.datasourceUid : value;
  }

  if (Array.isArray(value)) {
    return value.map((item) => rewriteDatasourceReferences(item, args));
  }

  if (!isRecord(value)) {
    return value;
  }

  const rewritten = Object.fromEntries(
    Object.entries(value).map(([key, item]) => [
      key,
      rewriteDatasourceReferences(item, args, key),
    ]),
  );
  const originalName = typeof value.name === "string" ? value.name : undefined;
  const originalUid = typeof value.uid === "string" ? value.uid : undefined;
  const pointsAtPrometheus =
    originalName === "Prometheus" ||
    originalUid === defaultDatasourceUid ||
    originalUid === datasourcePlaceholder;

  if (parentKey === "datasource" && pointsAtPrometheus) {
    if (args.datasourceName !== undefined) {
      rewritten.name = args.datasourceName;
    }

    if (args.datasourceUid !== defaultDatasourceUid || originalUid === datasourcePlaceholder) {
      rewritten.uid = args.datasourceUid;
    }
  }

  if (
    args.datasourceName !== undefined &&
    value.type === "datasource" &&
    originalName === "Prometheus"
  ) {
    rewritten.name = args.datasourceName;
  }

  return rewritten;
}

function dashboardProvisioningPayload(payload: Buffer, args: Args): Buffer {
  if (args.format !== "classic" && !shouldRewriteDatasource(args)) {
    return payload;
  }

  const dashboard = JSON.parse(payload.toString("utf8")) as unknown;
  const rewritten = rewriteDatasourceReferences(dashboard, args) as Record<string, unknown>;
  if (args.format === "classic") {
    delete rewritten.__inputs;
  }

  return Buffer.from(`${JSON.stringify(rewritten, null, 2)}\n`);
}

async function readCurrentFile(file: string): Promise<Buffer | null> {
  try {
    return await readFile(file);
  } catch {
    return null;
  }
}

async function existingJsonFiles(output: string): Promise<string[]> {
  try {
    const entries = await readdir(output, { withFileTypes: true });
    return entries
      .filter((entry) => entry.isFile() && entry.name.endsWith(".json"))
      .map((entry) => entry.name);
  } catch {
    return [];
  }
}

async function writeAtomically(destination: string, payload: Buffer): Promise<void> {
  const temporary = `${destination}.${process.pid}.${Date.now()}.tmp`;
  try {
    await writeFile(temporary, payload);
    await rename(temporary, destination);
  } catch (error) {
    try {
      await unlink(temporary);
    } catch {
      // Best effort cleanup for a failed atomic write.
    }
    throw error;
  }
}

async function syncOnce(args: Args): Promise<void> {
  const manifest = await fetchJson<DashboardManifest>(args.manifest);
  const output = args.output;
  if (!output) {
    throw new Error("--output is required");
  }

  const plannedDashboards: PlannedDashboard[] = [];
  const manifestFiles = new Set<string>();
  for (const item of manifest.dashboards) {
    const file = safeDashboardFile(
      args.format === "classic" ? item.classicFile : item.file,
    );
    if (manifestFiles.has(file)) {
      throw new Error(`${file}: duplicate dashboard filename in manifest`);
    }
    manifestFiles.add(file);

    const itemUrl = args.format === "classic" ? item.classicUrl : item.url;
    const expectedSha = args.format === "classic" ? item.classicSha256 : item.sha256;
    const url = dashboardUrl(file, itemUrl, args.dashboardBaseUrl);
    const fetchedPayload = await fetchBytes(url);
    const actualSha = sha256(fetchedPayload);
    if (actualSha !== expectedSha) {
      throw new Error(`${file}: sha256 mismatch, expected ${expectedSha}, got ${actualSha}`);
    }
    const payload = dashboardProvisioningPayload(fetchedPayload, args);

    plannedDashboards.push({
      file,
      destination: join(output, file),
      payload,
    });
  }

  const oldFiles = args.prune
    ? (await existingJsonFiles(output)).filter((file) => !manifestFiles.has(file))
    : [];

  if (!args.dryRun) {
    await mkdir(output, { recursive: true });
  }

  for (const dashboard of plannedDashboards) {
    const current = await readCurrentFile(dashboard.destination);
    if (current && current.equals(dashboard.payload)) {
      console.log(`unchanged: ${dashboard.destination}`);
      continue;
    }

    if (args.dryRun) {
      console.log(`would_update: ${dashboard.destination}`);
      continue;
    }

    await writeAtomically(dashboard.destination, dashboard.payload);
    console.log(`updated: ${dashboard.destination}`);
  }

  for (const file of oldFiles) {
    const destination = join(output, file);
    if (args.dryRun) {
      console.log(`would_remove: ${destination}`);
      continue;
    }

    await unlink(destination);
    console.log(`removed: ${destination}`);
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
