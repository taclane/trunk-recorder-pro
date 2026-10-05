//! The radio database: everything the registry hears, kept for good, in
//! SQLite (`<data>/radio.db`). Systems are keyed by what their control
//! channels say ("p25:<wacn>.<sysid>", or the config's site group), so the
//! sites of one system share their radios, talkgroups, links and aliases;
//! the site is where something was heard. The dashboard's `radioQuery`
//! reads it, for any span, for one site (by its short name) or the whole
//! system (by its key).
//!
//! The session hands records over without waiting ([`RadioSink`]); a writer
//! thread applies them, a transaction every couple of seconds. A call heard
//! on several sites of a system is one call here: its copies (same
//! talkgroup, begun within [`TWIN_S`] of each other) are held until all
//! have ended and kept as one. Hourly rows are kept per site and, as site 0,
//! for the system (each call once).

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, SyncSender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use rusqlite::{params, Connection, OpenFlags, OptionalExtension};
use serde_json::{json, Value};
use trunk_app::stats::{Heard, Names, RadioRecord, RadioSink, UnitEvent};

const SCHEMA: &str = "1";
/// Copies of a call on other sites begin within this long of each other (the engine's twin window).
const TWIN_S: i64 = 3;
/// A call is kept once it ended this long ago (its copies have ended too).
const HOLD_S: i64 = 8;
/// A site whose system isn't known after this long is kept as its own ("cfg:<short name>").
const UNRESOLVED_S: i64 = 60;
const BATCH: Duration = Duration::from_secs(2);
const QUEUE: usize = 50_000;
const BACKUP_EVERY: Duration = Duration::from_secs(24 * 3600);
/// Encrypted: at least this share of calls; mixed: some.
const ENC_ALL: f64 = 0.95;
/// "New": first heard within this long, once the system has been heard for [`BASELINE_S`].
const NEW_S: i64 = 24 * 3600;
const BASELINE_S: i64 = 3600;
/// A radio's talkgroups in list rows.
const TOP_TGS: usize = 8;
/// Talkgroup rows' activity: one value per hour up to a week, else this many buckets.
const MAX_POINTS: i64 = 168;
const LENGTH_BINS: [f32; 9] = [0.0, 1.0, 2.0, 4.0, 8.0, 16.0, 32.0, 64.0, 128.0];

pub struct RadioDb {
    tx: SyncSender<RadioRecord>,
    read: Mutex<Connection>,
    dropped: AtomicU64,
}

impl RadioSink for RadioDb {
    fn record(&self, r: RadioRecord) {
        if self.tx.try_send(r).is_err() {
            self.dropped.fetch_add(1, Ordering::Relaxed);
        }
    }
}

impl RadioDb {
    /// Open (or make) the database at `path` and start its writer.
    pub fn open(path: &Path) -> Result<Arc<RadioDb>, String> {
        let conn = open_rw(path)?;
        let writer = Writer::new(conn).map_err(|e| e.to_string())?;
        let read = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX).map_err(|e| e.to_string())?;
        let (tx, rx) = mpsc::sync_channel(QUEUE);
        let backup = path.with_extension("db.bak");
        std::thread::Builder::new().name("radio-db".into()).spawn(move || run(writer, rx, backup)).map_err(|e| e.to_string())?;
        Ok(Arc::new(RadioDb { tx, read: Mutex::new(read), dropped: AtomicU64::new(0) }))
    }

    /// `radioQuery` (see [`query`]).
    pub fn query(&self, q: &Value, now: i64, names: &Names) -> Value {
        let conn = self.read.lock().unwrap();
        let mut out = query(&conn, q, now, names).unwrap_or_else(|e| json!({ "error": e.to_string() }));
        out["dropped"] = json!(self.dropped.load(Ordering::Relaxed));
        out
    }
}

fn open_rw(path: &Path) -> Result<Connection, String> {
    let conn = Connection::open(path).map_err(|e| format!("{}: {e}", path.display()))?;
    conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL; PRAGMA foreign_keys=OFF;").map_err(|e| e.to_string())?;
    init(&conn).map_err(|e| format!("{}: {e}", path.display()))?;
    Ok(conn)
}

fn init(conn: &Connection) -> Result<(), String> {
    let tables: i64 = conn.query_row("SELECT count(*) FROM sqlite_master WHERE type = 'table'", [], |r| r.get(0)).map_err(|e| e.to_string())?;
    if tables > 0 {
        let v: Option<String> = conn.query_row("SELECT value FROM meta WHERE key = 'schema'", [], |r| r.get(0)).optional().unwrap_or(None);
        return match v.as_deref() {
            Some(SCHEMA) => Ok(()),
            Some(v) => Err(format!("radio database schema {v}, this version reads {SCHEMA}")),
            None => Err("not a radio database".into()),
        };
    }
    conn.execute_batch(&format!(
        "BEGIN;
         CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT);
         CREATE TABLE systems (id INTEGER PRIMARY KEY, key TEXT UNIQUE NOT NULL, wacn TEXT, sysid TEXT, first INTEGER, last INTEGER);
         CREATE TABLE sites (id INTEGER PRIMARY KEY, system INTEGER NOT NULL, short_name TEXT NOT NULL, rfss INTEGER, site INTEGER,
             first INTEGER, last INTEGER, UNIQUE (system, short_name));
         CREATE TABLE units (system INTEGER, unit INTEGER, first INTEGER, last INTEGER, last_tx INTEGER,
             tx INTEGER DEFAULT 0, tx_secs REAL DEFAULT 0, enc_tx INTEGER DEFAULT 0, started INTEGER DEFAULT 0,
             state INTEGER, state_t INTEGER, aff INTEGER, aff_t INTEGER, last_site INTEGER, alias TEXT, alias_t INTEGER, home TEXT,
             PRIMARY KEY (system, unit)) WITHOUT ROWID;
         CREATE INDEX units_aff ON units (system, aff);
         CREATE TABLE talkgroups (system INTEGER, tg INTEGER, first INTEGER, last INTEGER,
             calls INTEGER DEFAULT 0, secs REAL DEFAULT 0, enc_calls INTEGER DEFAULT 0, PRIMARY KEY (system, tg)) WITHOUT ROWID;
         CREATE TABLE links (system INTEGER, unit INTEGER, tg INTEGER, first INTEGER, last INTEGER,
             started INTEGER DEFAULT 0, talked INTEGER DEFAULT 0, enc INTEGER DEFAULT 0, joins INTEGER DEFAULT 0, locations INTEGER DEFAULT 0,
             PRIMARY KEY (system, unit, tg)) WITHOUT ROWID;
         CREATE INDEX links_tg ON links (system, tg);
         CREATE TABLE pairs (system INTEGER, a INTEGER, b INTEGER, n INTEGER DEFAULT 0, enc INTEGER DEFAULT 0, last INTEGER,
             PRIMARY KEY (system, a, b)) WITHOUT ROWID;
         CREATE INDEX pairs_b ON pairs (system, b);
         CREATE TABLE calls (id INTEGER PRIMARY KEY, system INTEGER, start INTEGER, secs REAL, tg INTEGER, uu INTEGER, freq INTEGER,
             enc INTEGER, emergency INTEGER, recorded INTEGER, reason TEXT, sites TEXT, units TEXT);
         CREATE INDEX calls_start ON calls (system, start);
         CREATE INDEX calls_tg ON calls (system, tg, start);
         CREATE TABLE call_units (system INTEGER, unit INTEGER, start INTEGER, call INTEGER, tg INTEGER, uu INTEGER, secs REAL, enc INTEGER,
             PRIMARY KEY (system, unit, start, call)) WITHOUT ROWID;
         CREATE TABLE unit_events (system INTEGER, unit INTEGER, t INTEGER, kind TEXT, tg INTEGER, site INTEGER,
             PRIMARY KEY (system, unit, t, kind)) WITHOUT ROWID;
         CREATE INDEX unit_events_t ON unit_events (system, t);
         CREATE TABLE aliases (system INTEGER, unit INTEGER, alias TEXT, first INTEGER, last INTEGER, PRIMARY KEY (system, unit, alias)) WITHOUT ROWID;
         CREATE TABLE freqs (system INTEGER, site INTEGER, freq INTEGER, calls INTEGER DEFAULT 0, frames INTEGER DEFAULT 0, errors INTEGER DEFAULT 0,
             bad INTEGER DEFAULT 0, snr REAL, ferr REAL, clean REAL, last INTEGER, PRIMARY KEY (system, site, freq)) WITHOUT ROWID;
         CREATE TABLE tg_hours (system INTEGER, site INTEGER, tg INTEGER, hour INTEGER, calls INTEGER DEFAULT 0, secs REAL DEFAULT 0, enc INTEGER DEFAULT 0,
             PRIMARY KEY (system, site, hour, tg)) WITHOUT ROWID;
         CREATE INDEX tg_hours_tg ON tg_hours (system, tg, hour);
         CREATE TABLE unit_hours (system INTEGER, site INTEGER, unit INTEGER, hour INTEGER, tx INTEGER DEFAULT 0, secs REAL DEFAULT 0,
             enc INTEGER DEFAULT 0, events INTEGER DEFAULT 0, PRIMARY KEY (system, site, hour, unit)) WITHOUT ROWID;
         CREATE TABLE freq_hours (system INTEGER, site INTEGER, freq INTEGER, hour INTEGER, calls INTEGER DEFAULT 0, frames INTEGER DEFAULT 0,
             errors INTEGER DEFAULT 0, bad INTEGER DEFAULT 0, PRIMARY KEY (system, site, hour, freq)) WITHOUT ROWID;
         INSERT INTO meta VALUES ('schema', '{SCHEMA}');
         COMMIT;"
    ))
    .map_err(|e| e.to_string())
}

