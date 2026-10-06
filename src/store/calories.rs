//! SQLite operations for user-owned calorie records.
use super::{Store, now_millis};
use crate::calories::{History, Log, Meal, Range, Remove, Update};
use anyhow::{Result, ensure};
use rusqlite::{Connection, OptionalExtension, Row, params};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

const FIELDS: &str = "id, description, consumed_date, calories, protein_g, carbs_g, fat_g, meal_type, source, version, created_at, updated_at, deleted_at";
fn record(row: &Row<'_>) -> rusqlite::Result<Value> {
    Ok(
        json!({"id":row.get::<_,i64>(0)?,"description":row.get::<_,String>(1)?,"consumed_date":row.get::<_,String>(2)?,
        "calories":row.get::<_,Option<f64>>(3)?,"protein_g":row.get::<_,Option<f64>>(4)?,"carbs_g":row.get::<_,Option<f64>>(5)?,"fat_g":row.get::<_,Option<f64>>(6)?,
        "meal_type":row.get::<_,Option<String>>(7)?,"source":row.get::<_,String>(8)?,"version":row.get::<_,i64>(9)?,
        "created_at":row.get::<_,i64>(10)?,"updated_at":row.get::<_,i64>(11)?,"deleted_at":row.get::<_,Option<i64>>(12)?}),
    )
}
fn entry(db: &Connection, owner: i64, id: i64) -> Result<Value> {
    let result = db.query_row(
        &format!("SELECT {FIELDS} FROM calorie_logs WHERE user_id=?1 AND id=?2"),
        params![owner, id],
        record,
    );
    Ok(result?)
}
fn source(meal: &Meal) -> &'static str {
    match meal.source {
        crate::calories::Source::User => "user",
        crate::calories::Source::Estimated => "estimated",
    }
}
impl Store {
    pub fn calorie_log(&self, owner: i64, mut args: Log) -> Result<Value> {
        args.validate()?;
        // IEEE signed zero represents the same nutritional quantity.
        for n in [
            &mut args.meal.calories,
            &mut args.meal.protein_g,
            &mut args.meal.carbs_g,
            &mut args.meal.fat_g,
        ]
        .into_iter()
        .flatten()
        {
            if *n == 0.0 {
                *n = 0.0;
            }
        }
        let hash = format!("{:x}", Sha256::digest(serde_json::to_vec(&args.meal)?));
        let mut db = self.db();
        let tx = db.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let prior: Option<(i64, String)> = tx
            .query_row(
                "SELECT id, request_hash FROM calorie_logs WHERE user_id=?1 AND request_key=?2",
                params![owner, args.request_key],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        let (id, existed) = match prior {
            Some((id, original)) => {
                ensure!(
                    original == hash,
                    "request_key conflicts with a different original meal"
                );
                (id, true)
            }
            None => {
                let m = &args.meal;
                tx.execute("INSERT INTO calorie_logs(user_id,request_key,request_hash,description,consumed_date,calories,protein_g,carbs_g,fat_g,meal_type,source,created_at,updated_at) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?12)",params![owner,args.request_key,hash,m.description,m.consumed_date,m.calories,m.protein_g,m.carbs_g,m.fat_g,m.meal_type,source(m),now_millis()])?;
                (tx.last_insert_rowid(), false)
            }
        };
        let result = json!({"entry":entry(&tx,owner,id)?,"already_existed":existed});
        tx.commit()?;
        Ok(result)
    }
    pub fn calorie_history(&self, owner: i64, args: History) -> Result<Value> {
        Range {
            start_date: args.start_date.clone(),
            end_date: args.end_date.clone(),
        }
        .validate()?;
        let limit = args.limit.unwrap_or(20);
        ensure!((1..=50).contains(&limit), "limit must be between 1 and 50");
        ensure!(
            args.before_id.is_none_or(|id| id > 0),
            "before_id must be positive"
        );
        let db = self.db();
        let mut q = db.prepare(&format!("SELECT {FIELDS} FROM calorie_logs WHERE user_id=?1 AND deleted_at IS NULL AND consumed_date BETWEEN ?2 AND ?3 AND (?4 IS NULL OR id<?4) ORDER BY id DESC LIMIT ?5"))?;
        let bindings = params![
            owner,
            args.start_date,
            args.end_date,
            args.before_id,
            limit + 1
        ];
        let rows = q.query_map(bindings, record)?;
        let mut entries = rows.collect::<rusqlite::Result<Vec<_>>>()?;
        let next = if entries.len() > limit as usize {
            entries.truncate(limit as usize);
            entries.last().map(|e| e["id"].clone())
        } else {
            None
        };
        Ok(json!({"entries":entries,"next_before_id":next}))
    }
    pub fn calorie_summary(&self, owner: i64, args: Range) -> Result<Value> {
        args.validate()?;
        let db = self.db();
        Ok(db.query_row("SELECT COUNT(*), SUM(calories), SUM(protein_g), SUM(carbs_g), SUM(fat_g), COUNT(*)-COUNT(calories), COUNT(*)-COUNT(protein_g), COUNT(*)-COUNT(carbs_g), COUNT(*)-COUNT(fat_g) FROM calorie_logs WHERE user_id=?1 AND deleted_at IS NULL AND consumed_date BETWEEN ?2 AND ?3",params![owner,args.start_date,args.end_date],|r| Ok(json!({"entry_count":r.get::<_,i64>(0)?,"totals":{"calories":r.get::<_,Option<f64>>(1)?,"protein_g":r.get::<_,Option<f64>>(2)?,"carbs_g":r.get::<_,Option<f64>>(3)?,"fat_g":r.get::<_,Option<f64>>(4)?},"missing":{"calories":r.get::<_,i64>(5)?,"protein_g":r.get::<_,i64>(6)?,"carbs_g":r.get::<_,i64>(7)?,"fat_g":r.get::<_,i64>(8)?}})))?)
    }
    pub fn calorie_update(&self, owner: i64, args: Update) -> Result<Value> {
        args.meal.validate()?;
        ensure!(
            args.id > 0 && args.expected_version > 0,
            "id and expected_version must be positive"
        );
        let mut db = self.db();
        let tx = db.transaction()?;
        let m = &args.meal;
        let changed = tx.execute("UPDATE calorie_logs SET description=?1,consumed_date=?2,calories=?3,protein_g=?4,carbs_g=?5,fat_g=?6,meal_type=?7,source=?8,updated_at=?9,version=version+1 WHERE user_id=?10 AND id=?11 AND version=?12 AND deleted_at IS NULL",params![m.description,m.consumed_date,m.calories,m.protein_g,m.carbs_g,m.fat_g,m.meal_type,source(m),now_millis(),owner,args.id,args.expected_version])?;
        ensure!(changed == 1, "entry missing, deleted, or version conflicts");
        let result = entry(&tx, owner, args.id)?;
        tx.commit()?;
        Ok(result)
    }
    pub fn calorie_remove(&self, owner: i64, args: Remove) -> Result<Value> {
        ensure!(
            args.id > 0 && args.expected_version > 0,
            "id and expected_version must be positive"
        );
        let mut db = self.db();
        let tx = db.transaction()?;
        let changed = tx.execute("UPDATE calorie_logs SET deleted_at=?1,updated_at=?1,version=version+1 WHERE user_id=?2 AND id=?3 AND version=?4 AND deleted_at IS NULL",params![now_millis(),owner,args.id,args.expected_version])?;
        ensure!(changed == 1, "entry missing, deleted, or version conflicts");
        let result = entry(&tx, owner, args.id)?;
        tx.commit()?;
        Ok(result)
    }
}
