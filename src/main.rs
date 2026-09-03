use env_logger::Env;
use input_capture::InputCaptureError;
use input_emulation::InputEmulationError;
use lan_mouse::{
    capture_test,
    config::{self, Command, Config, ConfigError},
    emulation_test,
    service::{Service, ServiceError},
    web::{self, WebError},
};
use lan_mouse_cli::CliError;
use lan_mouse_ipc::{IpcError, IpcListenerCreationError};
use std::{future::Future, io, process};
use thiserror::Error;
use tokio::task::LocalSet;

#[derive(Debug, Error)]
enum LanMouseError {
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
    #[error(transparent)]
    Web(#[from] WebError),
    #[error(transparent)]
    Cli(#[from] CliError),
}

fn main() {
    // Install the rustls crypto provider before any
    // `rustls::ClientConfig::builder` / `ServerConfig::builder` runs. Must be
    // the first statement of `main()` (before logging, config, or service
    // startup). `OnceLock` makes the call safe under repeated invocation and
    // multi-process scenarios.
    lan_mouse::install_crypto_provider();

    // init logging
    let env = Env::default().filter_or("LAN_MOUSE_LOG_LEVEL", "info");
    env_logger::init_from_env(env);

    // Initialise the daemon→browser event bus once, before either the
    // WebServer or the IPC bridge tries to subscribe to it. Done here
    // (not inside `web::run`) so a CLI-only invocation that never
    // starts the web UI still leaves the global in a consistent state.
    web::init_event_bus();

    if let Err(e) = run() {
        log::error!("{e}");
        process::exit(1);
    }
}

fn run() -> Result<(), LanMouseError> {
    let config = config::Config::new()?;
    match config.command() {
        Some(command) => match command {
            Command::TestEmulation(args) => run_async(emulation_test::run(config, args))?,
            Command::TestCapture(args) => run_async(capture_test::run(config, args))?,
            Command::Cli(cli_args) => run_async(lan_mouse_cli::run(cli_args))?,
            Command::Daemon => {
                // if daemon is specified we run the service
                match run_async(run_service(config)) {
                    Err(LanMouseError::Service(ServiceError::IpcListen(
                        IpcListenerCreationError::AlreadyRunning,
                    ))) => log::info!("service already running!"),
                    r => r?,
                }
            }
        },
        None => {
            // No subcommand → run the service in-process AND start the
            // embedded web frontend. We spawn both the service and the
            // HTTP/WS server in the same tokio LocalSet. The web server
            // connects back to the service over the existing IPC socket,
            // the same path the local CLI uses.
            let web_port = web::resolve_port(None, config.web_port());
            let config_path = config.config_path().to_owned();
            let release_bind = config.release_bind();

            run_async(async move {
                // Order matters: the service must bind the IPC socket
                // BEFORE the web server tries to connect to it. We
                // construct the service (which is what creates the
                // listener) up front, then run both `service.run()`
                // and `server.run()` concurrently.
                let mut service = Service::new(config).await?;
                let (request_tx, _event_pump) = web::spawn_ipc_bridge().await?;
                let server = web::WebServer::bind(web_port, request_tx).await?;

                // Best-effort: open the user's browser on launch. Skip
                // when LAN_MOUSE_HIDDEN is set (useful for headless /
                // LaunchAgent setups).
                if std::env::var_os("LAN_MOUSE_HIDDEN").is_none() {
                    let url = web::local_url(web_port);
                    log::info!("opening {url} in the default browser");
                    if let Err(e) = open::that_detached(&url) {
                        log::warn!("could not open browser: {e} — visit {url} manually");
                    }
                }

                log::info!("using config: {config_path:?}");
                log::info!("Press {release_bind:?} to release the mouse");

                // Run the service in the foreground; the HTTP server
                // runs on the same LocalSet as a concurrently polled
                // future so Ctrl-C from either side tears both down.
                tokio::select! {
                    res = server.run() => res?,
                    res = service.run() => res?,
                }
                log::info!("service exited!");
                Ok::<(), LanMouseError>(())
            })?;
        }
    }

    Ok(())
}

fn run_async<F, E>(f: F) -> Result<(), LanMouseError>
where
    F: Future<Output = Result<(), E>>,
    LanMouseError: From<E>,
{
    // create single threaded tokio runtime
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .build()?;

    // run async event loop
    Ok(runtime.block_on(LocalSet::new().run_until(f))?)
}

async fn run_service(config: Config) -> Result<(), ServiceError> {
    let release_bind = config.release_bind();
    let config_path = config.config_path().to_owned();
    let mut service = Service::new(config).await?;
    log::info!("using config: {config_path:?}");
    log::info!("Press {release_bind:?} to release the mouse");
    service.run().await?;
    log::info!("service exited!");
    Ok(())
}
