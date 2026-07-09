{
  description = "hookguard — the trust-first git hook manager";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
  };

  outputs = { self, nixpkgs }:
    let
      systems = [ "x86_64-linux" "aarch64-linux" "x86_64-darwin" "aarch64-darwin" ];
      forAllSystems = f: nixpkgs.lib.genAttrs systems (system: f system);
    in
    {
      packages = forAllSystems (system:
        let
          pkgs = nixpkgs.legacyPackages.${system};
        in
        {
          default = pkgs.rustPlatform.buildRustPackage {
            pname = "hookguard";
            version = "0.1.0";
            src = ./.;
            cargoLock.lockFile = ./Cargo.lock;

            # e2e tests shell out to `git`, use `setsid` to drop the
            # controlling terminal for no-tty consent-path tests, and use
            # `ssh-keygen` for the signed-trust tests.
            nativeCheckInputs = [ pkgs.git pkgs.util-linux pkgs.openssh ];

            meta = with pkgs.lib; {
              description = "Trust-first git hook manager — hooks auto-install on clone, nothing runs without your consent";
              homepage = "https://github.com/itayw/hookguard";
              license = with licenses; [ mit asl20 ];
              mainProgram = "git-hooks";
            };
          };
        });

      apps = forAllSystems (system: {
        default = {
          type = "app";
          program = "${self.packages.${system}.default}/bin/git-hooks";
        };
      });
    };
}
