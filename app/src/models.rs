use crate::{gh::ChildEnv, model::AgentKind};
use anyhow::{Context, Result, bail};
use serde::Deserialize;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ModelOption {
    pub value: String,
    pub label: String,
}

#[derive(Deserialize)]
struct ModelList<T> {
    models: Vec<T>,
}

#[derive(Deserialize)]
struct CodexModel {
    slug: String,
    display_name: String,
    visibility: String,
    priority: i64,
}

#[derive(Deserialize)]
struct OmpModel {
    kind: String,
    selector: String,
    name: String,
    provider: String,
}

fn parse_codex(text: &str) -> Result<Vec<ModelOption>> {
    let mut models: Vec<CodexModel> = serde_json::from_str::<ModelList<CodexModel>>(text)?.models;
    models.retain(|model| model.visibility == "list");
    models.sort_by_key(|model| model.priority);
    Ok(models
        .into_iter()
        .map(|model| ModelOption {
            value: model.slug,
            label: model.display_name,
        })
        .collect())
}

fn parse_claude(text: &str) -> Result<Vec<ModelOption>> {
    let mut lines = text.lines();
    let line = lines
        .find(|line| line.trim_start().starts_with("--model <model>"))
        .context("missing --model option")?;
    let mut option = line.to_owned();
    for line in lines {
        if line.trim_start().starts_with('-') {
            break;
        }
        option.push(' ');
        option.push_str(line.trim());
    }
    let mut parts = option.split('\'');
    let mut models = Vec::new();
    parts.next();
    while let Some(alias) = parts.next() {
        if parts.next().is_none() {
            break;
        }
        if !alias.is_empty() && !alias.chars().any(char::is_whitespace) {
            models.push(ModelOption {
                value: alias.to_owned(),
                label: alias.to_owned(),
            });
        }
    }
    if models.is_empty() {
        bail!("no model aliases in --model option");
    }
    Ok(models)
}

fn parse_pi(text: &str) -> Vec<ModelOption> {
    let mut rows = text
        .lines()
        .skip_while(|line| !line.trim_start().starts_with("provider"));
    rows.next();
    rows.filter_map(|line| {
        let line = line.trim();
        if line.is_empty() || line.starts_with("Warning") {
            return None;
        }
        let columns: Vec<&str> = line
            .split("  ")
            .map(str::trim)
            .filter(|part| !part.is_empty())
            .collect();
        if columns.len() < 2 {
            return None;
        }
        let value = format!("{}/{}", columns[0], columns[1]);
        Some(ModelOption {
            label: value.clone(),
            value,
        })
    })
    .collect()
}

fn parse_omp(text: &str) -> Result<Vec<ModelOption>> {
    let models = serde_json::from_str::<ModelList<OmpModel>>(text)?.models;
    Ok(models
        .into_iter()
        .filter(|model| model.kind == "chat")
        .map(|model| ModelOption {
            value: model.selector,
            label: format!("{} ({})", model.name, model.provider),
        })
        .collect())
}

pub fn list_models(kind: AgentKind, env: &ChildEnv) -> Result<Vec<ModelOption>> {
    let args: &[&str] = match kind {
        AgentKind::Codex => &["debug", "models"],
        AgentKind::Claude => &["--help"],
        AgentKind::Pi => &["--list-models"],
        AgentKind::Omp => &["models", "--json"],
    };
    let output = env
        .command(kind.arg())
        .args(args)
        .output()
        .with_context(|| format!("start {}", kind.arg()))?;
    if !output.status.success() {
        bail!(
            "{} exited {}: {}",
            kind.arg(),
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    let text = std::str::from_utf8(&output.stdout).context("model output is not UTF-8")?;
    match kind {
        AgentKind::Codex => parse_codex(text),
        AgentKind::Claude => parse_claude(text),
        AgentKind::Pi => Ok(parse_pi(text)),
        AgentKind::Omp => parse_omp(text),
    }
    .with_context(|| format!("parse {} models", kind.arg()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codex_lists_visible_models_in_priority_order() {
        let models = parse_codex(include_str!("../tests/fixtures/codex-models.json")).unwrap();
        let values: Vec<_> = models.iter().map(|model| model.value.as_str()).collect();
        assert_eq!(
            values,
            [
                "gpt-6.2-sol",
                "gpt-6.2-astra",
                "gpt-6.1-sol",
                "gpt-6.1-terra",
                "gpt-6-astra",
                "gpt-6-sol",
                "gpt-6-luna",
                "gpt-5.6-sol",
                "gpt-5.6-terra",
                "gpt-5.6-luna"
            ]
        );
    }
    #[test]
    fn claude_lists_aliases_from_help() {
        let models = parse_claude(include_str!("../tests/fixtures/claude-help.txt")).unwrap();
        assert_eq!(
            models.iter().map(|m| m.value.as_str()).collect::<Vec<_>>(),
            ["fable", "opus", "sonnet"]
        );
    }

    #[test]
    fn pi_lists_rows_or_empty_signed_out_output() {
        assert!(parse_pi(include_str!("../tests/fixtures/pi-models-empty.txt")).is_empty());
        let models = parse_pi(include_str!("../tests/fixtures/pi-models.txt"));
        let values: Vec<_> = models.iter().map(|m| m.value.as_str()).collect();
        assert_eq!(values.len(), 28);
        assert_eq!(values[0], "anthropic/claude-fable-5");
        assert!(values.contains(&"openai-codex/gpt-6.2-sol"));
    }

    #[test]
    fn omp_lists_chat_selectors() {
        let models = parse_omp(include_str!("../tests/fixtures/omp-models.json")).unwrap();
        let input: serde_json::Value =
            serde_json::from_str(include_str!("../tests/fixtures/omp-models.json")).unwrap();
        let expected = input["models"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|model| model["kind"] == "chat")
            .count();
        assert_eq!(models.len(), expected);
        assert!(
            models
                .iter()
                .any(|model| model.value == "anthropic/claude-fable-5"
                    && model.label == "Claude Fable 5 (anthropic)")
        );
        assert!(models.iter().all(|model| model.value.contains('/')));
        let mixed = r#"{"models":[{"kind":"chat","selector":"p/chat","name":"Chat","provider":"p"},{"kind":"embedding","selector":"p/embed","name":"Embed","provider":"p"}]}"#;
        assert_eq!(
            parse_omp(mixed).unwrap(),
            vec![ModelOption {
                value: "p/chat".into(),
                label: "Chat (p)".into(),
            }]
        );
    }
}
