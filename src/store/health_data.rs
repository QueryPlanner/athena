//! Reads of `health_points` for the model-facing tools (`health::data`):
//! how much there is, a page of points, and batches for an export.
//!
//! Every method takes the owner and filters by it, and every method holds
//! the store's single connection only for one bounded query (a few hundred
//! rows, or a few MiB of values), so a long read is many short ones with the
//! connection free in between.
use super::Store;
use anyhow::{Context, Result};
use jiff::{ToSpan, civil::Date, tz::TimeZone};
use rusqlite::{params_from_iter, types::Value};

/// How far, in days, the indexed `start_ms` band is widened on each side of
/// the asked-for dates. `civil_date` is the day in the zone the user had
/// when the point was synced, so a user who has since moved can have a point
/// whose local day differs from the same instant's day in their current
/// zone, by at most the spread of the world's offsets (26 hours). The band
/// only narrows the index scan; `civil_date` decides.
pub const BAND_DAYS: i64 = 2;

/// The days a read is limited to: inclusive civil dates, and the `start_ms`
/// band that contains every row of those days.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Window {
    /// `YYYY-MM-DD`, inclusive.
    pub from: Option<String>,
    pub to: Option<String>,
    /// `start_ms >= lo`.
    lo_ms: Option<i64>,
    /// `start_ms < hi`.
    hi_ms: Option<i64>,
}

fn midnight_ms(date: Date, zone: &TimeZone) -> Result<i64> {
    Ok(date
        .to_zoned(zone.clone())
        .with_context(|| format!("{date} is out of range"))?
        .timestamp()
        .as_millisecond())
}

impl Window {
    /// The window of `from` to `to` (either may be absent) in `zone`.
    pub fn new(from: Option<Date>, to: Option<Date>, zone: &TimeZone) -> Result<Self> {
        let lo_ms = from
            .map(|d| midnight_ms(d.saturating_sub(BAND_DAYS.days()), zone))
            .transpose()?;
        // The day after `to`, plus the slack: an exclusive bound.
        let hi_ms = to
            .map(|d| midnight_ms(d.saturating_add((BAND_DAYS + 1).days()), zone))
            .transpose()?;
        Ok(Self {
            from: from.map(|d| d.to_string()),
            to: to.map(|d| d.to_string()),
            lo_ms,
            hi_ms,
        })
    }

    /// Whether any date is asked for. Rows with no `civil_date` appear only
    /// when none is.
    pub fn is_open(&self) -> bool {
        self.from.is_none() && self.to.is_none()
    }

    /// SQL conditions and their parameters on `civil_date`, and on
    /// `start_ms` as `band` says.
    fn conditions(&self, band: Band) -> (Vec<&'static str>, Vec<Value>) {
        let mut sql = Vec::new();
        let mut args = Vec::new();
        if let Some(from) = &self.from {
            sql.push("civil_date >= ?");
            args.push(Value::Text(from.clone()));
        }
        if let Some(to) = &self.to {
            sql.push("civil_date <= ?");
            args.push(Value::Text(to.clone()));
        }
        if band != Band::Off {
            let or_unplaced = band == Band::Any;
            if let Some(lo) = self.lo_ms {
                sql.push(if or_unplaced {
                    "(start_ms IS NULL OR start_ms >= ?)"
                } else {
                    "start_ms >= ?"
                });
                args.push(Value::Integer(lo));
            }
            if let Some(hi) = self.hi_ms {
                sql.push(if or_unplaced {
                    "(start_ms IS NULL OR start_ms < ?)"
                } else {
                    "start_ms < ?"
                });
                args.push(Value::Integer(hi));
            }
        }
        (sql, args)
    }
}

/// How the `start_ms` band applies.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Band {
    /// Not at all.
    Off,
    /// To rows that have a `start_ms`.
    Placed,
    /// To every row, a row with no `start_ms` passing: only `civil_date`
    /// can tell where it belongs.
    Any,
}

/// How much of one data type is stored.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TypeSize {
    pub rows: i64,
    /// The earliest and latest `start_ms`, with the `civil_date` of those
    /// rows. Absent when no row has a `start_ms`.
    pub first: Option<(i64, Option<String>)>,
    pub last: Option<(i64, Option<String>)>,
    /// The mean size of a stored value in bytes, over a sample.
    pub avg_value_bytes: i64,
}

/// A stored point.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PointRecord {
    pub id: i64,
    pub data_type: String,
    pub key: String,
    pub start_ms: Option<i64>,
    pub end_ms: Option<i64>,
    pub civil_date: Option<String>,
    pub source: Option<String>,
    /// The point's JSON, whole.
    pub value: String,
}

