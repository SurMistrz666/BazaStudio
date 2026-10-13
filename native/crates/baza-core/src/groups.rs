//! Per-application encrypted connection folders. Profile records remain separate.
use crate::Vault;
use anyhow::{Result, anyhow, ensure};
use serde::{Deserialize, Serialize};
use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
};
use uuid::Uuid;

const RECORD: &str = "connection-groups";
const MAX_BYTES: usize = 256 * 1024;
pub const MAX_DEPTH: usize = 8;
pub const MAX_GROUPS: usize = 256;
pub const MAX_MEMBERSHIPS: usize = 1000;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Group {
    pub id: Uuid,
    pub name: String,
    pub color: [u8; 3],
    pub parent: Option<Uuid>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Snapshot {
    pub revision: u64,
    pub groups: Vec<Group>,
    pub memberships: HashMap<Uuid, Uuid>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Record {
    v: u8,
    revision: u64,
    groups: Vec<Group>,
    memberships: HashMap<Uuid, Uuid>,
}

impl Default for Record {
    fn default() -> Self {
        Self {
            v: 1,
            revision: 0,
            groups: Vec::new(),
            memberships: HashMap::new(),
        }
    }
}

fn name(input: &str) -> Result<String> {
    let value = input.trim();
    ensure!(
        !value.is_empty()
            && value.chars().count() <= 64
            && value.len() <= 256
            && !matches!(value, "." | "..")
            && !value
                .chars()
                .any(|c| c.is_control() || c == '/' || c == '\\'),
        "group_name_invalid"
    );
    Ok(value.to_owned())
}

fn validate(record: &Record) -> Result<()> {
    ensure!(record.v == 1, "group_record_invalid");
    ensure!(
        record.groups.len() <= MAX_GROUPS && record.memberships.len() <= MAX_MEMBERSHIPS,
        "group_count_limit"
    );
    let mut index = HashMap::with_capacity(record.groups.len());
    let mut siblings = HashSet::with_capacity(record.groups.len());
    for group in &record.groups {
        ensure!(
            !group.id.is_nil() && index.insert(group.id, group).is_none(),
            "group_identity_invalid"
        );
        ensure!(name(&group.name)? == group.name, "group_name_invalid");
        ensure!(
            siblings.insert((group.parent, group.name.to_lowercase())),
            "group_name_exists"
        );
    }
    for group in &record.groups {
        let mut current = Some(group.id);
        let mut visited = HashSet::new();
        while let Some(id) = current {
            ensure!(visited.insert(id), "group_cycle");
            ensure!(visited.len() <= MAX_DEPTH, "group_depth_limit");
            current = index
                .get(&id)
                .ok_or_else(|| anyhow!("group_parent_missing"))?
                .parent;
        }
    }
    ensure!(
        record
            .memberships
            .iter()
            .all(|(connection, group)| !connection.is_nil() && index.contains_key(group)),
        "group_membership_invalid"
    );
    Ok(())
}

fn parse(raw: Option<&[u8]>) -> Result<Record> {
    let record = match raw {
        None => Record::default(),
        Some(raw) => {
            ensure!(raw.len() <= MAX_BYTES, "group_storage_limit");
            serde_json::from_slice(raw).map_err(|_| anyhow!("group_record_invalid"))?
        }
    };
    validate(&record)?;
    Ok(record)
}

fn snapshot(record: Record) -> Snapshot {
    Snapshot {
        revision: record.revision,
        groups: record.groups,
        memberships: record.memberships,
    }
}

pub struct Groups {
    vault: Arc<Vault>,
}

impl Groups {
    pub fn open(vault: Arc<Vault>) -> Result<Self> {
        let groups = Self { vault };
        groups.load()?;
        Ok(groups)
    }

    pub fn load(&self) -> Result<Snapshot> {
        let bytes = self.vault.load_named(RECORD)?;
        Ok(snapshot(parse(bytes.as_deref().map(Vec::as_slice))?))
    }

    fn mutate(
        &self,
        expected: u64,
        mutation: impl FnOnce(&mut Record) -> Result<()>,
    ) -> Result<Snapshot> {
        self.vault.update_named(RECORD, |raw| {
            let mut record = parse(raw)?;
            ensure!(record.revision == expected, "group_revision_changed");
            mutation(&mut record)?;
            validate(&record)?;
            record.revision = record
                .revision
                .checked_add(1)
                .ok_or_else(|| anyhow!("group_revision_limit"))?;
            let bytes = serde_json::to_vec(&record).map_err(|_| anyhow!("group_record_invalid"))?;
            ensure!(bytes.len() <= MAX_BYTES, "group_storage_limit");
            Ok((bytes, snapshot(record)))
        })
    }

    pub fn create(
        &self,
        expected: u64,
        value: &str,
        color: [u8; 3],
        parent: Option<Uuid>,
    ) -> Result<Snapshot> {
        let value = name(value)?;
        self.mutate(expected, move |record| {
            record.groups.push(Group {
                id: Uuid::new_v4(),
                name: value,
                color,
                parent,
            });
            Ok(())
        })
    }

    pub fn rename(&self, expected: u64, id: Uuid, value: &str) -> Result<Snapshot> {
        let value = name(value)?;
        self.mutate(expected, move |record| {
            record
                .groups
                .iter_mut()
                .find(|g| g.id == id)
                .ok_or_else(|| anyhow!("group_missing"))?
                .name = value;
            Ok(())
        })
    }

    /// Saves one editor draft in a single authenticated CAS write. Invalid
    /// reparenting or sibling collisions never partially rename/recolor it.
    pub fn update(
        &self,
        expected: u64,
        id: Uuid,
        value: &str,
        color: [u8; 3],
        parent: Option<Uuid>,
    ) -> Result<Snapshot> {
        let value = name(value)?;
        self.mutate(expected, move |record| {
            let group = record
                .groups
                .iter_mut()
                .find(|g| g.id == id)
                .ok_or_else(|| anyhow!("group_missing"))?;
            group.name = value;
            group.color = color;
            group.parent = parent;
            Ok(())
        })
    }

    pub fn set_color(&self, expected: u64, id: Uuid, color: [u8; 3]) -> Result<Snapshot> {
        self.mutate(expected, |record| {
            record
                .groups
                .iter_mut()
                .find(|g| g.id == id)
                .ok_or_else(|| anyhow!("group_missing"))?
                .color = color;
            Ok(())
        })
    }

    pub fn move_group(&self, expected: u64, id: Uuid, parent: Option<Uuid>) -> Result<Snapshot> {
        self.mutate(expected, |record| {
            record
                .groups
                .iter_mut()
                .find(|g| g.id == id)
                .ok_or_else(|| anyhow!("group_missing"))?
                .parent = parent;
            Ok(())
        })
    }

    pub fn set_connection(
        &self,
        expected: u64,
        connection: Uuid,
        group: Option<Uuid>,
    ) -> Result<Snapshot> {
        ensure!(!connection.is_nil(), "group_membership_invalid");
        self.mutate(expected, |record| {
            if let Some(group) = group {
                record.memberships.insert(connection, group);
            } else {
                record.memberships.remove(&connection);
            }
            Ok(())
        })
    }

    /// Deletes only this folder, promotes its immediate children and releases
    /// its connections to the ungrouped root. Duplicate promoted names fail atomically.
    pub fn remove_group(&self, expected: u64, id: Uuid) -> Result<Snapshot> {
        self.mutate(expected, |record| {
            let parent = record
                .groups
                .iter()
                .find(|g| g.id == id)
                .ok_or_else(|| anyhow!("group_missing"))?
                .parent;
            record.groups.retain(|group| group.id != id);
            for child in &mut record.groups {
                if child.parent == Some(id) {
                    child.parent = parent;
                }
            }
            record.memberships.retain(|_, group| *group != id);
            Ok(())
        })
    }

    /// Call explicitly after deleting a profile. This method never edits profiles.
    pub fn remove_connection(&self, expected: u64, connection: Uuid) -> Result<Snapshot> {
        self.set_connection(expected, connection, None)
    }

    /// Explicit reconciliation after an import/delete changed the owned profile list.
    pub fn retain_connections(&self, expected: u64, alive: &[Uuid]) -> Result<Snapshot> {
        ensure!(
            alive.len() <= MAX_MEMBERSHIPS && alive.iter().all(|id| !id.is_nil()),
            "group_membership_invalid"
        );
        let alive: HashSet<Uuid> = alive.iter().copied().collect();
        self.mutate(expected, |record| {
            record.memberships.retain(|id, _| alive.contains(id));
            Ok(())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn group(value: &str, parent: Option<Uuid>) -> Group {
        Group {
            id: Uuid::new_v4(),
            name: value.to_owned(),
            color: [45, 123, 200],
            parent,
        }
    }

    #[test]
    fn unicode_names_and_sibling_identity_are_validated() {
        assert_eq!(name("  Produkcja · Łódź  ").unwrap(), "Produkcja · Łódź");
        for bad in ["", "   ", ".", "..", "a/b", "a\\b", "a\0b", "a\nb"] {
            assert!(name(bad).is_err());
        }
        assert!(name(&"🦀".repeat(65)).is_err());
        let a = group("Production", None);
        let mut b = group("production", None);
        let mut record = Record {
            groups: vec![a.clone(), b.clone()],
            ..Record::default()
        };
        assert_eq!(
            validate(&record).unwrap_err().to_string(),
            "group_name_exists"
        );
        b.parent = Some(a.id);
        record.groups[1] = b;
        validate(&record).unwrap();
    }

    #[test]
    fn cycles_missing_parents_and_maximum_depth_are_checked() {
        let mut record = Record::default();
        let mut parent = None;
        for i in 0..MAX_DEPTH {
            let node = group(&format!("level-{i}"), parent);
            parent = Some(node.id);
            record.groups.push(node);
        }
        validate(&record).unwrap();
        record.groups.push(group("too deep", parent));
        assert_eq!(
            validate(&record).unwrap_err().to_string(),
            "group_depth_limit"
        );
        record.groups.pop();
        record.groups[0].parent = parent;
        assert_eq!(validate(&record).unwrap_err().to_string(), "group_cycle");
        record.groups[0].parent = Some(Uuid::new_v4());
        assert_eq!(
            validate(&record).unwrap_err().to_string(),
            "group_parent_missing"
        );
    }

    #[test]
    fn unknown_records_orphans_and_resource_limits_fail_closed() {
        assert!(
            parse(Some(
                br#"{"v":2,"revision":0,"groups":[],"memberships":{}}"#
            ))
            .is_err()
        );
        assert!(
            parse(Some(
                br#"{"v":1,"revision":0,"groups":[],"memberships":{},"unknown":true}"#
            ))
            .is_err()
        );
        assert!(parse(Some(&vec![b' '; MAX_BYTES + 1])).is_err());
        let mut record = Record::default();
        record.memberships.insert(Uuid::new_v4(), Uuid::new_v4());
        assert_eq!(
            validate(&record).unwrap_err().to_string(),
            "group_membership_invalid"
        );
        record.memberships.clear();
        for i in 0..=MAX_GROUPS {
            record.groups.push(group(&format!("g{i}"), None));
        }
        assert_eq!(
            validate(&record).unwrap_err().to_string(),
            "group_count_limit"
        );
        record.groups.clear();
        let node = group("limit", None);
        let id = node.id;
        record.groups.push(node);
        for _ in 0..=MAX_MEMBERSHIPS {
            record.memberships.insert(Uuid::new_v4(), id);
        }
        assert_eq!(
            validate(&record).unwrap_err().to_string(),
            "group_count_limit"
        );
    }

    #[cfg(any(windows, target_os = "macos"))]
    struct Owned {
        _directory: tempfile::TempDir,
        vault: Arc<Vault>,
        one: Groups,
        two: Groups,
    }
    #[cfg(any(windows, target_os = "macos"))]
    impl Owned {
        fn new() -> Self {
            let directory = tempfile::tempdir().unwrap();
            let path = directory.path().join("private");
            let namespace = format!("BazaStudio.QA.{}", Uuid::new_v4());
            let vault = Arc::new(Vault::open_owned(&path, &namespace).unwrap());
            let other = Arc::new(Vault::open_owned(&path, &namespace).unwrap());
            let one = Groups::open(vault.clone()).unwrap();
            let two = Groups::open(other).unwrap();
            Self {
                _directory: directory,
                vault,
                one,
                two,
            }
        }
    }
    #[cfg(any(windows, target_os = "macos"))]
    impl Drop for Owned {
        fn drop(&mut self) {
            let _ = self.vault.remove_owned_qa_key();
        }
    }

    #[cfg(any(windows, target_os = "macos"))]
    #[test]
    fn owned_encrypted_roundtrip_and_fresh_default_empty() {
        let owned = Owned::new();
        assert_eq!(owned.one.load().unwrap(), Snapshot::default());
        let created = owned
            .one
            .create(0, "OWNED-GROUP-CIPHERTEXT-SENTINEL", [10, 20, 30], None)
            .unwrap();
        assert_eq!(created.revision, 1);
        assert_eq!(owned.two.load().unwrap(), created);
        let disk = std::fs::read(
            owned
                ._directory
                .path()
                .join("private/native-connection-groups.bzvault"),
        )
        .unwrap();
        assert!(
            !disk
                .windows(b"OWNED-GROUP-CIPHERTEXT-SENTINEL".len())
                .any(|bytes| bytes == b"OWNED-GROUP-CIPHERTEXT-SENTINEL")
        );
        assert_eq!(
            Groups::open(owned.vault.clone()).unwrap().load().unwrap(),
            created
        );
    }

    #[cfg(any(windows, target_os = "macos"))]
    #[test]
    fn two_instances_stale_revisions_cannot_overwrite_current_hierarchy() {
        let owned = Owned::new();
        let first = owned.one.create(0, "Parent", [1, 2, 3], None).unwrap();
        assert_eq!(
            owned
                .two
                .create(0, "Stale", [0, 0, 0], None)
                .unwrap_err()
                .to_string(),
            "group_revision_changed"
        );
        let parent = first.groups[0].id;
        let second = owned
            .two
            .create(first.revision, "Child", [2, 3, 4], Some(parent))
            .unwrap();
        assert_eq!(
            owned
                .one
                .rename(first.revision, parent, "Old rename")
                .unwrap_err()
                .to_string(),
            "group_revision_changed"
        );
        assert_eq!(owned.one.load().unwrap(), second);
    }

    #[cfg(any(windows, target_os = "macos"))]
    #[test]
    fn owned_crud_releases_memberships_and_never_deletes_connections() {
        let owned = Owned::new();
        let profile = crate::ConnectionProfile {
            name: "Owned preserved profile".into(),
            database: "owned_fixture".into(),
            username: "owned_user".into(),
            ..Default::default()
        };
        let mut profiles = crate::ProfileSet::default();
        profiles.connections.push(profile.clone());
        owned.vault.save_profiles(&profiles).unwrap();
        let before = std::fs::read(
            owned
                ._directory
                .path()
                .join("private/native-profiles.bzvault"),
        )
        .unwrap();
        let mut snap = owned.one.create(0, "Parent", [0, 0, 0], None).unwrap();
        let parent = snap.groups[0].id;
        snap = owned
            .one
            .create(snap.revision, "Child", [1, 1, 1], Some(parent))
            .unwrap();
        let child = snap.groups[1].id;
        snap = owned.one.rename(snap.revision, child, "Renamed").unwrap();
        snap = owned
            .one
            .set_color(snap.revision, child, [11, 22, 33])
            .unwrap();
        snap = owned
            .one
            .set_connection(snap.revision, profile.id, Some(parent))
            .unwrap();
        snap = owned.one.remove_group(snap.revision, parent).unwrap();
        assert!(!snap.memberships.contains_key(&profile.id));
        assert_eq!(snap.groups.len(), 1);
        assert_eq!(snap.groups[0].parent, None);
        assert_eq!(snap.groups[0].name, "Renamed");
        assert_eq!(snap.groups[0].color, [11, 22, 33]);
        assert_eq!(
            owned.vault.load_profiles().unwrap().connections[0].id,
            profile.id
        );
        assert_eq!(
            std::fs::read(
                owned
                    ._directory
                    .path()
                    .join("private/native-profiles.bzvault")
            )
            .unwrap(),
            before
        );
    }

    #[cfg(any(windows, target_os = "macos"))]
    #[test]
    fn owned_invalid_reparent_and_delete_name_collision_roll_back() {
        let owned = Owned::new();
        let mut snap = owned.one.create(0, "Same", [0, 0, 0], None).unwrap();
        snap = owned
            .one
            .create(snap.revision, "Parent", [0, 0, 0], None)
            .unwrap();
        let parent = snap.groups[1].id;
        snap = owned
            .one
            .create(snap.revision, "same", [0, 0, 0], Some(parent))
            .unwrap();
        let child = snap.groups[2].id;
        assert_eq!(
            owned
                .one
                .move_group(snap.revision, parent, Some(child))
                .unwrap_err()
                .to_string(),
            "group_cycle"
        );
        assert_eq!(
            owned
                .one
                .remove_group(snap.revision, parent)
                .unwrap_err()
                .to_string(),
            "group_name_exists"
        );
        assert_eq!(owned.two.load().unwrap(), snap);
        snap = owned.one.rename(snap.revision, child, "Distinct").unwrap();
        snap = owned.one.remove_group(snap.revision, parent).unwrap();
        assert_eq!(snap.groups[1].parent, None);
    }

    #[cfg(any(windows, target_os = "macos"))]
    #[test]
    fn owned_combined_update_rolls_back_name_color_parent_and_commits_once() {
        let owned = Owned::new();
        let mut before = owned
            .one
            .create(0, "Original root", [1, 2, 3], None)
            .unwrap();
        let root = before.groups[0].id;
        before = owned
            .one
            .create(before.revision, "Target", [4, 5, 6], None)
            .unwrap();
        let target = before.groups[1].id;
        before = owned
            .one
            .create(before.revision, "Original child", [7, 8, 9], Some(root))
            .unwrap();
        let child = before.groups[2].id;
        before = owned
            .one
            .create(before.revision, "Occupied", [10, 11, 12], Some(target))
            .unwrap();
        let encrypted_path = owned
            ._directory
            .path()
            .join("private/native-connection-groups.bzvault");
        let ciphertext = std::fs::read(&encrypted_path).unwrap();
        assert_eq!(
            owned
                .one
                .update(
                    before.revision,
                    root,
                    "Changed root",
                    [90, 91, 92],
                    Some(child)
                )
                .unwrap_err()
                .to_string(),
            "group_cycle"
        );
        assert_eq!(owned.two.load().unwrap(), before);
        assert_eq!(std::fs::read(&encrypted_path).unwrap(), ciphertext);
        assert_eq!(
            owned
                .one
                .update(
                    before.revision,
                    child,
                    "occupied",
                    [93, 94, 95],
                    Some(target)
                )
                .unwrap_err()
                .to_string(),
            "group_name_exists"
        );
        assert_eq!(owned.two.load().unwrap(), before);
        assert_eq!(std::fs::read(&encrypted_path).unwrap(), ciphertext);
        let updated = owned
            .one
            .update(
                before.revision,
                child,
                "Renamed child",
                [40, 50, 60],
                Some(target),
            )
            .unwrap();
        assert_eq!(updated.revision, before.revision + 1);
        let group = updated.groups.iter().find(|g| g.id == child).unwrap();
        assert_eq!(
            (&group.name, group.color, group.parent),
            (&"Renamed child".to_owned(), [40, 50, 60], Some(target))
        );
        assert_eq!(owned.two.load().unwrap(), updated);
        assert_eq!(
            owned
                .one
                .update(before.revision, child, "Stale", [0, 0, 0], None)
                .unwrap_err()
                .to_string(),
            "group_revision_changed"
        );
        assert_eq!(owned.two.load().unwrap(), updated);
    }

    #[cfg(any(windows, target_os = "macos"))]
    #[test]
    fn owned_explicit_orphan_removal_and_revision_overflow_are_atomic() {
        let owned = Owned::new();
        let mut snap = owned.one.create(0, "Folder", [1, 2, 3], None).unwrap();
        let id = snap.groups[0].id;
        let a = Uuid::new_v4();
        let b = Uuid::new_v4();
        snap = owned
            .one
            .set_connection(snap.revision, a, Some(id))
            .unwrap();
        snap = owned
            .one
            .set_connection(snap.revision, b, Some(id))
            .unwrap();
        snap = owned.one.retain_connections(snap.revision, &[a]).unwrap();
        assert!(snap.memberships.contains_key(&a));
        assert!(!snap.memberships.contains_key(&b));
        snap = owned.one.remove_connection(snap.revision, a).unwrap();
        assert!(snap.memberships.is_empty());
        assert_eq!(
            owned
                .one
                .set_connection(snap.revision, b, Some(Uuid::new_v4()))
                .unwrap_err()
                .to_string(),
            "group_membership_invalid"
        );
        assert_eq!(owned.one.load().unwrap(), snap);
        let mut overflow = parse(Some(&owned.vault.load_named(RECORD).unwrap().unwrap())).unwrap();
        overflow.revision = u64::MAX;
        owned
            .vault
            .update_named(RECORD, |_| Ok((serde_json::to_vec(&overflow).unwrap(), ())))
            .unwrap();
        let before = owned.one.load().unwrap();
        assert_eq!(
            owned
                .one
                .rename(u64::MAX, id, "Overflow")
                .unwrap_err()
                .to_string(),
            "group_revision_limit"
        );
        assert_eq!(owned.one.load().unwrap(), before);
    }
}
