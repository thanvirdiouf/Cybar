// SPDX-License-Identifier: GPL-3.0-or-later
#[cfg(all(feature = "luajit", feature = "lua54"))]
compile_error!(
    "features luajit and lua54 are mutually exclusive; use --no-default-features --features lua54"
);
#[cfg(not(any(feature = "luajit", feature = "lua54")))]
compile_error!("select one Lua runtime: luajit (default) or lua54");
pub mod config;
pub mod ipc;
pub mod model;
pub mod renderer;
pub mod runtime;
pub mod x11;