fn run(mut w: Writer, rx: Receiver<RadioRecord>, backup: PathBuf) {
    let mut batch = Vec::new();
    let mut flushed = Instant::now();
    // A backup a day; one already there counts as today's.
    let mut backed_up = backup.exists().then(Instant::now);
    loop {
        match rx.recv_timeout(BATCH) {
            Ok(r) => batch.push(r),
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
        }
        if flushed.elapsed() >= BATCH {
            flushed = Instant::now();
            if let Err(e) = w.apply(std::mem::take(&mut batch), unix_now()) {
                log::warn!("Radio database: {e}");
            }
            if backed_up.is_none_or(|t| t.elapsed() >= BACKUP_EVERY) {
                backed_up = Some(Instant::now());
                if let Err(e) = w.backup(&backup) {
                    log::warn!("Radio database backup: {e}");
                }
            }
        }
    }
    let _ = w.apply(batch, i64::MAX);
}

fn unix_now() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs() as i64)
}

/// A call (and its copies on other sites) until all have ended.
#[derive(Debug)]
struct Pending {
    system: i64,
    tg: u32,
    uu: bool,
    start: i64,
    end: i64,
    secs: f32,
    freq: u64,
    enc: bool,
    emergency: bool,
    recorded: bool,
    reason: String,
    units: Vec<(u32, f32)>,
    sites: Vec<i64>,
}

pub(crate) struct Writer {
    conn: Connection,
    systems: HashMap<String, i64>,
    sites: HashMap<(i64, String), i64>,
    /// Each short name's system key, as last heard.
    resolved: HashMap<String, String>,
    /// Records from sites whose system isn't known yet.
    waiting: Vec<RadioRecord>,
    pending: Vec<Pending>,
}

fn at(r: &RadioRecord) -> &Heard {
    match r {
        RadioRecord::Call { at, .. } | RadioRecord::Unit { at, .. } | RadioRecord::Voice { at, .. } | RadioRecord::Alias { at, .. } => at,
    }
}

fn hour(t: i64) -> i64 {
    t.div_euclid(3600)
}

