//! The words that matter in Oracle and the Time Machine, ranked (docs/TERMS.md; Blaze, 2026-10-08: "let's not remove
//! them, let's hide them, deprioritize them"). It was ops/terms.py; `oracle terms` writes the same file, held to it on
//! the live stores before the Python was retired.
//!
//! A word's weight is how often it is said times how few documents say it (count x ln(N / documents with it)): a word in
//! almost every message sinks, a word that concentrates rises. Filler words (`STOP`) are kept, marked common, at a
//! fiftieth of their weight. Nothing is removed. Both stores are read read-only.

use regex::Regex;
use rusqlite::{Connection, OpenFlags};
use serde_json::{json, Map, Value};
use std::collections::{HashMap, HashSet};
use std::path::Path;

/// Filler keeps its place in the list at a fiftieth of its weight.
pub const COMMON: f64 = 0.02;
pub const HOW: &str = "weight = count x ln(documents / documents with the word); common words kept at a fiftieth, marked common";

/// Python's `[^\W\d_][^\W_]{2,29}` in Unicode terms: a letter or non-decimal number, then 2 to 29 letters or numbers.
pub const WORD: &str = r"[\p{L}\p{Nl}\p{No}][\p{L}\p{N}]{2,29}";

/// The filler words, exactly ops/terms.py's list (a few, like "can't", never match `WORD`; kept so the list is the same).
pub const STOP: &str = "the and for are was were been this that with from but not you your our their his her its they them what which
who whom how why when where there here can could will would should shall may might must have has had just like also into
about over than too very more most some any all each every one two yes out off only own same such these those because while
after before again further once right okay let get got make made want need know think see say said going thing things way
really don doesn didn isn aren wasn weren won wouldn shouldn couldn can't it's i'm that's there's what's let's yeah yes
then now even still much many well back first last next new old good great big small sure actually basically literally
something anything everything nothing someone anyone everyone always never often already also though although however
another other others both either neither each few lot lots kind sort type part way ways time times day days use used using
just mean means meant look looks looking come comes came take takes took give gives gave keep keeps kept put puts tell
told ask asked try tried work works worked call called see seen show shows shown find found give done does did doing
being am is be an a of to in on at by as or if so do no up me my we us he she it i per yet between itself through
without different within across around against during toward towards upon whether rather quite almost";

/// One store's counts: times each word was said, documents that say it, documents read.
#[derive(Debug, Default)]
pub struct Counts {
    pub tf: HashMap<String, u64>,
    pub df: HashMap<String, u64>,
    pub n: u64,
}

pub struct Ranker {
    word: Regex,
    stop: HashSet<&'static str>,
}

impl Default for Ranker {
    fn default() -> Self {
        Ranker { word: Regex::new(WORD).expect("WORD compiles"), stop: STOP.split_whitespace().collect() }
    }
}

/// Python's `round(x, places)`: the nearest decimal, ties to even on the exact binary value (Rust's `{:.N}` does the same).
pub fn round_to(x: f64, places: usize) -> f64 {
    format!("{x:.places$}").parse().unwrap_or(x)
}

impl Ranker {
    /// The words of one document, lower-cased, in order.
    pub fn words(&self, text: &str) -> Vec<String> {
        self.word.find_iter(text).map(|m| m.as_str().to_lowercase()).collect()
    }

    /// Each room's own counts, from (room, text) documents: where a word was said, and how often there.
    pub fn count_by_room<I: IntoIterator<Item = (String, String)>>(&self, docs: I) -> HashMap<String, Counts> {
        let mut rooms: HashMap<String, Counts> = HashMap::new();
        for (room, text) in docs {
            let c = rooms.entry(room).or_default();
            c.n += 1;
            // The reader's text: an import's CLAUDE-EXPORT header names the import, not the turn (Blaze, 31 Oct 2026: #illuminati's
            // index read "room blaze claude turn export speaker assistant").
            let words = self.words(&crate::store::clean_text(&text));
            let mut seen: HashSet<&str> = HashSet::new();
            for w in &words {
                *c.tf.entry(w.clone()).or_default() += 1;
                if seen.insert(w) {
                    *c.df.entry(w.clone()).or_default() += 1;
                }
            }
        }
        rooms
    }

