//! Turns what Claude calls a conversation (an id, a link, `#name`,
//! `server/#name`, `@user`, part of a name) into a channel id. Several
//! matches are an `Ambiguous` error listing them: never pick one.

use crate::discord::types::{Channel, ChannelKind, Id};
use crate::domain::errors::{Error, Result};
use crate::domain::live::LiveState;

/// The most candidates an `Ambiguous` error lists.
const MAX_CANDIDATES: usize = 10;

struct Candidate {
    id: Id,
    label: String,
    /// The whole name matched, not just part of it.
    exact: bool,
}

pub fn resolve_channel(query: &str, live: &LiveState) -> Result<Id> {
    let q = query.trim();
    if q.is_empty() || q == "#" || q == "@" {
        return Err(Error::Invalid(
            "name a channel: an id, a discord.com/channels/... link, #name, server/#name, @user or part of a name"
                .into(),
        ));
    }
    if let Some(id) = snowflake(q) {
        return visible(id, q, live);
    }
    if let Some(id) = link_channel(q)? {
        return visible(id, q, live);
    }
    let found = if let Some(user) = q.strip_prefix('@') {
        dm_matches(user, live)
    } else if let Some((server, name)) = q.split_once("/#") {
        channel_matches(Some(server.trim()), name, live)
    } else if let Some(name) = q.strip_prefix('#') {
        channel_matches(None, name, live)
    } else {
        let mut all = channel_matches(None, q, live);
        all.extend(dm_matches(q, live));
        all.extend(group_dm_matches(q, live));
        all
    };
    pick(q, found)
}

/// A bare decimal id.
pub(crate) fn snowflake(s: &str) -> Option<Id> {
    if s.is_empty() || !s.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    s.parse().ok().map(Id)
}

/// The channel of a `discord.com/channels/{guild|@me}/{channel}[/{message}]`
/// link (any subdomain), `None` when `q` isn't such a link.
fn link_channel(q: &str) -> Result<Option<Id>> {
    const MARK: &str = "discord.com/channels/";
    let Some(at) = q.find(MARK) else { return Ok(None) };
    let mut parts = q[at + MARK.len()..].split(['/', '?', '#']);
    let _guild = parts.next();
    match parts.next().and_then(snowflake) {
        Some(id) => Ok(Some(id)),
        None => Err(Error::Invalid(format!("\"{q}\" is not a discord.com/channels/<server>/<channel> link"))),
    }
}

/// Known channels must be visible. An unknown id (e.g. a thread tritone
/// hasn't joined) goes through, and Discord answers for it.
fn visible(id: Id, q: &str, live: &LiveState) -> Result<Id> {
    if live.channel(id).is_some() && !live.can_view(id) {
        return Err(Error::NotFound(q.to_string()));
    }
    Ok(id)
}

fn name_match(name: &str, query: &str) -> Option<bool> {
    let (n, q) = (name.to_lowercase(), query.to_lowercase());
    n.contains(&q).then_some(n == q)
}

/// Server channels and threads by name (categories and nameless channels
/// never match), optionally only in servers whose name contains `server`.
fn channel_matches(server: Option<&str>, name: &str, live: &LiveState) -> Vec<Candidate> {
    let name = name.trim();
    if name.is_empty() {
        return vec![];
    }
    let mut out = Vec::new();
    for c in live.channels() {
        let (Some(gid), Some(cname)) = (c.guild_id, c.name.as_deref()) else { continue };
        if c.kind == ChannelKind::Category {
            continue;
        }
        let Some(exact) = name_match(cname, name) else { continue };
        let Some(g) = live.guild(gid) else { continue };
        if server.is_some_and(|s| name_match(&g.name, s).is_none()) || !live.can_view(c.id) {
            continue;
        }
        out.push(Candidate { id: c.id, label: format!("{} › #{} ({})", g.name, cname, c.id), exact });
    }
    out
}

/// 1:1 DMs whose person's username or display name contains `user`.
fn dm_matches(user: &str, live: &LiveState) -> Vec<Candidate> {
    let mut out = Vec::new();
    for c in live.channels().into_iter().filter(|c| c.kind == ChannelKind::Dm) {
        let Some(u) = c.recipients.first() else { continue };
        let by_name = name_match(&u.username, user);
        let by_display = u.display_name.as_deref().and_then(|d| name_match(d, user));
        let Some(exact) = by_name.into_iter().chain(by_display).reduce(|a, b| a || b) else { continue };
        out.push(Candidate { id: c.id, label: format!("@{} (DM, {})", u.label(), c.id), exact });
    }
    out
}

fn group_dm_matches(q: &str, live: &LiveState) -> Vec<Candidate> {
    live.channels()
        .into_iter()
        .filter(|c| c.kind == ChannelKind::GroupDm)
        .filter_map(|c| {
            let exact = name_match(c.name.as_deref()?, q)?;
            Some(Candidate { id: c.id, label: format!("{} (group DM, {})", group_label(&c), c.id), exact })
        })
        .collect()
}

/// A group DM's name, else its people.
pub(crate) fn group_label(c: &Channel) -> String {
    c.name.clone().unwrap_or_else(|| c.recipients.iter().map(|u| u.label()).collect::<Vec<_>>().join(", "))
}

/// Exact names win over partial ones; one left is the answer.
fn pick(q: &str, mut found: Vec<Candidate>) -> Result<Id> {
    if found.iter().any(|c| c.exact) {
        found.retain(|c| c.exact);
    }
    match found.len() {
        0 => Err(Error::NotFound(q.to_string())),
        1 => Ok(found[0].id),
        _ => {
            let mut candidates: Vec<String> = found.into_iter().map(|c| c.label).collect();
            candidates.sort();
            candidates.truncate(MAX_CANDIDATES);
            Err(Error::Ambiguous { query: q.to_string(), candidates })
        }
    }
}

#[cfg(test)]
#[path = "resolve_tests.rs"]
mod tests;
