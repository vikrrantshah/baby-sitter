use crate::{
    decide::{Batch, Item},
    domain::*,
    proc,
};
use anyhow::{Context, bail};
use serde::Deserialize;
use std::{collections::BTreeMap, fs, path::Path, process::Command, time::Instant};
pub struct Resolution {
    pub item: Item,
    pub outcome: Outcome,
    pub reply: String,
}
#[derive(Deserialize)]
struct Output {
    results: Vec<ResultItem>,
}
#[derive(Deserialize)]
struct ResultItem {
    id: String,
    outcome: String,
    reply: String,
}
pub fn run(
    root: &Path,
    state_dir: &Path,
    batch: &Batch,
    deadline: Instant,
) -> anyhow::Result<Vec<Resolution>> {
    let id = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_nanos();
    let dir = state_dir.join(format!("run-{id}"));
    fs::create_dir_all(&dir)?;
    let schema = dir.join("schema.json");
    let output = dir.join("out.json");
    let ids: Vec<_> = batch.0.iter().map(|i| i.id.to_string()).collect();
    let value = serde_json::json!({"type":"object","additionalProperties":false,"properties":{"results":{"type":"array","items":{"type":"object","additionalProperties":false,"properties":{"id":{"type":"string","enum":ids},"outcome":{"type":"string","enum":["fixed","declined","needs_human"]},"reply":{"type":"string"}},"required":["id","outcome","reply"]}}},"required":["results"]});
    fs::write(&schema, serde_json::to_vec(&value)?)?;
    let prompt = format!(
        "Edit files only. Do not run git or gh. Return exactly one result per item id. Use fixed only when the code addresses the feedback. Reply text is posted verbatim as the viewer.\n\n{}",
        batch
            .0
            .iter()
            .map(|i| format!("ITEM {}\n{}", i.id, i.context))
            .collect::<Vec<_>>()
            .join("\n\n")
    );
    proc::run(
        Command::new("codex")
            .args(["exec", "--sandbox", "workspace-write", "--ephemeral", "-C"])
            .arg(root)
            .arg("--output-schema")
            .arg(&schema)
            .arg("-o")
            .arg(&output)
            .arg("-"),
        Some(&prompt),
        deadline,
    )?;
    let result: Output =
        serde_json::from_slice(&fs::read(&output).context("reading codex output")?)
            .context("parsing codex output")?;
    let mut results = BTreeMap::new();
    for r in result.results {
        let id: ItemId = r.id.parse()?;
        if r.reply.trim().is_empty() {
            bail!("empty reply for {id}");
        }
        let outcome = r.outcome.parse()?;
        if results.insert(id.clone(), (outcome, r.reply)).is_some() {
            bail!("duplicate codex result {id}");
        }
    }
    let mut resolved = Vec::new();
    for item in &batch.0 {
        let (outcome, reply) = results
            .remove(&item.id)
            .with_context(|| format!("missing codex result {}", item.id))?;
        resolved.push(Resolution {
            item: item.clone(),
            outcome,
            reply,
        });
    }
    if !results.is_empty() {
        bail!("unexpected codex result ids: {:?}", results.keys());
    }
    Ok(resolved)
}
