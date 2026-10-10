//! The reads of `health_points` behind the health data tools, against a real
//! in-memory store: paging, date windows, sizes and export batches.
//!
//! Points are inserted with `health_points_put` and read back through the
//! public query types, so each test states what the store must return, not
//! how the SQL is written.

use athena::store::PointRow;
use athena::store::{After, BatchQuery, HealthWindow, PageQuery, PointRecord, Store, TypeSize};
use jiff::{Timestamp, civil::Date, tz::TimeZone};

const HEART: &str = "heart-rate";
const STEPS: &str = "steps";

fn utc() -> TimeZone {
    TimeZone::UTC
}

fn date(text: &str) -> Date {
    text.parse().unwrap()
}

/// A timestamp in milliseconds, from RFC 3339.
fn ms(text: &str) -> i64 {
    text.parse::<Timestamp>().unwrap().as_millisecond()
}

/// Midnight of `day` in `zone`, in milliseconds.
fn midnight(day: &str, zone: &TimeZone) -> i64 {
    date(day)
        .at(0, 0, 0, 0)
        .to_zoned(zone.clone())
        .unwrap()
        .timestamp()
        .as_millisecond()
}

fn point(key: &str, start: Option<i64>, civil: Option<&str>, value: &str) -> PointRow {
    PointRow {
        key: key.into(),
        start_ms: start,
        end_ms: start.map(|s| s + 60_000),
        civil_date: civil.map(str::to_string),
        value: value.into(),
        source: Some("fitbit".into()),
    }
}

fn store_with_user() -> (Store, i64) {
    let store = Store::open_in_memory().unwrap();
    let owner = store.user("telegram", "1").unwrap().id();
    (store, owner)
}

fn put(store: &Store, owner: i64, data_type: &str, rows: &[PointRow]) {
    store
        .health_points_put(owner, data_type, rows, Timestamp::UNIX_EPOCH)
        .unwrap();
}

fn keys(rows: &[PointRecord]) -> Vec<String> {
    rows.iter().map(|r| r.key.clone()).collect()
}

/// Every point of `data_type` in `window`, read `limit` at a time by
/// following `more` the way the tool does, and the number of pages read.
fn walk(
    store: &Store,
    owner: i64,
    data_type: &str,
    window: &HealthWindow,
    limit: usize,
    max_bytes: usize,
) -> (Vec<PointRecord>, usize) {
    let mut out = Vec::new();
    let mut after = None;
    let mut pages = 0;
    loop {
        let page = store
            .health_points_page(
                owner,
                &PageQuery {
                    data_type,
                    window,
                    after,
                    limit,
                    max_bytes,
                },
            )
            .unwrap();
        pages += 1;
        assert!(pages < 10_000, "paging did not end");
        let Some(last) = page.rows.last() else { break };
        after = Some(match last.start_ms {
            Some(start_ms) => After::Start {
                start_ms,
                id: last.id,
            },
            None => After::Unplaced { id: last.id },
        });
        out.extend(page.rows.iter().cloned());
        if !page.more {
            break;
        }
    }
    (out, pages)
}

fn open(window_from: Option<&str>, window_to: Option<&str>) -> HealthWindow {
    HealthWindow::new(window_from.map(date), window_to.map(date), &utc()).unwrap()
}

// ---------------------------------------------------------------- windows

#[test]
fn a_window_without_dates_is_open_and_one_with_either_is_not() {
    assert!(open(None, None).is_open());
    assert!(!open(Some("2026-03-01"), None).is_open());
    assert!(!open(None, Some("2026-03-01")).is_open());
    assert!(!open(Some("2026-03-01"), Some("2026-03-01")).is_open());
    let w = open(Some("2026-03-01"), Some("2026-03-05"));
    assert_eq!(w.from.as_deref(), Some("2026-03-01"));
    assert_eq!(w.to.as_deref(), Some("2026-03-05"));
}

