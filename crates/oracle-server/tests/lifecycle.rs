//! The code against its charts (src/lifecycle.rs). Random event sequences, from a fixed seed, run through the real
//! derivations: `tasks::derive` for an obligation, `asks::asks_for` for an ask as one seat sees it. After every event the
//! state is read again; a change the chart has no edge for fails, and so does an edge no sequence ever took. The SCXML
//! files beside the charts are checked to be what `lifecycle::scxml` writes, so the picture never drifts from the test.

use ed25519_dalek::SigningKey;
use oracle_proto::{pubkey_hex, Envelope, KeyAction, KeyEvent, Kind};
use oracle_server::asks::{asks_for, StateFilter};
use oracle_server::lifecycle::{scxml, Chart, Edge, ASK_FOR_SEAT, CHARTS, OBLIGATION};
use oracle_server::store::{Observation, Principal, Scope, Store};
use std::collections::BTreeSet;

/// A small deterministic generator (an LCG): the same sequences on every run and every machine.
struct Rng(u64);
impl Rng {
    fn pick(&mut self, n: usize) -> usize {
        self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        ((self.0 >> 33) % n as u64) as usize
    }
}

fn check(chart: &Chart, from: &str, event: &str, to: &str, trail: &[String], used: &mut BTreeSet<Edge>) {
    if from == to {
        return;
    }
    match chart.edge(from, event, to) {
        Some(e) => {
            used.insert(*e);
        }
        None => panic!("{}: {from} --{event}--> {to} is not an edge of the chart\nafter: {}", chart.id, trail.join(" · ")),
    }
}

fn every_edge_taken(chart: &Chart, used: &BTreeSet<Edge>) {
    let missing: Vec<&Edge> = chart.edges.iter().filter(|e| !used.contains(e)).collect();
    assert!(missing.is_empty(), "{}: edges the code never took (the chart allows more than the code does): {missing:#?}", chart.id);
}

// ── obligation ────────────────────────────────────────────────────────────────────────────────────────────────────────

fn obs(seq: i64, author: &str, tags: Vec<String>) -> Observation {
    Observation {
        seq, id: format!("OBS-{seq}"), channel: "oracle".into(), author: author.into(), kind: "say".into(), content: "x".into(), cargo: None,
        tags, created_ms: seq * 1000, authored_ms: None, pubkey: String::new(), sig: String::new(), envelope: 2,
    }
}

const OB_EVENTS: [&str; 16] = [
    "commit.holder", "commit.other", "result.holder", "result.other", "reply.holder", "verdict.approve.holder", "verdict.changes.holder",
    "verdict.approve.other", "revision.implementer", "revision.other", "close.asker", "cancel.asker", "close.decider", "cancel.decider",
    "close.other", "cancel.other",
];

