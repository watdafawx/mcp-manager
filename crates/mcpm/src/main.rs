#![windows_subsystem = "windows"]
//! `mcpm` entry point. Subcommands in SPEC.md.

mod backend;
mod cli;
mod commands;
mod print;

use std::path::PathBuf;
use std::process::ExitCode;

use clap::Parser;

use cli::{Cli, Cmd};

fn main() -> ExitCode {
    // Everything except the daemon and the bridge prints to the terminal that launched us.
    let first = std::env::args().nth(1);
    if !matches!(first.as_deref(), Some("daemon" | "bridge")) {
        attach_parent_console();
    }
    let cli = Cli::parse();
    match run(cli.json, cli.command.unwrap_or(Cmd::Gui)) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("mcpm: {e:#}");
            ExitCode::FAILURE
        }
    }
}

fn run(json: bool, cmd: Cmd) -> anyhow::Result<()> {
    match cmd {
        Cmd::Daemon {
            port,
            no_tray,
            wait_pid,
        } => commands::daemon(port, no_tray, wait_pid),
        Cmd::Bridge { harness } => {
            let exe: PathBuf = std::env::current_exe()?;
            mcpm_daemon::bridge::run_blocking(mcpm_daemon::bridge::BridgeOptions {
                home: mcpm_daemon::storage::state_dir(),
                harness,
                daemon_exe: Some(exe),
            })
        }
        Cmd::Gui => commands::gui(),
        Cmd::Status => commands::status(json),
        Cmd::Harnesses => commands::harnesses(json),
        Cmd::Takeover {
            harness,
            all,
            dry_run,
            text,
        } => commands::takeover(json, harness, all, dry_run, text),
        Cmd::Restore {
            harness,
            all,
            snapshot,
            keep,
        } => commands::restore(json, harness, all, snapshot, keep),
        Cmd::Snapshots => commands::snapshots(json),
        Cmd::Servers { action } => commands::servers(json, action),
        Cmd::Autostart { state } => commands::autostart(json, state),
        Cmd::Uninstall => commands::uninstall(json),
        Cmd::Update { check } => commands::update(json, check),
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