#[test]
fn a_from_only_window_keeps_days_from_that_one_on() {
    let (store, owner) = store_with_user();
    put(
        &store,
        owner,
        HEART,
        &[
            point(
                "a",
                Some(ms("2026-03-01T12:00:00Z")),
                Some("2026-03-01"),
                "{}",
            ),
            point(
                "b",
                Some(ms("2026-03-10T12:00:00Z")),
                Some("2026-03-10"),
                "{}",
            ),
        ],
    );
    let (rows, _) = walk(
        &store,
        owner,
        HEART,
        &open(Some("2026-03-05"), None),
        10,
        1 << 20,
    );
    assert_eq!(keys(&rows), ["b"]);
}

#[test]
fn a_to_only_window_keeps_days_up_to_that_one() {
    let (store, owner) = store_with_user();
    put(
        &store,
        owner,
        HEART,
        &[
            point(
                "a",
                Some(ms("2026-03-01T12:00:00Z")),
                Some("2026-03-01"),
                "{}",
            ),
            point(
                "b",
                Some(ms("2026-03-10T12:00:00Z")),
                Some("2026-03-10"),
                "{}",
            ),
        ],
    );
    let (rows, _) = walk(
        &store,
        owner,
        HEART,
        &open(None, Some("2026-03-05")),
        10,
        1 << 20,
    );
    assert_eq!(keys(&rows), ["a"]);
}

#[test]
fn equal_from_and_to_is_exactly_one_day() {
    let (store, owner) = store_with_user();
    put(
        &store,
        owner,
        HEART,
        &[
            point(
                "before",
                Some(ms("2026-03-09T23:00:00Z")),
                Some("2026-03-09"),
                "{}",
            ),
            point(
                "day",
                Some(ms("2026-03-10T00:00:00Z")),
                Some("2026-03-10"),
                "{}",
            ),
            point(
                "day-late",
                Some(ms("2026-03-10T23:59:59Z")),
                Some("2026-03-10"),
                "{}",
            ),
            point(
                "after",
                Some(ms("2026-03-11T00:00:00Z")),
                Some("2026-03-11"),
                "{}",
            ),
        ],
    );
    let window = open(Some("2026-03-10"), Some("2026-03-10"));
    let (rows, _) = walk(&store, owner, HEART, &window, 10, 1 << 20);
    assert_eq!(keys(&rows), ["day", "day-late"]);
}

#[test]
fn the_start_band_is_inclusive_at_from_and_exclusive_after_to_in_utc() {
    let (store, owner) = store_with_user();
    // from 2026-03-10: the band starts two days early, at 2026-03-08 00:00Z.
    // A point exactly there is inside; one millisecond earlier is outside
    // the band and so is not read, whatever its civil date says.
    put(
        &store,
        owner,
        HEART,
        &[
            point(
                "band-edge",
                Some(midnight("2026-03-08", &utc())),
                Some("2026-03-10"),
                "{}",
            ),
            point(
                "just-out",
                Some(midnight("2026-03-08", &utc()) - 1),
                Some("2026-03-10"),
                "{}",
            ),
            // to 2026-03-10: the band ends before 2026-03-13 00:00Z.
            point(
                "last-ms",
                Some(midnight("2026-03-13", &utc()) - 1),
                Some("2026-03-10"),
                "{}",
            ),
            point(
                "at-end",
                Some(midnight("2026-03-13", &utc())),
                Some("2026-03-10"),
                "{}",
            ),
        ],
    );
    let (rows, _) = walk(
        &store,
        owner,
        HEART,
        &open(Some("2026-03-10"), Some("2026-03-10")),
        10,
        1 << 20,
    );
    assert_eq!(keys(&rows), ["band-edge", "last-ms"]);
}

