use super::*;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

pub(super) const MAX_PRESSES: usize = 4096;
pub(super) const NODE_BYTES: usize = 512;
pub(super) const MAX_BYTES: usize = 8 * 1024 * 1024;
pub(super) const MAX_NODES: usize = MAX_BYTES / NODE_BYTES;

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub(crate) struct Press {
    pub source: Source,
    pub position: InputPosition,
    pub code: u16,
}
pub(super) struct Budget {
    used: AtomicUsize,
    limit: usize,
}
impl Default for Budget {
    fn default() -> Self {
        Self {
            used: AtomicUsize::new(0),
            limit: MAX_NODES * NODE_BYTES,
        }
    }
}
impl Budget {
    fn acquire(&self, bytes: usize) -> Result<(), &'static str> {
        self.used
            .fetch_update(Ordering::AcqRel, Ordering::Relaxed, |used| {
                used.checked_add(bytes).filter(|next| *next <= self.limit)
            })
            .map(|_| ())
            .map_err(|_| "key press provenance budget exhausted")
    }
    fn reserve(self: &Arc<Self>, bytes: usize) -> Result<Reservation, &'static str> {
        self.acquire(bytes)?;
        Ok(Reservation {
            budget: self.clone(),
            bytes,
        })
    }
}
pub(super) struct Reservation {
    budget: Arc<Budget>,
    bytes: usize,
}
impl Drop for Reservation {
    fn drop(&mut self) {
        self.budget.used.fetch_sub(self.bytes, Ordering::AcqRel);
    }
}

