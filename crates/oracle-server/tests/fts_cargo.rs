//! fts_cargo_v1: a LEXICAL repair. A message's cargo text fields (`items[].text`, `body`) are in its search row, so a phrase that lives only in the cargo is found
//! by its own words, and a hit says where it matched (`match_in`, `match_pointer`, `match_span`). The semantic index is separate and not touched. Real signed
//! writes on the real store.

use ed25519_dalek::SigningKey;
use oracle_proto::{pubkey_hex, Envelope, KeyAction, KeyEvent, Kind};
use oracle_server::roomread::{search, SearchQ};
use oracle_server::store::{fts_cargo_backfill, LegacyRow, Principal, Scope, Store};
use serde_json::{json, Value};

const NOW: i64 = 1_800_000_000_000;
const MIN: i64 = 60_000;
const PHRASE: &str = "Oracle and DOT are independent open-source repos; each works alone and both work together";

fn sk(n: u8) -> SigningKey {
    SigningKey::from_bytes(&[n; 32])
}
fn oracle() -> Store {
    let s = Store::memory().unwrap();
    s.init_owner("blaze", &pubkey_hex(&sk(1)), NOW - 1000 * MIN).unwrap();
    s.apply_key_event(&KeyEvent::sign(&sk(1), "blaze", KeyAction::Add, "rocky", &pubkey_hex(&sk(2)), NOW - 1000 * MIN), NOW - 1000 * MIN).unwrap();
    s
}
fn say(s: &Store, room: &str, text: &str, cargo: Option<&Value>, mins: i64) -> String {
    let at = NOW + mins * MIN;
    let env = Envelope::sign(&sk(2), room, "rocky", Kind::Say, text, cargo.map(Value::to_string), vec![], at);
    s.write(&Principal { handle: "rocky".into(), scope: Scope::Write }, &env, at).unwrap().id
}
fn d5_cargo() -> Value {
    json!({"items": [
        {"text": "first decision", "k": "D1"}, {"text": "second decision"}, {"text": "third decision"}, {"text": "fourth decision"},
        {"text": PHRASE}, {"note": "an item without text"}], "id": "zzkeyonly42", "n": 7771234})
}
fn ids(v: &Value) -> Vec<String> {
    v["items"].as_array().unwrap().iter().map(|i| i["id"].as_str().unwrap().to_owned()).collect()
}
fn q(text: &str) -> SearchQ {
    SearchQ { q: text.into(), ..Default::default() }
}
type Row = (i64, String, String, String);
fn fts_rows(s: &Store) -> Vec<Row> {
    s.with_conn(|c| {
        // Before the backfill the table has no cargo column: its cargo text is empty.
        let has: i64 = c.query_row("SELECT count(*) FROM pragma_table_info('obs_fts') WHERE name='cargo'", [], |r| r.get(0))?;
        let mut st = c.prepare(&format!("SELECT rowid, content, tags, {} FROM obs_fts ORDER BY rowid", if has > 0 { "cargo" } else { "''" }))?;
        let v = st.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?.collect::<Result<Vec<_>, _>>()?;
        Ok(v)
    })
    .unwrap()
}
/// A ledger as the code before fts_cargo_v1 left it: a two-column search row (content, tags), no flag.
fn downgrade(s: &Store, rows: &[Row]) {
    s.with_conn(|c| {
        c.execute_batch("DROP TABLE obs_fts; CREATE VIRTUAL TABLE obs_fts USING fts5(content, tags, tokenize = 'porter unicode61'); DELETE FROM store_meta;")?;
        for (seq, content, tags, _) in rows {
            c.execute("INSERT INTO obs_fts(rowid,content,tags) VALUES(?1,?2,?3)", rusqlite::params![seq, content, tags])?;
        }
        Ok(())
    })
    .unwrap();
}
/// A fixture with every way a search row is written: live, imported, and an import repaired in place.
fn fixture() -> Store {
    let s = oracle();
    say(&s, "core", "decision record D5", Some(&d5_cargo()), 0);
    say(&s, "core", "plain note, no cargo", None, 1);
    say(&s, "ops", "has body", Some(&json!({"body": "the Zürich runbook says drain first", "id": "zzbodyid"})), 2);
    say(&s, "ops", "cargo that is not json", Some(&json!("just a string, not an object")), 3);
    let legacy = |cargo: &str, content: &str| LegacyRow {
        id: "OBS-core-old".into(), channel: "core".into(), author: "rocky".into(), kind: "say".into(), content: content.into(), cargo: Some(cargo.into()),
        tags: vec![], created_ms: NOW - 9000 * MIN, pubkey: String::new(), sig: String::new(), news: false, source: None, repair_legacy: false,
    };
    s.import_legacy(&[legacy(&json!({"items": [{"text": "imported quokka evidence"}]}).to_string(), "an old note")]).unwrap();
    s
}