#[test]
fn the_start_band_follows_daylight_saving_in_the_owners_zone() {
    let zone = TimeZone::get("America/New_York").unwrap();
    let (store, owner) = store_with_user();
    // 2026-03-08 is the day clocks spring forward in New York, so the band
    // edges are not 24 hours apart.
    let lo = midnight("2026-03-06", &zone);
    let hi = midnight("2026-03-11", &zone);
    assert_eq!(hi - lo, 119 * 3_600_000, "five days, one of them 23 hours");
    put(
        &store,
        owner,
        HEART,
        &[
            point("lo", Some(lo), Some("2026-03-08"), "{}"),
            point("lo-1", Some(lo - 1), Some("2026-03-08"), "{}"),
            point("hi-1", Some(hi - 1), Some("2026-03-08"), "{}"),
            point("hi", Some(hi), Some("2026-03-08"), "{}"),
        ],
    );
    let window =
        HealthWindow::new(Some(date("2026-03-08")), Some(date("2026-03-08")), &zone).unwrap();
    let (rows, _) = walk(&store, owner, HEART, &window, 10, 1 << 20);
    assert_eq!(keys(&rows), ["lo", "hi-1"]);
}

#[test]
fn extreme_dates_do_not_panic() {
    // The result may be an error saying the date is out of range; it must
    // not be a panic.
    for d in [
        jiff::civil::date(-9999, 1, 1),
        jiff::civil::date(9999, 12, 31),
    ] {
        let _ = HealthWindow::new(Some(d), None, &utc());
        let _ = HealthWindow::new(None, Some(d), &utc());
        let _ = HealthWindow::new(Some(d), Some(d), &utc());
    }
    let err = HealthWindow::new(None, Some(date("9999-12-31")), &utc());
    if let Err(e) = err {
        assert!(e.to_string().contains("out of range"), "{e}");
    }
}

// ---------------------------------------------------------------- paging

/// Placed points with ties on `start_ms`, then unplaced ones, inserted out
/// of order so that `id` and `start_ms` disagree.
fn mixed_rows() -> Vec<PointRow> {
    let t = ms("2026-03-10T08:00:00Z");
    vec![
        point("p-late", Some(t + 5_000), Some("2026-03-10"), "{\"n\":1}"),
        point("u-first", None, Some("2026-03-10"), "{\"n\":2}"),
        point("p-tie-a", Some(t), Some("2026-03-10"), "{\"n\":3}"),
        point("p-tie-b", Some(t), Some("2026-03-10"), "{\"n\":4}"),
        point("u-second", None, Some("2026-03-11"), "{\"n\":5}"),
        point("p-early", Some(t - 5_000), Some("2026-03-09"), "{\"n\":6}"),
        point("p-tie-c", Some(t), Some("2026-03-10"), "{\"n\":7}"),
        point("u-third", None, None, "{\"n\":8}"),
    ]
}

#[test]
fn paging_reads_every_point_once_in_start_then_id_order_at_any_limit() {
    let (store, owner) = store_with_user();
    put(&store, owner, HEART, &mixed_rows());
    let expected = [
        "p-early", "p-tie-a", "p-tie-b", "p-tie-c", "p-late", "u-first", "u-second", "u-third",
    ];
    let window = open(None, None);
    for limit in [1, 4, 12] {
        let (rows, pages) = walk(&store, owner, HEART, &window, limit, 1 << 20);
        assert_eq!(keys(&rows), expected, "limit {limit}");
        assert!(
            pages >= 1 && pages <= expected.len() + 1,
            "limit {limit}: {pages} pages"
        );
    }
}

#[test]
fn a_cursor_on_a_point_without_a_start_continues_with_the_next_unplaced_one() {
    let (store, owner) = store_with_user();
    put(&store, owner, HEART, &mixed_rows());
    let window = open(None, None);
    let first = store
        .health_points_page(
            owner,
            &PageQuery {
                data_type: HEART,
                window: &window,
                after: None,
                limit: 6,
                max_bytes: 1 << 20,
            },
        )
        .unwrap();
    assert_eq!(keys(&first.rows)[5], "u-first");
    assert!(first.more);
    let last = first.rows.last().unwrap();
    assert_eq!(last.start_ms, None);
    let next = store
        .health_points_page(
            owner,
            &PageQuery {
                data_type: HEART,
                window: &window,
                after: Some(After::Unplaced { id: last.id }),
                limit: 6,
                max_bytes: 1 << 20,
            },
        )
        .unwrap();
    assert_eq!(keys(&next.rows), ["u-second", "u-third"]);
    assert!(!next.more);
}

