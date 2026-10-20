//! Taking a node's finished work from an earlier bundle of the same change.
//! This module judges one node against its old record and moves its files;
//! which nodes upstream of it were also reused is the scheduler's to say.

use crate::graph::{Graph, Kind, Node, ProviderKind};
use crate::run::Target;
use serde::de::DeserializeOwned;
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

pub struct Previous {
    pub dir: PathBuf,
    records: HashMap<String, Value>,
    /// Why nothing in the bundle can be taken, when that is so.
    pub unusable: Option<String>,
    /// The run is finishing this very bundle: nothing is copied, a taken
    /// node keeps its cost, and the manifest carries the first start.
    pub in_place: bool,
    pub started_at: Option<String>,
    pub run_id: Option<String>,
    pub resumed_at: Vec<String>,
    pub wall_ms: u64,
    graph: String,
    /// The earlier run withheld GitHub from its nodes.
    no_gh: bool,
}

/// Copies a directory tree; symlinks are left behind.
pub fn copy_dir(from: &Path, to: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(to)?;
    for entry in std::fs::read_dir(from)? {
        let entry = entry?;
        let kind = entry.file_type()?;
        if kind.is_dir() {
            copy_dir(&entry.path(), &to.join(entry.file_name()))?;
        } else if kind.is_file() {
            std::fs::copy(entry.path(), to.join(entry.file_name()))?;
        }
    }
    Ok(())
}

fn short(sha: &str) -> &str {
    &sha[..8.min(sha.len())]
}

impl Previous {
    /// Reads the bundle's manifest. A bundle of another change opens fine and
    /// is `unusable`; one with no readable manifest is an error, since the
    /// caller named it on purpose.
    pub fn open(dir: &Path, t: &Target) -> Result<Previous, String> {
        let path = dir.join("manifest.json");
        let text =
            std::fs::read_to_string(&path).map_err(|e| format!("{}: {e}", path.display()))?;
        let manifest: Value =
            serde_json::from_str(&text).map_err(|e| format!("{}: {e}", path.display()))?;
        let records = manifest["nodes"]
            .as_array()
            .ok_or_else(|| format!("{}: no `nodes`", path.display()))?
            .iter()
            .filter_map(|r| Some((r["name"].as_str()?.to_string(), r.clone())))
            .collect();
        let sha = |key: &str| manifest[key].as_str().unwrap_or_default().to_string();
        let (base, head) = (sha("base_sha"), sha("head_sha"));
        let unusable = if base != t.base_sha || head != t.head_sha {
            Some(format!(
                "it reviewed {}..{}, not {}..{}",
                short(&base),
                short(&head),
                short(&t.base_sha),
                short(&t.head_sha)
            ))
        } else if std::fs::read_to_string(dir.join("diff.patch"))
            .ok()
            .as_deref()
            != Some(t.diff.as_str())
        {
            // The same head with a different working tree is a different change.
            Some("its diff.patch is not this run's diff".to_string())
        } else {
            None
        };
        Ok(Previous {
            dir: dir.to_path_buf(),
            records,
            unusable,
            in_place: false,
            started_at: manifest["started_at"].as_str().map(str::to_string),
            run_id: manifest["run_id"].as_str().map(str::to_string),
            resumed_at: manifest["resumed_at"]
                .as_array()
                .map(|a| {
                    a.iter()
                        .filter_map(|s| s.as_str().map(str::to_string))
                        .collect()
                })
                .unwrap_or_default(),
            wall_ms: manifest["wall_ms"].as_u64().unwrap_or(0),
            graph: manifest["graph"].as_str().unwrap_or_default().to_string(),
            no_gh: manifest["no_gh"].as_bool().unwrap_or(false),
        })
    }

    /// Opens a bundle to finish in place. Unlike reuse, a bundle of another
    /// change is an error: the run would write over it.
    pub fn resuming(dir: &Path, t: &Target) -> Result<Previous, String> {
        let mut p = Previous::open(dir, t)?;
        if let Some(why) = &p.unusable {
            return Err(format!("--resume {}: {why}", dir.display()));
        }
        p.in_place = true;
        Ok(p)
    }

    /// A bundle made with GitHub access must not seed a run without it, nor
    /// the reverse: its findings could rest on what the other run could not see.
    pub fn gh_access_matches(&self, no_gh: bool) -> Result<(), String> {
        if self.no_gh == no_gh {
            return Ok(());
        }
        let not = |on: bool| {
            if on {
                "ran with --no-gh"
            } else {
                "ran without --no-gh"
            }
        };
        Err(format!(
            "the earlier run {}, this one {}",
            not(self.no_gh),
            not(no_gh)
        ))
    }

