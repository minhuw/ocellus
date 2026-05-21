import { createHash } from "node:crypto";
import { copyFile, mkdir, readFile, rm, stat, writeFile } from "node:fs/promises";
import { dirname, join, relative, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { execFileSync } from "node:child_process";
import {
  dashboardMetadata,
  githubRepo,
  publicBaseUrl,
} from "../lib/dashboard-metadata";
import type { DashboardEntry, DashboardManifest } from "../lib/dashboard-types";

type GrafanaDashboard = {
  metadata?: {
    name?: string;
  };
  spec?: {
    title?: string;
  };
  title?: string;
  uid?: string;
};

const siteDir = resolve(dirname(fileURLToPath(import.meta.url)), "..");
const repoRoot = resolve(siteDir, "..");
const dashboardSourceDir = join(repoRoot, "demo", "grafana", "dashboards");
const publicDir = join(siteDir, "public");
const publicDashboardDir = join(publicDir, "dashboards");

function dashboardTitle(payload: GrafanaDashboard): string {
  return (
    payload.spec?.title ??
    payload.title ??
    payload.metadata?.name ??
    "Ocellus dashboard"
  );
}

function dashboardUid(payload: GrafanaDashboard, file: string): string {
  return payload.metadata?.name ?? payload.uid ?? file.replace(/\.json$/, "");
}

function gitVersion(): string {
  if (process.env.OCELLUS_SITE_VERSION) {
    return process.env.OCELLUS_SITE_VERSION;
  }

  try {
    return execFileSync("git", ["describe", "--tags", "--always", "--dirty"], {
      cwd: repoRoot,
      encoding: "utf8",
      stdio: ["ignore", "pipe", "ignore"],
    }).trim();
  } catch {
    return "dev";
  }
}

function hasReleaseArtifacts(version: string): boolean {
  if (!version.startsWith("v")) {
    return false;
  }

  if (process.env.OCELLUS_RELEASE_BUILD === "1") {
    return true;
  }

  try {
    const exactTag = execFileSync("git", ["describe", "--exact-match", "--tags", "HEAD"], {
      cwd: repoRoot,
      encoding: "utf8",
      stdio: ["ignore", "pipe", "ignore"],
    }).trim();
    return exactTag === version;
  } catch {
    return false;
  }
}

function githubReleasePageUrl(version: string): string {
  return `https://github.com/${githubRepo}/releases/tag/${version}`;
}

function githubReleaseAssetUrl(version: string, file: string): string {
  return `https://github.com/${githubRepo}/releases/download/${version}/${file}`;
}

async function sha256File(path: string): Promise<string> {
  const payload = await readFile(path);
  return createHash("sha256").update(payload).digest("hex");
}

function manifestPayload(
  dashboards: DashboardEntry[],
  version: string,
  channel: string,
  hasRelease: boolean,
): DashboardManifest {
  return {
    schemaVersion: 1,
    project: "ocellus",
    version,
    channel,
    homepage: publicBaseUrl,
    release: hasRelease ? githubReleasePageUrl(version) : null,
    dashboards,
  };
}

async function loadDashboard(file: string): Promise<GrafanaDashboard> {
  return JSON.parse(await readFile(join(dashboardSourceDir, file), "utf8")) as GrafanaDashboard;
}

async function buildDashboardEntries(
  version: string,
  hasRelease: boolean,
): Promise<DashboardEntry[]> {
  const entries: DashboardEntry[] = [];
  await mkdir(publicDashboardDir, { recursive: true });

  for (const item of dashboardMetadata) {
    const source = join(dashboardSourceDir, item.file);
    const destination = join(publicDashboardDir, item.file);
    const payload = await loadDashboard(item.file);

    await copyFile(source, destination);

    const releaseUrl = hasRelease
      ? githubReleaseAssetUrl(version, item.file)
      : null;

    entries.push({
      title: dashboardTitle(payload),
      uid: dashboardUid(payload, item.file),
      architecture: item.architecture,
      file: item.file,
      url: `${publicBaseUrl}/dashboards/${item.file}`,
      releaseUrl,
      versionedUrl: hasRelease
        ? `${publicBaseUrl}/dashboards/${version}/${item.file}`
        : null,
      sha256: await sha256File(destination),
      bytes: (await stat(destination)).size,
      sourcePath: relative(repoRoot, source),
    });
  }

  return entries;
}

async function writeJson(path: string, payload: unknown): Promise<void> {
  await writeFile(path, `${JSON.stringify(payload, null, 2)}\n`);
}

async function writeHeaders(): Promise<void> {
  await writeFile(
    join(publicDir, "_headers"),
    `/dashboards/index.json
  Cache-Control: public, max-age=60
  Access-Control-Allow-Origin: *

/dashboards/release-index.json
  Cache-Control: public, max-age=300
  Access-Control-Allow-Origin: *

/dashboards/*.json
  Cache-Control: public, max-age=300
  Access-Control-Allow-Origin: *

/*
  X-Content-Type-Options: nosniff
  Referrer-Policy: strict-origin-when-cross-origin
`,
  );
}

async function writeRedirects(): Promise<void> {
  await writeFile(
    join(publicDir, "_redirects"),
    `/manifest.json /dashboards/index.json 200
/dashboards/:version/index.json https://github.com/${githubRepo}/releases/download/:version/ocellus-dashboards-:version.json 302
/dashboards/:version/:file https://github.com/${githubRepo}/releases/download/:version/:file 302
`,
  );
}

async function main(): Promise<void> {
  const version = gitVersion();
  const hasRelease = hasReleaseArtifacts(version);

  await rm(publicDashboardDir, { recursive: true, force: true });
  const dashboards = await buildDashboardEntries(version, hasRelease);

  await writeJson(
    join(publicDashboardDir, "index.json"),
    manifestPayload(dashboards, version, "latest", hasRelease),
  );

  if (hasRelease) {
    await writeJson(
      join(publicDashboardDir, "release-index.json"),
      manifestPayload(
        dashboards.map((dashboard) => ({
          ...dashboard,
          url: dashboard.releaseUrl ?? dashboard.url,
        })),
        version,
        version,
        hasRelease,
      ),
    );
  }

  await mkdir(publicDir, { recursive: true });
  await writeHeaders();
  await writeRedirects();
}

await main();