#[test]
fn unplaced_points_match_a_window_by_civil_date_only() {
    let (store, owner) = store_with_user();
    put(
        &store,
        owner,
        HEART,
        &[
            point("in", None, Some("2026-03-10"), "{}"),
            point("out", None, Some("2026-03-20"), "{}"),
            point("no-date", None, None, "{}"),
        ],
    );
    let (rows, _) = walk(
        &store,
        owner,
        HEART,
        &open(Some("2026-03-01"), Some("2026-03-15")),
        10,
        1 << 20,
    );
    assert_eq!(keys(&rows), ["in"]);
}

#[test]
fn a_point_without_a_civil_date_appears_only_when_no_dates_are_asked_for() {
    let (store, owner) = store_with_user();
    put(
        &store,
        owner,
        HEART,
        &[
            point(
                "placed-no-date",
                Some(ms("2026-03-10T08:00:00Z")),
                None,
                "{}",
            ),
            point("unplaced-no-date", None, None, "{}"),
        ],
    );
    let (all, _) = walk(&store, owner, HEART, &open(None, None), 10, 1 << 20);
    assert_eq!(keys(&all), ["placed-no-date", "unplaced-no-date"]);
    let (windowed, _) = walk(
        &store,
        owner,
        HEART,
        &open(Some("2026-03-01"), None),
        10,
        1 << 20,
    );
    assert!(windowed.is_empty(), "{:?}", keys(&windowed));
}

#[test]
fn more_says_whether_a_page_is_the_last_exactly() {
    let (store, owner) = store_with_user();
    put(&store, owner, HEART, &mixed_rows()); // eight points
    let window = open(None, None);
    let page = |limit: usize| {
        store
            .health_points_page(
                owner,
                &PageQuery {
                    data_type: HEART,
                    window: &window,
                    after: None,
                    limit,
                    max_bytes: 1 << 20,
                },
            )
            .unwrap()
    };
    let exact = page(8);
    assert_eq!(exact.rows.len(), 8);
    assert!(!exact.more, "all eight fit exactly");
    let short = page(7);
    assert_eq!(short.rows.len(), 7);
    assert!(short.more, "the eighth is there");
    assert!(page(100).rows.len() == 8 && !page(100).more);
}

#[test]
fn the_byte_limit_keeps_the_point_that_crosses_it_and_at_least_one_point() {
    let (store, owner) = store_with_user();
    let big = format!("{{\"pad\":\"{}\"}}", "x".repeat(1000));
    put(
        &store,
        owner,
        HEART,
        &[
            point(
                "a",
                Some(ms("2026-03-10T08:00:00Z")),
                Some("2026-03-10"),
                &big,
            ),
            point(
                "b",
                Some(ms("2026-03-10T08:01:00Z")),
                Some("2026-03-10"),
                &big,
            ),
            point(
                "c",
                Some(ms("2026-03-10T08:02:00Z")),
                Some("2026-03-10"),
                &big,
            ),
        ],
    );
    let window = open(None, None);
    // 1500 bytes: the first point (about 1010) fits, the second crosses it
    // and is kept, the third is not read.
    let crossing = store
        .health_points_page(
            owner,
            &PageQuery {
                data_type: HEART,
                window: &window,
                after: None,
                limit: 10,
                max_bytes: 1500,
            },
        )
        .unwrap();
    assert_eq!(keys(&crossing.rows), ["a", "b"]);
    assert!(crossing.more, "the third point is left for the next page");

    // A limit of one byte: the first point is still returned.
    let tiny = store
        .health_points_page(
            owner,
            &PageQuery {
                data_type: HEART,
                window: &window,
                after: None,
                limit: 10,
                max_bytes: 1,
            },
        )
        .unwrap();
    assert_eq!(keys(&tiny.rows), ["a"]);
    assert!(tiny.more);

    // Walking with that budget still reads every point once.
    let (rows, _) = walk(&store, owner, HEART, &window, 10, 1500);
    assert_eq!(keys(&rows), ["a", "b", "c"]);
}

