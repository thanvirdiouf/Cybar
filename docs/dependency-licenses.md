# Dependency licensing

Cybar is GPL-3.0-or-later. `deny.toml` permits only the compatible licenses
observed in the locked Linux graph: MIT, Apache-2.0, BSD-2-Clause, BSD-3-Clause,
ISC, Zlib, CC0-1.0, Unicode-3.0, MPL-2.0, and Cybar's own GPL-3.0-or-later.
An SPDX `OR` expression needs one accepted alternative; `AND` needs every
component. Unknown licenses and unknown registries/Git sources fail CI.

The initial `Cargo.lock` metadata review includes these points:

| Component | Declared license |
| --- | --- |
| `resvg`, `usvg`, `svgtypes`, `swash` | Apache-2.0 OR MIT |
| `tiny-skia`, `tiny-skia-path` | BSD-3-Clause |
| `mlua`, `mlua-sys`, `luajit-src`, `lua-src` | MIT |
| `x11rb`, `cosmic-text`, `image` | MIT OR Apache-2.0 |
| `notify` | CC0-1.0 |
| `option-ext` | MPL-2.0 |
| `unicode-ident` | (MIT OR Apache-2.0) AND Unicode-3.0 |
| `self_cell` | Apache-2.0 OR GPL-2.0-only; use Apache-2.0 |

The [GNU license list](https://www.gnu.org/licenses/license-list.html) and
[GPL compatibility discussion](https://www.gnu.org/licenses/license-compatibility.en.html)
explain these license relationships. Apache-2.0 can be combined with GPLv3; see
the [Apache Software Foundation's compatibility statement](https://www.apache.org/licenses/GPL-compatibility).
For MPL-2.0 components, preserve MPL-covered source and notices and check for any
incompatible-with-secondary-licenses marking before adding a new dependency.

Run the checks for both runtime graphs as described in
[BUILD.md](../BUILD.md#dependency-and-license-checks).

The [cargo-deny license checker](https://embarkstudios.github.io/cargo-deny/checks/licenses/index.html)
checks metadata and recognizable license files; it does not exhaustively identify
every embedded notice. A release audit must preserve the licenses/notices shipped
with dependencies, including vendored Lua/LuaJIT and Unicode data. Include the
corresponding source and required notices when distributing binaries. Update this
review when changing the dependency graph; do not add blanket license exceptions
to make CI pass.

## Font stack maintenance

Reviewed on 2026-10-04. `deny.toml` accepts exactly two advisory IDs for the locked
font stack: [RUSTSEC-2026-0206](https://rustsec.org/advisories/RUSTSEC-2026-0206.html)
for `rustybuzz`, and
[RUSTSEC-2026-0192](https://rustsec.org/advisories/RUSTSEC-2026-0192.html) for
`ttf-parser`. RustSec classifies both as **INFO Unmaintained** and names `harfrust`
and `skrifa` as maintained alternatives. These exceptions do not exempt security
advisories. Vulnerability and unsoundness checks remain strict, yanked crates fail,
and `unmaintained = "all"` continues to reject other maintenance advisories.
Unused exceptions fail the audit so they must be removed after migration.

The current graph reaches these crates through `cosmic-text 0.14.2`,
`usvg 0.45.1`, and their `fontdb` and `rustybuzz` dependencies. Cybar also uses
the text stack's font parser API to choose a font that covers an entire grapheme
before shaping it. Replacing the dependencies therefore needs a corresponding
renderer change and visual validation.

The investigated upgrades do not remove both crates from the whole graph:

| Upgrade | Effect and migration requirements |
| --- | --- |
| [cosmic-text 0.17.2](https://github.com/pop-os/cosmic-text/blob/0.17.2/Cargo.toml) | Uses `harfrust` and `skrifa`, but still requires `fontdb 0.23`, which [depends on `ttf-parser`](https://github.com/RazrFalcon/fontdb/blob/v0.23.0/Cargo.toml). Requires Rust 1.89 and adaptation of the font coverage API. |
| [resvg/usvg 0.48](https://github.com/linebender/resvg/blob/v0.48.0/crates/usvg/Cargo.toml) | Uses `harfrust`, `skrifa`, and `fontdb 0.24`, removing both legacy crates from the SVG branch. Its [changelog](https://github.com/linebender/resvg/blob/v0.48.0/CHANGELOG.md) records Rust 1.89, the `tiny-skia 0.12` transition, and possible rendering changes. Cybar's direct `tiny-skia` dependency must migrate with it. |

The plan is to adopt a published COSMIC release using the newer font database,
migrate Cybar's grapheme coverage check to a maintained font API, and update
`resvg` and `tiny-skia` together. COSMIC's
[main manifest](https://github.com/pop-os/cosmic-text/blob/main/Cargo.toml) already
uses `fontdb 0.24`; this is an upstream migration reference, not an unpinned Git
dependency for Cybar. Verify the resolved graph contains neither legacy crate,
then remove both exceptions. Run the font fallback and SVG tests, both Lua runtime
suites, the X11 smoke tests, and compare the horizontal and vertical screenshots
before accepting the rendering migration. The temporary exceptions keep the
current tested renderer while that complete migration is pending.
