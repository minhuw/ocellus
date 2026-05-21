import Link from "next/link";
import { readFile } from "node:fs/promises";
import { join } from "node:path";
import { CodeBlock } from "./code-block";
import { DashboardPicker } from "./dashboard-picker";
import type { DashboardManifest } from "../lib/dashboard-types";

const guideSteps = [
  {
    title: "Run the Ocellus agent",
    body: "Download the latest static Linux binary and start Prometheus daemon mode on the Xeon host.",
    language: "shell" as const,
    command:
      "curl -LO https://github.com/minhuw/ocellus/releases/latest/download/ocellus\nchmod +x ocellus\nsudo modprobe msr\nsudo ./ocellus --daemon --listen 0.0.0.0:8080 --measure-interval-ms 1000",
  },
  {
    title: "Add the Prometheus scrape target",
    body: "Point Prometheus at the Ocellus agent and reload Prometheus.",
    language: "yaml" as const,
    command:
      "scrape_configs:\n  - job_name: ocellus\n    static_configs:\n      - targets: ['xeon-host.example.com:8080']",
  },
];

async function loadManifest(): Promise<DashboardManifest> {
  const payload = await readFile(
    join(process.cwd(), "public", "dashboards", "index.json"),
    "utf8",
  );
  return JSON.parse(payload) as DashboardManifest;
}

export default async function HomePage() {
  const manifest = await loadManifest();

  return (
    <main>
      <section className="site-shell hero home-hero">
        <p className="eyebrow">Xeon uncore observability</p>
        <h1 className="hero-title">
          <span>
            Turn <strong>Intel Uncore PMU Counters</strong>
          </span>
          <span>
            into <strong>Production Telemetry</strong>
          </span>
        </h1>
        <p className="lede">
          Ocellus exports memory, cache, interconnect, power, and fabric
          counters as Prometheus metrics, with Grafana dashboards built for each
          Intel server generation.
        </p>
      </section>

      <section className="site-shell how-to-section" id="how-to">
        <div className="section-heading">
          <p className="eyebrow">How to use it</p>
          <h2>From host counters to Grafana in three steps.</h2>
        </div>
        <ol className="guide-list">
          <li>
            <h3>{guideSteps[0].title}</h3>
            <p>{guideSteps[0].body}</p>
            <CodeBlock language={guideSteps[0].language}>
              {guideSteps[0].command}
            </CodeBlock>
          </li>
          <li>
            <h3>{guideSteps[1].title}</h3>
            <p>{guideSteps[1].body}</p>
            <CodeBlock language={guideSteps[1].language}>
              {guideSteps[1].command}
            </CodeBlock>
          </li>
          <li>
            <h3>Import a Grafana dashboard</h3>
            <p>
              Pick the matching Xeon family and version, then copy the dashboard
              URL into Grafana's import flow.
            </p>
            <DashboardPicker
              dashboards={manifest.dashboards}
              version={manifest.version}
            />
          </li>
        </ol>
      </section>
    </main>
  );
}
