// A window program on Windows: it opens no console window of its own, which
// "Copy to RustInput" in Explorer would otherwise do for every selected item.
#![cfg_attr(windows, windows_subsystem = "windows")]

use env_logger::Env;
use input_capture::InputCaptureError;
use input_emulation::InputEmulationError;
use rustinput::{
    capture_test,
    config::{self, Command, Config, ConfigError},
    emulation_test,
    service::{Service, ServiceError},
};
use rustinput_cli::CliError;
#[cfg(feature = "gtk")]
use rustinput_gtk::GtkError;
use rustinput_ipc::{IpcError, IpcListenerCreationError};
use std::{
    future::Future,
    io,
    process::{self, Child},
};
use thiserror::Error;
use tokio::task::LocalSet;

#[derive(Debug, Error)]
enum RustInputError {
    #[error(transparent)]
    Service(#[from] ServiceError),
    #[error(transparent)]
    IpcError(#[from] IpcError),
    #[error(transparent)]
    Config(#[from] ConfigError),
    #[error(transparent)]
    Io(#[from] io::Error),
    #[error(transparent)]
    Capture(#[from] InputCaptureError),
    #[error(transparent)]
    Emulation(#[from] InputEmulationError),
    #[cfg(feature = "gtk")]
    #[error(transparent)]
    Gtk(#[from] GtkError),
    #[error(transparent)]
    Cli(#[from] CliError),
}

fn main() {
    // init logging
    let env = Env::default().filter_or("RUSTINPUT_LOG_LEVEL", "info");
    let mut logger = env_logger::Builder::from_env(env);
    #[cfg(windows)]
    windows_log::setup(&mut logger);
    logger.init();

    // what the installer asks, as administrator, and the Windows service:
    // before the configuration, which is that of a user
    #[cfg(windows)]
    if windows_setup::requested() {
        process::exit(windows_setup::run());
    }
    #[cfg(windows)]
    if windows_supervisor::requested() {
        if let Err(e) = windows_supervisor::run() {
            log::error!("could not run as the Windows service: {e}");
            process::exit(1);
        }
        return;
    }

    if let Err(e) = run() {
        log::error!("{e}");
        process::exit(1);
    }
}

fn run() -> Result<(), RustInputError> {
    let config = config::Config::new()?;
    match config.command() {
        Some(command) => match command {
            Command::TestEmulation(args) => run_async(emulation_test::run(config, args))?,
            Command::TestCapture(args) => run_async(capture_test::run(config, args))?,
            Command::Cli(cli_args) => run_async(rustinput_cli::run(cli_args))?,
            Command::SendFiles { paths } => run_async(rustinput_cli::send_files(paths))?,
            #[cfg(windows)]
            Command::WindowsService => windows_supervisor::run().map_err(io::Error::other)?,
            Command::Daemon => {
                // if daemon is specified we run the service
                match run_async(run_service(config)) {
                    Err(RustInputError::Service(ServiceError::IpcListen(
                        IpcListenerCreationError::AlreadyRunning,
                    ))) => log::info!("service already running!"),
                    r => r?,
                }
            }
        },
        None => {
            //  otherwise start the service as a child process and
            //  run a frontend
            #[cfg(feature = "gtk")]
            {
                // Only spawn a new daemon if one isn't already running
                let mut service = if rustinput_ipc::is_service_running() {
                    log::info!("daemon already running, connecting to existing instance");
                    None
                } else if daemon_from_windows_service() {
                    log::info!("the RustInput service starts the daemon");
                    None
                } else {
                    Some(start_service()?)
                };
                let res = rustinput_gtk::run(
                    env!("CARGO_PKG_VERSION"),
                    config::local_commit(),
                    config.theme(),
                    config.language(),
                );
                if let Some(ref mut service) = service {
                    #[cfg(unix)]
                    {
                        // on unix we give the service a chance to terminate gracefully
                        let pid = service.id() as libc::pid_t;
                        unsafe {
                            libc::kill(pid, libc::SIGINT);
                        }
                        service.wait()?;
                    }
                    service.kill()?;
                }
                res?;
                if rustinput_gtk::restart_requested() {
                    restart()?;
                }
            }
            #[cfg(not(feature = "gtk"))]
            {
                // run daemon if gtk is diabled
                match run_async(run_service(config)) {
                    Err(RustInputError::Service(ServiceError::IpcListen(
                        IpcListenerCreationError::AlreadyRunning,
                    ))) => log::info!("service already running!"),
                    r => r?,
                }
            }
        }
    }

    Ok(())
}

/// Starts RustInput again, with the same arguments: the window was asked
/// to, to show itself in the language just picked.
#[cfg(feature = "gtk")]
fn restart() -> Result<(), RustInputError> {
    let exe = std::env::current_exe()?;
    let args: Vec<_> = std::env::args_os().skip(1).collect();
    log::info!("starting again");
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // in this very process: the name of the window on the session bus
        // is free again before the new one asks for it
        let error = process::Command::new(exe)
            .args(args)
            .env_remove("RUSTINPUT_HIDDEN")
            .exec();
        Err(error.into())
    }
    #[cfg(not(unix))]
    {
        process::Command::new(exe)
            .args(args)
            .env_remove("RUSTINPUT_HIDDEN")
            .spawn()?;
        Ok(())
    }
}

fn run_async<F, E>(f: F) -> Result<(), RustInputError>
where
    F: Future<Output = Result<(), E>>,
    RustInputError: From<E>,
{
    // create single threaded tokio runtime
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .build()?;

    // run async event loop
    Ok(runtime.block_on(LocalSet::new().run_until(f))?)
}

/// whether the Windows service starts the daemon, with administrator rights
#[cfg(all(feature = "gtk", windows))]
fn daemon_from_windows_service() -> bool {
    rustinput_ipc::windows_service_runs()
}

#[cfg(all(feature = "gtk", not(windows)))]
fn daemon_from_windows_service() -> bool {
    false
}

fn start_service() -> Result<Child, io::Error> {
    let child = process::Command::new(std::env::current_exe()?)
        .args(std::env::args().skip(1))
        .arg("daemon")
        .spawn()?;
    Ok(child)
}

async fn run_service(config: Config) -> Result<(), ServiceError> {
    let release_bind = config.release_bind();
    let config_path = config.config_path().to_owned();
    #[cfg(windows)]
    log::info!(
        "RustInput {} daemon, {}",
        env!("CARGO_PKG_VERSION"),
        if windows_supervisor::runs_as_administrator() {
            "with administrator rights"
        } else {
            "without administrator rights"
        }
    );
    let mut service = Service::new(config).await?;
    log::info!("using config: {config_path:?}");
    log::info!("Press {release_bind:?} to release the mouse");
    service.run().await?;
    log::info!("service exited!");
    Ok(())
}

#[cfg(windows)]
mod windows_log;
#[cfg(windows)]
mod windows_setup;
#[cfg(windows)]
mod windows_supervisor;
