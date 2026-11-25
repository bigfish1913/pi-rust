//! Durable Persistent membership, independent of runtime residency and lifecycle receipts.
#[cfg(unix)]
use super::MAX_TRUSTED_PROFILES;
use super::{AgentId, CatalogError, CatalogResult};
#[cfg(unix)]
use rusqlite::OptionalExtension;
use rusqlite::{params, Connection};
use std::collections::BTreeMap;

pub(super) type BindingVersion = [u8; 32];
#[cfg(unix)]
pub(super) type Membership = BTreeMap<AgentId, Option<BindingVersion>>;

pub(super) fn create_schema(connection: &Connection) -> CatalogResult<()> {
    connection.execute_batch(
        "CREATE TABLE recovery_members (
            agent_id TEXT PRIMARY KEY NOT NULL REFERENCES agents(id),
            binding BLOB CHECK(binding IS NULL OR length(binding) = 32),
            close_pending INTEGER NOT NULL DEFAULT 0 CHECK(close_pending IN (0,1))
        );
        PRAGMA user_version=3;",
    )?;
    Ok(())
}

#[cfg(unix)]
pub(super) fn read(connection: &Connection, configured: &[AgentId]) -> CatalogResult<Membership> {
    let mut statement = connection.prepare(
        "SELECT agent_id,binding FROM recovery_members WHERE binding IS NOT NULL ORDER BY agent_id LIMIT ?1",
    )?;
    let rows = statement.query_map([MAX_TRUSTED_PROFILES as i64 + 1], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, Option<BindingVersion>>(1)?,
        ))
    })?;
    let mut members = BTreeMap::new();
    for row in rows {
        let (id, binding) = row?;
        members.insert(AgentId::try_from(id)?, binding);
    }
    if members.len() > MAX_TRUSTED_PROFILES {
        return Err(CatalogError::InvalidRecord(
            "recovery membership exceeds profile limit",
        ));
    }
    let mut removed = connection
        .prepare("SELECT 1 FROM recovery_members WHERE agent_id=?1 AND binding IS NULL")?;
    for id in configured {
        if removed
            .query_row([id.to_string()], |row| row.get::<_, i64>(0))
            .optional()?
            .is_some()
        {
            members.insert(*id, None);
        }
    }
    Ok(members)
}

pub(super) fn set(
    connection: &Connection,
    agent_id: AgentId,
    binding: BindingVersion,
) -> CatalogResult<()> {
    let id = agent_id.to_string();
    if connection.execute(
        "INSERT INTO recovery_members(agent_id,binding)
         SELECT id,?2 FROM agents WHERE id=?1 AND mini=0
         ON CONFLICT(agent_id) DO UPDATE SET binding=excluded.binding, close_pending=0",
        params![id, binding],
    )? != 1
    {
        return Err(CatalogError::InvalidRecord(
            "recovery requires a Persistent Agent",
        ));
    }
    Ok(())
}

pub(super) fn prepare_close(connection: &Connection, agent_id: AgentId) -> CatalogResult<()> {
    connection.execute(
        "UPDATE recovery_members SET close_pending=1 WHERE agent_id=?1 AND binding IS NOT NULL",
        [agent_id.to_string()],
    )?;
    Ok(())
}

pub(super) fn finish_close(
    connection: &Connection,
    agent_id: AgentId,
    rejected: bool,
) -> CatalogResult<()> {
    connection.execute(
        "UPDATE recovery_members SET binding=CASE WHEN ?2 THEN binding ELSE NULL END,
            close_pending=0 WHERE agent_id=?1 AND close_pending=1",
        params![agent_id.to_string(), rejected],
    )?;
    Ok(())
}

pub(super) fn recover(connection: &Connection) -> CatalogResult<()> {
    connection.execute(
        "UPDATE recovery_members SET binding=NULL, close_pending=0 WHERE close_pending=1",
        [],
    )?;
    Ok(())
}

pub(super) type Bindings = BTreeMap<AgentId, BindingVersion>;

#[cfg(unix)]
pub(super) fn profile_version(
    host_id: super::protocol::HostId,
    token: &super::protocol::AuthToken,
    bytes: &[u8],
    profile: &super::bindings::NativeBindingProfile,
) -> BindingVersion {
    use sha2::{Digest, Sha256};
    // Keep credential-bearing profile fingerprints private and keyed to the
    // configured Host. Public ProfileRefs remain independently random.
    let token_key = Sha256::digest(serde_json::to_vec(token).expect("token is a JSON string"));
    let resources = profile
        .instruction_resources
        .as_ref()
        .map(|value| value.identity());
    Sha256::new()
        .chain_update(b"rpi-host-private-profile-v1\0")
        .chain_update(token_key)
        .chain_update(host_id.to_string())
        .chain_update(Sha256::digest(bytes))
        .chain_update(resources.unwrap_or_default())
        .finalize()
        .into()
}
