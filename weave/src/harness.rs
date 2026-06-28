//! Seven-layer autonomous harness orchestration.
//!
//! This module deliberately wraps the checked-in Ralph loop script instead of
//! duplicating its prompts. The Rust CLI owns operator-facing discovery,
//! defaults, dry-run output, and process execution; the script remains the
//! durable iteration engine.

use anyhow::{Context, Result};
use serde::Serialize;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

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
            "1 discover/resume durable state from .handoff/loop",
            "2 Kimi Code preflight over backlog, handoff, and git status",
            "3 MiniMax implementation pass through Ollama-launched Claude",
            "4 IDE merge discipline: one cohesive item, scoped edits, committed evidence",
            "5 Kimi Code review of the MiniMax pass",
            "6 fresh-shell verification: fmt, clippy, tests, and feature smoke",
            "7 sentinel/handoff: DONE, NEEDS-HUMAN, or committed .handoff/packets/latest.md relay",
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


#[derive(Clone, Debug)]
pub struct ForgeLoop {
    pub worktree: PathBuf,
    pub task: String,
    pub budget: u32,
    pub max_iters: u32,
    pub sleep_secs: u64,
    pub execute: bool,
    pub apply: bool,
    pub codex_cmd: String,
    pub codex_model: String,
    pub json: bool,
}

impl ForgeLoop {
    pub fn with_defaults(worktree: Option<PathBuf>) -> Self {
        Self {
            worktree: worktree.unwrap_or_else(default_worktree),
            task: "continue the top forge-loop task".to_string(),
            budget: 1,
            max_iters: 1,
            sleep_secs: 5,
            execute: false,
            apply: true,
            codex_cmd: "codex".to_string(),
            codex_model: "gpt-5.5".to_string(),
            json: false,
        }
    }
}

#[derive(Serialize)]
struct ForgeLoopPlan {
    name: &'static str,
    mode: &'static str,
    worktree: String,
    task: String,
    skill: &'static str,
    layers: Vec<&'static str>,
    env: Vec<(String, String)>,
    command: Vec<String>,
}

pub fn run_forge_loop(opts: ForgeLoop) -> Result<()> {
    let plan = build_forge_plan(&opts);
    if !opts.execute {
        print_forge_plan(&plan, opts.json)?;
        return Ok(());
    }

    if !opts.worktree.join("Cargo.toml").is_file() {
        anyhow::bail!(
            "worktree does not look like a weave checkout: {}",
            opts.worktree.display()
        );
    }

    let mut cmd = Command::new(&opts.codex_cmd);
    cmd.arg("exec")
        .arg("--model")
        .arg(&opts.codex_model)
        .arg(forge_prompt(&opts))
        .current_dir(&opts.worktree)
        .stdin(Stdio::null());
    for (k, v) in forge_env_pairs(&opts) {
        cmd.env(k, v);
    }

    let status = cmd
        .status()
        .with_context(|| format!("spawning Codex forge-loop command {}", opts.codex_cmd))?;
    if !status.success() {
        anyhow::bail!("forge-loop exited with {status}");
    }
    Ok(())
}

fn build_forge_plan(opts: &ForgeLoop) -> ForgeLoopPlan {
    ForgeLoopPlan {
        name: "codex-forge-loop",
        mode: if opts.execute { "execute" } else { "dry-run" },
        worktree: opts.worktree.to_string_lossy().into_owned(),
        task: opts.task.clone(),
        skill: ".agents/skills/forge-loop/SKILL.md",
        layers: vec![
            "1 recover durable state: git, .handoff, ICM, and Codex session context",
            "2 select one cohesive task and write/refresh the execution note",
            "3 use Codex subagents for read-heavy exploration and review only",
            "4 implement through the Rust-native weave CLI/workspace path",
            "5 verify with fmt, clippy, tests, and feature/backend gates in fresh shells",
            "6 deliver immediately: commit, push, PR, and arm auto-merge",
            "7 persist handoff/memory and halt on DONE or NEEDS-HUMAN",
        ],
        env: forge_env_pairs(opts),
        command: vec![
            opts.codex_cmd.clone(),
            "exec".to_string(),
            "--model".to_string(),
            opts.codex_model.clone(),
            "<forge-loop-prompt>".to_string(),
        ],
    }
}