#[test]
fn a_cargo_only_phrase_is_found_by_its_own_words_and_content_only_indexing_misses_it() {
    let s = oracle();
    let id = say(&s, "oracle", "decision record D5", Some(&d5_cargo()), 0);
    let hit = search(&s, &q("independent open-source repos work together")).unwrap();
    assert_eq!(ids(&hit), vec![id.clone()], "the cargo's own words find it");
    // Every string value is text; object keys and non-string scalars are excluded.
    assert_eq!(search(&s, &q("zzkeyonly42")).unwrap()["items"][0]["match_pointer"], "/id");
    for stray in ["7771234", "zzobjectkey"] {
        assert_eq!(search(&s, &q(stray)).unwrap()["count"], 0, "{stray} is a key or a number, not an indexed text field");
    }
    // Negative control: the search row as it was before fts_cargo_v1 (content and tags only) does not find the phrase.
    s.with_conn(|c| Ok(c.execute("UPDATE obs_fts SET cargo=''", []).unwrap())).unwrap();
    assert_eq!(search(&s, &q("independent open-source repos work together")).unwrap()["count"], 0, "content-only indexing misses it");
    assert_eq!(search(&s, &q("decision record")).unwrap()["count"], 1, "while the content is still found");
}

#[test]
fn a_reader_who_may_not_read_the_room_gets_no_match_for_a_cargo_only_phrase() {
    // Search reads what the reader's scope and the room asked for allow; the cargo column adds no way around either. Other rooms, other authors: no match.
    let s = oracle();
    let id = say(&s, "private-room", "decision record D5", Some(&d5_cargo()), 0);
    let in_room = SearchQ { room: Some("private-room".into()), ..q("independent repos") };
    assert_eq!(ids(&search(&s, &in_room).unwrap()), vec![id.clone()]);
    let elsewhere = SearchQ { room: Some("other-room".into()), ..q("independent repos") };
    assert_eq!(search(&s, &elsewhere).unwrap()["count"], 0, "asking another room finds nothing of this one");
    let other_author = SearchQ { room: Some("private-room".into()), author: Some("dinesh".into()), ..q("independent repos") };
    assert_eq!(search(&s, &other_author).unwrap()["count"], 0);
    let legacy = s.search("independent repos", Some("other-room"), None, 10).unwrap();
    assert!(legacy.is_empty(), "the older search route keeps its room filter too");
    assert_eq!(s.search("independent repos", Some("private-room"), None, 10).unwrap().len(), 1);
}

#[test]
fn backfill_of_a_fixture_equals_live_insertion_byte_for_byte() {
    let s = fixture();
    let live = fts_rows(&s);
    assert!(live.iter().any(|r| r.3.contains("independent open-source")) && live.iter().any(|r| r.3.contains("Zürich")) && live.iter().any(|r| r.3.contains("quokka")));
    assert!(live.iter().any(|r| r.3.is_empty()), "a message without cargo text has an empty cargo column");
    downgrade(&s, &live);
    assert!(fts_rows(&s).iter().all(|r| r.3.is_empty()));
    assert!(s.with_conn(|c| fts_cargo_backfill(c, 500, None)).unwrap());
    assert_eq!(fts_rows(&s), live, "content, tags and cargo text: the same bytes");
    assert_eq!(ids(&search(&s, &q("independent open-source repos")).unwrap()).len(), 1);
}

