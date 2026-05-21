export type DashboardMetadata = {
  file: string;
  architecture: string;
};

export const dashboardMetadata: DashboardMetadata[] = [
  {
    file: "intel-sandy-ivy-bridge-xeon.json",
    architecture: "Sandy Bridge-EP / Ivy Bridge-EP",
  },
  {
    file: "intel-haswell-xeon.json",
    architecture: "Haswell / Broadwell Xeon",
  },
  {
    file: "intel-skylake-xeon.json",
    architecture: "Skylake / Cascade Lake Xeon",
  },
  {
    file: "intel-ice-lake-xeon.json",
    architecture: "Ice Lake Xeon",
  },
  {
    file: "intel-sapphire-rapids-xeon.json",
    architecture: "Sapphire Rapids / Emerald Rapids Xeon",
  },
];

export const publicBaseUrl = "https://ocellus.minhuw.dev";
export const githubRepo = "minhuw/ocellus";
