// SPDX-License-Identifier: GPL-3.0-or-later
use anyhow::Result;
use clap::{CommandFactory, Parser, Subcommand};
use cybar::{
    config,
    ipc::{self, Envelope},
    runtime,
};
use std::path::PathBuf;

#[derive(Parser)]
#[command(version, about = "An X11 status bar configured in Lua")]
struct Cli {
    #[arg(long, global = true)]
    config: Option<PathBuf>,
    #[arg(long, default_value_t=ipc::DEFAULT_TIMEOUT_MS, global=true)]
    timeout: u64,
    #[command(subcommand)]
    command: Option<Command>,
}
#[derive(Subcommand)]
enum Command {
    /// Validate the Lua configuration without opening an X window.
    Check,
    /// Reload configuration; keep the current bars if validation fails.
    Reload,
    /// Stop the running instance and release its windows and tray selection.
    Quit,
    /// Show or hide all bars, or one bar/monitor by name.
    Toggle { target: Option<String> },
    /// Deliver a message to a named module without waiting for a reply.
    Msg {
        module: String,
        #[arg(required=true, num_args=1.., trailing_var_arg=true)]
        text: Vec<String>,
    },
    /// Deliver a message and print the module handler's return value.
    Request {
        module: String,
        #[arg(required=true, num_args=1.., trailing_var_arg=true)]
        text: Vec<String>,
    },
    /// Print shell completions.
    Completions { shell: clap_complete::Shell },
}
fn run() -> Result<()> {
    let cli = Cli::parse();
    let path = cli.config.unwrap_or_else(config::default_path);
    let Some(command) = cli.command else {
        return runtime::run(&path);
    };
    match command {
        Command::Check => {
            runtime::check(&path)?;
            println!("Configuration is valid: {}", path.display());
            return Ok(());
        }
        Command::Completions { shell } => {
            clap_complete::generate(shell, &mut Cli::command(), "cybar", &mut std::io::stdout());
            return Ok(());
        }
        _ => {}
    }
    let (method, params, message) = match command {
        Command::Reload => ("reload", None, false),
        Command::Quit => ("quit", None, false),
        Command::Toggle { target } => ("toggle", Some(serde_json::json!({"target":target})), false),
        Command::Msg { module, text } => (
            "module.message",
            Some(serde_json::json!({"module":module,"text":text.join(" ")})),
            true,
        ),
        Command::Request { module, text } => (
            "module.message",
            Some(serde_json::json!({"module":module,"text":text.join(" ")})),
            false,
        ),
        _ => unreachable!(),
    };
    let socket = ipc::socket_path()?;
    if message {
        ipc::send(
            &socket,
            Envelope::request("module.exists", params.clone(), false),
            cli.timeout,
        )?;
    }
    if let Some(result) = ipc::send(
        &socket,
        Envelope::request(method, params, message),
        cli.timeout,
    )? {
        if let Some(s) = result.as_str() {
            println!("{s}");
        } else if method == "module.message" {
            println!("{result}");
        }
    }
    Ok(())
}
fn main() {
    if let Err(e) = run() {
        eprintln!("cybar: {e:#}");
        std::process::exit(1);
    }
}
