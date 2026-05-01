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
        /// `path:./examples#myhost`. The fragment names a
        /// `nixosConfigurations.<name>` entry (matches `nixos-rebuild
        /// --flake`); defaults to `default` when omitted.
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

        /// Run headless: detach from the controlling TTY after setup and
        /// run the VM as a launchd-owned daemon. Stdout/stderr go to a
        /// per-launch log under `~/Library/Application Support/nixvm/logs/`
        /// (the path is also printed on detach). Stop the VM by shutting
        /// down from inside the guest, or `pkill -f nixvm`.
        #[arg(short = 'd', long = "detach")]
        detach: bool,

        /// Number of vCPUs to allocate to the guest.
        #[arg(long, default_value_t = 2)]
        cpus: u8,

        /// Memory to allocate to the guest, in MiB.
        #[arg(long = "memory", default_value_t = 2048)]
        memory_mib: u32,
    },
    /// Boot a previously-saved image at PATH.
    ///
    /// Without `<flake_ref>`, the image is booted against the closure
    /// recorded in its sidecar at `nixvm run -p` time. With `<flake_ref>`,
    /// the flake is realised and the sidecar + GC root are refreshed
    /// against the new closure — so on-disk state (`/var`, `/etc`,
    /// `/home`) is preserved while the underlying NixOS is updated.
    /// Compatibility constraints are the same as `nixos-rebuild boot`
    /// followed by reboot on bare metal.
    Load {
        /// Path to a previously-saved image (from `nixvm run -p`).
        path: PathBuf,

        /// Optional flake reference. When present, realises the flake and
        /// boots the existing image against the new closure. Same syntax
        /// as `nixvm run`'s flake_ref.
        flake_ref: Option<String>,

        /// Override a flake input. Repeatable. Same syntax as
        /// `nix build --override-input KEY URI`. Only meaningful when
        /// `<flake_ref>` is given.
        #[arg(
            long = "override-input",
            value_names = ["KEY", "URI"],
            num_args = 2,
            action = clap::ArgAction::Append,
            requires = "flake_ref",
        )]
        override_input: Vec<String>,

        /// Set a Nix configuration option. Repeatable. Same syntax as
        /// `nix --option NAME VALUE`. Only meaningful when `<flake_ref>`
        /// is given.
        #[arg(
            long = "option",
            value_names = ["NAME", "VALUE"],
            num_args = 2,
            action = clap::ArgAction::Append,
            requires = "flake_ref",
        )]
        option: Vec<String>,

        /// Tarball-cache TTL in seconds. Mirrors `nix --tarball-ttl`.
        /// Only meaningful when `<flake_ref>` is given.
        #[arg(long = "tarball-ttl", value_name = "SECONDS", requires = "flake_ref")]
        tarball_ttl: Option<u32>,

        /// Run headless. See `nixvm run --detach`.
        #[arg(short = 'd', long = "detach")]
        detach: bool,

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
            detach,
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
                    detach,
                    cpus,
                    memory_mib,
                })
            }),
        Command::Load {
            path,
            flake_ref,
            override_input,
            option,
            tarball_ttl,
            detach,
            cpus,
            memory_mib,
        } => parse_pairs("--override-input", override_input)
            .and_then(|overrides| {
                let settings = parse_pairs("--option", option)?;
                Ok((overrides, settings))
            })
            .and_then(|(overrides, settings)| {
                nixvm::load(nixvm::LoadArgs {
                    path,
                    flake_ref,
                    overrides,
                    settings,
                    tarball_ttl,
                    detach,
                    cpus,
                    memory_mib,
                })
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
