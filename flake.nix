{
  description = "Run OpenCode in a Podman sandbox";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-26.05";

  outputs =
    { self, nixpkgs }:
    let
      lib = nixpkgs.lib;
      systems = [
        "x86_64-linux"
        "aarch64-linux"
      ];
      forAll = f: lib.genAttrs systems (system: f nixpkgs.legacyPackages.${system});

      mkPackage =
        pkgs:
        pkgs.rustPlatform.buildRustPackage {
          pname = "opencode-sandbox";
          version = (pkgs.lib.importTOML ./Cargo.toml).package.version;
          src = pkgs.lib.cleanSource ./.;
          cargoLock.lockFile = ./Cargo.lock;

          nativeBuildInputs = [ pkgs.makeWrapper ];

          # The CLI shells out to `podman`; a user-service PATH is minimal, so
          # pin the client into the wrapper rather than relying on the host
          # environment.
          postInstall = ''
            wrapProgram $out/bin/opencode-sandbox \
              --prefix PATH : ${pkgs.lib.makeBinPath [ pkgs.podman ]}
          '';

          meta = {
            description = "Run OpenCode in a Podman container with the workspace mounted";
            mainProgram = "opencode-sandbox";
          };
        };
    in
    {
      packages = forAll (pkgs: {
        default = mkPackage pkgs;
      });

      devShells = forAll (pkgs: {
        default = pkgs.mkShell {
          packages = with pkgs; [
            cargo
            rustc
            rustfmt
            clippy
          ];
        };
      });

      # Optional Home Manager integration. `programs.opencode-sandbox.enable`
      # installs the CLI; `programs.opencode-sandbox.build.enable` additionally
      # installs the daily image-rebuild service and timer.
      homeManagerModules.default =
        {
          config,
          lib,
          pkgs,
          ...
        }:
        let
          cfg = config.programs.opencode-sandbox;

          serviceName = "opencode-sandbox-build";
        in
        {
          options.programs.opencode-sandbox = {
            enable = lib.mkEnableOption "opencode-sandbox";

            package = lib.mkOption {
              type = lib.types.package;
              default = self.packages.${pkgs.stdenv.hostPlatform.system}.default;
              defaultText = lib.literalExpression "opencode-sandbox.packages.<system>.default";
              description = "The opencode-sandbox package to install.";
            };

            build = {
              enable = lib.mkEnableOption "the periodic opencode-sandbox image rebuild";

              calendar = lib.mkOption {
                type = lib.types.str;
                default = "daily";
                description = "systemd OnCalendar expression for the image rebuild.";
              };
            };
          };

          config = lib.mkMerge [
            (lib.mkIf (cfg.enable || cfg.build.enable) {
              home.packages = [ cfg.package ];
            })

            (lib.mkIf cfg.build.enable {
              systemd.user.services.${serviceName} = {
                Unit = {
                  Description = "Build opencode-sandbox Podman images (feature variants)";
                  After = [ "network-online.target" ];
                  Wants = [ "network-online.target" ];
                  StartLimitBurst = 3;
                  StartLimitIntervalSec = "10min";
                };

                Service = {
                  Type = "oneshot";
                  ExecStart = "${cfg.package}/bin/opencode-sandbox --build";
                  TimeoutStartSec = "1h";
                  Nice = 10;
                  IOSchedulingClass = "idle";
                  Restart = "on-failure";
                  RestartSec = "30s";
                };

                Install.WantedBy = [ "default.target" ];
              };

              systemd.user.timers.${serviceName} = {
                Unit.Description = "Periodically rebuild opencode-sandbox Podman images";

                Timer = {
                  Unit = "${serviceName}.service";
                  OnCalendar = cfg.build.calendar;
                  Persistent = true;
                };

                Install.WantedBy = [ "timers.target" ];
              };
            })
          ];
        };
    };
}
