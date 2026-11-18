//! Cleanup: which servers to leave and what to mute, from when the user last
//! wrote there, the unread volume and the mutes. `cleanup_plan` changes
//! nothing; `cleanup_apply` carries out only the items given, which the user
//! approved one by one.
//!
//! Item ids: `leave:<server>`, `mute:<server>`, `mute:<server>/<channel>`
//! and `read:<server>@<newest>` (mark the server's unread channels read, up
//! to the newest message the plan saw; anything newer stays unread).

use std::collections::HashSet;

use serde::{Deserialize, Serialize};

use crate::discord::types::{ChannelKind, DiscordError, Id, MuteTarget, SearchQuery, SearchScope};
use crate::domain::clock::{GapRng, ThreadRng};
use crate::domain::errors::{Error, Result};
use crate::domain::live::{ConnState, LiveState};
use crate::domain::paced_search::SearchPacer;
use crate::domain::pending::explicit_watch;
use crate::domain::resolve::channel_label;
use crate::domain::send_queue::halt_everything;
use crate::domain::voice::observe_own;
use crate::mcp::state::Ctx;
use crate::store::{Store, WatchLevel, WatchMap};

const DAY_MS: i64 = 86_400_000;
/// Leave a server with no post in this long (and no full-watch channel).
pub const LEAVE_AFTER_DAYS: i64 = 180;
/// Mute a server with more unread channels than this and no post in `MUTE_AFTER_DAYS`.
pub const MUTE_UNREAD_CHANNELS: usize = 20;
pub const MUTE_AFTER_DAYS: i64 = 60;
/// How long a searched "last post" is trusted.
pub const LAST_POST_TTL_MS: i64 = DAY_MS;
/// Searches per `cleanup_plan` call (each is 3–6 s apart): the rest wait for the next call.
pub const MAX_LOOKUPS_PER_PLAN: usize = 8;
/// Channel mutes offered per noisy server the user still posts in.
pub const CHANNEL_MUTE_SUGGESTIONS: usize = 5;
/// Random wait between two applied actions: at least 2 s.
pub const APPLY_GAP_MIN_MS: u64 = 2_000;
pub const APPLY_GAP_MAX_MS: u64 = 4_000;
/// `ack_bulk` takes at most this many channels per request.
const ACK_CHUNK: usize = 100;
const KV_LAST_POST: &str = "cleanup_last_post";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Suggestion {
    Leave,
    Mute,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PlanItem {
    /// Pass this to cleanup_apply once the user approved it.
    pub item: String,
    pub does: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct ServerEntry {
    pub id: Id,
    pub label: String,
    pub member_count: Option<u32>,
    pub muted_in_discord: bool,
    pub unread_channels: usize,
    /// Date of the user's last post there; `None` with `last_post_known` means never.
    pub my_last_post: Option<String>,
    /// False until a search (or the local log) told when the user last posted.
    pub last_post_known: bool,
    /// A server or channel there is on full watch in tritone.
    pub full_watch: bool,
    pub owner: bool,
    pub suggestion: Option<Suggestion>,
    pub items: Vec<PlanItem>,
}

#[derive(Debug, Clone, Serialize)]
pub struct CleanupPlan {
    /// Suggestions first (leave, then mute), then by unread channels.
    pub servers: Vec<ServerEntry>,
    /// Servers whose last post is still unknown: call cleanup_plan again to look them up.
    pub lookups_pending: usize,
    pub note: String,
}

/// A searched last post, cached in `kv` under `cleanup_last_post:<server>`.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
struct LastPost {
    checked_at: i64,
    /// Unix ms; `None` = no post found.
    last: Option<i64>,
}

fn kv_key(guild: Id) -> String {
    format!("{KV_LAST_POST}:{guild}")
}

fn cached_last_post(store: &Store, guild: Id, now: i64) -> Result<Option<LastPost>> {
    let Some(raw) = store.kv_get(&kv_key(guild))? else { return Ok(None) };
    let cached: Option<LastPost> = serde_json::from_str(&raw).ok();
    Ok(cached.filter(|c| now - c.checked_at < LAST_POST_TTL_MS))
}

fn date(ms: i64) -> String {
    chrono::DateTime::from_timestamp_millis(ms).map_or_else(|| ms.to_string(), |t| t.format("%Y-%m-%d").to_string())
}

/// A server or one of its channels is set to full watch in tritone.
fn full_watch_in_guild(watch: &WatchMap, live: &LiveState, guild: Id) -> bool {
    watch.guilds.get(&guild) == Some(&WatchLevel::Full)
        || watch.channels.iter().any(|(id, level)| {
            *level == WatchLevel::Full && live.channel(*id).is_some_and(|c| c.guild_id == Some(guild))
        })
}

/// The channel's tritone level (its own, its parent's, its server's) is full.
fn channel_full_watch(watch: &WatchMap, live: &LiveState, channel: Id) -> bool {
    live.channel(channel).is_some_and(|c| explicit_watch(&c, watch) == Some(WatchLevel::Full))
}

/// Visible, non-category channels of a server with unread messages, newest first.
fn unread_channels(live: &LiveState, guild: Id) -> Vec<(Id, Id)> {
    let mut out: Vec<(Id, Id)> = live
        .channels()
        .into_iter()
        .filter(|c| c.guild_id == Some(guild) && c.kind != ChannelKind::Category)
        .filter(|c| live.is_unread(c.id) && live.can_view(c.id))
        .filter_map(|c| c.last_message_id.map(|m| (c.id, m)))
        .collect();
    out.sort_by_key(|(_, m)| std::cmp::Reverse(*m));
    out
}

/// The last post a search found, cached for a day. `Ok(None)` means the
/// search couldn't tell (no access, or the index never got ready).
async fn look_up_last_post(ctx: &Ctx, pacer: &SearchPacer, guild: Id, me: Id, now: i64) -> Result<Option<LastPost>> {
    let q = SearchQuery { author_id: Some(me), ..SearchQuery::default() };
    match pacer.search(ctx, SearchScope::Guild(guild), &q).await {
        Ok(page) => {
            observe_own(&ctx.store, &page.messages, me, Some(guild), now)?;
            let found = LastPost { checked_at: now, last: page.messages.iter().map(|m| m.timestamp_ms).max() };
            let json = serde_json::to_string(&found).map_err(|e| Error::Store(e.into()))?;
            ctx.store.kv_set(&kv_key(guild), &json)?;
            Ok(Some(found))
        }
        Err(Error::Discord(e)) if !e.halts() => {
            tracing::warn!(kind = e.kind(), "cleanup lookup failed");
            Ok(None)
        }
        Err(e) => Err(e),
    }
}

struct Facts {
    id: Id,
    unread: Vec<(Id, Id)>,
    muted: bool,
    full_watch: bool,
    owner: bool,
    /// `Some(last)` when known, where `last` is `None` for never.
    last_post: Option<Option<i64>>,
    has_local: bool,
}

fn suggest(f: &Facts, now: i64) -> Option<Suggestion> {
    let last = f.last_post?;
    let quiet_for = |days: i64| last.is_none_or(|t| t < now - days * DAY_MS);
    if quiet_for(LEAVE_AFTER_DAYS) && !f.full_watch && !f.owner {
        return Some(Suggestion::Leave);
    }
    if quiet_for(MUTE_AFTER_DAYS) && f.unread.len() > MUTE_UNREAD_CHANNELS && !f.muted {
        return Some(Suggestion::Mute);
    }
    None
}

fn plan_items(
    store: &Store,
    live: &LiveState,
    watch: &WatchMap,
    f: &Facts,
    s: Option<Suggestion>,
    now: i64,
) -> Result<Vec<PlanItem>> {
    let g = f.id;
    let mut items = Vec::new();
    match s {
        Some(Suggestion::Leave) => items.push(PlanItem { item: format!("leave:{g}"), does: "leave the server".into() }),
        Some(Suggestion::Mute) => {
            items.push(PlanItem { item: format!("mute:{g}"), does: "mute the whole server in Discord".into() })
        }
        None => {}
    }
    // A noisy server the user still posts in: offer muting the busy channels they never write in.
    let posts_recently = f.last_post.flatten().is_some_and(|t| t >= now - MUTE_AFTER_DAYS * DAY_MS);
    if s.is_none() && !f.muted && !f.full_watch && f.unread.len() > MUTE_UNREAD_CHANNELS && posts_recently {
        let mine = store.own_channels_in_guild(g)?;
        let mut offered = 0;
        for (c, _) in &f.unread {
            if offered == CHANNEL_MUTE_SUGGESTIONS {
                break;
            }
            if mine.contains(c) || live.discord_muted(*c, now) || channel_full_watch(watch, live, *c) {
                continue;
            }
            let name = live.channel(*c).and_then(|c| c.name).unwrap_or_else(|| "unknown-channel".into());
            items.push(PlanItem { item: format!("mute:{g}/{c}"), does: format!("mute #{name} in Discord") });
            offered += 1;
        }
    }
    if !f.unread.is_empty() {
        let n = f.unread.len();
        // Unread is sorted newest first: the plan-time cutoff.
        let cutoff = f.unread[0].1;
        items.push(PlanItem { item: format!("read:{g}@{cutoff}"), does: format!("mark {n} unread channels read") });
    }
    Ok(items)
}

/// The cleanup plan, with the real pacer.
pub async fn cleanup_plan(ctx: &Ctx) -> Result<CleanupPlan> {
    cleanup_plan_with(ctx, SearchPacer::global()).await
}

/// Lists every server with what tritone knows, and suggests:
/// - `leave` when the user hasn't posted in 180 days, no channel there is on
///   full watch, and they don't own it;
/// - `mute` when more than 20 channels are unread and there's no post in 60 days;
/// - otherwise nothing.
///
/// The last post comes from the contribution log when it shows one in the
/// last 60 days, else from `search(author_id = me)` (one paced search per
/// server, cached a day). At most `MAX_LOOKUPS_PER_PLAN` searches run per
/// call; servers still unknown get no suggestion.
pub async fn cleanup_plan_with(ctx: &Ctx, pacer: &SearchPacer) -> Result<CleanupPlan> {
    let me = ctx.live.me().ok_or(Error::NotConnected)?.id;
    let now = ctx.timer.now_ms();
    let watch = ctx.store.all_watch()?;
    let mut facts = Vec::new();
    for g in ctx.live.guilds() {
        let local = ctx.store.last_own_post_in_guild(g.id)?;
        let cached = cached_last_post(&ctx.store, g.id, now)?;
        let recent_local = local.filter(|t| *t >= now - MUTE_AFTER_DAYS * DAY_MS);
        let last_post = match (recent_local, cached) {
            (Some(t), _) => Some(Some(t)),
            (None, Some(c)) => Some(c.last.max(local)),
            (None, None) => None,
        };
        facts.push(Facts {
            id: g.id,
            unread: unread_channels(&ctx.live, g.id),
            muted: ctx.live.settings(Some(g.id)).is_some_and(|s| s.muted.active(now)),
            full_watch: full_watch_in_guild(&watch, &ctx.live, g.id),
            owner: g.owner_id == me,
            last_post,
            has_local: local.is_some(),
        });
    }

    // Look up the servers most likely to need a suggestion first.
    let mut todo: Vec<usize> = (0..facts.len()).filter(|i| facts[*i].last_post.is_none()).collect();
    todo.sort_by_key(|i| (facts[*i].has_local, std::cmp::Reverse(facts[*i].unread.len()), facts[*i].id));
    for i in todo.into_iter().take(MAX_LOOKUPS_PER_PLAN) {
        if let Some(found) = look_up_last_post(ctx, pacer, facts[i].id, me, now).await? {
            let local = ctx.store.last_own_post_in_guild(facts[i].id)?;
            facts[i].last_post = Some(found.last.max(local));
        }
    }

    let mut servers = Vec::new();
    for f in &facts {
        let s = suggest(f, now);
        let g = ctx.live.guild(f.id);
        servers.push(ServerEntry {
            id: f.id,
            label: g.as_ref().map_or_else(|| "unknown server".into(), |g| g.name.clone()),
            member_count: g.and_then(|g| g.member_count),
            muted_in_discord: f.muted,
            unread_channels: f.unread.len(),
            my_last_post: f.last_post.flatten().map(date),
            last_post_known: f.last_post.is_some(),
            full_watch: f.full_watch,
            owner: f.owner,
            suggestion: s,
            items: plan_items(&ctx.store, &ctx.live, &watch, f, s, now)?,
        });
    }
    let rank = |s: &ServerEntry| match s.suggestion {
        Some(Suggestion::Leave) => 0,
        Some(Suggestion::Mute) => 1,
        None => 2,
    };
    servers.sort_by_key(|s| (rank(s), std::cmp::Reverse(s.unread_channels), s.id));
    let lookups_pending = servers.iter().filter(|s| !s.last_post_known).count();
    let note = if lookups_pending > 0 {
        format!(
            "{lookups_pending} servers still need a last-post lookup (a few per call, paced): call cleanup_plan again for them. Nothing was changed."
        )
    } else {
        "Nothing was changed. Each item needs the user's explicit OK before cleanup_apply.".into()
    };
    Ok(CleanupPlan { servers, lookups_pending, note })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ApplyStatus {
    Done,
    Failed,
    Skipped,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ApplyResult {
    pub item: String,
    pub status: ApplyStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    /// For `read:`: channels with messages newer than the plan, left unread
    /// for the user to see.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub unseen: Vec<UnseenChannel>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct UnseenChannel {
    pub channel_id: String,
    pub channel: String,
}

/// What one action did.
struct Outcome {
    detail: String,
    unseen: Vec<UnseenChannel>,
}

impl From<&str> for Outcome {
    fn from(detail: &str) -> Outcome {
        Outcome { detail: detail.into(), unseen: vec![] }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum Action {
    Leave(Id),
    MuteGuild(Id),
    MuteChannel(Id, Id),
    /// The server, and the newest message the plan saw there.
    Read(Id, Id),
}

fn parse_item(item: &str, live: &LiveState) -> std::result::Result<Action, String> {
    let bad = || {
        format!(
            "\"{item}\" is not a cleanup item (leave:<server>, mute:<server>, mute:<server>/<channel>, read:<server>@<newest>)"
        )
    };
    let (verb, rest) = item.trim().split_once(':').ok_or_else(bad)?;
    let (g, c) = match rest.split_once('/') {
        Some((g, c)) => (g, Some(c)),
        None => (rest, None),
    };
    let (g, cutoff) = match g.split_once('@') {
        Some((g, at)) => (g, Some(at.parse::<Id>().map_err(|_| bad())?)),
        None => (g, None),
    };
    let g: Id = g.parse().map_err(|_| bad())?;
    if live.guild(g).is_none() {
        return Err(format!("server {g} is not one the user is in"));
    }
    let c: Option<Id> = c.map(|c| c.parse().map_err(|_| bad())).transpose()?;
    if let Some(c) = c
        && live.channel(c).is_none_or(|ch| ch.guild_id != Some(g))
    {
        return Err(format!("channel {c} is not in server {g}"));
    }
    match (verb, c, cutoff) {
        ("leave", None, None) => Ok(Action::Leave(g)),
        ("mute", None, None) => Ok(Action::MuteGuild(g)),
        ("mute", Some(c), None) => Ok(Action::MuteChannel(g, c)),
        ("read", None, Some(at)) => Ok(Action::Read(g, at)),
        _ => Err(bad()),
    }
}

/// Spaces requests: the first goes at once, each later one a random
/// `APPLY_GAP_MIN_MS..=APPLY_GAP_MAX_MS` after the previous one.
struct Pace<'a> {
    ctx: &'a Ctx,
    rng: &'a dyn GapRng,
    last: Option<i64>,
}

impl Pace<'_> {
    async fn wait(&mut self) {
        if let Some(l) = self.last {
            let gap = self.rng.gap_ms(APPLY_GAP_MIN_MS, APPLY_GAP_MAX_MS) as i64;
            self.ctx.timer.sleep_until(l + gap).await;
        }
        self.last = Some(self.ctx.timer.now_ms());
    }
}

/// Runs one action, pacing every request.
async fn run_action(ctx: &Ctx, action: Action, pace: &mut Pace<'_>) -> std::result::Result<Outcome, DiscordError> {
    match action {
        Action::Leave(g) => {
            pace.wait().await;
            ctx.api.leave_guild(g).await.map(|_| "left the server".into())
        }
        Action::MuteGuild(g) => {
            pace.wait().await;
            ctx.api.set_mute(MuteTarget::Guild(g), true).await.map(|_| "server muted in Discord".into())
        }
        Action::MuteChannel(g, c) => {
            pace.wait().await;
            let target = MuteTarget::Channel { guild: Some(g), channel: c };
            ctx.api.set_mute(target, true).await.map(|_| "channel muted in Discord".into())
        }
        Action::Read(g, cutoff) => mark_read_until(ctx, g, cutoff, pace).await,
    }
}

/// Acks the server's unread channels whose newest message is no newer than
/// `cutoff`, the newest the user approved in the plan. A channel with
/// anything newer is left unread and reported, so a mention that arrived
/// after the OK is never hidden.
async fn mark_read_until(
    ctx: &Ctx,
    g: Id,
    cutoff: Id,
    pace: &mut Pace<'_>,
) -> std::result::Result<Outcome, DiscordError> {
    let unread = unread_channels(&ctx.live, g);
    let (ack, newer): (Vec<_>, Vec<_>) = unread.into_iter().partition(|(_, m)| *m <= cutoff);
    for chunk in ack.chunks(ACK_CHUNK) {
        pace.wait().await;
        ctx.api.ack_bulk(chunk).await?;
        for (c, m) in chunk {
            ctx.live.note_acked(*c, *m);
        }
    }
    let unseen: Vec<UnseenChannel> = newer
        .iter()
        .map(|(c, _)| UnseenChannel {
            channel_id: c.to_string(),
            channel: ctx.live.channel(*c).map(|ch| channel_label(&ch, &ctx.live)).unwrap_or_default(),
        })
        .collect();
    let mut detail = format!("marked {} channels read", ack.len());
    if !unseen.is_empty() {
        detail.push_str(&format!(
            "; {} got messages after the plan and stay unread (see unseen): show them to the user",
            unseen.len()
        ));
    }
    Ok(Outcome { detail, unseen })
}

/// Applies the items, with the real random gap.
pub async fn cleanup_apply(ctx: &Ctx, items: &[String]) -> Result<Vec<ApplyResult>> {
    cleanup_apply_with(ctx, items, &ThreadRng).await
}

/// Runs each item exactly as given, in order, one at a time, with a random
/// 2–4 s between requests. A malformed or unknown item fails on its own; a
/// repeated one is skipped. A halting error (CAPTCHA, lock, block, dead
/// login) halts tritone and skips the rest.
pub async fn cleanup_apply_with(ctx: &Ctx, items: &[String], rng: &dyn GapRng) -> Result<Vec<ApplyResult>> {
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    let mut pace = Pace { ctx, rng, last: None };
    let mut stopped: Option<String> = None;
    for item in items {
        let result =
            |status, detail: String| ApplyResult { item: item.clone(), status, detail: Some(detail), unseen: vec![] };
        if let Some(why) = &stopped {
            out.push(result(ApplyStatus::Skipped, format!("not run: {why}")));
            continue;
        }
        if let ConnState::Halted(e) = ctx.live.connection() {
            stopped = Some(e.to_string());
            out.push(result(ApplyStatus::Skipped, format!("not run: {e}")));
            continue;
        }
        let action = match parse_item(item, &ctx.live) {
            Ok(a) => a,
            Err(why) => {
                out.push(result(ApplyStatus::Failed, why));
                continue;
            }
        };
        if !seen.insert(action) {
            out.push(result(ApplyStatus::Skipped, "already in this list".into()));
            continue;
        }
        match run_action(ctx, action, &mut pace).await {
            Ok(done) => out.push(ApplyResult { unseen: done.unseen, ..result(ApplyStatus::Done, done.detail) }),
            Err(e) => {
                if e.halts() {
                    halt_everything(&ctx.live, &e);
                    stopped = Some(e.to_string());
                }
                tracing::warn!(kind = e.kind(), "cleanup action failed");
                out.push(result(ApplyStatus::Failed, e.to_string()));
            }
        }
    }
    Ok(out)
}

#[cfg(test)]
#[path = "cleanup_tests.rs"]
mod tests;
