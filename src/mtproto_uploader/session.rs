//! Persistent grammers session storage backed by the application database.
//!
//! grammers 0.12 ships `SqliteSession` behind its `sqlite-storage` default
//! feature, backed by `libsql`, which statically links its own SQLite
//! amalgamation. This application uses `rusqlite` with bundled SQLite for its
//! own database. Having both engines in one binary produces duplicate
//! `sqlite3_*` symbols at link time and, when only one copy is linked, a
//! runtime `SQLITE_MISUSE (21)` panic: libsql calls `sqlite3_config` after
//! rusqlite has already initialized SQLite. Upstream tracks this in
//! tursodatabase/libsql#526 and #2096 with no safe fix available.
//!
//! This module therefore implements `Session` directly on top of the one
//! SQLite engine the process already has. All persisted state lives in typed
//! columns (no JSON), every write goes through SQLite transactions, and auth
//! keys survive a restart. Peers are persisted too (`mtproto_peers`): MTProto
//! `sendMedia` needs the peer's access hash, grammers caches peers only when
//! the app drains the updates stream, and a restart without persisted peers
//! made every large delivery fail with "Cannot resolve peer".

use grammers_session::types::{
    ChannelKind, ChannelState, DcOption, PeerAuth, PeerId, PeerInfo, UpdateState, UpdatesState,
};
use grammers_session::{BoxFuture, Session, SessionData};
use rusqlite::{Connection, OptionalExtension, params};
use std::fmt;
use std::net::{AddrParseError, SocketAddrV4, SocketAddrV6};
use std::sync::Mutex;

#[derive(Debug)]
pub enum DbSessionError {
    Poisoned,
    Sqlite(rusqlite::Error),
    Addr(AddrParseError),
}

impl fmt::Display for DbSessionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Poisoned => write!(f, "session lock is poisoned"),
            Self::Sqlite(e) => write!(f, "session database: {e}"),
            Self::Addr(e) => write!(f, "session DC address: {e}"),
        }
    }
}

impl std::error::Error for DbSessionError {}

impl From<rusqlite::Error> for DbSessionError {
    fn from(e: rusqlite::Error) -> Self {
        Self::Sqlite(e)
    }
}

impl From<AddrParseError> for DbSessionError {
    fn from(e: AddrParseError) -> Self {
        Self::Addr(e)
    }
}

struct Inner {
    conn: Connection,
    data: SessionData,
}

/// A `Session` whose state survives restarts in `tiktok_downloader.db`.
/// Every bot uses its own `key` so six uploaders never share auth keys.
pub struct DbSession {
    key: String,
    inner: Mutex<Inner>,
}