    pub fn count<I: IntoIterator<Item = String>>(&self, docs: I) -> Counts {
        let mut c = Counts::default();
        for text in docs {
            c.n += 1;
            let words = self.words(&crate::store::clean_text(&text));
            let mut seen: HashSet<&str> = HashSet::new();
            for w in &words {
                *c.tf.entry(w.clone()).or_default() += 1;
                if seen.insert(w) {
                    *c.df.entry(w.clone()).or_default() += 1;
                }
            }
        }
        c
    }

    /// The `top` best words over every store, best first, and the documents read.
    pub fn rank(&self, stores: &[(&str, &Counts)], top: usize) -> (u64, Vec<Value>) {
        let (mut tf, mut df, mut n): (HashMap<&str, u64>, HashMap<&str, u64>, u64) = Default::default();
        for (_, c) in stores {
            for (w, k) in &c.tf {
                *tf.entry(w).or_default() += k;
            }
            for (w, k) in &c.df {
                *df.entry(w).or_default() += k;
            }
            n += c.n;
        }
        let mut rows: Vec<(f64, &str, Value)> = Vec::new();
        for (&w, &c) in &tf {
            let d = df[w];
            let mut letters = w.chars();
            let first = letters.next();
            // One letter repeated is a redaction mark (xxxx), not a word.
            if d < 3 || letters.all(|ch| Some(ch) == first) {
                continue;
            }
            let common = self.stop.contains(w);
            let weight = round_to(c as f64 * (n as f64 / d as f64).ln() * if common { COMMON } else { 1.0 }, 1);
            let mut row = Map::new();
            row.insert("term".into(), json!(w));
            row.insert("weight".into(), json!(weight));
            row.insert("count".into(), json!(c));
            row.insert("documents".into(), json!(d));
            row.insert("share".into(), json!(round_to(d as f64 / n as f64, 4)));
            row.insert("common".into(), json!(common));
            for (name, s) in stores {
                row.insert(format!("{name}_count"), json!(s.tf.get(w).copied().unwrap_or(0)));
            }
            rows.push((weight, w, Value::Object(row)));
        }
        // Python sorts on (-weight, term) with the rounded weight; terms are unique, so the order is total.
        rows.sort_by(|a, b| b.0.total_cmp(&a.0).then_with(|| a.1.cmp(b.1)));
        let out = rows
            .into_iter()
            .take(top)
            .enumerate()
            .map(|(i, (_, _, mut r))| {
                r["rank"] = json!(i + 1);
                r
            })
            .collect();
        (n, out)
    }
}

/// How many documents say each word, for every word said in at least `floor`: the rarity of any word, not only the top.
pub fn document_counts(stores: &[(&str, &Counts)], floor: u64) -> Map<String, Value> {
    let mut df: HashMap<&str, u64> = HashMap::new();
    for (_, c) in stores {
        for (w, k) in &c.df {
            *df.entry(w).or_default() += k;
        }
    }
    let mut keys: Vec<(&str, u64)> = df.into_iter().filter(|(_, k)| *k >= floor).collect();
    keys.sort();
    keys.into_iter().map(|(w, k)| (w.to_owned(), json!(k))).collect()
}

/// Every row's text column, read-only (`immutable` for a file nothing writes while it is read, as the chat store).
/// Two columns: (room, text). A null in either is the empty string.
pub fn read_pairs(db: &Path, sql: &str, immutable: bool) -> anyhow::Result<Vec<(String, String)>> {
    let uri = format!("file:{}?mode=ro{}", db.display(), if immutable { "&immutable=1" } else { "" });
    let c = Connection::open_with_flags(uri, OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI | OpenFlags::SQLITE_OPEN_NO_MUTEX)?;
    let mut st = c.prepare(sql)?;
    let rows = st.query_map([], |r| Ok((r.get::<_, Option<String>>(0)?, r.get::<_, Option<String>>(1)?)))?;
    let mut out = Vec::new();
    for r in rows {
        let (a, b) = r?;
        out.push((a.unwrap_or_default(), b.unwrap_or_default()));
    }
    Ok(out)
}

