// Style profile (SPEC 6.6 Q-26): a single global profile of how the user talks,
// derived only from the user's own messages and shared by every chat and session
// (project, notes, harness). It lives in one row of `style_profile` in the shared
// SQLite DB, so it survives restarts and is the same for all agents. The update
// is deterministic — counters with exponential decay, no extra LLM call — and the
// agent's short Russian style instruction is derived from those counters and
// injected into every system prompt through the [Memory] block (memory::identity_block).
//
// Signals per user message: address (ты vs вы pronouns), word count, emoji count,
// profanity count. Each stored counter is decayed by DECAY on every update, so
// recent turns weigh more and old style fades; every counter is hard-capped at
// MAX_COUNTER so growth stays bounded. A fully neutral profile yields an empty
// instruction (nothing is added to the prompt).
use crate::error::Result;
use rusqlite::{params, Connection, OptionalExtension};

// Exponential recency weight: each counter is multiplied by this on every update.
const DECAY: f64 = 0.85;
// Hard upper bound per counter (safety clamp; decay already bounds normal use).
const MAX_COUNTER: f64 = 1000.0;
// Minimum decayed address evidence before ты/вы is judged at all.
const MIN_ADDRESS: f64 = 1.0;
// Share of address signals on one side to call the address informal/formal.
const INFORMAL_RATIO: f64 = 0.6;
// A decayed profanity/emoji counter at or above this counts as present.
const MIN_PRESENCE: f64 = 0.5;
// Average words per message that read as short / long.
const SHORT_WORDS: f64 = 6.0;
const LONG_WORDS: f64 = 40.0;

// Second-person pronouns the user directs at the agent: informal (ты) vs formal (вы).
const INFORMAL_PRONOUNS: &[&str] = &[
    "ты", "тебя", "тебе", "тобой", "тобою", "твой", "твоя", "твое", "твоей", "твоего", "твоему",
    "твоим", "твою", "твои", "твоих", "твоими",
];
const FORMAL_PRONOUNS: &[&str] = &[
    "вы", "вас", "вам", "вами", "ваш", "ваша", "ваше", "вашей", "вашего", "вашему", "вашим",
    "вашу", "ваши", "ваших", "вашими",
];
// Obscene/strong-insult stems matched inside a word. Chosen to avoid false hits
// on ordinary words (e.g. "небо" is not caught: the еб-family is matched only as a
// word prefix or via the longer stems below).
const PROFANITY_SUBSTRINGS: &[&str] = &[
    "пизд", "бля", "хуй", "хуя", "хуё", "хуи", "заеб", "заёб", "наеб", "поеб", "выеб", "выёб",
    "уеб", "ъеб", "еба", "ебо", "еби", "ебл", "ебуч", "ебат", "сука", "сучк", "гондон", "мудак",
];

#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct StyleProfile {
    informal: f64,
    formal: f64,
    emoji: f64,
    profanity: f64,
    words: f64,
    messages: f64,
}

// Lowercased alphanumeric tokens of a message (Cyrillic letters are alphanumeric).
fn tokenize(text: &str) -> Vec<String> {
    text.split(|c: char| !c.is_alphanumeric())
        .filter(|t| !t.is_empty())
        .map(|t| t.to_lowercase())
        .collect()
}

fn count_profanity(text: &str) -> usize {
    tokenize(text)
        .iter()
        .filter(|t| {
            t.starts_with("еб")
                || t.starts_with("ёб")
                || PROFANITY_SUBSTRINGS.iter().any(|s| t.contains(s))
        })
        .count()
}

fn count_emoji(text: &str) -> usize {
    text.chars()
        .filter(|c| {
            matches!(
                c,
                '\u{1F300}'..='\u{1FAFF}'
                    | '\u{2600}'..='\u{27BF}'
                    | '\u{1F1E6}'..='\u{1F1FF}'
                    | '\u{2B00}'..='\u{2BFF}'
                    | '\u{FE0F}'
            )
        })
        .count()
}

