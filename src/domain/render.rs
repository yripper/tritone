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

/// Roles carry no name in tritone's model: the guild's own id is @everyone,
/// any other role of the guild shows its id.
fn role_label(id: Id, live: &LiveState, guild: Option<Id>) -> String {
    let Some(g) = guild.and_then(|g| live.guild(g)) else { return "@unknown-role".into() };
    if id == g.id {
        "@everyone".into()
    } else if g.roles.iter().any(|r| r.id == id) {
        format!("@role:{id}")
    } else {
        "@unknown-role".into()
    }
}

/// Turns `@name` into `<@id>` when exactly one person matches the username or
/// display name (case-insensitive): any known user in a server, the DM
/// people when `guild` is `None`. Ambiguous or unknown names, `@everyone`,
/// `@here`, e-mail addresses and code stay as written.
pub fn to_discord_mentions(text: &str, live: &LiveState, guild: Option<Id>) -> String {
    let people: Vec<UserRef> = match guild {
        Some(_) => live.users(),
        None => live.channels().into_iter().filter(|c| c.kind.is_private()).flat_map(|c| c.recipients).collect(),
    };
    map_outside_code(text, |s| replace_names(s, &people))
}

fn is_name_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_' || c == '.'
}

fn replace_names(s: &str, people: &[UserRef]) -> String {
    let mut out = String::with_capacity(s.len());
    let mut prev: Option<char> = None;
    let mut rest = s;
    while let Some(c) = rest.chars().next() {
        let starts_mention = c == '@' && !prev.is_some_and(|p| p.is_alphanumeric() || p == '<' || p == '_');
        if starts_mention {
            let after = &rest[1..];
            let len = after.find(|ch: char| !is_name_char(ch)).unwrap_or(after.len());
            // A trailing period ends the sentence, not the name.
            let name = after[..len].trim_end_matches('.');
            if let Some(id) = unique_match(name, people) {
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

fn unique_match(name: &str, people: &[UserRef]) -> Option<Id> {
    if name.is_empty() || name.eq_ignore_ascii_case("everyone") || name.eq_ignore_ascii_case("here") {
        return None;
    }
    let mut ids: Vec<Id> = people
        .iter()
        .filter(|u| {
            u.username.eq_ignore_ascii_case(name)
                || u.display_name.as_deref().is_some_and(|d| d.eq_ignore_ascii_case(name))
        })
        .map(|u| u.id)
        .collect();
    ids.sort();
    ids.dedup();
    match ids.as_slice() {
        [one] => Some(*one),
        _ => None,
    }
}

#[cfg(test)]
#[path = "render_tests.rs"]
mod tests;
