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
        pname = "pimsteward-workspace";
        version = "0.1.0";
        strictDeps = true;
        buildInputs = with pkgs; [openssl sqlite];
        nativeBuildInputs = with pkgs; [pkg-config];
      };

      cargoArtifacts = craneLib.buildDepsOnly commonArgs;

      package = craneLib.buildPackage (commonArgs
        // {
          inherit cargoArtifacts;
          pname = "pimsteward";
          cargoExtraArgs = "-p pimsteward";
          doCheck = false; # tests run in `nix flake check` separately
        });

      # Host-side ICS feed builder — consumed by the dotfiles flake as an
      # input so a systemd timer can run it OUTSIDE the pimsteward container.
      ics-feedbuilder = craneLib.buildPackage (commonArgs
        // {
          inherit cargoArtifacts;
          pname = "ics-feedbuilder";
          cargoExtraArgs = "-p ics-feedbuilder";
          doCheck = false;
        });
    in {
      packages = {
        default = package;
        inherit ics-feedbuilder;
      };

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
