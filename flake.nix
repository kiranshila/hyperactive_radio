{
  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";
    crane.url = "github:ipetkov/crane";
    flake-utils.url = "github:numtide/flake-utils";
    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };
  outputs = {
    self,
    nixpkgs,
    crane,
    flake-utils,
    rust-overlay,
    ...
  }:
    flake-utils.lib.eachDefaultSystem (
      system: let
        pkgs = import nixpkgs {
          inherit system;
          overlays = [(import rust-overlay)];
        };

        inherit (pkgs) lib;

        rustToolchainFor = p: p.rust-bin.fromRustupToolchainFile ./rust-toolchain.toml;
        craneLib = (crane.mkLib pkgs).overrideToolchain rustToolchainFor;

        src = lib.fileset.toSource rec {
          root = ./.;
          fileset = lib.fileset.unions [
            # Standard rust stuff
            (craneLib.fileset.commonCargoSources root)
            # Linker script
            ./ha_radio/memory.x
            # libopus C sources (submodule — maybeMissing since nix doesn't clone submodules;
            # the crane build fetches these via fetchSubmodules in the src override)
            (lib.fileset.maybeMissing ./opus-sys/opus)
          ];
        };

        # elf2uf2 has been abandonded, so we'll use a fork
        elf2uf2 = pkgs.rustPlatform.buildRustPackage rec {
          pname = "elf2flash";
          version = "0.1.0";
          src = pkgs.fetchCrate {
            inherit pname version;
            hash = "sha256-XSqH4qRNj6jTykEGHPCcI7z0m+B4iNCz/vu39UIYiPs=";
          };
          cargoHash = "sha256-nfkVLzMF09d8gofvgZvIVTq6I1YH5hDmcBYqlw8SEQ4=";
          nativeBuildInputs = with pkgs; [
            pkg-config
          ];
          buildInputs = with pkgs; [udev];
        };

        commonArgs = {
          inherit src;
          strictDeps = true;
          doCheck = false;
          nativeBuildInputs = with pkgs; [
            flip-link
            elf2uf2
          ];
        };

        asp_link = craneLib.buildPackage (commonArgs
          // {
            cargoArtifacts = null;
            postInstall = ''
              elf2flash convert --board rp2350 $out/bin/ha_radio $out/bin/ha_radio.uf2
            '';
          });
      in {
        checks = {
          inherit asp_link;
        };
        packages.default = asp_link;
        devShells.default = craneLib.devShell {
          checks = self.checks.${system};
          packages = with pkgs; [
            cargo-outdated
            probe-rs-tools
            gcc-arm-embedded
          ];
        };
      }
    );
}