#[test]
fn points_are_isolated_by_owner_and_by_type() {
    let store = Store::open_in_memory().unwrap();
    let mine = store.user("telegram", "1").unwrap().id();
    let theirs = store.user("telegram", "2").unwrap().id();
    put(
        &store,
        mine,
        HEART,
        &[point(
            "mine",
            Some(ms("2026-03-10T08:00:00Z")),
            Some("2026-03-10"),
            "{}",
        )],
    );
    put(
        &store,
        theirs,
        HEART,
        &[point(
            "theirs",
            Some(ms("2026-03-10T08:00:00Z")),
            Some("2026-03-10"),
            "{}",
        )],
    );
    put(
        &store,
        mine,
        STEPS,
        &[point(
            "steps",
            Some(ms("2026-03-10T08:00:00Z")),
            Some("2026-03-10"),
            "{}",
        )],
    );
    let window = open(None, None);
    let (rows, _) = walk(&store, mine, HEART, &window, 10, 1 << 20);
    assert_eq!(keys(&rows), ["mine"]);
    let (rows, _) = walk(&store, theirs, HEART, &window, 10, 1 << 20);
    assert_eq!(keys(&rows), ["theirs"]);
    let (rows, _) = walk(&store, mine, "weight", &window, 10, 1 << 20);
    assert!(rows.is_empty());
    assert!(
        store
            .health_points_page(
                mine,
                &PageQuery {
                    data_type: HEART,
                    window: &window,
                    after: None,
                    limit: 10,
                    max_bytes: 1 << 20
                }
            )
            .unwrap()
            .rows
            .iter()
            .all(|r| r.key == "mine")
    );
}

#[test]
fn an_upsert_between_pages_that_moves_a_point_does_not_break_paging() {
    let (store, owner) = store_with_user();
    put(
        &store,
        owner,
        HEART,
        &[
            point(
                "a",
                Some(ms("2026-03-10T08:00:00Z")),
                Some("2026-03-10"),
                "{}",
            ),
            point(
                "b",
                Some(ms("2026-03-10T09:00:00Z")),
                Some("2026-03-10"),
                "{}",
            ),
        ],
    );
    let window = open(None, None);
    let first = store
        .health_points_page(
            owner,
            &PageQuery {
                data_type: HEART,
                window: &window,
                after: None,
                limit: 1,
                max_bytes: 1 << 20,
            },
        )
        .unwrap();
    assert_eq!(keys(&first.rows), ["a"]);
    // The sync rewrites `a` with a new start and civil date after the first
    // page, and adds a point before it.
    put(
        &store,
        owner,
        HEART,
        &[
            point(
                "a",
                Some(ms("2026-03-12T08:00:00Z")),
                Some("2026-03-12"),
                "{\"v\":2}",
            ),
            point(
                "z",
                Some(ms("2026-03-01T08:00:00Z")),
                Some("2026-03-01"),
                "{}",
            ),
        ],
    );
    let last = first.rows.last().unwrap();
    let next = store
        .health_points_page(
            owner,
            &PageQuery {
                data_type: HEART,
                window: &window,
                after: Some(After::Start {
                    start_ms: last.start_ms.unwrap(),
                    id: last.id,
                }),
                limit: 10,
                max_bytes: 1 << 20,
            },
        )
        .unwrap();
    // Nothing is repeated from before the cursor, and the moved point is
    // read at its new place.
    assert_eq!(keys(&next.rows), ["b", "a"]);
}

// ----------------------------------------------------------------- sizes

