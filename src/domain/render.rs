//! Turns Discord messages into what Claude reads, and Claude's `@name`
//! mentions back into Discord's `<@id>` form.
//!
//! Message syntax: https://docs.discord.food/reference#message-formatting
//! Code blocks and inline code are left verbatim, as the Discord client does.

use chrono::{DateTime, SecondsFormat};
use serde::Serialize;

use crate::discord::types::{Id, Message, UserRef};
use crate::domain::live::LiveState;
use crate::domain::resolve::snowflake;

/// A message as the tools return it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RenderedMessage {
    pub id: String,
    /// The author's display name, else username.
    pub from: String,
    pub from_id: String,
    /// RFC 3339, UTC.
    pub at: String,
    pub text: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reply_to: Option<ReplyRef>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub reactions: Vec<(String, u32)>,
    pub link: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ReplyRef {
    pub message_id: String,
    /// "unknown-user" when Discord didn't say (e.g. the message was deleted).
    pub author: String,
}

/// Longest `<...>` token worth parsing: `<a:` + a 32-char name + `:` + a snowflake.
const MAX_TOKEN: usize = 64;

pub fn render_message(m: &Message, live: &LiveState) -> RenderedMessage {
    let guild = m.guild_id.or_else(|| live.channel(m.channel_id).and_then(|c| c.guild_id));
    let mut text = map_outside_code(&m.content, |s| replace_tokens(s, live, guild));
    let attachments = m.attachments.iter().map(|a| format!("[{a}]"));
    let embeds = m.embeds.iter().map(|e| format!("[embed: {e}]"));
    for p in attachments.chain(embeds) {
        if !text.is_empty() {
            text.push('\n');
        }
        text.push_str(&p);
    }
    RenderedMessage {
        id: m.id.to_string(),
        from: m.author.label().to_string(),
        from_id: m.author.id.to_string(),
        at: rfc3339(m.timestamp_ms),
        text,
        reply_to: m.reply_to.map(|id| ReplyRef {
            message_id: id.to_string(),
            author: m
                .reply_to_author
                .and_then(|a| live.user(a))
                .map_or_else(|| "unknown-user".to_string(), |u| u.label().to_string()),
        }),
        reactions: m.reactions.clone(),
        link: message_link(guild, m.channel_id, m.id),
    }
}

/// `https://discord.com/channels/{guild|@me}/{channel}/{message}`.
pub fn message_link(guild: Option<Id>, channel: Id, message: Id) -> String {
    let g = guild.map_or_else(|| "@me".to_string(), |g| g.to_string());
    format!("https://discord.com/channels/{g}/{channel}/{message}")
}

pub(crate) fn rfc3339(ms: i64) -> String {
    DateTime::from_timestamp_millis(ms).map_or_else(String::new, |d| d.to_rfc3339_opts(SecondsFormat::Secs, true))
}

/// Applies `f` to the text outside ``` fences and `inline code`. An unclosed
/// fence or backtick is plain text, as in the Discord client.
fn map_outside_code(text: &str, f: impl Fn(&str) -> String) -> String {
    let mut out = String::with_capacity(text.len());
    let mut plain_start = 0;
    let mut i = 0;
    while i < text.len() {
        let rest = &text[i..];
        let delim = if rest.starts_with("```") {
            "```"
        } else if rest.starts_with('`') {
            "`"
        } else {
            ""
        };
        let close = (!delim.is_empty()).then(|| rest[delim.len()..].find(delim)).flatten();
        match close {
            Some(at) => {
                out.push_str(&f(&text[plain_start..i]));
                let end = i + delim.len() + at + delim.len();
                out.push_str(&text[i..end]);
                i = end;
                plain_start = end;
            }
            // Backticks are ASCII, so stepping one byte past a lone one is safe;
            // otherwise step a whole char.
            None => i += rest.chars().next().map_or(1, char::len_utf8),
        }
    }
    out.push_str(&f(&text[plain_start..]));
    out
}

/// Rewrites `<@id>`, `<@!id>`, `<#id>`, `<@&id>` and custom emoji. Anything
/// else in angle brackets is left as written.
fn replace_tokens(s: &str, live: &LiveState, guild: Option<Id>) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(lt) = rest.find('<') {
        out.push_str(&rest[..lt]);
        let after = &rest[lt + 1..];
        let token = after.find('>').filter(|&gt| gt <= MAX_TOKEN).and_then(|gt| {
            let label = token_label(&after[..gt], live, guild)?;
            Some((label, gt))
        });
        match token {
            Some((label, gt)) => {
                out.push_str(&label);
                rest = &after[gt + 1..];
            }
            None => {
                out.push('<');
                rest = after;
            }
        }
    }
    out.push_str(rest);
    out
}

