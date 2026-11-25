//! A table of the objects of one watch (design §4.1): what the hub and the gateway reconciler
//! hold of each kind, with the relist rule of `kube::runtime::watcher`.

use std::collections::BTreeMap;

use super::convert::Key;
use super::hub::Event;

pub(crate) struct Table<V> {
    pub(crate) items: BTreeMap<Key, V>,
    /// The objects of a relist that is not complete: the old content is served meanwhile.
    staging: Option<BTreeMap<Key, V>>,
    /// A list was complete once.
    pub(crate) synced: bool,
}

impl<V> Default for Table<V> {
    fn default() -> Table<V> {
        Table {
            items: BTreeMap::new(),
            staging: None,
            synced: false,
        }
    }
}

impl<V: PartialEq> Table<V> {
    /// Applies an event; false when it changed nothing a view depends on (a watch event that
    /// only moved a `resourceVersion`, such as the echo of this controller's own status apply).
    pub(crate) fn feed(&mut self, event: Event<V>) -> bool {
        match event {
            Event::Init => {
                self.staging = Some(BTreeMap::new());
                false
            }
            Event::InitApply(key, value) => match &mut self.staging {
                Some(staging) => {
                    staging.insert(key, value);
                    false
                }
                None => {
                    self.items.insert(key, value);
                    true
                }
            },
            Event::InitDone => {
                let first = !self.synced;
                self.synced = true;
                match self.staging.take() {
                    Some(staging) if staging != self.items => {
                        self.items = staging;
                        true
                    }
                    _ => first,
                }
            }
            Event::Apply(key, value) => {
                let same = self.items.get(&key) == Some(&value);
                if !same {
                    self.items.insert(key, value);
                }
                !same
            }
            Event::Delete(key) => self.items.remove(&key).is_some(),
        }
    }
}
