{
  description = "cursor-sdk-rs: Rust SDK and seat for Cursor agents (sdk.v1)";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
  };

  outputs =
    { self, nixpkgs }:
    let
      systems = [
        "aarch64-darwin"
        "aarch64-linux"
        "x86_64-linux"
      ];
      forAllSystems = nixpkgs.lib.genAttrs systems;
    in
    {
      overlays.default = final: prev: {
        cursor-sdk-bridge = final.callPackage ./nix/cursor-sdk-bridge.nix { };
        cursor-seat = final.callPackage ./nix/cursor-seat.nix {
          cursor-sdk-bridge = final.cursor-sdk-bridge;
        };
      };

      packages = forAllSystems (
        system:
        let
          pkgs = import nixpkgs {
            inherit system;
            overlays = [ self.overlays.default ];
          };
        in
        {
          cursor-sdk-bridge = pkgs.cursor-sdk-bridge;
          cursor-seat = pkgs.cursor-seat;
          default = pkgs.cursor-seat;
        }
      );

      devShells = forAllSystems (
        system:
        let
          pkgs = import nixpkgs {
            inherit system;
            overlays = [ self.overlays.default ];
          };
          bridge = pkgs.cursor-sdk-bridge;
        in
        {
          default = pkgs.mkShell {
            inputsFrom = [ pkgs.cursor-seat ];
            packages = with pkgs; [
              cargo
              rustc
              clippy
              rustfmt
              rust-analyzer
            ];
            shellHook = ''
              export CURSOR_SDK_BRIDGE_BIN="${bridge}/bin/cursor-sdk-bridge"
            '';
          };
        }
      );

      checks = forAllSystems (
        system:
        let
          pkgs = nixpkgs.legacyPackages.${system};
        in
        {
          nix-files = pkgs.runCommand "cursor-sdk-rs-nix-files" { } ''
            test -f ${./nix/cursor-sdk-bridge.nix}
            test -f ${./nix/cursor-seat.nix}
            printf 'ok\n' > "$out"
          '';
        }
      );

      formatter = forAllSystems (system: nixpkgs.legacyPackages.${system}.nixfmt);
    };
}
