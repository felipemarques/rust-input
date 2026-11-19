self: {
  config,
  pkgs,
  lib,
  ...
}:
with lib; let
  cfg = config.programs.rustinput;
  defaultPackage = self.packages.${pkgs.stdenv.hostPlatform.system}.default;
  tomlFormat = pkgs.formats.toml {};
in {
  options.programs.rustinput = with types; {
    enable = mkEnableOption "Whether or not to enable rustinput.";
    package = mkOption {
      type = with types; nullOr package;
      default = defaultPackage;
      defaultText = literalExpression "inputs.rustinput.packages.${pkgs.stdenv.hostPlatform.system}.default";
      description = ''
        The rustinput package to use.

        By default, this option will use the `packages.default` as exposed by this flake.
      '';
    };
    systemd = mkOption {
      type = types.bool;
      default = pkgs.stdenv.hostPlatform.isLinux;
      description = "Whether to enable to systemd service for rustinput on linux.";
    };
    launchd = mkOption {
      type = types.bool;
      default = pkgs.stdenv.hostPlatform.isDarwin;
      description = "Whether to enable to launchd service for rustinput on macOS.";
    };
    settings = lib.mkOption {
      inherit (tomlFormat) type;
      default = {};
      example = builtins.fromTOML (builtins.readFile (self + /config.toml));
      description = ''
        Optional configuration written to {file}`$XDG_CONFIG_HOME/rustinput/config.toml`.

        See <https://github.com/felipemarques/rust-input/> for
        available options and documentation.
      '';
    };
  };

  config = mkIf cfg.enable {
    systemd.user.services.rustinput = lib.mkIf cfg.systemd {
      Unit = {
        Description = "Systemd service for RustInput";
        Requires = ["graphical-session.target"];
      };
      Service = {
        Type = "simple";
        ExecStart = "${cfg.package}/bin/rustinput daemon";
      };
      Install.WantedBy = [
        (lib.mkIf config.wayland.windowManager.hyprland.systemd.enable "hyprland-session.target")
        (lib.mkIf config.wayland.windowManager.sway.systemd.enable "sway-session.target")
      ];
    };

    launchd.agents.rustinput = lib.mkIf cfg.launchd {
      enable = true;
      config = {
        ProgramArguments = [
          "${cfg.package}/bin/rustinput"
          "daemon"
        ];
        KeepAlive = true;
      };
    };

    home.packages = [
      cfg.package
    ];

    xdg.configFile."rustinput/config.toml" = lib.mkIf (cfg.settings != {}) {
      source = tomlFormat.generate "config.toml" cfg.settings;
    };
  };
}
