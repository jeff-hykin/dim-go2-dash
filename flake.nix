{
    description = "dim-go2-dash: Unitree Go2 discovery + wifi provisioning, as a dimOS Desktop app";

    inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-25.05";
    inputs.dim-app.url = "github:jeff-hykin/dim-app/v0.4.0";

    outputs = { self, nixpkgs, dim-app }: {
        # the Go2 helper is a shipped prebuilt per system (go2_helper_rs/bin), which main.js finds beside itself in the store
        packages = dim-app.lib.forAllSystems nixpkgs (pkgs: {
            dimosApp = dim-app.lib.mkDimosApp {
                inherit pkgs;
                name = "dim-go2-dash";
                src = self;
                frontend = "dim/apps/go2_dash/frontend";
                backend = "dim/apps/go2_dash/main.js";
            };
        });
    };
}
