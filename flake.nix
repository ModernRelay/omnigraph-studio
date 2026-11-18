{
  description = "Omnigraph Studio, a native desktop app for Omnigraph";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    crane.url = "github:ipetkov/crane";
    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
    flake-utils.url = "github:numtide/flake-utils";
  };

  outputs =
    {
      self,
      nixpkgs,
      crane,
      rust-overlay,
      flake-utils,
    }:
    # nixpkgs no longer builds for x86_64-darwin (26.11), so the systems are named, not defaulted.
    flake-utils.lib.eachSystem [ "x86_64-linux" "aarch64-linux" "aarch64-darwin" ] (
      system:
      let
        pkgs = import nixpkgs {
          inherit system;
          overlays = [ (import rust-overlay) ];
        };
        inherit (pkgs) lib stdenv;
        isLinux = stdenv.hostPlatform.isLinux;

        # The toolchain rust-toolchain.toml pins, so Nix and rustup builds use the same compiler.
        craneLib = (crane.mkLib pkgs).overrideToolchain (
          p: p.rust-bin.fromRustupToolchainFile ./rust-toolchain.toml
        );

        version = (lib.importTOML ./Cargo.toml).workspace.package.version;

        # Everything a build reads: the crates, the grammars' C sources, and the files the crates
        # embed with include_str!/include_bytes! (assets, resources, OpenAPI contracts, skills,
        # the dev schema). Not specs, scripts or target/, so editing those rebuilds nothing.
        src = lib.fileset.toSource {
          root = ./.;
          fileset = lib.fileset.unions [
            ./Cargo.toml
            ./Cargo.lock
            ./.cargo
            ./crates
            ./grammars
            ./assets
            ./resources
            ./dev/cluster/knowledge.pg
            ./docs/deferred-engine-changes.md
          ];
        };

        # Crane vendors each crate of a git dependency as its own directory. Zed's crates read
        # files from the repository around them (`../../assets` for rust-embed and fonts,
        # `../../zed/RELEASE_CHANNEL`), which that layout drops: the `assets` crate then fails
        # with "no function `iter` found for `Assets`". So for Zed's checkout, put each vendored
        # crate back at its path in the repository, link the vendored name to it, and copy the
        # rest of the repository (without .git) around it, so those relative paths resolve.
        restoreGitLayout = ''
          repo=$PWD
          cargo metadata --format-version 1 --no-deps --offline --manifest-path "$repo/Cargo.toml" |
            jq -r '.packages[] | "\(.name)-\(.version) \(.manifest_path)"' |
            while read -r crate manifest; do
              [ -d "$out/$crate" ] && [ ! -L "$out/$crate" ] || continue
              rel=$(realpath --relative-to="$repo" "$(dirname "$manifest")")
              mkdir -p "$out/$(dirname "$rel")"
              mv "$out/$crate" "$out/$rel"
              ln -s "$rel" "$out/$crate"
            done
          # The vendored copies keep their resolved Cargo.toml; this only adds what they lack.
          cp -r --update=none --no-preserve=mode \
            $(find "$repo" -mindepth 1 -maxdepth 1 ! -name .git) "$out/"
        '';

        cargoVendorDir = craneLib.vendorCargoDeps {
          inherit src;
          overrideVendorGitCheckout =
            ps: drv:
            if lib.any (p: lib.hasPrefix "git+https://github.com/zed-industries/zed?" p.source) ps then
              drv.overrideAttrs (old: {
                postInstall = (old.postInstall or "") + restoreGitLayout;
              })
            else
              drv;
        };

        # Libraries GPUI opens with dlopen at run time, so linking alone does not keep them.
        runtimeLibs = with pkgs; [
          vulkan-loader
          wayland
          libxkbcommon
          libx11
          libxcb
        ];

        commonArgs = {
          pname = "omnigraph-studio";
          inherit version src cargoVendorDir;
          strictDeps = true;
          doCheck = false;
          cargoExtraArgs = "--locked -p studio --bin omnigraph-studio";

          # CMake builds tree-sitter's wasm runtime (AGENTS.md); it is not this project's build
          # system, so its configure hook stays off.
          nativeBuildInputs = with pkgs; [
            cmake
            perl
            pkg-config
          ];
          dontUseCmakeConfigure = true;

          buildInputs =
            with pkgs;
            [
              fontconfig
              freetype
              openssl
              sqlite
              zlib
              zstd
            ]
            ++ lib.optionals isLinux (
              runtimeLibs
              ++ (with pkgs; [
                alsa-lib
                libxcursor
                libxi
                libxrandr
              ])
            )
            ++ lib.optionals stdenv.hostPlatform.isDarwin [
              pkgs.apple-sdk_26
              (pkgs.darwinMinVersionHook "12.0")
            ];

          env = {
            ZSTD_SYS_USE_PKG_CONFIG = true;
          }
          // lib.optionalAttrs isLinux {
            NIX_LDFLAGS = "-rpath ${lib.makeLibraryPath runtimeLibs}";
          };
        };

        cargoArtifacts = craneLib.buildDepsOnly commonArgs;

        omnigraph-studio = craneLib.buildPackage (
          commonArgs
          // {
            inherit cargoArtifacts;
            # Keep the rpath above: patchelf would drop it, as nothing links those libraries.
            dontPatchELF = isLinux;
            postInstall = lib.optionalString isLinux ''
              install -Dm644 resources/linux/omnigraph-studio.desktop.in \
                $out/share/applications/omnigraph-studio.desktop
              install -Dm644 resources/linux/omnigraph-studio.png \
                $out/share/icons/hicolor/512x512/apps/omnigraph-studio.png
            '';
            meta = {
              description = "Native desktop app for Omnigraph";
              homepage = "https://github.com/ModernRelay/omnigraph-studio";
              license = lib.licenses.gpl3Plus;
              mainProgram = "omnigraph-studio";
              platforms = [
                "x86_64-linux"
                "aarch64-linux"
                "aarch64-darwin"
              ];
            };
          }
        );
      in
      {
        # nix build
        packages = {
          inherit omnigraph-studio;
          default = omnigraph-studio;
        };

        # nix run
        apps.default = flake-utils.lib.mkApp { drv = omnigraph-studio; };

        # nix develop: the pinned toolchain and the build's inputs, for `cargo build` and friends.
        devShells.default = craneLib.devShell (
          {
            inputsFrom = [ omnigraph-studio ];
          }
          // lib.optionalAttrs isLinux {
            # A `cargo build` binary has no rpath to them, unlike the package's.
            LD_LIBRARY_PATH = lib.makeLibraryPath runtimeLibs;
          }
        );
      }
    );
}