#[test]
fn an_obligation_moves_only_along_its_chart() {
    let mut rng = Rng(0x0b1_1fe);
    let mut used = BTreeSet::new();
    // Closes end an obligation for good, so they are drawn a third as often as the rest: the long paths (approve, then a
    // new revision) are reached too.
    let weighted: Vec<&str> = OB_EVENTS.iter().flat_map(|e| std::iter::repeat_n(*e, if e.starts_with("close.") || e.starts_with("cancel.") { 1 } else { 3 })).collect();
    for run in 0..1500 {
        let kind = if run % 3 == 0 { "act" } else { "review" };
        // rocky asks emmy; blaze decides; dinesh is a bystander. The implementer of a review is its asker, of an act its holder.
        let implementer = if kind == "review" { "rocky" } else { "emmy" };
        let mut msgs = vec![obs(1, "rocky", vec!["subject:x".into(), format!("ask:{kind}"), "to:emmy".into()])];
        let (mut revs, mut trail) = (0usize, vec![format!("{kind} ask")]);
        let state = |msgs: &[Observation]| {
            let t = oracle_server::tasks::derive(msgs, &["blaze".to_string()], &Default::default());
            t[0].obligations[0].state.clone()
        };
        let mut prev = state(&msgs);
        assert_eq!(prev, OBLIGATION.initial);
        for _ in 0..(4 + rng.pick(20)) {
            let ev = weighted[rng.pick(weighted.len())];
            let seq = msgs.len() as i64 + 1;
            let cur = if revs == 0 { "r0".to_string() } else { format!("r{revs}") };
            let (who, mut tags): (&str, Vec<String>) = match ev {
                "commit.holder" => ("emmy", vec!["re:OBS-1".into(), "intent:commit".into()]),
                "commit.other" => ("dinesh", vec!["re:OBS-1".into(), "intent:commit".into()]),
                "result.holder" => ("emmy", vec!["re:OBS-1".into(), "intent:result".into()]),
                "result.other" => ("dinesh", vec!["re:OBS-1".into(), "intent:result".into()]),
                "reply.holder" => ("emmy", vec!["re:OBS-1".into()]),
                "verdict.approve.holder" => ("emmy", vec![format!("rev:{cur}"), "verdict:approve".into()]),
                "verdict.changes.holder" => ("emmy", vec![format!("rev:{cur}"), "verdict:changes".into()]),
                "verdict.approve.other" => ("dinesh", vec![format!("rev:{cur}"), "verdict:approve".into()]),
                "revision.implementer" | "revision.other" => {
                    let who = if ev == "revision.other" { "dinesh" } else { implementer };
                    let mut t = vec!["intent:result".to_string(), format!("rev:r{}", revs + 1)];
                    if revs > 0 {
                        t.push(format!("prev:r{revs}"));
                    }
                    if ev == "revision.implementer" {
                        revs += 1;
                    }
                    (who, t)
                }
                "close.asker" => ("rocky", vec!["closes:OBS-1".into()]),
                "cancel.asker" => ("rocky", vec!["closes:OBS-1".into(), "done:cancelled".into()]),
                "close.decider" => ("blaze", vec!["closes:OBS-1".into()]),
                "cancel.decider" => ("blaze", vec!["closes:OBS-1".into(), "done:cancelled".into()]),
                "close.other" => ("dinesh", vec!["closes:OBS-1".into()]),
                "cancel.other" => ("dinesh", vec!["closes:OBS-1".into(), "done:cancelled".into()]),
                _ => unreachable!(),
            };
            tags.push("subject:x".into());
            msgs.push(obs(seq, who, tags));
            trail.push(ev.to_string());
            let next = state(&msgs);
            check(&OBLIGATION, &prev, ev, &next, &trail, &mut used);
            prev = next;
        }
    }
    every_edge_taken(&OBLIGATION, &used);
}

#[test]
fn a_new_revision_reopens_a_finished_review_as_far_as_its_holder_had_got() {
    let st = |msgs: &[Observation]| oracle_server::tasks::derive(msgs, &["blaze".to_string()], &Default::default())[0].obligations[0].state.clone();
    let tag = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
    let ask = obs(1, "rocky", tag(&["subject:x", "ask:review", "to:emmy"]));
    let r1 = obs(2, "rocky", tag(&["subject:x", "intent:result", "rev:r1"]));
    let ok = obs(3, "emmy", tag(&["subject:x", "rev:r1", "verdict:approve"]));
    let r2 = obs(4, "rocky", tag(&["subject:x", "intent:result", "rev:r2", "prev:r1"]));
    assert_eq!(st(&[ask.clone(), r1.clone(), ok.clone()]), "done");
    assert_eq!(st(&[ask.clone(), r1.clone(), ok.clone(), r2.clone()]), "open", "done --revision.implementer--> open");
    let commit = obs(3, "emmy", tag(&["subject:x", "re:OBS-1", "intent:commit"]));
    let ok4 = obs(4, "emmy", tag(&["subject:x", "rev:r1", "verdict:changes"]));
    let r2b = obs(5, "rocky", tag(&["subject:x", "intent:result", "rev:r2", "prev:r1"]));
    assert_eq!(st(&[ask.clone(), r1.clone(), commit.clone(), ok4.clone()]), "changes");
    assert_eq!(st(&[ask, r1, commit, ok4, r2b]), "owned", "changes --revision.implementer--> owned");
}

// ── ask, as one seat sees it ──────────────────────────────────────────────────────────────────────────────────────────

const NOW: i64 = 1_800_000_000_000;
const HOUR: i64 = 3_600_000;

fn sk(n: u8) -> SigningKey {
    SigningKey::from_bytes(&[n; 32])
}
fn oracle() -> Store {
    let s = Store::memory().unwrap();
    s.init_owner("blaze", &pubkey_hex(&sk(1)), NOW - 100 * HOUR).unwrap();
    for (h, k) in [("rocky", 2), ("dinesh", 3)] {
        s.apply_key_event(&KeyEvent::sign(&sk(1), "blaze", KeyAction::Add, h, &pubkey_hex(&sk(k)), NOW - 100 * HOUR), NOW - 100 * HOUR).unwrap();
    }
    s
}
fn say(s: &Store, who: &str, tags: Vec<String>, at: i64) -> String {
    let k = match who { "blaze" => 1, "rocky" => 2, _ => 3 };
    let env = Envelope::sign(&sk(k), "core", who, Kind::Say, "x", None, tags, at);
    s.write(&Principal { handle: who.into(), scope: Scope::Write }, &env, at).unwrap().id
}
fn list_of(s: &Store, q: &str) -> &'static str {
    let a = asks_for(s, "rocky", None, StateFilter::All, NOW).unwrap();
    let on = |l: &[oracle_server::asks::AskRow]| l.iter().any(|r| r.id == q);
    match (on(&a.waiting_on_me), on(&a.acked), on(&a.stuck), on(&a.later)) {
        (true, false, false, false) => "waiting",
        (false, true, false, false) => "acked",
        (false, false, true, false) => "stuck",
        (false, false, false, true) => "later",
        (false, false, false, false) => "off",
        other => panic!("one ask on two of rocky's lists at once: {other:?}"),
    }
}

