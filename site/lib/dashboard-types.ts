export type DashboardEntry = {
  title: string;
  uid: string;
  architecture: string;
  file: string;
  url: string;
  classicFile: string;
  classicUrl: string;
  releaseUrl: string | null;
  classicReleaseUrl: string | null;
  versionedUrl: string | null;
  classicVersionedUrl: string | null;
  sha256: string;
  classicSha256: string;
  bytes: number;
  classicBytes: number;
  sourcePath: string;
};

export type DashboardManifest = {
  schemaVersion: 1;
  project: "ocellus";
  version: string;
  channel: string;
  homepage: string;
  release: string | null;
  dashboards: DashboardEntry[];
};
