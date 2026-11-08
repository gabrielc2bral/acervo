//! Catálogo em SQLite. O mesmo código abre o banco normal e o banco do Cofre
//! (que fica dentro do volume VeraCrypt montado).

use anyhow::Result;
use rusqlite::{params, Connection, OptionalExtension};
use std::path::{Path, PathBuf};
use std::time::Duration;

pub struct Db {
    pub conn: Connection,
}

pub struct Source {
    pub id: i64,
    pub name: String,
    pub path: String,
    pub last_scan: Option<i64>,
}

pub struct FileRow {
    pub id: i64,
    pub source_id: i64,
    pub name: String,
    pub path: String,
    pub size: i64,
    pub kind: String,
    pub source_name: String,
    pub source_path: String,
    pub mtime: Option<i64>,
}

/// Arquivos com mesmo nome e tamanho em mais de um lugar.
pub struct DuplicateGroup {
    pub name: String,
    pub size: i64,
    pub copies: i64,
    pub kind: String,
    /// Caminho de uma das cópias (para a miniatura).
    pub first_path: String,
}

/// Ordenação da busca.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Sort {
    #[default]
    Name,
    Largest,
    Smallest,
    Newest,
    Oldest,
}

impl Sort {
    fn order_by(self) -> &'static str {
        match self {
            // O desempate por `id` no mesmo sentido deixa o SQLite andar direto pelo índice
            // (sem ordenar tudo) e mantém as páginas estáveis.
            Sort::Name => "f.name COLLATE NOCASE, f.id",
            Sort::Largest => "f.size DESC, f.id DESC",
            Sort::Smallest => "f.size ASC, f.id",
            Sort::Newest => "f.mtime DESC, f.id DESC",
            Sort::Oldest => "f.mtime ASC, f.id",
        }
    }
}

/// Filtros da busca. `kind: None` mostra tudo menos os arquivos da Steam.
#[derive(Clone, Debug, Default)]
pub struct SearchQuery {
    pub text: String,
    pub kind: Option<String>,
    /// Vazio = todas as fontes.
    pub source_ids: Vec<i64>,
    pub min_size: i64,
    pub sort: Sort,
    /// Linhas por página.
    pub limit: usize,
    /// Quantas linhas pular (página * limite).
    pub offset: usize,
}

impl SearchQuery {
    /// `WHERE ...` montado só com os filtros usados (sem `LIKE` quando não há texto, para o
    /// SQLite poder andar pelo índice de ordenação) e os valores na ordem dos `?`.
    fn where_clause(&self) -> (String, Vec<rusqlite::types::Value>) {
        let mut values: Vec<rusqlite::types::Value> = Vec::new();
        let mut sql = String::from("WHERE 1 = 1");
        if !self.text.is_empty() {
            sql.push_str(" AND f.name LIKE ? ESCAPE '\\'");
            values.push(like_pattern(&self.text).into());
        }
        match &self.kind {
            Some(kind) => {
                sql.push_str(" AND f.kind = ?");
                values.push(kind.clone().into());
            }
            None => sql.push_str(" AND f.kind != 'steam'"),
        }
        if self.min_size > 0 {
            sql.push_str(" AND f.size >= ?");
            values.push(self.min_size.into());
        }
        if !self.source_ids.is_empty() {
            let ids: Vec<String> = self.source_ids.iter().map(i64::to_string).collect();
            sql.push_str(&format!(" AND f.source_id IN ({})", ids.join(",")));
        }
        (sql, values)
    }
}

/// Texto para usar dentro de um `LIKE ... ESCAPE '\'`: `%`, `_` e `\` valem como texto comum.
fn escape_like(text: &str) -> String {
    text.replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_")
}

/// `%texto%`.
fn like_pattern(text: &str) -> String {
    format!("%{}%", escape_like(text))
}

/// Uma cópia de um arquivo duplicado.
pub struct CopyRow {
    pub id: i64,
    pub path: String,
    pub source_name: String,
    pub mtime: Option<i64>,
}

/// Pasta de um repack (contém `fg-*.bin`).
pub struct RepackFolder {
    pub dir: String,
    pub bytes: i64,
    pub files: i64,
    pub source_name: String,
}

pub struct Totals {
    pub files: i64,
    pub bytes: i64,
}

/// Duplicados só contam a partir de 1 MB, para não listar milhares de arquivos pequenos iguais.
const MIN_DUPLICATE_SIZE: i64 = 1_048_576;

