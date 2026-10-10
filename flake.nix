{
    description = "dim-go2-dash, a dimOS Desktop app: `nix build .#dimosApp` → bin/dimos-app-server (Rust backend + built React frontend); `nix build .#dimosApp-aarch64-linux` cross-builds for arm Linux from any machine";
    inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-25.05";
    inputs.rust-overlay = {
        url = "github:oxalica/rust-overlay";
        inputs.nixpkgs.follows = "nixpkgs";
    };
    nixConfig = {
        extra-substituters = [ "https://dimos-desktop.cachix.org" ];
        extra-trusted-public-keys = [ "dimos-desktop.cachix.org-1:A4P35aGJGmCan92LWyamtSFXMqaVE+VRFYnrJ8QMTeQ=" ];
    };
    outputs = { self, nixpkgs, rust-overlay }:
        let
            systems = [ "aarch64-darwin" "x86_64-darwin" "x86_64-linux" "aarch64-linux" ];
            forAll = f: nixpkgs.lib.genAttrs systems (system: f system nixpkgs.legacyPackages.${system});
        in {
            packages = forAll (system: pkgs:
                let
                    # a static musl binary linked by zig, so no Linux builder or cross gcc is needed; the frontend is plain JS
                    crossApp = frontend: arch:
                        let
                            target = "${arch}-unknown-linux-musl";
                            # 1.86 = nixpkgs' rustc; newer rustc passes aarch64 a linker flag zig rejects (--fix-cortex-a53-843419)
                            toolchain = (import nixpkgs { inherit system; overlays = [ (import rust-overlay) ]; })
                                .rust-bin.stable."1.86.0".minimal.override { targets = [ target ]; };
                            crossBackend = (pkgs.makeRustPlatform { cargo = toolchain; rustc = toolchain; }).buildRustPackage {
                                pname = "go2-dash-backend-${arch}-linux";
                                version = "0.2.0";
                                src = ./backend;
                                cargoLock.lockFile = ./backend/Cargo.lock;
                                nativeBuildInputs = [ pkgs.cargo-zigbuild pkgs.zig ];
                                # cargo-auditable's -Wl,--undefined is another flag zig's linker rejects
                                auditable = false;
                                # vendored-dbus: libdbus compiled in, since there is no target libdbus to link
                                buildPhase = ''
                                    export HOME=$TMPDIR ZIG_GLOBAL_CACHE_DIR=$TMPDIR/zig
                                    cargo zigbuild --release --offline --target ${target} --features vendored-dbus
                                '';
                                doCheck = false;
                                installPhase = "install -Dm755 target/${target}/release/dimos-app-server $out/bin/dimos-app-server";
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
                frontend = pkgs.buildNpmPackage {
                    pname = "go2-dash-frontend";
                    version = "0.2.0";
                    src = ./frontend;
                    # `nix build .#frontend` prints the right hash when package-lock.json changes
                    npmDepsHash = "sha256-fo99/1JEdfI8m4R2keoAe0ccA/xjoRZZsKWJ9RsXFFs=";
                    installPhase = "cp -r dist $out";
                };
                # Rust, not Deno: Bluetooth (CoreBluetooth / BlueZ) for discovery + Wi-Fi provisioning, and WebRTC to the robot
                backend = pkgs.rustPlatform.buildRustPackage {
                    pname = "go2-dash-backend";
                    version = "0.2.0";
                    src = ./backend;
                    cargoLock.lockFile = ./backend/Cargo.lock;
                    nativeBuildInputs = pkgs.lib.optionals pkgs.stdenv.isLinux [ pkgs.pkg-config ];
                    buildInputs = pkgs.lib.optionals pkgs.stdenv.isLinux [ pkgs.dbus ];
                    # the route tests run against the mock app: no Bluetooth, network or robot
                    doCheck = true;
                    # macOS: the server lives in "Go2 Ctrl.app", the app Location (Wi-Fi names) is granted to (src/macos_wifi.rs)
                    postInstall = pkgs.lib.optionalString pkgs.stdenv.isDarwin ''
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
