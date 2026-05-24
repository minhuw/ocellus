{
  description = "ocellus: a Rust hardware telemetry exporter";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    flake-utils.url = "github:numtide/flake-utils";
  };

  outputs = { self, nixpkgs, flake-utils }:
    flake-utils.lib.eachDefaultSystem (system:
      let
        pkgs = import nixpkgs { inherit system; };
        manifest = (pkgs.lib.importTOML ./Cargo.toml).package;
        dashboardSync = pkgs.stdenvNoCC.mkDerivation {
          pname = "ocellus-dashboard-sync";
          version = manifest.version;
          src = self;

          nativeBuildInputs = [
            pkgs.esbuild
            pkgs.makeWrapper
          ];

          buildPhase = ''
            runHook preBuild
            esbuild site/scripts/sync-grafana-dashboards.ts \
              --bundle \
              --platform=node \
              --format=esm \
              --target=node20 \
              --banner:js='#!/usr/bin/env node' \
              --outfile=ocellus-sync-dashboards.js
            runHook postBuild
          '';

          installPhase = ''
            runHook preInstall
            install -Dm755 ocellus-sync-dashboards.js $out/libexec/ocellus-sync-dashboards.js
            makeWrapper ${pkgs.nodejs_22}/bin/node $out/bin/ocellus-sync-dashboards \
              --add-flags $out/libexec/ocellus-sync-dashboards.js
            runHook postInstall
          '';

          meta.mainProgram = "ocellus-sync-dashboards";
        };
      in
      {
        packages.default = pkgs.rustPlatform.buildRustPackage {
          pname = manifest.name;
          version = manifest.version;
          src = self;
          cargoLock.lockFile = ./Cargo.lock;

          # Local cargo invocations use sudo as the Linux runner; Nix sandboxed
          # checks must run the test binary directly.
          postPatch = ''
            rm -f .cargo/config.toml
          '';
        };
        packages.dashboard-sync = dashboardSync;

        apps.default = {
          type = "app";
          program = "${self.packages.${system}.default}/bin/${manifest.name}";
          meta.description = "Run ocellus";
        };

        devShells.default = pkgs.mkShell {
          packages = with pkgs; [
            cargo
            clippy
            rustc
            rustfmt
          ];

          RUST_BACKTRACE = "1";
        };
      });
}
