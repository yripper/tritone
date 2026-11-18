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
}

/// Discord's bulk ack takes at most this many channels.
const ACK_CHUNK: usize = 100;

/// Marks channels read up to their last message, with Discord's bulk ack in
/// chunks of 100. Channels with nothing unread cost no call.
pub async fn mark_read(ctx: &Ctx, channels: &[Id]) -> Result<MarkReport> {
    let mut report = MarkReport::default();
    let mut items = Vec::new();
    for &id in channels {
        match ctx.live.channel(id).and_then(|c| c.last_message_id).filter(|_| ctx.live.is_unread(id)) {
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
/// channel; which one comes from the live model. Returns the kind.
pub fn set_watch(ctx: &Ctx, target: Id, level: Option<WatchLevel>) -> Result<TargetKind> {
    let (guild, channel) = (ctx.live.guild(target).is_some(), ctx.live.channel(target).is_some());
    // Older servers' default channel has the server's own id, and watch
    // levels are stored by id alone: a level here would apply to both.
    if guild && channel {
        return Err(Error::Invalid(format!(
            "{target} is both a server and its default channel (same id): tritone can't set a watch level on it yet. Set it on the other channels, or mute it in Discord"
        )));
    }
    let kind = if guild {
        TargetKind::Guild
    } else if channel {
        TargetKind::Channel
    } else {
        return Err(Error::NotFound(target.to_string()));
    };
    ctx.store.set_watch(target, kind, level)?;
    Ok(kind)
}

#[cfg(test)]
#[path = "triage_tests.rs"]
mod tests;
