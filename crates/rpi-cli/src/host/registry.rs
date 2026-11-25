use super::lifecycle::{
    self, LifecycleAdmission, LifecycleReceipt, LifecycleRequest, LifecycleState,
};
use super::recovery::{self, Bindings};
use super::{
    catalog::{is_mini, MAX_RECORD_BYTES},
    AgentId, AgentPage, AgentRecord, AgentSpec, AgentState, Capacity, CatalogError, CatalogGuard,
    CatalogResult, CatalogStatus, HostConfig, HostEpoch, ResidentPermit, MAX_PAGE_SIZE,
};
use rusqlite::{params, Connection, OpenFlags, OptionalExtension, Row, TransactionBehavior};
use std::{collections::HashMap, fs::File, num::NonZeroUsize, path::Path};

const COLUMNS: &str =
    "id, revision, resident, executing, busy, mini, session_id, session_path, data";
const SCHEMA: &str = "
CREATE TABLE host_meta (singleton INTEGER PRIMARY KEY CHECK(singleton = 1), epoch TEXT NOT NULL);
CREATE TABLE agents (
    id TEXT PRIMARY KEY NOT NULL,
    revision INTEGER NOT NULL CHECK(revision > 0),
    resident INTEGER NOT NULL CHECK(resident IN (0,1)),
    executing INTEGER NOT NULL CHECK(executing IN (0,1)),
    busy INTEGER NOT NULL CHECK(busy IN (0,1)),
    mini INTEGER NOT NULL CHECK(mini IN (0,1)),
    session_id TEXT UNIQUE,
    session_path TEXT UNIQUE,
    data TEXT NOT NULL CHECK(length(CAST(data AS BLOB)) <= 131072),
    CHECK(executing <= busy AND busy <= resident),
    CHECK((mini = 0 AND session_id IS NOT NULL AND session_path IS NOT NULL)
        OR (mini = 1 AND session_id IS NULL AND session_path IS NULL))
);
PRAGMA user_version = 1;";

// Close-on-exec does not prevent fork's temporary descriptor inheritance. A
// last-close-only flock release can outlive shutdown acknowledgement when a
// concurrent child still references the open file description before exec.
pub(super) struct InstanceLock(Option<File>);
impl InstanceLock {
    #[cfg(target_os = "macos")]
    pub(super) fn file(&self) -> &File {
        self.0.as_ref().expect("held Host instance lock")
    }

    fn release(&mut self) -> std::io::Result<()> {
        if let Some(file) = &self.0 {
            loop {
                match fs2::FileExt::unlock(file) {
                    Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                    result => {
                        result?;
                        break;
                    }
                }
            }
        }
        drop(self.0.take());
        Ok(())
    }
}
impl Drop for InstanceLock {
    fn drop(&mut self) {
        let _ = self.release();
    }
}

pub(crate) struct Registry {
    // Field drop order: release SQLite and reservations before the instance lock.
    connection: Connection,
    permits: HashMap<AgentId, ResidentPermit>,
    capacity: Capacity,
    config: HostConfig,
    epoch: HostEpoch,
    bindings: Bindings,
    _instance_lock: InstanceLock,
}

struct StoredRow {
    id: String,
    revision: i64,
    resident: bool,
    executing: bool,
    busy: bool,
    mini: bool,
    session_id: Option<String>,
    session_path: Option<String>,
    data: String,
}

impl StoredRow {
    fn read(row: &Row<'_>) -> rusqlite::Result<Self> {
        Ok(Self {
            id: row.get(0)?,
            revision: row.get(1)?,
            resident: row.get(2)?,
            executing: row.get(3)?,
            busy: row.get(4)?,
            mini: row.get(5)?,
            session_id: row.get(6)?,
            session_path: row.get(7)?,
            data: row.get(8)?,
        })
    }

    fn decode(self) -> CatalogResult<AgentRecord> {
        if self.data.len() > MAX_RECORD_BYTES {
            return Err(CatalogError::InvalidRecord("metadata byte budget exceeded"));
        }
        let record: AgentRecord = serde_json::from_str(&self.data)?;
        record.validate()?;
        let session_id = record.spec.session.as_ref().map(|s| s.id.as_str());
        let session_path = record.spec.session.as_ref().and_then(|s| s.path.to_str());
        if self.id != record.spec.id.to_string()
            || self.revision != record.revision as i64
            || self.resident != record.state.is_resident()
            || self.executing != record.state.is_executing()
            || self.busy != record.state.is_busy()
            || self.mini != is_mini(&record)
            || self.session_id.as_deref() != session_id
            || self.session_path.as_deref() != session_path
        {
            return Err(CatalogError::InvalidRecord(
                "indexed fields do not match metadata",
            ));
        }
        Ok(record)
    }
}