impl Db {
    /// Banco principal: `%APPDATA%\Acervo\acervo.db` no Windows e
    /// `~/.local/share/Acervo/acervo.db` (ou `$XDG_DATA_HOME`) no Linux.
    pub fn default_path() -> Result<PathBuf> {
        let base = if cfg!(windows) {
            std::env::var_os("APPDATA").map(PathBuf::from)
        } else {
            std::env::var_os("XDG_DATA_HOME")
                .filter(|value| !value.is_empty())
                .map(PathBuf::from)
                .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".local").join("share")))
        };
        let base = base.ok_or_else(|| anyhow::anyhow!("não achei a pasta de dados do usuário (APPDATA ou HOME)"))?;
        Ok(base.join("Acervo").join("acervo.db"))
    }

    pub fn open(path: &Path) -> Result<Db> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        Self::init(Connection::open(path)?)
    }

    pub fn open_in_memory() -> Result<Db> {
        Self::init(Connection::open_in_memory()?)
    }

    fn init(conn: Connection) -> Result<Db> {
        // Outra conexão (a varredura, o serviço) pode estar gravando: espera até 30 s em vez de
        // falhar na hora com "database is locked". Quem só lê (a busca) nunca espera, por causa do WAL.
        conn.busy_timeout(Duration::from_secs(30))?;
        // `acervo_key(caminho)`: a mesma chave das miniaturas, para ligar miniatura -> arquivo -> fonte.
        conn.create_scalar_function(
            "acervo_key",
            1,
            rusqlite::functions::FunctionFlags::SQLITE_UTF8
                | rusqlite::functions::FunctionFlags::SQLITE_DETERMINISTIC,
            |ctx| {
                let path: String = ctx.get(0)?;
                Ok(crate::thumbs::key_for(&path))
            },
        )?;
        conn.execute_batch(
            // WAL: quem lê (busca, miniaturas, telas) não fica bloqueado enquanto uma varredura
            // grava. Sem isso, durante uma varredura grande a janela recebia "database is locked".
            "PRAGMA journal_mode = WAL;
             PRAGMA synchronous = NORMAL;
             PRAGMA foreign_keys = ON;
             CREATE TABLE IF NOT EXISTS sources (
                 id        INTEGER PRIMARY KEY,
                 name      TEXT NOT NULL,
                 path      TEXT NOT NULL UNIQUE,
                 last_scan INTEGER
             );
             CREATE TABLE IF NOT EXISTS files (
                 id        INTEGER PRIMARY KEY,
                 source_id INTEGER NOT NULL REFERENCES sources(id) ON DELETE CASCADE,
                 dir       TEXT NOT NULL,
                 name      TEXT NOT NULL,
                 size      INTEGER NOT NULL,
                 mtime     INTEGER,
                 kind      TEXT NOT NULL
             );
             CREATE TABLE IF NOT EXISTS settings (
                 key   TEXT PRIMARY KEY,
                 value TEXT NOT NULL
             );
             CREATE INDEX IF NOT EXISTS files_name ON files(name COLLATE NOCASE);
             CREATE INDEX IF NOT EXISTS files_source ON files(source_id);
             CREATE INDEX IF NOT EXISTS files_dir ON files(dir);
             CREATE INDEX IF NOT EXISTS files_dupes ON files(size, name);
             CREATE INDEX IF NOT EXISTS files_mtime ON files(mtime);
             CREATE INDEX IF NOT EXISTS files_size ON files(size);
             CREATE TABLE IF NOT EXISTS thumbs (
                 key  INTEGER PRIMARY KEY,
                 data BLOB NOT NULL
             );
             -- Registro das mudanças feitas no catálogo sem varredura (arquivo criado/apagado):
             -- quem mantém o índice em memória lê daqui e aplica só o que mudou.
             CREATE TABLE IF NOT EXISTS changes (
                 seq     INTEGER PRIMARY KEY AUTOINCREMENT,
                 op      TEXT NOT NULL,
                 file_id INTEGER NOT NULL
             );
             -- (As tarefas do serviço do Windows ficam no service.db, ver `link`.)
             ",
        )?;
        // `frn`: número do arquivo no NTFS. Com ele o diário USN apaga/atualiza o arquivo certo
        // sem precisar saber o caminho. Catálogos antigos ganham a coluna vazia (sem refazer nada).
        let has_frn = {
            let mut stmt = conn.prepare("PRAGMA table_info(files)")?;
            let names: Vec<String> = stmt
                .query_map([], |r| r.get::<_, String>(1))?
                .filter_map(std::result::Result::ok)
                .collect();
            names.iter().any(|name| name == "frn")
        };
        if !has_frn {
            conn.execute("ALTER TABLE files ADD COLUMN frn INTEGER", [])?;
        }
        conn.execute_batch(
            "CREATE INDEX IF NOT EXISTS files_frn ON files(source_id, frn) WHERE frn IS NOT NULL;",
        )?;
        Self::check_schema_version(&conn)?;
        Ok(Db { conn })
    }

    /// Confere e atualiza o número de versão do catálogo (`PRAGMA user_version`).
    /// Os passos de criação acima são idempotentes e já deixam o banco no formato atual;
    /// aqui só se registra a versão e se recusa abrir um catálogo de um programa mais novo.
    fn check_schema_version(conn: &Connection) -> Result<()> {
        let found: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
        if found > crate::version::SCHEMA_VERSION {
            anyhow::bail!(
                "Este catálogo foi criado por uma versão mais nova do Acervo (formato v{found}; este programa entende até v{}). Atualize o Acervo.",
                crate::version::SCHEMA_VERSION
            );
        }
        if found < crate::version::SCHEMA_VERSION {
            conn.execute_batch(&format!("PRAGMA user_version = {}", crate::version::SCHEMA_VERSION))?;
        }
        Ok(())
    }

    /// Versão do formato gravada no catálogo.
    pub fn schema_version(&self) -> Result<i64> {
        Ok(self.conn.query_row("PRAGMA user_version", [], |r| r.get(0))?)
    }

    // ------------------------------------------------ Registro de mudanças (índice em memória)

    pub fn log_change(&self, op: &str, file_id: i64) -> Result<()> {
        self.conn.execute(
            "INSERT INTO changes (op, file_id) VALUES (?1, ?2)",
            params![op, file_id],
        )?;
        Ok(())
    }

    /// Mudanças depois de `seq`: (seq, "add" ou "del", id do arquivo), em ordem.
    pub fn changes_since(&self, seq: i64, limit: usize) -> Result<Vec<(i64, String, i64)>> {
        let mut stmt = self
            .conn
            .prepare("SELECT seq, op, file_id FROM changes WHERE seq > ?1 ORDER BY seq LIMIT ?2")?;
        let rows = stmt.query_map(params![seq, limit as i64], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?))
        })?;
        Ok(rows.collect::<std::result::Result<_, _>>()?)
    }

    pub fn last_change_seq(&self) -> Result<i64> {
        Ok(self
            .conn
            .query_row("SELECT COALESCE(MAX(seq), 0) FROM changes", [], |r| r.get(0))?)
    }

    /// Mantém só as últimas mudanças (o registro não precisa crescer para sempre).
    pub fn trim_changes(&self, keep: i64) -> Result<()> {
        let last = self.last_change_seq()?;
        self.conn
            .execute("DELETE FROM changes WHERE seq <= ?1", params![last - keep])?;
        Ok(())
    }

    /// Identifica o estado do catálogo (quantidade, maior id, soma dos tamanhos). O índice salvo
    /// em disco só vale se isso for igual ao de quando ele foi salvo.
    pub fn catalog_stamp(&self) -> Result<(i64, i64, i64)> {
        Ok(self.conn.query_row(
            "SELECT COUNT(*), COALESCE(MAX(id), 0), COALESCE(SUM(size), 0) FROM files",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )?)
    }

    pub fn get_setting(&self, key: &str) -> Result<Option<String>> {
        Ok(self
            .conn
            .query_row("SELECT value FROM settings WHERE key = ?1", params![key], |r| {
                r.get(0)
            })
            .optional()?)
    }

    pub fn set_setting(&self, key: &str, value: &str) -> Result<()> {
        self.conn.execute(
            "INSERT INTO settings (key, value) VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![key, value],
        )?;
        Ok(())
    }

    /// Cadastra a fonte (HD ou pasta) ou devolve o id da que já existe com o mesmo caminho.
    pub fn add_source(&self, name: &str, path: &str) -> Result<i64> {
        self.conn.execute(
            "INSERT OR IGNORE INTO sources (name, path) VALUES (?1, ?2)",
            params![name, path],
        )?;
        Ok(self.conn.query_row(
            "SELECT id FROM sources WHERE path = ?1",
            params![path],
            |r| r.get(0),
        )?)
    }

    /// Separa `D:\pasta\arq.txt` em (`D:\pasta`, `arq.txt`). Na raiz do disco a pasta fica `D:\`,
    /// como a varredura grava.
    pub fn split_path(path: &str) -> (String, String) {
        let path = path.trim_end_matches('\\');
        match path.rsplit_once('\\') {
            Some((dir, name)) if dir.ends_with(':') => (format!("{dir}\\"), name.to_string()),
            Some((dir, name)) => (dir.to_string(), name.to_string()),
            None => (String::new(), path.to_string()),
        }
    }

    /// Tira do catálogo um arquivo e, se era uma pasta, tudo que estava dentro dela.
    /// Devolve quantas linhas saíram.
    pub fn remove_path(&self, source_id: i64, path: &str) -> Result<usize> {
        let path = path.trim_end_matches('\\');
        let (dir, name) = Self::split_path(path);
        let mut ids: Vec<i64> = self.ids_where(
            "SELECT id FROM files WHERE source_id = ?1 AND dir = ?2 AND name = ?3",
            params![source_id, dir, name],
        )?;
        // `\\` + `%` no padrão = "pasta\" seguida de qualquer coisa (subpastas).
        let inside = format!("{}\\\\%", escape_like(path));
        ids.extend(self.ids_where(
            "SELECT id FROM files WHERE source_id = ?1 AND (dir = ?2 OR dir LIKE ?3 ESCAPE '\\')",
            params![source_id, path, inside],
        )?);
        self.delete_files(&ids)?;
        Ok(ids.len())
    }

    /// Tira do catálogo o arquivo com esse número do NTFS (usado pelo diário USN).
    pub fn remove_by_frn(&self, source_id: i64, frn: i64) -> Result<usize> {
        let ids = self.ids_where(
            "SELECT id FROM files WHERE source_id = ?1 AND frn = ?2",
            params![source_id, frn],
        )?;
        self.delete_files(&ids)?;
        Ok(ids.len())
    }

    /// Pasta renomeada ou movida: tudo que estava dentro dela passa para o caminho novo, sem
    /// varrer o disco. As linhas antigas saem e as novas entram no registro de mudanças, para o
    /// índice em memória acompanhar. Devolve quantos arquivos mudaram de lugar.
    pub fn move_dir(&self, source_id: i64, old_dir: &str, new_dir: &str) -> Result<usize> {
        let old_dir = old_dir.trim_end_matches('\\');
        let new_dir = new_dir.trim_end_matches('\\');
        type Row = (i64, String, String, i64, Option<i64>, String, Option<i64>);
        let inside = format!("{}\\\\%", escape_like(old_dir));
        let rows: Vec<Row> = {
            let mut stmt = self.conn.prepare(
                "SELECT id, dir, name, size, mtime, kind, frn FROM files
                 WHERE source_id = ?1 AND (dir = ?2 OR dir LIKE ?3 ESCAPE '\\')",
            )?;
            let found = stmt.query_map(params![source_id, old_dir, inside], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?))
            })?;
            found.collect::<std::result::Result<_, _>>()?
        };
        let ids: Vec<i64> = rows.iter().map(|row| row.0).collect();
        self.delete_files(&ids)?;
        for (_, dir, name, size, mtime, kind, frn) in &rows {
            let moved = format!("{new_dir}{}", &dir[old_dir.len()..]);
            self.conn.execute(
                "INSERT INTO files (source_id, dir, name, size, mtime, kind, frn)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![source_id, moved, name, size, mtime, kind, frn],
            )?;
            self.log_change("add", self.conn.last_insert_rowid())?;
        }
        Ok(rows.len())
    }

    fn ids_where(&self, sql: &str, params: impl rusqlite::Params) -> Result<Vec<i64>> {
        let mut stmt = self.conn.prepare(sql)?;
        let rows = stmt.query_map(params, |r| r.get(0))?;
        Ok(rows.collect::<std::result::Result<_, _>>()?)
    }

    /// Grava (ou atualiza) um arquivo no catálogo. Devolve o id da linha nova.
    pub fn upsert_file(
        &self,
        source_id: i64,
        dir: &str,
        name: &str,
        size: i64,
        mtime: Option<i64>,
        kind: &str,
    ) -> Result<i64> {
        self.upsert_file_frn(source_id, dir, name, size, mtime, kind, None)
    }

    /// Como `upsert_file`, guardando o número do arquivo no NTFS. A linha antiga (mesmo lugar)
    /// sai e a nova entra; as duas mudanças ficam no registro para o índice em memória.
    #[allow(clippy::too_many_arguments)]
    pub fn upsert_file_frn(
        &self,
        source_id: i64,
        dir: &str,
        name: &str,
        size: i64,
        mtime: Option<i64>,
        kind: &str,
        frn: Option<i64>,
    ) -> Result<i64> {
        let old = self.ids_where(
            "SELECT id FROM files WHERE source_id = ?1 AND dir = ?2 AND name = ?3",
            params![source_id, dir, name],
        )?;
        self.delete_files(&old)?;
        self.conn.execute(
            "INSERT INTO files (source_id, dir, name, size, mtime, kind, frn)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![source_id, dir, name, size, mtime, kind, frn],
        )?;
        let id = self.conn.last_insert_rowid();
        self.log_change("add", id)?;
        Ok(id)
    }

    /// Quantos arquivos estão catalogados nessa fonte (para avisar antes de remover).
    pub fn source_file_count(&self, id: i64) -> Result<i64> {
        Ok(self.conn.query_row(
            "SELECT COUNT(*) FROM files WHERE source_id = ?1",
            params![id],
            |r| r.get(0),
        )?)
    }

    /// Remove a fonte e todos os arquivos catalogados dela.
    pub fn remove_source(&self, id: i64) -> Result<()> {
        self.conn
            .execute("DELETE FROM sources WHERE id = ?1", params![id])?;
        Ok(())
    }

    pub fn sources(&self) -> Result<Vec<Source>> {
        let mut stmt = self
            .conn
            .prepare("SELECT id, name, path, last_scan FROM sources ORDER BY name")?;
        let rows = stmt.query_map([], |r| {
            Ok(Source {
                id: r.get(0)?,
                name: r.get(1)?,
                path: r.get(2)?,
                last_scan: r.get(3)?,
            })
        })?;
        Ok(rows.collect::<std::result::Result<_, _>>()?)
    }

    /// Busca simples por parte do nome (atalho de `search_query`), ordenada por nome.
    pub fn search(&self, text: &str, kind: Option<&str>, limit: usize) -> Result<Vec<FileRow>> {
        self.search_query(&SearchQuery {
            text: text.to_string(),
            kind: kind.map(str::to_string),
            limit,
            ..SearchQuery::default()
        })
    }

    /// Busca com filtros e ordenação. Tudo roda dentro do SQLite.
    /// Texto, tipo, tamanho mínimo e fontes são filtros; texto vazio lista tudo que passa neles.
    pub fn search_query(&self, query: &SearchQuery) -> Result<Vec<FileRow>> {
        // Sem JOIN: o nome e o caminho da fonte vêm de uma tabelinha carregada uma vez só.
        let sources: std::collections::HashMap<i64, (String, String)> = self
            .sources()?
            .into_iter()
            .map(|source| (source.id, (source.name, source.path)))
            .collect();

        let (where_sql, mut values) = query.where_clause();
        values.push((query.limit as i64).into());
        values.push((query.offset as i64).into());
        let sql = format!(
            "SELECT f.name, f.dir, f.size, f.kind, f.source_id, f.mtime, f.id
             FROM files f {where_sql}
             ORDER BY {}
             LIMIT ? OFFSET ?",
            query.sort.order_by()
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt.query_map(rusqlite::params_from_iter(values), |r| {
            let name: String = r.get(0)?;
            let dir: String = r.get(1)?;
            let source_id: i64 = r.get(4)?;
            let (source_name, source_path) = sources.get(&source_id).cloned().unwrap_or_default();
            Ok(FileRow {
                id: r.get(6)?,
                source_id,
                path: format!("{}\\{}", dir.trim_end_matches('\\'), name),
                name,
                size: r.get(2)?,
                kind: r.get(3)?,
                source_name,
                source_path,
                mtime: r.get(5)?,
            })
        })?;
        Ok(rows.collect::<std::result::Result<_, _>>()?)
    }

    /// Linhas completas dos ids dados, na mesma ordem dos ids (o índice em memória só sabe ids).
    pub fn rows_by_ids(&self, ids: &[i64]) -> Result<Vec<FileRow>> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        let sources: std::collections::HashMap<i64, (String, String)> = self
            .sources()?
            .into_iter()
            .map(|source| (source.id, (source.name, source.path)))
            .collect();
        let list: Vec<String> = ids.iter().map(i64::to_string).collect();
        let sql = format!(
            "SELECT f.name, f.dir, f.size, f.kind, f.source_id, f.mtime, f.id FROM files f WHERE f.id IN ({})",
            list.join(",")
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let found = stmt.query_map([], |r| {
            let name: String = r.get(0)?;
            let dir: String = r.get(1)?;
            let source_id: i64 = r.get(4)?;
            let (source_name, source_path) = sources.get(&source_id).cloned().unwrap_or_default();
            Ok(FileRow {
                id: r.get(6)?,
                source_id,
                path: format!("{}\\{}", dir.trim_end_matches('\\'), name),
                name,
                size: r.get(2)?,
                kind: r.get(3)?,
                source_name,
                source_path,
                mtime: r.get(5)?,
            })
        })?;
        let mut by_id: std::collections::HashMap<i64, FileRow> = std::collections::HashMap::new();
        for row in found {
            let row = row?;
            by_id.insert(row.id, row);
        }
        Ok(ids.iter().filter_map(|id| by_id.remove(id)).collect())
    }

    /// Quantos arquivos passam nos filtros (ignora limite e página). Mais lenta que uma página:
    /// a tela pede em separado, sem travar a lista.
    pub fn count_query(&self, query: &SearchQuery) -> Result<i64> {
        let (where_sql, values) = query.where_clause();
        let sql = format!("SELECT COUNT(*) FROM files f {where_sql}");
        Ok(self
            .conn
            .query_row(&sql, rusqlite::params_from_iter(values), |r| r.get(0))?)
    }

    // ------------------------------------------------------------ Miniaturas

    /// Miniatura guardada (formato JPEG ou PNG pequeno) para a chave do caminho.
    pub fn get_thumb(&self, key: i64) -> Result<Option<Vec<u8>>> {
        Ok(self
            .conn
            .query_row("SELECT data FROM thumbs WHERE key = ?1", params![key], |r| r.get(0))
            .optional()?)
    }

    pub fn put_thumb(&self, key: i64, data: &[u8]) -> Result<()> {
        self.conn.execute(
            "INSERT OR REPLACE INTO thumbs (key, data) VALUES (?1, ?2)",
            params![key, data],
        )?;
        Ok(())
    }

    /// Já existe registro para a chave (miniatura pronta, ou marca de "sem miniatura").
    pub fn has_thumb(&self, key: i64) -> Result<bool> {
        Ok(self
            .conn
            .query_row("SELECT 1 FROM thumbs WHERE key = ?1", params![key], |_| Ok(()))
            .optional()?
            .is_some())
    }

    /// Arquivos (caminho, tipo) dessas fontes e tipos, para gerar miniaturas em lote.
    pub fn files_for_thumbs(&self, source_ids: &[i64], kinds: &[&str]) -> Result<Vec<(PathBuf, String)>> {
        if source_ids.is_empty() || kinds.is_empty() {
            return Ok(Vec::new());
        }
        let ids: Vec<String> = source_ids.iter().map(i64::to_string).collect();
        // `kinds` vem de constantes do programa, nunca de texto digitado.
        let kinds_sql: Vec<String> = kinds.iter().map(|kind| format!("'{}'", kind.replace('\'', ""))).collect();
        let sql = format!(
            "SELECT dir, name, kind FROM files WHERE source_id IN ({}) AND kind IN ({})",
            ids.join(","),
            kinds_sql.join(",")
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt.query_map([], |r| {
            let dir: String = r.get(0)?;
            let name: String = r.get(1)?;
            Ok((PathBuf::from(format!("{}\\{}", dir.trim_end_matches('\\'), name)), r.get(2)?))
        })?;
        Ok(rows.collect::<std::result::Result<_, _>>()?)
    }

    /// Por fonte: (id da fonte, quantidade de miniaturas, bytes que ocupam no banco).
    /// Miniaturas sem fonte (ex.: capas da Steam) ficam de fora; a diferença para
    /// `thumb_stats` é o espaço delas.
    pub fn thumb_usage_by_source(&self) -> Result<Vec<(i64, i64, i64)>> {
        let mut stmt = self.conn.prepare(
            "SELECT f.source_id, COUNT(*), COALESCE(SUM(LENGTH(t.data)), 0)
             FROM files f
             JOIN thumbs t ON t.key = acervo_key(RTRIM(f.dir, '\\') || '\\' || f.name)
             WHERE f.kind IN ('photo', 'video', 'document', 'program', 'installer')
               AND LENGTH(t.data) > 0
             GROUP BY f.source_id",
        )?;
        let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
        Ok(rows.collect::<std::result::Result<_, _>>()?)
    }

    /// Apaga miniaturas de foto, vídeo e documento que se repetem 6 vezes ou mais (arquivos
    /// diferentes com a mesma "miniatura"): quase sempre é o ícone genérico do Windows guardado
    /// por versões antigas no lugar da miniatura de verdade. Elas são geradas de novo, e as que o
    /// Windows realmente não consegue gerar ficam só com o quadrado colorido. Devolve quantas apagou.
    pub fn purge_generic_thumbs(&self) -> Result<usize> {
        Ok(self.conn.execute(
            "DELETE FROM thumbs WHERE key IN (
                 SELECT t.key FROM files f
                 JOIN thumbs t ON t.key = acervo_key(RTRIM(f.dir, '\\') || '\\' || f.name)
                 WHERE f.kind IN ('photo', 'video', 'document') AND LENGTH(t.data) > 0
                   AND t.data IN (SELECT data FROM thumbs WHERE LENGTH(data) > 0
                                  GROUP BY data HAVING COUNT(*) >= 6))",
            [],
        )?)
    }

    /// (quantidade, bytes) de miniaturas guardadas.
    pub fn thumb_stats(&self) -> Result<(i64, i64)> {
        Ok(self.conn.query_row(
            "SELECT COUNT(*), COALESCE(SUM(LENGTH(data)), 0) FROM thumbs",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?)
    }

    /// (tipo, quantidade de arquivos, bytes totais), do maior para o menor.
    pub fn stats_by_kind(&self) -> Result<Vec<(String, i64, i64)>> {
        let mut stmt = self.conn.prepare(
            "SELECT kind, COUNT(*), COALESCE(SUM(size), 0) FROM files GROUP BY kind ORDER BY 3 DESC",
        )?;
        let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
        Ok(rows.collect::<std::result::Result<_, _>>()?)
    }

    pub fn totals(&self) -> Result<Totals> {
        Ok(self.conn.query_row(
            "SELECT COUNT(*), COALESCE(SUM(size), 0) FROM files",
            [],
            |r| {
                Ok(Totals {
                    files: r.get(0)?,
                    bytes: r.get(1)?,
                })
            },
        )?)
    }

    pub fn duplicates(&self, limit: usize) -> Result<Vec<DuplicateGroup>> {
        self.duplicates_filtered("", None, MIN_DUPLICATE_SIZE, limit)
    }

    /// Duplicados com filtros: parte do nome, tipo e tamanho mínimo (nunca abaixo de 1 MB).
    pub fn duplicates_filtered(
        &self,
        text: &str,
        kind: Option<&str>,
        min_size: i64,
        limit: usize,
    ) -> Result<Vec<DuplicateGroup>> {
        let mut stmt = self.conn.prepare(
            "SELECT name, size, COUNT(*) AS copies, MIN(dir), kind
             FROM files
             WHERE size >= ?1 AND kind NOT IN ('repack', 'steam')
               AND name LIKE ?2 ESCAPE '\\'
               AND (?3 IS NULL OR kind = ?3)
             GROUP BY name, size
             HAVING copies > 1
             ORDER BY size * (copies - 1) DESC
             LIMIT ?4",
        )?;
        let rows = stmt.query_map(
            params![
                min_size.max(MIN_DUPLICATE_SIZE),
                like_pattern(text),
                kind,
                limit as i64
            ],
            |r| {
                let dir: String = r.get(3)?;
                let name: String = r.get(0)?;
                Ok(DuplicateGroup {
                    first_path: format!("{}\\{}", dir.trim_end_matches('\\'), name),
                    name,
                    size: r.get(1)?,
                    copies: r.get(2)?,
                    kind: r.get(4)?,
                })
            },
        )?;
        Ok(rows.collect::<std::result::Result<_, _>>()?)
    }

    pub fn duplicate_group_count(&self) -> Result<i64> {
        Ok(self.conn.query_row(
            "SELECT COUNT(*) FROM (
                 SELECT 1 FROM files WHERE size >= ?1 AND kind NOT IN ('repack', 'steam')
                 GROUP BY name, size HAVING COUNT(*) > 1)",
            params![MIN_DUPLICATE_SIZE],
            |r| r.get(0),
        )?)
    }

    /// Todas as cópias de um grupo de duplicados, da mais antiga para a mais nova.
    pub fn duplicate_copies(&self, name: &str, size: i64) -> Result<Vec<CopyRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT f.id, f.dir, s.name, f.mtime
             FROM files f JOIN sources s ON s.id = f.source_id
             WHERE f.name = ?1 AND f.size = ?2 AND f.kind NOT IN ('repack', 'steam')
             ORDER BY f.mtime, f.dir",
        )?;
        let rows = stmt.query_map(params![name, size], |r| {
            let dir: String = r.get(1)?;
            Ok(CopyRow {
                id: r.get(0)?,
                path: format!("{}\\{}", dir.trim_end_matches('\\'), name),
                source_name: r.get(2)?,
                mtime: r.get(3)?,
            })
        })?;
        Ok(rows.collect::<std::result::Result<_, _>>()?)
    }

    /// Tira arquivos do catálogo (não mexe no disco).
    pub fn delete_files(&self, ids: &[i64]) -> Result<()> {
        for id in ids {
            let removed = self
                .conn
                .execute("DELETE FROM files WHERE id = ?1", params![id])?;
            if removed > 0 {
                self.log_change("del", *id)?;
            }
        }
        Ok(())
    }

    pub fn repack_folders(&self) -> Result<Vec<RepackFolder>> {
        let mut stmt = self.conn.prepare(
            "SELECT f.dir, SUM(f.size), COUNT(*), s.name
             FROM files f JOIN sources s ON s.id = f.source_id
             WHERE f.kind = 'repack'
             GROUP BY f.source_id, f.dir
             ORDER BY f.dir COLLATE NOCASE",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok(RepackFolder {
                dir: r.get(0)?,
                bytes: r.get(1)?,
                files: r.get(2)?,
                source_name: r.get(3)?,
            })
        })?;
        Ok(rows.collect::<std::result::Result<_, _>>()?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn insert(db: &Db, source: i64, dir: &str, name: &str, size: i64, kind: &str) {
        db.conn
            .execute(
                "INSERT INTO files (source_id, dir, name, size, mtime, kind) VALUES (?1, ?2, ?3, ?4, 0, ?5)",
                params![source, dir, name, size, kind],
            )
            .unwrap();
    }

    #[test]
    fn reading_works_while_another_connection_is_writing_a_big_transaction() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("catalogo.db");
        let mut writer = Db::open(&path).unwrap();
        let source = writer.add_source("D:", "D:\\").unwrap();
        let tx = writer.conn.transaction().unwrap();
        tx.execute(
            "INSERT INTO files (source_id, dir, name, size, mtime, kind) VALUES (?1, 'D:\\x', 'a.mkv', 1, 0, 'video')",
            params![source],
        )
        .unwrap();

        // Com a varredura (transação) aberta, outra conexão abre, lê e busca sem esperar.
        let started = std::time::Instant::now();
        let reader = Db::open(&path).unwrap();
        assert_eq!(reader.sources().unwrap().len(), 1);
        assert!(reader.search("a.mkv", None, 10).unwrap().is_empty()); // ainda não confirmado
        assert!(started.elapsed() < Duration::from_secs(2));

        tx.commit().unwrap();
        assert_eq!(reader.search("a.mkv", None, 10).unwrap().len(), 1);
    }

    #[test]
    fn thumbnail_space_is_reported_per_source() {
        let db = Db::open_in_memory().unwrap();
        let a = db.add_source("D:", "D:\\").unwrap();
        let b = db.add_source("E:", "E:\\").unwrap();
        insert(&db, a, "D:\\fotos", "1.jpg", 10, "photo");
        insert(&db, a, "D:\\fotos", "2.jpg", 10, "photo");
        insert(&db, b, "E:\\", "3.mkv", 10, "video"); // pasta raiz com barra final
        insert(&db, b, "E:\\", "4.mkv", 10, "video"); // sem miniatura
        let key = crate::thumbs::key_for;
        db.put_thumb(key("D:\\fotos\\1.jpg"), &[0u8; 3000]).unwrap();
        db.put_thumb(key("D:\\fotos\\2.jpg"), &[0u8; 2000]).unwrap();
        db.put_thumb(key("E:\\3.mkv"), &[0u8; 4000]).unwrap();
        db.put_thumb(key("steam:123"), &[0u8; 9000]).unwrap(); // capa, sem fonte

        let mut usage = db.thumb_usage_by_source().unwrap();
        usage.sort();
        assert_eq!(usage, vec![(a, 2, 5000), (b, 1, 4000)]);
        assert_eq!(db.thumb_stats().unwrap(), (4, 18000));
    }

    #[test]
    fn purges_repeated_generic_thumbnails_but_keeps_unique_ones_and_program_icons() {
        let db = Db::open_in_memory().unwrap();
        let s = db.add_source("D:", "D:\\").unwrap();
        let key = crate::thumbs::key_for;
        // 7 vídeos com a mesma miniatura (ícone genérico), 1 vídeo com miniatura própria
        for n in 0..7 {
            insert(&db, s, "D:\\v", &format!("{n}.mp4"), 10, "video");
            db.put_thumb(key(&format!("D:\\v\\{n}.mp4")), &[9u8; 500]).unwrap();
        }
        insert(&db, s, "D:\\v", "unico.mp4", 10, "video");
        db.put_thumb(key("D:\\v\\unico.mp4"), &[1u8; 800]).unwrap();
        // 7 programas com o mesmo ícone: é legítimo (mesmo programa), não apaga
        for n in 0..7 {
            insert(&db, s, "D:\\p", &format!("{n}.exe"), 10, "program");
            db.put_thumb(key(&format!("D:\\p\\{n}.exe")), &[5u8; 300]).unwrap();
        }

        assert_eq!(db.purge_generic_thumbs().unwrap(), 7);
        assert!(db.has_thumb(key("D:\\v\\unico.mp4")).unwrap());
        assert!(!db.has_thumb(key("D:\\v\\0.mp4")).unwrap());
        assert!(db.has_thumb(key("D:\\p\\0.exe")).unwrap());
    }

    #[test]
    fn catalog_records_its_schema_version_and_refuses_newer_ones() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("catalogo.db");

        // catálogo novo já nasce na versão atual
        let db = Db::open(&path).unwrap();
        assert_eq!(db.schema_version().unwrap(), crate::version::SCHEMA_VERSION);

        // catálogo "antigo" (sem número de versão) é aceito e atualizado
        db.conn.execute_batch("PRAGMA user_version = 0").unwrap();
        drop(db);
        let db = Db::open(&path).unwrap();
        assert_eq!(db.schema_version().unwrap(), crate::version::SCHEMA_VERSION);

        // catálogo de um programa mais novo: não abre e explica por quê
        db.conn
            .execute_batch(&format!("PRAGMA user_version = {}", crate::version::SCHEMA_VERSION + 1))
            .unwrap();
        drop(db);
        let error = Db::open(&path).err().expect("devia recusar").to_string();
        assert!(error.contains("mais nova"), "{error}");
    }

    #[test]
    fn add_source_is_idempotent() {
        let db = Db::open_in_memory().unwrap();
        let a = db.add_source("D:", "D:\\").unwrap();
        let b = db.add_source("D: de novo", "D:\\").unwrap();
        assert_eq!(a, b);
        assert_eq!(db.sources().unwrap().len(), 1);
    }

    #[test]
    fn search_treats_percent_as_plain_text() {
        let db = Db::open_in_memory().unwrap();
        let id = db.add_source("D:", "D:\\").unwrap();
        insert(&db, id, "D:\\x", "100%.txt", 1, "document");
        insert(&db, id, "D:\\x", "outro.txt", 1, "document");
        let found = db.search("%", None, 10).unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].name, "100%.txt");
    }

    #[test]
    fn search_filters_by_kind() {
        let db = Db::open_in_memory().unwrap();
        let id = db.add_source("D:", "D:\\").unwrap();
        insert(&db, id, "D:\\x", "a.mkv", 1, "video");
        insert(&db, id, "D:\\x", "a.mp3", 1, "music");
        let videos = db.search("", Some("video"), 10).unwrap();
        assert_eq!(videos.len(), 1);
        assert_eq!(videos[0].name, "a.mkv");
    }

    #[test]
    fn moving_a_folder_rewrites_paths_inside_it_only() {
        let db = Db::open_in_memory().unwrap();
        let id = db.add_source("D:", "D:\\").unwrap();
        insert(&db, id, "D:\\Jogos", "a.iso", 1, "other");
        insert(&db, id, "D:\\Jogos\\Sub", "b.iso", 1, "other");
        insert(&db, id, "D:\\JogosVelhos", "c.iso", 1, "other");
        let moved = db.move_dir(id, "D:\\Jogos", "D:\\Games").unwrap();
        assert_eq!(moved, 2);
        let paths: Vec<String> = db.search("iso", None, 10).unwrap().into_iter().map(|f| f.path).collect();
        assert!(paths.contains(&"D:\\Games\\a.iso".to_string()), "{paths:?}");
        assert!(paths.contains(&"D:\\Games\\Sub\\b.iso".to_string()), "{paths:?}");
        assert!(paths.contains(&"D:\\JogosVelhos\\c.iso".to_string()), "{paths:?}");
        assert_eq!(paths.len(), 3);
    }

    #[test]
    fn removing_a_source_removes_its_files() {
        let db = Db::open_in_memory().unwrap();
        let id = db.add_source("D:", "D:\\").unwrap();
        insert(&db, id, "D:\\x", "a.mkv", 1, "video");
        db.remove_source(id).unwrap();
        assert!(db.search("", None, 10).unwrap().is_empty());
    }

    #[test]
    fn finds_duplicates_only_above_minimum_size() {
        let db = Db::open_in_memory().unwrap();
        let id = db.add_source("D:", "D:\\").unwrap();
        let big = MIN_DUPLICATE_SIZE * 2;
        insert(&db, id, "D:\\a", "filme.mkv", big, "video");
        insert(&db, id, "E:\\b", "filme.mkv", big, "video");
        insert(&db, id, "D:\\a", "pequeno.txt", 10, "document");
        insert(&db, id, "E:\\b", "pequeno.txt", 10, "document");
        let groups = db.duplicates(10).unwrap();
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].copies, 2);
        assert_eq!(db.duplicate_group_count().unwrap(), 1);
    }

    #[test]
    fn settings_roundtrip() {
        let db = Db::open_in_memory().unwrap();
        assert_eq!(db.get_setting("x").unwrap(), None);
        db.set_setting("x", "1").unwrap();
        db.set_setting("x", "2").unwrap();
        assert_eq!(db.get_setting("x").unwrap().as_deref(), Some("2"));
    }
}