impl Writer {
    pub(crate) fn new(conn: Connection) -> rusqlite::Result<Writer> {
        let mut w = Writer { conn, systems: HashMap::new(), sites: HashMap::new(), resolved: HashMap::new(), waiting: Vec::new(), pending: Vec::new() };
        {
            let mut st = w.conn.prepare("SELECT id, key FROM systems")?;
            for r in st.query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)))? {
                let (id, key) = r?;
                w.systems.insert(key, id);
            }
            let mut st = w.conn.prepare("SELECT sites.id, sites.system, sites.short_name, systems.key FROM sites JOIN systems ON systems.id = sites.system ORDER BY sites.last")?;
            for r in st.query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?, r.get::<_, String>(2)?, r.get::<_, String>(3)?)))? {
                let (id, sys, short, key) = r?;
                w.sites.insert((sys, short.clone()), id);
                w.resolved.insert(short, key);
            }
        }
        Ok(w)
    }

    fn backup(&self, to: &Path) -> rusqlite::Result<()> {
        let tmp = to.with_extension("tmp");
        let _ = std::fs::remove_file(&tmp);
        self.conn.execute("VACUUM INTO ?1", [tmp.to_string_lossy()])?;
        std::fs::rename(&tmp, to).map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)))
    }

    /// Apply `recs` at `now` (Unix s): calls that ended long enough ago are kept.
    pub(crate) fn apply(&mut self, recs: Vec<RadioRecord>, now: i64) -> rusqlite::Result<()> {
        self.conn.execute_batch("BEGIN")?;
        let r = self.apply_in(recs, now);
        self.conn.execute_batch(if r.is_ok() { "COMMIT" } else { "ROLLBACK" })?;
        r
    }

    fn apply_in(&mut self, recs: Vec<RadioRecord>, now: i64) -> rusqlite::Result<()> {
        let mut todo = std::mem::take(&mut self.waiting);
        todo.extend(recs);
        // What the records say of their sites' systems, before any is placed.
        for r in &todo {
            let a = at(r);
            if let Some(k) = &a.system {
                self.resolved.insert(a.short_name.clone(), k.clone());
            }
        }
        for r in todo {
            let a = at(&r);
            let key = match a.system.clone().or_else(|| self.resolved.get(&a.short_name).cloned()) {
                Some(k) => k,
                None if a.t < now - UNRESOLVED_S => format!("cfg:{}", a.short_name),
                None => {
                    self.waiting.push(r);
                    continue;
                }
            };
            let (sys, site) = self.place(&key, a)?;
            self.one(r, sys, site)?;
        }
        let (done, keep): (Vec<Pending>, Vec<Pending>) = std::mem::take(&mut self.pending).into_iter().partition(|p| p.end <= now - HOLD_S);
        self.pending = keep;
        for p in done {
            self.keep_call(p)?;
        }
        Ok(())
    }

    /// The system and site ids of a record (made when new).
    fn place(&mut self, key: &str, a: &Heard) -> rusqlite::Result<(i64, i64)> {
        let sys = match self.systems.get(key) {
            Some(&id) => id,
            None => {
                let (wacn, sysid) = key.strip_prefix("p25:").and_then(|k| k.split_once('.')).map_or((None, None), |(w, s)| (Some(w.to_string()), Some(s.to_string())));
                self.conn.execute("INSERT INTO systems (key, wacn, sysid, first, last) VALUES (?1, ?2, ?3, ?4, ?4)", params![key, wacn, sysid, a.t])?;
                let id = self.conn.last_insert_rowid();
                self.systems.insert(key.to_string(), id);
                id
            }
        };
        let site = match self.sites.get(&(sys, a.short_name.clone())) {
            Some(&id) => id,
            None => {
                self.conn.execute("INSERT INTO sites (system, short_name, first, last) VALUES (?1, ?2, ?3, ?3)", params![sys, a.short_name, a.t])?;
                let id = self.conn.last_insert_rowid();
                self.sites.insert((sys, a.short_name.clone()), id);
                id
            }
        };
        let (rfss, num) = a.site.map_or((None, None), |(r, s)| (Some(r), Some(s)));
        self.conn.execute(
            "UPDATE sites SET last = max(last, ?2), rfss = coalesce(?3, rfss), site = coalesce(?4, site) WHERE id = ?1",
            params![site, a.t, rfss, num],
        )?;
        self.conn.execute("UPDATE systems SET last = max(last, ?2) WHERE id = ?1", params![sys, a.t])?;
        Ok((sys, site))
    }

    fn one(&mut self, r: RadioRecord, sys: i64, site: i64) -> rusqlite::Result<()> {
        match r {
            RadioRecord::Call { at, tg, unit_to_unit, freq_hz, start, secs, encrypted, emergency, recorded, reason, units } => {
                let h = hour(at.t);
                self.hours(sys, site, h, tg, unit_to_unit, secs, encrypted, &units)?;
                let twin = self.pending.iter_mut().find(|p| p.system == sys && p.tg == tg && p.uu == unit_to_unit && (p.start - start).abs() <= TWIN_S && !p.sites.contains(&site));
                match twin {
                    Some(p) => {
                        p.end = p.end.max(at.t);
                        p.secs = p.secs.max(secs);
                        p.enc |= encrypted;
                        p.emergency |= emergency;
                        if recorded && !p.recorded {
                            p.recorded = true;
                            p.reason.clear();
                            p.freq = freq_hz;
                        }
                        for (u, off) in units {
                            match p.units.iter_mut().find(|x| x.0 == u) {
                                Some(x) => x.1 = x.1.min(off),
                                None => p.units.push((u, off)),
                            }
                        }
                        p.units.sort_by(|a, b| a.1.total_cmp(&b.1));
                        p.sites.push(site);
                    }
                    None => self.pending.push(Pending {
                        system: sys,
                        tg,
                        uu: unit_to_unit,
                        start,
                        end: at.t,
                        secs,
                        freq: freq_hz,
                        enc: encrypted,
                        emergency,
                        recorded,
                        reason,
                        units,
                        sites: vec![site],
                    }),
                }
            }
            RadioRecord::Unit { at, kind, unit, tg } => self.unit_event(sys, site, at.t, kind, unit, tg)?,
            RadioRecord::Voice { at, freq_hz, frames, errors, bad, snr, freq_error, clean } => {
                self.conn.execute(
                    "INSERT INTO freqs (system, site, freq, calls, frames, errors, bad, snr, ferr, clean, last) VALUES (?1, ?2, ?3, 1, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
                     ON CONFLICT DO UPDATE SET calls = calls + 1, frames = frames + excluded.frames, errors = errors + excluded.errors, bad = bad + excluded.bad,
                       snr = CASE WHEN excluded.snr IS NULL THEN snr WHEN snr IS NULL THEN excluded.snr ELSE snr + 0.1 * (excluded.snr - snr) END,
                       ferr = CASE WHEN excluded.ferr IS NULL THEN ferr WHEN ferr IS NULL THEN excluded.ferr ELSE ferr + 0.1 * (excluded.ferr - ferr) END,
                       clean = CASE WHEN excluded.clean IS NULL THEN clean WHEN clean IS NULL THEN excluded.clean ELSE clean + 0.1 * (excluded.clean - clean) END,
                       last = max(last, excluded.last)",
                    params![sys, site, freq_hz as i64, frames as i64, errors as i64, bad as i64, snr, freq_error, clean, at.t],
                )?;
                self.conn.execute(
                    "INSERT INTO freq_hours (system, site, freq, hour, calls, frames, errors, bad) VALUES (?1, ?2, ?3, ?4, 1, ?5, ?6, ?7)
                     ON CONFLICT DO UPDATE SET calls = calls + 1, frames = frames + excluded.frames, errors = errors + excluded.errors, bad = bad + excluded.bad",
                    params![sys, site, freq_hz as i64, hour(at.t), frames as i64, errors as i64, bad as i64],
                )?;
            }
            RadioRecord::Alias { at, unit, alias, .. } => {
                self.touch_unit(sys, unit, at.t, site)?;
                self.conn.execute("UPDATE units SET alias = ?3, alias_t = ?4 WHERE system = ?1 AND unit = ?2", params![sys, unit, alias, at.t])?;
                self.conn.execute(
                    "INSERT INTO aliases (system, unit, alias, first, last) VALUES (?1, ?2, ?3, ?4, ?4) ON CONFLICT DO UPDATE SET last = max(last, excluded.last)",
                    params![sys, unit, alias, at.t],
                )?;
            }
        }
        Ok(())
    }

    /// A call copy's hourly rows for its site.
    #[allow(clippy::too_many_arguments)]
    fn hours(&self, sys: i64, site: i64, h: i64, tg: u32, uu: bool, secs: f32, enc: bool, units: &[(u32, f32)]) -> rusqlite::Result<()> {
        if !uu {
            self.conn.execute(
                "INSERT INTO tg_hours (system, site, tg, hour, calls, secs, enc) VALUES (?1, ?2, ?3, ?4, 1, ?5, ?6)
                 ON CONFLICT DO UPDATE SET calls = calls + 1, secs = secs + excluded.secs, enc = enc + excluded.enc",
                params![sys, site, tg, h, secs as f64, enc as i64],
            )?;
        }
        for (u, turn) in turns(units, secs) {
            self.conn.execute(
                "INSERT INTO unit_hours (system, site, unit, hour, tx, secs, enc) VALUES (?1, ?2, ?3, ?4, 1, ?5, ?6)
                 ON CONFLICT DO UPDATE SET tx = tx + 1, secs = secs + excluded.secs, enc = enc + excluded.enc",
                params![sys, site, u, h, turn as f64, enc as i64],
            )?;
        }
        Ok(())
    }

    fn touch_unit(&self, sys: i64, unit: u32, t: i64, site: i64) -> rusqlite::Result<()> {
        self.conn.execute(
            "INSERT INTO units (system, unit, first, last, last_site) VALUES (?1, ?2, ?3, ?3, ?4)
             ON CONFLICT DO UPDATE SET first = min(first, excluded.first), last = max(last, excluded.last),
               last_site = CASE WHEN excluded.last >= last THEN excluded.last_site ELSE last_site END",
            params![sys, unit, t, if site > 0 { Some(site) } else { None }],
        )?;
        Ok(())
    }

    /// The radio is on (heard doing something) at `t`.
    fn on(&self, sys: i64, unit: u32, t: i64) -> rusqlite::Result<()> {
        self.conn.execute(
            "UPDATE units SET state_t = CASE WHEN state IS 1 THEN state_t ELSE ?3 END, state = 1 WHERE system = ?1 AND unit = ?2 AND (state_t IS NULL OR state_t <= ?3)",
            params![sys, unit, t],
        )?;
        Ok(())
    }

    fn unit_event(&self, sys: i64, site: i64, t: i64, kind: UnitEvent, unit: u32, tg: Option<u32>) -> rusqlite::Result<()> {
        self.touch_unit(sys, unit, t, site)?;
        match kind {
            UnitEvent::Off => {
                self.conn.execute(
                    "UPDATE units SET state_t = CASE WHEN state IS 0 THEN state_t ELSE ?3 END, state = 0, aff = NULL WHERE system = ?1 AND unit = ?2",
                    params![sys, unit, t],
                )?;
            }
            UnitEvent::Join => {
                self.on(sys, unit, t)?;
                self.conn.execute("UPDATE units SET aff = ?3, aff_t = ?4 WHERE system = ?1 AND unit = ?2", params![sys, unit, tg, t])?;
            }
            _ => self.on(sys, unit, t)?,
        }
        if let (Some(tg), true) = (tg, kind.links()) {
            let (joins, locations) = if kind == UnitEvent::Join { (1, 0) } else { (0, 1) };
            self.conn.execute(
                "INSERT INTO links (system, unit, tg, first, last, joins, locations) VALUES (?1, ?2, ?3, ?4, ?4, ?5, ?6)
                 ON CONFLICT DO UPDATE SET first = min(first, excluded.first), last = max(last, excluded.last),
                   joins = joins + excluded.joins, locations = locations + excluded.locations",
                params![sys, unit, tg, t, joins, locations],
            )?;
            self.conn.execute(
                "INSERT INTO talkgroups (system, tg, first, last) VALUES (?1, ?2, ?3, ?3) ON CONFLICT DO NOTHING",
                params![sys, tg, t],
            )?;
        }
        self.conn.execute(
            "INSERT OR IGNORE INTO unit_events (system, unit, t, kind, tg, site) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![sys, unit, t, kind.as_str(), tg, site],
        )?;
        for s in [site, 0] {
            self.conn.execute(
                "INSERT INTO unit_hours (system, site, unit, hour, events) VALUES (?1, ?2, ?3, ?4, 1) ON CONFLICT DO UPDATE SET events = events + 1",
                params![sys, s, unit, hour(t)],
            )?;
        }
        Ok(())
    }

    /// A call (all its copies) is over: count it once.
    fn keep_call(&self, p: Pending) -> rusqlite::Result<()> {
        let t = p.end;
        let sites = format!(",{},", p.sites.iter().map(|s| s.to_string()).collect::<Vec<_>>().join(","));
        let units = p.units.iter().map(|(u, o)| format!("{u}:{o:.1}")).collect::<Vec<_>>().join(",");
        self.conn.execute(
            "INSERT INTO calls (system, start, secs, tg, uu, freq, enc, emergency, recorded, reason, sites, units) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
            params![p.system, p.start, p.secs as f64, p.tg, p.uu, p.freq as i64, p.enc, p.emergency, p.recorded, p.reason, sites, units],
        )?;
        let call = self.conn.last_insert_rowid();
        self.hours(p.system, 0, hour(t), p.tg, p.uu, p.secs, p.enc, &p.units)?;
        if !p.uu {
            self.conn.execute(
                "INSERT INTO talkgroups (system, tg, first, last, calls, secs, enc_calls) VALUES (?1, ?2, ?3, ?4, 1, ?5, ?6)
                 ON CONFLICT DO UPDATE SET first = min(first, excluded.first), last = max(last, excluded.last),
                   calls = calls + 1, secs = secs + excluded.secs, enc_calls = enc_calls + excluded.enc_calls",
                params![p.system, p.tg, p.start, t, p.secs as f64, p.enc as i64],
            )?;
        }
        let site = p.sites.first().copied().unwrap_or(0);
        // Links count each radio once a call, however many times it keyed up.
        let mut linked = HashSet::new();
        for (i, (u, turn)) in turns(&p.units, p.secs).into_iter().enumerate() {
            let at = p.start + p.units[i].1.round() as i64;
            self.touch_unit(p.system, u, at, site)?;
            self.conn.execute(
                "UPDATE units SET tx = tx + 1, tx_secs = tx_secs + ?3, enc_tx = enc_tx + ?4, started = started + ?5, last_tx = max(coalesce(last_tx, 0), ?6)
                 WHERE system = ?1 AND unit = ?2",
                params![p.system, u, turn as f64, p.enc as i64, (i == 0) as i64, at],
            )?;
            self.on(p.system, u, at)?;
            self.conn.execute(
                "INSERT OR IGNORE INTO call_units (system, unit, start, call, tg, uu, secs, enc) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                params![p.system, u, at, call, p.tg, p.uu, turn as f64, p.enc],
            )?;
            if !p.uu && linked.insert(u) {
                self.conn.execute(
                    "INSERT INTO links (system, unit, tg, first, last, started, talked, enc) VALUES (?1, ?2, ?3, ?4, ?4, ?5, 1, ?6)
                     ON CONFLICT DO UPDATE SET first = min(first, excluded.first), last = max(last, excluded.last),
                       started = started + excluded.started, talked = talked + 1, enc = enc + excluded.enc",
                    params![p.system, u, p.tg, at, (i == 0) as i64, p.enc as i64],
                )?;
            }
        }
        let mut pairs: Vec<(u32, u32)> = p.units.windows(2).map(|w| (w[0].0, w[1].0)).collect();
        if p.uu {
            if let Some(&(a, _)) = p.units.first() {
                pairs.push((a, p.tg));
            }
        }
        let mut seen = HashSet::new();
        for (a, b) in pairs {
            let (a, b) = (a.min(b), a.max(b));
            if a == 0 || a == b || !seen.insert((a, b)) {
                continue;
            }
            self.conn.execute(
                "INSERT INTO pairs (system, a, b, n, enc, last) VALUES (?1, ?2, ?3, 1, ?4, ?5)
                 ON CONFLICT DO UPDATE SET n = n + 1, enc = enc + excluded.enc, last = max(last, excluded.last)",
                params![p.system, a, b, p.enc as i64, t],
            )?;
        }
        Ok(())
    }
}

