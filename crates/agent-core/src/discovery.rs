use serde::{Deserialize, Serialize};
use std::{
    env,
    path::{Path, PathBuf},
};

struct Detector {
    id: &'static str,
    name: &'static str,
    cli: &'static str,
    bundle: &'static str,
    history: &'static str,
}
const AGENTS: &[Detector] = &[
    Detector {
        id: "codex",
        name: "OpenAI Codex",
        cli: "codex",
        bundle: "Codex",
        history: ".codex/sessions",
    },
    Detector {
        id: "claude",
        name: "Claude Code",
        cli: "claude",
        bundle: "",
        history: ".claude/projects",
    },
    Detector {
        id: "antigravity",
        name: "Antigravity",
        cli: "agy",
        bundle: "Antigravity",
        history: ".gemini/antigravity",
    },
    Detector {
        id: "deepseek",
        name: "DeepSeek",
        cli: "",
        bundle: "DeepSeek",
        history: "",
    },
    Detector {
        id: "qwen",
        name: "Qwen Code",
        cli: "qwen",
        bundle: "",
        history: "",
    },
    Detector {
        id: "kimi",
        name: "Kimi CLI",
        cli: "kimi",
        bundle: "",
        history: "",
    },
    Detector {
        id: "trae",
        name: "Trae",
        cli: "",
        bundle: "Trae",
        history: "",
    },
    Detector {
        id: "cursor",
        name: "Cursor",
        cli: "cursor",
        bundle: "Cursor",
        history: "",
    },
    Detector {
        id: "gemini",
        name: "Gemini CLI",
        cli: "gemini",
        bundle: "",
        history: "",
    },
    Detector {
        id: "opencode",
        name: "OpenCode",
        cli: "opencode",
        bundle: "OpenCode",
        history: "",
    },
    Detector {
        id: "aider",
        name: "Aider",
        cli: "aider",
        bundle: "",
        history: "",
    },
];

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Agent {
    pub id: String,
    pub name: String,
    pub status: String,
    pub binary: Option<PathBuf>,
}

pub fn log_root(id: &str, home: &Path) -> Option<PathBuf> {
    match id {
        "codex" => Some(
            env::var_os("CODEX_HOME")
                .map(PathBuf::from)
                .unwrap_or_else(|| home.join(".codex"))
                .join("sessions"),
        ),
        "claude" => Some(
            env::var_os("CLAUDE_CONFIG_DIR")
                .map(PathBuf::from)
                .unwrap_or_else(|| home.join(".claude"))
                .join("projects"),
        ),
        _ => None,
    }
}

pub fn scan(home: &Path) -> Vec<Agent> {
    let mut bins: Vec<_> = env::split_paths(&env::var_os("PATH").unwrap_or_default())
        .filter(|p| p.is_absolute())
        .collect();
    bins.extend([
        home.join(".local/bin"),
        home.join(".bun/bin"),
        home.join(".volta/bin"),
        home.join(".npm-global/bin"),
        home.join(".cargo/bin"),
        home.join(".asdf/shims"),
        home.join(".local/share/mise/shims"),
        home.join(".mise/shims"),
        "/opt/homebrew/bin".into(),
        "/usr/local/bin".into(),
    ]);
    if let Some(bin) = env::var_os("NVM_BIN") {
        bins.push(bin.into())
    }
    // Finder-launched apps do not inherit the interactive shell's nvm PATH.
    let nvm = env::var_os("NVM_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join(".nvm"));
    if let Ok(versions) = nvm.join("versions/node").read_dir() {
        let mut installed: Vec<_> = versions
            .flatten()
            .map(|entry| entry.path().join("bin"))
            .collect();
        installed.sort();
        bins.extend(installed.into_iter().rev());
    }
    bins.extend([
        "/Applications/Codex.app/Contents/Resources".into(),
        home.join("Applications/Codex.app/Contents/Resources"),
        "/Applications/ChatGPT.app/Contents/Resources/codex-cli/CodexCLI.app/Contents/MacOS".into(),
        home.join("Applications/ChatGPT.app/Contents/Resources/codex-cli/CodexCLI.app/Contents/MacOS"),
    ]);
    AGENTS
        .iter()
        .map(|rule| {
            let binary = if rule.cli.is_empty() {
                None
            } else {
                bins.iter()
                    .map(|p| p.join(rule.cli))
                    .find(|p| executable(p))
            };
            let app = format!("{}.app", rule.bundle);
            let installed = binary.is_some()
                || (!rule.bundle.is_empty()
                    && [PathBuf::from("/Applications"), home.join("Applications")]
                        .iter()
                        .any(|p| p.join(&app).is_dir()));
            let history = log_root(rule.id, home).is_some_and(|p| p.is_dir())
                || (!rule.history.is_empty() && home.join(rule.history).is_dir());
            let status = if installed {
                "installed"
            } else if history {
                "data"
            } else {
                "missing"
            };
            Agent {
                id: rule.id.into(),
                name: rule.name.into(),
                status: status.into(),
                binary,
            }
        })
        .collect()
}

fn executable(path: &Path) -> bool {
    let Ok(meta) = path.metadata() else {
        return false;
    };
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if meta.permissions().mode() & 0o111 == 0 {
            return false;
        }
    }
    meta.is_file()
}
