//! Pure, append-only pi 1.4.0 session trees (design §8.11).
//!
//! File order restores the last entry as the leaf; navigation alone is ephemeral.
//! Serialization also carries the current leaf for API/snapshot use. No execution
//! ledger, persistence, tool execution, or provider request repair lives here.

mod entry;
mod projection;

pub use entry::{EntryKind, SessionEntry, SessionHeader};
pub use projection::SessionProjection;

use std::collections::{HashMap, HashSet};

use serde::{Deserialize, Deserializer, Serialize};

#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionTree {
    entries: Vec<SessionEntry>,
    leaf_id: Option<String>,
    #[serde(skip)]
    by_id: HashMap<String, usize>,
}

impl<'de> Deserialize<'de> for SessionTree {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct Snapshot {
            entries: Vec<SessionEntry>,
            leaf_id: Option<String>,
        }
        let snapshot = Snapshot::deserialize(deserializer)?;
        let mut tree = Self::from_entries(snapshot.entries).map_err(serde::de::Error::custom)?;
        tree.branch(snapshot.leaf_id.as_deref())
            .map_err(serde::de::Error::custom)?;
        Ok(tree)
    }
}

impl SessionTree {
    pub fn new() -> Self {
        Self::default()
    }

    /// Restore append order, including its last entry as the current position.
    /// Parents must precede their children, as in an append-only session file.
    pub fn from_entries(entries: Vec<SessionEntry>) -> Result<Self, String> {
        let mut tree = Self::new();
        for entry in entries {
            tree.append(entry)?;
        }
        Ok(tree)
    }

    pub fn entries(&self) -> &[SessionEntry] {
        &self.entries
    }

    pub fn leaf_id(&self) -> Option<&str> {
        self.leaf_id.as_deref()
    }

    pub fn get(&self, id: &str) -> Option<&SessionEntry> {
        self.by_id.get(id).and_then(|&i| self.entries.get(i))
    }

    /// The caller supplies the parent explicitly; append never rewrites it.
    /// Rejected appends leave both the entries and the leaf unchanged.
    pub fn append(&mut self, entry: SessionEntry) -> Result<(), String> {
        if self.by_id.contains_key(&entry.id) {
            return Err(format!("Duplicate session entry {}", entry.id));
        }
        if let Some(parent) = &entry.parent_id {
            if !self.by_id.contains_key(parent) {
                return Err(format!("Parent entry {parent} not found for {}", entry.id));
            }
        }
        self.by_id.insert(entry.id.clone(), self.entries.len());
        self.leaf_id = Some(entry.id.clone());
        self.entries.push(entry);
        Ok(())
    }

    /// Move the current position, or reset before all roots with `None`.
    pub fn branch(&mut self, target: Option<&str>) -> Result<(), String> {
        if let Some(id) = target {
            if self.get(id).is_none() {
                return Err(format!("Entry {id} not found"));
            }
        }
        self.leaf_id = target.map(str::to_owned);
        Ok(())
    }

    /// Root-to-target path. `None` explicitly denotes the empty path.
    pub fn path(&self, target: Option<&str>) -> Result<Vec<&SessionEntry>, String> {
        let mut path = Vec::new();
        let mut visited = HashSet::new();
        let mut current = target;
        while let Some(id) = current {
            if !visited.insert(id) {
                return Err(format!("Cycle in session path at {id}"));
            }
            let entry = self
                .get(id)
                .ok_or_else(|| format!("Entry {id} not found"))?;
            path.push(entry);
            current = entry.parent_id.as_deref();
        }
        path.reverse();
        Ok(path)
    }

    pub fn children(&self, parent: Option<&str>) -> Vec<&SessionEntry> {
        self.entries
            .iter()
            .filter(|entry| entry.parent_id.as_deref() == parent)
            .collect()
    }

    /// Labels, unlike context edits, are session-wide and resolved in file order.
    pub fn label(&self, target: &str) -> Option<&str> {
        self.entries
            .iter()
            .rev()
            .find_map(|entry| match &entry.kind {
                EntryKind::Label { target_id, label } if target_id == target => {
                    Some(label.as_deref().filter(|label| !label.is_empty()))
                }
                _ => None,
            })?
    }

    /// The latest session-info entry wins, even on an inactive branch.
    pub fn name(&self) -> Option<&str> {
        self.entries
            .iter()
            .rev()
            .find_map(|entry| match &entry.kind {
                EntryKind::SessionInfo { name } => {
                    Some((!name.trim().is_empty()).then(|| name.trim()))
                }
                _ => None,
            })?
    }

    /// Extract a self-contained path for fork/clone, retaining entry IDs.
    /// Labels are rebuilt from the globally resolved labels on retained targets.
    /// New label IDs are deterministic and collision-free, requiring no RNG/I/O.
    pub fn extract(&self, target: &str) -> Result<Vec<SessionEntry>, String> {
        let path = self.path(Some(target))?;
        let mut extracted = Vec::new();
        let mut replacement_by_label: HashMap<&str, &str> = HashMap::new();
        let mut pending_labels = Vec::new();
        let mut parent = None;
        for entry in path {
            if matches!(entry.kind, EntryKind::Label { .. }) {
                pending_labels.push(entry.id.as_str());
                continue;
            }
            for label_id in pending_labels.drain(..) {
                replacement_by_label.insert(label_id, &entry.id);
            }
            let mut copy = entry.clone();
            copy.parent_id = parent;
            if let EntryKind::Compaction {
                first_kept_entry_id,
                ..
            } = &mut copy.kind
            {
                if first_kept_entry_id != &copy.id {
                    if let Some(replacement) =
                        replacement_by_label.get(first_kept_entry_id.as_str())
                    {
                        *first_kept_entry_id = (*replacement).to_owned();
                    }
                }
            }
            parent = Some(copy.id.clone());
            extracted.push(copy);
        }
        let retained: HashSet<_> = extracted.iter().map(|entry| entry.id.clone()).collect();
        for label_entry in self.resolved_labels() {
            let EntryKind::Label { target_id, .. } = &label_entry.kind else {
                continue;
            };
            if !retained.contains(target_id) {
                continue;
            }
            let mut copy = label_entry.clone();
            let base = format!("{}-fork", copy.id);
            copy.id = base.clone();
            let mut suffix = 0;
            while self.by_id.contains_key(&copy.id)
                || extracted.iter().any(|entry| entry.id == copy.id)
            {
                suffix += 1;
                copy.id = format!("{base}-{suffix}");
            }
            copy.parent_id = parent;
            parent = Some(copy.id.clone());
            extracted.push(copy);
        }
        Ok(extracted)
    }

    /// Preserve JavaScript Map insertion order when a label is updated/cleared.
    fn resolved_labels(&self) -> Vec<&SessionEntry> {
        let mut slots = Vec::new();
        let mut by_target = HashMap::new();
        for entry in &self.entries {
            if let EntryKind::Label { target_id, label } = &entry.kind {
                if label.as_ref().is_some_and(|label| !label.is_empty()) {
                    if let Some(&slot) = by_target.get(target_id) {
                        slots[slot] = Some(entry);
                    } else {
                        by_target.insert(target_id, slots.len());
                        slots.push(Some(entry));
                    }
                } else if let Some(slot) = by_target.remove(target_id) {
                    slots[slot] = None;
                }
            }
        }
        slots.into_iter().flatten().collect()
    }
}

#[cfg(test)]
mod tests;
