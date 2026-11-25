#![windows_subsystem = "windows"]
//! `mcpm` entry point. Subcommands in SPEC.md. Only `daemon` and `bridge` exist so far.

use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(
    name = "mcpm",
    version,
    about = "MCP Manager: one place for every MCP server of every AI harness"
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Cmd>,
}

#[derive(Subcommand)]
enum Cmd {
    /// Run the background daemon (no window).
    Daemon {
        /// Listen on this port instead of the configured one.
        #[arg(long)]
        port: Option<u16>,
    },
    /// stdio <-> HTTP bridge for stdio-only harnesses; starts the daemon if needed.
    Bridge {
        #[arg(long)]
        harness: String,
    },
    /// Open the window (default).
    Gui,
    Status,
    Harnesses,
    Takeover {
        harness: Option<String>,
        #[arg(long)]
        all: bool,
        #[arg(long)]
        dry_run: bool,
    },
    Restore {
        harness: Option<String>,
        #[arg(long)]
        all: bool,
        #[arg(long)]
        snapshot: Option<String>,
    },
    Snapshots,
    Servers {
        #[command(subcommand)]
        action: Option<ServersCmd>,
    },
    Autostart {
        #[arg(value_parser = ["on", "off"])]
        state: String,
    },
    Uninstall,
}

#[derive(Subcommand)]
enum ServersCmd {
    List,
    Add,
    Remove { id: String },
    Enable { id: String },
    Disable { id: String },
}

fn main() -> ExitCode {
    // Everything except the daemon and the bridge prints to the terminal that launched us.
    let first = std::env::args().nth(1);
    if !matches!(first.as_deref(), Some("daemon" | "bridge")) {
        attach_parent_console();
    }
    let cli = Cli::parse();
    match run(cli.command.unwrap_or(Cmd::Gui)) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("mcpm: {e:#}");
            ExitCode::FAILURE
        }
    }
}

fn run(cmd: Cmd) -> anyhow::Result<()> {
    let home = mcpm_daemon::storage::state_dir();
    match cmd {
        Cmd::Daemon { port } => {
            let mut opts = mcpm_daemon::DaemonOptions::new(home);
            opts.port = port;
            mcpm_daemon::run_blocking(opts)
        }
        Cmd::Bridge { harness } => {
            let exe: PathBuf = std::env::current_exe()?;
            mcpm_daemon::bridge::run_blocking(mcpm_daemon::bridge::BridgeOptions {
                home,
                harness,
                daemon_exe: Some(exe),
            })
        }
        other => {
            // TODO: gui, status, harnesses, takeover, restore, snapshots, servers, autostart, uninstall.
            let name = match other {
                Cmd::Gui => "gui",
                Cmd::Status => "status",
                Cmd::Harnesses => "harnesses",
                Cmd::Takeover { .. } => "takeover",
                Cmd::Restore { .. } => "restore",
                Cmd::Snapshots => "snapshots",
                Cmd::Servers { .. } => "servers",
                Cmd::Autostart { .. } => "autostart",
                Cmd::Uninstall => "uninstall",
                Cmd::Daemon { .. } | Cmd::Bridge { .. } => unreachable!(),
            };
            println!("mcpm {name}: not yet");
            Ok(())
        }
    }
}

/// The exe is a GUI-subsystem binary: re-attach to the parent terminal so CLI output shows up.
#[cfg(windows)]
fn attach_parent_console() {
    use std::os::windows::io::IntoRawHandle;
    use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
    use windows_sys::Win32::System::Console::{
        ATTACH_PARENT_PROCESS, AttachConsole, GetStdHandle, STD_ERROR_HANDLE, STD_INPUT_HANDLE,
        STD_OUTPUT_HANDLE, SetStdHandle,
    };
    // SAFETY: Win32 console calls; handles we open are leaked on purpose (they live for the process).
    unsafe {
        if AttachConsole(ATTACH_PARENT_PROCESS) == 0 {
            return; // no parent console (double-click, or redirected): nothing to do
        }
        let reopen = |std: u32, name: &str, write: bool| {
            let h = GetStdHandle(std);
            if (h.is_null() || h == INVALID_HANDLE_VALUE)
                && let Ok(f) = std::fs::OpenOptions::new()
                    .read(!write)
                    .write(write)
                    .open(name)
            {
                SetStdHandle(std, f.into_raw_handle() as _);
            }
        };
        reopen(STD_OUTPUT_HANDLE, "CONOUT$", true);
        reopen(STD_ERROR_HANDLE, "CONOUT$", true);
        reopen(STD_INPUT_HANDLE, "CONIN$", false);
    }
}

#[cfg(not(windows))]
fn attach_parent_console() {}
