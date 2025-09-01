{
  description = "Kubernetes controller for dynamically scaling Nix remote builders";

  inputs = {
    nixpkgs.url = "github:nixos/nixpkgs?ref=nixos-unstable";
    flake-utils.url = "github:numtide/flake-utils";
  };

  outputs =
    {
      self,
      nixpkgs,
      flake-utils,
    }:
    flake-utils.lib.eachDefaultSystem (
      system:
      let
        pkgs = nixpkgs.legacyPackages.${system};
        version = "0.1.0";
      in
      {
        packages = {
          controller = pkgs.buildGoModule {
            pname = "nix-remote-build-controller";
            inherit version;
            src = ./.;
            vendorHash = "sha256-hpAsYPhiYnTpY5Z7QZz9cr5RtleHnR1ezgoVaQ+cvp0=";
            subPackages = [ "cmd/controller" ];
            ldflags = [
              "-s"
              "-w"
              "-X main.version=${version}"
            ];
          };

          proxy = pkgs.buildGoModule {
            pname = "nix-remote-build-proxy";
            inherit version;
            src = ./.;
            vendorHash = "sha256-hpAsYPhiYnTpY5Z7QZz9cr5RtleHnR1ezgoVaQ+cvp0=";
            subPackages = [ "cmd/proxy" ];
            ldflags = [
              "-s"
              "-w"
              "-X main.version=${version}"
            ];
          };

          default = self.packages.${system}.controller;
        };

        devShells.default = pkgs.mkShell {
          buildInputs = with pkgs; [
            go
            golangci-lint
            nixfmt-rfc-style
          ];
        };

        apps = {
          controller = flake-utils.lib.mkApp {
            drv = self.packages.${system}.controller;
          };
          proxy = flake-utils.lib.mkApp {
            drv = self.packages.${system}.proxy;
          };
          default = self.apps.${system}.controller;
        };
      }
    );
}
