# Home Manager module for Cybar.
#
# The flake exposes this as `homeManagerModules.default`. The flake itself is
# passed in as `self`, so `programs.cybar.package` can default to the package
# built from the very same flake revision.
self:
{
  config,
  lib,
  pkgs,
  ...
}:
let
  inherit (lib)
    mkEnableOption
    mkIf
    mkOption
    types
    ;

  cfg = config.programs.cybar;

  # Cybar reads $XDG_CONFIG_HOME/cybar/config.lua by default, which is exactly
  # where `xdg.configFile` writes, so no `--config` flag is needed.
  configKey = "cybar/config.lua";
  hasConfig = cfg.config != null || cfg.configFile != null;
in
{
  options.programs.cybar = {
    enable = mkEnableOption "Cybar, a configurable X11 status bar scripted in Lua";

    package = mkOption {
      type = types.package;
      default = self.packages.${pkgs.stdenv.hostPlatform.system}.default;
      defaultText = lib.literalExpression "cybar.packages.\${pkgs.stdenv.hostPlatform.system}.default";
      example = lib.literalExpression "cybar.packages.\${pkgs.stdenv.hostPlatform.system}.cybar-lua54";
      description = ''
        The Cybar package to install. The default uses the LuaJIT runtime; use
        the `cybar-lua54` package for the Lua 5.4 runtime.
      '';
    };

    config = mkOption {
      type = types.nullOr types.lines;
      default = null;
      example = ''
        local bar = require("cybar")

        return {
          bars = {{
            name = "main", monitor = "primary", position = "top",
            size = { thickness = 28, length = "100%" },
            sections = {
              left = { bar.workspaces{}, bar.window_title{ max_length = 32 } },
              middle = { bar.clock{ format = "%a %d %b  %H:%M" } },
              right = { bar.cpu{}, bar.ram{}, bar.systray{} },
            },
          }},
        }
      '';
      description = ''
        Contents of `$XDG_CONFIG_HOME/cybar/config.lua`, written as Lua.
        Mutually exclusive with {option}`programs.cybar.configFile`.

        If both are null, Home Manager does not manage the configuration and
        you provide `config.lua` yourself.

        Extra Lua modules or icons that your configuration needs can be
        added with `xdg.configFile."cybar/<name>".source`.
      '';
    };

    configFile = mkOption {
      type = types.nullOr types.path;
      default = null;
      example = lib.literalExpression "./cybar/config.lua";
      description = ''
        Path to an existing Lua configuration file to install as
        `$XDG_CONFIG_HOME/cybar/config.lua`. Mutually exclusive with
        {option}`programs.cybar.config`.
      '';
    };

    systemd = {
      enable = mkOption {
        type = types.bool;
        default = true;
        description = ''
          Whether to run Cybar as a systemd user service tied to
          {option}`programs.cybar.systemd.target`.

          The service needs `DISPLAY` (and `XAUTHORITY` when applicable) in
          the systemd user environment. Home Manager's {option}`xsession`
          module imports these for you. If you start X11 some other way,
          either import them yourself
          (`systemctl --user import-environment DISPLAY XAUTHORITY`) or
          disable this option and launch `cybar` from your window manager's
          startup commands instead.
        '';
      };

      target = mkOption {
        type = types.str;
        default = "graphical-session.target";
        example = "tray.target";
        description = ''
          The systemd user target the service is bound to.
        '';
      };
    };
  };

  config = mkIf cfg.enable {
    assertions = [
      {
        assertion = !(cfg.config != null && cfg.configFile != null);
        message = "`programs.cybar.config` and `programs.cybar.configFile` are mutually exclusive; set only one.";
      }
    ];

    home.packages = [ cfg.package ];

    # `mkIf` sits on the whole attribute so that, when no config is managed,
    # no half-defined `xdg.configFile` entry is left behind.
    xdg.configFile = mkIf hasConfig {
      ${configKey} = if cfg.configFile != null then { source = cfg.configFile; } else { text = cfg.config; };
    };

    systemd.user.services.cybar = mkIf cfg.systemd.enable {
      Unit = {
        Description = "Cybar X11 status bar";
        Documentation = "man:cybar(1)";
        After = [ cfg.systemd.target ];
        PartOf = [ cfg.systemd.target ];
      }
      # The config is a symlink into the Nix store. Restarting when its store
      # path changes makes `home-manager switch` apply new configs reliably.
      // lib.optionalAttrs hasConfig {
        X-Restart-Triggers = [ "${config.xdg.configFile.${configKey}.source}" ];
      };

      Service = {
        ExecStart = lib.getExe cfg.package;
        ExecReload = "${lib.getExe cfg.package} reload";
        # The bar can start before the window manager is ready, or lose a
        # race for the tray selection; retry rather than staying dead.
        Restart = "on-failure";
        RestartSec = 2;
      };

      Install.WantedBy = [ cfg.systemd.target ];
    };
  };
}
