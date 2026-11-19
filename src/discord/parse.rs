//! Discord JSON (REST and gateway share the shapes) to the slim model.
//! Object shapes: https://docs.discord.food/resources/message,
//! https://docs.discord.food/resources/channel, https://docs.discord.food/resources/user

use serde_json::Value;

use super::types::{Channel, ChannelKind, Id, Message, Overwrite, UserRef};

fn id(v: &Value) -> Option<Id> {
    match v {
        Value::String(s) => s.parse().ok(),
        Value::Number(n) => n.as_u64().map(Id),
        _ => None,
    }
}

fn field_id(v: &Value, key: &str) -> Option<Id> {
    v.get(key).and_then(id)
}

fn str_field(v: &Value, key: &str) -> Option<String> {
    v.get(key).and_then(Value::as_str).map(str::to_string)
}

fn ids(v: &Value, key: &str) -> Vec<Id> {
    v.get(key).and_then(Value::as_array).map(|a| a.iter().filter_map(id).collect()).unwrap_or_default()
}

fn array<'a>(v: &'a Value, key: &str) -> &'a [Value] {
    v.get(key).and_then(Value::as_array).map(Vec::as_slice).unwrap_or(&[])
}

/// ISO 8601 / RFC 3339, as Discord writes timestamps, to Unix ms.
fn timestamp_ms(s: &str) -> Option<i64> {
    chrono::DateTime::parse_from_rfc3339(s).ok().map(|t| t.timestamp_millis())
}

/// A user object (`global_name` is the display name). `None` without id or username.
pub(crate) fn parse_user(v: &Value) -> Option<UserRef> {
    Some(UserRef {
        id: field_id(v, "id")?,
        username: str_field(v, "username")?,
        display_name: str_field(v, "global_name").filter(|s| !s.is_empty()),
        bot: v.get("bot").and_then(Value::as_bool).unwrap_or(false),
    })
}

const IMAGE_EXTENSIONS: &[&str] = &["png", "jpg", "jpeg", "gif", "webp", "avif", "bmp"];

/// "image: a.png" for images (by content type, else extension), "file: b.pdf" otherwise.
fn attachment_placeholder(a: &Value) -> Option<String> {
    let name = str_field(a, "filename")?;
    let by_type = a.get("content_type").and_then(Value::as_str).map(|t| t.starts_with("image/"));
    let by_ext =
        || name.rsplit_once('.').is_some_and(|(_, ext)| IMAGE_EXTENSIONS.contains(&ext.to_ascii_lowercase().as_str()));
    let kind = if by_type.unwrap_or_else(by_ext) { "image" } else { "file" };
    Some(format!("{kind}: {name}"))
}

/// (":name:" for a custom emoji, the emoji itself for unicode, count).
fn reaction(r: &Value) -> Option<(String, u32)> {
    let emoji = r.get("emoji")?;
    let name = str_field(emoji, "name")?;
    let label = if field_id(emoji, "id").is_some() { format!(":{name}:") } else { name };
    let count = r.get("count").and_then(Value::as_u64).unwrap_or(1) as u32;
    Some((label, count))
}

/// Titles of a message's embeds; untitled ones (link previews, images) are dropped.
fn embed_titles(v: &Value) -> Vec<String> {
    array(v, "embeds").iter().filter_map(|e| str_field(e, "title")).filter(|t| !t.is_empty()).collect()
}

/// Placeholders for what a message (or a forward's snapshot) carries besides
/// text: attachments, stickers, a poll, and "embed" when an untitled embed is
/// all there is (with text it's a preview of that text's link).
fn placeholders(v: &Value) -> Vec<String> {
    let mut out: Vec<String> = array(v, "attachments").iter().filter_map(attachment_placeholder).collect();
    out.extend(array(v, "sticker_items").iter().filter_map(|s| str_field(s, "name")).map(|n| format!("sticker: {n}")));
    if let Some(q) = v.get("poll").and_then(|p| p.get("question")).and_then(|q| str_field(q, "text")) {
        out.push(format!("poll: {q}"));
    }
    let no_text = str_field(v, "content").is_none_or(|c| c.is_empty());
    if no_text && out.is_empty() && embed_titles(v).is_empty() && !array(v, "embeds").is_empty() {
        out.push("embed".to_string());
    }
    out
}

