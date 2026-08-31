//! How user and agent-to-agent turns are wrapped for the model.
//!
//! Matches Grok Bot's JSONL shape: `<timestamp>` + `<user_query>`, hidden
//! `[agent]` arrivals, @mention roster cards (real id), and `/skill` bodies.

use super::profile::Profile;
use crate::skills::Skill;

/// Header snapshot for an arriving message. It is data, never raw markup.
pub fn with_cwd(wrapped: &str, cwd: &str) -> String {
    let cwd = cwd
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;");
    let header = format!("<cwd>{cwd}</cwd>\n");
    if let Some(end) = wrapped.find("</timestamp>\n") {
        let split = end + "</timestamp>\n".len();
        format!("{}{header}{}", &wrapped[..split], &wrapped[split..])
    } else {
        format!("{header}{wrapped}")
    }
}

pub fn timestamp_now() -> String {
    chrono::Local::now()
        .format("%A, %b %-d, %Y, %-I:%M %p (%Z)")
        .to_string()
}

pub fn looks_wrapped(text: &str) -> bool {
    text.contains("<user_query>") || text.contains("[SAND_HIDDEN_PROMPT]")
}

/// Wrap a human turn. `raw` is what they typed.
pub fn wrap_user_turn(raw: &str, roster: &[Profile], skills: &[Skill]) -> String {
    wrap_user_turn_at(raw, roster, skills, &[], &[], &timestamp_now())
}

pub fn wrap_user_turn_at(
    raw: &str,
    roster: &[Profile],
    skills: &[Skill],
    updated: &[Skill],
    removed: &[String],
    timestamp: &str,
) -> String {
    if looks_wrapped(raw) {
        return raw.to_string();
    }
    let mentions = mentions_in(raw, roster);
    let invoked = skills_in(raw, skills);
    let mut inner = String::new();
    if !mentions.is_empty() {
        inner.push_str("[SAND_HIDDEN_PROMPT][mentions]\n");
        inner.push_str(
            "Agents mentioned in this message — you can reach any of them with SendAgentMessage using their id:\n",
        );
        for profile in mentions {
            inner.push_str(&mention_line(profile));
            inner.push('\n');
        }
        inner.push_str("[/SAND_HIDDEN_PROMPT]\n\n");
    }
    if !invoked.is_empty() {
        inner.push_str("[SAND_HIDDEN_PROMPT][skills]\n");
        inner.push_str(
            "The user invoked the following skill(s). Follow their instructions for this turn.\n",
        );
        for skill in &invoked {
            inner.push_str(&format_skill_card(skill));
        }
        inner.push_str("[/SAND_HIDDEN_PROMPT]\n\n");
    }
    let invoked_names: Vec<&str> = invoked.iter().map(|s| s.name.as_str()).collect();
    let fresh: Vec<&Skill> = updated
        .iter()
        .filter(|s| !invoked_names.contains(&s.name.as_str()))
        .collect();
    if !fresh.is_empty() || !removed.is_empty() {
        inner.push_str("[SAND_HIDDEN_PROMPT][skills-updated]\n");
        inner.push_str(
            "Skills created or edited since your last turn. Follow the latest body when they apply.\n",
        );
        if !removed.is_empty() {
            inner.push_str(&format!("Removed: {}\n", removed.join(", ")));
        }
        for skill in fresh {
            inner.push_str(&format_skill_card(skill));
        }
        inner.push_str("[/SAND_HIDDEN_PROMPT]\n\n");
    }
    inner.push_str(raw.trim());
    format!("<timestamp>{timestamp}</timestamp>\n<user_query>\n{inner}\n</user_query>")
}

fn format_skill_card(skill: &Skill) -> String {
    format!(
        "\n## /{}\n{}\n\n{}\n",
        skill.name,
        skill.description,
        skill.body.trim()
    )
}

fn mention_line(profile: &Profile) -> String {
    let mut line = format!("- {} (id: {})", profile.name, profile.id);
    let title = profile.title.trim();
    let desc = profile.description.trim();
    if !title.is_empty() {
        line.push_str(" — ");
        line.push_str(title);
        if !desc.is_empty() {
            line.push_str(". ");
            line.push_str(desc);
        }
    } else if !desc.is_empty() {
        line.push_str(" — ");
        line.push_str(desc);
    }
    line
}