impl DbSession {
    pub fn open(key: impl Into<String>) -> Result<Self, DbSessionError> {
        let key = key.into();
        let conn = Connection::open(crate::database::get_database_path())?;
        conn.execute_batch("PRAGMA busy_timeout = 10000;")?;

        let mut data = SessionData::default();

        if let Some((home_dc, pts, qts, date, seq)) = conn
            .query_row(
                "SELECT home_dc, pts, qts, date, seq FROM mtproto_sessions WHERE bot_id = ?1",
                params![key],
                |row| {
                    Ok((
                        row.get::<_, i32>(0)?,
                        row.get::<_, i32>(1)?,
                        row.get::<_, i32>(2)?,
                        row.get::<_, i32>(3)?,
                        row.get::<_, i32>(4)?,
                    ))
                },
            )
            .optional()?
        {
            data.home_dc = home_dc;
            data.updates_state.pts = pts;
            data.updates_state.qts = qts;
            data.updates_state.date = date;
            data.updates_state.seq = seq;
        }

        {
            let mut stmt = conn.prepare(
                "SELECT dc_id, ipv4, ipv6, auth_key FROM mtproto_dc_options WHERE bot_id = ?1",
            )?;
            let rows = stmt.query_map(params![key], |row| {
                Ok((
                    row.get::<_, i32>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, Option<Vec<u8>>>(3)?,
                ))
            })?;
            for row in rows {
                let (id, ipv4, ipv6, auth_key) = row?;
                let auth_key = match auth_key {
                    Some(bytes) => match <[u8; 256]>::try_from(bytes) {
                        Ok(key) => Some(key),
                        Err(_) => {
                            log::warn!("Ignoring malformed auth key for DC {}", id);
                            None
                        }
                    },
                    None => None,
                };
                data.dc_options.insert(
                    id,
                    DcOption {
                        id,
                        ipv4: ipv4.parse::<SocketAddrV4>()?,
                        ipv6: ipv6.parse::<SocketAddrV6>()?,
                        auth_key,
                    },
                );
            }
        }

        {
            let mut stmt = conn.prepare(
                "SELECT channel_id, pts FROM mtproto_channel_states WHERE bot_id = ?1",
            )?;
            let rows = stmt.query_map(params![key], |row| {
                Ok(ChannelState {
                    id: row.get(0)?,
                    pts: row.get(1)?,
                })
            })?;
            for row in rows {
                data.updates_state.channels.push(row?);
            }
        }

        {
            let mut stmt = conn.prepare(
                "SELECT kind, peer_id, auth, flags FROM mtproto_peers WHERE bot_id = ?1",
            )?;
            let rows = stmt.query_map(params![key], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, Option<i64>>(2)?,
                    row.get::<_, Option<i64>>(3)?,
                ))
            })?;
            for row in rows {
                let (kind, id, auth, flags) = row?;
                let auth = auth.map(PeerAuth::from_hash);
                let flags = flags.unwrap_or(0);
                let info = match kind.as_str() {
                    "user" => PeerInfo::User {
                        id,
                        auth,
                        bot: Some(flags & 1 != 0),
                        is_self: Some(flags & 2 != 0),
                    },
                    "chat" => PeerInfo::Chat { id },
                    "channel" => PeerInfo::Channel {
                        id,
                        auth,
                        kind: match flags & 15 {
                            1 => Some(ChannelKind::Broadcast),
                            2 => Some(ChannelKind::Megagroup),
                            4 => Some(ChannelKind::Gigagroup),
                            8 => Some(ChannelKind::Community),
                            _ => None,
                        },
                    },
                    _ => continue,
                };
                data.peer_infos.insert(info.id(), info);
            }
        }

        Ok(Self {
            key,
            inner: Mutex::new(Inner { conn, data }),
        })
    }

    /// Drop every persisted row for `key`. Used by the session self-heal path:
    /// the next `build_client` starts from a fresh key and re-authenticates.
    pub fn purge(key: &str) -> Result<(), DbSessionError> {
        let conn = Connection::open(crate::database::get_database_path())?;
        conn.execute_batch("PRAGMA busy_timeout = 10000;")?;
        for table in [
            "mtproto_sessions",
            "mtproto_dc_options",
            "mtproto_channel_states",
            "mtproto_peers",
        ] {
            conn.execute(&format!("DELETE FROM {table} WHERE bot_id = ?1"), params![key])?;
        }
        Ok(())
    }

    fn lock(&self) -> Result<std::sync::MutexGuard<'_, Inner>, DbSessionError> {
        self.inner.lock().map_err(|_| DbSessionError::Poisoned)
    }
}

fn upsert_session_row(
    conn: &Connection,
    key: &str,
    data: &SessionData,
) -> Result<(), DbSessionError> {
    conn.execute(
        "INSERT INTO mtproto_sessions (bot_id, home_dc, pts, qts, date, seq)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)
         ON CONFLICT(bot_id) DO UPDATE SET
             home_dc = excluded.home_dc,
             pts = excluded.pts,
             qts = excluded.qts,
             date = excluded.date,
             seq = excluded.seq",
        params![
            key,
            data.home_dc,
            data.updates_state.pts,
            data.updates_state.qts,
            data.updates_state.date,
            data.updates_state.seq
        ],
    )?;
    Ok(())
}

fn upsert_dc_option(
    conn: &Connection,
    key: &str,
    option: &DcOption,
) -> Result<(), DbSessionError> {
    conn.execute(
        "INSERT INTO mtproto_dc_options (bot_id, dc_id, ipv4, ipv6, auth_key)
         VALUES (?1, ?2, ?3, ?4, ?5)
         ON CONFLICT(bot_id, dc_id) DO UPDATE SET
             ipv4 = excluded.ipv4,
             ipv6 = excluded.ipv6,
             auth_key = excluded.auth_key",
        params![
            key,
            option.id,
            option.ipv4.to_string(),
            option.ipv6.to_string(),
            option.auth_key.as_ref().map(|key| key.as_slice())
        ],
    )?;
    Ok(())
}

fn sync_channel_states(
    conn: &Connection,
    key: &str,
    channels: &[ChannelState],
) -> Result<(), DbSessionError> {
    let tx = conn.unchecked_transaction()?;
    tx.execute("DELETE FROM mtproto_channel_states WHERE bot_id = ?1", params![key])?;
    for channel in channels {
        tx.execute(
            "INSERT INTO mtproto_channel_states (bot_id, channel_id, pts) VALUES (?1, ?2, ?3)",
            params![key, channel.id, channel.pts],
        )?;
    }
    tx.commit()?;
    Ok(())
}

impl Session for DbSession {
    type Error = DbSessionError;

    fn home_dc_id(&self) -> Result<i32, Self::Error> {
        Ok(self.lock()?.data.home_dc)
    }

