//! Ícone do Acervo na bandeja do Windows ("ícones ocultos").
//!
//! Mostra se o serviço está ativo e deixa abrir o Acervo, parar/iniciar o serviço e ver o
//! registro. Quem está logado pode parar e iniciar o serviço porque o instalador libera só isso
//! nas permissões dele (ver `acervo::service::SERVICE_SDDL`).

#![cfg_attr(windows, windows_subsystem = "windows")]

#[cfg(windows)]
mod tray {
    use acervo::service::{Installed, SERVICE_NAME};
    use std::ffi::OsString;
    use std::time::{Duration, Instant};
    use tray_icon::menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem};
    use tray_icon::{Icon, TrayIconBuilder};
    use windows::Win32::UI::WindowsAndMessaging::{
        DispatchMessageW, PeekMessageW, TranslateMessage, MSG, PM_REMOVE,
    };
    use windows_service::service::{ServiceAccess, ServiceState};
    use windows_service::service_manager::{ServiceManager, ServiceManagerAccess};

    fn load_icon() -> Option<Icon> {
        let image = image::load_from_memory(include_bytes!("../../assets/acervo-256.png")).ok()?;
        let small = image.resize(32, 32, image::imageops::FilterType::Lanczos3).to_rgba8();
        let (width, height) = small.dimensions();
        Icon::from_rgba(small.into_raw(), width, height).ok()
    }

    fn status_text() -> String {
        let db_path = acervo::db::Db::default_path().ok();
        match acervo::service::installed_state() {
            Installed::No => "Serviço: não instalado".to_string(),
            Installed::Stopped => "Serviço: parado".to_string(),
            Installed::Running => {
                if db_path.is_some_and(|path| acervo::service::is_alive_at(&path)) {
                    "Serviço: ativo".to_string()
                } else {
                    "Serviço: iniciando...".to_string()
                }
            }
        }
    }

    /// Inicia ou para o serviço. Devolve o texto de erro, se houver.
    fn toggle_service() -> Result<(), String> {
        let manager = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)
            .map_err(|e| e.to_string())?;
        let service = manager
            .open_service(
                SERVICE_NAME,
                ServiceAccess::START | ServiceAccess::STOP | ServiceAccess::QUERY_STATUS,
            )
            .map_err(|e| format!("não consegui abrir o serviço: {e}"))?;
        let state = service.query_status().map_err(|e| e.to_string())?.current_state;
        if state == ServiceState::Stopped {
            service.start::<OsString>(&[]).map_err(|e| e.to_string())
        } else {
            service.stop().map(|_| ()).map_err(|e| e.to_string())
        }
    }

    fn open_app() {
        if let Some(dir) = std::env::current_exe().ok().and_then(|p| p.parent().map(|d| d.to_path_buf())) {
            let _ = std::process::Command::new(dir.join("acervo.exe")).spawn();
        }
    }

    fn open_log() {
        let path = acervo::log::path();
        let _ = std::process::Command::new("notepad.exe").arg(path).spawn();
    }

    pub fn main() {
        let status = MenuItem::new(status_text(), false, None);
        let open = MenuItem::new("Abrir o Acervo", true, None);
        let toggle = MenuItem::new("Parar / iniciar o serviço", true, None);
        let log = MenuItem::new("Ver o registro do serviço", true, None);
        let quit = MenuItem::new("Sair do ícone", true, None);
        let menu = Menu::new();
        let _ = menu.append_items(&[&status, &PredefinedMenuItem::separator(), &open, &toggle, &log, &PredefinedMenuItem::separator(), &quit]);

        let mut builder = TrayIconBuilder::new().with_menu(Box::new(menu)).with_tooltip("Acervo");
        if let Some(icon) = load_icon() {
            builder = builder.with_icon(icon);
        }
        let Ok(_tray) = builder.build() else { return };

        let events = MenuEvent::receiver();
        let mut last_status = Instant::now();
        loop {
            // Bomba de mensagens do Windows (o menu da bandeja depende dela).
            unsafe {
                let mut message = MSG::default();
                while PeekMessageW(&mut message, None, 0, 0, PM_REMOVE).as_bool() {
                    let _ = TranslateMessage(&message);
                    DispatchMessageW(&message);
                }
            }
            while let Ok(event) = events.try_recv() {
                if event.id == open.id() {
                    open_app();
                } else if event.id == toggle.id() {
                    if let Err(error) = toggle_service() {
                        acervo::log::write(&format!("ícone da bandeja: {error}"));
                    }
                    status.set_text(status_text());
                } else if event.id == log.id() {
                    open_log();
                } else if event.id == quit.id() {
                    return;
                }
            }
            if last_status.elapsed() > Duration::from_secs(5) {
                status.set_text(status_text());
                last_status = Instant::now();
            }
            std::thread::sleep(Duration::from_millis(60));
        }
    }
}

#[cfg(windows)]
fn main() {
    tray::main();
}

#[cfg(not(windows))]
fn main() {
    eprintln!("O ícone da bandeja só existe no Windows.");
}