fn print_forge_plan(plan: &ForgeLoopPlan, json: bool) -> Result<()> {
    if json {
        println!("{}", serde_json::to_string_pretty(plan)?);
        return Ok(());
    }

    println!("{} harness ({})", plan.name, plan.mode);
    println!("  worktree: {}", plan.worktree);
    println!("  skill:    {}", plan.skill);
    println!("  task:     {}", plan.task);
    println!("  command:  {}", plan.command.join(" "));
    println!("  layers:");
    for layer in &plan.layers {
        println!("    {layer}");
    }
    println!("  environment:");
    for (k, v) in &plan.env {
        println!("    {k}={v}");
    }
    println!("dry-run only; pass --execute to run codex exec.");
    Ok(())
}

fn forge_env_pairs(opts: &ForgeLoop) -> Vec<(String, String)> {
    vec![
        (
            "WEAVE_FORGE_WORKTREE".to_string(),
            opts.worktree.to_string_lossy().into_owned(),
        ),
        ("WEAVE_FORGE_TASK".to_string(), opts.task.clone()),
        ("WEAVE_FORGE_BUDGET".to_string(), opts.budget.to_string()),
        (
            "WEAVE_FORGE_MAX_ITERS".to_string(),
            opts.max_iters.to_string(),
        ),
        (
            "WEAVE_FORGE_SLEEP".to_string(),
            opts.sleep_secs.to_string(),
        ),
        (
            "WEAVE_FORGE_APPLY".to_string(),
            if opts.apply { "1" } else { "0" }.to_string(),
        ),
    ]
}

fn forge_prompt(opts: &ForgeLoop) -> String {
    format!(
        "Use the repo-local forge-loop skill at .agents/skills/forge-loop/SKILL.md. \
Run exactly one cohesive forge-loop cycle for this task: {task}. \
Budget={budget}; max_iters={max_iters}; sleep_secs={sleep}; apply={apply}. \
Follow the repository rules: Rust-native only, fresh verification, commit, push, PR, and auto-merge for completed chunks.",
        task = opts.task,
        budget = opts.budget,
        max_iters = opts.max_iters,
        sleep = opts.sleep_secs,
        apply = if opts.apply { "true" } else { "false" },
    )
}

#[derive(Clone, Debug)]
pub struct CodexTools {
    pub home: PathBuf,
    pub weave_exe: String,
    pub codex_cmd: String,
    pub json: bool,
    pub dry_run: bool,
    pub force: bool,
}

impl CodexTools {
    pub fn with_defaults(home: Option<PathBuf>) -> Self {
        Self {
            home: home.unwrap_or_else(default_codex_home),
            weave_exe: "weave".to_string(),
            codex_cmd: "codex".to_string(),
            json: false,
            dry_run: false,
            force: false,
        }
    }
}

#[derive(Serialize)]
struct CodexDoctorReport {
    codex_home: String,
    codex_cli: String,
    codex_cli_ok: bool,
    repo_config_ok: bool,
    repo_agents_ok: bool,
    forge_skill_ok: bool,
    prompt_shim_ok: bool,
    install_command: Vec<String>,
}

pub fn run_codex_doctor(opts: CodexTools) -> Result<()> {
    let report = codex_report(&opts);
    if opts.json {
        println!("{}", serde_json::to_string_pretty(&report)?);
        return Ok(());
    }
    println!("codex tools doctor");
    println!("  codex_home:      {}", report.codex_home);
    println!("  codex_cli:       {}", status(report.codex_cli_ok));
    println!("  repo_config:     {}", status(report.repo_config_ok));
    println!("  repo_agents:     {}", status(report.repo_agents_ok));
    println!("  forge_skill:     {}", status(report.forge_skill_ok));
    println!("  /forge-loop shim:{}", status(report.prompt_shim_ok));
    println!("  install:         {}", report.install_command.join(" "));
    Ok(())
}