/// For each ranked word, the rooms it was said in and how often there (the chat store counts as one room, `chats`).
pub fn rooms_of(rows: &mut [Value], rooms: &HashMap<String, Counts>) {
    for row in rows.iter_mut() {
        let w = row["term"].as_str().unwrap_or("").to_owned();
        let mut m: Vec<(&String, u64)> = rooms.iter().filter_map(|(room, c)| c.tf.get(&w).map(|k| (room, *k))).collect();
        m.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(b.0)));
        row["rooms"] = Value::Object(m.into_iter().map(|(room, k)| (room.clone(), json!(k))).collect());
    }
}

pub fn read_column(db: &Path, sql: &str, immutable: bool) -> anyhow::Result<Vec<String>> {
    let uri = format!("file:{}?mode=ro{}", db.display(), if immutable { "&immutable=1" } else { "" });
    let c = Connection::open_with_flags(uri, OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI | OpenFlags::SQLITE_OPEN_NO_MUTEX)?;
    let mut st = c.prepare(sql)?;
    let rows = st.query_map([], |r| r.get::<_, Option<String>>(0))?;
    let mut out = Vec::new();
    for r in rows {
        out.push(r?.unwrap_or_default());
    }
    Ok(out)
}

/// The whole file `oracle terms` writes: both stores counted, ranked, and the document counts.
pub fn build(oracle_db: &Path, chats_db: &Path, top: usize, generated_at: &str, started: std::time::Instant) -> anyhow::Result<Value> {
    let r = Ranker::default();
    let by_room = r.count_by_room(read_pairs(oracle_db, "SELECT channel, content FROM observations", false)?);
    let mut oracle = Counts::default();
    for c in by_room.values() {
        oracle.n += c.n;
        for (w, k) in &c.tf {
            *oracle.tf.entry(w.clone()).or_default() += k;
        }
        for (w, k) in &c.df {
            *oracle.df.entry(w.clone()).or_default() += k;
        }
    }
    let chats = r.count(read_column(chats_db, "SELECT content_text FROM messages", true)?);
    let (n, mut rows, df) = {
        let stores = [("oracle", &oracle), ("chats", &chats)];
        let (n, rows) = r.rank(&stores, top);
        (n, rows, document_counts(&stores, 5))
    };
    let (oracle_n, chats_n) = (oracle.n, chats.n);
    let mut rooms = by_room;
    rooms.insert("chats".into(), chats);
    rooms_of(&mut rows, &rooms);
    Ok(json!({
        "generated_at": generated_at,
        "seconds": round_to(started.elapsed().as_secs_f64(), 1),
        "documents": n,
        "stores": {"oracle": oracle_n, "chats": chats_n},
        "how": HOW,
        "terms": rows,
        "df": df,
    }))
}

