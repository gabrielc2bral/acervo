//! Índice de nomes em memória, para busca instantânea.
//!
//! Todos os nomes de arquivo ficam em um único bloco de memória (minúsculos, separados por `\0`).
//! Buscar é procurar o texto nesse bloco com SIMD (`memchr`): milhões de nomes em poucos
//! milissegundos, sem passar pelo SQLite. As ordenações usam vetores prontos (posição do nome na
//! ordem alfabética, tamanho, data).
//!
//! - **Incremental:** arquivo criado ou apagado entra/sai do índice na hora (`apply`), sem
//!   reconstruir. Os apagados viram "lápides" até acumularem o bastante para valer a pena refazer.
//! - **Salvo em disco:** `save`/`load` guardam o índice pronto; ao abrir o app ele carrega em uma
//!   fração de segundo em vez de ser montado de novo (vale enquanto o catálogo for o mesmo).
//! O SQLite continua sendo a fonte da verdade.

use crate::db::{Db, SearchQuery, Sort};
use anyhow::{bail, Result};
use memchr::memmem;
use std::collections::HashMap;
use std::io::{Read, Write};
use std::path::Path;

const KINDS: [&str; 10] = [
    "video", "photo", "music", "document", "program", "installer", "archive", "repack", "steam", "other",
];
const STEAM: u8 = 8;
const OTHER: u8 = 9;
/// Marca de entrada apagada (lápide).
const DELETED: u8 = 255;
const MAGIC: &[u8; 8] = b"ACVIDX03";
/// Formato do arquivo de índice salvo (o número do `MAGIC`). Se mudar, o índice antigo é ignorado
/// e refeito.
pub const FORMAT: u32 = 3;

fn kind_code(kind: &str) -> u8 {
    KINDS.iter().position(|k| *k == kind).unwrap_or(OTHER as usize) as u8
}

#[derive(Clone)]
struct Entry {
    id: i64,
    size: i64,
    mtime: i64,
    source: i64,
    kind: u8,
}

/// Arquivo novo para o índice (os mesmos campos que o SQLite guarda).
pub struct NewEntry {
    pub id: i64,
    pub name: String,
    pub size: i64,
    pub mtime: Option<i64>,
    pub source: i64,
    pub kind: String,
}

pub struct Index {
    /// Nomes em minúsculas, cada um terminado em `\0`.
    names: Vec<u8>,
    /// Onde cada nome começa em `names`.
    starts: Vec<u32>,
    entries: Vec<Entry>,
    /// Posições dos arquivos na ordem alfabética dos nomes.
    sorted: Vec<u32>,
    /// Inverso de `sorted`: a posição alfabética de cada arquivo.
    name_rank: Vec<u32>,
    /// id do SQLite -> posição, montado na primeira mudança incremental.
    id_to_pos: Option<HashMap<i64, u32>>,
    tombstones: usize,
}

pub struct Found {
    /// Ids (da tabela `files`) da página pedida, já na ordem escolhida.
    pub ids: Vec<i64>,
    /// Quantos arquivos passam nos filtros, contando todas as páginas.
    pub total: usize,
}

impl Index {
    /// Lê o catálogo inteiro (centenas de milhares de linhas em poucos segundos).
    pub fn build(db: &Db) -> Result<Index> {
        let mut stmt = db
            .conn
            .prepare("SELECT id, name, size, mtime, kind, source_id FROM files")?;
        let mut names: Vec<u8> = Vec::new();
        let mut starts: Vec<u32> = Vec::new();
        let mut entries: Vec<Entry> = Vec::new();
        let mut rows = stmt.query([])?;
        while let Some(row) = rows.next()? {
            let name: String = row.get(1)?;
            let kind: String = row.get(4)?;
            starts.push(names.len() as u32);
            names.extend_from_slice(name.to_lowercase().as_bytes());
            names.push(0);
            entries.push(Entry {
                id: row.get(0)?,
                size: row.get(2)?,
                mtime: row.get::<_, Option<i64>>(3)?.unwrap_or(0),
                source: row.get(5)?,
                kind: kind_code(&kind),
            });
        }

        let mut index = Index {
            names,
            starts,
            entries,
            sorted: Vec::new(),
            name_rank: Vec::new(),
            id_to_pos: None,
            tombstones: 0,
        };
        let mut order: Vec<u32> = (0..index.entries.len() as u32).collect();
        order.sort_unstable_by(|a, b| index.name(*a).cmp(index.name(*b)).then(a.cmp(b)));
        index.sorted = order;
        index.recompute_rank();
        Ok(index)
    }

