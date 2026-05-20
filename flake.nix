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

      jjrToml = builtins.fromTOML (builtins.readFile (self + "/crates/jjr/Cargo.toml"));
      ggrToml = builtins.fromTOML (builtins.readFile (self + "/crates/ggr/Cargo.toml"));

      mkTool = {
        toml,
        mainProgram,
      }:
        rustPlatform.buildRustPackage {
          pname = toml.package.name;
          inherit (toml.package) version;

          src = self;
          cargoLock.lockFile = self + "/Cargo.lock";

          cargoBuildFlags = ["--package" toml.package.name];

          # Tests use /bin/true and /bin/false as agent-spawn fixtures; the Nix
          # build sandbox strips /bin to /bin/sh on Linux, so they fail there.
          # The full suite runs outside the sandbox via `just validate`.
          doCheck = false;

          meta = with pkgs.lib; {
            inherit (toml.package) description;
            homepage = "https://github.com/ericbmerritt/local-review";
            license = with licenses; [mit asl20];
            inherit mainProgram;
            platforms = platforms.unix;
          };
        };
      jjrPkg = mkTool {
        toml = jjrToml;
        mainProgram = "jjr";
      };
      ggrPkg = mkTool {
        toml = ggrToml;
        mainProgram = "ggr";
      };
    in {
      packages = {
        jjr = jjrPkg;
        ggr = ggrPkg;
        default = jjrPkg;
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
          # README screenshot recorder. VHS drives a headless terminal
          # emulator (ttyd) and renders GIF/SVG via ffmpeg.
          vhs
          ttyd
        ];
      };
    });
}