/// Wrap an async arrival from another bot. Stored as a user entry; the model
/// is told it is not the human typing.
pub fn wrap_agent_arrival(from_name: &str, from_id: &str, text: &str) -> String {
    wrap_agent_arrival_at(from_name, from_id, text, &timestamp_now())
}

pub fn wrap_agent_arrival_at(
    from_name: &str,
    from_id: &str,
    text: &str,
    timestamp: &str,
) -> String {
    format!(
        "<timestamp>{timestamp}</timestamp>\n\
         <user_query>\n\
         [SAND_HIDDEN_PROMPT][agent] A message just arrived from another of your user's agents: {from_name} (id: {from_id}).\n\
         This is another assistant reaching out — not the user typing here. It arrived asynchronously, and your user can already see it in this chat.\n\
         \n\
         {from_name}: {text}\n\
         \n\
         If it needs a reply or an action, handle it: reply to {from_name} with SendAgentMessage (their id: {from_id}), which reaches them on a later turn — not a live back-and-forth — and use SendUserMessage to tell your user only when you have a real result to share. If it is just an FYI with nothing for you to do, it is fine to stay silent — no need to reply just to acknowledge it.\n\
         </user_query>"
    )
}

/// Longest-name-first @mentions against the roster (name or id).
pub fn mentions_in<'a>(raw: &str, roster: &'a [Profile]) -> Vec<&'a Profile> {
    let mut ranked: Vec<&Profile> = roster.iter().collect();
    ranked.sort_by(|a, b| {
        b.name
            .len()
            .cmp(&a.name.len())
            .then(b.id.len().cmp(&a.id.len()))
    });
    let mut hit = Vec::new();
    for profile in ranked {
        let needle_name = format!("@{}", profile.name);
        let needle_id = format!("@{}", profile.id);
        let found = raw.contains(&needle_name) || raw.contains(&needle_id);
        if found && !hit.iter().any(|p: &&Profile| p.id == profile.id) {
            hit.push(profile);
        }
    }
    hit
}

/// `/skillname` tokens (start of string or after whitespace). Longest name first
/// so `/lua-plugins` does not also fire `/lua`.
pub fn skills_in<'a>(raw: &str, skills: &'a [Skill]) -> Vec<&'a Skill> {
    let mut ranked: Vec<&Skill> = skills.iter().collect();
    ranked.sort_by_key(|b| std::cmp::Reverse(b.name.len()));
    let mut hit = Vec::new();
    let lower = raw.to_ascii_lowercase();
    for skill in ranked {
        if slash_invokes(&lower, &skill.name) && !hit.iter().any(|s: &&Skill| s.name == skill.name)
        {
            hit.push(skill);
        }
    }
    hit
}

fn slash_invokes(raw_lower: &str, name: &str) -> bool {
    let needle = format!("/{name}");
    let bytes = raw_lower.as_bytes();
    let n = needle.as_bytes();
    let mut i = 0;
    while i + n.len() <= bytes.len() {
        if bytes[i..].starts_with(n) {
            let ok_before = i == 0 || bytes[i - 1].is_ascii_whitespace();
            let after = i + n.len();
            let ok_after = after == bytes.len() || !is_skill_name_char(bytes[after]);
            if ok_before && ok_after {
                return true;
            }
        }
        i += 1;
    }
    false
}

fn is_skill_name_char(b: u8) -> bool {
    b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_'
}