#[test]
fn a_type_with_no_points_has_a_default_size() {
    let (store, owner) = store_with_user();
    let size = store
        .health_type_size(owner, HEART, &open(None, None))
        .unwrap();
    assert_eq!(size, TypeSize::default());
    assert_eq!(size.rows, 0);
    assert!(size.first.is_none() && size.last.is_none());
}

#[test]
fn a_size_counts_placed_and_unplaced_points_and_their_edges() {
    let (store, owner) = store_with_user();
    put(&store, owner, HEART, &mixed_rows());
    let size = store
        .health_type_size(owner, HEART, &open(None, None))
        .unwrap();
    assert_eq!(size.rows, 8);
    // first and last come from placed points only, with their civil dates.
    assert_eq!(
        size.first,
        Some((ms("2026-03-10T07:59:55Z"), Some("2026-03-09".into())))
    );
    assert_eq!(
        size.last,
        Some((ms("2026-03-10T08:00:05Z"), Some("2026-03-10".into())))
    );
}

#[test]
fn a_size_narrows_with_the_window() {
    let (store, owner) = store_with_user();
    put(&store, owner, HEART, &mixed_rows());
    let size = store
        .health_type_size(owner, HEART, &open(Some("2026-03-10"), Some("2026-03-10")))
        .unwrap();
    // Placed on the 10th: p-late, p-tie-a, b, c; unplaced on the 10th: u-first.
    assert_eq!(size.rows, 5);
    assert_eq!(
        size.first.as_ref().map(|f| f.1.clone()),
        Some(Some("2026-03-10".into()))
    );
}

#[test]
fn only_unplaced_points_have_no_edges_but_still_count() {
    let (store, owner) = store_with_user();
    put(
        &store,
        owner,
        HEART,
        &[
            point("u1", None, Some("2026-03-10"), &"x".repeat(40)),
            point("u2", None, None, &"x".repeat(60)),
        ],
    );
    let size = store
        .health_type_size(owner, HEART, &open(None, None))
        .unwrap();
    assert_eq!(size.rows, 2);
    assert!(size.first.is_none() && size.last.is_none());
    assert_eq!(size.avg_value_bytes, 50);
}

#[test]
fn the_average_size_comes_from_a_bounded_sample() {
    let (store, owner) = store_with_user();
    // The first 500 points by start are ten bytes; the 100 after are a
    // thousand. A sample of 500 gives 10, a read of every value gives more.
    let small = format!("\"{}\"", "s".repeat(8));
    let big = format!("\"{}\"", "b".repeat(998));
    let mut rows = Vec::new();
    for i in 0..600 {
        let start = ms("2026-01-01T00:00:00Z") + i * 60_000;
        let value = if i < 500 { &small } else { &big };
        rows.push(point(
            &format!("p{i}"),
            Some(start),
            Some("2026-01-01"),
            value,
        ));
    }
    put(&store, owner, HEART, &rows);
    let size = store
        .health_type_size(owner, HEART, &open(None, None))
        .unwrap();
    assert_eq!(size.rows, 600);
    assert_eq!(size.avg_value_bytes, 10);
}

#[test]
fn a_database_error_reaches_the_caller_as_an_error() {
    let path = std::env::temp_dir().join(format!("athena-size-{}.db", uuid::Uuid::new_v4()));
    let store = Store::open(path.to_str().unwrap()).unwrap();
    let owner = store.user("telegram", "1").unwrap().id();
    // The table is renamed under the store's own connection: the next query
    // fails, and the size must say so rather than report zero points.
    rusqlite::Connection::open(&path)
        .unwrap()
        .execute_batch("ALTER TABLE health_points RENAME TO gone")
        .unwrap();
    let err = store
        .health_type_size(owner, HEART, &open(None, None))
        .unwrap_err();
    assert!(err.to_string().contains("no such table"), "{err}");
    let window = open(None, None);
    let err = store
        .health_export_batch(
            owner,
            &BatchQuery {
                data_types: &[],
                window: &window,
                after_id: 0,
                max_rows: 10,
                max_bytes: 1 << 20,
            },
        )
        .unwrap_err();
    assert!(err.to_string().contains("no such table"), "{err}");
    drop(store);
    for suffix in ["", "-wal", "-shm"] {
        let _ = std::fs::remove_file(format!("{}{suffix}", path.display()));
    }
}