/// Each radio's turn on a call: from when it was first heard to the next's (or the end).
fn turns(units: &[(u32, f32)], secs: f32) -> Vec<(u32, f32)> {
    units.iter().enumerate().map(|(i, &(u, off))| (u, (units.get(i + 1).map_or(secs, |n| n.1) - off).max(0.0))).collect()
}

// ── queries ──────────────────────────────────────────────────────────────────

/// What a query is about: one site of a system, or (site 0) all of it.
struct Scope {
    system: i64,
    site: i64,
    /// The system's key and its sites' short names (the first names talkgroups).
    key: String,
    short_names: Vec<String>,
    created: i64,
}

fn scope(conn: &Connection, name: &str) -> rusqlite::Result<Option<Scope>> {
    let found: Option<(i64, i64)> = conn
        .query_row("SELECT system, id FROM sites WHERE short_name = ?1 ORDER BY last DESC LIMIT 1", [name], |r| Ok((r.get(0)?, r.get(1)?)))
        .optional()?;
    let (system, site) = match found {
        Some(x) => x,
        None => match conn.query_row("SELECT id FROM systems WHERE key = ?1", [name], |r| r.get::<_, i64>(0)).optional()? {
            Some(id) => (id, 0),
            None => return Ok(None),
        },
    };
    let (key, created): (String, i64) = conn.query_row("SELECT key, first FROM systems WHERE id = ?1", [system], |r| Ok((r.get(0)?, r.get(1)?)))?;
    let mut st = conn.prepare("SELECT short_name FROM sites WHERE system = ?1 ORDER BY (id = ?2) DESC, id")?;
    let short_names = st.query_map(params![system, site], |r| r.get(0))?.collect::<rusqlite::Result<Vec<String>>>()?;
    Ok(Some(Scope { system, site, key, short_names, created }))
}

impl Scope {
    fn tg_name(&self, names: &Names, tg: u32) -> Option<trunk_app::stats::TgInfo> {
        self.short_names.iter().find_map(|n| (names.tg)(n, tg))
    }
    fn alpha(&self, names: &Names, tg: u32) -> String {
        self.tg_name(names, tg).map(|i| i.alpha_tag).unwrap_or_default()
    }
    fn baseline(&self, now: i64) -> bool {
        now - self.created >= BASELINE_S
    }
    fn is_new(&self, first: i64, now: i64) -> bool {
        first > now - NEW_S && first - self.created >= BASELINE_S
    }
}

/// clear / mixed / encrypted, by the share of calls that were.
fn enc_state(enc: i64, all: i64) -> &'static str {
    if enc <= 0 || all <= 0 {
        "clear"
    } else if enc as f64 >= all as f64 * ENC_ALL {
        "encrypted"
    } else {
        "mixed"
    }
}

fn pct(n: i64, of: i64) -> f64 {
    if of > 0 {
        (n as f64 * 1000.0 / of as f64).round() / 10.0
    } else {
        0.0
    }
}

fn round1(x: f64) -> f64 {
    (x * 10.0).round() / 10.0
}

fn state_name(s: Option<i64>) -> Value {
    match s {
        Some(1) => json!("on"),
        Some(0) => json!("off"),
        _ => Value::Null,
    }
}

/// `radioQuery {what, system, key?, hours?, limit?}` at `now`: the shapes the
/// in-memory registry answers ([`trunk_app::stats::Registry::query`]), for
/// any span (`hours` 0: all), plus link kinds, on/off and encryption.
/// `system`: a site's short name, or a system's key for all its sites.
pub fn query(conn: &Connection, q: &Value, now: i64, names: &Names) -> rusqlite::Result<Value> {
    let what = q["what"].as_str().unwrap_or("summary");
    if what == "summary" {
        return Ok(json!({ "what": what, "systems": summary_all(conn, now, names)? }));
    }
    if what == "systems" {
        return Ok(json!({ "what": what, "rows": systems(conn)? }));
    }
    let Some(name) = q["system"].as_str() else { return Ok(json!({ "what": what, "error": "which system?" })) };
    let Some(s) = scope(conn, name)? else { return Ok(json!({ "what": what, "system": name, "rows": [] })) };
    let hours = match q["hours"].as_i64() {
        Some(0) => ((now - s.created) / 3600 + 1).max(1),
        Some(h) => h.max(1),
        None => 24,
    };
    let limit = q["limit"].as_u64().unwrap_or(500).min(5000) as usize;
    let key = q["key"].as_u64().map(|k| k as u32);
    let mut out = match what {
        "talkgroups" => talkgroups(conn, &s, now, hours, limit, names, None)?,
        "units" => units(conn, &s, now, hours, limit, names)?,
        "tg" => match key {
            Some(k) => tg_detail(conn, &s, k, now, hours, names)?,
            None => json!({ "error": "which talkgroup?" }),
        },
        "unit" => match key {
            Some(k) => unit_detail(conn, &s, k, now, names)?,
            None => json!({ "error": "which radio?" }),
        },
        "freqs" => freqs(conn, &s, now, hours)?,
        "lengths" => json!({ "histogram": lengths(conn, &s, now - hours * 3600, None)? }),
        "events" => events(conn, &s, now - hours * 3600, limit, names)?,
        "reasons" => reasons(conn, &s, now - hours * 3600)?,
        _ => json!({ "error": format!("unknown query {what}") }),
    };
    out["what"] = json!(what);
    out["system"] = json!(name);
    out["systemKey"] = json!(s.key);
    out["sites"] = json!(s.short_names);
    Ok(out)
}

fn systems(conn: &Connection) -> rusqlite::Result<Vec<Value>> {
    let mut st = conn.prepare("SELECT id, key, wacn, sysid, first, last FROM systems ORDER BY id")?;
    let rows = st.query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?, r.get::<_, Option<String>>(2)?, r.get::<_, Option<String>>(3)?, r.get::<_, i64>(4)?, r.get::<_, i64>(5)?)))?;
    let mut out = Vec::new();
    for r in rows {
        let (id, key, wacn, sysid, first, last) = r?;
        let mut st = conn.prepare_cached("SELECT short_name, rfss, site, first, last FROM sites WHERE system = ?1 ORDER BY id")?;
        let sites: Vec<Value> = st
            .query_map([id], |r| Ok(json!({ "shortName": r.get::<_, String>(0)?, "rfss": r.get::<_, Option<i64>>(1)?, "site": r.get::<_, Option<i64>>(2)?, "first": r.get::<_, i64>(3)?, "last": r.get::<_, i64>(4)? })))?
            .collect::<rusqlite::Result<_>>()?;
        out.push(json!({ "key": key, "wacn": wacn, "sysid": sysid, "first": first, "last": last, "sites": sites }));
    }
    Ok(out)
}

fn summary_all(conn: &Connection, now: i64, names: &Names) -> rusqlite::Result<serde_json::Map<String, Value>> {
    let mut targets: Vec<String> = Vec::new();
    {
        let mut st = conn.prepare("SELECT short_name FROM sites ORDER BY id")?;
        for n in st.query_map([], |r| r.get::<_, String>(0))? {
            targets.push(n?);
        }
        let mut st = conn.prepare("SELECT systems.key FROM systems WHERE (SELECT count(*) FROM sites WHERE sites.system = systems.id) > 1")?;
        for n in st.query_map([], |r| r.get::<_, String>(0))? {
            targets.push(n?);
        }
    }
    let mut out = serde_json::Map::new();
    for name in targets {
        if out.contains_key(&name) {
            continue;
        }
        if let Some(s) = scope(conn, &name)? {
            out.insert(name, summary(conn, &s, now, names)?);
        }
    }
    Ok(out)
}

fn count(conn: &Connection, sql: &str, p: impl rusqlite::Params) -> rusqlite::Result<i64> {
    conn.prepare_cached(sql)?.query_row(p, |r| r.get(0))
}

