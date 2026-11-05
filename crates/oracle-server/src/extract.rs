//! Memories extracted from a session (the competitor map's session-extraction row, 4 Nov 2026: mem0, OpenViking, memU,
//! Hindsight retain and Cognee all extract on write; we had only hand-written memories). `POST /extract` takes the text of
//! a session's new turns and answers the durable things in it: decisions, lessons, preferences and commitments, each one
//! sentence that stands alone. It stores nothing: the seat that sent the text signs what it keeps (`ox capture`), so a
//! memory is never written in a seat's name by the server.
//!
//! The model endpoint is configured (`ORACLE_EXTRACT_URL`, an Ollama-compatible `/api/chat`;
//! `ORACLE_EXTRACT_MODEL`, default qwen3:8b). Locality is not enforced. Text reaches Oracle before filtering;
//! a line the secret scan matches is dropped before the model reads it.
//! One extraction runs at a time; a CPU model is slow and the store must stay quick.

use serde_json::{json, Value};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

/// The newest characters of one call that are read: about 2,000 tokens, two minutes of a CPU model (measured 4 Nov 2026 on
/// core, qwen3:8b on six threads: 17 tokens a second read, 3.4 written). `ox capture` sends a long session in pieces of this.
pub const MAX_INPUT: usize = 8_000;
pub const MAX_MEMORIES: usize = 8;
pub const BUDGET: Duration = Duration::from_secs(600);
pub const KINDS: [&str; 4] = ["decision", "lesson", "preference", "commitment"];
const MIN_TEXT: usize = 24;
const MAX_TEXT: usize = 400;

const PROMPT: &str = "You read the transcript of a working session between a person and an AI agent, and write down what is worth \
remembering in a month. Keep only durable knowledge:\n\
- decision: something chosen, with what was chosen;\n\
- lesson: something learned the hard way, a cause found, a mistake not to repeat;\n\
- preference: how the person wants things done;\n\
- commitment: something promised, with by whom and by when when it is said.\n\
Never keep status, progress, tool output, file listings, greetings, or anything true only for this session. \
Never copy a key, token, password or credential. Name people by the names the transcript uses. Each memory is one sentence that stands alone without the transcript: \
name the thing, the project and the person, never 'it' or 'this'. Write nothing that the transcript does not say. \
Return at most 8, the most durable first; return none when nothing qualifies.";

pub struct Extractor {
    url: String,
    model: String,
    /// `ORACLE_EXTRACT_THREADS`: a model server held to a CPU quota stalls when it starts a thread per core (4 Nov 2026: six
    /// threads under a six-core quota read 4.7 times faster than the default).
    threads: Option<u32>,
    ctx: u32,
}

static CONFIGURED: OnceLock<Option<Extractor>> = OnceLock::new();
static ONE_AT_A_TIME: Mutex<()> = Mutex::new(());

/// The extractor this server was started with, if any.
pub fn configured() -> Option<&'static Extractor> {
    CONFIGURED.get_or_init(Extractor::from_env).as_ref()
}

impl Extractor {
    pub fn from_env() -> Option<Extractor> {
        let url = std::env::var("ORACLE_EXTRACT_URL").ok().map(|u| u.trim().trim_end_matches('/').to_string()).filter(|u| !u.is_empty())?;
        let model = std::env::var("ORACLE_EXTRACT_MODEL").ok().filter(|m| !m.trim().is_empty()).unwrap_or_else(|| "qwen3:8b".into());
        let threads = std::env::var("ORACLE_EXTRACT_THREADS").ok().and_then(|t| t.trim().parse().ok()).filter(|t| *t > 0);
        let ctx = std::env::var("ORACLE_EXTRACT_CTX").ok().and_then(|t| t.trim().parse().ok()).filter(|t| *t >= 2048).unwrap_or(4096);
        Some(Extractor { url, model, threads, ctx })
    }

    pub fn model(&self) -> &str {
        &self.model
    }

    /// The candidates in `text`, checked: known kinds, a sentence's length, no secret, no repeats.
    pub fn extract(&self, text: &str) -> Result<Value, String> {
        let started = Instant::now();
        let (clean, dropped) = scrub(text);
        let input = newest(&clean, MAX_INPUT);
        if input.trim().is_empty() {
            return Ok(answer(&self.model, vec![], dropped, 0, started));
        }
        let schema = json!({"type": "object", "properties": {"memories": {"type": "array", "items": {"type": "object",
            "properties": {"kind": {"type": "string", "enum": KINDS}, "text": {"type": "string"}, "why": {"type": "string"}},
            "required": ["kind", "text"]}}}, "required": ["memories"]});
        let content = self.chat(PROMPT, input, schema)?;
        Ok(answer(&self.model, parse(&content), dropped, input.chars().count(), started))
    }