#[test]
fn an_interrupted_backfill_resumes_with_no_row_lost_or_doubled() {
    let s = fixture();
    let live = fts_rows(&s);
    downgrade(&s, &live);
    let step = |max: Option<usize>| s.with_conn(|c| fts_cargo_backfill(c, 2, max)).unwrap();
    assert!(!step(Some(1)), "stopped after one batch");
    let progress: String = s.with_conn(|c| Ok(c.query_row("SELECT v FROM store_meta WHERE k='fts_cargo_v1_seq'", [], |r| r.get(0)).unwrap())).unwrap();
    assert_eq!(progress, live[1].0.to_string(), "the saved rowid is the last one done");
    let half: i64 = s.with_conn(|c| Ok(c.query_row("SELECT count(*) FROM obs_fts_v1c", [], |r| r.get(0)).unwrap())).unwrap();
    assert_eq!(half, 2, "two rows copied so far");
    // The old table stays in use until the swap; a restart carries on from the saved rowid.
    assert_eq!(fts_rows(&s).len(), live.len());
    assert!(!step(Some(1)));
    assert!(step(None), "finishes");
    assert!(step(None), "done is a no-op");
    let after = fts_rows(&s);
    assert_eq!(after.len(), live.len(), "old and new totals are equal");
    assert_eq!(after, live);
    let (n, distinct): (i64, i64) = s.with_conn(|c| Ok(c.query_row("SELECT count(*), count(DISTINCT rowid) FROM obs_fts", [], |r| Ok((r.get(0)?, r.get(1)?))).unwrap())).unwrap();
    assert_eq!((n as usize, n), (live.len(), distinct), "no duplicate rows");
    let gone: i64 = s.with_conn(|c| Ok(c.query_row("SELECT count(*) FROM sqlite_master WHERE name='obs_fts_v1c'", [], |r| r.get(0)).unwrap())).unwrap();
    assert_eq!(gone, 0, "the scratch table is gone");
}

#[test]
fn a_hit_says_where_it_matched_with_a_pointer_and_a_span() {
    let s = fixture();
    let word_at = |text: &str, span: &Value| text[span["start"].as_u64().unwrap() as usize..span["end"].as_u64().unwrap() as usize].to_owned();

    // cargo, an item: the pointer and the span name the word in that field
    let r = search(&s, &q("independent open-source repos")).unwrap();
    let h = &r["items"][0];
    assert_eq!((h["match_in"].as_str(), h["match_pointer"].as_str()), (Some("cargo"), Some("/items/4/text")), "{h}");
    assert_eq!(h["match_span"]["unit"], "utf8_bytes");
    assert_eq!(word_at(PHRASE, &h["match_span"]), "independent open-source repos");

    // cargo, the body, after a multi-byte letter: offsets are bytes within the field
    let r = search(&s, &q("drain")).unwrap();
    let h = &r["items"][0];
    assert_eq!((h["match_in"].as_str(), h["match_pointer"].as_str()), (Some("cargo"), Some("/body")), "{h}");
    assert_eq!(word_at("the Zürich runbook says drain first", &h["match_span"]), "drain");

    // content: no pointer, the span is in the content
    let r = search(&s, &q("decision record")).unwrap();
    let h = &r["items"][0];
    assert_eq!(h["match_in"], "content");
    assert!(h.get("match_pointer").is_none());
    assert_eq!(word_at("decision record D5", &h["match_span"]), "decision record");

    let r = search(&s, &SearchQ { text_only: true, ..q("quokka") }).unwrap();
    assert_eq!(r["items"][0]["match_pointer"], "/items/0/text", "an imported row too, and in=text covers the cargo's text");

    // a tag alone is neither: the message says it matched in its tags
    let at = NOW + 50 * MIN;
    let env = Envelope::sign(&sk(2), "core", "rocky", Kind::Say, "nothing to see", None, vec!["zebratag".into()], at);
    s.write(&Principal { handle: "rocky".into(), scope: Scope::Write }, &env, at).unwrap();
    let h = &search(&s, &q("zebratag")).unwrap()["items"][0];
    assert_eq!(h["match_in"], "tags");
    assert!(h.get("match_span").is_none());
    let legacy = s.search("independent open-source repos", None, None, 5).unwrap();
    let j = serde_json::to_value(&legacy[0]).unwrap();
    assert_eq!((j["match_in"].as_str(), j["match_pointer"].as_str()), (Some("cargo"), Some("/items/4/text")), "the older search route says it too: {j}");
}

