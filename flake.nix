{
  description = "Rust service — rename me when cloning.";

  inputs = {
    nixpkgs.url = "github:nixos/nixpkgs/nixos-unstable";
    flake-utils.url = "github:numtide/flake-utils";
    crane = {
      url = "github:ipetkov/crane";
    };
  };

  outputs = {
    nixpkgs,
    flake-utils,
    crane,
    ...
  }:
    flake-utils.lib.eachDefaultSystem (system: let
      pkgs = import nixpkgs {inherit system;};
      craneLib = crane.mkLib pkgs;
      src = craneLib.cleanCargoSource ./.;

      commonArgs = {
        inherit src;
        strictDeps = true;
        buildInputs = with pkgs; [openssl sqlite];
        nativeBuildInputs = with pkgs; [pkg-config];
      };

      cargoArtifacts = craneLib.buildDepsOnly commonArgs;

      package = craneLib.buildPackage (commonArgs
        // {
          inherit cargoArtifacts;
          doCheck = false; # tests run in `nix flake check` separately
        });
    in {
      packages.default = package;

      checks = {
        inherit package;
        clippy = craneLib.cargoClippy (commonArgs
          // {
            inherit cargoArtifacts;
            cargoClippyExtraArgs = "--all-targets -- --deny warnings";
          });
        fmt = craneLib.cargoFmt {inherit src;};
        nextest = craneLib.cargoNextest (commonArgs
          // {
            inherit cargoArtifacts;
            partitions = 1;
            partitionType = "count";
          });
      };

      devShells.default = pkgs.mkShell {
        inputsFrom = [package];
        packages = with pkgs; [cargo-nextest cargo-watch bacon rust-analyzer];
      };
    });
}
