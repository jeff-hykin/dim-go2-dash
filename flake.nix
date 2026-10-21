{
    description = "dim-go2-dash, a dimOS Desktop app: `nix build .#dimosApp` → bin/dimos-app-server (Rust backend + built React frontend); `nix build .#dimosApp-aarch64-linux` cross-builds for arm Linux from any machine";
    inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-25.05";
    # the frontend's deno: nixos-25.11's 2.6.10, the version dimos pins (newer deno refuses packages younger than a day)
    inputs.nixpkgs-deno.url = "github:NixOS/nixpkgs/nixos-25.11";
    inputs.rust-overlay = {
        url = "github:oxalica/rust-overlay";
        inputs.nixpkgs.follows = "nixpkgs";
    };
    # deps build once into their own derivation (keyed on Cargo.toml / Cargo.lock), so a commit that only edits our code rebuilds just our crate
    # (v0.21: the newest crane that takes nixos-25.05)
    inputs.crane.url = "github:ipetkov/crane/v0.21.3";
    nixConfig = {
        extra-substituters = [ "https://dimos-desktop.cachix.org" ];
        extra-trusted-public-keys = [ "dimos-desktop.cachix.org-1:A4P35aGJGmCan92LWyamtSFXMqaVE+VRFYnrJ8QMTeQ=" ];
    };
    outputs = { self, nixpkgs, nixpkgs-deno, rust-overlay, crane }:
        let
            systems = [ "aarch64-darwin" "x86_64-darwin" "x86_64-linux" "aarch64-linux" ];
            forAll = f: nixpkgs.lib.genAttrs systems (system: f system nixpkgs.legacyPackages.${system});
        in {
            packages = forAll (system: pkgs:
                let
                    # git dependencies (dimos' recorder, dimos-lcm's LCM types): `nix build` prints a hash when its rev changes
                    # vendored by nixpkgs (crane's own fetcher would pull dimos's LFS data), then handed to crane as a vendor dir
                    vendorDir = pkgs.rustPlatform.importCargoLock {
                        lockFile = ./backend/Cargo.lock;
                        outputHashes = {
                            "dimos-memory-recorder-0.1.0" = "sha256-U4KazHTvsmHcnWUEJ0Yg5gHJi/giL0KV8zUTamwMPhM=";
                            "dimos-lcm-0.1.0" = "sha256-GGkx4Mn6NYP6KZecmoRLKGWIih/+y8OgNn12DeXX6n8=";
                        };
                    };
                    cargoVendorDir = pkgs.runCommand "go2-dash-vendor" { } ''
                        mkdir $out
                        sed 's|^directory = "cargo-vendor-dir"$|directory = "${vendorDir}"|' ${vendorDir}/.cargo/config.toml > $out/config.toml
                    '';
                    # crane: buildDepsOnly compiles every dependency against a dummy of our crate (so its hash only moves with Cargo.toml / Cargo.lock), buildPackage starts from that target dir
                    rustBuild = { toolchain, pname, extra ? { }, package ? { } }:
                        let
                            craneLib = (crane.mkLib pkgs).overrideToolchain (_: toolchain);
                            common = {
                                inherit pname cargoVendorDir;
                                version = "0.2.0";
                                src = ./backend;
                                strictDeps = true;
                                doCheck = false;
                                # cmake is only for the recorder's libjpeg-turbo (its build script runs it), not a configure step of ours
                                dontUseCmakeConfigure = true;
                            } // extra;
                            cargoArtifacts = craneLib.buildDepsOnly (common // {
                                # only the build's artifacts are reused; a `cargo check` pass would be a second compile nobody reads
                                cargoCheckCommand = "true";
                            });
                        in
                        craneLib.buildPackage (common // {
                            inherit cargoArtifacts;
                            # nothing builds on this target dir, so don't archive it into $out
                            doInstallCargoArtifacts = false;
                        } // package);
                    # 1.89: dimos' recorder needs it (nalgebra, validator), past nixos-25.05's 1.86
                    rust = (import nixpkgs { inherit system; overlays = [ (import rust-overlay) ]; }).rust-bin.stable."1.89.0";
                    # the recorder's TurboJPEG builds libjpeg-turbo with CMake (and NASM for x86 SIMD)
                    jpegTools = [ pkgs.cmake pkgs.nasm ];
                    # a static musl binary linked by zig, so no Linux builder or cross gcc is needed; the frontend is plain JS
                    crossApp = frontend: arch:
                        let
                            target = "${arch}-unknown-linux-musl";
                            envTarget = builtins.replaceStrings [ "-" ] [ "_" ] target;
                            # (nixos-25.05's zigbuild 0.20 writes them straight into cargo-zigbuild/<version>/, newer ones under wrappers/<hash>/)
                            zigShim = wrapper: pkgs.writeShellScript "zigbuild-shim" ''
                                for f in "$CARGO_ZIGBUILD_CACHE_DIR"/cargo-zigbuild/*/${wrapper} "$CARGO_ZIGBUILD_CACHE_DIR"/cargo-zigbuild/*/wrappers/*/${wrapper}; do
                                    [ -e "$f" ] && exec "$f" "$@"
                                done
                                echo "zigbuild-shim: no ${wrapper} under $CARGO_ZIGBUILD_CACHE_DIR" >&2
                                exit 127
                            '';
                            cc = zigShim "zigcc-${target}-*.sh";
                            cxx = zigShim "zigcxx-${target}-*.sh";
                            ar = zigShim "ar";
                            ranlib = zigShim "zigranlib.sh";
                            # zigbuild's own CMake toolchain file is per build too (libjpeg-turbo's build would rerun), so this one, on the same shims
                            cmakeToolchain = pkgs.writeText "${target}-toolchain.cmake" ''
                                set(CMAKE_SYSTEM_NAME Linux)
                                set(CMAKE_SYSTEM_PROCESSOR ${arch})
                                set(CMAKE_C_COMPILER ${cc})
                                set(CMAKE_CXX_COMPILER ${cxx})
                                set(CMAKE_AR ${ar})
                                set(CMAKE_RANLIB ${ranlib})
                                set(CMAKE_C_LINKER_DEPFILE_SUPPORTED FALSE)
                                set(CMAKE_CXX_LINKER_DEPFILE_SUPPORTED FALSE)
                                set(CMAKE_FIND_ROOT_PATH_MODE_PROGRAM NEVER)
                                set(CMAKE_FIND_ROOT_PATH_MODE_LIBRARY ONLY)
                                set(CMAKE_FIND_ROOT_PATH_MODE_INCLUDE ONLY)
                                set(CMAKE_FIND_ROOT_PATH_MODE_PACKAGE ONLY)
                            '';
                            toolchain = rust.minimal.override { targets = [ target ]; };
                            crossBackend = rustBuild {
                                inherit toolchain;
                                pname = "go2-dash-backend-${arch}-linux";
                                extra = {
                                    # vendored-dbus: libdbus compiled in, since there is no target libdbus to link
                                    cargoExtraArgs = "--locked --target ${target} --features vendored-dbus";
                                    nativeBuildInputs = [ pkgs.cargo-zigbuild pkgs.zig ] ++ jpegTools;
                                    cargoBuildCommand = "cargo zigbuild --profile release";
                                    # cargo fingerprints the linker, CC, AR and CMake toolchain paths, so they are nix-store shims (the same in every build, whichever nixbld user runs it) that hand off to the wrappers zigbuild writes in this build's own $TMPDIR; zigbuild leaves a CC / linker it finds set alone
                                    # (not one shared dir: zigbuild writes its wrappers 0700, so a second nixbld user, as in CI's parallel builds, can't run them)
                                    preBuild = ''
                                        export HOME=$TMPDIR ZIG_GLOBAL_CACHE_DIR=$TMPDIR/zig CARGO_ZIGBUILD_CACHE_DIR=$TMPDIR/zigbuild
                                        export CC_${envTarget}=${cc} CXX_${envTarget}=${cxx} AR_${envTarget}=${ar} RANLIB_${envTarget}=${ranlib}
                                        export CARGO_TARGET_${pkgs.lib.toUpper envTarget}_LINKER=${cc} CMAKE_TOOLCHAIN_FILE_${envTarget}=${cmakeToolchain}
                                    '';
                                };
                                package.installPhaseCommand = "install -Dm755 target/${target}/release/dimos-app-server $out/bin/dimos-app-server";
                            };
                            linux = nixpkgs.legacyPackages."${arch}-linux";
                        in
                        # its shell is the target's (a cache.nixos.org download, nothing built)
                        pkgs.writeTextFile {
                            name = "dimos-app-server-${arch}-linux";
                            destination = "/bin/dimos-app-server";
                            executable = true;
                            text = "#!${linux.runtimeShell}\nexec ${crossBackend}/bin/dimos-app-server --frontend ${frontend} \"$@\"\n";
                        };
                in rec {
                deno = nixpkgs-deno.legacyPackages.${system}.deno;
                denoEnv = "export HOME=$TMPDIR DENO_DIR=$TMPDIR/deno DENO_NO_UPDATE_CHECK=1 DENO_NO_PROMPT=1";
                frontendFiles = paths: pkgs.lib.fileset.toSource { root = ./frontend; fileset = pkgs.lib.fileset.unions paths; };
                # `deno install --frozen` (deno.lock pins every package) with network, so fixed-output. Its hash is per
                # build machine (vite's native rollup and esbuild differ by OS and CPU): `deno task update-hashes` refreshes
                # this machine's after frontend/deno.json or deno.lock change.
                frontendModules = pkgs.stdenvNoCC.mkDerivation {
                    name = "go2-dash-frontend-modules";
                    src = frontendFiles [ ./frontend/deno.json ./frontend/deno.lock ];
                    nativeBuildInputs = [ deno ];
                    buildPhase = ''
                        ${denoEnv}
                        export SSL_CERT_FILE=${pkgs.cacert}/etc/ssl/certs/ca-bundle.crt
                        deno install --frozen
                        # deno's own record of the install: not needed, and not byte-identical between installs
                        rm -f node_modules/.deno/.setup-cache.bin
                    '';
                    installPhase = "cp -R node_modules $out";
                    dontFixup = true;
                    outputHashMode = "recursive";
                    outputHashAlgo = "sha256";
                    outputHash = {
                        aarch64-darwin = "sha256-ujwfB0uQ17HeD2t2aMXWD81JLgE8ydF4b+NlzZOMWtA=";
                        x86_64-darwin = "";
                        x86_64-linux = "sha256-wGV7tTDAVU10wWkKU1PIm1K8Susp/MjbRZyQiUHam6k=";
                        aarch64-linux = "";
                    }.${system};
                };
                # the page: React + Vite against those modules, offline
                frontend = pkgs.stdenvNoCC.mkDerivation {
                    name = "go2-dash-frontend";
                    src = frontendFiles [ ./frontend/deno.json ./frontend/deno.lock ./frontend/index.html ./frontend/vite.config.ts ./frontend/src ];
                    nativeBuildInputs = [ deno ];
                    buildPhase = ''
                        ${denoEnv}
                        cp -R ${frontendModules} node_modules
                        chmod -R u+w node_modules
                        deno run -A --cached-only --node-modules-dir=manual npm:vite build
                    '';
                    installPhase = "cp -R dist $out";
                };
                # Rust, not Deno: Bluetooth (CoreBluetooth / BlueZ) for discovery + Wi-Fi provisioning, and WebRTC to the robot
                backend = rustBuild {
                    toolchain = rust.minimal;
                    pname = "go2-dash-backend";
                    extra = {
                        cargoExtraArgs = "--locked";
                        nativeBuildInputs = jpegTools ++ pkgs.lib.optionals pkgs.stdenv.isLinux [ pkgs.pkg-config ];
                        buildInputs = pkgs.lib.optionals pkgs.stdenv.isLinux [ pkgs.dbus ];
                        # the route tests run against the mock app: no Bluetooth, network or robot (their dev-dependencies are in the deps derivation too)
                        doCheck = true;
                    };
                    # macOS: the server lives in "Go2 Ctrl.app", the app Location (Wi-Fi names) is granted to (src/macos_wifi.rs)
                    package.postInstall = pkgs.lib.optionalString pkgs.stdenv.isDarwin ''
                        app="$out/Go2 Ctrl.app/Contents"
                        mkdir -p "$app/MacOS"
                        mv $out/bin/dimos-app-server "$app/MacOS/dimos-app-server"
                        cp macos/Info.plist "$app/Info.plist"
                        ln -s "$app/MacOS/dimos-app-server" $out/bin/dimos-app-server
                    '';
                };
                # on macOS straight to the binary in its bundle (not the bin/ link), so it finds the bundle it runs from
                dimosApp = pkgs.writeShellScriptBin "dimos-app-server" ''
                    exec "${backend}/${if pkgs.stdenv.isDarwin then "Go2 Ctrl.app/Contents/MacOS" else "bin"}/dimos-app-server" --frontend ${frontend} "$@"
                '';
                default = dimosApp;
                dimosApp-aarch64-linux = crossApp frontend "aarch64";
                dimosApp-x86_64-linux = crossApp frontend "x86_64";
            });
        };
}
