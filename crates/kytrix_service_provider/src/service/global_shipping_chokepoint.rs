use ahash::AHashMap;
use kytrix_proto::types::v1::GlobalShippingChokepointEvent;
use smol_str::SmolStr;

pub type GlobalShippingChokepointArea = kytrix_proto::types::v1::GlobalShippingChokepointArea;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GlobalShippingChokepointUpdate {
    pub epoch: u64,
    pub keyframe: bool,
    pub sequence: u64,
    pub base: u64,
    pub ships: Vec<GlobalShippingChokepointShip>,
    pub positions: Vec<GlobalShippingChokepointPosition>,
    pub removed: Vec<u32>,
    pub timestamp: i64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GlobalShippingChokepointShip {
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
    pub static_updated_at: i64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct GlobalShippingChokepointPosition {
    pub mmsi: u32,
    pub latitude: i32,
    pub longitude: i32,
    pub sog: u32,
    pub cog: u32,
    pub heading: u32,
    pub nav_status: u32,
    pub last_update: i64,
}

impl GlobalShippingChokepointUpdate {
    pub fn from_proto(delta: GlobalShippingChokepointEvent) -> Self {
        Self {
            epoch: delta.epoch,
            keyframe: delta.keyframe,
            sequence: delta.sequence,
            base: delta.base,
            ships: delta
                .ships
                .into_iter()
                .map(|vessel| GlobalShippingChokepointShip {
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
                    static_updated_at: vessel.static_updated_at,
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

#[derive(Debug, Default)]
pub(crate) struct GlobalShippingChokepointState {
    epoch: u64,
    sequence: u64,
    timestamp: i64,
    ships: AHashMap<u32, GlobalShippingChokepointShip>,
    positions: AHashMap<u32, GlobalShippingChokepointPosition>,
}

#[derive(Debug)]
pub(crate) struct ResyncGap;

impl GlobalShippingChokepointState {
    pub(crate) fn apply(
        &mut self,
        update: &GlobalShippingChokepointUpdate,
    ) -> Result<(), ResyncGap> {
        if update.keyframe {
            self.ships.clear();
            self.positions.clear();
            self.epoch = update.epoch;
            self.sequence = update.sequence;
            self.timestamp = update.timestamp;
            if update.timestamp == 0 {
                return Ok(());
            }
            self.ships
                .extend(update.ships.iter().map(|entry| (entry.mmsi, entry.clone())));
            self.positions
                .extend(update.positions.iter().map(|entry| (entry.mmsi, *entry)));
            return Ok(());
        }

        if update.epoch != self.epoch || update.base != self.sequence {
            return Err(ResyncGap);
        }
        for entry in &update.ships {
            self.ships.insert(entry.mmsi, entry.clone());
        }
        for entry in &update.positions {
            self.positions.insert(entry.mmsi, *entry);
        }
        for mmsi in &update.removed {
            self.ships.remove(mmsi);
            self.positions.remove(mmsi);
        }
        self.sequence = update.sequence;
        self.timestamp = update.timestamp;
        Ok(())
    }

    pub(crate) fn snapshot(&self) -> GlobalShippingChokepointUpdate {
        let mut ships = self.ships.values().cloned().collect::<Vec<_>>();
        ships.sort_unstable_by_key(|entry| entry.mmsi);
        let mut positions = self.positions.values().copied().collect::<Vec<_>>();
        positions.sort_unstable_by_key(|entry| entry.mmsi);
        GlobalShippingChokepointUpdate {
            epoch: self.epoch,
            keyframe: true,
            sequence: self.sequence,
            base: 0,
            ships,
            positions,
            removed: Vec::new(),
            timestamp: self.timestamp,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const EPOCH: u64 = 7;

    fn ship(mmsi: u32) -> GlobalShippingChokepointShip {
        GlobalShippingChokepointShip {
            mmsi,
            imo: 0,
            call_sign: SmolStr::default(),
            name: SmolStr::new_static("vessel"),
            destination: SmolStr::default(),
            ship_type: 0,
            flag_mid: 0,
            draught: 0,
            dim_to_bow: 0,
            dim_to_stern: 0,
            dim_to_port: 0,
            dim_to_starboard: 0,
            eta_month: 0,
            eta_day: 0,
            eta_hour: 0,
            eta_minute: 0,
            static_updated_at: 0,
        }
    }

    fn position(mmsi: u32, latitude: i32) -> GlobalShippingChokepointPosition {
        GlobalShippingChokepointPosition {
            mmsi,
            latitude,
            longitude: 0,
            sog: 0,
            cog: 0,
            heading: 0,
            nav_status: 0,
            last_update: 1,
        }
    }

    fn keyframe(
        sequence: u64,
        ships: Vec<GlobalShippingChokepointShip>,
        positions: Vec<GlobalShippingChokepointPosition>,
    ) -> GlobalShippingChokepointUpdate {
        GlobalShippingChokepointUpdate {
            epoch: EPOCH,
            keyframe: true,
            sequence,
            base: 0,
            ships,
            positions,
            removed: Vec::new(),
            timestamp: 1_700_000_000_000,
        }
    }

    fn delta(
        base: u64,
        sequence: u64,
        positions: Vec<GlobalShippingChokepointPosition>,
    ) -> GlobalShippingChokepointUpdate {
        GlobalShippingChokepointUpdate {
            epoch: EPOCH,
            keyframe: false,
            sequence,
            base,
            ships: Vec::new(),
            positions,
            removed: Vec::new(),
            timestamp: 1_700_000_000_000,
        }
    }

    #[test]
    fn a_snapshot_replaces_the_picture_and_deltas_continue_it() {
        let mut state = GlobalShippingChokepointState::default();
        state
            .apply(&keyframe(1, vec![ship(1)], vec![position(1, 10)]))
            .unwrap();
        state.apply(&delta(1, 2, vec![position(1, 20)])).unwrap();

        let snapshot = state.snapshot();
        assert_eq!(snapshot.sequence, 2);
        assert_eq!(snapshot.ships, vec![ship(1)]);
        assert_eq!(snapshot.positions, vec![position(1, 20)]);
        assert!(snapshot.keyframe);
    }

    #[test]
    fn a_delta_removes_a_vessel_from_both_tables() {
        let mut state = GlobalShippingChokepointState::default();
        state
            .apply(&keyframe(1, vec![ship(1)], vec![position(1, 10)]))
            .unwrap();

        let mut removed = delta(1, 2, Vec::new());
        removed.removed = vec![1];
        state.apply(&removed).unwrap();

        let snapshot = state.snapshot();
        assert!(snapshot.ships.is_empty());
        assert!(snapshot.positions.is_empty());
        assert_eq!(snapshot.sequence, 2);
    }

    #[test]
    fn a_delta_that_does_not_continue_the_picture_is_a_gap() {
        let mut state = GlobalShippingChokepointState::default();
        state
            .apply(&keyframe(5, vec![ship(1)], vec![position(1, 10)]))
            .unwrap();

        assert!(state.apply(&delta(4, 5, Vec::new())).is_err());
        assert_eq!(state.snapshot().sequence, 5);
        assert_eq!(state.snapshot().positions, vec![position(1, 10)]);
    }

    #[test]
    fn a_no_state_snapshot_clears_the_picture_and_accepts_the_first_delta() {
        let mut state = GlobalShippingChokepointState::default();
        state
            .apply(&keyframe(3, vec![ship(1)], vec![position(1, 10)]))
            .unwrap();

        let mut no_state = keyframe(0, Vec::new(), Vec::new());
        no_state.timestamp = 0;
        state.apply(&no_state).unwrap();
        assert!(state.snapshot().ships.is_empty());
        assert_eq!(state.snapshot().timestamp, 0);

        state.apply(&delta(0, 1, vec![position(2, 30)])).unwrap();
        let snapshot = state.snapshot();
        assert_eq!(snapshot.sequence, 1);
        assert_eq!(snapshot.positions, vec![position(2, 30)]);
    }

    #[test]
    fn a_snapshot_is_sorted_by_mmsi() {
        let mut state = GlobalShippingChokepointState::default();
        state
            .apply(&keyframe(
                1,
                vec![ship(3), ship(1), ship(2)],
                vec![position(3, 0), position(1, 0), position(2, 0)],
            ))
            .unwrap();

        let snapshot = state.snapshot();
        assert_eq!(
            snapshot
                .ships
                .iter()
                .map(|entry| entry.mmsi)
                .collect::<Vec<_>>(),
            vec![1, 2, 3]
        );
        assert_eq!(
            snapshot
                .positions
                .iter()
                .map(|entry| entry.mmsi)
                .collect::<Vec<_>>(),
            vec![1, 2, 3]
        );
    }
}
