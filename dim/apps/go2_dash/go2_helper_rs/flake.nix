{
  description = "go2_helper — Unitree Go2 discovery + wifi provisioning, for dim-go2-dash";

  inputs = {
    nixpkgs.url      = "github:NixOS/nixpkgs/nixpkgs-unstable";
    rust-overlay.url = "github:oxalica/rust-overlay";
    rust-overlay.inputs.nixpkgs.follows = "nixpkgs";
    flake-utils.url  = "github:numtide/flake-utils";
  };

  outputs = { self, nixpkgs, rust-overlay, flake-utils }:
    flake-utils.lib.eachDefaultSystem (system:
      let
        overlays = [ (import rust-overlay) ];
        pkgs = import nixpkgs { inherit system overlays; };
        lib = pkgs.lib;

        rustToolchain = pkgs.rust-bin.stable.latest.default.override {
          targets = [
            "x86_64-unknown-linux-musl"
            "aarch64-unknown-linux-musl"
          ];
        };

        rustPlatform = pkgs.makeRustPlatform {
          cargo = rustToolchain;
          rustc = rustToolchain;
        };

        # BLE backends: BlueZ (D-Bus, C libdbus) on Linux, CoreBluetooth on macOS.
        # macOS frameworks are provided automatically by the stdenv SDK on current
        # nixpkgs (the old darwin.apple_sdk.frameworks stubs were removed).
        commonArgs = {
          pname   = "go2_helper";
          version = "0.1.0";
          src     = ./.;
          cargoLock.lockFile = ./Cargo.lock;
          doCheck = false;
          meta.mainProgram = "go2_helper";
        };

        # ── native build for the current system (what `nix run` uses) ────────
        # Links libdbus dynamically on Linux (present on any BlueZ host) and the
        # CoreBluetooth framework on macOS.
        nativeBuild = rustPlatform.buildRustPackage (commonArgs // {
          nativeBuildInputs = lib.optionals pkgs.stdenv.isLinux [ pkgs.pkg-config ];
          buildInputs = lib.optionals pkgs.stdenv.isLinux [ pkgs.dbus ];
        });

        # ── portable static musl cross builds (Linux targets) ───────────────
        # Static-links libdbus so the result runs on any Linux without a runtime
        # dbus/glibc dependency. Only meaningful when building from Linux/macOS
        # for distribution; the runtime path uses `nativeBuild` via `nix run`.
        crossPkgs = target: import nixpkgs {
          inherit system overlays;
          crossSystem.config = target;
        };

        buildMuslCross = target:
          let
            cross      = crossPkgs target;
            targetSnake = builtins.replaceStrings ["-"] ["_"] target;
            targetUpper = lib.toUpper targetSnake;
            ccBinDir    = "${cross.stdenv.cc}/bin";
            ccPrefix    = cross.stdenv.cc.targetPrefix;

            # libdbus-sys only needs the client library. The full dbus package
            # enables daemon-only features (systemd activation, AppArmor, audit,
            # X11 autolaunch) whose transitive deps don't cross-compile from
            # darwin: systemd → util-linux → sqlite → tcl, and AppArmor → python3.
            # Strip them all — the client libdbus doesn't use any of them.
            leanDbus = (cross.dbus.override {
              x11Support = false;
              enableSystemd = false;
            }).overrideAttrs (old: {
              buildInputs = [ cross.expat cross.libcap_ng ];
              # The Rust musl target links fully static, so libdbus-sys needs a
              # libdbus-1.a (nixpkgs dbus ships only the shared lib by default).
              mesonFlags = (builtins.filter
                (f: f != "-Dapparmor=enabled" && f != "-Dlibaudit=enabled")
                old.mesonFlags)
                ++ [ "-Dapparmor=disabled" "-Dlibaudit=disabled"
                     "-Ddefault_library=static" ];
              doCheck = false;
            });
          in
          rustPlatform.buildRustPackage (commonArgs // {
            pname = "go2_helper-${target}";

            nativeBuildInputs = [ pkgs.pkg-config ];
            buildInputs = [ leanDbus ];

            buildPhase = ''
              runHook preBuild
              cargo build --release --target ${target}
              runHook postBuild
            '';
            installPhase = ''
              runHook preInstall
              mkdir -p $out/bin
              install -m755 target/${target}/release/go2_helper $out/bin/go2_helper
              runHook postInstall
            '';

            # Env vars go in `env` so they don't collide with the cross
            # stdenv's own env attrs (modern nixpkgs forbids overlap).
            env = {
              "CARGO_TARGET_${targetUpper}_LINKER" = "${ccBinDir}/${ccPrefix}cc";
              "CC_${targetSnake}"  = "${ccBinDir}/${ccPrefix}cc";
              "AR_${targetSnake}"  = "${ccBinDir}/${ccPrefix}ar";
              # Static libdbus + its deps, resolved by pkg-config for libdbus-sys.
              PKG_CONFIG_ALLOW_CROSS = "1";
              PKG_CONFIG_ALL_STATIC = "1";
              PKG_CONFIG_PATH = "${leanDbus.dev}/lib/pkgconfig";
            };
          });

      in {
        packages = {
          default     = nativeBuild;
          native      = nativeBuild;
          linux-x86   = buildMuslCross "x86_64-unknown-linux-musl";
          linux-arm64 = buildMuslCross "aarch64-unknown-linux-musl";
        };

        apps.default = {
          type = "app";
          program = "${nativeBuild}/bin/go2_helper";
        };

        devShells.default = pkgs.mkShell {
          buildInputs = [ rustToolchain pkgs.pkg-config ]
            ++ lib.optionals pkgs.stdenv.isLinux [ pkgs.dbus ];
        };
      }
    );
}
