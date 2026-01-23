use anyhow::{anyhow, Result};
use grammers_client::Client;
use grammers_tl_types as tl;
use std::sync::Arc;
use tokio::sync::Mutex;

pub async fn resolve_peer(
    client: &Arc<Mutex<Client>>,
    chat_id: i64,
    username: Option<&str>,
) -> Result<tl::enums::InputPeer> {
    // Access the actual client through the mutex
    let actual_client = client.lock().await;

    // 1) Basic group: negative id, but not a channel (-100...)
    if chat_id < 0 && !format!("{}", chat_id).starts_with("-100") {
        let raw_id = chat_id.abs() as i32; // basic group id without -100 prefix
        return Ok(tl::enums::InputPeer::Chat(tl::types::InputPeerChat { chat_id: raw_id as i64 }));
    }

    // 2) User or channel/supergroup: resolve by username (dialogs are forbidden for bots)
    if let Some(un) = username {
        // contacts.resolveUsername is available for bots
        let res = actual_client.invoke(&tl::functions::contacts::ResolveUsername { username: un.to_string() }).await.map_err(|e| anyhow!("contacts.resolveUsername failed for @{}: {:?}", un, e))?;
        let tl::enums::contacts::ResolvedPeer::Peer(r) = res;
            // Trying to match the returned peer with users/chats to get the access_hash
            match r.peer {
                tl::enums::Peer::User(pu) => {
                    if let Some(u) = r.users.into_iter().find_map(|u| match u {
                        tl::enums::User::User(u) if u.id == pu.user_id => Some(u),
                        _ => None
                    }) {
                        let hash = u.access_hash.ok_or_else(|| anyhow!("user access_hash missing"))?;
                        return Ok(tl::enums::InputPeer::User(tl::types::InputPeerUser {
                            user_id: pu.user_id,
                            access_hash: hash,
                        }));
                    }
                }
                tl::enums::Peer::Channel(pc) => {
                    if let Some(c) = r.chats.into_iter().find_map(|c| match c {
                        tl::enums::Chat::Channel(c) if c.id == pc.channel_id => Some(c),
                        _ => None
                    }) {
                        let hash = c.access_hash.ok_or_else(|| anyhow!("channel access_hash missing"))?;
                        return Ok(tl::enums::InputPeer::Channel(tl::types::InputPeerChannel {
                            channel_id: pc.channel_id,
                            access_hash: hash,
                        }));
                    }
                }
                tl::enums::Peer::Chat(pg) => {
                    return Ok(tl::enums::InputPeer::Chat(tl::types::InputPeerChat {
                        chat_id: pg.chat_id,
                    }));
                            }
                        }
                        return Err(anyhow!("Failed to map ResolvedPeer to InputPeer for @{}", un));    }
    Err(anyhow!("Cannot resolve peer {} as bot without username; dialogs are forbidden for bots", chat_id))
}