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
import {
  classicDashboardFile,
  convertDashboardV2ToClassic,
} from "../lib/grafana-dashboard-converter";
import type { GrafanaDashboardV2 } from "../lib/grafana-dashboard-converter";

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
const publicDashboardAssetDir = join(publicDir, "dashboard-assets");
const publicClassicDashboardDir = join(publicDashboardAssetDir, "classic");
const publicV2DashboardDir = join(publicDashboardAssetDir, "v2");

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

async function loadDashboard(file: string): Promise<GrafanaDashboardV2> {
  return JSON.parse(await readFile(join(dashboardSourceDir, file), "utf8")) as GrafanaDashboardV2;
}

async function buildDashboardEntries(
  version: string,
  hasRelease: boolean,
): Promise<DashboardEntry[]> {
  const entries: DashboardEntry[] = [];
  await mkdir(publicDashboardDir, { recursive: true });
  await mkdir(publicClassicDashboardDir, { recursive: true });
  await mkdir(publicV2DashboardDir, { recursive: true });

  for (const item of dashboardMetadata) {
    const source = join(dashboardSourceDir, item.file);
    const v2Destination = join(publicV2DashboardDir, item.file);
    const classicFile = classicDashboardFile(item.file);
    const classicDestination = join(publicClassicDashboardDir, classicFile);
    const payload = await loadDashboard(item.file);
    const classicDashboard = convertDashboardV2ToClassic(payload);

    await copyFile(source, v2Destination);
    await writeJson(classicDestination, classicDashboard);

    const releaseUrl = hasRelease
      ? githubReleaseAssetUrl(version, item.file)
      : null;
    const classicReleaseUrl = hasRelease
      ? githubReleaseAssetUrl(version, classicFile)
      : null;

    entries.push({
      title: dashboardTitle(payload),
      uid: dashboardUid(payload, item.file),
      architecture: item.architecture,
      file: item.file,
      url: `${publicBaseUrl}/dashboard-assets/v2/${item.file}`,
      classicFile,
      classicUrl: `${publicBaseUrl}/dashboard-assets/classic/${classicFile}`,
      releaseUrl,
      classicReleaseUrl,
      versionedUrl: hasRelease
        ? `${publicBaseUrl}/dashboards/${version}/v2/${item.file}`
        : null,
      classicVersionedUrl: hasRelease
        ? `${publicBaseUrl}/dashboards/${version}/classic/${classicFile}`
        : null,
      sha256: await sha256File(v2Destination),
      classicSha256: await sha256File(classicDestination),
      bytes: (await stat(v2Destination)).size,
      classicBytes: (await stat(classicDestination)).size,
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

/dashboard-assets/classic/*.json
  Cache-Control: public, max-age=300
  Access-Control-Allow-Origin: *

/dashboard-assets/v2/*.json
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
/dashboards/index.json /dashboards/index.json 200
/dashboards/release-index.json /dashboards/release-index.json 200
/dashboards/:version/index.json https://github.com/${githubRepo}/releases/download/:version/ocellus-dashboards-:version.json 302
/dashboards/:version/:file https://github.com/${githubRepo}/releases/download/:version/:file 302
/dashboards/:version/:format/:file https://github.com/${githubRepo}/releases/download/:version/:file 302
/dashboards/:file /dashboard-assets/v2/:file 301
`,
  );
}

async function main(): Promise<void> {
  const version = gitVersion();
  const hasRelease = hasReleaseArtifacts(version);

  await rm(publicDashboardDir, { recursive: true, force: true });
  await rm(publicDashboardAssetDir, { recursive: true, force: true });
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
          classicUrl: dashboard.classicReleaseUrl ?? dashboard.classicUrl,
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