pub(super) fn find(connection: &Connection, id: AgentId) -> CatalogResult<Option<AgentRecord>> {
    connection
        .query_row(
            &format!("SELECT {COLUMNS} FROM agents WHERE id = ?1"),
            [id.to_string()],
            StoredRow::read,
        )
        .optional()?
        .map(StoredRow::decode)
        .transpose()
}

fn page(connection: &Connection, after: &str, limit: usize) -> CatalogResult<Vec<AgentRecord>> {
    let mut statement = connection.prepare(&format!(
        "SELECT {COLUMNS} FROM agents WHERE id > ?1 ORDER BY id LIMIT ?2"
    ))?;
    let rows = statement.query_map(params![after, limit as i64], StoredRow::read)?;
    rows.map(|row| row.map_err(CatalogError::from).and_then(StoredRow::decode))
        .collect()
}

fn next_revision(record: &mut AgentRecord) -> CatalogResult<()> {
    record.revision = record
        .revision
        .checked_add(1)
        .filter(|revision| *revision <= i64::MAX as u64)
        .ok_or(CatalogError::InvalidRecord("revision overflow"))?;
    Ok(())
}

fn write(
    connection: &Connection,
    record: &AgentRecord,
    expected: Option<u64>,
) -> CatalogResult<()> {
    record.validate()?;
    let data = serde_json::to_string(record)?;
    if data.len() > MAX_RECORD_BYTES {
        return Err(CatalogError::InvalidRecord("metadata byte budget exceeded"));
    }
    let session_id = record.spec.session.as_ref().map(|s| s.id.as_str());
    let session_path = record.spec.session.as_ref().and_then(|s| s.path.to_str());
    let values = params![
        record.spec.id.to_string(),
        record.revision as i64,
        record.state.is_resident(),
        record.state.is_executing(),
        record.state.is_busy(),
        is_mini(record),
        session_id,
        session_path,
        data
    ];
    if let Some(expected) = expected {
        let mut values = values.to_vec();
        let expected = expected as i64;
        values.push(&expected);
        if connection.execute(
            "UPDATE agents SET revision=?2,resident=?3,executing=?4,busy=?5,
            mini=?6,session_id=?7,session_path=?8,data=?9 WHERE id=?1 AND revision=?10",
            values.as_slice(),
        )? != 1
        {
            return Err(CatalogError::InvalidRecord("guarded update did not match"));
        }
    } else {
        connection.execute(
            "INSERT INTO agents
            (id,revision,resident,executing,busy,mini,session_id,session_path,data)
            VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9)",
            values,
        )?;
    }
    Ok(())
}

fn check_owner(record: &AgentRecord, epoch: HostEpoch) -> CatalogResult<()> {
    if record.state.is_resident() && record.last_epoch != epoch {
        return Err(CatalogError::InvalidRecord(
            "resident belongs to another epoch",
        ));
    }
    Ok(())
}