fn summary(conn: &Connection, s: &Scope, now: i64, names: &Names) -> rusqlite::Result<Value> {
    let (h1, h24) = (hour(now - 3600), hour(now - NEW_S));
    let distinct_units = |h: i64| count(conn, "SELECT count(DISTINCT unit) FROM unit_hours WHERE system = ?1 AND site = ?2 AND hour >= ?3", params![s.system, s.site, h]);
    let tgs24: Vec<u32> = {
        let mut st = conn.prepare_cached("SELECT DISTINCT tg FROM tg_hours WHERE system = ?1 AND site = ?2 AND hour >= ?3")?;
        st.query_map(params![s.system, s.site, h24], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?
    };
    let (tgs_all, units_all) = if s.site == 0 {
        (count(conn, "SELECT count(*) FROM talkgroups WHERE system = ?1", [s.system])?, count(conn, "SELECT count(*) FROM units WHERE system = ?1", [s.system])?)
    } else {
        (
            count(conn, "SELECT count(DISTINCT tg) FROM tg_hours WHERE system = ?1 AND site = ?2", params![s.system, s.site])?,
            count(conn, "SELECT count(DISTINCT unit) FROM unit_hours WHERE system = ?1 AND site = ?2", params![s.system, s.site])?,
        )
    };
    let since_new = (now - NEW_S).max(s.created + BASELINE_S);
    let new_tgs: Vec<Value> = {
        let mut st = conn.prepare_cached("SELECT tg, first FROM talkgroups WHERE system = ?1 AND first > ?2 ORDER BY first DESC LIMIT 200")?;
        let rows = st.query_map(params![s.system, since_new], |r| Ok((r.get::<_, u32>(0)?, r.get::<_, i64>(1)?)))?;
        let mut v = Vec::new();
        for r in rows {
            let (tg, first) = r?;
            v.push(json!({ "tg": tg, "first": first, "alphaTag": s.alpha(names, tg) }));
        }
        v
    };
    Ok(json!({
        "since": s.created,
        "systemKey": s.key,
        "sites": s.short_names,
        "talkgroups": tgs_all,
        "tgs24h": tgs24.len(),
        "units": units_all,
        "units1h": distinct_units(h1)?,
        "units24h": distinct_units(h24)?,
        "newUnits24h": count(conn, "SELECT count(*) FROM units WHERE system = ?1 AND first > ?2", params![s.system, since_new])?,
        "newTgs": new_tgs,
        "unknownTgs24h": tgs24.iter().filter(|&&tg| s.tg_name(names, tg).is_none()).count(),
        "baseline": s.baseline(now),
    }))
}

/// Per-point activity over `hours` back from `now`: one point an hour up to
/// [`MAX_POINTS`], else that many buckets. Returns (first hour, hours a point).
fn buckets(now: i64, hours: i64) -> (i64, i64, usize) {
    let h1 = hour(now);
    let per = (hours + MAX_POINTS - 1) / MAX_POINTS;
    let n = (hours + per - 1) / per;
    (h1 - n * per + 1, per, n as usize)
}

#[allow(clippy::too_many_arguments)]
fn talkgroups(conn: &Connection, s: &Scope, now: i64, hours: i64, limit: usize, names: &Names, only: Option<u32>) -> rusqlite::Result<Value> {
    let (h0, per, n) = buckets(now, hours);
    let mut st = conn.prepare_cached(
        "SELECT h.tg, sum(h.calls), sum(h.secs), sum(h.enc), g.first, g.last, g.calls, g.enc_calls
         FROM tg_hours h JOIN talkgroups g ON g.system = h.system AND g.tg = h.tg
         WHERE h.system = ?1 AND h.site = ?2 AND h.hour >= ?3 AND (?4 IS NULL OR h.tg = ?4)
         GROUP BY h.tg ORDER BY sum(h.secs) DESC, g.last DESC",
    )?;
    let rows: Vec<(u32, i64, f64, i64, i64, i64, i64, i64)> = st
        .query_map(params![s.system, s.site, h0, only], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?, r.get(7)?)))?
        .collect::<rusqlite::Result<_>>()?;
    let total = rows.len();
    let rows = &rows[..rows.len().min(limit)];
    let mut series: HashMap<u32, Vec<i64>> = HashMap::new();
    {
        let mut st = conn.prepare_cached("SELECT tg, hour, calls FROM tg_hours WHERE system = ?1 AND site = ?2 AND hour >= ?3 AND (?4 IS NULL OR tg = ?4)")?;
        let want: HashSet<u32> = rows.iter().map(|r| r.0).collect();
        for r in st.query_map(params![s.system, s.site, h0, only], |r| Ok((r.get::<_, u32>(0)?, r.get::<_, i64>(1)?, r.get::<_, i64>(2)?)))? {
            let (tg, h, c) = r?;
            if want.contains(&tg) {
                let i = ((h - h0) / per) as usize;
                if i < n {
                    series.entry(tg).or_insert_with(|| vec![0; n])[i] += c;
                }
            }
        }
    }
    let out: Vec<Value> = rows
        .iter()
        .map(|&(tg, calls, secs, enc, first, last, total_calls, total_enc)| {
            let info = s.tg_name(names, tg);
            json!({
                "tg": tg,
                "alphaTag": info.as_ref().map_or("", |i| i.alpha_tag.as_str()),
                "known": info.is_some(),
                "ignore": info.as_ref().is_some_and(|i| i.ignore),
                "first": first, "last": last,
                "calls": calls, "secs": round1(secs), "encCalls": enc,
                "totalCalls": total_calls,
                "encPct": pct(enc, calls),
                "enc": enc_state(enc, calls),
                "totalEnc": enc_state(total_enc, total_calls),
                "new": s.is_new(first, now),
                "hourly": series.remove(&tg).unwrap_or_else(|| vec![0; n]),
            })
        })
        .collect();
    Ok(json!({ "hours": hours, "bucketHours": per, "total": total, "rows": out }))
}