#[test]
fn literal_highlight_markers_never_invent_a_match_or_shift_utf8_spans() {
    let s = oracle();
    let content = "prefix\u{1}noise\u{2} normal";
    let body = "Zürich x\u{1}y\u{2} \u{1}oracle-fts-start-0\u{2} \u{3}oracle-fts-end-0\u{4} drain";
    let id = say(&s, "core", content, Some(&json!({"body":body})), 0);
    let before = serde_json::to_value(s.get(&id).unwrap()).unwrap();
    let r = search(&s, &q("drain")).unwrap();
    let h = &r["items"][0];
    assert_eq!(h["match_in"], "cargo", "literal content markers cannot win attribution");
    assert_eq!(h["match_pointer"], "/body");
    let start = h["match_span"]["start"].as_u64().unwrap() as usize;
    let end = h["match_span"]["end"].as_u64().unwrap() as usize;
    assert_eq!((start,end), (body.find("drain").unwrap(), body.len()));
    assert_eq!(&body[start..end], "drain");
    assert_eq!(serde_json::to_value(s.get(&id).unwrap()).unwrap(), before, "signed source bytes unchanged");
    let literal = "pré \u{1}alpha\u{2} tail";
    let id = say(&s,"core",literal,None,1);
    let h = &search(&s,&q("alpha")).unwrap()["items"][0];
    assert_eq!(h["id"],id);
    assert_eq!(h["match_in"],"content");
    assert_eq!((h["match_span"]["start"].as_u64().unwrap() as usize,h["match_span"]["end"].as_u64().unwrap() as usize),(literal.find("alpha").unwrap(),literal.find("alpha").unwrap()+5));
}

#[test]
fn resume_rebuilds_a_copied_prefix_when_only_source_cargo_changed() {
    let s = fixture();
    let live = fts_rows(&s);
    downgrade(&s,&live);
    assert!(!s.with_conn(|c| fts_cargo_backfill(c,2,Some(1))).unwrap());
    let first = live[0].0;
    let repaired = json!({"body":"repaired wombat evidence"}).to_string();
    // An old binary repairs observations + content/tags but cannot project the cargo.
    // Content/tags deliberately stay identical; only the durable source generation sees it.
    s.with_conn(|c| {
        c.execute("UPDATE observations SET cargo=?2 WHERE seq=?1",rusqlite::params![first,repaired])?;
        Ok(())
    }).unwrap();
    let source = s.with_conn(|c| Ok(c.query_row("SELECT cargo FROM observations WHERE seq=?1",[first],|r|r.get::<_,String>(0))?)).unwrap();
    assert!(s.with_conn(|c| fts_cargo_backfill(c,2,None)).unwrap());
    assert_eq!(fts_rows(&s)[0].3,"repaired wombat evidence");
    assert_eq!(search(&s,&q("wombat")).unwrap()["count"],1);
    assert_eq!(search(&s,&q("independent repos")).unwrap()["count"],0);
    assert_eq!(s.with_conn(|c| Ok(c.query_row("SELECT cargo FROM observations WHERE seq=?1",[first],|r|r.get::<_,String>(0))?)).unwrap(),source);
    let after = fts_rows(&s);
    assert!(s.with_conn(|c| fts_cargo_backfill(c,2,None)).unwrap());
    assert_eq!(fts_rows(&s),after);
}

