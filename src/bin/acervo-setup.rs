//! Instalador e desinstalador do Acervo (um arquivo só).
//!
//! Instalar: escolhe a pasta, copia o programa, instala o serviço do Windows (obrigatório), cria
//! os atalhos e registra o Acervo em "Aplicativos e recursos". Os arquivos vão embutidos neste
//! executável (pasta `payload/`, preenchida por `scripts/build-release.ps1`).
//!
//!   acervo-setup.exe                 abre o instalador
//!   uninstall.exe --uninstall        (na pasta instalada) remove tudo
//!   acervo-setup.exe --test-dir <p>  só extrai os arquivos em <p>, sem administrador nem serviço
//!
//! Precisa de administrador (Arquivos de Programas e serviço): se abrir sem, ele se reabre pedindo
//! a permissão do Windows.

#![cfg_attr(windows, windows_subsystem = "windows")]

#[cfg(windows)]
mod windows_main {
    use acervo::{db::Db, fast, service};
    use anyhow::{bail, Context, Result};
    use slint::ComponentHandle;
    use std::os::windows::process::CommandExt;
    use std::path::{Path, PathBuf};
    use std::process::Command;
    use std::time::Duration;

    include!(concat!(env!("OUT_DIR"), "/setup.rs"));

    const APP: &[u8] = include_bytes!("../../payload/acervo.exe");
    const SERVICE: &[u8] = include_bytes!("../../payload/acervo-service.exe");
    const CLI: &[u8] = include_bytes!("../../payload/acervo-cli.exe");
    const ICON_SVG: &[u8] = include_bytes!("../../assets/acervo.svg");
    const ICON_ICO: &[u8] = include_bytes!("../../assets/acervo.ico");

    const UNINSTALL_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Uninstall\Acervo";
    const VERSION: &str = env!("CARGO_PKG_VERSION");
    /// Janela de console escondida nos programas que o instalador chama.
    const NO_WINDOW: u32 = 0x0800_0000;

    /// O que o usuário escolheu, mais as pastas do usuário (o instalador roda como administrador,
    /// então as pastas dele são descobertas antes e repassadas).
    #[derive(Clone)]
    struct Options {
        dir: PathBuf,
        desktop_shortcut: bool,
        start_menu_shortcut: bool,
        remove_catalog: bool,
        appdata: PathBuf,
        desktop_dir: PathBuf,
        programs_dir: PathBuf,
    }

    impl Options {
        fn db_path(&self) -> PathBuf {
            self.appdata.join("Acervo").join("acervo.db")
        }
        fn desktop_link(&self) -> PathBuf {
            self.desktop_dir.join("Acervo.lnk")
        }
        fn start_menu_link(&self) -> PathBuf {
            self.programs_dir.join("Acervo.lnk")
        }
    }

