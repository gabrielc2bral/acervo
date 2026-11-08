use std::path::Path;

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=ui");
    println!("cargo:rerun-if-changed=assets");

    // O instalador embute estes arquivos (`payload/`). Sem eles o projeto ainda compila, com
    // arquivos vazios; o instalador recusa rodar assim. Ver `scripts/build-release.ps1`.
    let payload = Path::new("payload");
    let _ = std::fs::create_dir_all(payload);
    for name in ["acervo.exe", "acervo-service.exe", "acervo-cli.exe", "acervo-tray.exe"] {
        let file = payload.join(name);
        if !file.exists() {
            let _ = std::fs::write(file, b"");
        }
    }

    let config = || slint_build::CompilerConfiguration::new().with_style("fluent-dark".into());
    // O instalador usa o arquivo gerado direto (setup.rs). O app usa `slint::include_modules!()`,
    // que aponta para o ÚLTIMO arquivo compilado: por isso o app é compilado por último.
    slint_build::compile_with_config("ui/setup.slint", config()).expect("falha ao compilar a tela do instalador");
    slint_build::compile_with_config("ui/app.slint", config()).expect("falha ao compilar a interface");

    // Ícone do programa (o .exe e a barra de tarefas). Se o recurso não compilar, o programa
    // continua funcionando, só sem o ícone no arquivo.
    #[cfg(windows)]
    if Path::new("assets/acervo.ico").exists() {
        let mut resource = winresource::WindowsResource::new();
        resource.set_icon("assets/acervo.ico");
        resource.set("ProductName", "Acervo");
        resource.set("FileDescription", "Acervo - catálogo rápido de arquivos");
        if let Err(error) = resource.compile() {
            println!("cargo:warning=não foi possível embutir o ícone: {error}");
        }
    }
}