#[test]
fn old_format_null_rows_are_readable_and_completed_migrations_catch_up_old_writes() {
    let s = fixture();
    let seq = fts_rows(&s)[0].0;
    s.with_conn(|c| {
        let tx=c.unchecked_transaction()?;
        tx.execute("UPDATE observations SET cargo=?2 WHERE seq=?1",rusqlite::params![seq,json!({"body":"rollback numbat"}).to_string()])?;
        tx.execute("DELETE FROM obs_fts WHERE rowid=?1",[seq])?;
        // Exact old index format: omitted cargo is SQL NULL, not an empty string.
        tx.execute("INSERT INTO obs_fts(rowid,content,tags) SELECT seq,content,'zzch636f7265 zzclsmessage' FROM observations WHERE seq=?1",[seq])?;
        tx.commit()?;
        Ok(())
    }).unwrap();
    assert_eq!(s.search("decision record",Some("core"),None,10).unwrap().len(),1,"NULL highlight must not fail a content hit before repair");
    // A later known new writer must not bless the earlier unknown old write.
    say(&s,"core","new writer",Some(&json!({"body":"new bandicoot"})),20);
    assert!(s.with_conn(|c| fts_cargo_backfill(c,2,None)).unwrap());
    assert_eq!(search(&s,&q("numbat")).unwrap()["count"],1);
    assert_eq!(search(&s,&q("bandicoot")).unwrap()["count"],1);
    assert_eq!(search(&s,&q("independent repos")).unwrap()["count"],0);
    assert!(fts_rows(&s).iter().any(|r|r.0==seq && r.3=="rollback numbat"));
}

#[test]
fn prior_v1_done_does_not_hide_old_writes_before_generation_triggers_existed() {
    let s = fixture();
    let seq=fts_rows(&s)[0].0;
    s.with_conn(|c| {
        c.execute_batch("DROP TRIGGER fts_cargo_source_insert; DROP TRIGGER fts_cargo_source_update; DROP TRIGGER fts_cargo_source_delete; DELETE FROM store_meta WHERE k LIKE 'fts_cargo_%'; INSERT INTO store_meta VALUES('fts_cargo_v1','done');")?;
        c.execute("UPDATE observations SET cargo=?2 WHERE seq=?1",rusqlite::params![seq,json!({"body":"pretrigger echidna"}).to_string()])?;
        Ok(())
    }).unwrap();
    assert!(s.with_conn(|c| fts_cargo_backfill(c,2,None)).unwrap());
    assert_eq!(search(&s,&q("echidna")).unwrap()["count"],1);
    assert_eq!(search(&s,&q("independent repos")).unwrap()["count"],0);
}

#[test]
fn resume_preserves_current_fts_membership_and_retagging_without_resurrecting_retired_rows() {
    let s=fixture();let live=fts_rows(&s);downgrade(&s,&live);
    assert!(!s.with_conn(|c|fts_cargo_backfill(c,2,Some(1))).unwrap());
    s.with_conn(|c| {
        c.execute("UPDATE obs_fts SET tags=tags || ' changedtag' WHERE rowid=?1",[live[0].0])?;
        c.execute("DELETE FROM obs_fts WHERE rowid=?1",[live[1].0])?;
        Ok(())
    }).unwrap();
    assert!(s.with_conn(|c|fts_cargo_backfill(c,2,None)).unwrap());
    let after=fts_rows(&s);
    assert!(after[0].2.contains("changedtag"));
    assert!(!after.iter().any(|r|r.0==live[1].0));
    assert_eq!(after.len(),live.len()-1);
}