impl Registry {
    pub(crate) fn open(
        directory: &Path,
        config: HostConfig,
        bindings: Bindings,
    ) -> CatalogResult<Self> {
        config.validate()?;
        let instance_lock = own_directory(directory)?;
        // Resolve trusted parent aliases (e.g. /var -> /private/var on macOS)
        // before SQLite's NOFOLLOW check; the final directory was checked above.
        let directory = directory.canonicalize()?;
        let database = directory.join("agents.sqlite3");
        drop(private_file(&database)?);
        for suffix in [
            "agents.sqlite3-wal",
            "agents.sqlite3-shm",
            "agents.sqlite3-journal",
        ] {
            let path = directory.join(suffix);
            match std::fs::symlink_metadata(&path) {
                Ok(_) => check_private(&path, false)?,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }
        }
        let mut connection = Connection::open_with_flags(
            database,
            OpenFlags::SQLITE_OPEN_READ_WRITE
                | OpenFlags::SQLITE_OPEN_NO_MUTEX
                | OpenFlags::SQLITE_OPEN_NOFOLLOW,
        )?;
        // Bound SQLite's own row materialization too, before decoding JSON.
        // Leave space for duplicated reference/index columns around the payload.
        connection.set_limit(
            rusqlite::limits::Limit::SQLITE_LIMIT_LENGTH,
            (MAX_RECORD_BYTES * 2) as i32,
        );
        connection.busy_timeout(std::time::Duration::from_secs(1))?;
        let version: i64 = connection.query_row("PRAGMA user_version", [], |row| row.get(0))?;
        if !matches!(version, 0..=3) {
            return Err(CatalogError::UnsupportedVersion(version));
        }
        if version == 0 {
            let count: i64 = connection.query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE name NOT GLOB 'sqlite_*'",
                [],
                |row| row.get(0),
            )?;
            if count != 0 {
                return Err(CatalogError::InvalidRecord("unversioned nonempty database"));
            }
        }
        connection.execute_batch(
            "PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL;
            PRAGMA foreign_keys=ON; PRAGMA trusted_schema=OFF;",
        )?;
        let epoch = HostEpoch::new();
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        if version == 0 {
            transaction.execute_batch(SCHEMA)?;
            lifecycle::create_schema(&transaction)?;
            transaction.execute("INSERT INTO host_meta VALUES (1,?1)", [epoch.to_string()])?;
        } else {
            if version == 1 {
                lifecycle::create_schema(&transaction)?;
            }
            let previous: String = transaction.query_row(
                "SELECT epoch FROM host_meta WHERE singleton=1",
                [],
                |row| row.get(0),
            )?;
            let previous = HostEpoch::try_from(previous)?;
            let mut cursor = String::new();
            loop {
                // Bounded keyset pages; do not load the complete registry on restart.
                let records = page(&transaction, &cursor, MAX_PAGE_SIZE)?;
                if records.is_empty() {
                    break;
                }
                for mut record in records {
                    cursor = record.spec.id.to_string();
                    check_owner(&record, previous)?;
                    let next = record.state.after_restart(record.spec.mode)?;
                    if next != record.state {
                        let expected = record.revision;
                        record.state = next;
                        next_revision(&mut record)?;
                        write(&transaction, &record, Some(expected))?;
                    }
                }
            }
            lifecycle::recover(&transaction, previous)?;
            transaction.execute(
                "UPDATE host_meta SET epoch=?1 WHERE singleton=1",
                [epoch.to_string()],
            )?;
        }
        if version < 3 {
            recovery::create_schema(&transaction)?;
        }
        recovery::recover(&transaction)?;
        transaction.commit()?;
        Ok(Self {
            connection,
            permits: HashMap::new(),
            capacity: Capacity::new(config.max_agents),
            config,
            epoch,
            bindings,
            _instance_lock: instance_lock,
        })
    }

    pub(crate) fn prepare_lifecycle(
        &mut self,
        request: LifecycleRequest,
    ) -> CatalogResult<LifecycleAdmission> {
        let admission = lifecycle::prepare(&mut self.connection, self.epoch, request)?;
        lifecycle::checkpoint(&self.connection)?;
        Ok(admission)
    }

    pub(crate) fn finish_lifecycle(
        &mut self,
        request: LifecycleRequest,
        status: LifecycleState,
    ) -> CatalogResult<LifecycleReceipt> {
        let receipt = lifecycle::finish(
            &mut self.connection,
            self.epoch,
            request,
            status,
            &self.bindings,
        )?;
        lifecycle::checkpoint(&self.connection)?;
        Ok(receipt)
    }

    pub(crate) fn lifecycle_receipt(
        &self,
        scope: super::ControlScopeId,
        id: super::RequestId,
    ) -> CatalogResult<Option<LifecycleReceipt>> {
        let receipt = lifecycle::get(&self.connection, scope, id)?;
        if receipt.is_some() {
            lifecycle::checkpoint(&self.connection)?;
        }
        Ok(receipt)
    }

    #[cfg(unix)]
    pub(super) fn recovery_members(
        &self,
        configured: &[AgentId],
    ) -> CatalogResult<recovery::Membership> {
        recovery::read(&self.connection, configured)
    }

    #[cfg(unix)]
    pub(super) fn remember_members(&mut self, loaded: &[AgentId]) -> CatalogResult<()> {
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        for &id in loaded {
            let binding = self
                .bindings
                .get(&id)
                .copied()
                .ok_or(CatalogError::InvalidRecord("missing recovery profile"))?;
            recovery::set(&transaction, id, binding)?;
        }
        transaction.commit()?;
        Ok(())
    }

    pub(crate) fn shutdown(self) -> CatalogResult<()> {
        let Self {
            connection,
            permits,
            _instance_lock: mut lock,
            ..
        } = self;
        drop(connection);
        drop(permits);
        // Explicit unlock affects inherited references to this same open file
        // description too. Report unlock failure rather than falsely ACK cleanup.
        lock.release()?;
        Ok(())
    }

    pub(crate) fn epoch(&self) -> HostEpoch {
        self.epoch
    }

    pub(crate) fn register(&mut self, spec: AgentSpec) -> CatalogResult<AgentRecord> {
        spec.validate()?;
        self.config.resolve_timeouts(spec.timeouts)?;
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        if find(&transaction, spec.id)?.is_some() {
            return Err(CatalogError::AlreadyRegistered);
        }
        if let Some(session) = &spec.session {
            let exists: bool = transaction.query_row(
                "SELECT EXISTS(SELECT 1 FROM agents WHERE session_id=?1 OR session_path=?2)",
                params![session.id, session.path.to_str()],
                |row| row.get(0),
            )?;
            if exists {
                return Err(CatalogError::SessionAlreadyRegistered);
            }
        }
        let record = AgentRecord {
            spec,
            state: AgentState::Registered,
            revision: 1,
            last_epoch: self.epoch,
        };
        write(&transaction, &record, None)?;
        transaction.commit()?;
        Ok(record)
    }

    pub(crate) fn get(&self, id: AgentId) -> CatalogResult<AgentRecord> {
        let record = find(&self.connection, id)?.ok_or(CatalogError::UnknownAgent)?;
        check_owner(&record, self.epoch)?;
        Ok(record)
    }

    pub(crate) fn list(&self, after: Option<AgentId>, limit: usize) -> CatalogResult<AgentPage> {
        let after = after.map(|id| id.to_string()).unwrap_or_default();
        let mut agents = page(&self.connection, &after, limit + 1)?;
        let more = agents.len() > limit;
        agents.truncate(limit);
        for record in &agents {
            check_owner(record, self.epoch)?;
        }
        let next_cursor = more
            .then(|| agents.last().map(|record| record.spec.id))
            .flatten();
        Ok(AgentPage {
            agents,
            next_cursor,
        })
    }

    pub(crate) fn transition(
        &mut self,
        id: AgentId,
        guard: CatalogGuard,
        next: AgentState,
    ) -> CatalogResult<AgentRecord> {
        if guard.epoch != self.epoch {
            return Err(CatalogError::EpochConflict);
        }
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let mut record = find(&transaction, id)?.ok_or(CatalogError::UnknownAgent)?;
        check_owner(&record, self.epoch)?;
        if guard.revision != record.revision {
            return Err(CatalogError::RevisionConflict {
                expected: guard.revision,
                actual: record.revision,
            });
        }
        let old = record.state;
        record.state = old.transition(next, record.spec.mode)?;
        if self.permits.contains_key(&id) != old.is_resident() {
            return Err(CatalogError::InvalidRecord("resident permit mismatch"));
        }
        if next == AgentState::Starting {
            self.config.resolve_timeouts(record.spec.timeouts)?;
        }
        let reserved = if !old.is_resident() && next.is_resident() {
            Some(self.capacity.try_reserve(id)?)
        } else {
            None
        };
        next_revision(&mut record)?;
        record.last_epoch = self.epoch;
        write(&transaction, &record, Some(guard.revision))?;
        transaction.commit()?;
        // Publish ownership only after storage commits. Errors above drop a new
        // reservation; failed close commits retain the existing permit.
        if let Some(permit) = reserved {
            self.permits.insert(id, permit);
        }
        if old.is_resident() && !next.is_resident() {
            self.permits.remove(&id);
        }
        Ok(record)
    }

    pub(crate) fn status(&self) -> CatalogResult<CatalogStatus> {
        let (registered, resident, executing, busy, mini): (usize, usize, usize, usize, usize) =
            self.connection.query_row(
                "SELECT COUNT(*),COALESCE(SUM(resident),0),
                COALESCE(SUM(executing),0),COALESCE(SUM(busy),0),COALESCE(SUM(mini),0)
                FROM agents",
                [],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                    ))
                },
            )?;
        let capacity = self.capacity.snapshot()?;
        if resident != capacity.resident || resident != self.permits.len() {
            return Err(CatalogError::InvalidRecord("resident accounting mismatch"));
        }
        Ok(CatalogStatus {
            epoch: self.epoch,
            registered,
            resident,
            executing,
            busy,
            mini,
            max_agents: capacity.limit,
        })
    }

    pub(crate) fn set_limit(&mut self, limit: Option<NonZeroUsize>) -> CatalogResult<()> {
        self.capacity.set_limit(limit)?;
        self.config.max_agents = limit;
        Ok(())
    }
}