    /// Arquivos que ainda existem (sem as lápides).
    pub fn len(&self) -> usize {
        self.entries.len() - self.tombstones
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    fn recompute_rank(&mut self) {
        self.name_rank = vec![0; self.entries.len()];
        for (rank, position) in self.sorted.iter().enumerate() {
            self.name_rank[*position as usize] = rank as u32;
        }
    }

    /// Nome (minúsculo) do arquivo `i`, sem o `\0` final.
    fn name(&self, i: u32) -> &[u8] {
        let start = self.starts[i as usize] as usize;
        let end = self.starts.get(i as usize + 1).map_or(self.names.len(), |next| *next as usize) - 1;
        &self.names[start..end]
    }

    /// Aplica mudanças sem reconstruir: `removed` são ids que saíram do catálogo e `added` os que
    /// entraram (um id já existente é substituído). Devolve `true` quando há tantas lápides que
    /// vale reconstruir o índice do zero.
    pub fn apply(&mut self, removed: &[i64], added: Vec<NewEntry>) -> bool {
        if self.id_to_pos.is_none() {
            let map = self.entries.iter().enumerate().map(|(position, entry)| (entry.id, position as u32));
            self.id_to_pos = Some(map.filter(|(id, _)| *id != i64::MIN).collect());
        }
        let mut map = self.id_to_pos.take().unwrap_or_default();

        for id in removed {
            if let Some(position) = map.remove(id) {
                self.entries[position as usize].kind = DELETED;
                self.tombstones += 1;
            }
        }

        let mut new_positions: Vec<u32> = Vec::new();
        for entry in added {
            if let Some(old) = map.remove(&entry.id) {
                self.entries[old as usize].kind = DELETED;
                self.tombstones += 1;
            }
            let position = self.entries.len() as u32;
            self.starts.push(self.names.len() as u32);
            self.names.extend_from_slice(entry.name.to_lowercase().as_bytes());
            self.names.push(0);
            self.entries.push(Entry {
                id: entry.id,
                size: entry.size,
                mtime: entry.mtime.unwrap_or(0),
                source: entry.source,
                kind: kind_code(&entry.kind),
            });
            map.insert(entry.id, position);
            new_positions.push(position);
        }
        self.id_to_pos = Some(map);

        if !new_positions.is_empty() {
            // Intercala os novos (ordenados) na ordem alfabética existente.
            new_positions.sort_unstable_by(|a, b| self.name(*a).cmp(self.name(*b)).then(a.cmp(b)));
            let mut merged: Vec<u32> = Vec::with_capacity(self.sorted.len() + new_positions.len());
            let mut next_new = new_positions.iter().peekable();
            for old in &self.sorted {
                while let Some(candidate) = next_new.peek() {
                    if self.name(**candidate) < self.name(*old) {
                        merged.push(**candidate);
                        next_new.next();
                    } else {
                        break;
                    }
                }
                merged.push(*old);
            }
            merged.extend(next_new.copied());
            self.sorted = merged;
            self.recompute_rank();
        }

        self.tombstones > 1000 && self.tombstones * 10 > self.entries.len()
    }

    /// Quem tem o texto no nome. Várias palavras separadas por espaço = todas precisam aparecer
    /// (busca por todas as palavras).
    fn candidates(&self, terms: &[Vec<u8>]) -> Vec<u32> {
        let Some(first) = terms.first() else {
            return (0..self.entries.len() as u32).collect();
        };
        let finder = memmem::Finder::new(first);
        let mut found: Vec<u32> = Vec::new();
        for offset in finder.find_iter(&self.names) {
            let index = self.starts.partition_point(|start| (*start as usize) <= offset) as u32 - 1;
            if found.last() != Some(&index) {
                found.push(index);
            }
        }
        // Palavras seguintes: confere só em quem já passou na primeira.
        for term in &terms[1..] {
            let finder = memmem::Finder::new(term);
            found.retain(|i| finder.find(self.name(*i)).is_some());
        }
        found
    }

    pub fn search(&self, query: &SearchQuery) -> Found {
        let lower = query.text.to_lowercase();
        let terms: Vec<Vec<u8>> = lower.split_whitespace().map(|term| term.as_bytes().to_vec()).collect();
        let wanted_kind = query.kind.as_deref().map(kind_code);

        let mut matches = self.candidates(&terms);
        matches.retain(|i| {
            let entry = &self.entries[*i as usize];
            if entry.kind == DELETED {
                return false;
            }
            let kind_ok = match wanted_kind {
                Some(code) => entry.kind == code,
                None => entry.kind != STEAM,
            };
            kind_ok
                && entry.size >= query.min_size
                && (query.source_ids.is_empty() || query.source_ids.contains(&entry.source))
        });
        let total = matches.len();

        // Só a página pedida precisa ficar em ordem: separa as primeiras `want` e ordena só elas.
        let want = (query.offset + query.limit).min(total);
        let key = |i: &u32| -> (i64, u32) {
            let entry = &self.entries[*i as usize];
            match query.sort {
                Sort::Name => (self.name_rank[*i as usize] as i64, *i),
                Sort::Largest => (-entry.size, *i),
                Sort::Smallest => (entry.size, *i),
                Sort::Newest => (-entry.mtime, *i),
                Sort::Oldest => (entry.mtime, *i),
            }
        };
        if want > 0 && want < total {
            matches.select_nth_unstable_by_key(want - 1, key);
            matches.truncate(want);
        }
        matches.sort_unstable_by_key(key);

        let ids = matches
            .iter()
            .skip(query.offset)
            .take(query.limit)
            .map(|i| self.entries[*i as usize].id)
            .collect();
        Found { ids, total }
    }

    // ------------------------------------------------------------------ Salvar e carregar

    /// Guarda o índice pronto em `path`. `stamp` identifica o catálogo de onde ele veio
    /// (`Db::catalog_stamp`). As lápides não vão para o arquivo.
    pub fn save(&self, path: &Path, stamp: (i64, i64, i64)) -> Result<()> {
        let live: Vec<u32> = (0..self.entries.len() as u32)
            .filter(|i| self.entries[*i as usize].kind != DELETED)
            .collect();
        let mut remap = vec![u32::MAX; self.entries.len()];
        for (new, old) in live.iter().enumerate() {
            remap[*old as usize] = new as u32;
        }

        let temporary = path.with_extension("tmp");
        let mut out = std::io::BufWriter::new(std::fs::File::create(&temporary)?);
        out.write_all(MAGIC)?;
        for number in [stamp.0, stamp.1, stamp.2] {
            out.write_all(&number.to_le_bytes())?;
        }
        out.write_all(&(live.len() as u64).to_le_bytes())?;

        let names_len: usize = live.iter().map(|i| self.name(*i).len() + 1).sum();
        out.write_all(&(names_len as u64).to_le_bytes())?;
        for old in &live {
            out.write_all(self.name(*old))?;
            out.write_all(&[0])?;
        }
        for old in &live {
            let entry = &self.entries[*old as usize];
            out.write_all(&entry.id.to_le_bytes())?;
            out.write_all(&entry.size.to_le_bytes())?;
            out.write_all(&entry.mtime.to_le_bytes())?;
            out.write_all(&entry.source.to_le_bytes())?;
            out.write_all(&[entry.kind])?;
        }
        for position in self.sorted.iter().filter(|p| remap[**p as usize] != u32::MAX) {
            out.write_all(&remap[*position as usize].to_le_bytes())?;
        }
        out.flush()?;
        drop(out);
        std::fs::rename(&temporary, path)?;
        Ok(())
    }

    /// Carrega um índice salvo, só se ele for do mesmo catálogo (`stamp` igual).
    pub fn load(path: &Path, stamp: (i64, i64, i64)) -> Option<Index> {
        let bytes = std::fs::read(path).ok()?;
        Self::decode(&bytes, stamp).ok()
    }

    fn decode(bytes: &[u8], stamp: (i64, i64, i64)) -> Result<Index> {
        let mut cursor = std::io::Cursor::new(bytes);
        let mut magic = [0u8; 8];
        cursor.read_exact(&mut magic)?;
        if &magic != MAGIC {
            bail!("formato do índice desconhecido");
        }
        let read_i64 = |cursor: &mut std::io::Cursor<&[u8]>| -> Result<i64> {
            let mut buffer = [0u8; 8];
            cursor.read_exact(&mut buffer)?;
            Ok(i64::from_le_bytes(buffer))
        };
        let saved = (read_i64(&mut cursor)?, read_i64(&mut cursor)?, read_i64(&mut cursor)?);
        if saved != stamp {
            bail!("o catálogo mudou desde que o índice foi salvo");
        }
        let count = read_i64(&mut cursor)? as usize;
        let names_len = read_i64(&mut cursor)? as usize;

        let mut names = vec![0u8; names_len];
        cursor.read_exact(&mut names)?;
        let mut starts: Vec<u32> = Vec::with_capacity(count);
        let mut offset = 0u32;
        for chunk in names.split_inclusive(|byte| *byte == 0) {
            starts.push(offset);
            offset += chunk.len() as u32;
        }
        if starts.len() != count {
            bail!("índice corrompido");
        }

        let mut entries = Vec::with_capacity(count);
        for _ in 0..count {
            let id = read_i64(&mut cursor)?;
            let size = read_i64(&mut cursor)?;
            let mtime = read_i64(&mut cursor)?;
            let source = read_i64(&mut cursor)?;
            let mut kind = [0u8; 1];
            cursor.read_exact(&mut kind)?;
            entries.push(Entry { id, size, mtime, source, kind: kind[0] });
        }
        let mut sorted = Vec::with_capacity(count);
        let mut buffer = [0u8; 4];
        for _ in 0..count {
            cursor.read_exact(&mut buffer)?;
            let position = u32::from_le_bytes(buffer);
            if position as usize >= count {
                bail!("índice corrompido");
            }
            sorted.push(position);
        }

        let mut index = Index {
            names,
            starts,
            entries,
            sorted,
            name_rank: Vec::new(),
            id_to_pos: None,
            tombstones: 0,
        };
        index.recompute_rank();
        Ok(index)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn db_with(files: &[(&str, i64, i64, &str)]) -> Db {
        let db = Db::open_in_memory().unwrap();
        let source = db.add_source("D:", "D:\\").unwrap();
        for (name, size, mtime, kind) in files {
            db.upsert_file(source, "D:\\x", name, *size, Some(*mtime), kind).unwrap();
        }
        db
    }

    fn query(text: &str) -> SearchQuery {
        SearchQuery { text: text.to_string(), limit: 100, ..SearchQuery::default() }
    }

    fn names(db: &Db, index: &Index, q: &SearchQuery) -> Vec<String> {
        index
            .search(q)
            .ids
            .iter()
            .map(|id| {
                db.conn
                    .query_row("SELECT name FROM files WHERE id = ?1", [id], |r| r.get(0))
                    .unwrap()
            })
            .collect()
    }

    #[test]
    fn finds_substrings_ignoring_case_and_matches_each_name_once() {
        let db = db_with(&[("Dexter S01E01.mkv", 5, 1, "video"), ("dexter dexter.txt", 1, 2, "document"), ("Outro.mp3", 1, 3, "music")]);
        let index = Index::build(&db).unwrap();
        assert_eq!(index.search(&query("DEXTER")).total, 2);
        assert_eq!(index.search(&query("")).total, 3);
        assert_eq!(index.search(&query("nada")).total, 0);
    }

    #[test]
    fn several_words_must_all_appear_and_never_span_two_names() {
        let db = db_with(&[("filme de acao.mkv", 1, 1, "video"), ("acao", 1, 1, "video"), ("filme", 1, 1, "video")]);
        let index = Index::build(&db).unwrap();
        assert_eq!(index.search(&query("filme acao")).total, 1);
        assert_eq!(index.search(&query("filmeacao")).total, 0);
    }

    #[test]
    fn filters_by_kind_size_and_hides_steam_unless_asked() {
        let db = db_with(&[("a.mkv", 100, 1, "video"), ("b.mkv", 10, 1, "video"), ("c.dll", 5, 1, "steam")]);
        let index = Index::build(&db).unwrap();
        assert_eq!(index.search(&query("")).total, 2);
        assert_eq!(index.search(&SearchQuery { kind: Some("steam".into()), limit: 10, ..SearchQuery::default() }).total, 1);
        assert_eq!(index.search(&SearchQuery { min_size: 50, limit: 10, ..SearchQuery::default() }).total, 1);
    }

    #[test]
    fn sorts_and_paginates() {
        let db = db_with(&[("b.mkv", 20, 5, "video"), ("a.mkv", 30, 9, "video"), ("c.mkv", 10, 1, "video"), ("D.mkv", 40, 7, "video")]);
        let index = Index::build(&db).unwrap();
        let sorted = |sort: Sort, offset: usize, limit: usize| {
            names(&db, &index, &SearchQuery { sort, offset, limit, ..SearchQuery::default() })
        };
        assert_eq!(sorted(Sort::Name, 0, 10), ["a.mkv", "b.mkv", "c.mkv", "D.mkv"]);
        assert_eq!(sorted(Sort::Largest, 0, 2), ["D.mkv", "a.mkv"]);
        assert_eq!(sorted(Sort::Largest, 2, 2), ["b.mkv", "c.mkv"]);
        assert_eq!(sorted(Sort::Newest, 0, 1), ["a.mkv"]);
        assert_eq!(sorted(Sort::Oldest, 0, 1), ["c.mkv"]);
        assert_eq!(index.search(&SearchQuery { limit: 2, ..SearchQuery::default() }).total, 4);
    }

    #[test]
    fn same_results_as_the_sql_search() {
        let db = db_with(&[("Dexter 1x00.mkv", 5, 1, "video"), ("dexter.txt", 1, 2, "document"), ("zeta.mp3", 1, 3, "music"), ("Alpha Dexter.mkv", 9, 4, "video")]);
        let index = Index::build(&db).unwrap();
        let q = SearchQuery { text: "dexter".into(), kind: Some("video".into()), limit: 10, ..SearchQuery::default() };
        let from_index = names(&db, &index, &q);
        let from_sql: Vec<String> = db.search_query(&q).unwrap().into_iter().map(|row| row.name).collect();
        assert_eq!(from_index, from_sql);
    }

    #[test]
    fn incremental_changes_match_a_full_rebuild() {
        let db = db_with(&[("b.mkv", 20, 5, "video"), ("a.mkv", 30, 9, "video"), ("c.mkv", 10, 1, "video")]);
        let mut index = Index::build(&db).unwrap();
        let source = db.sources().unwrap()[0].id;

        // apaga "a.mkv" e cria "aa.mkv" e "0.mkv" pelo caminho normal do banco (que registra as mudanças)
        let before = db.last_change_seq().unwrap();
        db.remove_path(source, "D:\\x\\a.mkv").unwrap();
        db.upsert_file(source, "D:\\x", "aa.mkv", 99, Some(50), "video").unwrap();
        db.upsert_file(source, "D:\\x", "0.mkv", 1, Some(2), "video").unwrap();

        let mut removed = Vec::new();
        let mut added_ids = Vec::new();
        for (_, op, id) in db.changes_since(before, 100).unwrap() {
            if op == "del" { removed.push(id) } else { added_ids.push(id) }
        }
        let added: Vec<NewEntry> = db
            .rows_by_ids(&added_ids)
            .unwrap()
            .into_iter()
            .map(|row| NewEntry { id: row.id, name: row.name, size: row.size, mtime: row.mtime, source: row.source_id, kind: row.kind })
            .collect();
        assert!(!index.apply(&removed, added));

        let fresh = Index::build(&db).unwrap();
        for sort in [Sort::Name, Sort::Largest, Sort::Newest] {
            let q = SearchQuery { sort, limit: 10, ..SearchQuery::default() };
            assert_eq!(names(&db, &index, &q), names(&db, &fresh, &q), "ordem {sort:?}");
        }
        assert_eq!(index.search(&query("")).total, 4);
        assert_eq!(index.search(&query("a.mkv")).total, 1); // só "aa.mkv"
        assert_eq!(index.len(), 4);
    }

    #[test]
    fn saved_index_loads_only_for_the_same_catalog() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("index.bin");
        let db = db_with(&[("b.mkv", 20, 5, "video"), ("a.mkv", 30, 9, "video"), ("c.mkv", 10, 1, "video")]);
        let mut index = Index::build(&db).unwrap();
        let source = db.sources().unwrap()[0].id;
        // uma lápide: o arquivo salvo não deve carregá-la
        let id = db.search("b.mkv", None, 1).unwrap()[0].id;
        db.remove_path(source, "D:\\x\\b.mkv").unwrap();
        index.apply(&[id], Vec::new());

        let stamp = db.catalog_stamp().unwrap();
        index.save(&path, stamp).unwrap();

        let loaded = Index::load(&path, stamp).expect("devia carregar");
        assert_eq!(loaded.len(), 2);
        assert_eq!(names(&db, &loaded, &query("")), names(&db, &index, &query("")));
        assert_eq!(names(&db, &loaded, &SearchQuery { sort: Sort::Largest, limit: 5, ..SearchQuery::default() }), ["a.mkv", "c.mkv"]);

        // catálogo diferente: não carrega
        assert!(Index::load(&path, (stamp.0 + 1, stamp.1, stamp.2)).is_none());
        assert!(Index::load(&dir.path().join("nao-existe.bin"), stamp).is_none());
    }
}