fn token_label(inner: &str, live: &LiveState, guild: Option<Id>) -> Option<String> {
    if let Some(id) = inner.strip_prefix("@&") {
        return snowflake(id).map(|id| role_label(id, live, guild));
    }
    if let Some(id) = inner.strip_prefix("@!").or_else(|| inner.strip_prefix('@')) {
        let id = snowflake(id)?;
        return Some(live.user(id).map_or_else(|| "@unknown-user".to_string(), |u| format!("@{}", u.label())));
    }
    if let Some(id) = inner.strip_prefix('#') {
        let id = snowflake(id)?;
        let name = live.channel(id).and_then(|c| c.name);
        return Some(name.map_or_else(|| "#unknown-channel".to_string(), |n| format!("#{n}")));
    }
    let emoji = inner.strip_prefix("a:").or_else(|| inner.strip_prefix(':'))?;
    let (name, id) = emoji.split_once(':')?;
    snowflake(id)?;
    (!name.is_empty() && name.chars().all(|c| c.is_alphanumeric() || c == '_')).then(|| format!(":{name}:"))
}

/// `@everyone` for the server's own id, `@name` for a role the model knows
/// by name, else `@role:<id>` (an unnamed, new or deleted role, or an
/// unknown server).
fn role_label(id: Id, live: &LiveState, guild: Option<Id>) -> String {
    let g = guild.and_then(|g| live.guild(g));
    if g.as_ref().is_some_and(|g| g.id == id) {
        return "@everyone".into();
    }
    match g.and_then(|g| g.roles.into_iter().find(|r| r.id == id)).filter(|r| !r.name.is_empty()) {
        Some(r) => format!("@{}", r.name),
        None => format!("@role:{id}"),
    }
}

/// Turns `@name` into `<@id>` only when it can't reach the wrong person.
/// Candidates: in a DM or group DM, its recipients; in a server, the people
/// seen there (`LiveState::guild_people`) plus the author of the message
/// replied to. A username matches exactly (any case); a display name only
/// when one candidate has it. Anything else, `@everyone`, `@here`, code, and
/// an `@` that doesn't start a word (`a@b`, `x.com/@name`, a URL) stay as
/// typed.
pub fn to_discord_mentions(text: &str, live: &LiveState, channel: Id, reply_to: Option<Id>) -> String {
    let Some(c) = live.channel(channel) else { return text.to_string() };
    let people: Vec<UserRef> = match c.guild_id {
        None => c.recipients,
        Some(g) => {
            let mut people = live.guild_people(g);
            let replied = reply_to.and_then(|r| live.recent(channel).into_iter().find(|m| m.id == r));
            people.extend(replied.map(|m| m.author));
            people
        }
    };
    map_outside_code(text, |s| replace_names(s, &people))
}

fn is_name_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_' || c == '.'
}

/// An `@` starts a mention only at the start, after whitespace or after
/// opening or separating punctuation, and never inside a URL.
fn may_start_mention(prev: Option<char>, written: &str) -> bool {
    let after_ok = prev.is_none_or(|p| p.is_whitespace() || "([{,;:!?\"'*~".contains(p));
    let word = written.rsplit(char::is_whitespace).next().unwrap_or_default();
    after_ok && !word.contains("://")
}

fn replace_names(s: &str, people: &[UserRef]) -> String {
    let mut out = String::with_capacity(s.len());
    let mut prev: Option<char> = None;
    let mut rest = s;
    while let Some(c) = rest.chars().next() {
        if c == '@' && may_start_mention(prev, &out) {
            let after = &rest[1..];
            let len = after.find(|ch: char| !is_name_char(ch)).unwrap_or(after.len());
            // A trailing period ends the sentence, not the name.
            let name = after[..len].trim_end_matches('.');
            // `@scope/pkg` and `@a@b` are not people.
            let path_like = after[name.len()..].starts_with(['/', '@']);
            if let Some(id) = unique_match(name, people).filter(|_| !path_like) {
                out.push_str(&format!("<@{id}>"));
                rest = &after[name.len()..];
                prev = Some('>');
                continue;
            }
        }
        out.push(c);
        prev = Some(c);
        rest = &rest[c.len_utf8()..];
    }
    out
}

/// One person by exact username, else one by display name.
fn unique_match(name: &str, people: &[UserRef]) -> Option<Id> {
    if name.is_empty() || name.eq_ignore_ascii_case("everyone") || name.eq_ignore_ascii_case("here") {
        return None;
    }
    let only = |pred: &dyn Fn(&UserRef) -> bool| {
        let mut ids: Vec<Id> = people.iter().filter(|u| pred(u)).map(|u| u.id).collect();
        ids.sort();
        ids.dedup();
        ids
    };
    let by_username = only(&|u| u.username.eq_ignore_ascii_case(name));
    let ids = if by_username.is_empty() {
        only(&|u| u.display_name.as_deref().is_some_and(|d| d.eq_ignore_ascii_case(name)))
    } else {
        by_username
    };
    match ids.as_slice() {
        [one] => Some(*one),
        _ => None,
    }
}

#[cfg(test)]
#[path = "render_tests.rs"]
mod tests;
