//! Safe diagnostics and maintenance operations (T-056).

use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use rusqlite::{params, OptionalExtension};

use crate::error::{CoreError, ErrorCode};
use crate::store::{sql_err, Core};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DiagnosticsSnapshot {
    pub accounts: u64,
    pub messages: u64,
    pub indexed_messages: u64,
    pub pending_index: u64,
    pub pending_operations: u64,
    pub failed_operations: u64,
    pub cached_bodies: u64,
    pub cache_bytes: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct McpAuditEntry {
    pub client_id: String,
    pub tool: String,
    pub outcome: String,
    pub created_at: i64,
}

impl Core {
    pub fn list_mcp_audit(&self, limit: usize) -> Result<Vec<McpAuditEntry>, CoreError> {
        let mut stmt = self
            .db
            .conn()
            .prepare(
                "SELECT COALESCE(client_id, 'unknown'), tool, outcome, created_at \
                 FROM mcp_audit ORDER BY created_at DESC, id DESC LIMIT ?1",
            )
            .map_err(sql_err)?;
        let rows = stmt
            .query_map(params![limit.clamp(1, 100) as i64], |row| {
                Ok(McpAuditEntry {
                    client_id: row.get(0)?,
                    tool: row.get(1)?,
                    outcome: row.get(2)?,
                    created_at: row.get(3)?,
                })
            })
            .map_err(sql_err)?
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(sql_err)?;
        Ok(rows)
    }

    /// Append a metadata-only MCP audit row. Arguments and results are never
    /// accepted here, so a caller cannot accidentally persist a body. The
    /// caller-supplied client and account ids are re-read from persistent Core
    /// state; unknown identifiers and every target id are written as NULL.
    pub fn record_mcp_audit(
        &self,
        client_id: &str,
        tool: &'static str,
        account_id: Option<&crate::model::AccountId>,
        outcome: &'static str,
    ) -> Result<(), CoreError> {
        if tool.len() > 128 || outcome.len() > 64 {
            return Err(CoreError::from_code(ErrorCode::InvalidArgument));
        }
        let conn = self.db.conn();
        let client_id = conn
            .query_row(
                "SELECT id FROM mcp_clients WHERE id = ?1",
                params![client_id],
                |row| row.get::<_, String>(0),
            )
            .optional()
            .map_err(sql_err)?;
        let account_id = match account_id {
            Some(account_id) => conn
                .query_row(
                    "SELECT id FROM accounts WHERE id = ?1",
                    params![account_id.as_str()],
                    |row| row.get::<_, String>(0),
                )
                .optional()
                .map_err(sql_err)?,
            None => None,
        };
        conn.execute(
            "INSERT INTO mcp_audit \
                 (client_id, tool, account_id, target_id, outcome, created_at) \
                 VALUES (?1, ?2, ?3, ?4, ?5, strftime('%s','now'))",
            params![client_id, tool, account_id, Option::<String>::None, outcome],
        )
        .map_err(sql_err)?;
        Ok(())
    }

    pub fn diagnostics_snapshot(&self) -> Result<DiagnosticsSnapshot, CoreError> {
        let conn = self.db.conn();
        let scalar = |sql: &str| {
            conn.query_row(sql, [], |row| row.get::<_, i64>(0))
                .map(|value| value.max(0) as u64)
                .map_err(sql_err)
        };
        let queue = self.queue_counts()?;
        Ok(DiagnosticsSnapshot {
            accounts: scalar("SELECT COUNT(*) FROM accounts")?,
            messages: scalar("SELECT COUNT(*) FROM messages")?,
            indexed_messages: scalar("SELECT COUNT(*) FROM messages_fts")?,
            pending_index: scalar("SELECT COUNT(*) FROM fts_pending")?,
            pending_operations: queue.pending as u64,
            failed_operations: queue.failed as u64,
            cached_bodies: scalar("SELECT COUNT(*) FROM messages WHERE body_path IS NOT NULL")?,
            cache_bytes: scalar("SELECT COALESCE(SUM(body_bytes), 0) FROM messages")?,
        })
    }

    /// Invalidates the FTS index and queues every current message for the
    /// existing bounded background indexer. It never parses mail on the
    /// caller's thread.
    pub fn rebuild_search_index(&self) -> Result<usize, CoreError> {
        let conn = self.db.conn();
        let tx = conn.unchecked_transaction().map_err(sql_err)?;
        tx.execute("DELETE FROM messages_fts", [])
            .map_err(sql_err)?;
        // The FTS table is deliberately rebuilt from scratch, so its v19
        // message-id -> rowid map must be rebuilt in lockstep. Leaving old
        // rowids here would make the next incremental replacement target a
        // row that no longer exists.
        tx.execute("DELETE FROM fts_message_rows", [])
            .map_err(sql_err)?;
        tx.execute("DELETE FROM fts_pending", []).map_err(sql_err)?;
        let queued = tx
            .execute(
                "INSERT INTO fts_pending (message_id, queued_at) \
                 SELECT id, strftime('%s','now') FROM messages",
                [],
            )
            .map_err(sql_err)?;
        tx.commit().map_err(sql_err)?;
        Ok(queued)
    }

    /// Detaches cached RFC822 files from SQLite first, then removes only
    /// those exact paths. A missing file is already a successful clear.
    pub fn clear_body_cache(&self) -> Result<usize, CoreError> {
        let conn = self.db.conn();
        let paths: Vec<PathBuf> = {
            let mut stmt = conn
                .prepare("SELECT body_path FROM messages WHERE body_path IS NOT NULL")
                .map_err(sql_err)?;
            let rows = stmt
                .query_map([], |row| row.get::<_, String>(0).map(PathBuf::from))
                .map_err(sql_err)?
                .collect::<rusqlite::Result<Vec<_>>>()
                .map_err(sql_err)?;
            rows
        };
        let tx = conn.unchecked_transaction().map_err(sql_err)?;
        tx.execute(
            "UPDATE messages SET body_path = NULL, body_bytes = NULL \
             WHERE body_path IS NOT NULL",
            [],
        )
        .map_err(sql_err)?;
        tx.execute(
            "INSERT OR IGNORE INTO fts_pending (message_id, queued_at) \
             SELECT id, strftime('%s','now') FROM messages",
            [],
        )
        .map_err(sql_err)?;
        tx.commit().map_err(sql_err)?;
        for path in &paths {
            match fs::remove_file(path) {
                Ok(()) => {}
                Err(err) if err.kind() == io::ErrorKind::NotFound => {}
                Err(_) => {
                    // The database is already fail-closed: no future read can
                    // expose this orphan. A later cache sweep may remove it.
                }
            }
        }
        Ok(paths.len())
    }

    /// Writes a small ZIP containing aggregate state only. No subjects,
    /// addresses, bodies, paths, operation payloads or credentials enter it.
    pub fn export_diagnostics(&self, destination: &Path) -> Result<(), CoreError> {
        let snapshot = self.diagnostics_snapshot()?;
        let report = format!(
            "Feather Mail diagnostics\nversion={}\naccounts={}\nmessages={}\nindexed_messages={}\npending_index={}\npending_operations={}\nfailed_operations={}\ncached_bodies={}\ncache_bytes={}\n",
            env!("CARGO_PKG_VERSION"),
            snapshot.accounts,
            snapshot.messages,
            snapshot.indexed_messages,
            snapshot.pending_index,
            snapshot.pending_operations,
            snapshot.failed_operations,
            snapshot.cached_bodies,
            snapshot.cache_bytes,
        );
        write_single_file_zip(destination, "diagnostics.txt", report.as_bytes()).map_err(|_| {
            CoreError::new(
                ErrorCode::InvalidArgument,
                "Could not write diagnostics export.",
            )
        })
    }
}

// Minimal PKZIP "stored" writer. Keeping this tiny avoids pulling an archive
// parser into Core just to export one bounded, generated text file.
fn write_single_file_zip(path: &Path, name: &str, data: &[u8]) -> io::Result<()> {
    let name = name.as_bytes();
    let name_len = u16::try_from(name.len()).map_err(|_| io::Error::other("long zip name"))?;
    let data_len = u32::try_from(data.len()).map_err(|_| io::Error::other("large report"))?;
    let crc = crc32(data);
    let mut file = fs::File::create(path)?;
    write_u32(&mut file, 0x0403_4b50)?;
    write_u16(&mut file, 20)?;
    write_u16(&mut file, 0)?;
    write_u16(&mut file, 0)?;
    write_u16(&mut file, 0)?;
    write_u16(&mut file, 0)?;
    write_u32(&mut file, crc)?;
    write_u32(&mut file, data_len)?;
    write_u32(&mut file, data_len)?;
    write_u16(&mut file, name_len)?;
    write_u16(&mut file, 0)?;
    file.write_all(name)?;
    file.write_all(data)?;
    let central_offset = 30_u32 + u32::from(name_len) + data_len;
    write_u32(&mut file, 0x0201_4b50)?;
    write_u16(&mut file, 20)?;
    write_u16(&mut file, 20)?;
    write_u16(&mut file, 0)?;
    write_u16(&mut file, 0)?;
    write_u16(&mut file, 0)?;
    write_u16(&mut file, 0)?;
    write_u32(&mut file, crc)?;
    write_u32(&mut file, data_len)?;
    write_u32(&mut file, data_len)?;
    write_u16(&mut file, name_len)?;
    write_u16(&mut file, 0)?;
    write_u16(&mut file, 0)?;
    write_u16(&mut file, 0)?;
    write_u16(&mut file, 0)?;
    write_u32(&mut file, 0)?;
    write_u32(&mut file, 0)?;
    file.write_all(name)?;
    let central_size = 46_u32 + u32::from(name_len);
    write_u32(&mut file, 0x0605_4b50)?;
    write_u16(&mut file, 0)?;
    write_u16(&mut file, 0)?;
    write_u16(&mut file, 1)?;
    write_u16(&mut file, 1)?;
    write_u32(&mut file, central_size)?;
    write_u32(&mut file, central_offset)?;
    write_u16(&mut file, 0)?;
    file.flush()
}

fn write_u16(out: &mut impl Write, value: u16) -> io::Result<()> {
    out.write_all(&value.to_le_bytes())
}

fn write_u32(out: &mut impl Write, value: u32) -> io::Result<()> {
    out.write_all(&value.to_le_bytes())
}

fn crc32(data: &[u8]) -> u32 {
    let mut crc = !0_u32;
    for byte in data {
        crc ^= u32::from(*byte);
        for _ in 0..8 {
            crc = (crc >> 1) ^ (0xedb8_8320 & 0_u32.wrapping_sub(crc & 1));
        }
    }
    !crc
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mcp_audit_projection_is_bounded_ordered_and_metadata_only() {
        let mut core = Core::memory().unwrap();
        core.set_mcp_enabled(0, true).unwrap();
        let conn = core.db.conn();
        conn.execute(
            "INSERT INTO mcp_audit (client_id, tool, outcome, created_at) \
             VALUES ('stdio', 'list_accounts', 'allowed', 1)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO mcp_audit (client_id, tool, outcome, created_at) \
             VALUES (NULL, 'get_account', 'denied', 2)",
            [],
        )
        .unwrap();

        assert_eq!(
            core.list_mcp_audit(1).unwrap(),
            vec![McpAuditEntry {
                client_id: "unknown".into(),
                tool: "get_account".into(),
                outcome: "denied".into(),
                created_at: 2,
            }]
        );
        assert_eq!(
            core.list_mcp_audit(200).unwrap(),
            vec![
                McpAuditEntry {
                    client_id: "unknown".into(),
                    tool: "get_account".into(),
                    outcome: "denied".into(),
                    created_at: 2,
                },
                McpAuditEntry {
                    client_id: "stdio".into(),
                    tool: "list_accounts".into(),
                    outcome: "allowed".into(),
                    created_at: 1,
                },
            ]
        );

        for created_at in 3..=103 {
            conn.execute(
                "INSERT INTO mcp_audit (client_id, tool, outcome, created_at) \
                 VALUES (NULL, 'get_account', 'denied', ?1)",
                [created_at],
            )
            .unwrap();
        }
        let capped = core.list_mcp_audit(200).unwrap();
        assert_eq!(capped.len(), 100);
        assert_eq!(capped.first().unwrap().created_at, 103);
        assert_eq!(capped.last().unwrap().created_at, 4);
    }

    #[test]
    fn export_is_a_zip_and_contains_no_mail_or_secret_fields() {
        let core = Core::memory().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("diagnostics.zip");
        core.export_diagnostics(&path).unwrap();
        let bytes = fs::read(path).unwrap();
        assert!(bytes.starts_with(b"PK\x03\x04"));
        let text = String::from_utf8_lossy(&bytes).to_ascii_lowercase();
        for forbidden in ["password", "oauth", "token", "subject", "body_path"] {
            assert!(!text.contains(forbidden), "leaked field name: {forbidden}");
        }
    }

    #[test]
    fn rebuild_queues_every_message_without_parsing_it_inline() {
        let core = Core::memory().unwrap();
        let conn = core.db.conn();
        conn.execute(
            "INSERT INTO accounts (id, name, email, provider, status, download_policy, created_at, updated_at) VALUES ('a', 'A', 'a@example.com', 'generic', 'synced', 'recent', 0, 0)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO folders (id, account_id, name, kind) VALUES ('inbox', 'a', 'Inbox', 'inbox')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO threads (id, account_id, folder_id, subject, snippet, date) VALUES ('t', 'a', 'inbox', '', '', 0)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO messages (id, account_id, thread_id, folder_id, date) VALUES ('m', 'a', 't', 'inbox', 0)",
            [],
        )
        .unwrap();
        let messages = core.diagnostics_snapshot().unwrap().messages;
        assert_eq!(core.rebuild_search_index().unwrap() as u64, messages);
        assert_eq!(core.diagnostics_snapshot().unwrap().pending_index, messages);
    }
}
