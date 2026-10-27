//! AIS deltas received from the Kytrix service.
//!
//! The wire carries one incremental push per tick: a keyframe replaces the whole picture,
//! and every other push is applied only when the client already holds `base`. These are the
//! domain types a widget holds between pushes; the proto envelope stays at the protocol
//! boundary, and the only exception is the area a subscription is keyed by.

use kytrix_proto::types::StrategicMaritimeChokepointsTrackingDelta;
use smol_str::SmolStr;

/// The area one AIS subscription covers.
pub type GlobalShippingChokepointArea =
    kytrix_proto::types::StrategicMaritimeChokepointsTrackingArea;

/// One AIS push for an area.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GlobalShippingChokepointUpdate {
    /// The API process the delta was produced by. A new epoch means every sequence the
    /// client holds belongs to a process that no longer exists.
    pub epoch: u64,
    /// Whether `statics` and `positions` are the whole picture.
    pub keyframe: bool,
    /// The sequence this push assigns.
    pub sequence: u64,
    /// The sequence the server expects the client to have applied before this push.
    pub base: u64,
    /// The static part of every vessel the push describes.
    pub statics: Vec<GlobalShippingChokepointStatic>,
    /// The positions the push describes.
    pub positions: Vec<GlobalShippingChokepointPosition>,
    /// The MMSIs that left the area.
    pub removed: Vec<u32>,
    /// The server's generation time in milliseconds; `0` means it holds no state yet.
    pub timestamp: i64,
}

/// The low-frequency part of one vessel.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GlobalShippingChokepointStatic {
    pub mmsi: u32,
    pub imo: u32,
    pub call_sign: SmolStr,
    pub name: SmolStr,
    pub destination: SmolStr,
    pub ship_type: u32,
    pub flag_mid: u32,
    pub draught: u32,
    pub dim_to_bow: u32,
    pub dim_to_stern: u32,
    pub dim_to_port: u32,
    pub dim_to_starboard: u32,
    pub eta_month: u32,
    pub eta_day: u32,
    pub eta_hour: u32,
    pub eta_minute: u32,
}

/// The high-frequency part of one vessel.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct GlobalShippingChokepointPosition {
    pub mmsi: u32,
    /// Latitude in 1e-6 degrees.
    pub latitude: i32,
    /// Longitude in 1e-6 degrees.
    pub longitude: i32,
    /// Speed over ground in 0.1 knots; `1023` is unavailable.
    pub sog: u32,
    /// Course over ground in 0.1 degrees; `3600` is unavailable.
    pub cog: u32,
    /// True heading in degrees; `511` is unavailable.
    pub heading: u32,
    /// AIS navigation status; `15` is undefined.
    pub nav_status: u32,
    /// The server's receipt time of this position report in milliseconds.
    pub last_update: i64,
}

impl GlobalShippingChokepointUpdate {
    /// Converts one proto delta into the domain update.
    pub fn from_proto(delta: StrategicMaritimeChokepointsTrackingDelta) -> Self {
        Self {
            epoch: delta.epoch,
            keyframe: delta.keyframe,
            sequence: delta.sequence,
            base: delta.base,
            statics: delta
                .statics
                .into_iter()
                .map(|vessel| GlobalShippingChokepointStatic {
                    mmsi: vessel.mmsi,
                    imo: vessel.imo,
                    call_sign: SmolStr::new(vessel.call_sign),
                    name: SmolStr::new(vessel.name),
                    destination: SmolStr::new(vessel.destination),
                    ship_type: vessel.ship_type,
                    flag_mid: vessel.flag_mid,
                    draught: vessel.draught,
                    dim_to_bow: vessel.dim_to_bow,
                    dim_to_stern: vessel.dim_to_stern,
                    dim_to_port: vessel.dim_to_port,
                    dim_to_starboard: vessel.dim_to_starboard,
                    eta_month: vessel.eta_month,
                    eta_day: vessel.eta_day,
                    eta_hour: vessel.eta_hour,
                    eta_minute: vessel.eta_minute,
                })
                .collect(),
            positions: delta
                .positions
                .into_iter()
                .map(|position| GlobalShippingChokepointPosition {
                    mmsi: position.mmsi,
                    latitude: position.latitude,
                    longitude: position.longitude,
                    sog: position.sog,
                    cog: position.cog,
                    heading: position.heading,
                    nav_status: position.nav_status,
                    last_update: position.last_update,
                })
                .collect(),
            removed: delta.removed,
            timestamp: delta.timestamp,
        }
    }
}