#[cfg(unix)]
fn check_private(path: &Path, directory: bool) -> CatalogResult<()> {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    let metadata = std::fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink()
        || metadata.permissions().mode() & 0o077 != 0
        || metadata.uid() != nix::unistd::geteuid().as_raw()
        || if directory {
            !metadata.is_dir()
        } else {
            !metadata.is_file() || metadata.nlink() != 1
        }
    {
        return Err(CatalogError::UnsafePath);
    }
    Ok(())
}

#[cfg(unix)]
pub(super) fn private_file(path: &Path) -> CatalogResult<File> {
    use std::os::unix::fs::OpenOptionsExt;
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(nix::libc::O_NOFOLLOW | nix::libc::O_CLOEXEC)
        .open(path)?;
    check_private(path, false)?;
    Ok(file)
}

#[cfg(unix)]
fn own_directory(path: &Path) -> CatalogResult<InstanceLock> {
    own_directory_lock(path, "instance.lock")
}

#[cfg(unix)]
pub(super) fn own_directory_lock(path: &Path, lock_name: &str) -> CatalogResult<InstanceLock> {
    use std::os::unix::fs::DirBuilderExt;
    // Never chmod existing user paths. New directories are private from creation.
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(path)?;
    check_private(path, true)?;
    let lock = private_file(&path.join(lock_name))?;
    fs2::FileExt::try_lock_exclusive(&lock).map_err(|error| {
        if error.kind() == std::io::ErrorKind::WouldBlock {
            CatalogError::AlreadyRunning
        } else {
            CatalogError::Io(error)
        }
    })?;
    // Keep the lock inode on disk: unlinking on shutdown admits split ownership.
    Ok(InstanceLock(Some(lock)))
}

