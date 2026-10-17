use prek::input::{Contact, ContactId, Device, Frame, Normalizer};
use std::{collections::BTreeMap, time::Duration};

#[derive(Debug, PartialEq)]
pub(super) struct Packet {
    pub operation: u32,
    pub time: u32,
    pub id: i32,
    pub x: f64,
    pub y: f64,
}
impl Packet {
    pub fn bytes(&self) -> [u8; 32] {
        let mut bytes = [0; 32];
        bytes[0..4].copy_from_slice(&1u32.to_ne_bytes());
        bytes[4..8].copy_from_slice(&self.operation.to_ne_bytes());
        bytes[8..12].copy_from_slice(&self.time.to_ne_bytes());
        bytes[12..16].copy_from_slice(&self.id.to_ne_bytes());
        bytes[16..24].copy_from_slice(&self.x.to_ne_bytes());
        bytes[24..32].copy_from_slice(&self.y.to_ne_bytes());
        bytes
    }
}
pub(super) struct Bridge {
    normalizer: Normalizer,
    contacts: BTreeMap<ContactId, Contact>,
    epoch: Duration,
}
impl Bridge {
    pub fn new(device: &Device, epoch: Duration) -> Self {
        Self {
            normalizer: Normalizer::new(device),
            contacts: BTreeMap::new(),
            epoch,
        }
    }
    pub fn encode(&mut self, frame: Frame) -> Vec<Packet> {
        let normalized = self.normalizer.process(frame).unwrap();
        let time = u32::try_from(
            normalized
                .raw
                .at
                .checked_sub(self.epoch)
                .unwrap()
                .as_millis(),
        )
        .unwrap()
        .checked_add(1000)
        .unwrap();
        let mut packets = Vec::new();
        for id in &normalized.contacts_ended {
            let old = self.contacts.remove(id).unwrap();
            let last = normalized
                .ended_contacts
                .iter()
                .find(|contact| contact.id == *id)
                .unwrap();
            if old.x != last.x || old.y != last.y {
                packets.push(Packet {
                    operation: 2,
                    time,
                    id: i32::from(id.slot),
                    x: f64::from(last.x) / 1000.0,
                    y: f64::from(last.y) / 1000.0,
                });
            }
            packets.push(Packet {
                operation: 3,
                time,
                id: i32::from(id.slot),
                x: 0.0,
                y: 0.0,
            });
        }
        for contact in normalized.contacts {
            assert!(contact.id.slot < 32);
            let operation = match self.contacts.get(&contact.id) {
                None => 1,
                Some(old) if old.x != contact.x || old.y != contact.y => 2,
                Some(_) => continue,
            };
            let x = f64::from(contact.x) / 1000.0;
            let y = f64::from(contact.y) / 1000.0;
            assert!((0.0..=1.0).contains(&x) && (0.0..=1.0).contains(&y));
            packets.push(Packet {
                operation,
                time,
                id: i32::from(contact.id.slot),
                x,
                y,
            });
            self.contacts.insert(contact.id, contact);
        }
        packets.push(Packet {
            operation: 4,
            time,
            id: 0,
            x: 0.0,
            y: 0.0,
        });
        packets
    }
    pub fn neutral(&self) -> bool {
        self.contacts.is_empty()
    }
    pub fn epoch(&self) -> Duration {
        self.epoch
    }
}

#[test]
fn touch_bridge_preserves_frames_cadence_identity_and_unchanged_contacts() {
    let device = super::device();
    let mut bridge = Bridge::new(&device, Duration::ZERO);
    let first = super::frame(&device, 1, Duration::from_millis(10), &super::down(2));
    let packets = bridge.encode(first);
    assert_eq!(
        packets
            .iter()
            .map(|p| (p.operation, p.id, p.time))
            .collect::<Vec<_>>(),
        vec![(1, 0, 1010), (1, 1, 1010), (4, 0, 1010)]
    );
    assert_eq!(packets[0].x, 0.25);
    assert_eq!(packets[1].x, 0.75);
    assert_eq!(packets[0].bytes().len(), 32);
    assert_eq!(packets[0].bytes()[4..8], 1u32.to_ne_bytes());
    let unchanged = bridge.encode(super::frame(&device, 2, Duration::from_millis(20), &[]));
    assert_eq!(unchanged.len(), 1);
    assert_eq!(unchanged[0].operation, 4);
    let moved = bridge.encode(super::frame(
        &device,
        3,
        Duration::from_millis(35),
        &super::motion(2, 500),
    ));
    assert_eq!(
        moved
            .iter()
            .map(|p| (p.operation, p.id, p.y, p.time))
            .collect::<Vec<_>>(),
        vec![(2, 0, 0.5, 1035), (2, 1, 0.5, 1035), (4, 0, 0.0, 1035)]
    );
    let released = bridge.encode(super::frame(
        &device,
        4,
        Duration::from_millis(60),
        &super::up(2),
    ));
    assert_eq!(
        released
            .iter()
            .map(|p| (p.operation, p.id, p.time))
            .collect::<Vec<_>>(),
        vec![(3, 0, 1060), (3, 1, 1060), (4, 0, 1060)]
    );
    assert!(bridge.neutral());
}

#[test]
fn touch_bridge_keeps_last_motion_before_same_frame_release() {
    let device = super::device();
    let mut bridge = Bridge::new(&device, Duration::ZERO);
    bridge.encode(super::frame(
        &device,
        1,
        Duration::from_millis(1),
        &super::down(1),
    ));
    let mut events = super::motion(1, 600);
    events.extend(super::up(1));
    let packets = bridge.encode(super::frame(&device, 2, Duration::from_millis(20), &events));
    assert_eq!(
        packets
            .iter()
            .map(|packet| packet.operation)
            .collect::<Vec<_>>(),
        vec![2, 3, 4]
    );
    assert_eq!(packets[0].y, 0.6);
    assert!(bridge.neutral());
}
