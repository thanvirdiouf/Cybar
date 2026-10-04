# Building, installation, and development

Cybar targets Linux with X11. Use a current stable Rust toolchain, a C compiler,
and make. Lua is vendored at build time; no system Lua installation is needed.
The X11 and graphics implementation uses Rust libraries. Running the bar requires
an authorized X11 session (`DISPLAY`, and `XAUTHORITY` when needed) and readable
desktop fonts.

## Build and install

The default runtime is LuaJIT:

```sh
cargo build --release --locked
cargo install --path . --locked
```

The Lua 5.4 alternative:

```sh
cargo build --release --locked --no-default-features --features lua54
cargo install --path . --locked --no-default-features --features lua54
```

Choose exactly one runtime. The `luajit` and `lua54` features are mutually
exclusive; do not use `--all-features`. Both variants install the same `cybar`
binary. Shipped Lua examples use syntax supported by both.

The compiled binary is `target/release/cybar`. Cargo installs it into
`$CARGO_HOME/bin`, usually `~/.cargo/bin`; make that directory available on `PATH`.
If you only built the checkout, invoke `./target/release/cybar` directly. Follow
[README.md](README.md#get-started) to install an example configuration and start
the bar.

Publication to crates.io and AUR requires a release repository and maintainer.
Until publication, install this checkout rather than assuming that a published
`cybar` crate or package already exists. Once the crate is published,
`cargo install cybar --locked` will install the default vendored LuaJIT build.

## Formatting, tests, and linting

```sh
cargo fmt --check
cargo test --locked
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked --no-default-features --features lua54
cargo clippy --locked --all-targets --no-default-features --features lua54 -- -D warnings
```

Validate the shipped configurations with each runtime:

```sh
cargo run --locked -- --config examples/config.lua check
cargo run --locked -- --config examples/custom.lua check
cargo run --locked --no-default-features --features lua54 -- --config examples/config.lua check
cargo run --locked --no-default-features --features lua54 -- --config examples/custom.lua check
```

`check` evaluates the Lua configuration and validates it without opening X
windows. Direct Lua I/O in construction code still runs; queued commands are not
executed by this validation command.

## X11 integration and smoke tests

Install Xvfb, xauth, Python 3, and desktop fonts. The protocol integration test
requires `CYBAR_TEST_DISPLAY` to point to an isolated X server. Without that
variable it reports a skip. Do not point it at your active desktop: the fixture
sets EWMH properties and claims the tray selection.

```sh
xvfb-run -a sh -c 'CYBAR_TEST_DISPLAY="$DISPLAY" cargo test --locked --test x11_integration'
cargo build --locked
xvfb-run -a python3 scripts/xvfb-smoke.py target/debug/cybar
```

Repeat for Lua 5.4:

```sh
xvfb-run -a sh -c 'CYBAR_TEST_DISPLAY="$DISPLAY" cargo test --locked --no-default-features --features lua54 --test x11_integration'
cargo build --locked --no-default-features --features lua54
xvfb-run -a python3 scripts/xvfb-smoke.py target/debug/cybar
```

The GitHub Actions matrix runs formatting, tests, Clippy, example validation,
isolated X11 integration, and daemon/CLI smoke tests for both runtimes. Xvfb
establishes protocol behavior against a controlled server; real-desktop testing
remains necessary for intended WMs, tray applications, fonts, DPI settings, and
physical smooth-scroll devices.

## Dependency and license checks

```sh
cargo install cargo-deny --locked
cargo deny --no-default-features --features luajit check
cargo deny --no-default-features --features lua54 check
```

`deny.toml` checks crate licenses, sources, duplicate versions, and the RustSec
advisory database. Its feature graph selects one Lua runtime at a time. Unknown
licenses and dependency sources fail the policy. Vulnerability advisories and
yanked crates also fail the audit. Two individually identified informational
maintenance notices for the locked font stack have documented exceptions;
[the maintenance review](docs/dependency-licenses.md#font-stack-maintenance)
explains their scope and removal plan. Other maintenance advisories remain errors.

With dependencies and the advisory database already cached, use `cargo deny
--offline --locked --no-default-features --features luajit check` (and repeat with
`lua54`) for a check that does not fetch data. Online release and CI audits refresh
the advisory database; offline success only reflects the cached database.

[Dependency licensing](docs/dependency-licenses.md) records the initial metadata
review and the GPLv3 compatibility policy. Release distributors must preserve
dependency notices and corresponding source requirements, including vendored
Lua/LuaJIT and Unicode data. A metadata checker cannot replace a review of
embedded notices.

## Packages and completions

The [packaging guide](packaging/README.md) documents Debian and four Arch/AUR
templates, repository configuration, package conflicts, and release checks.

```sh
cargo install cargo-deb --locked
bash packaging/deb/build.sh luajit
bash packaging/deb/build.sh lua54
```

Debian artifacts go under `target/debian/`. The packages are `cybar` and
`cybar-lua54`; they conflict and install the same binary. The helper regenerates
shell completions and builds the requested runtime before packaging. When
`cargo-deb` is unavailable, it uses the bundled Python packager (Python 3.11+,
`dpkg-deb`, and `readelf` are required). It does not
install or launch the package. Inspect packages using `dpkg-deb --contents` and
`dpkg-deb --info` before distribution.

Arch templates use `CYBAR_REPOSITORY`; the default `example.invalid` URL is an
explicit placeholder. Configure the actual repository and release tags before
running makepkg or publishing `.SRCINFO`.

Generate completions from a built binary:

```sh
mkdir -p completions
target/release/cybar completions bash > completions/cybar.bash
target/release/cybar completions zsh > completions/_cybar
target/release/cybar completions fish > completions/cybar.fish
```

Packages include the man page, example configuration, license, and completions.
Before publishing, verify both Lua runtimes, package metadata, source tags,
repository URLs, runtime conflicts, and dependency notices.

## Local build record — October 4, 2026

The local release artifacts were built on x86_64 Linux with Rust/Cargo 1.98.1
and glibc 2.39. The `dist/` directory contains:

- `cybar-luajit` and `cybar-lua54`: optimized binaries with vendored Lua runtimes.
- `cybar_0.1.0_amd64.deb` and `cybar-lua54_0.1.0_amd64.deb`: validated Debian
  archives requiring `libc6 (>= 2.39)` and `libgcc-s1`.
- `cybar-0.1.0.crate`: the project source archive and locked dependency manifest.
  Cargo fetches dependency sources when rebuilding; this archive is not a vendor
  bundle. Extract it and follow the build instructions above.
- `SHA256SUMS`: checksums for these five artifacts.

Both runtimes passed 44 unit tests, the live Xvfb protocol integration fixture,
63 daemon/CLI regression assertions, Clippy with warnings denied, and all four
shipped bar configurations. Formatting and the dependency/license audit also
passed; the latter used a cached advisory database with the two documented
maintenance exceptions. The package helper checked archive metadata, root file
ownership, the extracted binary, and the compressed manual.

The default `target/release/cybar` is the LuaJIT build. From this checkout, start
it in your X11 desktop with:

```sh
./dist/cybar-luajit --config examples/config.lua
```

These are local build artifacts. Real-desktop compatibility and external release
publication remain separate from the isolated verification above.