const ASK_EVENTS: [&str; 15] = [
    "mark.seen", "mark.ack", "mark.go", "mark.stuck", "mark.later", "mark.later_past", "mark.later_untimed", "mark.done", "mark.no",
    "other.mark.done", "other.mark.ack", "deliver.seat", "deliver.other", "close.asker", "reply.other",
];

#[test]
fn an_ask_moves_between_a_seats_lists_only_along_its_chart() {
    let mut rng = Rng(0xa5c);
    let mut used = BTreeSet::new();
    for _ in 0..160 {
        let s = oracle();
        // blaze asks rocky and dinesh to act; rocky's inbox is the chart.
        let mut at = NOW - 50 * HOUR;
        let q = say(&s, "blaze", vec!["to:rocky".into(), "to:dinesh".into(), "ask:act".into()], at);
        let mut prev = list_of(&s, &q);
        assert_eq!(prev, ASK_FOR_SEAT.initial);
        let mut trail = vec!["act ask to rocky and dinesh".to_string()];
        for _ in 0..(3 + rng.pick(10)) {
            at += 60_000;
            let ev = ASK_EVENTS[rng.pick(ASK_EVENTS.len())];
            let mark = |m: &str| vec![format!("reply:{q}"), format!("react:{m}")];
            let (who, tags, chart_event): (&str, Vec<String>, &str) = match ev {
                "mark.seen" => ("rocky", mark("seen"), "mark.seen"),
                "mark.ack" => ("rocky", mark("ack"), "mark.ack"),
                "mark.go" => ("rocky", mark("go"), "mark.ack"),
                "mark.stuck" => ("rocky", mark("stuck"), "mark.stuck"),
                "mark.later" => ("rocky", [mark("later"), vec![format!("until:{}", NOW + 10 * HOUR)]].concat(), "mark.later"),
                "mark.later_past" => ("rocky", [mark("later"), vec![format!("until:{}", NOW - HOUR)]].concat(), "mark.later_past"),
                "mark.later_untimed" => ("rocky", mark("later"), "mark.later_untimed"),
                "mark.done" => ("rocky", mark("done"), "mark.done"),
                "mark.no" => ("rocky", mark("no"), "mark.done"),
                "other.mark.done" => ("dinesh", mark("done"), "other.mark"),
                "other.mark.ack" => ("dinesh", mark("ack"), "other.mark"),
                "deliver.seat" => ("rocky", vec![format!("reply:{q}"), "intent:result".into()], "deliver.seat"),
                "deliver.other" => ("dinesh", vec![format!("reply:{q}"), "intent:result".into()], "deliver.other"),
                "close.asker" => ("blaze", vec![format!("closes:{q}")], "close.asker"),
                "reply.other" => ("dinesh", vec![format!("reply:{q}")], "reply.other"),
                _ => unreachable!(),
            };
            say(&s, who, tags, at);
            trail.push(ev.to_string());
            let next = list_of(&s, &q);
            check(&ASK_FOR_SEAT, prev, chart_event, next, &trail, &mut used);
            prev = next;
        }
    }
    every_edge_taken(&ASK_FOR_SEAT, &used);
}

#[test]
fn the_scxml_files_are_what_the_charts_write() {
    for c in CHARTS {
        let path = format!("{}/statecharts/{}.scxml", env!("CARGO_MANIFEST_DIR"), c.id);
        let want = scxml(c);
        if std::env::var("ORACLE_WRITE_CHARTS").is_ok() {
            std::fs::create_dir_all(std::path::Path::new(&path).parent().unwrap()).unwrap();
            std::fs::write(&path, &want).unwrap();
        }
        let have = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{path}: {e} (write it with ORACLE_WRITE_CHARTS=1 cargo test --test lifecycle)"));
        assert_eq!(have, want, "{path} drifted from src/lifecycle.rs: rewrite it with ORACLE_WRITE_CHARTS=1");
    }
}
