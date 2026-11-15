{
  description = "OmniGraph Studio";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";
    fenix = {
      url = "github:nix-community/fenix";
      inputs.nixpkgs.follows = "nixpkgs";
    };
    naersk = {
      url = "github:nix-community/naersk";
      inputs.nixpkgs.follows = "nixpkgs";
      inputs.fenix.follows = "fenix";
    };
  };

  outputs = { self, nixpkgs, fenix, naersk }:
    let
      # The release archive targets (.github/workflows/build-release-archives.yml)
      # other than Windows.
      systems = [ "x86_64-linux" "aarch64-linux" "aarch64-darwin" ];
      forAllSystems = f: nixpkgs.lib.genAttrs systems (system: f system nixpkgs.legacyPackages.${system});
      bundle = pkgs: with pkgs; [
        cargo
        cmake
        pkg-config
        fontconfig
        libxkbcommon
        libxcb

        lld
        glfw
        wayland-utils
        wayland.dev
        wayland-protocols
      ];

# build = pkgs.callPackage ./build.nix {
#   crane = inputs.crane.mkLib pkgs;
#   rustToolchain = rustBin.fromRustupToolchainFile ../rust-toolchain.toml;
#   commitSha = inputs.self.rev or null;
# };
    in
    {
      devShells = forAllSystems (system: pkgs: let
        lib = pkgs.lib;
        stdenv' = pkgs.stdenv;
      in {
        default = pkgs.mkShell {
          packages = bundle pkgs;
        };

        # included from zed-industries/zed:nix/build.nix
        shellHook = ''
          export LD_LIBRARY_PATH=${pkgs.wayland}/lib:$LD_LIBRARY_PATH
        '';
        NIX_LDFLAGS = lib.optionalString stdenv'.hostPlatform.isLinux "-rpath ${
          with pkgs; lib.makeLibraryPath [
            gpu-lib
            wayland
            libva
          ]
        }";
      });

      packages = forAllSystems (system: pkgs:
        let
          inherit (pkgs) lib;
          # The compiler rust-toolchain.toml pins. nixpkgs' own rustc links an
          # LLVM older than 22, which cannot lower Lance's AVX-512 intrinsics.
          # The hash is that channel's manifest, so it changes with every
          # toolchain bump.
          toolchain = fenix.packages.${system}.fromToolchainFile {
            file = ./rust-toolchain.toml;
            sha256 = "sha256-p8h3Sl/YRByZfZTAKXdsvF6xEenXKrXSVvpphmZENH4";
          };

          naersk' = pkgs.callPackage naersk {
            cargo = toolchain;
            rustc = toolchain;
          };

          # The release archives' flags: none, except long calls on arm64
          # macOS, whose text exceeds the +/-128 MiB direct-branch range.
          # releaseFlags = lib.optionalString pkgs.stdenv.hostPlatform.isDarwin "-C code-model=large";
        in
        {
          default = naersk'.buildPackage {
            # naersk names a workspace build `rust-workspace` without `name`.
            name = "omnigraph-studio";
            version = (builtins.fromTOML (builtins.readFile ./Cargo.toml)).workspace.package.version;
            src = ./.;

            # The packages and features of the release archives
            # (build-release-archives.yml).
            cargoBuildOptions = options: options ++ [
              # "--locked"
              # "-p" "omnigraph-studio"
            ];

            nativeBuildInputs = bundle pkgs;

            # .cargo/config.toml links Linux through scripts/cargo-linker.sh,
            # which the dependency build's dummy source does not hold (naersk
            # copies only the manifests, the lock file and the Cargo config).
            # Cargo reads these over the file, and naersk passes them to both
            # derivations; `cc` is what the script runs when mold is absent,
            # as it is here.

            # CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_LINKER = "cc";
            # CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_LINKER = "cc";

            # A set RUSTFLAGS replaces the workspace `[build] rustflags`, the
            # test suites' `--cfg tokio_unstable`, as in every release build.
            # naersk would put its path remapping in CARGO_BUILD_RUSTFLAGS,
            # which Cargo merges with that list instead, so the remapping is
            # spelled here. It keeps the vendored sources out of the closure.

            # remapPathPrefix = false;
            # preBuild = ''
            #   export RUSTFLAGS="--remap-path-prefix $cratesio_sources=/sources --remap-path-prefix $git_sources=/sources ${releaseFlags}"
            # '';

            # Stamped on the top-level build only, so a new commit does not
            # rebuild the dependencies. A dirty tree has no revision and,
            # like a source build, reports no commit.

            # overrideMain = _: lib.optionalAttrs (self ? rev) {
            #   OMNIGRAPH_SOURCE_VERSION = self.rev;
            # };

            meta = {
              description = "Omnigraph Studio";
              homepage = "https://github.com/ModernRelay/omnigraph-studio";
              license = lib.licenses.mit;
              mainProgram = "omnigraph-studio";
            };
          };
        });

      checks = forAllSystems (system: _: {
        default = self.packages.${system}.default;
      });
    };
}