    /// One call to the model, one at a time, its answer held to `schema`.
    fn chat(&self, system: &str, user: &str, schema: Value) -> Result<String, String> {
        let mut options = json!({"temperature": 0, "num_ctx": self.ctx});
        if let Some(t) = self.threads {
            options["num_thread"] = json!(t);
        }
        let body = json!({"model": self.model, "stream": false, "think": false, "format": schema, "options": options,
            "messages": [{"role": "system", "content": system}, {"role": "user", "content": user}]});
        let _turn = ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
        let resp = ureq::post(&format!("{}/api/chat", self.url)).timeout(BUDGET).send_json(body).map_err(|e| match e {
            ureq::Error::Status(code, r) => format!("extractor {code}: {}", r.into_string().unwrap_or_default().chars().take(200).collect::<String>()),
            other => format!("extractor unreachable: {other}"),
        })?;
        let v: Value = resp.into_json().map_err(|e| format!("extractor answered no JSON: {e}"))?;
        Ok(v["message"]["content"].as_str().unwrap_or("").to_string())
    }

    /// Reflection (the competitor map's reflection row: Hindsight's observations, Letta's sleep-time): a mind's own recent
    /// memories and its current observations in, observations out. Each observation is about one named subject, cites only
    /// the memories it was given, and names the observation it refines, so a refined one supersedes it and the old one stays.
    pub fn reflect(&self, memories: &[Value], previous: &[Value]) -> Result<Value, String> {
        let started = Instant::now();
        let line = |v: &Value| match v["author"].as_str() {
            Some(a) => format!("[{}] ({a}) {}", v["id"].as_str().unwrap_or(""), one_line(v["text"].as_str().unwrap_or(""), 400)),
            None => format!("[{}] {}", v["id"].as_str().unwrap_or(""), one_line(v["text"].as_str().unwrap_or(""), 400)),
        };
        let ids: std::collections::HashSet<&str> = memories.iter().filter_map(|m| m["id"].as_str()).collect();
        let prev: std::collections::HashMap<&str, &str> =
            previous.iter().filter_map(|p| Some((p["id"].as_str()?, p["about"].as_str().unwrap_or("")))).collect();
        let mut input = String::from("CURRENT OBSERVATIONS\n");
        for p in previous.iter().take(MAX_PREVIOUS) {
            input.push_str(&format!("{} (about {})\n", line(p), p["about"].as_str().unwrap_or("")));
        }
        input.push_str("\nNEW MEMORIES\n");
        for m in memories.iter().take(MAX_REFLECT) {
            input.push_str(&line(m));
            input.push('\n');
        }
        let (clean, dropped) = scrub(&input);
        let schema = json!({"type": "object", "properties": {"observations": {"type": "array", "items": {"type": "object",
            "properties": {"about": {"type": "string"}, "text": {"type": "string"}, "cites": {"type": "array", "items": {"type": "string"}},
                "refines": {"type": "string"}}, "required": ["about", "text", "cites"]}}}, "required": ["observations"]});
        let content = self.chat(REFLECT_PROMPT, newest(&clean, MAX_INPUT), schema)?;
        Ok(json!({"schema": "reflect/1", "model": self.model, "observations": parse_reflection(&content, &ids, &prev),
            "dropped_lines": dropped, "ms": started.elapsed().as_millis() as u64}))
    }
}

pub const MAX_REFLECT: usize = 24;
pub const MAX_PREVIOUS: usize = 12;
pub const MAX_OBSERVATIONS: usize = 5;

const REFLECT_PROMPT: &str = "You keep the observations of an Oracle, the shared memory of a team of people and agents: what \
the team now holds true about the subjects it works on. You are given the CURRENT OBSERVATIONS and the NEW MEMORIES, each with \
an id in brackets; a memory names the seat that wrote it in parentheses. Write observations for the subjects \
that two or more memories speak about, or that a new memory changes. An observation is two or three sentences: what is true \
now, and what changed, with dates when the memories give them. When a new memory contradicts what is held, say which is \
newer and that the older was replaced. 'about' is the subject in two to four lowercase words. 'cites' lists the ids of the \
new memories it rests on, only ids given to you. When it updates a current observation, 'refines' is that observation's id. \
A subject is a thing: a project, a system, a release, a place, a named person; never 'person', 'user', 'agent' or 'me'. \
One observation is about one subject: never join two unrelated facts. Write nothing that the memories do not say. Return at \
most 5; none when no subject has two memories or a change.";

