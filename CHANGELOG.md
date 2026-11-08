# Histórico de versões

O Acervo segue o [versionamento semântico](https://semver.org/lang/pt-BR/): `MAJOR.MINOR.PATCH`.

- **MAJOR**: muda algo que quebra o que existia (ex.: um formato de catálogo que programas antigos não leem).
- **MINOR**: recurso novo que não quebra nada.
- **PATCH**: correção de erro.

A versão do programa fica só no `Cargo.toml` (campo `version`). O formato do catálogo (banco SQLite) tem
número próprio, `SCHEMA_VERSION` em `src/version.rs`, gravado dentro do banco. Ao subir um deles:
1. altere o número;
2. anote aqui o que mudou;
3. gere o instalador (`scripts\build-release.ps1`) e marque a versão no git (`git tag vX.Y.Z`).

## [1.2.3] - 2026-11-08

### Novo
- **AppImage para Linux** (`Acervo-X.Y.Z-x86_64.AppImage`): um arquivo só, roda em qualquer
  distro x86_64 sem instalar. Também há o instalador `scripts/install-linux.sh` (sem sudo, confere o
  SHA-256) e o README explica a instalação e as bibliotecas por distro.

### Corrigido
- No Linux o programa não abria: exigia a variável `APPDATA`. Agora usa `~/.local/share/Acervo`.

## [1.2.2] - 2026-11-08

### Corrigido
- O ícone da bandeja fazia uma janela de console piscar na barra de tarefas a cada 5 segundos
  (a consulta ao estado do serviço abria um console). Agora roda sem janela.

## [1.2.1] - 2026-11-07

### Corrigido
- **"database is locked" na tela Fontes e buscas travando durante varreduras.** A varredura agora
  grava num arquivo temporário e só troca as linhas do catálogo no fim, numa operação de poucos
  segundos. O tempo de espera do banco subiu para 30 s e a limpeza de arquivos apagados deixou de
  falhar a listagem quando o banco está ocupado.
- **Serviço do Windows confiável.** O sinal de vida e as tarefas ficam num banco próprio
  (`service.db`), então o serviço não parece "morto" durante uma varredura longa. A instalação
  remove o serviço antigo de verdade antes de criar o novo, confere que ele iniciou e deu sinal de
  vida, e mostra o erro real se algo falhar. Tudo é anotado em `C:\ProgramData\Acervo\service.log`.
- O app avisa quando o serviço não pega a tarefa em 30 s, em vez de esperar para sempre.

### Novo
- **O serviço acompanha tudo pelo diário USN, sem varrer de novo:** arquivos criados, alterados,
  renomeados, movidos e apagados, e agora também **pastas renomeadas ou movidas** (os arquivos de
  dentro mudam de caminho no catálogo na hora).
- **Ícone na bandeja (ícones ocultos)** com o estado do serviço, "Abrir o Acervo", "Parar / iniciar
  o serviço" e "Ver o registro do serviço". Abre junto com o Windows.
- **Aviso no instalador** explicando o serviço, o ícone da bandeja e como remover.
- O serviço inicia junto com o Windows (início atrasado, para não pesar na entrada) e roda com
  prioridade abaixo do normal.
- Em "Adicionar HD" não aparecem mais as unidades que já estão no Acervo, e adicionar uma pasta
  que já está dentro de uma fonte é recusado.
- **Versão para Linux, experimental** (programa e linha de comando, `.tar.gz` no release). Sem
  serviço nem leitura da MFT: usa a varredura por pastas e o vigia de pastas. Ainda assume
  caminhos no estilo do Windows em alguns pontos (separação de Steam/repacks, Cofre), então a
  classificação pode falhar no Linux.

## [1.2.0] - 2026-11-07

### Novo
- **Atualização pelo GitHub Releases.** O Acervo procura o último release ao abrir e a cada 6 horas
  e, se houver versão mais nova, mostra uma faixa na tela Início (Atualizar agora, Ver novidades,
  Ignorar esta versão). Nada é baixado nem instalado sem o clique. O instalador baixado só é aberto
  depois de conferir o SHA-256 do release; só endereços https do GitHub são aceitos.
- A verificação automática pode ser desligada em Configurações > Atualizações (também há o botão
  "Verificar agora").
- O instalador ganhou o modo `--update` (sem janela): atualiza no mesmo lugar, mantém atalhos e
  configurações (inclusive a escolha de usar o serviço) e reabre o Acervo. Instalar de novo por cima
  também atualiza na pasta onde já estava.

## [1.1.0] - 2026-11-07

### Novo
- **Backup e restauração** (Configurações): um arquivo `.acervo-backup` leva fontes, arquivos
  catalogados, miniaturas e configurações, para guardar antes de formatar o PC. Restaurar guarda
  antes uma cópia do catálogo atual (`antes-de-restaurar.acervo-backup`) e recusa backups de uma
  versão mais nova do programa.
- **Desinstalar pelo app** (Configurações), com aviso para fazer o backup antes.
- **GitHub Actions**: `CI` roda os testes a cada envio; `Release` (manual) confere a versão,
  testa, compila otimizado, monta o instalador e cria o release com o SHA-256.

### Corrigido
- Janela travando: ler/gravar o banco e conferir se os HDs estão conectados (que pode demorar com
  HD dormindo) saíram da thread da janela (lista de fontes, unidades, remover fonte, configurações).
- Remover uma fonte grande não congela mais a tela.
- Adicionar fonte durante outra varredura não trava mais a janela esperando o banco.
- A tela não é mais refeita toda hora em discos com muita escrita (avisos juntados em no máximo
  um a cada 3 segundos); a contagem de duplicados é guardada por 2 minutos.
- A grade de instaladores mostra os 300 primeiros (com busca) em vez de milhares de cartões.
- Miniaturas já vistas ficam em memória entre telas (cache com descarte só quando enche).

## [1.0.0] - 2026-11-07

Primeira versão completa.

### Catálogo e busca
- Catálogo de arquivos de HDs e pastas em SQLite, que continua pesquisável com o HD desconectado.
- Busca instantânea por um índice de nomes em memória (vários termos, filtros por tipo, fonte e
  tamanho, ordenação), com paginação de 100 itens.
- O índice é atualizado só com o que mudou (arquivo criado ou apagado) e é salvo em disco para o app
  abrir rápido.
- O catálogo se atualiza sozinho quando arquivos são criados, apagados ou renomeados nas fontes
  conectadas; a Biblioteca também confere no disco o que mostra.

### Varredura
- Varredura por pastas (qualquer disco) e leitura direta da tabela de arquivos do NTFS (MFT), mais
  rápida, para HDs inteiros.
- Ignora arquivos e pastas do sistema e arquivos ocultos.
- Serviço do Windows (obrigatório na instalação): lê a MFT e acompanha o diário USN do NTFS sem pedir
  administrador a cada uso. Modos: perguntar a cada vez, usar o serviço ou nunca usar administrador.

### Telas
- Início, Biblioteca (lista ou miniaturas), Jogos (Steam com capas locais, instaladores, repacks),
  Fontes, Duplicados (com conferência de conteúdo e envio para a Lixeira), Cofre e Configurações.
- Arquivos da Steam ficam separados dos demais.
- Confirmação antes de ações que mudam ou apagam coisas.

### Miniaturas
- Pequenas (2 a 4 KB), guardadas no próprio SQLite, usando o cache de miniaturas do Windows.
- Geração em lote só para arquivos que podem ter miniatura; cache em memória entre telas.

### Cofre
- Catálogo dentro de um volume VeraCrypt: o Acervo nunca vê a senha; bloqueio automático.

### Instalador
- Escolha da pasta, serviço do Windows, atalhos (Área de Trabalho e Menu Iniciar), ícone e
  desinstalador em "Aplicativos e recursos".

### Formatos
- Catálogo v2 (`PRAGMA user_version = 2`), índice salvo v3.
