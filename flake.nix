{
  description = "local-review — workspace housing jjr (jj stacks) and ggr (GitHub PRs)";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    fenix = {
      url = "github:nix-community/fenix";
      inputs.nixpkgs.follows = "nixpkgs";
    };
    flake-utils.url = "github:numtide/flake-utils";
  };

  outputs = {
    self,
    nixpkgs,
    fenix,
    flake-utils,
    ...
  }:
    flake-utils.lib.eachDefaultSystem (system: let
      pkgs = nixpkgs.legacyPackages.${system};

      toolchain = fenix.packages.${system}.stable.withComponents [
        "cargo"
        "clippy"
        "rustc"
        "rustfmt"
        "rust-src"
        "llvm-tools-preview"
      ];

      rustPlatform = pkgs.makeRustPlatform {
        cargo = toolchain;
        rustc = toolchain;
      };

      cargoToml = builtins.fromTOML (builtins.readFile (self + "/crates/jjr/Cargo.toml"));
    in {
      packages.default = rustPlatform.buildRustPackage {
        pname = cargoToml.package.name;
        inherit (cargoToml.package) version;

        src = self;
        cargoLock.lockFile = self + "/Cargo.lock";

        cargoBuildFlags = ["--package" "jjr"];

        # Tests use /bin/true and /bin/false as agent-spawn fixtures; the Nix
        # build sandbox strips /bin to /bin/sh on Linux, so they fail there.
        # The full suite runs outside the sandbox via `just validate`.
        doCheck = false;

        meta = with pkgs.lib; {
          inherit (cargoToml.package) description;
          homepage = "https://github.com/ericbmerritt/jujutsu-review";
          license = with licenses; [mit asl20];
          mainProgram = "jjr";
          platforms = platforms.unix;
        };
      };

      devShells.default = pkgs.mkShell {
        name = "jjr";

        packages = with pkgs; [
          toolchain
          fenix.packages.${system}.rust-analyzer
          jujutsu
          just
          alejandra
          statix
          cargo-deny
          cargo-llvm-cov
          cargo-nextest
          ripgrep
          prettier
        ];
      };
    });
}