    /// For a resume: the graph and every node's model and effort must be what
    /// the original run used, or the merged bundle would mix two experiments.
    pub fn settings_match(&self, graph: &Graph) -> Result<(), String> {
        if self.graph != graph.name {
            return Err(format!(
                "--resume: graph was `{}` in the original run, `{}` now",
                self.graph, graph.name
            ));
        }
        for node in &graph.nodes {
            let Some(r) = self.records.get(&node.name) else {
                continue;
            };
            for (field, was, now) in [
                ("model", r["model"].as_str(), node.model.as_deref()),
                ("effort", r["effort"].as_str(), node.effort.as_deref()),
            ] {
                if was != now {
                    return Err(format!(
                        "--resume: {field} of node `{}` was {} in the original run, {} now",
                        node.name,
                        was.map_or("unset".to_string(), |v| format!("`{v}`")),
                        now.map_or("unset".to_string(), |v| format!("`{v}`"))
                    ));
                }
            }
        }
        Ok(())
    }

    /// Whether the bundle recorded `name` as left out of its run.
    pub fn skipped(&self, name: &str) -> bool {
        self.records
            .get(name)
            .is_some_and(|r| r["status"] == "skipped")
    }

    pub fn record(&self, name: &str) -> Option<&Value> {
        self.records.get(name)
    }

    /// The bundle's records of nodes this run has not recorded yet, so a
    /// manifest written part way through a resume still has them.
    pub fn records_except(&self, have: &HashSet<&str>) -> Vec<Value> {
        let mut rest: Vec<&Value> = self
            .records
            .iter()
            .filter(|(name, _)| !have.contains(name.as_str()))
            .map(|(_, r)| r)
            .collect();
        rest.sort_by_key(|r| r["name"].as_str().unwrap_or_default().to_string());
        rest.into_iter().cloned().collect()
    }

    /// The bundle's record of `node` if the node itself is unchanged: it
    /// finished ok as the same kind, on the same model and effort, with the
    /// same tools or command and a byte-identical system prompt. A mechanical
    /// node never qualifies; it is cheap and may raise triggers.
    pub fn matching(&self, node: &Node, composed_prompt: &str) -> Option<&Value> {
        if self.unusable.is_some() || node.kind == Kind::Mechanical {
            return None;
        }
        let r = self.records.get(&node.name)?;
        let old: Vec<&str> = r["command"]
            .as_array()?
            .iter()
            .filter_map(Value::as_str)
            .collect();
        let same_invocation = match node.provider {
            ProviderKind::Command => old
                .iter()
                .copied()
                .eq(node.command.iter().map(String::as_str)),
            ProviderKind::Claude => {
                let tools = old
                    .iter()
                    .position(|a| *a == "--tools")
                    .and_then(|i| old.get(i + 1));
                tools.copied() == Some(node.tools.join(",").as_str())
            }
        };
        let prompt = std::fs::read_to_string(
            self.dir
                .join("nodes")
                .join(&node.name)
                .join("system-prompt.md"),
        )
        .ok()?;
        (r["status"] == "ok"
            && r["kind"] == node.kind.as_str()
            && r["model"].as_str() == node.model.as_deref()
            && r["effort"].as_str() == node.effort.as_deref()
            && same_invocation
            && prompt == composed_prompt)
            .then_some(r)
    }

    /// The accepted output the node left in its directory.
    pub fn output<T: DeserializeOwned>(&self, name: &str) -> Result<T, String> {
        let path = self.dir.join("nodes").join(name).join("output.json");
        let text =
            std::fs::read_to_string(&path).map_err(|e| format!("{}: {e}", path.display()))?;
        serde_json::from_str(&text).map_err(|e| format!("{}: {e}", path.display()))
    }

    /// Copies the node's directory into `out`'s `nodes/`.
    pub fn copy_node(&self, name: &str, out: &Path) -> Result<(), String> {
        if self.in_place {
            return Ok(());
        }
        let to = out.join("nodes").join(name);
        copy_dir(&self.dir.join("nodes").join(name), &to)
            .map_err(|e| format!("{}: {e}", to.display()))
    }

    /// Puts the node's earlier `out` into `out`'s, for a node that runs again
    /// and may build on what it left. Anything unreadable is left behind.
    pub fn seed_out(&self, name: &str, out: &Path) {
        if self.unusable.is_some() || self.in_place {
            return;
        }
        let _ = copy_dir(
            &self.dir.join("nodes").join(name).join("out"),
            &out.join("nodes").join(name).join("out"),
        );
    }
}
