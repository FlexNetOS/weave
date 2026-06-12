//! Seven-layer autonomous harness orchestration.
//!
//! This module deliberately wraps the checked-in Ralph loop script instead of
//! duplicating its prompts. The Rust CLI owns operator-facing discovery,
//! defaults, dry-run output, and process execution; the script remains the
//! durable iteration engine.

use anyhow::{Context, Result};
use serde::Serialize;
use std::path::{Path, PathBuf};
use std::process::Command;

const DEFAULT_MODEL: &str = "minimax-m3:cloud";
const DEFAULT_AGENT_CMD: &str = "ollama launch claude --model minimax-m3:cloud --";
const DEFAULT_KIMI_CMD: &str = "kimi-legacy";
const DEFAULT_KIMI_MODEL: &str = "kimi-code/kimi-for-coding";
const DEFAULT_KIMI_SESSION: &str = "3c6e42cf-090d-4553-a84b-e63fb9c511c1";
const DEFAULT_KIMI_SESSION_FLAG: &str = "-r";
const DEFAULT_KIMI_EXTRA_ARGS: &str = "--quiet";

#[derive(Clone, Debug)]
pub struct IdeMergeIde {
    pub worktree: PathBuf,
    pub budget: u32,
    pub max_iters: u32,
    pub sleep_secs: u64,
    pub execute: bool,
    pub apply: bool,
    pub no_kimi_plan: bool,
    pub no_kimi_review: bool,
    pub agent_cmd: String,
    pub model: String,
    pub agent_model_args: String,
    pub kimi_cmd: String,
    pub kimi_model: String,
    pub kimi_session: String,
    pub kimi_session_flag: String,
    pub kimi_extra_args: String,
    pub json: bool,
}

impl IdeMergeIde {
    pub fn with_defaults(worktree: Option<PathBuf>) -> Self {
        Self {
            worktree: worktree.unwrap_or_else(default_worktree),
            budget: 3,
            max_iters: 50,
            sleep_secs: 5,
            execute: false,
            apply: true,
            no_kimi_plan: false,
            no_kimi_review: false,
            agent_cmd: DEFAULT_AGENT_CMD.to_string(),
            model: DEFAULT_MODEL.to_string(),
            agent_model_args: String::new(),
            kimi_cmd: DEFAULT_KIMI_CMD.to_string(),
            kimi_model: DEFAULT_KIMI_MODEL.to_string(),
            kimi_session: DEFAULT_KIMI_SESSION.to_string(),
            kimi_session_flag: DEFAULT_KIMI_SESSION_FLAG.to_string(),
            kimi_extra_args: DEFAULT_KIMI_EXTRA_ARGS.to_string(),
            json: false,
        }
    }
}

#[derive(Serialize)]
struct HarnessPlan {
    name: &'static str,
    mode: &'static str,
    worktree: String,
    script: String,
    layers: Vec<&'static str>,
    env: Vec<(String, String)>,
    command: Vec<String>,
}

pub fn run_ide_merge_ide(opts: IdeMergeIde) -> Result<()> {
    let script = opts
        .worktree
        .join(".claude/skills/weave-loop/scripts/ralph-weave.sh");
    let plan = build_plan(&opts, &script);

    if !opts.execute {
        print_plan(&plan, opts.json)?;
        return Ok(());
    }

    if !opts.worktree.join("Cargo.toml").is_file() {
        anyhow::bail!(
            "worktree does not look like a weave checkout: {}",
            opts.worktree.display()
        );
    }
    if !script.is_file() {
        anyhow::bail!("harness script not found: {}", script.display());
    }

    let mut cmd = Command::new("bash");
    cmd.arg(&script).current_dir(&opts.worktree);
    for (k, v) in env_pairs(&opts) {
        cmd.env(k, v);
    }

    let status = cmd
        .status()
        .with_context(|| format!("spawning harness script {}", script.display()))?;
    if !status.success() {
        anyhow::bail!("harness exited with {status}");
    }
    Ok(())
}