/// A radio's talkgroups: (talkgroup, meetings of any kind, encrypted calls), most first.
fn unit_tgs(conn: &Connection, s: &Scope, unit: u32, n: usize) -> rusqlite::Result<Vec<(u32, i64, i64)>> {
    let mut st = conn.prepare_cached("SELECT tg, talked + joins + locations, enc FROM links WHERE system = ?1 AND unit = ?2 ORDER BY talked + joins + locations DESC, last DESC LIMIT ?3")?;
    st.query_map(params![s.system, unit, n as i64], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?.collect()
}

fn partners(conn: &Connection, s: &Scope, unit: u32, n: usize) -> rusqlite::Result<Vec<(u32, i64, i64)>> {
    let mut st = conn.prepare_cached(
        "SELECT b, n, enc FROM pairs WHERE system = ?1 AND a = ?2 UNION ALL SELECT a, n, enc FROM pairs WHERE system = ?1 AND b = ?2 ORDER BY 2 DESC LIMIT ?3",
    )?;
    st.query_map(params![s.system, unit, n as i64], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?.collect()
}

struct UnitRow {
    unit: u32,
    first: i64,
    last: i64,
    last_tx: Option<i64>,
    tx: i64,
    tx_secs: f64,
    enc_tx: i64,
    started: i64,
    state: Option<i64>,
    state_t: Option<i64>,
    aff: Option<u32>,
    aff_t: Option<i64>,
    alias: Option<String>,
    last_site: Option<String>,
}

const UNIT_COLS: &str = "u.unit, u.first, u.last, u.last_tx, u.tx, u.tx_secs, u.enc_tx, u.started, u.state, u.state_t, u.aff, u.aff_t, u.alias,
    (SELECT short_name FROM sites WHERE sites.id = u.last_site)";

fn unit_row_of(r: &rusqlite::Row, at: usize) -> rusqlite::Result<UnitRow> {
    Ok(UnitRow {
        unit: r.get(at)?,
        first: r.get(at + 1)?,
        last: r.get(at + 2)?,
        last_tx: r.get(at + 3)?,
        tx: r.get(at + 4)?,
        tx_secs: r.get(at + 5)?,
        enc_tx: r.get(at + 6)?,
        started: r.get(at + 7)?,
        state: r.get(at + 8)?,
        state_t: r.get(at + 9)?,
        aff: r.get(at + 10)?,
        aff_t: r.get(at + 11)?,
        alias: r.get(at + 12)?,
        last_site: r.get(at + 13)?,
    })
}

fn unit_json(conn: &Connection, s: &Scope, u: &UnitRow, now: i64, names: &Names, span: Option<(i64, f64, i64, i64)>, partners_n: usize) -> rusqlite::Result<Value> {
    let alias = u.alias.clone().filter(|a| !a.is_empty()).or_else(|| s.short_names.iter().find_map(|n| (names.unit)(n, u.unit))).unwrap_or_default();
    let mut v = json!({
        "unit": u.unit,
        "alias": alias,
        "first": u.first, "last": u.last, "lastTx": u.last_tx,
        "tx": u.tx, "txSecs": round1(u.tx_secs), "encTx": u.enc_tx, "started": u.started,
        "enc": enc_state(u.enc_tx, u.tx),
        "aff": u.aff, "affT": u.aff.and(u.aff_t),
        "reg": u.state.map(|x| x == 1),
        "state": state_name(u.state), "stateT": u.state_t,
        "lastSite": u.last_site,
        "tgs": unit_tgs(conn, s, u.unit, TOP_TGS)?,
        "partners": if partners_n > 0 { partners(conn, s, u.unit, partners_n)?.into_iter().map(|(p, n, _)| (p, n)).collect() } else { Vec::new() },
        "new": s.is_new(u.first, now),
    });
    if let Some((tx, secs, enc, events)) = span {
        v["spanTx"] = json!(tx);
        v["spanSecs"] = json!(round1(secs));
        v["spanEnc"] = json!(enc_state(enc, tx));
        v["spanEvents"] = json!(events);
    }
    Ok(v)
}

fn units(conn: &Connection, s: &Scope, now: i64, hours: i64, limit: usize, names: &Names) -> rusqlite::Result<Value> {
    let h0 = hour(now) - hours + 1;
    let mut st = conn.prepare_cached(&format!(
        "SELECT sum(h.tx), sum(h.secs), sum(h.enc), sum(h.events), {UNIT_COLS}
         FROM unit_hours h JOIN units u ON u.system = h.system AND u.unit = h.unit
         WHERE h.system = ?1 AND h.site = ?2 AND h.hour >= ?3
         GROUP BY h.unit ORDER BY coalesce(u.last_tx, 0) DESC, u.last DESC"
    ))?;
    let rows: Vec<((i64, f64, i64, i64), UnitRow)> = st
        .query_map(params![s.system, s.site, h0], |r| Ok(((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?), unit_row_of(r, 4)?)))?
        .collect::<rusqlite::Result<_>>()?;
    let total = rows.len();
    let mut out = Vec::new();
    for (i, (span, u)) in rows.iter().take(limit).enumerate() {
        out.push(unit_json(conn, s, u, now, names, Some(*span), if i < 100 { 3 } else { 0 })?);
    }
    Ok(json!({ "hours": hours, "total": total, "rows": out }))
}

fn alias_of(conn: &Connection, s: &Scope, names: &Names, unit: u32) -> rusqlite::Result<String> {
    let a: Option<Option<String>> = conn.prepare_cached("SELECT alias FROM units WHERE system = ?1 AND unit = ?2")?.query_row(params![s.system, unit], |r| r.get(0)).optional()?;
    Ok(a.flatten().filter(|a| !a.is_empty()).or_else(|| s.short_names.iter().find_map(|n| (names.unit)(n, unit))).unwrap_or_default())
}

fn lengths(conn: &Connection, s: &Scope, since: i64, tg: Option<u32>) -> rusqlite::Result<Value> {
    let mut h = [0u32; LENGTH_BINS.len()];
    let mut st = conn.prepare_cached("SELECT secs FROM calls WHERE system = ?1 AND start >= ?2 AND uu = 0 AND (?3 IS NULL OR tg = ?3) AND (?4 = 0 OR instr(sites, ',' || ?4 || ',') > 0)")?;
    for l in st.query_map(params![s.system, since, tg, s.site], |r| r.get::<_, f64>(0))? {
        let l = l? as f32;
        h[LENGTH_BINS.iter().rposition(|&b| l >= b).unwrap_or(0)] += 1;
    }
    Ok(json!({ "edges": LENGTH_BINS, "counts": h }))
}

fn tg_detail(conn: &Connection, s: &Scope, tg: u32, now: i64, hours: i64, names: &Names) -> rusqlite::Result<Value> {
    let row = talkgroups(conn, s, now, hours, 1, names, Some(tg))?["rows"][0].clone();
    if row.is_null() {
        return Ok(json!({ "tg": tg, "row": null }));
    }
    let week = talkgroups(conn, s, now, 168, 1, names, Some(tg))?["rows"][0]["hourly"].clone();
    let mut st = conn.prepare_cached(&format!(
        "SELECT l.started, l.talked, l.enc, l.joins, l.locations, l.first, l.last, {UNIT_COLS}
         FROM links l JOIN units u ON u.system = l.system AND u.unit = l.unit
         WHERE l.system = ?1 AND l.tg = ?2 ORDER BY l.talked DESC, l.joins + l.locations DESC, l.last DESC LIMIT 300"
    ))?;
    let members: Vec<((i64, i64, i64, i64, i64, i64, i64), UnitRow)> = st
        .query_map(params![s.system, tg], |r| Ok(((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?), unit_row_of(r, 7)?)))?
        .collect::<rusqlite::Result<_>>()?;
    let alias = |u: &UnitRow| u.alias.clone().filter(|a| !a.is_empty()).or_else(|| s.short_names.iter().find_map(|n| (names.unit)(n, u.unit))).unwrap_or_default();
    let talkers: Vec<Value> = members.iter().filter(|m| m.0 .1 > 0).take(60).map(|(l, u)| json!({ "unit": u.unit, "alias": alias(u), "n": l.1, "lastTx": u.last_tx })).collect();
    let mut st = conn.prepare_cached("SELECT unit, alias FROM units WHERE system = ?1 AND aff = ?2 AND (state IS NULL OR state = 1) ORDER BY aff_t DESC LIMIT 200")?;
    let affiliated: Vec<Value> = st
        .query_map(params![s.system, tg], |r| Ok((r.get::<_, u32>(0)?, r.get::<_, Option<String>>(1)?)))?
        .map(|r| r.map(|(u, a)| json!({ "unit": u, "alias": a.filter(|a| !a.is_empty()).or_else(|| s.short_names.iter().find_map(|n| (names.unit)(n, u))).unwrap_or_default() })))
        .collect::<rusqlite::Result<_>>()?;
    let links: Vec<Value> = members
        .iter()
        .map(|((started, talked, enc, joins, locations, first, last), u)| {
            json!({
                "unit": u.unit, "alias": alias(u), "started": started, "talked": talked, "enc": enc_state(*enc, *talked), "encCalls": enc,
                "joins": joins, "locations": locations, "first": first, "last": last,
                "state": state_name(u.state), "unitLast": u.last, "unitEnc": enc_state(u.enc_tx, u.tx),
            })
        })
        .collect();
    Ok(json!({
        "tg": tg,
        "row": row,
        "week": week,
        "talkers": talkers,
        "affiliated": affiliated,
        "links": links,
        "lengths": lengths(conn, s, now - hours * 3600, Some(tg))?,
    }))
}

fn unit_detail(conn: &Connection, s: &Scope, id: u32, now: i64, names: &Names) -> rusqlite::Result<Value> {
    let u = conn
        .prepare_cached(&format!("SELECT {UNIT_COLS} FROM units u WHERE u.system = ?1 AND u.unit = ?2"))?
        .query_row(params![s.system, id], |r| unit_row_of(r, 0))
        .optional()?;
    let Some(u) = u else { return Ok(json!({ "unit": id, "row": null })) };
    let mut st = conn.prepare_cached("SELECT start, tg, uu, secs, enc FROM call_units WHERE system = ?1 AND unit = ?2 ORDER BY start DESC LIMIT 30")?;
    let recent: Vec<Value> = st
        .query_map(params![s.system, id], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, u32>(1)?, r.get::<_, bool>(2)?, r.get::<_, f64>(3)?, r.get::<_, bool>(4)?)))?
        .map(|r| r.map(|(t, tg, uu, secs, enc)| json!({ "t": t, "tg": tg, "uu": uu, "alphaTag": if uu { String::new() } else { s.alpha(names, tg) }, "secs": round1(secs), "enc": enc })))
        .collect::<rusqlite::Result<_>>()?;
    let mut st = conn.prepare_cached(
        "SELECT e.t, e.kind, e.tg, (SELECT short_name FROM sites WHERE sites.id = e.site) FROM unit_events e WHERE e.system = ?1 AND e.unit = ?2 ORDER BY e.t DESC LIMIT 200",
    )?;
    let evs: Vec<(i64, String, Option<u32>, Option<String>)> = st.query_map(params![s.system, id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?.collect::<rusqlite::Result<_>>()?;
    let affiliations: Vec<Value> = evs.iter().filter(|e| e.1 == "join").take(50).map(|e| json!({ "t": e.0, "tg": e.2, "alphaTag": e.2.map(|tg| s.alpha(names, tg)).unwrap_or_default(), "site": e.3 })).collect();
    let events: Vec<Value> = evs.iter().map(|e| json!({ "t": e.0, "kind": e.1, "tg": e.2, "alphaTag": e.2.map(|tg| s.alpha(names, tg)).unwrap_or_default(), "site": e.3 })).collect();
    let mut st = conn.prepare_cached("SELECT tg, started, talked, enc, joins, locations, first, last FROM links WHERE system = ?1 AND unit = ?2 ORDER BY talked + joins + locations DESC, last DESC")?;
    let tgs: Vec<Value> = st
        .query_map(params![s.system, id], |r| Ok((r.get::<_, u32>(0)?, r.get::<_, i64>(1)?, r.get::<_, i64>(2)?, r.get::<_, i64>(3)?, r.get::<_, i64>(4)?, r.get::<_, i64>(5)?, r.get::<_, i64>(6)?, r.get::<_, i64>(7)?)))?
        .map(|r| {
            r.map(|(tg, started, talked, enc, joins, locations, first, last)| {
                json!({ "tg": tg, "alphaTag": s.alpha(names, tg), "n": talked, "started": started, "talked": talked, "enc": enc_state(enc, talked), "encCalls": enc,
                        "joins": joins, "locations": locations, "first": first, "last": last })
            })
        })
        .collect::<rusqlite::Result<_>>()?;
    let mut ps = Vec::new();
    for (p, n, enc) in partners(conn, s, id, 24)? {
        ps.push(json!({ "unit": p, "alias": alias_of(conn, s, names, p)?, "n": n, "enc": enc_state(enc, n) }));
    }
    let mut st = conn.prepare_cached("SELECT alias, first, last FROM aliases WHERE system = ?1 AND unit = ?2 ORDER BY last DESC")?;
    let aliases: Vec<Value> = st
        .query_map(params![s.system, id], |r| Ok(json!({ "alias": r.get::<_, String>(0)?, "first": r.get::<_, i64>(1)?, "last": r.get::<_, i64>(2)? })))?
        .collect::<rusqlite::Result<_>>()?;
    Ok(json!({
        "unit": id,
        "row": unit_json(conn, s, &u, now, names, None, 0)?,
        "recent": recent,
        "affiliations": affiliations,
        "events": events,
        "tgs": tgs,
        "partners": ps,
        "aliases": aliases,
    }))
}

fn freqs(conn: &Connection, s: &Scope, now: i64, hours: i64) -> rusqlite::Result<Value> {
    let (h0, per, n) = buckets(now, hours);
    let mut st = conn.prepare_cached(
        "SELECT f.freq, f.calls, f.frames, f.errors, f.bad, f.snr, f.ferr, f.clean, f.last, (SELECT short_name FROM sites WHERE sites.id = f.site), f.site
         FROM freqs f WHERE f.system = ?1 AND (?2 = 0 OR f.site = ?2) ORDER BY f.freq, f.site",
    )?;
    type F = (i64, i64, i64, i64, i64, Option<f64>, Option<f64>, Option<f64>, i64, Option<String>, i64);
    let all: Vec<F> = st
        .query_map(params![s.system, s.site], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?, r.get(7)?, r.get(8)?, r.get(9)?, r.get(10)?)))?
        .collect::<rusqlite::Result<_>>()?;
    let mut hours_of: HashMap<(i64, i64), Vec<(i64, i64, i64, i64)>> = HashMap::new();
    {
        let mut st = conn.prepare_cached("SELECT freq, site, hour, calls, frames, errors, bad FROM freq_hours WHERE system = ?1 AND (?2 = 0 OR site = ?2) AND hour >= ?3")?;
        for r in st.query_map(params![s.system, s.site, h0], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?, r.get::<_, i64>(2)?, (r.get::<_, i64>(3)?, r.get::<_, i64>(4)?, r.get::<_, i64>(5)?, r.get::<_, i64>(6)?))))? {
            let (f, site, h, v) = r?;
            let row = hours_of.entry((f, site)).or_insert_with(|| vec![(0, 0, 0, 0); n]);
            let i = ((h - h0) / per) as usize;
            if i < n {
                row[i].0 += v.0;
                row[i].1 += v.1;
                row[i].2 += v.2;
                row[i].3 += v.3;
            }
        }
    }
    let rows: Vec<Value> = all
        .into_iter()
        .map(|(hz, calls, frames, errors, bad, snr, ferr, clean, last, site_name, site)| {
            let hs = hours_of.remove(&(hz, site)).unwrap_or_else(|| vec![(0, 0, 0, 0); n]);
            let (c, fr, er, bd) = hs.iter().fold((0, 0, 0, 0), |a, x| (a.0 + x.0, a.1 + x.1, a.2 + x.2, a.3 + x.3));
            let spark: Vec<Value> = hs.iter().map(|x| if x.1 > 0 { json!(pct(x.3, x.1)) } else { Value::Null }).collect();
            json!({
                "freqHz": hz, "site": site_name, "calls": c, "frames": fr, "errors": er,
                "errPerFrame": if fr > 0 { Some((er as f64 / fr as f64 * 1000.0).round() / 1000.0) } else { None },
                "totalCalls": calls,
                "allErrPerFrame": if frames > 0 { Some((errors as f64 / frames as f64 * 1000.0).round() / 1000.0) } else { None },
                "badPct": if fr > 0 { Some(pct(bd, fr)) } else { None },
                "allBadPct": if frames > 0 { Some(pct(bad, frames)) } else { None },
                "snr": snr.map(round1), "freqError": ferr.map(f64::round), "clean": clean.map(round1),
                "last": last, "hourly": spark,
            })
        })
        .collect();
    Ok(json!({ "hours": hours, "bucketHours": per, "rows": rows }))
}