#[derive(Clone)]
pub(super) struct Set {
    budget: Arc<Budget>,
    root: Option<Arc<Node>>,
    count: usize,
    latest: Option<InputPosition>,
}
enum Links {
    Press {
        previous: Option<Arc<Node>>,
        press: Press,
    },
    Union {
        left: Arc<Node>,
        right: Arc<Node>,
    },
}
struct Node {
    charge: usize,
    budget: Arc<Budget>,
    links: Option<Links>,
}
impl Node {
    fn children(&mut self, pending: &mut Vec<Arc<Node>>) {
        match self.links.take() {
            Some(Links::Press {
                previous: Some(previous),
                ..
            }) => pending.push(previous),
            Some(Links::Union { left, right }) => {
                pending.push(left);
                pending.push(right);
            }
            _ => {}
        }
    }
}
impl Drop for Node {
    fn drop(&mut self) {
        let mut pending = Vec::new();
        self.children(&mut pending);
        while let Some(node) = pending.pop() {
            if let Ok(mut node) = Arc::try_unwrap(node) {
                node.children(&mut pending);
            }
        }
        self.budget.used.fetch_sub(self.charge, Ordering::AcqRel);
    }
}
pub(super) struct Iter<'a> {
    pending: Vec<&'a Node>,
    seen: BTreeSet<*const Node>,
    yielded: BTreeSet<InputPosition>,
}
impl<'a> Iterator for Iter<'a> {
    type Item = &'a Press;
    fn next(&mut self) -> Option<Self::Item> {
        while let Some(node) = self.pending.pop() {
            if !self.seen.insert(std::ptr::from_ref(node)) {
                continue;
            }
            match node.links.as_ref()? {
                Links::Press { previous, press } => {
                    if let Some(previous) = previous {
                        self.pending.push(previous);
                    }
                    if self.yielded.insert(press.position) {
                        return Some(press);
                    }
                }
                Links::Union { left, right } => {
                    self.pending.push(left);
                    self.pending.push(right);
                }
            }
        }
        None
    }
}
impl Set {
    pub(super) fn reserve(&self, bytes: usize) -> Result<Reservation, &'static str> {
        self.budget.reserve(bytes)
    }
    pub(super) fn empty(budget: Arc<Budget>) -> Self {
        Self {
            budget,
            root: None,
            count: 0,
            latest: None,
        }
    }
    pub(super) fn iter(&self) -> Iter<'_> {
        Iter {
            pending: self.root.as_deref().into_iter().collect(),
            seen: BTreeSet::new(),
            yielded: BTreeSet::new(),
        }
    }
    pub(super) fn empty_like(&self) -> Self {
        Self::empty(self.budget.clone())
    }
    pub(super) fn contains(&self, source: &Source, code: u16, position: InputPosition) -> bool {
        self.iter().any(|press| {
            press.source == *source && press.code == code && press.position == position
        })
    }
    pub(super) fn select(&self, mut presses: Vec<Press>) -> Result<Self, &'static str> {
        if presses.len() > MAX_PRESSES {
            return Err("key press provenance member limit");
        }
        presses.sort_by_key(|press| press.position);
        if presses
            .windows(2)
            .any(|pair| pair[0].position == pair[1].position)
        {
            return Err("conflicting original key press position");
        }
        if self.count == presses.len()
            && presses
                .iter()
                .all(|press| self.contains(&press.source, press.code, press.position))
        {
            return Ok(self.clone());
        }
        let mut selected = Self::empty(self.budget.clone());
        for press in presses {
            selected.insert(press)?;
        }
        Ok(selected)
    }
    pub(super) fn insert(&mut self, press: Press) -> Result<(), &'static str> {
        if press.position.frame == 0
            || press.position.event >= 4096
            || press.source.generation == 0
            || press.source.seat.is_empty()
            || press.code > 0x2ff
        {
            return Err("invalid original key press position");
        }
        if self.latest.is_some_and(|last| press.position <= last) {
            return if self.contains(&press.source, press.code, press.position) {
                Ok(())
            } else {
                Err("key press provenance moved backwards")
            };
        }
        if self.count >= MAX_PRESSES {
            return Err("key press provenance member limit");
        }
        let charge = NODE_BYTES
            .checked_add(press.source.seat.len())
            .ok_or("key press provenance byte overflow")?;
        self.budget.acquire(charge)?;
        let position = press.position;
        self.root = Some(Arc::new(Node {
            charge,
            budget: self.budget.clone(),
            links: Some(Links::Press {
                previous: self.root.take(),
                press,
            }),
        }));
        self.count += 1;
        self.latest = Some(position);
        Ok(())
    }
    pub(super) fn merge(&mut self, other: &Self) -> Result<(), &'static str> {
        if !Arc::ptr_eq(&self.budget, &other.budget) {
            return Err("foreign key press provenance budget");
        }
        let Some(right) = &other.root else {
            return Ok(());
        };
        let Some(left) = &self.root else {
            *self = other.clone();
            return Ok(());
        };
        if Arc::ptr_eq(left, right) {
            return Ok(());
        }
        let count = {
            let entries: BTreeSet<_> = self.iter().chain(other.iter()).collect();
            if entries.len() > MAX_PRESSES {
                return Err("key press provenance member limit");
            }
            let mut positions = BTreeSet::new();
            if entries
                .iter()
                .any(|press| !positions.insert(press.position))
            {
                return Err("conflicting original key press position");
            }
            entries.len()
        };
        if count == self.count {
            return Ok(());
        }
        if count == other.count {
            *self = other.clone();
            return Ok(());
        }
        self.budget.acquire(NODE_BYTES)?;
        let links = Links::Union {
            left: left.clone(),
            right: right.clone(),
        };
        self.root = Some(Arc::new(Node {
            charge: NODE_BYTES,
            budget: self.budget.clone(),
            links: Some(links),
        }));
        self.count = count;
        self.latest = self.latest.max(other.latest);
        Ok(())
    }
}

pub(super) fn record(
    worker: &mut Worker,
    owner: &Source,
    press: Press,
    at: Duration,
    events: &mut Vec<Event>,
) -> bool {
    if !worker.routing {
        return true;
    }
    let failure = worker
        .candidates
        .get_mut(owner)
        .and_then(|candidate| candidate.key_presses.insert(press).err());
    if let Some(reason) = failure {
        cancel_candidate(worker, owner, at, reason, events);
        false
    } else {
        true
    }
}

#[cfg(test)]
mod tests;