pub fn run_codex_install(opts: CodexTools) -> Result<()> {
    let prompt_path = forge_prompt_path(&opts.home);
    let content = forge_prompt_shim(&opts.weave_exe);
    if opts.dry_run {
        println!("would write {}", prompt_path.display());
        return Ok(());
    }
    if let Ok(existing) = fs::read_to_string(&prompt_path) {
        if existing != content && !existing.contains("weave-managed: forge-loop") && !opts.force {
            anyhow::bail!(
                "refusing to overwrite non-weave prompt shim {}; rerun with --force if intentional",
                prompt_path.display()
            );
        }
    }
    let parent = prompt_path
        .parent()
        .context("forge-loop prompt path has no parent")?;
    fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
    fs::write(&prompt_path, content)
        .with_context(|| format!("writing {}", prompt_path.display()))?;
    println!("installed Codex /forge-loop shim: {}", prompt_path.display());
    Ok(())
}

fn codex_report(opts: &CodexTools) -> CodexDoctorReport {
    let codex_cli_ok = Command::new(&opts.codex_cmd)
        .arg("--version")
        .stdin(Stdio::null())
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    CodexDoctorReport {
        codex_home: opts.home.to_string_lossy().into_owned(),
        codex_cli: opts.codex_cmd.clone(),
        codex_cli_ok,
        repo_config_ok: Path::new(".codex/config.toml").is_file(),
        repo_agents_ok: Path::new(".codex/agents/explorer.toml").is_file()
            && Path::new(".codex/agents/reviewer.toml").is_file()
            && Path::new(".codex/agents/docs-researcher.toml").is_file(),
        forge_skill_ok: Path::new(".agents/skills/forge-loop/SKILL.md").is_file(),
        prompt_shim_ok: forge_prompt_path(&opts.home).is_file(),
        install_command: vec![
            "weave".to_string(),
            "codex-tools".to_string(),
            "install".to_string(),
            "--home".to_string(),
            opts.home.to_string_lossy().into_owned(),
        ],
    }
}

fn status(ok: bool) -> &'static str {
    if ok { "ok" } else { "missing" }
}

fn default_codex_home() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".codex")
}

fn forge_prompt_path(home: &Path) -> PathBuf {
    home.join("prompts").join("forge-loop.md")
}

fn forge_prompt_shim(weave_exe: &str) -> String {
    format!(
        r#"---
description: Run the Rust-native Weave forge loop
argument-hint: [TASK="..."]
---

<!-- weave-managed: forge-loop -->
Invoke the Rust-native forge loop for this repository.

Run:

```bash
{weave_exe} harness forge-loop --task "$ARGUMENTS"
```

If execution is requested, use:

```bash
{weave_exe} harness forge-loop --execute --task "$ARGUMENTS"
```

Use the repo-local skill `.agents/skills/forge-loop/SKILL.md` as the durable workflow source of truth.
"#
    )
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

    #[test]
    fn forge_loop_plan_is_codex_native_and_dry_run_by_default() {
        let mut opts = ForgeLoop::with_defaults(Some(PathBuf::from("/tmp/weave")));
        opts.task = "fix the next task".to_string();
        let plan = build_forge_plan(&opts);

        assert_eq!(plan.name, "codex-forge-loop");
        assert_eq!(plan.mode, "dry-run");
        assert_eq!(plan.layers.len(), 7);
        assert!(plan.command.iter().any(|p| p == "codex"));
        assert!(plan.env.iter().any(|(k, v)| k == "WEAVE_FORGE_APPLY" && v == "1"));
    }

    #[test]
    fn codex_install_prompt_shim_is_weave_managed() {
        let shim = forge_prompt_shim("/usr/bin/weave");
        assert!(shim.contains("weave-managed: forge-loop"));
        assert!(shim.contains("/usr/bin/weave harness forge-loop"));
    }
}
