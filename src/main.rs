use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::{Result, anyhow};
use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(
    name = "nixvm",
    about = "Launch a Nix flake output as a Linux VM via libkrun",
    arg_required_else_help = true
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Build a flake's image and boot it.
    Run {
        /// Flake reference, e.g. `github:user/repo` or
        /// `path:./examples#nixosConfigurations.test`. The attribute path
        /// must point to a NixOS configuration; if omitted, defaults to
        /// `nixosConfigurations.default`.
        flake_ref: String,

        /// Override a flake input. Repeatable. Same syntax as
        /// `nix build --override-input KEY URI`. Useful for pointing
        /// `inputs.nixvm` at a local checkout, e.g.
        /// `--override-input nixvm path:.`.
        #[arg(
            long = "override-input",
            value_names = ["KEY", "URI"],
            num_args = 2,
            action = clap::ArgAction::Append,
        )]
        override_input: Vec<String>,

        /// Set a Nix configuration option. Repeatable. Same syntax as
        /// `nix --option NAME VALUE` (e.g. `--option substitute false`,
        /// `--option connect-timeout 5`). Unknown names warn and are
        /// skipped, matching `nix --option <unknown>`.
        #[arg(
            long = "option",
            value_names = ["NAME", "VALUE"],
            num_args = 2,
            action = clap::ArgAction::Append,
        )]
        option: Vec<String>,

        /// Tarball-cache TTL in seconds. Mirrors `nix --tarball-ttl`.
        /// Pass `0` to force re-fetching cached flake inputs (e.g.
        /// `github:` refs) on every invocation.
        #[arg(long = "tarball-ttl", value_name = "SECONDS")]
        tarball_ttl: Option<u32>,

        /// Save the image to PATH instead of an ephemeral tempfile.
        /// Resume later with `nixvm load PATH`.
        #[arg(short = 'p', long = "persist", value_name = "PATH")]
        persist: Option<PathBuf>,

        /// Overwrite the `--persist` path if it already exists.
        #[arg(short = 'f', long = "force", requires = "persist")]
        force: bool,

        /// Number of vCPUs to allocate to the guest.
        #[arg(long, default_value_t = 2)]
        cpus: u8,

        /// Memory to allocate to the guest, in MiB.
        #[arg(long = "memory", default_value_t = 2048)]
        memory_mib: u32,
    },
    /// Boot a previously-saved image at PATH.
    Load {
        /// Path to a previously-saved image (from `nixvm run -p`).
        path: PathBuf,

        #[arg(long, default_value_t = 2)]
        cpus: u8,

        #[arg(long = "memory", default_value_t = 2048)]
        memory_mib: u32,
    },
}

fn main() -> ExitCode {
    // Tracing on stderr. Defaults to `info`; set NIXVM_LOG to override
    // (e.g. `NIXVM_LOG=debug`, `NIXVM_LOG=warn`, `NIXVM_LOG=nixvm=debug,krun=info`).
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_env("NIXVM_LOG")
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .with_writer(std::io::stderr)
        .without_time()
        .compact()
        .init();

    let result = match Cli::parse().command {
        Command::Run {
            flake_ref,
            override_input,
            option,
            tarball_ttl,
            persist,
            force,
            cpus,
            memory_mib,
        } => parse_pairs("--override-input", override_input)
            .and_then(|overrides| {
                let settings = parse_pairs("--option", option)?;
                Ok((overrides, settings))
            })
            .and_then(|(overrides, settings)| {
                nixvm::run(nixvm::RunArgs {
                    flake_ref,
                    overrides,
                    settings,
                    tarball_ttl,
                    persist,
                    force,
                    cpus,
                    memory_mib,
                })
            }),
        Command::Load {
            path,
            cpus,
            memory_mib,
        } => nixvm::load(nixvm::LoadArgs {
            path,
            cpus,
            memory_mib,
        }),
    };

    match result {
        Ok(code) => ExitCode::from(code),
        Err(err) => {
            eprintln!("nixvm: {err:#}");
            ExitCode::from(1)
        }
    }
}

/// Pair up the flat `num_args = 2, action = Append` Vec clap produces for
/// repeatable two-argument flags (`--override-input KEY URI`,
/// `--option NAME VALUE`) into `(key, value)` tuples.
fn parse_pairs(flag: &str, raw: Vec<String>) -> Result<Vec<(String, String)>> {
    if raw.len() % 2 != 0 {
        return Err(anyhow!("{flag} requires two arguments per use"));
    }
    Ok(raw
        .chunks_exact(2)
        .map(|c| (c[0].clone(), c[1].clone()))
        .collect())
}
