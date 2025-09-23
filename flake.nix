{
  inputs = {
    fedimint.url = "github:fedimint/fedimint?rev=464fb59f04ee1efdd6fc428ffb857e9419af84be";
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