#[test]
fn genuine_source_backed_import_repair_replaces_cargo_terms_at_the_same_seq() {
    let s=fixture();
    let old=s.get("OBS-core-old").unwrap();
    let repaired=LegacyRow {
        id:old.id.clone(),channel:old.channel.clone(),author:old.author.clone(),kind:old.kind.clone(),content:old.content.clone(),
        cargo:Some(json!({"items":[{"text":"repaired possum"}]}).to_string()),tags:old.tags.clone(),created_ms:old.created_ms,
        pubkey:old.pubkey.clone(),sig:old.sig.clone(),news:false,source:Some(json!({"fixture":"source-backed repair"})),repair_legacy:true,
    };
    assert_eq!(s.import_legacy(std::slice::from_ref(&repaired)).unwrap(),0);
    assert_eq!(s.get(&old.id).unwrap().seq,old.seq);
    assert_eq!(search(&s,&q("quokka")).unwrap()["count"],0);
    let r=search(&s,&q("possum")).unwrap();
    assert_eq!(r["count"],1);assert_eq!(r["items"][0]["match_pointer"],"/items/0/text");
    assert_eq!(s.import_legacy(std::slice::from_ref(&repaired)).unwrap(),0);
    let before=fts_rows(&s);
    assert!(s.with_conn(|c|fts_cargo_backfill(c,2,None)).unwrap());
    assert_eq!(fts_rows(&s),before);
}


fn word_at<'a>(text: &'a str, span: &Value) -> &'a str {
    assert_eq!(span["unit"], "utf8_bytes");
    &text[span["start"].as_u64().unwrap() as usize..span["end"].as_u64().unwrap() as usize]
}