/// Subjects that name nobody and nothing: a small model reaches for them to join unrelated facts.
const VAGUE: [&str; 8] = ["person", "user", "agent", "me", "the-person", "the-user", "the-agent", "people"];

/// One line of at most `max` characters.
fn one_line(s: &str, max: usize) -> String {
    let s = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if s.chars().count() > max { format!("{}…", s.chars().take(max).collect::<String>()) } else { s }
}

/// A subject as a tag: lowercase words joined by '-', at most 48 characters.
pub fn slug(about: &str) -> String {
    let words: Vec<String> = about.split(|c: char| !c.is_alphanumeric()).filter(|w| !w.is_empty()).map(str::to_lowercase).collect();
    let mut s = words.join("-");
    while s.chars().count() > 48 {
        match s.rfind('-') {
            Some(i) => s.truncate(i),
            None => s = s.chars().take(48).collect(),
        }
    }
    s.chars().filter(|c| c.is_ascii_alphanumeric() || *c == '-').collect()
}

/// The model's observations, checked: a subject, a length, only citations that were given (at least one), a refines that
/// names a current observation or nothing. Anything else is dropped, never repaired.
pub fn parse_reflection(content: &str, ids: &std::collections::HashSet<&str>, previous: &std::collections::HashMap<&str, &str>) -> Vec<Value> {
    let v: Value = serde_json::from_str(content.trim()).unwrap_or(Value::Null);
    let mut out = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for o in v["observations"].as_array().map(Vec::as_slice).unwrap_or_default() {
        let (Some(about), Some(text)) = (o["about"].as_str(), o["text"].as_str()) else { continue };
        let about = slug(about);
        let text = one_line(text, 800);
        let cites: Vec<&str> = o["cites"].as_array().map(Vec::as_slice).unwrap_or_default().iter()
            .filter_map(Value::as_str).filter(|c| ids.contains(c)).collect();
        let len = text.chars().count();
        if about.is_empty() || cites.is_empty() || !(40..=800).contains(&len) || crate::secrets::find(&text).is_some() || !seen.insert(about.clone()) {
            continue;
        }
        // An observation about the subject of a current one refines it, whether or not the model said so (a small model
        // often leaves `refines` empty): one current observation per subject.
        let refines = o["refines"].as_str().filter(|r| previous.contains_key(r))
            .or_else(|| previous.iter().find(|(_, a)| slug(a) == about).map(|(id, _)| *id));
        // The rule the prompt states, held here: two memories behind it, or a change to what is held.
        if VAGUE.contains(&about.as_str()) || (cites.len() < 2 && refines.is_none()) {
            continue;
        }
        out.push(json!({"about": about, "text": text, "cites": cites, "refines": refines}));
        if out.len() == MAX_OBSERVATIONS {
            break;
        }
    }
    out
}

fn answer(model: &str, memories: Vec<Value>, dropped: usize, chars: usize, started: Instant) -> Value {
    json!({"schema": "extract/1", "model": model, "memories": memories, "dropped_lines": dropped, "input_chars": chars,
        "ms": started.elapsed().as_millis() as u64})
}

/// Every line the secret scan matches is left out, and counted.
pub fn scrub(text: &str) -> (String, usize) {
    let mut dropped = 0;
    let kept: Vec<&str> = text
        .lines()
        .filter(|l| {
            let hit = crate::secrets::find(l).is_some();
            dropped += hit as usize;
            !hit
        })
        .collect();
    (kept.join("\n"), dropped)
}

/// The last `max` characters, starting on a line.
pub fn newest(text: &str, max: usize) -> &str {
    let n = text.chars().count();
    if n <= max {
        return text;
    }
    let cut = text.char_indices().nth(n - max).map(|(i, _)| i).unwrap_or(0);
    let s = &text[cut..];
    s.find('\n').map(|i| &s[i + 1..]).unwrap_or(s)
}

