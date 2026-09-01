{
  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    flake-utils.url = "github:numtide/flake-utils";
    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };
  outputs =
    {
      self,
      nixpkgs,
      flake-utils,
      rust-overlay,
    }:
    flake-utils.lib.eachDefaultSystem (
      system:
      let
        overlays = [ (import rust-overlay) ];
        pkgs = import nixpkgs {
          inherit system overlays;
        };
        rustToolchain = pkgs.pkgsBuildHost.rust-bin.fromRustupToolchainFile ./rust-toolchain.toml;
        rustPlatform = pkgs.makeRustPlatform {
          cargo = rustToolchain;
          rustc = rustToolchain;
        };
        cargoManifest = builtins.fromTOML (builtins.readFile ./Cargo.toml);
      in
      with pkgs;
      {
        formatter = nixfmt-tree;

        packages.default = rustPlatform.buildRustPackage {
          pname = "arbora";
          version = cargoManifest.package.version;
          src = self;

          cargoLock.lockFile = ./Cargo.lock;
          nativeBuildInputs = [ makeWrapper ];
          nativeCheckInputs = [ git ];

          postInstall = ''
            wrapProgram "$out/bin/arbora" \
              --prefix PATH : "${lib.makeBinPath [ git ]}"
          '';
        };

        devShells.default = mkShell {
          shellHook = ''
            export LD_LIBRARY_PATH="$LD_LIBRARY_PATH:${
              pkgs.lib.makeLibraryPath [
                pkgs.libclang.lib
                pkgs.stdenv.cc.cc.lib
              ]
            }"
            export LIBCLANG_PATH="${pkgs.libclang.lib}/lib"
          '';

          packages = [
            rustToolchain
            rust-analyzer
            rustfmt
            cargo-edit
            cargo-watch
            git
            pkg-config
            just
            bacon
          ];
        };
      }
    );
}
