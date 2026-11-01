//! Offline documentation and parser-derived coverage: no seat, keys or transport.
use anyhow::{bail, Result};
use clap::CommandFactory;
use std::path::Path;

const REFERENCE: &str = include_str!("../../../docs/ox.md");
const WORKFLOW: &str = include_str!("../../../docs/ox-workflow.md");

#[derive(Clone, Copy, clap::ValueEnum)]
pub enum Topic { Reference, Workflow, Commands }

fn paths(command: &clap::Command, prefix: &str, out: &mut Vec<String>) {
    for child in command.get_subcommands().filter(|c| !c.is_hide_set()) {
        let path = format!("{prefix} {}", child.get_name());
        out.push(path.clone());
        paths(child, &path, out);
    }
}

fn commands() -> Vec<String> {
    let mut out = Vec::new();
    paths(&super::Cli::command(), "ox", &mut out);
    out
}

// Only command cells count, not incidental mentions in prose or descriptions.
fn missing(reference: &str, commands: &[String]) -> Vec<String> {
    let invocations: Vec<&str> = reference.lines().filter_map(|line| {
        line.trim().strip_prefix('|')?.split('|').next()
    }).flat_map(|cell| cell.split('`').enumerate()
        .filter_map(|(i, text)| (i % 2 == 1).then_some(text))).collect();
    commands.iter().filter(|command| !invocations.iter().any(|invocation| {
        *invocation == command.as_str() || invocation.strip_prefix(command.as_str())
            .is_some_and(|rest| rest.starts_with(" <") || rest.starts_with(" ["))
    })).cloned().collect()
}

pub fn run(topic: Topic, check: Option<&Path>, json: bool) -> Result<()> {
    if let Some(path) = check {
        let absent = missing(&std::fs::read_to_string(path)?, &commands());
        if !absent.is_empty() { bail!("missing manual command rows: {}", absent.join(", ")); }
        println!("manual command coverage: ok");
        return Ok(());
    }
    let (name, text) = match topic {
        Topic::Reference => ("reference", REFERENCE.to_owned()),
        Topic::Workflow => ("workflow", WORKFLOW.to_owned()),
        Topic::Commands => ("commands", commands().join("\n") + "\n"),
    };
    if json { println!("{}", serde_json::json!({"topic": name, "text": text})); }
    else { print!("{text}"); }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn every_visible_command_has_a_manual_row() {
        std::thread::Builder::new().stack_size(32 << 20).spawn(coverage_body).unwrap().join().unwrap();
    }
    fn coverage_body() {
        assert_eq!(missing(REFERENCE, &commands()), Vec::<String>::new());
    }
    #[test]
    fn removed_row_and_new_command_fail_coverage() {
        std::thread::Builder::new().stack_size(32 << 20).spawn(negative_body).unwrap().join().unwrap();
    }
    fn negative_body() {
        let damaged = REFERENCE.lines().filter(|l| !l.starts_with("| `ox doctor`"))
            .collect::<Vec<_>>().join("\n");
        assert!(missing(&damaged, &commands()).contains(&"ox doctor".to_owned()));
        assert_eq!(missing(REFERENCE, &["ox future-command".into()]), vec!["ox future-command"]);
        assert_eq!(missing("ox doctor\n| `ox doctor-extra` | mentions `ox doctor` |", &["ox doctor".into()]), vec!["ox doctor"]);
    }
}
