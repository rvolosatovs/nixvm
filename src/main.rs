use std::path::PathBuf;
use std::process::ExitCode;

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

        /// Save the image to PATH instead of an ephemeral tempfile.
        /// Resume later with `nixvm load PATH`.
        #[arg(short = 'p', long = "persist", value_name = "PATH")]
        persist: Option<PathBuf>,

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
            persist,
            cpus,
            memory_mib,
        } => nixvm::run(nixvm::RunArgs {
            flake_ref,
            persist,
            cpus,
            memory_mib,
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
