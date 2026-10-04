{
  description = "Cybar: a configurable X11 status bar scripted in Lua";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
  };

  outputs =
    { self, nixpkgs }:
    let
      lib = nixpkgs.lib;

      # Cybar is Linux/X11 only.
      systems = [
        "x86_64-linux"
        "aarch64-linux"
      ];
      forAllSystems = f: lib.genAttrs systems (system: f nixpkgs.legacyPackages.${system});

      # Single source of truth for the version: Cargo.toml.
      version = (builtins.fromTOML (builtins.readFile ./Cargo.toml)).package.version;

      # Build Cybar against a given package set.
      #   lua54 = false -> default runtime, vendored LuaJIT   (feature "luajit")
      #   lua54 = true  -> alternative runtime, vendored Lua 5.4 (feature "lua54")
      # The two features are mutually exclusive, so the Lua 5.4 build has to turn
      # the default features off rather than just adding one.
      mkCybar =
        pkgs: { lua54 ? false }:
        pkgs.rustPlatform.buildRustPackage {
          pname = if lua54 then "cybar-lua54" else "cybar";
          inherit version;

          # Only what the build, the tests and postInstall read. Nothing in the
          # crate uses include_str!/include_bytes!, and the tests use temp dirs.
          # Keeping flake.nix, nix/ and CI files out means editing them does
          # not trigger a rebuild.
          src = lib.fileset.toSource {
            root = ./.;
            fileset = lib.fileset.unions [
              ./Cargo.toml
              ./Cargo.lock
              ./src
              ./tests
              ./examples
              ./docs
              ./completions
              ./README.md
              ./BUILD.md
              ./LICENSE
            ];
          };

          # All dependencies come from crates.io, so no outputHashes are needed.
          cargoLock.lockFile = ./Cargo.lock;

          buildNoDefaultFeatures = lua54;
          buildFeatures = lib.optional lua54 "lua54";

          # Lua/LuaJIT are compiled from vendored sources by the `mlua` crate, and
          # the X11 and graphics stack is pure Rust, so only a C compiler and make
          # are needed (both come with stdenv). No buildInputs required.
          nativeBuildInputs = [ pkgs.installShellFiles ];

          # The X11 integration test skips itself unless CYBAR_TEST_DISPLAY is
          # set, so the rest of the test suite runs fine in the Nix sandbox.
          doCheck = true;

          postInstall = ''
            installManPage docs/cybar.1

            installShellCompletion --cmd cybar \
              --bash completions/cybar.bash \
              --zsh completions/_cybar \
              --fish completions/cybar.fish

            # Mirror the Debian/Arch layout: example config + docs under share/doc.
            install -Dm644 examples/config.lua -t $out/share/doc/cybar
            install -Dm644 examples/*.lua -t $out/share/doc/cybar/examples
            install -Dm644 README.md BUILD.md -t $out/share/doc/cybar
            install -Dm644 docs/*.md -t $out/share/doc/cybar/docs
          '';

          meta = {
            description = "Configurable X11 status bar scripted in Lua";
            homepage = "https://github.com/thanvirdiouf/Cybar";
            license = lib.licenses.gpl3Plus;
            mainProgram = "cybar";
            platforms = lib.platforms.linux;
          };
        };
    in
    {
      packages = forAllSystems (pkgs: {
        cybar = mkCybar pkgs { };
        cybar-lua54 = mkCybar pkgs { lua54 = true; };
        default = self.packages.${pkgs.stdenv.hostPlatform.system}.cybar;
      });

      # For NixOS / Home Manager configs:
      #   nixpkgs.overlays = [ inputs.cybar.overlays.default ];
      # then use `pkgs.cybar` or `pkgs.cybar-lua54`.
      overlays.default = final: _prev: {
        cybar = mkCybar final { };
        cybar-lua54 = mkCybar final { lua54 = true; };
      };

      # Home Manager module (programs.cybar.*). See nix/home-manager.nix.
      homeManagerModules = {
        default = import ./nix/home-manager.nix self;
        cybar = self.homeManagerModules.default;
      };

      devShells = forAllSystems (pkgs: {
        default = pkgs.mkShell {
          inputsFrom = [ (mkCybar pkgs { }) ];
          packages = with pkgs; [
            cargo
            rustc
            clippy
            rustfmt
            rust-analyzer
            cargo-deny
            # For the Xvfb integration/smoke tests described in BUILD.md.
            xvfb-run
            python3
          ];
          env.RUST_SRC_PATH = pkgs.rustPlatform.rustLibSrc;
        };
      });
    };
}
