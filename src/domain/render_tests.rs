use super::*;
use crate::discord::gateway::GatewayEvent;
use crate::discord::types::{Id, Message, Role, UserRef};
use crate::domain::live::{BufferPolicy, LiveState};
use crate::test_support::*;

const ME: u64 = 7;
const NOW: i64 = 1_800_000_000_000;

fn named(n: u64, display: &str) -> UserRef {
    UserRef { display_name: Some(display.into()), ..user(n) }
}

/// G001 (with role 5) holding C101, a DM D201 with U002, and known users
/// U003 ("Three"), U004 and U005 (both "Sam").
fn world() -> LiveState {
    let l = LiveState::new(BufferPolicy { per_channel: 50, max_age_ms: 86_400_000 });
    let mut g = guild(1);
    g.roles.push(Role { id: Id(5), permissions: 0 });
    let mut snap = ready(ME, vec![g], vec![text_channel(101, 1), dm(201, 2)]);
    snap.users = vec![named(3, "Three"), named(4, "Sam"), named(5, "Sam")];
    l.apply(GatewayEvent::Ready(Box::new(snap)), NOW);
    l
}

fn in_guild(id: Id, content: &str) -> Message {
    Message { guild_id: Some(Id(1)), ..msg(id, 101, 3, content) }
}

#[test]
fn render_handles_unknown_mentions_and_custom_emoji() {
    let l = world();
    let content = "hi <@2> <@!3> <@999> in <#101> and <#998>, <@&1> <@&5> <@&777> <:party:12345> <a:wave:678> \
                   bad <@abc> <# <:x:> <@&> tail <";
    let mut m = in_guild(id_at(NOW), content);
    m.reply_to = Some(Id(50));
    m.attachments = vec!["image: a.png".into(), "file: b.pdf".into()];
    m.embeds = vec!["Title".into()];
    m.reactions = vec![(":party:".into(), 3)];
    let r = render_message(&m, &l);
    assert_eq!(
        r.text,
        "hi @U002 @Three @unknown-user in #C101 and #unknown-channel, @everyone @role:5 @unknown-role :party: :wave: \
         bad <@abc> <# <:x:> <@&> tail <\n[image: a.png]\n[file: b.pdf]\n[embed: Title]"
    );
    // The referenced message was deleted: no author known.
    assert_eq!(r.reply_to, Some(ReplyRef { message_id: "50".into(), author: "unknown-user".into() }));
    assert_eq!(r.reactions, vec![(":party:".to_string(), 3)]);
}

#[test]
fn render_link_for_dm_uses_me() {
    let l = world();
    let id = id_at(NOW);
    let mut m = msg(id, 201, 2, "hello");
    m.reply_to = Some(Id(40));
    m.reply_to_author = Some(Id(ME));
    let r = render_message(&m, &l);
    assert_eq!(r.link, format!("https://discord.com/channels/@me/201/{id}"));
    assert_eq!(r.id, id.to_string());
    assert_eq!(r.from, "U002");
    assert_eq!(r.from_id, "2");
    assert_eq!(r.at, "2027-01-15T08:00:00Z");
    assert_eq!(r.reply_to, Some(ReplyRef { message_id: "40".into(), author: "U007".into() }));

    // A guild message whose payload lacks guild_id still links to its guild.
    let g = render_message(&msg(id, 101, 3, "x"), &l);
    assert_eq!(g.link, format!("https://discord.com/channels/1/101/{id}"));
    assert_eq!(g.from, "U003", "the author object's own name, not the cached one");
}

#[test]
fn render_long_codeblock_untouched() {
    let l = world();
    let mut code = String::from("```rust\nfn main() { let v: Vec<u8> = vec![]; } // <@2> <#101> <:x:1>\n");
    while code.len() < 1_900 {
        code.push_str("    let x = 1; // padding line inside the block\n");
    }
    code.push_str("```");
    let content = format!("<@2> see `<#101>` and:\n{code}\nok");
    assert!(content.len() >= 1_950 && content.len() <= 2_000, "{}", content.len());
    let r = render_message(&in_guild(id_at(NOW), &content), &l);
    assert_eq!(r.text, format!("@U002 see `<#101>` and:\n{code}\nok"));
}

#[test]
fn render_unclosed_fence_is_plain_text() {
    let l = world();
    let r = render_message(&in_guild(id_at(NOW), "``` <@2>"), &l);
    assert_eq!(r.text, "``` @U002");
}

#[test]
fn outgoing_mentions_converted() {
    let l = world();
    let g = Some(Id(1));
    assert_eq!(
        to_discord_mentions("hey @U003 and @three, cc @nobody. mail a@U003.com, @everyone, `@U003`, end @u003.", &l, g),
        "hey <@3> and <@3>, cc @nobody. mail a@U003.com, @everyone, `@U003`, end <@3>."
    );
    // Two users called Sam: left as typed.
    assert_eq!(to_discord_mentions("@Sam hi", &l, g), "@Sam hi");
    // In DMs only the DM's people count.
    assert_eq!(to_discord_mentions("@U002 @U003", &l, None), "<@2> @U003");
}