#[test]
fn a_size_whose_sample_cannot_be_read_is_an_error() {
    let path = std::env::temp_dir().join(format!("athena-avg-{}.db", uuid::Uuid::new_v4()));
    let store = Store::open(path.to_str().unwrap()).unwrap();
    let owner = store.user("telegram", "1").unwrap().id();
    put(&store, owner, HEART, &mixed_rows());
    // The count and the first and last points do not read `value`; only the
    // sample of values does, so only the average fails.
    rusqlite::Connection::open(&path)
        .unwrap()
        .execute_batch("ALTER TABLE health_points RENAME COLUMN value TO renamed")
        .unwrap();
    let err = store
        .health_type_size(owner, HEART, &open(None, None))
        .unwrap_err();
    assert!(err.to_string().contains("no such column"), "{err}");
    drop(store);
    for suffix in ["", "-wal", "-shm"] {
        let _ = std::fs::remove_file(format!("{}{suffix}", path.display()));
    }
}

#[test]
fn a_size_is_isolated_by_owner() {
    let store = Store::open_in_memory().unwrap();
    let mine = store.user("telegram", "1").unwrap().id();
    let theirs = store.user("telegram", "2").unwrap().id();
    put(&store, theirs, HEART, &mixed_rows());
    assert_eq!(
        store
            .health_type_size(mine, HEART, &open(None, None))
            .unwrap()
            .rows,
        0
    );
    assert_eq!(
        store
            .health_type_size(theirs, HEART, &open(None, None))
            .unwrap()
            .rows,
        8
    );
}

// ---------------------------------------------------------- export batches

fn batch(
    store: &Store,
    owner: i64,
    types: &[&str],
    window: &HealthWindow,
    after_id: i64,
    max_rows: usize,
    max_bytes: usize,
) -> Vec<PointRecord> {
    store
        .health_export_batch(
            owner,
            &BatchQuery {
                data_types: types,
                window,
                after_id,
                max_rows,
                max_bytes,
            },
        )
        .unwrap()
}

#[test]
fn an_export_batch_starts_after_an_id_in_id_order_and_respects_max_rows() {
    let (store, owner) = store_with_user();
    put(&store, owner, HEART, &mixed_rows());
    let window = open(None, None);
    let first = batch(&store, owner, &[], &window, 0, 3, 1 << 20);
    assert_eq!(keys(&first), ["p-late", "u-first", "p-tie-a"]);
    let ids: Vec<i64> = first.iter().map(|r| r.id).collect();
    assert!(ids.windows(2).all(|w| w[0] < w[1]));
    let next = batch(
        &store,
        owner,
        &[],
        &window,
        *ids.last().unwrap(),
        3,
        1 << 20,
    );
    assert_eq!(keys(&next), ["p-tie-b", "u-second", "p-early"]);
    assert!(batch(&store, owner, &[], &window, 1_000_000, 3, 1 << 20).is_empty());
}

#[test]
fn an_export_batch_stops_after_the_point_that_crosses_max_bytes() {
    let (store, owner) = store_with_user();
    let value = format!("\"{}\"", "v".repeat(100));
    let rows: Vec<PointRow> = (0..5)
        .map(|i| {
            point(
                &format!("k{i}"),
                Some(ms("2026-03-10T00:00:00Z") + i * 60_000),
                Some("2026-03-10"),
                &value,
            )
        })
        .collect();
    put(&store, owner, HEART, &rows);
    let window = open(None, None);
    // Each value is 102 bytes: a budget of 150 is crossed by the second.
    assert_eq!(
        keys(&batch(&store, owner, &[], &window, 0, 100, 150)),
        ["k0", "k1"]
    );
    // A budget of nothing still returns one point.
    assert_eq!(keys(&batch(&store, owner, &[], &window, 0, 100, 0)), ["k0"]);
}

