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