/// The model's answer, checked. Anything malformed is dropped, never repaired into a claim it did not make.
pub fn parse(content: &str) -> Vec<Value> {
    let v: Value = serde_json::from_str(content.trim()).unwrap_or(Value::Null);
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    for m in v["memories"].as_array().map(Vec::as_slice).unwrap_or_default() {
        let (Some(kind), Some(text)) = (m["kind"].as_str(), m["text"].as_str()) else { continue };
        let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
        let len = text.chars().count();
        if !KINDS.contains(&kind) || !(MIN_TEXT..=MAX_TEXT).contains(&len) || crate::secrets::find(&text).is_some() {
            continue;
        }
        if !seen.insert(text.to_lowercase()) {
            continue;
        }
        let why = m["why"].as_str().map(|w| w.chars().take(200).collect::<String>()).filter(|w| !w.trim().is_empty());
        out.push(json!({"kind": kind, "text": text, "why": why}));
        if out.len() == MAX_MEMORIES {
            break;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_good_answer_is_kept_and_a_bad_one_is_dropped() {
        let raw = r#"{"memories":[
            {"kind":"decision","text":"Blaze chose Friday 7 Nov for the oracle-core v0.1.0 release.","why":"said at the end"},
            {"kind":"status","text":"The build is running right now on core, please wait."},
            {"kind":"lesson","text":"too short"},
            {"kind":"decision","text":"Blaze  chose Friday 7 Nov for the oracle-core v0.1.0 release."},
            {"kind":"preference","text":"Blaze wants every reply to him to go through the Bridge, never the terminal."}]}"#;
        let got = parse(raw);
        assert_eq!(got.len(), 2, "{got:?}");
        assert_eq!(got[0]["kind"], "decision");
        assert_eq!(got[1]["kind"], "preference");
        assert!(parse("not json").is_empty(), "a malformed answer yields nothing");
        assert!(parse(r#"{"memories":"x"}"#).is_empty());
    }

    #[test]
    fn an_observation_cites_only_what_it_was_given_and_refines_only_a_current_one() {
        let ids: std::collections::HashSet<&str> = ["OBS-a", "OBS-b"].into();
        let prev: std::collections::HashMap<&str, &str> = [("OBS-old", "release date")].into();
        let raw = r#"{"observations":[
            {"about":"Release Date!","text":"oracle-core v0.1.0 ships Friday 7 Nov from dev; the earlier Thursday date was replaced on 3 Nov.","cites":["OBS-a","OBS-made-up"],"refines":"OBS-old"},
            {"about":"embedder","text":"Only one embedder runs on core: two copies exhausted memory and broke the canary.","cites":["OBS-made-up"]},
            {"about":"ports","text":"too short","cites":["OBS-b"]},
            {"about":"release date","text":"A second observation about the same subject is dropped as a repeat of the first.","cites":["OBS-b"]},
            {"about":"design server","text":"The design server stays on port 3001 for good, so links never change.","cites":["OBS-a","OBS-b"],"refines":"OBS-not-current"},
            {"about":"embedder memory","text":"One memory alone and no change to what is held is not yet an observation.","cites":["OBS-b"]},
            {"about":"Person","text":"The person prefers plain English and learned that two embedders broke the canary.","cites":["OBS-a","OBS-b"]}]}"#;
        let got = parse_reflection(raw, &ids, &prev);
        assert_eq!(got.len(), 2, "{got:?}");
        assert_eq!((got[0]["about"].as_str(), got[0]["refines"].as_str()), (Some("release-date"), Some("OBS-old")));
        assert_eq!(got[0]["cites"], json!(["OBS-a"]), "a made-up citation is removed");
        assert_eq!(got[1]["refines"], Value::Null, "refines only a current observation");
        let same = r#"{"observations":[{"about":"release date","text":"The release moved again, to Monday 10 November, after a signing bug was found in review.","cites":["OBS-b"]}]}"#;
        let got = parse_reflection(same, &ids, &prev);
        assert_eq!(got[0]["refines"], "OBS-old", "the same subject refines the current observation even when the model leaves it out");
        assert!(parse_reflection("{}", &ids, &prev).is_empty());
        assert_eq!(slug("The  AXXIS design-server / ports"), "the-axxis-design-server-ports");
        assert!(slug(&"word ".repeat(40)).chars().count() <= 48);
    }

    #[test]
    fn the_newest_text_is_read_from_a_line_start() {
        let t = "first line\nsecond line\nthird line";
        assert_eq!(newest(t, 100), t);
        assert_eq!(newest(t, 14), "third line");
        let wide = "é".repeat(30) + "\nend";
        assert_eq!(newest(&wide, 10), "end", "a multibyte cut lands on a char boundary");
    }

    #[test]
    fn a_line_that_holds_a_secret_never_reaches_the_model() {
        let key = ["-----BEGIN ", "PRIVATE KEY-----"].concat();
        let (clean, dropped) = scrub(&format!("we decided to ship\n{key}\nthen we slept"));
        assert_eq!(dropped, 1);
        assert_eq!(clean, "we decided to ship\nthen we slept");
        let (same, none) = scrub("nothing secret here");
        assert_eq!((same.as_str(), none), ("nothing secret here", 0), "negative control");
    }
}