    fn set_home_dc_id(&self, dc_id: i32) -> BoxFuture<'_, Result<(), Self::Error>> {
        Box::pin(async move {
            let mut inner = self.lock()?;
            inner.data.home_dc = dc_id;
            upsert_session_row(&inner.conn, &self.key, &inner.data)?;
            Ok(())
        })
    }

    fn dc_option(&self, dc_id: i32) -> Result<Option<DcOption>, Self::Error> {
        Ok(self.lock()?.data.dc_options.get(&dc_id).cloned())
    }

    fn set_dc_option(&self, dc_option: &DcOption) -> BoxFuture<'_, Result<(), Self::Error>> {
        let dc_option = dc_option.clone();
        Box::pin(async move {
            let mut inner = self.lock()?;
            let unchanged = inner.data.dc_options.get(&dc_option.id) == Some(&dc_option);
            inner
                .data
                .dc_options
                .insert(dc_option.id, dc_option.clone());
            if !unchanged {
                upsert_dc_option(&inner.conn, &self.key, &dc_option)?;
            }
            Ok(())
        })
    }

    fn peer(&self, peer: PeerId) -> BoxFuture<'_, Result<Option<PeerInfo>, Self::Error>> {
        Box::pin(async move { Ok(self.lock()?.data.peer_infos.get(&peer).cloned()) })
    }

    fn cache_peer(&self, peer: PeerInfo) -> BoxFuture<'_, Result<(), Self::Error>> {
        Box::pin(async move {
            let mut inner = self.lock()?;
            inner
                .data
                .peer_infos
                .entry(peer.id())
                .or_insert_with(|| peer.clone())
                .extend_info(&peer);

            // Persist the peer so a restart keeps MTProto sends resolvable:
            // sendMedia needs the peer's access hash and nothing else can
            // provide it for a bot (dialogs are forbidden for bots).
            let (kind, peer_id, auth, flags): (&str, i64, Option<i64>, Option<i64>) = match &peer {
                PeerInfo::User { id, auth, bot, is_self } => {
                    let mut flags = 0i64;
                    if bot.unwrap_or(false) {
                        flags |= 1;
                    }
                    if is_self.unwrap_or(false) {
                        flags |= 2;
                    }
                    ("user", *id, auth.map(|a| a.hash()), Some(flags))
                }
                PeerInfo::Chat { id } => ("chat", *id, None, None),
                PeerInfo::Channel { id, auth, kind } => {
                    let mut flags = 0i64;
                    match kind {
                        Some(ChannelKind::Broadcast) => flags |= 1,
                        Some(ChannelKind::Megagroup) => flags |= 2,
                        Some(ChannelKind::Gigagroup) => flags |= 4,
                        Some(ChannelKind::Community) => flags |= 8,
                        None => {}
                    }
                    ("channel", *id, auth.map(|a| a.hash()), Some(flags))
                }
            };
            inner.conn.execute(
                "INSERT INTO mtproto_peers (bot_id, kind, peer_id, auth, flags, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, CURRENT_TIMESTAMP)
                 ON CONFLICT(bot_id, kind, peer_id) DO UPDATE SET
                     auth = excluded.auth,
                     flags = excluded.flags,
                     updated_at = CURRENT_TIMESTAMP",
                params![self.key, kind, peer_id, auth, flags],
            )?;
            Ok(())
        })
    }

    fn updates_state(&self) -> BoxFuture<'_, Result<UpdatesState, Self::Error>> {
        Box::pin(async move { Ok(self.lock()?.data.updates_state.clone()) })
    }

    fn set_update_state(&self, update: UpdateState) -> BoxFuture<'_, Result<(), Self::Error>> {
        Box::pin(async move {
            let mut inner = self.lock()?;
            let mut channels_changed = false;
            match update {
                UpdateState::All(state) => {
                    inner.data.updates_state = state;
                    channels_changed = true;
                }
                UpdateState::Primary { pts, date, seq } => {
                    inner.data.updates_state.pts = pts;
                    inner.data.updates_state.date = date;
                    inner.data.updates_state.seq = seq;
                }
                UpdateState::Secondary { qts } => {
                    inner.data.updates_state.qts = qts;
                }
                UpdateState::Channel { id, pts } => {
                    inner.data.updates_state.channels.retain(|c| c.id != id);
                    inner
                        .data
                        .updates_state
                        .channels
                        .push(ChannelState { id, pts });
                    channels_changed = true;
                }
            }
            upsert_session_row(&inner.conn, &self.key, &inner.data)?;
            if channels_changed {
                sync_channel_states(&inner.conn, &self.key, &inner.data.updates_state.channels)?;
            }
            Ok(())
        })
    }
}