/// A message object as REST and the gateway send it. `None` when the id,
/// channel or author is missing. A missing or odd timestamp falls back to the
/// id's; embeds without a title are dropped (link previews, images).
pub(crate) fn parse_message(v: &Value) -> Option<Message> {
    let msg_id = field_id(v, "id")?;
    let reference = v.get("message_reference");
    // A forward carries a copy of the original (one snapshot, no author).
    let snapshot = array(v, "message_snapshots").first().and_then(|s| s.get("message"));
    Some(Message {
        id: msg_id,
        channel_id: field_id(v, "channel_id")?,
        guild_id: field_id(v, "guild_id"),
        author: parse_user(v.get("author")?)?,
        content: str_field(v, "content").unwrap_or_default(),
        timestamp_ms: v
            .get("timestamp")
            .and_then(Value::as_str)
            .and_then(timestamp_ms)
            .unwrap_or_else(|| msg_id.timestamp_ms()),
        edited: v.get("edited_timestamp").is_some_and(|t| !t.is_null()),
        reply_to: reference.and_then(|r| field_id(r, "message_id")),
        reply_to_author: v.get("referenced_message").and_then(|m| m.get("author")).and_then(|a| field_id(a, "id")),
        mentions: array(v, "mentions").iter().filter_map(|u| field_id(u, "id")).collect(),
        mention_roles: ids(v, "mention_roles"),
        mention_everyone: v.get("mention_everyone").and_then(Value::as_bool).unwrap_or(false),
        forwarded: snapshot.and_then(|m| str_field(m, "content")).filter(|t| !t.is_empty()),
        attachments: placeholders(v).into_iter().chain(snapshot.map(placeholders).unwrap_or_default()).collect(),
        embeds: embed_titles(v).into_iter().chain(snapshot.map(embed_titles).unwrap_or_default()).collect(),
        reactions: array(v, "reactions").iter().filter_map(reaction).collect(),
        kind: v.get("type").and_then(Value::as_u64).unwrap_or(0) as u8,
    })
}

/// Permission bits come as decimal strings (numbers accepted too).
fn bits(v: &Value, key: &str) -> u64 {
    match v.get(key) {
        Some(Value::String(s)) => s.parse().unwrap_or(0),
        Some(Value::Number(n)) => n.as_u64().unwrap_or(0),
        _ => 0,
    }
}

/// Overwrite `type` is 0 (role) or 1 (member); some payloads say "role"/"member".
fn overwrite(o: &Value) -> Option<Overwrite> {
    let is_member = match o.get("type") {
        Some(Value::Number(n)) => n.as_u64() == Some(1),
        Some(Value::String(s)) => s == "member" || s == "1",
        _ => false,
    };
    Some(Overwrite { id: field_id(o, "id")?, is_member, allow: bits(o, "allow"), deny: bits(o, "deny") })
}

/// A channel or thread object. `None` without id or type. `recipients` are
/// full user objects (REST, gateway CHANNEL_CREATE); READY's `recipient_ids`
/// need the gateway's own user table.
pub(crate) fn parse_channel(v: &Value) -> Option<Channel> {
    Some(Channel {
        id: field_id(v, "id")?,
        guild_id: field_id(v, "guild_id"),
        parent_id: field_id(v, "parent_id"),
        name: str_field(v, "name"),
        kind: ChannelKind::from_u8(u8::try_from(v.get("type")?.as_u64()?).ok()?),
        recipients: array(v, "recipients").iter().filter_map(parse_user).collect(),
        last_message_id: field_id(v, "last_message_id"),
        overwrites: array(v, "permission_overwrites").iter().filter_map(overwrite).collect(),
        position: v.get("position").and_then(Value::as_i64).unwrap_or(0) as i32,
        // A thread object carries the user's thread member when they're in
        // it (THREAD_CREATE on being added, REST thread objects).
        joined: v.get("member").is_some_and(Value::is_object),
    })
}

#[cfg(test)]
#[path = "parse_tests.rs"]
mod tests;