/// Write `v` to `out` whole: to `<out>.tmp`, mode 0644, then renamed, so a reader never sees half of it.
pub fn write_atomic(out: &Path, v: &Value) -> anyhow::Result<()> {
    if let Some(dir) = out.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = out.with_file_name(format!("{}.tmp", out.file_name().and_then(|f| f.to_str()).unwrap_or("terms.json")));
    std::fs::write(&tmp, serde_json::to_vec(v)?)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o644))?;
    }
    std::fs::rename(&tmp, out)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn counted(r: &Ranker, docs: &[&str]) -> Counts {
        r.count(docs.iter().map(|d| d.to_string()))
    }

    // The four cases of the retired ops/test_terms.py, carried over.
    #[test]
    fn a_word_in_every_document_sinks_below_one_that_concentrates() {
        let r = Ranker::default();
        let mut docs = vec!["right oracle protocol"; 3];
        docs.extend(vec!["right right lunch"; 7]);
        docs.extend(vec!["right oracle"; 2]);
        let c = counted(&r, &docs);
        let (n, rows) = r.rank(&[("oracle", &c)], 500);
        assert_eq!(n, 12);
        let terms: Vec<&str> = rows.iter().map(|r| r["term"].as_str().unwrap()).collect();
        // oracle 5x in 5/12 = 4.38, protocol 3x in 3/12 = 4.16, lunch 7x in 7/12 = 3.77, right in 12/12 = 0
        assert_eq!(terms, ["oracle", "protocol", "lunch", "right"]);
        assert_eq!(rows[3]["common"], true, "common words are kept and marked, not removed");
        assert_eq!(rows[0]["oracle_count"], 5);
        assert_eq!(rows[0]["rank"], 1);
    }

    #[test]
    fn a_word_knows_the_rooms_it_was_said_in_and_how_often() {
        let r = Ranker::default();
        let rooms = r.count_by_room(vec![
            ("axxis".to_string(), "recall recall recall is slow".to_string()),
            ("axxis".to_string(), "recall again".to_string()),
            ("oracle".to_string(), "recall once; graph graph".to_string()),
            ("dot".to_string(), "nothing here".to_string()),
        ]);
        let mut total = Counts::default();
        for c in rooms.values() {
            total.n += c.n;
            for (w, k) in &c.tf {
                *total.tf.entry(w.clone()).or_default() += k;
            }
            for (w, k) in &c.df {
                *total.df.entry(w.clone()).or_default() += k;
            }
        }
        let (_, mut rows) = r.rank(&[("oracle", &total)], 10);
        rooms_of(&mut rows, &rooms);
        let recall = rows.iter().find(|x| x["term"] == "recall").unwrap();
        assert_eq!(recall["rooms"], json!({"axxis": 4, "oracle": 1}), "rooms by count, then name; a room that never said it is absent");
    }

    #[test]
    fn an_import_header_is_not_the_rooms_words() {
        let r = Ranker::default();
        let by = r.count_by_room([("illuminati".to_string(), "[CLAUDE-EXPORT · blaze · 2026-03-05 · room:8cbfa7a9 'Hogwarts' · turn 35 · speaker:assistant]\nHogwarts as a training ground for agents".to_string())]);
        let tf = &by["illuminati"].tf;
        for w in ["export", "speaker", "assistant", "turn"] {
            assert!(!tf.contains_key(w), "{w} is the import's header");
        }
        assert!(tf.contains_key("hogwarts") && tf.contains_key("training"), "the turn's own words count");
    }

    #[test]
    fn redaction_marks_are_not_words() {
        let r = Ranker::default();
        let mut docs = vec!["xxxx oracle"; 4];
        docs.push("lunch");
        let (_, rows) = r.rank(&[("x", &counted(&r, &docs))], 500);
        assert!(rows.iter().all(|r| r["term"] != "xxxx"));
    }

    #[test]
    fn document_counts_cover_every_word_past_the_floor() {
        let r = Ranker::default();
        let mut docs = vec!["right a"; 6];
        docs.extend(vec!["oracle"; 2]);
        let c = counted(&r, &docs);
        assert_eq!(Value::Object(document_counts(&[("x", &c)], 5)), json!({"right": 6}));
    }

    #[test]
    fn rare_words_need_three_documents() {
        let r = Ranker::default();
        let (_, rows) = r.rank(&[("x", &counted(&r, &["alpha beta", "alpha", "alpha gamma"]))], 500);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["term"], "alpha");
    }

    // What Python's `[^\W\d_][^\W_]{2,29}` finds, case by case (checked against python3 on core, 2026-10-17).
    #[test]
    fn words_are_what_the_python_pattern_found() {
        let r = Ranker::default();
        assert_eq!(r.words("Oracle's 3rd x2y snake_case ab ÉCOLE"), ["oracle", "x2y", "snake", "case", "école"], "a digit may follow, never lead");
        assert_eq!(r.words("Ⅻth ½ab"), ["ⅻth", "½ab"], "a non-decimal number may start a word");
        assert_eq!(r.words(&"a".repeat(65)), ["a".repeat(30), "a".repeat(30), "a".repeat(5)], "a long run is cut every 30");
        assert_eq!(r.words("नमस्ते"), ["नमस"], "a combining mark ends a word, as in Python");
    }

    #[test]
    fn rounding_is_pythons() {
        assert_eq!((round_to(0.25, 1), round_to(2.675, 2), round_to(4.3778, 1)), (0.2, 2.67, 4.4));
    }
}