/// What happened to calls: recorded, followed only, or why not recorded.
fn reasons(conn: &Connection, s: &Scope, since: i64) -> rusqlite::Result<Value> {
    let mut st = conn.prepare_cached(
        "SELECT CASE WHEN recorded THEN 'recorded' WHEN reason = '' THEN 'monitored' ELSE reason END, count(*)
         FROM calls WHERE system = ?1 AND start >= ?2 AND (?3 = 0 OR instr(sites, ',' || ?3 || ',') > 0) GROUP BY 1",
    )?;
    let mut counts = serde_json::Map::new();
    for r in st.query_map(params![s.system, since, s.site], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))? {
        let (k, n) = r?;
        counts.insert(k, json!(n));
    }
    Ok(json!({ "counts": counts }))
}

/// Radios' joins, locations and on/off, newest first.
fn events(conn: &Connection, s: &Scope, since: i64, limit: usize, names: &Names) -> rusqlite::Result<Value> {
    let mut st = conn.prepare_cached(
        "SELECT e.t, e.unit, e.kind, e.tg, (SELECT short_name FROM sites WHERE sites.id = e.site), u.alias
         FROM unit_events e LEFT JOIN units u ON u.system = e.system AND u.unit = e.unit
         WHERE e.system = ?1 AND e.t >= ?2 AND (?3 = 0 OR e.site = ?3) ORDER BY e.t DESC LIMIT ?4",
    )?;
    let rows: Vec<Value> = st
        .query_map(params![s.system, since, s.site, limit as i64], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, u32>(1)?, r.get::<_, String>(2)?, r.get::<_, Option<u32>>(3)?, r.get::<_, Option<String>>(4)?, r.get::<_, Option<String>>(5)?)))?
        .map(|r| {
            r.map(|(t, unit, kind, tg, site, alias)| {
                let alias = alias.filter(|a| !a.is_empty()).or_else(|| s.short_names.iter().find_map(|n| (names.unit)(n, unit))).unwrap_or_default();
                json!({ "t": t, "unit": unit, "alias": alias, "kind": kind, "tg": tg, "alphaTag": tg.map(|tg| s.alpha(names, tg)).unwrap_or_default(), "site": site })
            })
        })
        .collect::<rusqlite::Result<_>>()?;
    Ok(json!({ "rows": rows }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp() -> PathBuf {
        static N: AtomicU64 = AtomicU64::new(0);
        let p = std::env::temp_dir().join(format!("radiodb-test-{}-{}.db", std::process::id(), N.fetch_add(1, Ordering::Relaxed)));
        let _ = std::fs::remove_file(&p);
        p
    }

    fn heard(short: &str, sys: Option<&str>, site: u32, t: i64) -> Heard {
        Heard { short_name: short.into(), system: sys.map(String::from), site: Some((1, site)), t }
    }

    const SYS: &str = "p25:bee00.3a9";

    #[allow(clippy::too_many_arguments)]
    fn call(short: &str, site: u32, tg: u32, start: i64, secs: f32, enc: bool, units: &[(u32, f32)]) -> RadioRecord {
        RadioRecord::Call {
            at: heard(short, Some(SYS), site, start + secs as i64),
            tg,
            unit_to_unit: false,
            freq_hz: 771_000_000,
            start,
            secs,
            encrypted: enc,
            emergency: false,
            recorded: !enc,
            reason: if enc { "encrypted".into() } else { String::new() },
            units: units.to_vec(),
        }
    }

    fn unit(short: &str, site: u32, t: i64, kind: UnitEvent, unit: u32, tg: Option<u32>) -> RadioRecord {
        RadioRecord::Unit { at: heard(short, Some(SYS), site, t), kind, unit, tg }
    }

    fn names() -> (impl Fn(&str, u32) -> Option<trunk_app::stats::TgInfo>, impl Fn(&str, u32) -> Option<String>) {
        (|_: &str, tg: u32| (tg == 100).then(|| trunk_app::stats::TgInfo { alpha_tag: "Dispatch".into(), ignore: false }), |_: &str, _: u32| None)
    }

    fn q(conn: &Connection, v: Value, now: i64) -> Value {
        let (tg, unit) = names();
        query(conn, &v, now, &Names { tg: &tg, unit: &unit }).unwrap()
    }

    #[test]
    fn a_call_heard_on_two_sites_is_one_call() {
        let path = temp();
        let mut w = Writer::new(open_rw(&path).unwrap()).unwrap();
        let t0 = 1_800_000_000;
        w.apply(vec![call("site-a", 1, 100, t0, 6.0, false, &[(11, 0.0), (12, 3.0), (11, 5.0)]), call("site-b", 2, 100, t0 + 1, 5.0, false, &[(11, 0.0), (13, 4.0)])], t0 + 10).unwrap();
        w.apply(vec![], t0 + 30).unwrap();
        let c = &w.conn;
        assert_eq!(count(c, "SELECT count(*) FROM calls", []).unwrap(), 1);
        assert_eq!(count(c, "SELECT calls FROM talkgroups WHERE tg = 100", []).unwrap(), 1);
        assert_eq!(count(c, "SELECT talked FROM links WHERE unit = 11 AND tg = 100", []).unwrap(), 1, "once per call, not per copy or turn");
        assert_eq!(count(c, "SELECT tx FROM units WHERE unit = 11", []).unwrap(), 2, "each turn a transmission");
        assert_eq!(count(c, "SELECT started FROM links WHERE unit = 11 AND tg = 100", []).unwrap(), 1);
        assert_eq!(count(c, "SELECT count(*) FROM links WHERE tg = 100", []).unwrap(), 3, "every radio on any copy");
        // Per site, each copy; the system (site 0), once.
        assert_eq!(count(c, "SELECT sum(calls) FROM tg_hours WHERE site = 0", []).unwrap(), 1);
        assert_eq!(count(c, "SELECT sum(calls) FROM tg_hours WHERE site > 0", []).unwrap(), 2);
        let sys = q(c, json!({ "what": "talkgroups", "system": SYS, "hours": 24 }), t0 + 60);
        assert_eq!(sys["rows"][0]["calls"], 1);
        assert_eq!(sys["rows"][0]["alphaTag"], "Dispatch");
        assert_eq!(sys["sites"], json!(["site-a", "site-b"]));
        let a = q(c, json!({ "what": "talkgroups", "system": "site-a", "hours": 24 }), t0 + 60);
        assert_eq!(a["rows"][0]["calls"], 1);
        assert_eq!(a["systemKey"], SYS);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn joins_locations_and_on_off_are_kept() {
        let path = temp();
        let mut w = Writer::new(open_rw(&path).unwrap()).unwrap();
        let t0 = 1_800_000_000;
        w.apply(
            vec![
                unit("site-a", 1, t0, UnitEvent::On, 21, None),
                unit("site-a", 1, t0 + 1, UnitEvent::Join, 21, Some(100)),
                unit("site-b", 2, t0 + 60, UnitEvent::Location, 21, Some(200)),
                unit("site-b", 2, t0 + 120, UnitEvent::Off, 21, None),
            ],
            t0 + 200,
        )
        .unwrap();
        let c = &w.conn;
        assert_eq!(count(c, "SELECT joins FROM links WHERE unit = 21 AND tg = 100", []).unwrap(), 1);
        assert_eq!(count(c, "SELECT locations FROM links WHERE unit = 21 AND tg = 200", []).unwrap(), 1);
        assert_eq!(count(c, "SELECT count(*) FROM unit_events WHERE unit = 21", []).unwrap(), 4);
        let u = q(c, json!({ "what": "unit", "system": SYS, "key": 21 }), t0 + 300);
        assert_eq!(u["row"]["state"], "off");
        assert_eq!(u["row"]["stateT"], t0 + 120);
        assert_eq!(u["row"]["aff"], Value::Null, "off clears the affiliation");
        assert_eq!(u["row"]["lastSite"], "site-b");
        assert_eq!(u["events"].as_array().unwrap().len(), 4);
        assert_eq!(u["tgs"].as_array().unwrap().len(), 2, "a location links too");
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn encryption_is_counted_per_talkgroup_radio_and_link() {
        let path = temp();
        let mut w = Writer::new(open_rw(&path).unwrap()).unwrap();
        let t0 = 1_800_000_000;
        w.apply(vec![call("site-a", 1, 100, t0, 3.0, true, &[(31, 0.0)]), call("site-a", 1, 100, t0 + 60, 3.0, false, &[(31, 0.0)]), call("site-a", 1, 300, t0 + 120, 3.0, true, &[(32, 0.0)])], t0 + 300).unwrap();
        let c = &w.conn;
        let tgs = q(c, json!({ "what": "talkgroups", "system": "site-a" }), t0 + 400);
        let row = |tg: u32| tgs["rows"].as_array().unwrap().iter().find(|r| r["tg"] == tg).unwrap().clone();
        assert_eq!(row(100)["enc"], "mixed");
        assert_eq!(row(300)["enc"], "encrypted");
        let u = q(c, json!({ "what": "unit", "system": "site-a", "key": 31 }), t0 + 400);
        assert_eq!(u["row"]["enc"], "mixed");
        assert_eq!(u["tgs"][0]["encCalls"], 1);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn a_site_waits_for_its_system_then_falls_back_to_its_own() {
        let path = temp();
        let mut w = Writer::new(open_rw(&path).unwrap()).unwrap();
        let t0 = 1_800_000_000;
        let early = RadioRecord::Unit { at: Heard { short_name: "site-a".into(), system: None, site: None, t: t0 }, kind: UnitEvent::On, unit: 41, tg: None };
        w.apply(vec![early], t0 + 1).unwrap();
        assert_eq!(count(&w.conn, "SELECT count(*) FROM units", []).unwrap(), 0, "waiting");
        w.apply(vec![unit("site-a", 1, t0 + 2, UnitEvent::On, 42, None)], t0 + 3).unwrap();
        assert_eq!(count(&w.conn, "SELECT count(*) FROM units u JOIN systems s ON s.id = u.system WHERE s.key = ?1", [SYS]).unwrap(), 2, "placed once the system is known");
        let conv = RadioRecord::Unit { at: Heard { short_name: "conv".into(), system: None, site: None, t: t0 }, kind: UnitEvent::On, unit: 43, tg: None };
        w.apply(vec![conv], t0 + UNRESOLVED_S + 5).unwrap();
        assert_eq!(count(&w.conn, "SELECT count(*) FROM systems WHERE key = 'cfg:conv'", []).unwrap(), 1);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn its_answers_match_the_protocol() {
        let path = temp();
        let mut w = Writer::new(open_rw(&path).unwrap()).unwrap();
        let t0 = 1_800_000_000;
        w.apply(
            vec![
                unit("site-a", 1, t0, UnitEvent::On, 11, None),
                unit("site-a", 1, t0 + 1, UnitEvent::Join, 11, Some(100)),
                call("site-a", 1, 100, t0 + 5, 6.0, false, &[(11, 0.0), (12, 3.0)]),
                call("site-b", 2, 100, t0 + 6, 5.0, true, &[(11, 0.0)]),
                RadioRecord::Alias { at: heard("site-a", Some(SYS), 1, t0 + 7), unit: 11, alias: "Engine 1".into(), tg: 100 },
                RadioRecord::Voice { at: heard("site-a", Some(SYS), 1, t0 + 12), freq_hz: 771_000_000, frames: 100, errors: 3, bad: 1, snr: Some(18.0), freq_error: Some(-40.0), clean: Some(97.0) },
            ],
            t0 + 60,
        )
        .unwrap();
        let mut msgs = Vec::new();
        for (i, v) in [
            json!({ "what": "summary" }),
            json!({ "what": "systems" }),
            json!({ "what": "talkgroups", "system": SYS, "hours": 0 }),
            json!({ "what": "talkgroups", "system": "site-a", "hours": 8760 }),
            json!({ "what": "units", "system": SYS }),
            json!({ "what": "tg", "system": SYS, "key": 100 }),
            json!({ "what": "unit", "system": "site-b", "key": 11 }),
            json!({ "what": "freqs", "system": SYS }),
            json!({ "what": "lengths", "system": SYS }),
            json!({ "what": "events", "system": SYS }),
            json!({ "what": "reasons", "system": SYS }),
            json!({ "what": "units", "system": "nowhere" }),
        ]
        .into_iter()
        .enumerate()
        {
            let mut r = q(&w.conn, v, t0 + 120);
            r["type"] = json!("radioResult");
            r["id"] = json!(i);
            msgs.push(r);
        }
        assert_eq!(msgs[6]["row"]["alias"], "Engine 1", "an alias heard on one site, on the other");
        assert_eq!(msgs[10]["counts"]["recorded"], 1);
        crate::protocol_tests::check_all("FromRecorder", &msgs);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn spans_past_a_week_are_bucketed() {
        assert_eq!(buckets(3600 * 1000, 24), (1000 - 23, 1, 24));
        let (h0, per, n) = buckets(3600 * 10_000, 24 * 365);
        assert_eq!((per, n), (53, 166));
        assert!(h0 + per * n as i64 - 1 == 10_000);
    }
}
