//! What the radio registry hears, handed over as it happens to a store that
//! keeps all of it (the desktop app's radio database). The [`Registry`]
//! keeps only bounded, recent state; a [`RadioSink`] gets every call, every
//! radio's joins, locations and on/off, and how each frequency decoded.
//!
//! [`Registry`]: super::Registry

use trunk_core::trunk::{Call, Engine, Message, MessageType};

/// Where and when something was heard.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Heard {
    /// The config's short name (one site, or a conventional system).
    pub short_name: String,
    /// The system it's a site of: "p25:<wacn>.<sysid>", "smartnet:<sysid>",
    /// "group:<name>" (the config's site group). None until the control
    /// channel says, and for conventional systems.
    pub system: Option<String>,
    /// (RFSS, site), once the control channel has said.
    pub site: Option<(u32, u32)>,
    /// Unix seconds.
    pub t: i64,
}

impl Heard {
    pub fn new(engine: &Engine, system: u16, short_name: &str, t: i64) -> Heard {
        let key = engine.site_key(system);
        Heard { short_name: short_name.to_string(), system: key.as_ref().map(|k| k.group.clone()), site: key.and_then(|k| k.site), t }
    }
}

/// A control channel message about one radio.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UnitEvent {
    /// Registered: the radio came on.
    On,
    /// Deregistered: it went off.
    Off,
    /// Affiliated with a talkgroup.
    Join,
    /// Location registration (carries a talkgroup).
    Location,
    /// A data channel grant.
    Data,
    /// An acknowledgement or call alert about it.
    Ack,
}

impl UnitEvent {
    pub fn of(kind: MessageType) -> Option<UnitEvent> {
        Some(match kind {
            MessageType::Registration => UnitEvent::On,
            MessageType::Deregistration => UnitEvent::Off,
            MessageType::Affiliation => UnitEvent::Join,
            MessageType::Location => UnitEvent::Location,
            MessageType::DataGrant => UnitEvent::Data,
            MessageType::Acknowledge | MessageType::CallAlert => UnitEvent::Ack,
            _ => return None,
        })
    }

    pub fn as_str(self) -> &'static str {
        match self {
            UnitEvent::On => "on",
            UnitEvent::Off => "off",
            UnitEvent::Join => "join",
            UnitEvent::Location => "location",
            UnitEvent::Data => "data",
            UnitEvent::Ack => "ack",
        }
    }

    pub fn from_name(s: &str) -> Option<UnitEvent> {
        [UnitEvent::On, UnitEvent::Off, UnitEvent::Join, UnitEvent::Location, UnitEvent::Data, UnitEvent::Ack].into_iter().find(|e| e.as_str() == s)
    }

    /// Whether its talkgroup links the radio to a talkgroup (an ack's or
    /// alert's "talkgroup" is another radio, or nothing).
    pub fn links(self) -> bool {
        matches!(self, UnitEvent::Join | UnitEvent::Location)
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum RadioRecord {
    /// A call ended, recorded or not. `units`: each radio heard on it and
    /// when, seconds into the call; the first is the one granted the call.
    Call {
        at: Heard,
        tg: u32,
        unit_to_unit: bool,
        freq_hz: u64,
        /// Unix seconds it began.
        start: i64,
        secs: f32,
        encrypted: bool,
        emergency: bool,
        recorded: bool,
        /// Why it wasn't recorded ("encrypted", "no_source", …), or "".
        reason: String,
        units: Vec<(u32, f32)>,
    },
    Unit { at: Heard, kind: UnitEvent, unit: u32, tg: Option<u32> },
    /// A recorded call's file: how its frequency decoded.
    Voice { at: Heard, freq_hz: u64, frames: u64, errors: u64, bad: u64, snr: Option<f64>, freq_error: Option<f64>, clean: Option<f64> },
    /// A radio's talker alias, heard over the air.
    Alias { at: Heard, unit: u32, alias: String, tg: u32 },
}

impl RadioRecord {
    /// A call's record, at its end (`t`: Unix s).
    pub fn call(at: Heard, c: &Call) -> RadioRecord {
        let secs = (c.last_update_s - c.start_s).max(0.0) as f32;
        let start = at.t - secs.round() as i64;
        RadioRecord::Call {
            at,
            tg: c.talkgroup,
            unit_to_unit: c.unit_to_unit,
            freq_hz: c.freq_hz,
            start,
            secs,
            encrypted: c.encrypted,
            emergency: c.emergency,
            recorded: c.recording,
            reason: c.reason.map_or(String::new(), |r| r.as_str().to_string()),
            units: c.sources.iter().filter(|s| s.src != 0).map(|s| (s.src, (s.time_s - c.start_s).max(0.0) as f32)).collect(),
        }
    }

    /// A control channel message's record, if it's about one radio.
    pub fn message(at: Heard, m: &Message) -> Option<RadioRecord> {
        let kind = UnitEvent::of(m.kind)?;
        if m.source <= 0 {
            return None;
        }
        let tg = (kind.links() && m.talkgroup != 0).then_some(m.talkgroup);
        Some(RadioRecord::Unit { at, kind, unit: m.source as u32, tg })
    }
}

/// Keeps what the registry hears. Called on the session's thread: it must
/// not block (hand the record to another thread).
pub trait RadioSink: Send + Sync {
    fn record(&self, r: RadioRecord);
}