// (informal, formal) address-signal counts for one message.
fn count_address(text: &str) -> (usize, usize) {
    let tokens = tokenize(text);
    let informal = tokens
        .iter()
        .filter(|t| INFORMAL_PRONOUNS.contains(&t.as_str()))
        .count();
    let formal = tokens
        .iter()
        .filter(|t| FORMAL_PRONOUNS.contains(&t.as_str()))
        .count();
    (informal, formal)
}

pub(crate) fn load(conn: &Connection) -> Result<StyleProfile> {
    Ok(conn
        .query_row(
            "SELECT informal, formal, emoji, profanity, words, messages
             FROM style_profile WHERE id = 1",
            [],
            |r| {
                Ok(StyleProfile {
                    informal: r.get(0)?,
                    formal: r.get(1)?,
                    emoji: r.get(2)?,
                    profanity: r.get(3)?,
                    words: r.get(4)?,
                    messages: r.get(5)?,
                })
            },
        )
        .optional()?
        .unwrap_or_default())
}

pub(crate) fn save(conn: &Connection, p: &StyleProfile) -> Result<()> {
    conn.execute(
        "INSERT INTO style_profile (id, informal, formal, emoji, profanity, words, messages, updated_at)
         VALUES (1, ?1, ?2, ?3, ?4, ?5, ?6, datetime('now','localtime'))
         ON CONFLICT(id) DO UPDATE SET
             informal = excluded.informal,
             formal = excluded.formal,
             emoji = excluded.emoji,
             profanity = excluded.profanity,
             words = excluded.words,
             messages = excluded.messages,
             updated_at = excluded.updated_at",
        params![p.informal, p.formal, p.emoji, p.profanity, p.words, p.messages],
    )?;
    Ok(())
}

// Fold one user message into the profile: decay every counter, add this message's
// signal counts, clamp to MAX_COUNTER, persist. Returns the updated profile.
pub fn observe_user_message(conn: &Connection, text: &str) -> Result<StyleProfile> {
    let mut p = load(conn)?;
    p.informal = (p.informal * DECAY).min(MAX_COUNTER);
    p.formal = (p.formal * DECAY).min(MAX_COUNTER);
    p.emoji = (p.emoji * DECAY).min(MAX_COUNTER);
    p.profanity = (p.profanity * DECAY).min(MAX_COUNTER);
    p.words = (p.words * DECAY).min(MAX_COUNTER);
    p.messages = (p.messages * DECAY).min(MAX_COUNTER);

    let (informal, formal) = count_address(text);
    p.informal = (p.informal + informal as f64).min(MAX_COUNTER);
    p.formal = (p.formal + formal as f64).min(MAX_COUNTER);
    p.emoji = (p.emoji + count_emoji(text) as f64).min(MAX_COUNTER);
    p.profanity = (p.profanity + count_profanity(text) as f64).min(MAX_COUNTER);
    p.words = (p.words + tokenize(text).len() as f64).min(MAX_COUNTER);
    p.messages = (p.messages + 1.0).min(MAX_COUNTER);

    save(conn, &p)?;
    Ok(p)
}

// Zero the profile (Settings → «Сбросить стиль общения»).
pub fn reset(conn: &Connection) -> Result<()> {
    save(conn, &StyleProfile::default())
}

