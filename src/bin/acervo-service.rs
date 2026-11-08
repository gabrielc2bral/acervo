//! Serviço do Windows do Acervo.
//!
//!   acervo-service install --db <catalogo.db>   instala, inicia e confere (precisa de administrador)
//!   acervo-service uninstall                    para e remove (precisa de administrador)
//!   acervo-service run --db <catalogo.db>       usado pelo próprio Windows ao iniciar o serviço
//!   acervo-service console --db <catalogo.db>   roda em primeiro plano, para testar
//!
//! O serviço roda como LocalSystem: lê a MFT e o diário USN dos HDs NTFS e grava no catálogo do
//! usuário. O app fala com ele pelo `service.db` (ver `acervo::link`). Tudo que acontece, inclusive
//! os erros de instalação, é anotado em `C:\ProgramData\Acervo\service.log`.

#[cfg(windows)]
mod windows_main {
    use acervo::link::ServiceLink;
    use acervo::log;
    use acervo::service::{run_loop, SERVICE_NAME, SERVICE_SDDL};
    use anyhow::{bail, Context, Result};
    use std::ffi::OsString;
    use std::path::PathBuf;
    use std::process::Command;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, OnceLock};
    use std::time::{Duration, Instant};
    use windows_service::service::{
        ServiceAccess, ServiceControl, ServiceControlAccept, ServiceErrorControl, ServiceExitCode, ServiceInfo,
        ServiceStartType, ServiceState, ServiceStatus, ServiceType,
    };
    use windows_service::service_control_handler::{self, ServiceControlHandlerResult};
    use windows_service::service_manager::{ServiceManager, ServiceManagerAccess};
    use windows_service::{define_windows_service, service_dispatcher};

    /// O Windows ainda tem o serviço antigo "marcado para apagar" (códigos 1072).
    const ERROR_SERVICE_MARKED_FOR_DELETE: i32 = 1072;

    static DB_PATH: OnceLock<PathBuf> = OnceLock::new();

    define_windows_service!(ffi_service_main, service_main);

    fn service_main(_arguments: Vec<OsString>) {
        if let Err(error) = run_service() {
            log::write(&format!("ERRO no serviço: {error}"));
        }
    }

    fn run_service() -> windows_service::Result<()> {
        let stop = Arc::new(AtomicBool::new(false));
        let stop_for_handler = stop.clone();
        let handler = move |event| match event {
            ServiceControl::Stop | ServiceControl::Shutdown => {
                log::write("o Windows pediu para parar");
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

        match DB_PATH.get() {
            Some(db_path) => run_loop(db_path, &stop),
            None => log::write("ERRO: o serviço foi iniciado sem --db"),
        }

        status_handle.set_service_status(status(ServiceState::Stopped, ServiceControlAccept::empty()))?;
        Ok(())
    }

    fn db_argument(arguments: &[String]) -> Result<PathBuf> {
        let position = arguments.iter().position(|a| a == "--db").context("faltou --db <catalogo.db>")?;
        Ok(PathBuf::from(arguments.get(position + 1).context("faltou o caminho depois de --db")?))
    }

    /// Mensagem na tela (quando houver console) e no registro.
    fn say(message: &str) {
        println!("{message}");
        log::write(message);
    }

    fn wait_until(seconds: u64, mut condition: impl FnMut() -> bool) -> bool {
        let started = Instant::now();
        while started.elapsed() < Duration::from_secs(seconds) {
            if condition() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(250));
        }
        condition()
    }

    /// Para e remove o serviço que já existir, esperando ele SUMIR de verdade. Sem essa espera, criar
    /// o novo logo em seguida falha (o antigo ainda está "marcado para apagar") e o PC fica sem serviço.
    fn remove_existing() -> Result<bool> {
        let manager = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)?;
        let Ok(service) = manager.open_service(
            SERVICE_NAME,
            ServiceAccess::STOP | ServiceAccess::DELETE | ServiceAccess::QUERY_STATUS,
        ) else {
            return Ok(false); // não existe
        };
        say("Removendo o serviço que já existe...");
        let running = service
            .query_status()
            .map(|status| status.current_state != ServiceState::Stopped)
            .unwrap_or(false);
        if running {
            let _ = service.stop();
            let stopped = wait_until(30, || {
                service.query_status().map(|status| status.current_state == ServiceState::Stopped).unwrap_or(true)
            });
            if !stopped {
                bail!("o serviço antigo não parou em 30 segundos");
            }
        }
        service.delete()?;
        drop(service);
        let gone = wait_until(20, || {
            manager.open_service(SERVICE_NAME, ServiceAccess::QUERY_STATUS).is_err()
        });
        if !gone {
            bail!("o Windows não terminou de remover o serviço antigo (reiniciar o PC resolve se isso se repetir)");
        }
        Ok(true)
    }

    fn create_with_retry(manager: &ServiceManager, info: &ServiceInfo) -> Result<windows_service::service::Service> {
        let started = Instant::now();
        loop {
            match manager.create_service(info, ServiceAccess::CHANGE_CONFIG | ServiceAccess::START | ServiceAccess::QUERY_STATUS) {
                Ok(service) => return Ok(service),
                Err(windows_service::Error::Winapi(error))
                    if error.raw_os_error() == Some(ERROR_SERVICE_MARKED_FOR_DELETE)
                        && started.elapsed() < Duration::from_secs(20) =>
                {
                    std::thread::sleep(Duration::from_millis(500));
                }
                Err(error) => return Err(error).context("o Windows não aceitou criar o serviço"),
            }
        }
    }

    fn install(db_path: PathBuf) -> Result<()> {
        say(&format!("Instalando o serviço do Acervo (catálogo: {})", db_path.display()));
        let executable = std::env::current_exe()?;
        remove_existing()?;

        let manager = ServiceManager::local_computer(
            None::<&str>,
            ServiceManagerAccess::CONNECT | ServiceManagerAccess::CREATE_SERVICE,
        )?;
        let info = ServiceInfo {
            name: OsString::from(SERVICE_NAME),
            display_name: OsString::from("Acervo - catálogo de arquivos"),
            service_type: ServiceType::OWN_PROCESS,
            start_type: ServiceStartType::AutoStart,
            error_control: ServiceErrorControl::Normal,
            executable_path: executable,
            launch_arguments: vec![OsString::from("run"), OsString::from("--db"), db_path.clone().into_os_string()],
            dependencies: vec![],
            account_name: None, // LocalSystem
            account_password: None,
        };
        let service = create_with_retry(&manager, &info)?;
        service
            .set_description(
                "Mantém o catálogo do Acervo atualizado (leitura da MFT e diário USN do NTFS) sem pedir administrador a cada uso.",
            )
            .context("não foi possível descrever o serviço")?;

        // Inicia junto com o Windows, mas atrasado (depois do boot), para não deixar a entrada lenta.
        match Command::new("sc").args(["config", SERVICE_NAME, "start=", "delayed-auto"]).output() {
            Ok(output) if output.status.success() => {}
            _ => log::write("aviso: não foi possível marcar o início atrasado; o serviço inicia no boot normal"),
        }

        // Quem está logado pode iniciar e parar (é o que o ícone da bandeja faz), nada além disso.
        match Command::new("sc").args(["sdset", SERVICE_NAME, SERVICE_SDDL]).output() {
            Ok(output) if output.status.success() => say("Permissões do serviço ajustadas."),
            Ok(output) => log::write(&format!(
                "aviso: não foi possível ajustar as permissões do serviço: {}",
                String::from_utf8_lossy(&output.stdout).trim()
            )),
            Err(error) => log::write(&format!("aviso: sc sdset falhou: {error}")),
        }

        say("Iniciando o serviço...");
        service.start::<OsString>(&[]).context("o Windows não conseguiu iniciar o serviço")?;
        let running = wait_until(30, || {
            service.query_status().map(|status| status.current_state == ServiceState::Running).unwrap_or(false)
        });
        if !running {
            let state = service.query_status().map(|s| format!("{:?}", s.current_state)).unwrap_or_default();
            bail!("o serviço foi criado mas não ficou em execução (estado: {state}). Veja {}", log::path().display());
        }

        // Rodando não basta: precisa estar dando sinal de vida (abriu o catálogo e está trabalhando).
        let link = ServiceLink::open(&db_path).context("não foi possível abrir o service.db")?;
        if !wait_until(20, || link.is_alive()) {
            bail!(
                "o serviço iniciou mas não deu sinal de vida em 20 segundos. Veja {}",
                log::path().display()
            );
        }
        say("Serviço instalado e funcionando.");
        Ok(())
    }

    fn run(arguments: &[String]) -> Result<()> {
        match arguments.first().map(String::as_str) {
            Some("install") => install(db_argument(arguments)?),
            Some("uninstall") => {
                if remove_existing()? {
                    say("Serviço removido.");
                } else {
                    say("O serviço não estava instalado.");
                }
                Ok(())
            }
            Some("run") => {
                log::write("iniciado pelo Windows");
                DB_PATH.set(db_argument(arguments)?).ok();
                service_dispatcher::start(SERVICE_NAME, ffi_service_main)
                    .context("não foi possível ligar ao gerenciador de serviços do Windows")?;
                Ok(())
            }
            Some("console") => {
                let stop = AtomicBool::new(false);
                println!("Acervo (modo console): rodando. Feche a janela para parar.");
                run_loop(&db_argument(arguments)?, &stop);
                Ok(())
            }
            _ => bail!("uso: acervo-service install --db <catalogo.db> | uninstall | run --db <catalogo.db> | console --db <catalogo.db>"),
        }
    }

    pub fn main() -> std::process::ExitCode {
        let arguments: Vec<String> = std::env::args().skip(1).collect();
        match run(&arguments) {
            Ok(()) => std::process::ExitCode::SUCCESS,
            Err(error) => {
                // Sem janela o erro some: vai para o registro e para a saída de erro (o instalador lê).
                log::write(&format!("ERRO: {error:#}"));
                eprintln!("{error:#}");
                std::process::ExitCode::FAILURE
            }
        }
    }
}

#[cfg(windows)]
fn main() -> std::process::ExitCode {
    windows_main::main()
}

#[cfg(not(windows))]
fn main() {
    eprintln!("O serviço do Acervo só existe no Windows.");
}