/// The last point a page returned: where the next one starts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum After {
    /// A point with a `start_ms`.
    Start { start_ms: i64, id: i64 },
    /// A point with none: those come last.
    Unplaced { id: i64 },
}

/// One page of points of one data type.
#[derive(Clone, Debug)]
pub struct PageQuery<'a> {
    pub data_type: &'a str,
    pub window: &'a Window,
    pub after: Option<After>,
    pub limit: usize,
    /// Stop after the point that takes the values past this many bytes.
    pub max_bytes: usize,
}

/// A page of points, in order, and whether there are more.
#[derive(Clone, Debug, Default)]
pub struct Page {
    pub rows: Vec<PointRecord>,
    pub more: bool,
}

/// A batch of an export.
#[derive(Clone, Debug)]
pub struct BatchQuery<'a> {
    pub data_types: &'a [&'a str],
    pub window: &'a Window,
    /// Points with a larger `id` than this.
    pub after_id: i64,
    pub max_rows: usize,
    /// Stop after the point that takes the values past this many bytes.
    pub max_bytes: usize,
}

/// The points sampled to estimate the mean size of a value.
pub const SIZE_SAMPLE: i64 = 500;

const COLUMNS: &str = "id, data_type, point_key, start_ms, end_ms, civil_date, source, value";

fn record(r: &rusqlite::Row<'_>) -> rusqlite::Result<PointRecord> {
    Ok(PointRecord {
        id: r.get(0)?,
        data_type: r.get(1)?,
        key: r.get(2)?,
        start_ms: r.get(3)?,
        end_ms: r.get(4)?,
        civil_date: r.get(5)?,
        source: r.get(6)?,
        value: r.get(7)?,
    })
}

impl Store {
    /// How many points of `data_type` the owner has in `window`, the first
    /// and last by `start_ms`, and the mean size of their values over a
    /// sample (never the sum over the table, which would read every value).
    ///
    /// Points with a `start_ms` are counted through the index band, and
    /// those without one separately, so a narrow window reads a narrow
    /// range of the index. The sample is of the first points by `start_ms`
    /// (of the unplaced ones if there are no others).
    pub fn health_type_size(
        &self,
        owner: i64,
        data_type: &str,
        window: &Window,
    ) -> Result<TypeSize> {
        let filter = |placed: bool| {
            let band = if placed { Band::Placed } else { Band::Off };
            let (conditions, args) = window.conditions(band);
            let mut sql = format!(
                "user_id = ? AND data_type = ? AND start_ms IS {}NULL",
                if placed { "NOT " } else { "" }
            );
            for condition in conditions {
                sql.push_str(" AND ");
                sql.push_str(condition);
            }
            let mut all = vec![Value::Integer(owner), Value::Text(data_type.into())];
            all.extend(args);
            (sql, all)
        };
        let db = self.db();
        let count = |(sql, args): &(String, Vec<Value>)| -> Result<i64> {
            Ok(db.query_row(
                &format!("SELECT COUNT(*) FROM health_points WHERE {sql}"),
                params_from_iter(args),
                |r| r.get(0),
            )?)
        };
        let (placed, unplaced) = (filter(true), filter(false));
        let (with, without) = (count(&placed)?, count(&unplaced)?);
        if with + without == 0 {
            return Ok(TypeSize::default());
        }
        let edge = |order: &str| -> Result<Option<(i64, Option<String>)>> {
            let mut q = db.prepare_cached(&format!(
                "SELECT start_ms, civil_date FROM health_points
                 WHERE {} ORDER BY start_ms {order}, id {order} LIMIT 1",
                placed.0
            ))?;
            let mut found = q.query(params_from_iter(&placed.1))?;
            Ok(match found.next()? {
                Some(r) => Some((r.get(0)?, r.get(1)?)),
                None => None,
            })
        };
        let (first, last) = (edge("ASC")?, edge("DESC")?);
        let (sql, mut args) = if with > 0 { placed } else { unplaced };
        args.push(Value::Integer(SIZE_SAMPLE));
        let avg: f64 = db.query_row(
            &format!(
                "SELECT COALESCE(AVG(LENGTH(CAST(value AS BLOB))), 0) FROM
                 (SELECT value FROM health_points WHERE {sql} LIMIT ?)"
            ),
            params_from_iter(&args),
            |r| r.get(0),
        )?;
        Ok(TypeSize {
            rows: with + without,
            first,
            last,
            avg_value_bytes: avg.round() as i64,
        })
    }

