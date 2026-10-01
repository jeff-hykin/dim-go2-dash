{
    description = "dim-go2-dash: Unitree Go2 discovery + wifi provisioning, as a dimOS Desktop app";

    inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-25.05";

    outputs = { self, nixpkgs }:
        let
            systems = [ "aarch64-darwin" "x86_64-darwin" "x86_64-linux" "aarch64-linux" ];
            forAllSystems = f: nixpkgs.lib.genAttrs systems (system: f system nixpkgs.legacyPackages.${system});
            # the shipped helper binary main.js picks for each system (go2_helper-<Deno.build.os>-<Deno.build.arch>)
            shippedName = {
                aarch64-darwin = "go2_helper-darwin-aarch64";
                x86_64-darwin = "go2_helper-darwin-x86_64";
                x86_64-linux = "go2_helper-linux-x86_64";
                aarch64-linux = "go2_helper-linux-aarch64";
            };
        in {
            apps = forAllSystems (system: pkgs: {
                install = {
                    type = "app";
                    program = toString (pkgs.writeShellScript "install" ''
                        set -e
                        app=dim/apps/go2_dash
                        helper=$app/go2_helper_rs
                        # fetch the backend's remote imports now so the first start is fast
                        ${pkgs.deno}/bin/deno cache --no-lock "$app/main.js"
                        # the Go2 helper: use the shipped binary for this system, else build it now (not lazily on first scan).
                        # Not test-run here: starting it probes Bluetooth, which pops the macOS permission prompt.
                        shipped="$helper/bin/${shippedName.${system}}"
                        if [ -x "$shipped" ]; then
                            echo "dim-go2-dash: using shipped helper $shipped"
                        else
                            echo "dim-go2-dash: no shipped helper for ${system}, building go2_helper with nix"
                            nix --extra-experimental-features "nix-command flakes" build -L "path:$PWD/$helper" -o "$helper/result"
                            echo "dim-go2-dash: built $helper/result/bin/go2_helper"
                        fi
                    '');
                };
            });
        };
}
