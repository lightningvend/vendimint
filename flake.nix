{
  inputs = {
    fedimint.url = "github:fedimint/fedimint?rev=2620789610a2c65c1068de973ebb5657d08d549d";
    flake-utils.url = "github:numtide/flake-utils";
  };

  outputs = { self, fedimint, flake-utils, ... }:
    flake-utils.lib.eachDefaultSystem (system:
      let
        # Import the `devShells` from the fedimint flake
        devShells = fedimint.devShells.${system};
      in {
        devShells = {
          # You can expose all or specific shells from the original flake
          default = devShells.default.overrideAttrs (old: {
            nativeBuildInputs = old.nativeBuildInputs or [] ++ [
              fedimint.packages.${system}.devimint
              fedimint.packages.${system}.gateway-pkgs
            ];
          });
        };
      }
    );
}