    /// A page of the owner's points of `query.data_type` in `query.window`,
    /// ordered by `start_ms`, then `id`, the points with no `start_ms` last
    /// (by `id`): the order of the `(user_id, data_type, start_ms)` index, so
    /// no sort is needed however many points match. The first `limit`
    /// points after `query.after`, or fewer if their values pass
    /// `max_bytes` first (the point that does is the last one returned).
    /// At least one point is returned when there is one.
    pub fn health_points_page(&self, owner: i64, query: &PageQuery<'_>) -> Result<Page> {
        let want = query.limit + 1;
        let mut rows: Vec<PointRecord> = Vec::new();
        let mut bytes = 0usize;
        let db = self.db();
        let base = |placed: bool| {
            let band = if placed { Band::Placed } else { Band::Off };
            let (conditions, args) = query.window.conditions(band);
            let mut sql = format!(
                "SELECT {COLUMNS} FROM health_points WHERE user_id = ? AND data_type = ?
                 AND start_ms IS {}NULL",
                if placed { "NOT " } else { "" }
            );
            for condition in conditions {
                sql.push_str(" AND ");
                sql.push_str(condition);
            }
            let mut all = vec![Value::Integer(owner), Value::Text(query.data_type.into())];
            all.extend(args);
            (sql, all)
        };
        if !matches!(query.after, Some(After::Unplaced { .. })) {
            let (mut sql, mut args) = base(true);
            if let Some(After::Start { start_ms, id }) = query.after {
                sql.push_str(" AND (start_ms, id) > (?, ?)");
                args.push(Value::Integer(start_ms));
                args.push(Value::Integer(id));
            }
            sql.push_str(" ORDER BY start_ms, id LIMIT ?");
            args.push(Value::Integer(want as i64));
            read(&db, &sql, &args, query.max_bytes, &mut rows, &mut bytes)?;
        }
        if rows.len() < want && bytes <= query.max_bytes {
            let (mut sql, mut args) = base(false);
            if let Some(After::Unplaced { id }) = query.after {
                sql.push_str(" AND id > ?");
                args.push(Value::Integer(id));
            }
            sql.push_str(" ORDER BY id LIMIT ?");
            args.push(Value::Integer((want - rows.len()) as i64));
            read(&db, &sql, &args, query.max_bytes, &mut rows, &mut bytes)?;
        }
        // The extra point only says there is more; so does going over the
        // byte limit, which can be wrong when that point was the last one.
        let more = rows.len() > query.limit || bytes > query.max_bytes;
        rows.truncate(query.limit);
        Ok(Page { rows, more })
    }

    /// The next batch of the owner's points for an export: of the data
    /// types in `query.data_types` (all if empty), in `query.window`, with
    /// an `id` over `query.after_id`, in `id` order. At most `max_rows`
    /// points, fewer if their values pass `max_bytes` first (the point that
    /// does is the last one returned). Empty when there are no more.
    pub fn health_export_batch(
        &self,
        owner: i64,
        query: &BatchQuery<'_>,
    ) -> Result<Vec<PointRecord>> {
        let (conditions, window_args) = query.window.conditions(Band::Any);
        // NOT INDEXED: walk the table by `id` from where the last batch
        // ended. Left to itself the planner can pick the type index and sort
        // every matching row for each batch.
        let mut sql =
            format!("SELECT {COLUMNS} FROM health_points NOT INDEXED WHERE id > ? AND user_id = ?");
        let mut args = vec![Value::Integer(query.after_id), Value::Integer(owner)];
        if !query.data_types.is_empty() {
            let marks = vec!["?"; query.data_types.len()].join(", ");
            sql.push_str(&format!(" AND data_type IN ({marks})"));
            args.extend(query.data_types.iter().map(|t| Value::Text((*t).into())));
        }
        for condition in conditions {
            sql.push_str(" AND ");
            sql.push_str(condition);
        }
        sql.push_str(" ORDER BY id LIMIT ?");
        args.extend(window_args);
        args.push(Value::Integer(query.max_rows as i64));
        let (mut out, mut bytes) = (Vec::new(), 0usize);
        read(
            &self.db(),
            &sql,
            &args,
            query.max_bytes,
            &mut out,
            &mut bytes,
        )?;
        Ok(out)
    }
}

/// Run `sql`, adding points to `rows` until their values pass `max_bytes`
/// (the point that does is kept).
fn read(
    db: &rusqlite::Connection,
    sql: &str,
    args: &[Value],
    max_bytes: usize,
    rows: &mut Vec<PointRecord>,
    bytes: &mut usize,
) -> Result<()> {
    let mut q = db.prepare_cached(sql)?;
    let mut found = q.query(params_from_iter(args))?;
    while let Some(r) = found.next()? {
        let point = record(r)?;
        *bytes += point.value.len();
        rows.push(point);
        if *bytes > max_bytes {
            break;
        }
    }
    Ok(())
}
