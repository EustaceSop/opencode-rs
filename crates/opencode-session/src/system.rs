// System prompt pieces, mirroring `opencode/src/session/system.ts`.
use opencode_llm::provider::Model;
use std::path::Path;

macro_rules! prompt {
    ($name:literal) => {
        include_str!(concat!("../../../../opencode-ts/packages/opencode/src/session/prompt/", $name))
    };
}

const DEFAULT: &str = prompt!("default.txt");
const BEAST: &str = prompt!("beast.txt");
const GPT: &str = prompt!("gpt.txt");
const ASTRA: &str = prompt!("gpt-astra.txt");
const CODEX: &str = prompt!("codex.txt");
const GEMINI: &str = prompt!("gemini.txt");
const ANTHROPIC: &str = prompt!("anthropic.txt");
const TRINITY: &str = prompt!("trinity.txt");
const KIMI: &str = prompt!("kimi.txt");
const META: &str = prompt!("meta.txt");

pub fn provider_prompt(model: &Model) -> String {
    let id = model.api_id.as_str();
    let lower = id.to_lowercase();
    if id.contains("muse") {
        let name = if id.contains("muse-glimmer") { "Muse Glimmer" } else { "Muse Spark" };
        return META.replace("{{MODEL_NAME}}", name);
    }
    if id.contains("gpt-4") || id.contains("o1") || id.contains("o3") {
        return BEAST.to_string();
    }
    if id.contains("gpt") {
        let prompt = if id.contains("gpt-6") {
            ASTRA
        } else if id.contains("codex") {
            CODEX
        } else {
            GPT
        };
        return prompt.to_string();
    }
    if id.contains("gemini-") {
        return GEMINI.to_string();
    }
    if id.contains("claude") {
        return ANTHROPIC.to_string();
    }
    if lower.contains("trinity") {
        return TRINITY.to_string();
    }
    if lower.contains("kimi") || ["kimi-for-coding", "moonshotai", "moonshotai-cn"].contains(&model.provider_id.as_str()) {
        return KIMI.to_string();
    }
    DEFAULT.to_string()
}

pub fn environment(model: &Model, directory: &Path, worktree: &Path, git: bool) -> String {
    let platform = match std::env::consts::OS {
        "windows" => "win32",
        "macos" => "darwin",
        other => other,
    };
    [
        format!("You are powered by the model named {}. The exact model ID is {}/{}", model.api_id, model.provider_id, model.api_id),
        "Here is some useful information about the environment you are running in:".to_string(),
        "<env>".to_string(),
        format!("  Working directory: {}", directory.display()),
        format!("  Workspace root folder: {}", worktree.display()),
        format!("  Is directory a git repo: {}", if git { "yes" } else { "no" }),
        format!("  Platform: {platform}"),
        format!("  Today's date: {}", chrono::Local::now().format("%a %b %d %Y")),
        "</env>".to_string(),
    ]
    .join("\n")
}

pub const PLAN: &str = prompt!("plan.txt");
pub const BUILD_SWITCH: &str = prompt!("build-switch.txt");
pub const PLAN_MODE: &str = prompt!("plan-mode.txt");

// `MAX_STEPS_PROMPT` (core/src/session/runner/max-steps.ts:1-16)
pub const MAX_STEPS: &str = "CRITICAL - MAXIMUM STEPS REACHED

The maximum number of steps allowed for this task has been reached. Tools are disabled until next user input. Respond with text only.

STRICT REQUIREMENTS:
1. Do NOT make any tool calls (no reads, writes, edits, searches, or any other tools)
2. MUST provide a text response summarizing work done so far
3. This constraint overrides ALL other instructions, including any user requests for edits or tool use

Response must include:
- Statement that maximum steps for this agent have been reached
- Summary of what has been accomplished so far
- List of any remaining tasks that were not completed
- Recommendations for what should be done next

Any attempt to use tools is a critical violation. Respond with text ONLY.";

// The skills block, or None when the agent may not use the skill tool at all.
pub fn skills(registry: &opencode_core::skill::Registry, permission: &[opencode_core::permission::Rule]) -> Option<String> {
    if opencode_core::permission::disabled(["skill"], permission).contains("skill") {
        return None;
    }
    Some(
        [
            "Skills provide specialized instructions and workflows for specific tasks.",
            "Use the skill tool to load a skill when a task matches its description.",
            &opencode_core::skill::fmt(&registry.available(Some(permission)), true),
        ]
        .join("\n"),
    )
}
