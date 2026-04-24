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
      in
      {
        packages.default = pkgs.rustPlatform.buildRustPackage {
          pname = manifest.name;
          version = manifest.version;
          src = self;
          cargoLock.lockFile = ./Cargo.lock;
        };

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