// The short Russian style instruction derived from the profile, or an empty string
// when the profile is neutral (nothing to mirror). Subordinate to task correctness:
// mirror address/tone/formality, allow mild profanity only if the user swears and
// only where it carries meaning, never as filler, never more often or coarser than
// the user, never insulting the user or third parties; keep technical/document
// output professional and accurate; add no profanity/slang for a neutral user.
pub fn instruction_block(conn: &Connection) -> Result<String> {
    let p = load(conn)?;
    let address_total = p.informal + p.formal;
    let informal = address_total >= MIN_ADDRESS && p.informal / address_total >= INFORMAL_RATIO;
    let formal = address_total >= MIN_ADDRESS && p.formal / address_total >= 1.0 - INFORMAL_RATIO;
    let profanity = p.profanity >= MIN_PRESENCE;
    let emoji = p.emoji >= MIN_PRESENCE;
    let avg_words = if p.messages > 0.0 {
        p.words / p.messages
    } else {
        0.0
    };
    let short = p.messages >= 1.0 && avg_words <= SHORT_WORDS;
    let long = p.messages >= 1.0 && avg_words >= LONG_WORDS;

    if !(informal || formal || profanity || emoji || short || long) {
        return Ok(String::new());
    }

    let mut lines = vec!["Стиль общения (вторичен по отношению к точности задачи):"];
    if informal {
        lines.push("пользователь обращается на «ты» — держи неформальный тон и обращение на «ты»");
    }
    if formal {
        lines.push(
            "пользователь обращается на «вы» — отвечай формально на «вы» (это важнее общей договорённости о «ты»)",
        );
    }
    if short {
        lines.push("пользователь пишет коротко — отвечай кратко");
    }
    if long {
        lines.push("пользователь пишет развёрнуто — можно отвечать подробнее");
    }
    if emoji {
        lines.push("пользователь ставит эмодзи — можно изредка повторять, без перебора");
    }
    if profanity {
        lines.push(
            "пользователь иногда матерится — допускай мягкий мат только там, где он несёт смысл, не как слово-паразит, не чаще и не грубее, чем пользователь; не оскорбляй пользователя и третьих лиц",
        );
    }
    lines.push(
        "технические и документные ответы держи профессиональными и точными; при нейтральном стиле пользователя мат и сленг не добавляй",
    );
    Ok(lines.join("\n"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::schema;
    use crate::onboarding::temp_test_dir;
    use rusqlite::Connection;
    use std::fs;

    fn conn() -> Connection {
        crate::db::ensure_vec_extension();
        let conn = Connection::open_in_memory().unwrap();
        schema::run_migrations(&conn).unwrap();
        conn
    }

    #[test]
    fn observe_informal_sets_informal_clause() {
        let conn = conn();
        observe_user_message(&conn, "Как ты? Сделай это покороче, ты же понял").unwrap();
        let block = instruction_block(&conn).unwrap();
        assert!(block.contains("обращается на «ты»"), "{block}");
        assert!(!block.contains("обращается на «вы»"), "{block}");
        assert!(!block.contains("матерится"), "{block}");
    }

    #[test]
    fn observe_formal_sets_formal_clause() {
        let conn = conn();
        observe_user_message(&conn, "Подскажите, как у Вас дела, жду Вашего ответа").unwrap();
        let block = instruction_block(&conn).unwrap();
        assert!(block.contains("обращается на «вы»"), "{block}");
        assert!(!block.contains("обращается на «ты»"), "{block}");
    }

    #[test]
    fn observe_profanity_sets_capped_profanity_clause() {
        let conn = conn();
        observe_user_message(&conn, "Бля, ну ты даёшь, просто пиздец").unwrap();
        let block = instruction_block(&conn).unwrap();
        assert!(block.contains("иногда матерится"), "{block}");
        // The cap: never more often or coarser than the user, never as filler.
        assert!(block.contains("не чаще и не грубее, чем пользователь"), "{block}");
        assert!(block.contains("не как слово-паразит"), "{block}");
        assert!(block.contains("не оскорбляй пользователя и третьих лиц"), "{block}");
    }

    #[test]
    fn neutral_message_yields_no_instruction() {
        let conn = conn();
        observe_user_message(
            &conn,
            "Подготовь пожалуйста отчёт по продажам за прошлый квартал и пришли его на почту завтра утром",
        )
        .unwrap();
        assert_eq!(instruction_block(&conn).unwrap(), "");
    }

    #[test]
    fn profanity_never_mirrored_for_neutral_user() {
        let conn = conn();
        observe_user_message(
            &conn,
            "Создай пожалуйста новую задачу на завтра и прикрепи к ней документы проекта",
        )
        .unwrap();
        let block = instruction_block(&conn).unwrap();
        assert!(!block.contains("матерится"), "{block}");
        assert!(!block.contains("ругат"), "{block}");
    }

    #[test]
    fn technical_quality_clause_always_present_when_instructing() {
        let conn = conn();
        observe_user_message(&conn, "Привет! Как ты, братан?").unwrap();
        let block = instruction_block(&conn).unwrap();
        assert!(block.contains("технические и документные ответы держи профессиональными и точными"), "{block}");
    }

    #[test]
    fn profanity_fades_after_neutral_turns() {
        let conn = conn();
        observe_user_message(&conn, "Бля, ну ты даёшь, пиздец").unwrap();
        assert!(instruction_block(&conn).unwrap().contains("матерится"));
        for _ in 0..12 {
            observe_user_message(
                &conn,
                "Подготовь пожалуйста отчёт по продажам за прошлый квартал и пришли его на почту завтра утром",
            )
            .unwrap();
        }
        assert!(!instruction_block(&conn).unwrap().contains("матерится"));
    }

    #[test]
    fn counters_are_capped_and_bounded() {
        let conn = conn();
        let huge = "пиздец ".repeat(500);
        for _ in 0..50 {
            observe_user_message(&conn, &huge).unwrap();
        }
        let p = load(&conn).unwrap();
        assert!(p.profanity <= MAX_COUNTER && p.profanity > 0.0);
        assert!(p.messages <= MAX_COUNTER);
        assert!(p.words <= MAX_COUNTER);
    }

    #[test]
    fn reset_clears_the_profile() {
        let conn = conn();
        observe_user_message(&conn, "Бля, ну ты даёшь, пиздец").unwrap();
        reset(&conn).unwrap();
        assert_eq!(load(&conn).unwrap(), StyleProfile::default());
        assert_eq!(instruction_block(&conn).unwrap(), "");
    }

    #[test]
    fn profile_persists_across_connections() {
        let dir = temp_test_dir("style-persist");
        let file = dir.join("project-s.db");
        {
            let conn = crate::db::open(&file).unwrap();
            observe_user_message(&conn, "Бля, ну ты даёшь, пиздец").unwrap();
        }
        let conn = crate::db::open(&file).unwrap();
        let p = load(&conn).unwrap();
        assert!(p.profanity > 0.0 && p.messages > 0.0);
        assert!(instruction_block(&conn).unwrap().contains("мат"));
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn instruction_injected_into_all_three_agent_prompts() {
        let conn = conn();
        observe_user_message(&conn, "Бля, ну ты даёшь, пиздец").unwrap();
        let identity = crate::memory::identity_block(&conn).unwrap();
        assert!(identity.contains("иногда матерится"), "{identity}");

        let project_tools = crate::agent::project_tools::project_catalog().summary_prompt_block();
        let harness_tools = crate::agent::harness_tools::harness_catalog().summary_prompt_block();
        let notes_tools = crate::agent::notes_tools::notes_catalog().summary_prompt_block();
        let project = crate::projects::ProjectDto {
            id: 1,
            name: "example.ru".into(),
            path: "/wd/Projects/example.ru".into(),
            created_at: "2026-01-01".into(),
            opened_at: None,
            position: 1.0,
            file_count: 0,
            task_count: 0,
            overdue_count: 0,
            has_active_tasks: false,
            task_columns: crate::projects::DEFAULT_TASK_COLUMNS.map(str::to_owned),
            files_view: crate::projects::DEFAULT_FILES_VIEW.into(),
            archived: false,
            archived_at: None,
        };
        for (label, prompt) in [
            (
                "project",
                crate::agent::prompts::project_system_prompt(&project, &[], &project_tools),
            ),
            (
                "harness",
                crate::agent::prompts::harness_system_prompt(&[], &harness_tools),
            ),
            ("notes", crate::agent::prompts::notes_system_prompt(&[], &notes_tools)),
        ] {
            // run_agent_inner appends the [Memory] block to every agent's prompt.
            let full = format!("{prompt}\n\n{identity}");
            assert!(full.contains("иногда матерится"), "{label} prompt missing style injection");
            assert!(full.contains("не чаще и не грубее, чем пользователь"), "{label}");
        }
    }
}
