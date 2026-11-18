//! Triage: mark read, snooze, watch levels.

use serde::Serialize;

use crate::discord::types::Id;
use crate::domain::errors::{Error, Result};
use crate::mcp::state::Ctx;
use crate::store::{TargetKind, WatchLevel};

#[derive(Debug, Clone, Default, Serialize)]
pub struct MarkReport {
    /// Channels now marked read.
    pub marked: Vec<Id>,
    /// Channels that had nothing unread.
    pub already_read: Vec<Id>,
    /// Ids tritone doesn't know as channels: nothing to ack.
    pub not_found: Vec<Id>,
}

/// Discord's bulk ack takes at most this many channels.
const ACK_CHUNK: usize = 100;

/// Marks channels read up to their last message, with Discord's bulk ack in
/// chunks of 100. Channels with nothing unread cost no call.
pub async fn mark_read(ctx: &Ctx, channels: &[Id]) -> Result<MarkReport> {
    let mut report = MarkReport::default();
    let mut items = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for &id in channels.iter().filter(|id| seen.insert(**id)) {
        let Some(c) = ctx.live.channel(id) else {
            report.not_found.push(id);
            continue;
        };
        match c.last_message_id.filter(|_| ctx.live.is_unread(id)) {
            Some(last) => items.push((id, last)),
            None => report.already_read.push(id),
        }
    }
    for chunk in items.chunks(ACK_CHUNK) {
        ctx.api.ack_bulk(chunk).await?;
        for &(channel, message) in chunk {
            ctx.live.note_acked(channel, message);
            report.marked.push(channel);
        }
    }
    Ok(report)
}

/// Hides a channel from pending until `until_ms` (Unix ms). It stays unread in Discord.
pub fn snooze(ctx: &Ctx, channel: Id, until_ms: i64) -> Result<()> {
    if until_ms <= ctx.timer.now_ms() {
        return Err(Error::Invalid("the snooze time is in the past: give a future time".into()));
    }
    ctx.store.set_snooze(channel, Some(until_ms))?;
    Ok(())
}

pub fn unsnooze(ctx: &Ctx, channel: Id) -> Result<()> {
    ctx.store.set_snooze(channel, None)?;
    Ok(())
}

/// Sets (or with `None` clears) the tritone watch level of a server or a
/// channel. `kind` says which when the caller knows (a `#name` is a channel);
/// otherwise it comes from the live model. An older server's default channel
/// shares the server's id, so a bare id that is both needs `kind`.
pub fn set_watch(ctx: &Ctx, target: Id, kind: Option<TargetKind>, level: Option<WatchLevel>) -> Result<TargetKind> {
    let (guild, channel) = (ctx.live.guild(target).is_some(), ctx.live.channel(target).is_some());
    let kind = match (kind, guild, channel) {
        (Some(TargetKind::Guild), true, _) => TargetKind::Guild,
        (Some(TargetKind::Channel), _, true) => TargetKind::Channel,
        (Some(_), _, _) => return Err(Error::NotFound(target.to_string())),
        (None, true, true) => {
            return Err(Error::Invalid(format!(
                "{target} is both a server and its default channel: use the server name for the server, or #name for the channel"
            )));
        }
        (None, true, false) => TargetKind::Guild,
        (None, false, true) => TargetKind::Channel,
        (None, false, false) => return Err(Error::NotFound(target.to_string())),
    };
    ctx.store.set_watch(target, kind, level)?;
    Ok(kind)
}

#[cfg(test)]
#[path = "triage_tests.rs"]
mod tests;
