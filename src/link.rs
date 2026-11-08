//! Ligação entre o app e o serviço do Windows: tarefas e sinal de vida.
//!
//! Fica num banco PRÓPRIO e pequeno (`service.db`, ao lado do catálogo), não no catálogo. Uma
//! varredura grava milhares de linhas no catálogo e segura o banco; se o sinal de vida e as
//! tarefas ficassem lá, o serviço pareceria "morto" durante qualquer varredura longa e o app
//! esperaria para sempre. Aqui as gravações são minúsculas e nunca esperam por varredura.

use anyhow::Result;
use rusqlite::{params, Connection, OptionalExtension};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// O serviço é considerado ativo se deu sinal de vida nos últimos segundos.
const ALIVE_WINDOW_SECS: i64 = 10;

pub fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Tarefa que o app pede ao serviço.
#[derive(Debug, Clone)]
pub struct Job {
    pub id: i64,
    pub kind: String,
    pub source_id: i64,
    /// queued, running, done, failed, cancel (pedido do app) ou cancelled.
    pub state: String,
    pub progress: String,
    pub created: i64,
}

pub struct ServiceLink {
    conn: Connection,
}

impl ServiceLink {
    /// `service.db` fica na mesma pasta do catálogo.
    pub fn path_for(catalog_path: &Path) -> PathBuf {
        catalog_path.with_file_name("service.db")
    }

    pub fn open(catalog_path: &Path) -> Result<ServiceLink> {
        // Duas aberturas ao mesmo tempo (app e serviço) podem esbarrar na criação do banco e na
        // troca para WAL, que não esperam pelo tempo de espera normal: tenta de novo.
        let mut attempt = 0;
        loop {
            match Self::open_once(catalog_path) {
                Err(error) if attempt < 20 && error.to_string().contains("locked") => {
                    attempt += 1;
                    std::thread::sleep(Duration::from_millis(50));
                }
                result => return result,
            }
        }
    }

    fn open_once(catalog_path: &Path) -> Result<ServiceLink> {
        let path = Self::path_for(catalog_path);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let conn = Connection::open(path)?;
        conn.busy_timeout(Duration::from_secs(10))?;
        conn.execute_batch(
            "PRAGMA journal_mode = WAL;
             PRAGMA synchronous = NORMAL;
             CREATE TABLE IF NOT EXISTS service_jobs (
                 id        INTEGER PRIMARY KEY AUTOINCREMENT,
                 kind      TEXT NOT NULL,
                 source_id INTEGER NOT NULL,
                 state     TEXT NOT NULL,
                 progress  TEXT NOT NULL DEFAULT '',
                 created   INTEGER NOT NULL
             );
             CREATE TABLE IF NOT EXISTS state (
                 key   TEXT PRIMARY KEY,
                 value TEXT NOT NULL
             );",
        )?;
        Ok(ServiceLink { conn })
    }

    fn set_state(&self, key: &str, value: &str) -> Result<()> {
        self.conn.execute(
            "INSERT INTO state (key, value) VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![key, value],
        )?;
        Ok(())
    }

    fn get_state(&self, key: &str) -> Result<Option<String>> {
        Ok(self
            .conn
            .query_row("SELECT value FROM state WHERE key = ?1", params![key], |r| r.get(0))
            .optional()?)
    }

    /// O serviço chama isto a cada segundo, de uma thread só para isso.
    pub fn beat(&self) -> Result<()> {
        self.set_state("heartbeat", &now_secs().to_string())
    }

    /// Ao parar normalmente, o serviço zera o sinal.
    pub fn clear_beat(&self) -> Result<()> {
        self.set_state("heartbeat", "0")
    }

    pub fn seconds_since_beat(&self) -> Option<i64> {
        let beat: i64 = self.get_state("heartbeat").ok().flatten()?.parse().ok()?;
        (beat > 0).then(|| now_secs() - beat)
    }

    pub fn is_alive(&self) -> bool {
        self.seconds_since_beat().is_some_and(|seconds| seconds <= ALIVE_WINDOW_SECS)
    }