    fn value<'a>(args: &'a [String], name: &str) -> Option<&'a str> {
        args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).map(String::as_str)
    }

    fn is_admin() -> bool {
        #[allow(deprecated)]
        unsafe {
            windows::Win32::UI::Shell::IsUserAnAdmin().as_bool()
        }
    }

    fn quiet(command: &mut Command) -> &mut Command {
        command.creation_flags(NO_WINDOW)
    }

    fn quote(text: &Path) -> String {
        text.to_string_lossy().replace('\'', "''")
    }

    fn known_folder(name: &str) -> Option<PathBuf> {
        let output = quiet(Command::new("powershell").args([
            "-NoProfile",
            "-Command",
            &format!("[Environment]::GetFolderPath('{name}')"),
        ]))
        .output()
        .ok()?;
        let text = String::from_utf8_lossy(&output.stdout).trim().to_string();
        (!text.is_empty()).then(|| PathBuf::from(text))
    }

    fn payload_is_complete() -> bool {
        !APP.is_empty() && !SERVICE.is_empty() && !CLI.is_empty() && !ICON_ICO.is_empty()
    }

    fn write_payload(dir: &Path) -> Result<()> {
        std::fs::create_dir_all(dir)?;
        for (name, bytes) in [
            ("acervo.exe", APP),
            ("acervo-service.exe", SERVICE),
            ("acervo-cli.exe", CLI),
            ("acervo.svg", ICON_SVG),
            ("acervo.ico", ICON_ICO),
        ] {
            std::fs::write(dir.join(name), bytes)
                .with_context(|| format!("não foi possível gravar {name} (feche o Acervo e tente de novo)"))?;
        }
        Ok(())
    }

    fn wait_service_stopped() {
        for _ in 0..40 {
            if service::installed_state() != service::Installed::Running {
                return;
            }
            std::thread::sleep(Duration::from_millis(250));
        }
    }

    fn install(options: &Options, report: &dyn Fn(&str)) -> Result<()> {
        if !payload_is_complete() {
            bail!("Instalador incompleto: gere com scripts\\build-release.ps1");
        }
        let dir = &options.dir;

        report("Parando o Acervo e o serviço antigos, se existirem...");
        if service::installed_state() != service::Installed::No {
            let _ = quiet(Command::new("sc").args(["stop", service::SERVICE_NAME])).status();
            wait_service_stopped();
        }
        let _ = quiet(Command::new("taskkill").args(["/F", "/IM", "acervo.exe"])).status();

        report("Copiando os arquivos...");
        write_payload(dir)?;
        std::fs::copy(std::env::current_exe()?, dir.join("uninstall.exe"))?;

        report("Preparando o catálogo...");
        let db_path = options.db_path();
        Db::open(&db_path)?.set_setting("admin_mode", "service")?;

        report("Instalando o serviço do Windows...");
        let status = quiet(
            Command::new(dir.join("acervo-service.exe"))
                .arg("install")
                .arg("--db")
                .arg(&db_path),
        )
        .status()
        .context("não foi possível rodar o instalador do serviço")?;
        if !status.success() || service::installed_state() == service::Installed::No {
            bail!("O serviço do Windows não foi instalado. Ele é obrigatório para o Acervo.");
        }

        report("Criando os atalhos...");
        let target = dir.join("acervo.exe");
        let icon = dir.join("acervo.ico");
        if options.desktop_shortcut {
            create_shortcut(&options.desktop_link(), &target, &icon, dir)?;
        }
        if options.start_menu_shortcut {
            create_shortcut(&options.start_menu_link(), &target, &icon, dir)?;
        }

        report("Registrando em Aplicativos e recursos...");
        register_uninstall(dir)?;
        Ok(())
    }

    fn create_shortcut(link: &Path, target: &Path, icon: &Path, working_dir: &Path) -> Result<()> {
        if let Some(parent) = link.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let script = format!(
            "$s = (New-Object -ComObject WScript.Shell).CreateShortcut('{}'); $s.TargetPath = '{}'; $s.WorkingDirectory = '{}'; $s.IconLocation = '{}'; $s.Description = 'Acervo - catálogo rápido de arquivos'; $s.Save()",
            quote(link),
            quote(target),
            quote(working_dir),
            quote(icon)
        );
        let status = quiet(Command::new("powershell").args(["-NoProfile", "-Command", &script])).status()?;
        if !status.success() {
            bail!("não foi possível criar o atalho {}", link.display());
        }
        Ok(())
    }

    fn register_uninstall(dir: &Path) -> Result<()> {
        use winreg::{enums::HKEY_LOCAL_MACHINE, RegKey};
        let (key, _) = RegKey::predef(HKEY_LOCAL_MACHINE).create_subkey(UNINSTALL_KEY)?;
        key.set_value("DisplayName", &"Acervo")?;
        key.set_value("DisplayVersion", &VERSION)?;
        key.set_value("Publisher", &"Acervo")?;
        key.set_value("InstallLocation", &dir.to_string_lossy().as_ref())?;
        key.set_value("DisplayIcon", &dir.join("acervo.ico").to_string_lossy().as_ref())?;
        key.set_value(
            "UninstallString",
            &format!("\"{}\" --uninstall", dir.join("uninstall.exe").display()),
        )?;
        key.set_value("NoModify", &1u32)?;
        key.set_value("NoRepair", &1u32)?;
        key.set_value("EstimatedSize", &(((APP.len() + SERVICE.len() + CLI.len()) / 1024) as u32))?;
        Ok(())
    }

    fn uninstall(options: &Options, report: &dyn Fn(&str)) -> Result<()> {
        let dir = &options.dir;
        report("Removendo o serviço do Windows...");
        let service_exe = dir.join("acervo-service.exe");
        if service_exe.is_file() {
            let _ = quiet(Command::new(&service_exe).arg("uninstall")).status();
        } else if service::installed_state() != service::Installed::No {
            let _ = quiet(Command::new("sc").args(["stop", service::SERVICE_NAME])).status();
            let _ = quiet(Command::new("sc").args(["delete", service::SERVICE_NAME])).status();
        }
        let _ = quiet(Command::new("taskkill").args(["/F", "/IM", "acervo.exe"])).status();

        report("Removendo os atalhos...");
        let _ = std::fs::remove_file(options.desktop_link());
        let _ = std::fs::remove_file(options.start_menu_link());

        report("Removendo o registro...");
        {
            use winreg::{enums::HKEY_LOCAL_MACHINE, RegKey};
            let _ = RegKey::predef(HKEY_LOCAL_MACHINE).delete_subkey_all(UNINSTALL_KEY);
        }

        if options.remove_catalog {
            report("Apagando o catálogo...");
            let _ = std::fs::remove_dir_all(options.appdata.join("Acervo"));
        }

        report("Removendo os arquivos...");
        // O próprio desinstalador está nessa pasta: a remoção acontece um instante depois que ele fecha.
        let _ = quiet(Command::new("cmd").args([
            "/C",
            &format!("ping 127.0.0.1 -n 3 > nul & rmdir /s /q \"{}\"", dir.display()),
        ]))
        .spawn();
        Ok(())
    }

    fn test_extract(dir: &Path) -> Result<()> {
        let log = dir.join("instalacao-teste.txt");
        std::fs::create_dir_all(dir)?;
        let result = if !payload_is_complete() {
            Err(anyhow::anyhow!("instalador incompleto (payload vazio)"))
        } else {
            write_payload(dir)
        };
        let text = match &result {
            Ok(()) => format!(
                "ok: acervo.exe {} bytes, acervo-service.exe {} bytes, acervo-cli.exe {} bytes",
                APP.len(),
                SERVICE.len(),
                CLI.len()
            ),
            Err(error) => format!("erro: {error}"),
        };
        std::fs::write(log, text)?;
        result
    }

    pub fn main() -> Result<()> {
        let args: Vec<String> = std::env::args().skip(1).collect();
        if let Some(dir) = value(&args, "--test-dir") {
            return test_extract(Path::new(dir));
        }
        let uninstall_mode = args.iter().any(|a| a == "--uninstall");

        // Sem administrador: descobre as pastas do usuário e se reabre pedindo a permissão.
        if !is_admin() && !args.iter().any(|a| a == "--elevated") {
            let appdata = std::env::var("APPDATA").unwrap_or_default();
            let desktop = known_folder("Desktop").unwrap_or_default();
            let programs = known_folder("Programs").unwrap_or_default();
            let mut forward = vec![
                "--elevated".to_string(),
                "--appdata".to_string(),
                appdata,
                "--desktop".to_string(),
                desktop.to_string_lossy().into_owned(),
                "--programs".to_string(),
                programs.to_string_lossy().into_owned(),
            ];
            if uninstall_mode {
                forward.push("--uninstall".to_string());
            }
            let _ = fast::run_elevated(&std::env::current_exe()?, &forward);
            return Ok(());
        }

        let path_arg = |name: &str, fallback: Option<PathBuf>| {
            value(&args, name)
                .filter(|text| !text.is_empty())
                .map(PathBuf::from)
                .or(fallback)
                .unwrap_or_default()
        };
        let default_dir = if uninstall_mode {
            std::env::current_exe()?.parent().map(Path::to_path_buf).unwrap_or_default()
        } else {
            PathBuf::from(std::env::var("ProgramFiles").unwrap_or_else(|_| r"C:\Program Files".into())).join("Acervo")
        };
        let base = Options {
            dir: default_dir.clone(),
            desktop_shortcut: true,
            start_menu_shortcut: true,
            remove_catalog: false,
            appdata: path_arg("--appdata", std::env::var_os("APPDATA").map(PathBuf::from)),
            desktop_dir: path_arg("--desktop", known_folder("Desktop")),
            programs_dir: path_arg("--programs", known_folder("Programs")),
        };

        let ui = SetupWindow::new()?;
        ui.set_uninstall_mode(uninstall_mode);
        ui.set_version(VERSION.into());
        ui.set_install_dir(default_dir.to_string_lossy().as_ref().into());
        if !uninstall_mode && !payload_is_complete() {
            ui.set_status("Instalador incompleto: gere com scripts\\build-release.ps1".into());
        }

        ui.on_browse({
            let weak = ui.as_weak();
            move || {
                let Some(ui) = weak.upgrade() else { return };
                if let Some(mut folder) = rfd::FileDialog::new().set_title("Escolha a pasta de instalação").pick_folder() {
                    if folder.file_name().is_some_and(|name| name != "Acervo") {
                        folder.push("Acervo");
                    }
                    ui.set_install_dir(folder.to_string_lossy().as_ref().into());
                }
            }
        });

        ui.on_start({
            let weak = ui.as_weak();
            move || {
                let Some(ui) = weak.upgrade() else { return };
                let mut options = base.clone();
                options.dir = PathBuf::from(ui.get_install_dir().as_str());
                options.desktop_shortcut = ui.get_desktop_shortcut();
                options.start_menu_shortcut = ui.get_start_menu_shortcut();
                options.remove_catalog = ui.get_remove_catalog();
                if options.dir.as_os_str().is_empty() {
                    ui.set_status("Escolha uma pasta de instalação.".into());
                    return;
                }
                ui.set_busy(true);
                ui.set_status("Começando...".into());

                let thread_weak = weak.clone();
                std::thread::spawn(move || {
                    let report_weak = thread_weak.clone();
                    let report = move |text: &str| {
                        let text = text.to_string();
                        let _ = report_weak.upgrade_in_event_loop(move |ui| ui.set_status(text.into()));
                    };
                    let result = if uninstall_mode { uninstall(&options, &report) } else { install(&options, &report) };
                    let _ = thread_weak.upgrade_in_event_loop(move |ui| {
                        ui.set_busy(false);
                        ui.set_done(true);
                        match result {
                            Ok(()) => {
                                ui.set_failed(false);
                                ui.set_status(
                                    if uninstall_mode {
                                        "Tudo removido. O catálogo foi mantido, a menos que você tenha pedido para apagar.".to_string()
                                    } else {
                                        format!("Instalado em {}. O serviço do Windows já está ativo.", options.dir.display())
                                    }
                                    .into(),
                                );
                            }
                            Err(error) => {
                                ui.set_failed(true);
                                ui.set_status(format!("{error}").into());
                            }
                        }
                    });
                });
            }
        });

        ui.on_open_app({
            let weak = ui.as_weak();
            move || {
                if let Some(ui) = weak.upgrade() {
                    // O explorer abre o programa com a conta normal, não como administrador.
                    let app = PathBuf::from(ui.get_install_dir().as_str()).join("acervo.exe");
                    let _ = Command::new("explorer").arg(app).spawn();
                }
                let _ = slint::quit_event_loop();
            }
        });
        ui.on_close_window(|| {
            let _ = slint::quit_event_loop();
        });

        ui.run()?;
        Ok(())
    }
}

#[cfg(windows)]
fn main() -> anyhow::Result<()> {
    windows_main::main()
}

#[cfg(not(windows))]
fn main() {
    eprintln!("O instalador do Acervo só existe para Windows.");
}
