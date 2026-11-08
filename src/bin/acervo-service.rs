//! Serviço do Windows do Acervo.
//!
//!   acervo-service install --db <catalogo.db>   instala e inicia (precisa de administrador, uma vez)
//!   acervo-service uninstall                    para e remove (precisa de administrador)
//!   acervo-service run --db <catalogo.db>       usado pelo próprio Windows ao iniciar o serviço
//!   acervo-service console --db <catalogo.db>   roda em primeiro plano (administrador), para testar
//!
//! O serviço roda como LocalSystem: lê a MFT e o diário USN dos HDs NTFS e grava no catálogo do
//! usuário. O app comum fala com ele pela tabela `service_jobs` do catálogo.

#![cfg_attr(windows, windows_subsystem = "console")]

#[cfg(windows)]
mod windows_main {
    use acervo::service::{run_loop, SERVICE_NAME};
    use anyhow::{bail, Context, Result};
    use std::ffi::OsString;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, OnceLock};
    use std::time::Duration;
    use windows_service::service::{
        ServiceAccess, ServiceControl, ServiceControlAccept, ServiceErrorControl, ServiceExitCode, ServiceInfo,
        ServiceStartType, ServiceState, ServiceStatus, ServiceType,
    };
    use windows_service::service_control_handler::{self, ServiceControlHandlerResult};
    use windows_service::service_manager::{ServiceManager, ServiceManagerAccess};
    use windows_service::{define_windows_service, service_dispatcher};

    static DB_PATH: OnceLock<PathBuf> = OnceLock::new();

    define_windows_service!(ffi_service_main, service_main);

    fn service_main(_arguments: Vec<OsString>) {
        let _ = run_service();
    }

    fn run_service() -> windows_service::Result<()> {
        let stop = Arc::new(AtomicBool::new(false));
        let stop_for_handler = stop.clone();
        let handler = move |event| match event {
            ServiceControl::Stop | ServiceControl::Shutdown => {
                stop_for_handler.store(true, Ordering::Relaxed);
                ServiceControlHandlerResult::NoError
            }
            ServiceControl::Interrogate => ServiceControlHandlerResult::NoError,
            _ => ServiceControlHandlerResult::NotImplemented,
        };
        let status_handle = service_control_handler::register(SERVICE_NAME, handler)?;
        let status = |state: ServiceState, accepted: ServiceControlAccept| ServiceStatus {
            service_type: ServiceType::OWN_PROCESS,
            current_state: state,
            controls_accepted: accepted,
            exit_code: ServiceExitCode::Win32(0),
            checkpoint: 0,
            wait_hint: Duration::default(),
            process_id: None,
        };
        status_handle.set_service_status(status(
            ServiceState::Running,
            ServiceControlAccept::STOP | ServiceControlAccept::SHUTDOWN,
        ))?;

        if let Some(db_path) = DB_PATH.get() {
            run_loop(db_path, &stop);
        }

        status_handle.set_service_status(status(ServiceState::Stopped, ServiceControlAccept::empty()))?;
        Ok(())
    }

    fn db_argument(arguments: &[String]) -> Result<PathBuf> {
        let position = arguments.iter().position(|a| a == "--db").context("faltou --db <catalogo.db>")?;
        Ok(PathBuf::from(arguments.get(position + 1).context("faltou o caminho depois de --db")?))
    }

    fn install(db_path: PathBuf) -> Result<()> {
        let executable = std::env::current_exe()?;
        let manager = ServiceManager::local_computer(
            None::<&str>,
            ServiceManagerAccess::CONNECT | ServiceManagerAccess::CREATE_SERVICE,
        )?;
        // Se já existir (versão antiga), remove antes de instalar de novo.
        let _ = remove_existing();
        let info = ServiceInfo {
            name: OsString::from(SERVICE_NAME),
            display_name: OsString::from("Acervo - catálogo de arquivos"),
            service_type: ServiceType::OWN_PROCESS,
            start_type: ServiceStartType::AutoStart,
            error_control: ServiceErrorControl::Normal,
            executable_path: executable,
            launch_arguments: vec![OsString::from("run"), OsString::from("--db"), db_path.into_os_string()],
            dependencies: vec![],
            account_name: None, // LocalSystem
            account_password: None,
        };
        let service = manager.create_service(&info, ServiceAccess::CHANGE_CONFIG | ServiceAccess::START)?;
        service.set_description(
            "Mantém o catálogo do Acervo atualizado (leitura da MFT e diário USN do NTFS) sem pedir administrador a cada uso.",
        )?;
        service.start::<OsString>(&[])?;
        println!("Serviço instalado e iniciado.");
        Ok(())
    }

    fn remove_existing() -> Result<()> {
        let manager = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)?;
        let service = manager.open_service(
            SERVICE_NAME,
            ServiceAccess::STOP | ServiceAccess::DELETE | ServiceAccess::QUERY_STATUS,
        )?;
        let _ = service.stop();
        // Dá um tempo para o serviço parar antes de apagar.
        for _ in 0..20 {
            match service.query_status() {
                Ok(status) if status.current_state != ServiceState::Stopped => {
                    std::thread::sleep(Duration::from_millis(250))
                }
                _ => break,
            }
        }
        service.delete()?;
        Ok(())
    }

    pub fn main() -> Result<()> {
        let arguments: Vec<String> = std::env::args().skip(1).collect();
        match arguments.first().map(String::as_str) {
            Some("install") => install(db_argument(&arguments)?),
            Some("uninstall") => {
                remove_existing()?;
                println!("Serviço removido.");
                Ok(())
            }
            Some("run") => {
                DB_PATH.set(db_argument(&arguments)?).ok();
                service_dispatcher::start(SERVICE_NAME, ffi_service_main)?;
                Ok(())
            }
            Some("console") => {
                let stop = AtomicBool::new(false);
                println!("Acervo (modo console): rodando. Feche a janela para parar.");
                run_loop(&db_argument(&arguments)?, &stop);
                Ok(())
            }
            _ => bail!("uso: acervo-service install --db <catalogo.db> | uninstall | run --db <catalogo.db> | console --db <catalogo.db>"),
        }
    }
}

#[cfg(windows)]
fn main() -> anyhow::Result<()> {
    windows_main::main()
}

#[cfg(not(windows))]
fn main() {
    eprintln!("O serviço do Acervo só existe no Windows.");
}
