export type DashboardEntry = {
  title: string;
  uid: string;
  architecture: string;
  file: string;
  url: string;
  releaseUrl: string | null;
  versionedUrl: string | null;
  sha256: string;
  bytes: number;
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
