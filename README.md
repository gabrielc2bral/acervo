<img src="assets/acervo-256.png" alt="Ícone do Acervo: três disquetes empilhados" width="96" align="right">

# Acervo

Catálogo de arquivos para quem guarda coisa demais em HD demais.

Você aponta o Acervo para um HD ou uma pasta, ele lê nomes, tamanhos e datas e guarda tudo num banco SQLite local. Depois dá para buscar, filtrar e ver miniaturas mesmo com o HD na gaveta.

Comecei por causa dos meus próprios HDs: jogos, instaladores, repacks, fotos, vídeos e livros espalhados em vários discos, e eu nunca lembrava onde estava cada coisa. É um programa para Windows (com uma versão experimental para Linux), escrito em Rust, com interface em [Slint](https://slint.dev).

## Instalar

Baixe o `Acervo-Setup-X.Y.Z.exe` na página de [Releases](../../releases) e execute. O instalador deixa escolher a pasta, instala o serviço do Windows (obrigatório, veja abaixo), cria os atalhos e aparece em "Aplicativos e recursos" para desinstalar. O Windows pede permissão de administrador uma vez.

O instalador não é assinado, então o SmartScreen pode avisar na primeira execução ("Mais informações" > "Executar assim mesmo"). Cada release traz o SHA-256 do instalador para conferir.

Requisitos: Windows 10 ou 11, 64 bits. Para o Cofre, o [VeraCrypt](https://veracrypt.fr) instalado.

### Linux (experimental)

**AppImage (o mais simples).** Baixe o `Acervo-X.Y.Z-x86_64.AppImage` em [Releases](../../releases), dê permissão e abra. Não precisa instalar nem de sudo, e dá para guardar num pendrive:

```
chmod +x Acervo-*-x86_64.AppImage
./Acervo-*-x86_64.AppImage
```

Se der erro de FUSE, instale o `libfuse2` (no Ubuntu 24.04: `sudo apt install libfuse2t64`), ou rode com `--appimage-extract-and-run`. Quem usa o [AppImageLauncher](https://github.com/TheAssassin/AppImageLauncher) ganha o atalho no menu automaticamente.

**Instalar pelo terminal.** Um comando só, sem sudo. Ele baixa o último release, confere o SHA-256, instala em `~/.local/share/acervo` e cria o atalho no menu de aplicativos:

```
curl -fsSL https://raw.githubusercontent.com/gabrielc2bral/acervo/main/scripts/install-linux.sh | sh
```

Funciona em qualquer distribuição x86_64 com glibc recente (Ubuntu 22.04 ou mais novo, Debian 12, Fedora, Arch, openSUSE, Mint, Pop!_OS...). Para atualizar, rode o mesmo comando de novo. Para remover:

```
curl -fsSL https://raw.githubusercontent.com/gabrielc2bral/acervo/main/scripts/install-linux.sh | sh -s -- --remover
```

Prefere fazer na mão? Baixe o `Acervo-X.Y.Z-linux-x86_64.tar.gz` em [Releases](../../releases), extraia e rode `./acervo` (janela) ou `./acervo-cli` (terminal). O catálogo fica em `~/.local/share/Acervo`.

Se a janela não abrir por falta de biblioteca, instale as de interface da sua distro:

| Distro | Comando |
| --- | --- |
| Ubuntu, Debian, Mint | `sudo apt install libfontconfig1 libxkbcommon0 libxkbcommon-x11-0 libwayland-client0 libgl1` |
| Fedora | `sudo dnf install fontconfig libxkbcommon libxkbcommon-x11 wayland-libs-client mesa-libGL` |
| Arch, Manjaro | `sudo pacman -S fontconfig libxkbcommon libxkbcommon-x11 wayland mesa` |

No Linux ele varre por pastas e acompanha as mudanças enquanto está aberto. Não existem o serviço, a leitura da MFT, o Cofre com VeraCrypt nem as miniaturas do Windows. A separação de jogos e repacks ainda pode errar, porque foi pensada para caminhos do Windows. Se algo quebrar, abra uma issue com a distro e a versão.

## O que ele faz

**Catálogo que funciona offline.** O catálogo guarda nome, pasta, tamanho, data e tipo de cada arquivo (não lê o conteúdo). Fontes desconectadas aparecem como offline, mas continuam na busca.

**Busca rápida.** Os nomes ficam num índice em memória, então a busca não passa pelo banco. No meu catálogo, com 1,26 milhão de arquivos, uma busca leva de 1 a 9 ms. Aceita várias palavras (todas precisam aparecer), filtros por tipo, fonte e tamanho, ordenação, e a lista é paginada de 100 em 100. O índice é salvo em disco para o programa abrir já pronto.

**Varredura.** Em HD NTFS inteiro ele lê a tabela de arquivos (MFT) direto do disco, o que é bem mais rápido que percorrer pasta por pasta, mas exige administrador. Em pastas e em discos que não são NTFS, percorre as pastas normalmente. Arquivos e pastas do sistema (Windows, Program Files, AppData...) e arquivos ocultos ficam de fora.

**Catálogo sempre atualizado.** O Acervo observa as fontes conectadas e acerta o catálogo quando algo é criado, apagado ou renomeado. O serviço do Windows faz o mesmo pelo diário USN do NTFS, inclusive com o programa fechado. Mesmo assim, a Biblioteca confere no disco o que está mostrando.

**Jogos.** Os jogos da Steam são lidos dos arquivos da própria Steam (sem login), com as capas que ela já guardou no PC. Os arquivos dentro de `steamapps` ficam separados do resto. Instaladores e pastas de repack (FitGirl e parecidos) têm abas próprias.

**Miniaturas.** Usam o cache de miniaturas do Windows e ficam guardadas no próprio banco, de 2 a 4 KB cada, então aparecem com o HD desconectado. O botão "Gerar miniaturas" pede ao Windows as que faltam, só para arquivos que podem ter uma (foto, vídeo, PDF, ícone de programa).

**Duplicados.** Agrupa arquivos de mesmo nome e tamanho (a partir de 1 MB). Antes de apagar uma cópia, confere que o arquivo ainda existe e compara o começo, o meio e o fim com a cópia que fica. Apagar manda para a Lixeira do Windows.

**Cofre.** O catálogo de itens privados fica dentro de um volume do VeraCrypt: sem o volume montado, nada dele existe no PC. O Acervo chama o VeraCrypt, que pede a senha na janela dele. O programa nunca vê a senha. O Cofre bloqueia sozinho depois de um tempo parado.

**Backup.** Em Configurações, "Fazer backup" gera um único arquivo `.acervo-backup` com fontes, arquivos catalogados, miniaturas e configurações. É para guardar antes de formatar o PC. Depois de reinstalar, "Restaurar backup" traz tudo de volta (e guarda antes uma cópia do catálogo que estava lá).

**Atualizações.** O Acervo olha os releases deste repositório ao abrir e a cada 6 horas. Se houver versão mais nova, aparece uma faixa na tela Início; o clique em "Atualizar agora" baixa o instalador, confere o SHA-256 publicado no release e abre o instalador, que atualiza no mesmo lugar e mantém seus dados. Não baixa nada sozinho, só aceita endereços https do GitHub, e dá para desligar em Configurações.

## O serviço do Windows

A leitura da MFT e do diário USN precisa de administrador. Para o programa não pedir permissão a cada varredura, o instalador registra um serviço (`AcervoService`, conta do sistema) que faz esse trabalho e conversa com o programa por uma tabela no próprio catálogo. Ele só lê nomes e tamanhos e grava no seu catálogo.

Em Configurações dá para escolher: usar o serviço, perguntar a cada vez (permissão do Windows) ou nunca usar administrador (varredura por pastas, mais lenta).

## Compilar

Precisa do Rust estável (toolchain MSVC) e das Build Tools do Visual Studio com o Windows SDK.

```
cargo test
cargo build --release
```

Para gerar o instalador (compila tudo otimizado, embute os programas e escreve `dist\Acervo-Setup-X.Y.Z.exe`):

```
powershell -ExecutionPolicy Bypass -File scripts\build-release.ps1
```

Os executáveis:

| Programa | Para quê |
| --- | --- |
| `acervo` | a janela |
| `acervo-cli` | linha de comando: varrer, buscar, estatísticas, diagnóstico |
| `acervo-service` | o serviço do Windows (instalar, remover, rodar) |
| `acervo-setup` | o instalador e o desinstalador |

Onde está cada coisa em `src/`: `db.rs` (catálogo SQLite), `scan.rs` e `fast.rs` (varredura por pastas e pela MFT), `index.rs` (índice em memória), `watch.rs` (acompanha as pastas), `service.rs` (serviço e diário USN), `thumbs.rs`, `dedupe.rs`, `steam.rs`, `vault.rs`, `backup.rs`, e `ui.rs` com as telas em `ui/*.slint`.

## Versões

O Acervo segue [versionamento semântico](https://semver.org/lang/pt-BR/). A versão do programa fica no `Cargo.toml`, o formato do catálogo tem número próprio (gravado no banco, e um catálogo de versão mais nova que o programa não é aberto) e o histórico está no [CHANGELOG](CHANGELOG.md).

Para publicar uma versão: suba o número no `Cargo.toml`, escreva a seção no `CHANGELOG.md`, envie as mudanças e rode o fluxo **Release** na aba Actions informando a versão. Ele confere tudo, roda os testes, compila, monta o instalador e cria o release com o SHA-256. O fluxo **CI** roda os testes a cada envio.

## Limites

- Feito para Windows. A versão para Linux é experimental e tem menos recursos (veja acima).
- Busca só por nome, não pelo conteúdo dos arquivos.
- A leitura da MFT, o diário USN e o serviço dependem de administrador e ainda foram pouco usados fora do meu PC. Se algo falhar, o programa cai para a varredura por pastas.
- Miniaturas de vídeo dependem de o Windows saber abrir o formato; sem isso o arquivo fica com um quadrado com a extensão.
- Uma pasta renomeada fora do programa só aparece no novo lugar depois de varrer a fonte de novo.
- Ainda não tem licença definida.