#[cfg(not(unix))]
fn own_directory(_: &Path) -> CatalogResult<InstanceLock> {
    Err(CatalogError::UnsupportedPlatform)
}
#[cfg(not(unix))]
fn private_file(_: &Path) -> CatalogResult<File> {
    Err(CatalogError::UnsupportedPlatform)
}
#[cfg(not(unix))]
fn check_private(_: &Path, _: bool) -> CatalogResult<()> {
    Err(CatalogError::UnsupportedPlatform)
}

#[cfg(all(test, unix))]
mod ownership_tests {
    use super::*;
    use std::os::unix::fs::MetadataExt;

    #[test]
    fn shutdown_and_drop_unlock_shared_descriptions_without_releasing_a_later_owner() {
        let root = tempfile::tempdir().unwrap();
        // Deterministic reproduction of the fork/exec window: dup and fork
        // references share the same flock-bearing open file description.
        let path = root.path().join("last-close-only.lock");
        let original = private_file(&path).unwrap();
        fs2::FileExt::try_lock_exclusive(&original).unwrap();
        let inherited = original.try_clone().unwrap();
        drop(original);
        let contender = private_file(&path).unwrap();
        assert_eq!(
            fs2::FileExt::try_lock_exclusive(&contender)
                .unwrap_err()
                .kind(),
            std::io::ErrorKind::WouldBlock
        );
        fs2::FileExt::unlock(&inherited).unwrap();
        fs2::FileExt::try_lock_exclusive(&contender).unwrap();
        drop(inherited);
        drop(contender);

        for explicit_shutdown in [false, true] {
            let directory = root
                .path()
                .join(if explicit_shutdown { "ack" } else { "drop" });
            let registry =
                Registry::open(&directory, HostConfig::default(), Bindings::new()).unwrap();
            let inherited = registry
                ._instance_lock
                .0
                .as_ref()
                .unwrap()
                .try_clone()
                .unwrap();
            let inode = inherited.metadata().unwrap().ino();
            if explicit_shutdown {
                registry.shutdown().unwrap();
            } else {
                drop(registry);
            }
            let next = Registry::open(&directory, HostConfig::default(), Bindings::new()).unwrap();
            assert_eq!(
                std::fs::metadata(directory.join("instance.lock"))
                    .unwrap()
                    .ino(),
                inode,
                "never unlink the singleton inode"
            );
            assert!(matches!(
                Registry::open(&directory, HostConfig::default(), Bindings::new()),
                Err(CatalogError::AlreadyRunning)
            ));
            drop(inherited);
            assert!(
                matches!(
                    Registry::open(&directory, HostConfig::default(), Bindings::new()),
                    Err(CatalogError::AlreadyRunning)
                ),
                "dropping an old inherited descriptor must not unlock a new owner"
            );
            next.shutdown().unwrap();
        }
    }
}