#[test]
fn recursive_string_leaves_preserve_document_order_and_escaped_pointers_without_wrapper_invention() {
    use oracle_server::store::cargo_text_fields;
    let raw = r#"{"z":["first",{"a/b~c":"Zürich final evidence","n":42,"nil":null,"bool":true}],"a":"last","zzobjectkey":99}"#;
    assert_eq!(cargo_text_fields(Some(raw)), vec![("/z/0".into(),"first".into()),("/z/1/a~1b~0c".into(),"Zürich final evidence".into()),("/a".into(),"last".into())]);
    assert!(cargo_text_fields(Some("not-json")).is_empty());
    assert!(cargo_text_fields(Some("null")).is_empty());
    let wrapped = json!(r#"{"body":"literal serialized payload"}"#).to_string();
    assert_eq!(cargo_text_fields(Some(&wrapped)),vec![(String::new(),r#"{"body":"literal serialized payload"}"#.into())]);
    let s=oracle();
    let env=Envelope::sign(&sk(2),"core","rocky",Kind::Say,"face",Some(raw.into()),vec![],NOW);
    let id=s.write(&Principal{handle:"rocky".into(),scope:Scope::Write},&env,NOW).unwrap().id;
    let before=serde_json::to_value(s.get(&id).unwrap()).unwrap();
    let r=search(&s,&q("\"Zürich final evidence\"")).unwrap();let h=&r["items"][0];
    assert_eq!(h["match_pointer"],"/z/1/a~1b~0c");assert_eq!(word_at("Zürich final evidence",&h["match_span"]),"Zürich final evidence");
    for negative in ["zzobjectkey","42","nil","bool"] {assert_eq!(search(&s,&q(negative)).unwrap()["count"],0);}
    assert_eq!(serde_json::to_value(s.get(&id).unwrap()).unwrap(),before);
}

#[test]
fn quoted_phrases_and_dense_windows_prefer_complete_unrelated_cargo_over_repeated_common_content() {
    let s=oracle();let body="pré silver river δέλτα post";
    say(&s,"core","river river river silver noise",Some(&json!({"ui_policy":["irrelevant",body]})),0);
    say(&s,"core","silver unrelated river then δέλτα",None,1);
    let (hits,engine)=s.search_with("\"silver river δέλτα\"",Some("core"),None,10,None).unwrap();
    assert_eq!(engine,"keyword");assert_eq!(hits.len(),1);
    let h=serde_json::to_value(&hits[0]).unwrap();assert_eq!(h["match_in"],"cargo");assert_eq!(h["match_pointer"],"/ui_policy/1");
    assert_eq!(word_at(body,&h["match_span"]),"silver river δέλτα");
    let dense=search(&s,&q("silver river δέλτα")).unwrap();
    let h=dense["items"].as_array().unwrap().iter().find(|h|h["match_pointer"]=="/ui_policy/1").unwrap();
    assert_eq!(word_at(body,&h["match_span"]),"silver river δέλτα");
    // Quoting preserves term adjacency; injection-looking characters never become syntax.
    assert_eq!(search(&s,&q("\"silver nonexistent river\"")).unwrap()["count"],0);
    assert_eq!(search(&s,&SearchQ{room:Some("other".into()),..q("\"silver river δέλτα\" OR core")}).unwrap()["count"],0);
}

#[test]
fn quoted_no_match_reports_relaxation_and_unquoted_fallback_still_finds_words() {
    let s=oracle();say(&s,"core","silver unrelated river",None,0);
    let (hits,engine)=s.search_with("\"silver river\"",Some("core"),None,10,None).unwrap();
    assert_eq!(hits.len(),1);assert!(engine.contains("quoted phrase relaxed"));
    let h=serde_json::to_value(&hits[0]).unwrap();assert_eq!(word_at("silver unrelated river",&h["match_span"]),"silver unrelated river");
    assert_eq!(s.search("silver absent",Some("core"),None,10).unwrap().len(),1,"existing any-word fallback");
    let huge=format!("\"{}\"",vec!["silver";25].join(" "));
    assert!(s.search(&huge,Some("core"),None,10).unwrap().is_empty(),"never clip an oversized quoted phrase");
}

#[test]
fn quoted_phrase_crossing_string_leaves_has_no_invented_field_span() {
    let s=oracle();say(&s,"core","face",Some(&json!({"a":"silver","b":"river"})),0);
    let r=search(&s,&q("\"silver river\"")).unwrap();let h=&r["items"][0];
    assert_eq!(h["match_in"],"cargo");assert!(h.get("match_pointer").is_none()&&h.get("match_span").is_none());
}

#[test]
fn v3_reprojects_completed_and_interrupted_v2_without_losing_sources_or_rows() {
    let s=oracle();let id=say(&s,"core","face",Some(&json!({"body":"old explicit body","ui_policy":["legacy multi sidebar shell"]})),0);
    let source=serde_json::to_value(s.get(&id).unwrap()).unwrap();
    s.with_conn(|c|{c.execute_batch("UPDATE obs_fts SET cargo='old explicit body'; DELETE FROM store_meta WHERE k IN ('fts_cargo_v3','fts_cargo_v3_projector'); CREATE VIRTUAL TABLE obs_fts_v1c USING fts5(content,tags,cargo,tokenize='porter unicode61'); INSERT INTO obs_fts_v1c(rowid,content,tags,cargo) SELECT rowid,content,tags,cargo FROM obs_fts; INSERT OR REPLACE INTO store_meta SELECT 'fts_cargo_v1_seq',CAST(max(rowid) AS TEXT) FROM obs_fts; INSERT OR REPLACE INTO store_meta SELECT 'fts_cargo_v2_generation',v FROM store_meta WHERE k='fts_cargo_source_generation';")?;assert!(fts_cargo_backfill(c,1,None)?);Ok(())}).unwrap();
    assert_eq!(search(&s,&q("\"legacy multi sidebar shell\"")).unwrap()["items"][0]["match_pointer"],"/ui_policy/0");
    assert_eq!(serde_json::to_value(s.get(&id).unwrap()).unwrap(),source);
    // A previously completed v2 without a shadow must rebuild too.
    s.with_conn(|c|{c.execute_batch("UPDATE obs_fts SET cargo='old explicit body'; DELETE FROM store_meta WHERE k IN ('fts_cargo_v3','fts_cargo_v3_projector');")?;assert!(fts_cargo_backfill(c,1,None)?);Ok(())}).unwrap();
    assert_eq!(search(&s,&q("sidebar")).unwrap()["count"],1);
    assert_eq!(serde_json::to_value(s.get(&id).unwrap()).unwrap(),source);
}
