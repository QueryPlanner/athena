//! Session persistence. One table, messages stored as opaque Rig JSON.

use anyhow::Result;
use rig_agent::prelude::Message;
use rusqlite::Connection;

pub fn open(path: &str) -> Result<Connection> {
    let db = Connection::open(path)?;
    db.pragma_update(None, "journal_mode", "WAL")?;
    db.execute_batch(
        "CREATE TABLE IF NOT EXISTS messages (
             session_id TEXT NOT NULL,
             seq        INTEGER NOT NULL,
             json       TEXT NOT NULL,
             PRIMARY KEY (session_id, seq)
         );",
    )?;
    Ok(db)
}

pub fn load(db: &Connection, session: &str) -> Result<Vec<Message>> {
    let mut q = db.prepare("SELECT json FROM messages WHERE session_id=?1 ORDER BY seq")?;
    let rows = q.query_map([session], |r| r.get::<_, String>(0))?;
    rows.map(|r| Ok(serde_json::from_str(&r?)?)).collect()
}

pub fn save(db: &Connection, session: &str, history: &[Message]) -> Result<()> {
    let tx = db.unchecked_transaction()?;
    tx.execute("DELETE FROM messages WHERE session_id=?1", [session])?;
    for (i, m) in history.iter().enumerate() {
        tx.execute(
            "INSERT INTO messages (session_id, seq, json) VALUES (?1, ?2, ?3)",
            rusqlite::params![session, i as i64, serde_json::to_string(m)?],
        )?;
    }
    tx.commit()?;
    Ok(())
}

pub fn sessions(db: &Connection) -> Result<Vec<(String, i64)>> {
    let mut q = db.prepare(
        "SELECT session_id, COUNT(*) FROM messages GROUP BY session_id ORDER BY session_id",
    )?;
    let rows = q.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?;
    Ok(rows.collect::<std::result::Result<_, _>>()?)
}