fn build_plan(opts: &IdeMergeIde, script: &Path) -> HarnessPlan {
    HarnessPlan {
        name: "codex-7-layer ide-merge-ide",
        mode: if opts.execute { "execute" } else { "dry-run" },
        worktree: opts.worktree.to_string_lossy().into_owned(),
        script: script.to_string_lossy().into_owned(),
        layers: vec![
            "1 discover/resume durable state from _workspace",
            "2 Kimi Code preflight over backlog, handoff, and git status",
            "3 MiniMax implementation pass through Ollama-launched Claude",
            "4 IDE merge discipline: one cohesive item, scoped edits, committed evidence",
            "5 Kimi Code review of the MiniMax pass",
            "6 fresh-shell verification: fmt, clippy, tests, and feature smoke",
            "7 sentinel/handoff: DONE, NEEDS-HUMAN, or committed HANDOFF.md relay",
        ],
        env: env_pairs(opts),
        command: vec!["bash".to_string(), script.to_string_lossy().into_owned()],
    }
}

fn print_plan(plan: &HarnessPlan, json: bool) -> Result<()> {
    if json {
        println!("{}", serde_json::to_string_pretty(plan)?);
        return Ok(());
    }

    println!("codex-7-layer ide-merge-ide harness ({})", plan.mode);
    println!("  worktree: {}", plan.worktree);
    println!("  script:   {}", plan.script);
    println!("  command:  {}", plan.command.join(" "));
    println!("  layers:");
    for layer in &plan.layers {
        println!("    {layer}");
    }
    println!("  environment:");
    for (k, v) in &plan.env {
        println!("    {k}={v}");
    }
    println!("dry-run only; pass --execute to run the loop.");
    Ok(())
}

fn env_pairs(opts: &IdeMergeIde) -> Vec<(String, String)> {
    vec![
        (
            "WEAVE_WORKTREE".to_string(),
            opts.worktree.to_string_lossy().into_owned(),
        ),
        ("WEAVE_BUDGET".to_string(), opts.budget.to_string()),
        ("WEAVE_MAX_ITERS".to_string(), opts.max_iters.to_string()),
        ("WEAVE_SLEEP".to_string(), opts.sleep_secs.to_string()),
        ("WEAVE_MODEL".to_string(), opts.model.clone()),
        ("WEAVE_AGENT_CMD".to_string(), opts.agent_cmd.clone()),
        (
            "WEAVE_AGENT_MODEL_ARGS".to_string(),
            opts.agent_model_args.clone(),
        ),
        (
            "WEAVE_APPLY".to_string(),
            if opts.apply { "1" } else { "0" }.to_string(),
        ),
        (
            "WEAVE_KIMI_PLAN".to_string(),
            if opts.no_kimi_plan { "0" } else { "1" }.to_string(),
        ),
        (
            "WEAVE_KIMI_REVIEW".to_string(),
            if opts.no_kimi_review { "0" } else { "1" }.to_string(),
        ),
        ("WEAVE_KIMI_CMD".to_string(), opts.kimi_cmd.clone()),
        ("WEAVE_KIMI_MODEL".to_string(), opts.kimi_model.clone()),
        ("WEAVE_KIMI_SESSION".to_string(), opts.kimi_session.clone()),
        (
            "WEAVE_KIMI_SESSION_FLAG".to_string(),
            opts.kimi_session_flag.clone(),
        ),
        (
            "WEAVE_KIMI_EXTRA_ARGS".to_string(),
            opts.kimi_extra_args.clone(),
        ),
    ]
}

fn default_worktree() -> PathBuf {
    std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dry_run_plan_has_seven_layers_and_expected_defaults() {
        let opts = IdeMergeIde::with_defaults(Some(PathBuf::from("/tmp/weave")));
        let script = opts
            .worktree
            .join(".claude/skills/weave-loop/scripts/ralph-weave.sh");
        let plan = build_plan(&opts, &script);

        assert_eq!(plan.layers.len(), 7);
        assert_eq!(plan.mode, "dry-run");
        assert!(plan
            .env
            .iter()
            .any(|(k, v)| k == "WEAVE_AGENT_CMD" && v.contains("ollama launch claude")));
        assert!(plan
            .env
            .iter()
            .any(|(k, v)| k == "WEAVE_KIMI_CMD" && v == DEFAULT_KIMI_CMD));
        assert!(plan.env.iter().any(|(k, v)| k == "WEAVE_APPLY" && v == "1"));
    }
}
