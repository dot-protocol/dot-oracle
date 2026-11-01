//! The lifecycles as statecharts (Blaze, 1 Nov 2026: "go for the statechart"; David Khourshid's "Statecharts in 2 minutes").
//! An obligation and an ask are each one chart: states, the events that move them, and who
//! may send each event. The charts are data, so a test drives the real code ([`crate::tasks`], [`crate::asks`]) through
//! random event sequences and fails on any change of state that is not an edge here (tests/lifecycle.rs), and fails again if
//! an edge here is never taken (a chart that allows more than the code does is wrong too). [`scxml`] writes a chart in the
//! W3C format, which state.new and XState read, so a person can see it and simulate it.

/// One edge: from a state (or `*`, any state of the region), on an event, to a state. `event` names who sent it after a dot
/// (`result.holder`), so a guard on the sender is part of the edge, not a comment.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Edge {
    pub from: &'static str,
    pub event: &'static str,
    pub to: &'static str,
    /// The condition, in words (it becomes the SCXML `cond` comment); empty when there is none.
    pub when: &'static str,
}

#[derive(Debug)]
pub struct Chart {
    pub id: &'static str,
    pub about: &'static str,
    pub initial: &'static str,
    pub states: &'static [&'static str],
    /// States nothing leaves except the events that name them in `edges`.
    pub edges: &'static [Edge],
}

const fn e(from: &'static str, event: &'static str, to: &'static str, when: &'static str) -> Edge {
    Edge { from, event, to, when }
}

/// One holder's part of an act, review or decide ask on a subject (`tasks::fold`). An event no edge names moves nothing:
/// a reply, anything a non-holder says, a verdict on an act ask.
pub const OBLIGATION: Chart = Chart {
    id: "obligation",
    about: "one holder's part of an addressed act, review or decide ask on a subject",
    initial: "open",
    states: &["open", "owned", "answered", "changes", "done", "cancelled"],
    edges: &[
        e("open", "commit.holder", "owned", "the holder says it takes it (intent:commit)"),
        e("open", "result.holder", "answered", "the holder's result or receipt"),
        e("owned", "result.holder", "answered", "the holder's result or receipt"),
        // A review is settled by the holder's newest verdict on the current revision, and only there.
        e("open", "verdict.approve.holder", "done", "review; on the current revision"),
        e("owned", "verdict.approve.holder", "done", "review; on the current revision"),
        e("answered", "verdict.approve.holder", "done", "review; on the current revision"),
        e("changes", "verdict.approve.holder", "done", "review; on the current revision"),
        e("open", "verdict.changes.holder", "changes", "review; on the current revision"),
        e("owned", "verdict.changes.holder", "changes", "review; on the current revision"),
        e("answered", "verdict.changes.holder", "changes", "review; on the current revision"),
        e("done", "verdict.changes.holder", "changes", "review; the newest verdict wins"),
        // A new current revision leaves the old verdict on its own revision: the review is open again, as far as the
        // holder had got before it (the implementer's evidence, rev: with prev: naming the current one).
        e("done", "revision.implementer", "open", "review; the holder had not committed or answered"),
        e("done", "revision.implementer", "owned", "review; the holder had committed"),
        e("done", "revision.implementer", "answered", "review; the holder had answered"),
        e("changes", "revision.implementer", "open", "review; the holder had not committed or answered"),
        e("changes", "revision.implementer", "owned", "review; the holder had committed"),
        e("changes", "revision.implementer", "answered", "review; the holder had answered"),
        // Closing is the asker's or a room decider's, and wins over everything; the newest close wins.
        e("*", "close.asker", "done", ""),
        e("*", "close.decider", "done", ""),
        e("*", "cancel.asker", "cancelled", ""),
        e("*", "cancel.decider", "cancelled", ""),
    ],
};