#[test]
fn an_export_batch_filters_by_data_type() {
    let (store, owner) = store_with_user();
    put(
        &store,
        owner,
        HEART,
        &[point(
            "h",
            Some(ms("2026-03-10T08:00:00Z")),
            Some("2026-03-10"),
            "{}",
        )],
    );
    put(
        &store,
        owner,
        STEPS,
        &[point(
            "s",
            Some(ms("2026-03-10T08:00:00Z")),
            Some("2026-03-10"),
            "{}",
        )],
    );
    put(
        &store,
        owner,
        "weight",
        &[point("w", None, Some("2026-03-10"), "{}")],
    );
    let window = open(None, None);
    assert_eq!(
        keys(&batch(&store, owner, &[], &window, 0, 100, 1 << 20)),
        ["h", "s", "w"]
    );
    assert_eq!(
        keys(&batch(&store, owner, &[STEPS], &window, 0, 100, 1 << 20)),
        ["s"]
    );
    assert_eq!(
        keys(&batch(
            &store,
            owner,
            &["weight", HEART],
            &window,
            0,
            100,
            1 << 20
        )),
        ["h", "w"]
    );
    assert!(batch(&store, owner, &["sleep"], &window, 0, 100, 1 << 20).is_empty());
}

#[test]
fn an_export_batch_in_a_window_keeps_unplaced_points_by_civil_date() {
    let (store, owner) = store_with_user();
    put(
        &store,
        owner,
        HEART,
        &[
            point(
                "placed-in",
                Some(ms("2026-03-10T08:00:00Z")),
                Some("2026-03-10"),
                "{}",
            ),
            point(
                "placed-out",
                Some(ms("2026-04-10T08:00:00Z")),
                Some("2026-04-10"),
                "{}",
            ),
            point("unplaced-in", None, Some("2026-03-10"), "{}"),
            point("unplaced-out", None, Some("2026-04-10"), "{}"),
            point("unplaced-none", None, None, "{}"),
        ],
    );
    let window = open(Some("2026-03-10"), Some("2026-03-10"));
    assert_eq!(
        keys(&batch(&store, owner, &[], &window, 0, 100, 1 << 20)),
        ["placed-in", "unplaced-in"]
    );
}

#[test]
fn an_export_batch_is_isolated_by_owner() {
    let store = Store::open_in_memory().unwrap();
    let mine = store.user("telegram", "1").unwrap().id();
    let theirs = store.user("telegram", "2").unwrap().id();
    put(&store, theirs, HEART, &mixed_rows());
    assert!(batch(&store, mine, &[], &open(None, None), 0, 100, 1 << 20).is_empty());
}

#[test]
fn walking_export_batches_returns_every_point_exactly_once() {
    let (store, owner) = store_with_user();
    let rows: Vec<PointRow> = (0..2500)
        .map(|i| {
            let placed = i % 3 != 0;
            point(
                &format!("k{i}"),
                placed.then(|| ms("2026-03-10T00:00:00Z") + i * 1000),
                Some("2026-03-10"),
                "{\"n\":1}",
            )
        })
        .collect();
    put(&store, owner, HEART, &rows);
    let window = open(None, None);
    let mut seen = Vec::new();
    let mut after_id = 0;
    loop {
        let batch_rows = batch(&store, owner, &[], &window, after_id, 1000, 1 << 20);
        let Some(last) = batch_rows.last() else { break };
        after_id = last.id;
        seen.extend(batch_rows.into_iter().map(|r| r.key));
    }
    assert_eq!(seen.len(), 2500);
    let mut unique = seen.clone();
    unique.sort();
    unique.dedup();
    assert_eq!(unique.len(), 2500);
}
