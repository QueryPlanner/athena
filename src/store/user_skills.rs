//! SQLite operations for each user's own skills and the previews that
//! stand between a request and a saved skill. See `crate::user_skills`.
use super::Store;
use crate::user_skills::{
    GitSource, MAX_PENDING, MAX_USER_SKILLS, PREVIEW_TTL_MS, SkillFile, SkillRecord,
};
use anyhow::{Result, bail};
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};

/// One line of `skill_list`: a skill without its body and files.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillSummary {
    pub name: String,
    pub description: String,
    pub source: Option<GitSource>,
    pub files: i64,
    /// Milliseconds since the Unix epoch.
    pub updated_at: i64,
}

fn source(repo: Option<String>, path: Option<String>, sha: Option<String>) -> Option<GitSource> {
    // The table's CHECK makes the three present together or not at all.
    Some(GitSource {
        repo: repo?,
        path: path?,
        sha: sha?,
    })
}

fn live_skill(db: &Connection, owner: i64, name: &str) -> Result<Option<(i64, SkillRecord)>> {
    let found = db
        .query_row(
            "SELECT id, name, description, body, source_repo, source_path, source_sha
             FROM user_skills WHERE user_id = ?1 AND name = ?2 AND deleted_at IS NULL",
            params![owner, name],
            |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    SkillRecord {
                        name: r.get(1)?,
                        description: r.get(2)?,
                        body: r.get(3)?,
                        source: source(r.get(4)?, r.get(5)?, r.get(6)?),
                        files: Vec::new(),
                    },
                ))
            },
        )
        .optional()?;
    let Some((id, mut record)) = found else {
        return Ok(None);
    };
    let mut q =
        db.prepare("SELECT path, content FROM user_skill_files WHERE skill_id = ?1 ORDER BY path")?;
    let rows = q.query_map([id], |r| {
        Ok(SkillFile {
            path: r.get(0)?,
            content: r.get(1)?,
        })
    })?;
    record.files = rows.collect::<rusqlite::Result<_>>()?;
    Ok(Some((id, record)))
}

