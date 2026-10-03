{
    description = "dim-go2-dash, a dimOS Desktop app: `nix build .#dimosApp` → bin/dimos-app-server (Rust backend + built React frontend)";
    inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-25.05";
    nixConfig = {
        extra-substituters = [ "https://dimos-desktop.cachix.org" ];
        extra-trusted-public-keys = [ "dimos-desktop.cachix.org-1:A4P35aGJGmCan92LWyamtSFXMqaVE+VRFYnrJ8QMTeQ=" ];
    };
    outputs = { self, nixpkgs }:
        let
            systems = [ "aarch64-darwin" "x86_64-darwin" "x86_64-linux" "aarch64-linux" ];
            forAll = f: nixpkgs.lib.genAttrs systems (system: f nixpkgs.legacyPackages.${system});
        in {
            packages = forAll (pkgs: rec {
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
                };
                dimosApp = pkgs.writeShellScriptBin "dimos-app-server" ''
                    exec ${backend}/bin/dimos-app-server --frontend ${frontend} "$@"
                '';
                default = dimosApp;
            });
        };
}
