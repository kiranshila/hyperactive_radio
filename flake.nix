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

        elf2uf2-rs = pkgs.rustPlatform.buildRustPackage {
          pname = "elf2uf2-rs";
          version = "git";
          src = pkgs.fetchFromGitHub {
            owner = "jonil";
            repo = "elf2uf2-rs";
            rev = "master";
            hash = "sha256-UkB3papVAyr5wxXBp4erzL25W2pXf52Ud0cbi6PtqLo=";
          };
          cargoHash = "sha256-A2PORcFt2rO+ZekyLIjNTpcpyhzx5up10HEa88n5Su8=";
          nativeBuildInputs = [ pkgs.pkg-config ];
          buildInputs = [ pkgs.udev ];
        };

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

        commonArgs = {
          inherit src;
          strictDeps = true;
          doCheck = false;
          nativeBuildInputs = with pkgs; [
            flip-link
          ];
        };

        asp_link = craneLib.buildPackage (commonArgs
          // {
            cargoArtifacts = null;
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
            elf2uf2-rs
            tio
          ];
        };
      }
    );
}