impl Store {
    /// The owner's skills, by name, without bodies.
    pub fn user_skills(&self, owner: i64) -> Result<Vec<SkillSummary>> {
        let db = self.db();
        let mut q = db.prepare(
            "SELECT name, description, source_repo, source_path, source_sha, updated_at,
                    (SELECT COUNT(*) FROM user_skill_files WHERE skill_id = s.id)
             FROM user_skills AS s WHERE user_id = ?1 AND deleted_at IS NULL ORDER BY name",
        )?;
        let rows = q.query_map([owner], |r| {
            Ok(SkillSummary {
                name: r.get(0)?,
                description: r.get(1)?,
                source: source(r.get(2)?, r.get(3)?, r.get(4)?),
                updated_at: r.get(5)?,
                files: r.get(6)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// The owner's skill `name` with its files, unless there is none or it
    /// was removed.
    pub fn user_skill(&self, owner: i64, name: &str) -> Result<Option<SkillRecord>> {
        Ok(live_skill(&self.db(), owner, name)?.map(|(_, record)| record))
    }

    /// Keep `record` as the preview `preview_id` of the owner in `session`,
    /// for [`PREVIEW_TTL_MS`] after `now`. Drops the owner's expired
    /// previews, and all but the newest [`MAX_PENDING`].
    pub fn stage_user_skill(
        &self,
        owner: i64,
        session: &str,
        preview_id: &str,
        record: &SkillRecord,
        now: i64,
    ) -> Result<()> {
        let expires_at = now + PREVIEW_TTL_MS;
        let payload = serde_json::to_string(record)?;
        let mut db = self.db();
        let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
        tx.execute(
            "DELETE FROM skill_previews WHERE user_id = ?1 AND expires_at <= ?2",
            params![owner, now],
        )?;
        tx.execute(
            "INSERT INTO skill_previews
                 (user_id, preview_id, session_id, payload, created_at, expires_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![owner, preview_id, session, payload, now, expires_at],
        )?;
        tx.execute(
            "DELETE FROM skill_previews WHERE user_id = ?1 AND rowid NOT IN
                 (SELECT rowid FROM skill_previews WHERE user_id = ?1
                  ORDER BY created_at DESC, rowid DESC LIMIT ?2)",
            params![owner, MAX_PENDING as i64],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Save the skill previewed as `preview_id`, which must be the owner's,
    /// from `session` and not expired, and forget the preview. A skill of
    /// the same name, removed or not, is replaced whole. Writes nothing when
    /// it fails.
    pub fn confirm_user_skill(
        &self,
        owner: i64,
        session: &str,
        preview_id: &str,
        now: i64,
    ) -> Result<SkillRecord> {
        let mut db = self.db();
        let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let payload: Option<String> = tx
            .query_row(
                "SELECT payload FROM skill_previews WHERE user_id = ?1 AND preview_id = ?2
                 AND session_id = ?3 AND expires_at > ?4",
                params![owner, preview_id, session, now],
                |r| r.get(0),
            )
            .optional()?;
        let Some(payload) = payload else {
            bail!(
                "no pending preview with that id in this conversation; it may have \
                 expired or been used. Preview it again."
            );
        };
        let record: SkillRecord = serde_json::from_str(&payload)?;
        let replacing = live_skill(&tx, owner, &record.name)?.is_some();
        let count = "SELECT COUNT(*) FROM user_skills WHERE user_id = ?1 AND deleted_at IS NULL";
        let live: i64 = tx.query_row(count, [owner], |r| r.get(0))?;
        if !replacing && live >= MAX_USER_SKILLS as i64 {
            bail!("you already have {MAX_USER_SKILLS} skills; remove one first");
        }
        let (repo, path, sha) = match &record.source {
            Some(s) => (Some(&s.repo), Some(&s.path), Some(&s.sha)),
            None => (None, None, None),
        };
        let origin = if record.source.is_some() {
            "github"
        } else {
            "user"
        };
        // Every column is written, so nothing of a replaced or removed skill
        // survives into the new one.
        let upsert =
            "INSERT INTO user_skills (user_id, name, description, body, origin, source_repo,
                 source_path, source_sha, created_at, updated_at, deleted_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?9, NULL)
             ON CONFLICT (user_id, name) DO UPDATE SET description = excluded.description,
                 body = excluded.body, origin = excluded.origin,
                 source_repo = excluded.source_repo, source_path = excluded.source_path,
                 source_sha = excluded.source_sha, created_at = excluded.created_at,
                 updated_at = excluded.updated_at, deleted_at = NULL
             RETURNING id";
        let values = params![
            owner,
            record.name,
            record.description,
            record.body,
            origin,
            repo,
            path,
            sha,
            now
        ];
        let id: i64 = tx.query_row(upsert, values, |r| r.get(0))?;
        tx.execute("DELETE FROM user_skill_files WHERE skill_id = ?1", [id])?;
        for file in &record.files {
            tx.execute(
                "INSERT INTO user_skill_files (skill_id, path, content) VALUES (?1, ?2, ?3)",
                params![id, file.path, file.content],
            )?;
        }
        tx.execute(
            "DELETE FROM skill_previews WHERE user_id = ?1 AND preview_id = ?2",
            params![owner, preview_id],
        )?;
        tx.commit()?;
        Ok(record)
    }

    /// Remove the owner's skill `name`. Returns whether there was one.
    pub fn remove_user_skill(&self, owner: i64, name: &str, now: i64) -> Result<bool> {
        let changed = self.db().execute(
            "UPDATE user_skills SET deleted_at = ?1, updated_at = ?1
             WHERE user_id = ?2 AND name = ?3 AND deleted_at IS NULL",
            params![now, owner, name],
        )?;
        Ok(changed == 1)
    }
}