/// Where an ask stands for one seat it names (`asks::asks_for`): the list it sits on in that seat's inbox. `off` is on
/// none: the seat marked it done or no, delivered its part, or the ask settled with nothing left for it.
pub const ASK_FOR_SEAT: Chart = Chart {
    id: "ask_for_seat",
    about: "an open ask as one named seat's inbox shows it: which list, or none",
    initial: "waiting",
    states: &["waiting", "acked", "stuck", "later", "off"],
    edges: &[
        // The seat's own marks: the newest mark wins.
        e("waiting", "mark.ack", "acked", "ack or go"),
        e("stuck", "mark.ack", "acked", "ack or go"),
        e("later", "mark.ack", "acked", "ack or go"),
        e("off", "mark.ack", "acked", "the ask is still open for this seat"),
        e("waiting", "mark.stuck", "stuck", ""),
        e("acked", "mark.stuck", "stuck", ""),
        e("later", "mark.stuck", "stuck", ""),
        e("off", "mark.stuck", "stuck", "the ask is still open for this seat"),
        e("waiting", "mark.later", "later", "until a time still to come"),
        e("acked", "mark.later", "later", "until a time still to come"),
        e("stuck", "mark.later", "later", "until a time still to come"),
        e("off", "mark.later", "later", "the ask is still open for this seat"),
        // seen, or a later whose time has passed, keeps it asking.
        e("acked", "mark.seen", "waiting", ""),
        e("stuck", "mark.seen", "waiting", ""),
        e("later", "mark.seen", "waiting", ""),
        e("off", "mark.seen", "waiting", "the ask is still open for this seat"),
        e("acked", "mark.later_past", "waiting", "its time has passed"),
        e("stuck", "mark.later_past", "waiting", "its time has passed"),
        e("later", "mark.later_past", "waiting", "its time has passed"),
        e("off", "mark.later_past", "waiting", "the ask is still open for this seat"),
        e("waiting", "mark.done", "off", "done or no"),
        e("acked", "mark.done", "off", "done or no"),
        e("stuck", "mark.done", "off", "done or no"),
        e("later", "mark.done", "off", "done or no"),
        // The seat delivers its part (a result on an act ask, any answer on a reply ask), or the asker closes it.
        e("waiting", "deliver.seat", "off", ""),
        e("acked", "deliver.seat", "off", ""),
        e("stuck", "deliver.seat", "off", ""),
        e("later", "deliver.seat", "off", ""),
        e("waiting", "close.asker", "off", ""),
        e("acked", "close.asker", "off", ""),
        e("stuck", "close.asker", "off", ""),
        e("later", "close.asker", "off", ""),
    ],
};

pub const CHARTS: [&Chart; 2] = [&OBLIGATION, &ASK_FOR_SEAT];

impl Chart {
    /// The edge an observed change takes, if the chart has one.
    pub fn edge(&self, from: &str, event: &str, to: &str) -> Option<&'static Edge> {
        self.edges.iter().find(|x| (x.from == from || x.from == "*") && x.event == event && x.to == to)
    }
}

fn esc(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

/// The chart as SCXML (W3C), one `<state>` per state with its transitions; a `*` edge is written on every state.
pub fn scxml(c: &Chart) -> String {
    let mut out = format!("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<!-- oracle-core {}: {} (generated by lifecycle::scxml; tests/lifecycle.rs checks the code against it) -->\n<scxml xmlns=\"http://www.w3.org/2005/07/scxml\" version=\"1.0\" name=\"{}\" initial=\"{}\">\n",
        c.id, esc(c.about), c.id, c.initial);
    for s in c.states {
        let edges: Vec<&Edge> = c.edges.iter().filter(|x| x.from == *s || x.from == "*").filter(|x| x.to != *s).collect();
        if edges.is_empty() {
            out += &format!("  <state id=\"{s}\"/>\n");
            continue;
        }
        out += &format!("  <state id=\"{s}\">\n");
        for x in edges {
            let note = if x.when.is_empty() { String::new() } else { format!(" <!-- {} -->", esc(x.when)) };
            out += &format!("    <transition event=\"{}\" target=\"{}\"/>{note}\n", x.event, x.to);
        }
        out += "  </state>\n";
    }
    out + "</scxml>\n"
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_edge_joins_states_the_chart_has_and_no_edge_is_written_twice() {
        for c in CHARTS {
            assert!(c.states.contains(&c.initial), "{}", c.id);
            for (i, x) in c.edges.iter().enumerate() {
                assert!(x.from == "*" || c.states.contains(&x.from), "{} {x:?}", c.id);
                assert!(c.states.contains(&x.to), "{} {x:?}", c.id);
                assert!(!c.edges[..i].iter().any(|y| y.from == x.from && y.event == x.event && y.to == x.to), "{} twice: {x:?}", c.id);
            }
        }
    }

    #[test]
    fn scxml_names_every_state_and_starts_at_the_initial_one() {
        let x = scxml(&OBLIGATION);
        assert!(x.contains("name=\"obligation\" initial=\"open\""), "{x}");
        for s in OBLIGATION.states {
            assert!(x.contains(&format!("<state id=\"{s}\"")), "{s}");
        }
        assert!(x.contains("<transition event=\"verdict.approve.holder\" target=\"done\"/> <!-- review; on the current revision -->"), "{x}");
        assert!(x.contains("<state id=\"cancelled\">\n    <transition event=\"close.asker\" target=\"done\"/>"), "a * edge is written on every state:\n{x}");
    }
}
