{
  description = "tendr: keep shells, REPLs and long-running jobs alive between tool calls";

  inputs = {
    # Deliberately unpinned to anything clever. A consumer that already has a
    # nixpkgs -- nix-config does -- sets `inputs.nixpkgs.follows` and this
    # revision never reaches its closure.
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
  };

  outputs =
    { self, nixpkgs }:
    let
      # No rust-overlay, on purpose. nixpkgs' own rustc already satisfies
      # rust-toolchain.toml's 1.98.0 (and the crate's 1.85 MSRV floor), so
      # pulling a second toolchain source in would add an input, a lock entry
      # and a second answer to "which compiler built this".
      #
      # rust-toolchain.toml stays the SOURCE OF TRUTH for devenv and rustup --
      # it is the only form rustup understands, which is what makes Windows
      # builds possible at all, and devenv.nix reads it. This flake does not
      # read it and does not need to: a Nix build has no rustup to override, and
      # the file is excluded from `src` below so nothing can silently start
      # honouring it.
      systems = [
        "aarch64-darwin"
        "x86_64-linux"
        "aarch64-linux"
      ];
      forAllSystems = f: nixpkgs.lib.genAttrs systems (system: f nixpkgs.legacyPackages.${system});

      mkTendr =
        pkgs:
        let
          inherit (pkgs) lib;
          fs = lib.fileset;
          cargo = builtins.fromTOML (builtins.readFile ./Cargo.toml);
        in
        pkgs.rustPlatform.buildRustPackage {
          pname = "tendr";

          # Read, never restated. A second version to bump is a second version
          # to forget.
          inherit (cargo.package) version;

          # Exactly what `cargo build` and `cargo test` read, and nothing else.
          # target/ is the big one -- an unfiltered ./. would copy a multi-gigabyte
          # build directory into the store and change the derivation on every
          # local build. devenv.{nix,lock,yaml} and rust-toolchain.toml are
          # excluded because this build does not use them, so editing them must
          # not rebuild the package.
          #
          # docs/guide.md is NOT documentation here: src/commands/guide.rs does
          # `include_str!("../../docs/guide.md")`, so it is build input. The rest
          # of docs/ is left out.
          src = fs.toSource {
            root = ./.;
            fileset = fs.unions [
              ./Cargo.toml
              ./Cargo.lock
              ./src
              ./tests
              ./docs/guide.md
            ];
          };

          cargoLock.lockFile = ./Cargo.lock;

          # `cargo test` here is the real suite, run in the sandbox: it forks
          # children, opens PTYs, signals process groups and writes session state
          # under a TMPDIR-derived HOME. It passes, so the package is also the
          # test gate -- see checks.<system>.tendr.
          doCheck = true;

          # Interpreters the TESTS spawn, not runtime dependencies of the binary:
          # tendr resolves whatever it is asked to run from the caller's PATH.
          # Neither test skips when its interpreter is missing, so both are
          # required for the suite to mean anything in the sandbox.
          #
          #   python3  the PythonRepl lane -- cli_exec asserts variables survive
          #            between execs; without an interpreter five tests report
          #            `spawn_failed_syscall` instead of skipping.
          #   perl     session_fs's lock_exclusivity_across_processes holds an
          #            flock from another process. The test picks perl precisely
          #            because it is present on stock macOS and Linux where a
          #            `flock` CLI is not -- which is not true of a Nix sandbox.
          nativeCheckInputs = [
            pkgs.python3
            pkgs.perl
          ];

          # The tests are built in DEBUG while the binary is built in release,
          # and that is load-bearing rather than a preference. The WAL-ordering
          # tests drive the sidecar's crash injection, which is compiled out of a
          # release build:
          #
          #     src/sidecar.rs:656
          #     if cfg!(debug_assertions) && std::env::var("TENDR_TEST_ABORT")...
          #
          # With buildRustPackage's default (checkType follows buildType) the
          # injection point vanishes, the sidecar finalises normally, and
          # cli_events_lifecycle's crash_before_terminal_event_leaves_neither and
          # crash_after_terminal_event_leaves_event_without_meta fail -- reporting
          # a real run where they expect the orphan the crash should have left.
          # They are not flaky and not sandbox-hostile; they simply require the
          # profile they were written for. tests/sidecar_client_loss.rs likewise
          # needs the debug-only TENDR_TEST_READY_GATE readiness hold, and
          # tests/sidecar_failure.rs the fault hooks TENDR_TEST_FAIL,
          # TENDR_TEST_PANIC and TENDR_TEST_FAULT_GATE.
          checkType = "debug";

          # tests/docs.rs checks README.md, docs/ and docs/plans/, which the
          # source set above leaves out so that editing them does not rebuild the
          # package. CI runs those tests from a full checkout.
          checkFlags = [
            "--skip"
            "docs_"
          ];

          # src/bin/ holds three helper executables that exist only for the
          # integration tests to spawn, and cargo auto-discovers them as
          # binaries. Left alone, `test_callback` and friends would land on the
          # PATH of everyone who installs this package, so the release build is
          # restricted to the one real binary. The check phase still builds the
          # helpers, but into target/debug (see checkType), and the install hook
          # only ever copies out of target/release.
          cargoBuildFlags = [
            "--bin"
            "tendr"
          ];

          # The skill, installed from the SAME file the binary embeds
          # (src/commands/skill.rs include_str!s it), so `tendr skill install`
          # and the packaged copy cannot describe different versions of tendr.
          # nix-config links ~/.claude/skills/using-tendr and
          # ~/.agents/skills/using-tendr straight at this directory.
          postInstall = ''
            install -Dm644 src/embedded/SKILL.md \
              "$out/share/agent-skills/using-tendr/SKILL.md"
          '';

          meta = {
            inherit (cargo.package) description;
            homepage = cargo.package.repository;
            license = with lib.licenses; [
              mit
              asl20
            ];
            mainProgram = "tendr";
            platforms = lib.platforms.unix;
          };
        };
    in
    {
      packages = forAllSystems (pkgs: rec {
        tendr = mkTendr pkgs;
        default = tendr;
      });

      # The package IS the check: doCheck above runs the suite, so building this
      # is running the tests.
      checks = forAllSystems (pkgs: {
        inherit (self.packages.${pkgs.stdenv.hostPlatform.system}) tendr;
      });
    };
}
