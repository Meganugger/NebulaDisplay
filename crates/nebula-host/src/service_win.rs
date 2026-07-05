//! Windows service integration.
//!
//! `nebula-host --service` runs under the Service Control Manager (installed
//! by the Windows installer as service `NebulaDisplayHost`). The service
//! wrapper translates SCM start/stop into running/aborting the same async
//! `run` path the console mode uses.

#![cfg(windows)]

use std::ffi::OsString;
use std::sync::mpsc;
use std::time::Duration;

use tracing::{error, info};
use windows_service::service::{
    ServiceControl, ServiceControlAccept, ServiceExitCode, ServiceState, ServiceStatus, ServiceType,
};
use windows_service::service_control_handler::{self, ServiceControlHandlerResult};

pub const SERVICE_NAME: &str = "NebulaDisplayHost";

windows_service::define_windows_service!(ffi_service_main, service_main);

/// Entry point used by `main` when `--service` is passed.
pub fn run_as_service() -> anyhow::Result<()> {
    windows_service::service_dispatcher::start(SERVICE_NAME, ffi_service_main)?;
    Ok(())
}

fn service_main(_arguments: Vec<OsString>) {
    if let Err(e) = service_body() {
        error!("service failed: {e:#}");
    }
}

fn service_body() -> anyhow::Result<()> {
    let (shutdown_tx, shutdown_rx) = mpsc::channel::<()>();

    let status_handle =
        service_control_handler::register(SERVICE_NAME, move |control| match control {
            ServiceControl::Stop | ServiceControl::Shutdown => {
                shutdown_tx.send(()).ok();
                ServiceControlHandlerResult::NoError
            }
            ServiceControl::Interrogate => ServiceControlHandlerResult::NoError,
            _ => ServiceControlHandlerResult::NotImplemented,
        })?;

    let set_state = |state: ServiceState| {
        status_handle.set_service_status(ServiceStatus {
            service_type: ServiceType::OWN_PROCESS,
            current_state: state,
            controls_accepted: if state == ServiceState::Running {
                ServiceControlAccept::STOP | ServiceControlAccept::SHUTDOWN
            } else {
                ServiceControlAccept::empty()
            },
            exit_code: ServiceExitCode::Win32(0),
            checkpoint: 0,
            wait_hint: Duration::from_secs(10),
            process_id: None,
        })
    };

    set_state(ServiceState::Running)?;
    info!("NebulaDisplay host service running");

    // Run the normal server on a background runtime; stop when SCM says so.
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    let server = std::thread::spawn(move || {
        rt.block_on(async {
            if let Err(e) = crate::run_with_defaults().await {
                error!("host server exited: {e:#}");
            }
        });
    });

    shutdown_rx.recv().ok();
    info!("service stop requested");
    set_state(ServiceState::Stopped)?;
    // Process exit tears down the runtime and all sessions.
    drop(server);
    std::process::exit(0);
}
