# Ocellus Dashboard Site

This directory builds the Next.js static site for `https://ocellus.minhuw.dev`.

The dashboard source of truth stays in `demo/grafana/dashboards` as Grafana v2
resource JSON. The TypeScript prebuild publishes those files under
`site/public/dashboard-assets/v2`, generates classic import JSON under
`site/public/dashboard-assets/classic`, and writes the public manifest at
`site/public/dashboards/index.json`.

The build derives the dashboard bundle version from `OCELLUS_SITE_VERSION` or `git describe --tags --always --dirty`. Latest dashboard JSON is served by Cloudflare Pages, while version-pinned dashboard URLs under `/dashboards/<tag>/...` redirect to immutable GitHub Release assets.

## Local build

```sh
cd site
npm install
npm run build
npx serve out
```

Open the local URL printed by `serve`.

## Cloudflare Pages

The site deploys through `.github/workflows/site-deploy.yml` using Wrangler direct upload.

Create a Cloudflare Pages project named `ocellus`, then create a GitHub Actions
environment named `deployment` with these environment secrets:

- `CLOUDFLARE_API_TOKEN`: Cloudflare API token with Pages edit access for the account.
- `CLOUDFLARE_ACCOUNT_ID`: Cloudflare account ID that owns the Pages project.

The workflow builds with:

- Build command: `cd site && npm ci && npm run build`
- Build output directory: `site/out`

Pushes to `main` deploy production. Pull requests build the site but do not deploy.
Configure the custom domain `ocellus.minhuw.dev` on the Cloudflare Pages project.

Next.js static export copies `_headers` and `_redirects` from `site/public` into the generated output.

## Existing Grafana auto-sync

Grafana still needs file provisioning, Git Sync, or an API workflow to update dashboards.
The default sync format is Grafana v2 resource JSON:

```sh
npx --yes --package=github:minhuw/ocellus ocellus-sync-dashboards \
  --manifest https://ocellus.minhuw.dev/dashboards/index.json \
  --output /var/lib/grafana/dashboards/ocellus
```

Then point a Grafana dashboard file provider at `/var/lib/grafana/dashboards/ocellus`.
An example provider config is available at `site/examples/grafana-dashboard-provider.yml`.

Use `--format classic` to sync generated classic dashboard JSON instead. Classic
sync rewrites import placeholders to the datasource UID `Prometheus`; pass
`--datasource-uid UID` if your Grafana datasource uses a different UID. For a
long-running sidecar style process, add `--interval-seconds 300`.

To pin dashboards to a release, use a versioned manifest URL:

```sh
npx --yes --package=github:minhuw/ocellus ocellus-sync-dashboards \
  --manifest https://ocellus.minhuw.dev/dashboards/v0.12.0/index.json \
  --output /var/lib/grafana/dashboards/ocellus
```

Tagged releases upload the dashboard JSON files and `ocellus-dashboards-<tag>.json` to GitHub Releases.
