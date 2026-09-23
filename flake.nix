{
  description = "cargo-oracle - symbol-level test attribution for Rust";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";

  outputs = { self, nixpkgs }:
    let
      systems = [ "x86_64-linux" "aarch64-linux" "x86_64-darwin" "aarch64-darwin" ];
      forAllSystems = f: nixpkgs.lib.genAttrs systems (system: f {
        inherit system;
        pkgs = nixpkgs.legacyPackages.${system};
      });
    in
    {
      devShells = forAllSystems ({ pkgs, ... }: {
        default = pkgs.mkShell {
          name = "cargo-oracle";

          packages = with pkgs; [
            # Toolchain
            rustc
            cargo
            rustfmt
            clippy
            rust-analyzer

            # Slice 1-3: the tools cargo-oracle drives and joins against.
            cargo-nextest # process-per-test, needed for per-test attribution
            cargo-llvm-cov # function-level coverage via -C instrument-coverage
            cargo-mutants # body-replacement mutants == extreme mutation

            # Handy during development
            jq
            git
          ];

          env = {
            RUST_BACKTRACE = "1";
            # cargo-llvm-cov needs the LLVM tools that ship with the toolchain.
            LLVM_COV = "${pkgs.llvmPackages.libllvm}/bin/llvm-cov";
            LLVM_PROFDATA = "${pkgs.llvmPackages.libllvm}/bin/llvm-profdata";
          };

          shellHook = ''
            echo "cargo-oracle dev shell"
            echo "  cargo oracle inventory   - list testable symbols"
            echo "  cargo oracle lint        - static oracle-strength audit"
            echo "  cargo oracle report      - full attribution report"
          '';
        };
      });

      packages = forAllSystems ({ pkgs, ... }: {
        default = pkgs.rustPlatform.buildRustPackage {
          pname = "cargo-oracle";
          version = "0.1.0";
          src = ./.;
          cargoLock.lockFile = ./Cargo.lock;

          meta = with pkgs.lib; {
            description = "Symbol-level test attribution: which tests verify which symbols";
            license = licenses.mit;
            mainProgram = "cargo-oracle";
          };
        };
      });

      formatter = forAllSystems ({ pkgs, ... }: pkgs.nixpkgs-fmt);
    };
}