    pub fn enqueue_job(&self, kind: &str, source_id: i64) -> Result<i64> {
        self.conn.execute(
            "INSERT INTO service_jobs (kind, source_id, state, created) VALUES (?1, ?2, 'queued', ?3)",
            params![kind, source_id, now_secs()],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    fn job_from_row(r: &rusqlite::Row) -> rusqlite::Result<Job> {
        Ok(Job {
            id: r.get(0)?,
            kind: r.get(1)?,
            source_id: r.get(2)?,
            state: r.get(3)?,
            progress: r.get(4)?,
            created: r.get(5)?,
        })
    }

    pub fn next_queued_job(&self) -> Result<Option<Job>> {
        Ok(self
            .conn
            .query_row(
                "SELECT id, kind, source_id, state, progress, created FROM service_jobs
                 WHERE state = 'queued' ORDER BY id LIMIT 1",
                [],
                Self::job_from_row,
            )
            .optional()?)
    }

    pub fn job(&self, id: i64) -> Result<Option<Job>> {
        Ok(self
            .conn
            .query_row(
                "SELECT id, kind, source_id, state, progress, created FROM service_jobs WHERE id = ?1",
                params![id],
                Self::job_from_row,
            )
            .optional()?)
    }

    pub fn set_job(&self, id: i64, state: &str, progress: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE service_jobs SET state = ?2, progress = ?3 WHERE id = ?1",
            params![id, state, progress],
        )?;
        Ok(())
    }

    /// Só muda o texto de andamento; se o app pediu para cancelar, o estado não é sobrescrito.
    pub fn set_job_progress(&self, id: i64, progress: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE service_jobs SET progress = ?2 WHERE id = ?1",
            params![id, progress],
        )?;
        Ok(())
    }

    /// Ao iniciar, o serviço marca como falhas as tarefas que ficaram "rodando" de uma parada brusca.
    pub fn fail_interrupted_jobs(&self) -> Result<usize> {
        Ok(self.conn.execute(
            "UPDATE service_jobs SET state = 'failed', progress = 'Interrompida: o serviço foi reiniciado.'
             WHERE state IN ('running', 'cancel')",
            [],
        )?)
    }

    /// Últimas tarefas, da mais nova para a mais antiga (diagnóstico).
    pub fn recent_jobs(&self, limit: i64) -> Result<Vec<Job>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, kind, source_id, state, progress, created FROM service_jobs ORDER BY id DESC LIMIT ?1",
        )?;
        let jobs = stmt.query_map(params![limit], Self::job_from_row)?.collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(jobs)
    }

    /// Apaga tarefas terminadas com mais de um dia.
    pub fn clean_jobs(&self) -> Result<()> {
        self.conn.execute(
            "DELETE FROM service_jobs WHERE state IN ('done', 'failed', 'cancelled') AND created < ?1",
            params![now_secs() - 86_400],
        )?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn heartbeat_means_alive_only_when_fresh() {
        let dir = tempfile::tempdir().unwrap();
        let link = ServiceLink::open(&dir.path().join("acervo.db")).unwrap();
        assert!(!link.is_alive());
        link.beat().unwrap();
        assert!(link.is_alive());
        link.set_state("heartbeat", &(now_secs() - 60).to_string()).unwrap();
        assert!(!link.is_alive());
        link.beat().unwrap();
        link.clear_beat().unwrap();
        assert!(!link.is_alive());
        assert_eq!(link.seconds_since_beat(), None);
    }

    #[test]
    fn jobs_go_in_order_and_keep_their_progress() {
        let dir = tempfile::tempdir().unwrap();
        let link = ServiceLink::open(&dir.path().join("acervo.db")).unwrap();
        let first = link.enqueue_job("scan", 1).unwrap();
        let second = link.enqueue_job("scan", 2).unwrap();
        assert_eq!(link.next_queued_job().unwrap().unwrap().id, first);
        link.set_job(first, "running", "começou").unwrap();
        link.set_job_progress(first, "10 arquivos").unwrap();
        let job = link.job(first).unwrap().unwrap();
        assert_eq!((job.state.as_str(), job.progress.as_str()), ("running", "10 arquivos"));
        assert_eq!(link.next_queued_job().unwrap().unwrap().id, second);
    }

    #[test]
    fn interrupted_jobs_are_failed_on_restart_but_queued_ones_stay() {
        let dir = tempfile::tempdir().unwrap();
        let link = ServiceLink::open(&dir.path().join("acervo.db")).unwrap();
        let running = link.enqueue_job("scan", 1).unwrap();
        let queued = link.enqueue_job("scan", 2).unwrap();
        link.set_job(running, "running", "...").unwrap();
        assert_eq!(link.fail_interrupted_jobs().unwrap(), 1);
        assert_eq!(link.job(running).unwrap().unwrap().state, "failed");
        assert_eq!(link.job(queued).unwrap().unwrap().state, "queued");
    }

    #[test]
    fn two_connections_can_write_at_the_same_time_without_locking() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("acervo.db");
        let service = ServiceLink::open(&path).unwrap();
        let app = ServiceLink::open(&path).unwrap();
        // o app enfileira enquanto o serviço dá sinal de vida e atualiza uma tarefa
        let id = app.enqueue_job("scan", 1).unwrap();
        service.beat().unwrap();
        service.set_job(id, "running", "x").unwrap();
        assert!(app.is_alive());
        assert_eq!(app.job(id).unwrap().unwrap().state, "running");
    }
}