/// House kernel. Goes before the bot's SOUL.md.
pub fn house_kernel(bot: &Profile, teammates: &[Profile]) -> String {
    let others: Vec<&Profile> = teammates.iter().filter(|p| p.id != bot.id).collect();
    let roster = if others.is_empty() {
        "This house has no other agents yet. If a job needs a distinct owner, offer to CreateAgent one."
            .to_string()
    } else {
        let mut lines = vec!["Teammates you can reach with SendAgentMessage:".into()];
        for p in others.iter().take(40) {
            let desc = p.description.trim();
            if desc.is_empty() {
                lines.push(format!("- {} (id: {})", p.name, p.id));
            } else {
                lines.push(format!("- {} (id: {}) — {desc}", p.name, p.id));
            }
        }
        lines.join("\n")
    };
    format!(
        "\
# House

You are a bot in a local house of teammates sharing one computer (`/workspace`).
Your id (folder name) is `{id}`. Your display name is {name}. The id does not
change if the name does.

## Talking to the user

The user is in this chat. They see:
1. Your **opening assistant text** this turn (one bubble), streamed live.
2. After any tool starts, **Working…** — later assistant prose is internal, not a bubble.
3. Each **SendUserMessage** as its own extra bubble, posted immediately.

SendUserMessage always succeeds. The function result is `ok` — that is the
harness closing the call, not a user reply, not a receipt to wait on, not a
failure. The user already has the bubble. Never retry SendUserMessage. Never
call it twice with the same text this turn. Do not wait for a result before
continuing work. Do not SendUserMessage just to say you replied.

## Talking to other bots

SendAgentMessage is asynchronous, like texting. You get an ack, not a reply
this turn. Their reply arrives later as a user message wrapped with
`[SAND_HIDDEN_PROMPT][agent]` and their name/id.

- Reply with SendAgentMessage using **their id**.
- The user already sees the arrival in this chat.
- Use SendUserMessage for the user only when there is a real result to share.
- If the arrival is FYI and nothing is needed from you, stay silent. Do not
  acknowledge just to acknowledge.
- Do not fan out to several teammates unless the user asked.

When the user writes `@Name`, a hidden roster card with that bot's **id** is
attached to their turn. Reach that bot with SendAgentMessage using the id,
not the display name.

When the user writes `/skill`, that skill's full instructions are attached
to the turn. Follow them.

## Tools

- `update_state` — target `profile` for name/title/description/avatar/group/model/projects; target `memory` for exact write/forget facts with profile/log/note tiers and agent/user/project scopes.
- `cd` — change this conversation's guest working directory and load all ancestor AGENTS.md files.
- `CreateAgent` — a sibling under `/workspace/agents/`. Returns its id. Then message it.
- `UpdateAgent` — merge-patch another bot's profile. Cannot delete.
- `SendAgentMessage` — `id` + `text` (optional `priority`). Async.
- `SendUserMessage` — `text`. User-visible bubble.
- `AskUserForSecret` — `title`, `description`, `reason`, `env`. The user fills an inline form; the VM never holds the value.
You cannot delete a bot. The user does that in the sidebar.

## Files

Your home: `/workspace/agents/{id}/` (`SOUL.md`, `profile.json`, `memory/`, `skills/`).
Your SOUL.md is your identity and standing remit. Your initial working directory is your home's `workspace/`.
Private memory is yours only. Shared user memory requires explicit scope `user`; project memory requires explicit profile.projects membership.
Only `/workspace/VM.md` is shared machine knowledge. Do not adopt other agents' projects from the roster.
Use `cd` to change work location, not identity. Do not write under any agent's `sessions/`.

{roster}
",
        id = bot.id,
        name = bot.name,
        roster = roster,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cwd_is_an_escaped_message_header_not_part_of_the_user_query() {
        let wrapped = with_cwd(
            &wrap_user_turn_at("hello", &[], &[], &[], &[], "ts"),
            "/repo/a&<b>",
        );
        assert!(wrapped.starts_with(
            "<timestamp>ts</timestamp>\n<cwd>/repo/a&amp;&lt;b&gt;</cwd>\n<user_query>"
        ));
    }

    fn roster() -> Vec<Profile> {
        vec![
            Profile {
                id: "chief-of-staff".into(),
                name: "Chief of Staff".into(),
                title: String::new(),
                description: "Own the roster.".into(),
                avatar: None,
                group: String::new(),
                created_at: None,
                model: None,
                projects: Vec::new(),
            },
            Profile {
                id: "qmd-hero".into(),
                name: "QMD Hero".into(),
                title: String::new(),
                description: "Maintain tobi/qmd.".into(),
                avatar: None,
                group: String::new(),
                created_at: None,
                model: None,
                projects: Vec::new(),
            },
        ]
    }

    fn skill(name: &str, description: &str, body: &str) -> Skill {
        Skill {
            name: name.into(),
            description: description.into(),
            path: std::path::PathBuf::from("skills"),
            body: body.into(),
        }
    }

    #[test]
    fn a_user_turn_gets_timestamp_and_query_tags() {
        let wrapped = wrap_user_turn_at(
            "Sup buddy",
            &[],
            &[],
            &[],
            &[],
            "Wednesday, Aug 12, 2026, 6:54 PM (EDT)",
        );
        assert!(wrapped.contains("<timestamp>Wednesday, Aug 12, 2026, 6:54 PM (EDT)</timestamp>"));
        assert!(wrapped.contains("<user_query>\nSup buddy\n</user_query>"));
        assert!(!wrapped.contains("[SAND_HIDDEN_PROMPT]"));
    }

    #[test]
    fn an_at_mention_attaches_a_roster_card_with_real_id() {
        let wrapped = wrap_user_turn_at("ask @QMD Hero to triage", &roster(), &[], &[], &[], "ts");
        assert!(wrapped.contains("[SAND_HIDDEN_PROMPT][mentions]"));
        assert!(wrapped.contains("Agents mentioned in this message"));
        assert!(wrapped.contains("QMD Hero (id: qmd-hero) — Maintain tobi/qmd."));
        assert!(wrapped.contains("SendAgentMessage using their id"));
        assert!(wrapped.contains("ask @QMD Hero to triage"));
        assert!(!wrapped.contains("Chief of Staff (id:"));
    }

    #[test]
    fn a_slash_skill_attaches_the_skill_body() {
        let skills = vec![
            skill("lua", "tiny", "TINY BODY"),
            skill("lua-plugins", "Write Lua plugins", "Read ctx.sh docs."),
        ];
        let wrapped =
            wrap_user_turn_at("please /lua-plugins on this", &[], &skills, &[], &[], "ts");
        assert!(wrapped.contains("[SAND_HIDDEN_PROMPT][skills]"));
        assert!(wrapped.contains("## /lua-plugins"));
        assert!(wrapped.contains("Write Lua plugins"));
        assert!(wrapped.contains("Read ctx.sh docs."));
        assert!(wrapped.contains("please /lua-plugins on this"));
        assert!(
            !wrapped.contains("TINY BODY"),
            "/lua-plugins must not also invoke /lua"
        );
    }

    #[test]
    fn an_agent_arrival_is_hidden_and_tells_how_to_reply() {
        let wrapped = wrap_agent_arrival_at("QMD Hero", "qmd-hero", "Triage done.", "ts");
        assert!(wrapped.contains("[SAND_HIDDEN_PROMPT][agent]"));
        assert!(wrapped.contains("QMD Hero (id: qmd-hero)"));
        assert!(wrapped.contains("not the user typing here"));
        assert!(wrapped.contains("QMD Hero: Triage done."));
        assert!(wrapped.contains("SendAgentMessage (their id: qmd-hero)"));
        assert!(wrapped.contains("stay silent"));
        assert!(wrapped.contains("SendUserMessage"));
    }

    #[test]
    fn already_wrapped_text_is_left_alone() {
        let once = wrap_user_turn_at("hi", &[], &[], &[], &[], "ts");
        let twice = wrap_user_turn(&once, &[], &[]);
        assert_eq!(once, twice);
    }

    #[test]
    fn kernel_names_this_bot_and_send_paths() {
        let roster = roster();
        let text = house_kernel(&roster[0], &roster);
        assert!(text.starts_with("# House\n"));
        assert!(text.contains("id (folder name) is `chief-of-staff`"));
        assert!(text.contains("SendUserMessage"));
        assert!(text.contains("SendAgentMessage"));
        assert!(text.contains("QMD Hero (id: qmd-hero)"));
        assert!(text.contains("[SAND_HIDDEN_PROMPT][agent]"));
        assert!(text.contains("When the user writes `@Name`"));
        assert!(text.contains("When the user writes `/skill`"));
        assert!(text.contains("AskUserForSecret"));
        assert!(text.contains("SendUserMessage always succeeds"));
        assert!(!text.contains("Ack ≠ delivery"));
        assert!(!text.contains("failed to deliver"));
    }

    #[test]
    fn created_or_changed_skills_are_attached_as_hidden_cards() {
        let skills = vec![skill("review", "review changes", "Read the diff.")];
        let wrapped = wrap_user_turn_at("hi", &[], &[], &skills, &["gone".into()], "ts");
        assert!(wrapped.contains("[SAND_HIDDEN_PROMPT][skills-updated]"));
        assert!(wrapped.contains("## /review"));
        assert!(wrapped.contains("Read the diff."));
        assert!(wrapped.contains("Removed: gone"));
        assert!(wrapped.contains("<user_query>\n"));
        assert!(wrapped.contains("hi"));
    }
}
