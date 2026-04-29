use std::process::ExitCode;

use clap::Parser;

#[derive(Parser)]
#[command(name = "nixvm", about = "Launch a Nix flake output as a Linux VM via libkrun")]
struct Cli {
    /// Flake reference, e.g. `github:user/repo#name` or `path:./examples#test`.
    /// Must evaluate to a derivation that builds a libkrun-bootable raw EFI disk image.
    flake_ref: String,

    /// Number of vCPUs to allocate to the guest.
    #[arg(long, default_value_t = 2)]
    cpus: u8,

    /// Memory to allocate to the guest, in MiB.
    #[arg(long = "memory", default_value_t = 2048)]
    memory_mib: u32,
}

fn main() -> ExitCode {
    // `NIXVM_LOG=debug` (or `=trace`, `=nixvm=debug,krun=info`, etc.) enables
    // tracing output on stderr. Defaults to off so the host TTY belongs to
    // the guest.
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_env("NIXVM_LOG")
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("off")),
        )
        .with_writer(std::io::stderr)
        .without_time()
        .compact()
        .init();

    let cli = Cli::parse();
    let args = nixvm::Args {
        flake_ref: cli.flake_ref,
        cpus: cli.cpus,
        memory_mib: cli.memory_mib,
    };
    match nixvm::run(args) {
        Ok(code) => ExitCode::from(code),
        Err(err) => {
            eprintln!("nixvm: {err:#}");
            ExitCode::from(1)
        }
    }
}
