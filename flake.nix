{
  description = "pterm - persistent terminal multiplexer";

  nixConfig = {
    extra-substituters = [ "https://ttak0422-pterm.cachix.org" ];
    extra-trusted-public-keys = [ "ttak0422-pterm.cachix.org-1:s0zSh4J7l8NrisVESCYNxcSw7rz2vLsGa5fh+E42NDY=" ];
  };

  inputs = {
    nixpkgs.url = "github:nixos/nixpkgs/nixos-unstable";
    flake-parts.url = "github:hercules-ci/flake-parts";
    naersk = {
      url = "github:nix-community/naersk";
      inputs.nixpkgs.follows = "nixpkgs";
    };
    git-hooks = {
      url = "github:cachix/git-hooks.nix";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs =
    inputs@{ flake-parts, ... }:
    flake-parts.lib.mkFlake { inherit inputs; } {
      systems = [
        "x86_64-linux"
        "aarch64-linux"
        "aarch64-darwin"
      ];
      perSystem =
        {
          self',
          system,
          pkgs,
          lib,
          ...
        }:
        let
          naersk' = pkgs.callPackage inputs.naersk { };
          darwinInputs = lib.optionals pkgs.stdenv.isDarwin [ pkgs.libiconv ];
          pterm-daemon = naersk'.buildPackage {
            src = ./.;
            buildInputs = darwinInputs;
          };
          inherit (lib) fileset;
          nixFiles = fileset.fileFilter (file: file.hasExt "nix") ./.;
          rustFiles = fileset.unions [
            (fileset.fileFilter (file: lib.hasPrefix "Cargo" file.name) ./.)
            (fileset.fileFilter (file: file.hasExt "rs") ./.)
          ];
          nvimFiles = fileset.difference ./. (
            fileset.unions [
              nixFiles
              rustFiles
              ./docs
              (fileset.maybeMissing ./target)
            ]
          );
          pterm = pkgs.vimUtils.buildVimPlugin {
            pname = "pterm";
            version = "0.1.0";
            src = fileset.toSource {
              root = ./.;
              fileset = nvimFiles;
            };
            preInstall = ''
              mkdir -p target/release
              rm -f target/release/pterm
              ln -s ${pterm-daemon}/bin/pterm target/release/pterm
            '';
          };
        in
        {
          checks = {
            pre-commit-check = inputs.git-hooks.lib.${system}.run {
              src = ./.;
              hooks = {
                deadnix.enable = true;
                nixfmt.enable = true;
                statix.enable = true;
                stylua.enable = true;
                rustfmt.enable = true;
              };
            };
            cargo-check = naersk'.buildPackage {
              src = ./.;
              mode = "check";
              buildInputs = darwinInputs;
            };
            clippy = naersk'.buildPackage {
              src = ./.;
              mode = "clippy";
              buildInputs = darwinInputs;
            };
          };

          packages = {
            default = pterm;
            inherit pterm pterm-daemon;
          };

          devShells.default = pkgs.mkShell {
            inherit (self'.checks.pre-commit-check) shellHook;
            inputsFrom = [ pterm-daemon ];
          };
        };
    };
}