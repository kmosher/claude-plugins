//! A review graph: nodes, what each needs, and how each is run. Loaded from
//! one TOML file; prompt paths and a command's path to its executable are
//! relative to that file.

use crate::conventions::{self, Mode};
use crate::triggers::{RawTrigger, Trigger};
use serde::Deserialize;
use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    Generator,
    Critic,
    Mechanical,
    Prewarm,
    Fixer,
    Verify,
    Merger,
}

impl Kind {
    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Generator => "generator",
            Kind::Critic => "critic",
            Kind::Mechanical => "mechanical",
            Kind::Prewarm => "prewarm",
            Kind::Fixer => "fixer",
            Kind::Verify => "verify",
            Kind::Merger => "merger",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ProviderKind {
    Claude,
    Command,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct Defaults {
    provider: Option<ProviderKind>,
    model: Option<String>,
    effort: Option<String>,
    tools: Option<Vec<String>>,
    timeout: Option<u64>,
    conventions: Option<Mode>,
    conventions_budget: Option<usize>,
    max_turns: Option<u32>,
    read_budget_bytes: Option<usize>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawNode {
    name: String,
    kind: Kind,
    provider: Option<ProviderKind>,
    model: Option<String>,
    effort: Option<String>,
    #[serde(default)]
    system_prompt: Vec<String>,
    tools: Option<Vec<String>>,
    timeout: Option<u64>,
    conventions: Option<Mode>,
    #[serde(default)]
    needs: Vec<String>,
    #[serde(default)]
    command: Vec<String>,
    #[serde(default)]
    trigger: Vec<String>,
    #[serde(default)]
    emits: Vec<String>,
    #[serde(default)]
    per_need: bool,
    fork_from: Option<String>,
    max_turns: Option<u32>,
    read_budget_bytes: Option<usize>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct GraphFile {
    #[serde(default)]
    defaults: Defaults,
    #[serde(default)]
    triggers: BTreeMap<String, RawTrigger>,
    #[serde(default)]
    node: Vec<RawNode>,
}

#[derive(Debug, Clone)]
pub struct Node {
    pub name: String,
    pub kind: Kind,
    pub provider: ProviderKind,
    pub model: Option<String>,
    pub effort: Option<String>,
    /// Concatenated in order into the child's whole system prompt.
    pub system_prompt: Vec<PathBuf>,
    pub tools: Vec<String>,
    /// Seconds before the child is killed.
    pub timeout: u64,
    /// How a generator is handed the project's written conventions.
    pub conventions: Mode,
    pub needs: Vec<String>,
    pub command: Vec<String>,
    /// The node runs only if at least one of these has fired.
    pub trigger: Vec<String>,
    /// Trigger names a mechanical node may raise at run time.
    pub emits: Vec<String>,
    /// A critic that runs once per needed generator, as soon as that
    /// generator finishes with findings, rather than once after all of them.
    pub per_need: bool,
    /// A generator that continues a copy of this prewarm node's session
    /// instead of starting cold. Its `system_prompt` is then the prewarm's,
    /// since a fork must run under the prompt its prefix was cached with,
    /// and the files it declared itself are `lens`.
    pub fork_from: Option<String>,
    /// The fork's own declared `system_prompt` files, handed over in its
    /// user turn rather than its system prompt.
    pub lens: Vec<PathBuf>,
    /// Most model turns the child may take (`--max-turns`); unlimited if unset.
    pub max_turns: Option<u32>,
    /// What a prewarm node is told it may read, in bytes of file content.
    pub read_budget_bytes: usize,
}

impl Node {
    /// The name a per-need critic's instance for `generator` goes by: its
    /// record in the manifest and its directory under `nodes/`.
    pub fn instance_name(&self, generator: &str) -> String {
        format!("{}-{generator}", self.name)
    }
}

#[derive(Debug, Clone)]
pub struct Graph {
    pub name: String,
    pub nodes: Vec<Node>,
    /// The triggers the harness evaluates itself, by name.
    pub triggers: Vec<Trigger>,
    /// Bytes of convention files an inline node's prompt may carry.
    pub conventions_budget: usize,
}

const DEFAULT_TIMEOUT: u64 = 1800;
pub const DEFAULT_READ_BUDGET: usize = 400_000;

/// A command named by a relative path with a slash in it is a file beside the
/// graph, and children run in the workspace, so it must not stay relative. A
/// bare name is left for PATH.
fn beside(dir: &Path, program: String) -> String {
    if program.contains('/') && Path::new(&program).is_relative() {
        dir.join(&program).to_string_lossy().into_owned()
    } else {
        program
    }
}

impl Graph {
    pub fn load(path: &Path) -> Result<Graph, String> {
        let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
        let dir = path.parent().unwrap_or(Path::new("."));
        let name = path
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        Graph::parse(&text, dir, &name).map_err(|e| format!("{}: {e}", path.display()))
    }

    pub fn parse(text: &str, dir: &Path, name: &str) -> Result<Graph, String> {
        let file: GraphFile = toml::from_str(text).map_err(|e| e.to_string())?;
        let d = file.defaults;
        let triggers = file
            .triggers
            .iter()
            .map(|(name, raw)| Trigger::compile(name, raw))
            .collect::<Result<Vec<_>, _>>()?;
        let nodes = file
            .node
            .into_iter()
            .map(|n| Node {
                name: n.name,
                kind: n.kind,
                provider: n.provider.or(d.provider).unwrap_or(ProviderKind::Claude),
                model: n.model.or_else(|| d.model.clone()),
                effort: n.effort.or_else(|| d.effort.clone()),
                system_prompt: n.system_prompt.iter().map(|p| dir.join(p)).collect(),
                tools: n.tools.or_else(|| d.tools.clone()).unwrap_or_default(),
                timeout: n.timeout.or(d.timeout).unwrap_or(DEFAULT_TIMEOUT),
                conventions: n
                    .conventions
                    .or(d.conventions)
                    .unwrap_or(conventions::DEFAULT_MODE),
                needs: n.needs,
                command: n
                    .command
                    .into_iter()
                    .enumerate()
                    .map(|(i, c)| if i == 0 { beside(dir, c) } else { c })
                    .collect(),
                trigger: n.trigger,
                emits: n.emits,
                per_need: n.per_need,
                fork_from: n.fork_from,
                lens: Vec::new(),
                max_turns: n.max_turns.or(d.max_turns),
                read_budget_bytes: n
                    .read_budget_bytes
                    .or(d.read_budget_bytes)
                    .unwrap_or(DEFAULT_READ_BUDGET),
            })
            .collect();
        let mut g = Graph {
            name: name.to_string(),
            nodes,
            triggers,
            conventions_budget: d.conventions_budget.unwrap_or(conventions::DEFAULT_BUDGET),
        };
        g.resolve_forks()?;
        g.validate()?;
        Ok(g)
    }

    /// Gives each fork its prewarm node's system prompt and keeps the files it
    /// declared as its lens. Checked here because everything after this reads
    /// `system_prompt` as the prompt the child actually runs under.
    fn resolve_forks(&mut self) -> Result<(), String> {
        for i in 0..self.nodes.len() {
            let Some(from) = self.nodes[i].fork_from.clone() else {
                continue;
            };
            let name = self.nodes[i].name.clone();
            if self.nodes[i].kind != Kind::Generator {
                return Err(format!(
                    "node `{name}`: only a generator can be forked from a prewarm node"
                ));
            }
            let Some(pre) = self.node(&from).cloned() else {
                return Err(format!("node `{name}` forks from unknown node `{from}`"));
            };
            if pre.kind != Kind::Prewarm {
                return Err(format!(
                    "generator `{name}` forks from `{from}`, which is not a prewarm node"
                ));
            }
            let fork = &mut self.nodes[i];
            if fork.provider != ProviderKind::Claude {
                return Err(format!(
                    "node `{name}`: a forked generator must use the claude provider"
                ));
            }
            if fork.tools != pre.tools {
                return Err(format!(
                    "generator `{name}` has tools [{}] but its prewarm `{from}` has [{}]; a fork shares the prewarm's tool list or the cached prefix is lost",
                    fork.tools.join(","),
                    pre.tools.join(",")
                ));
            }
            fork.lens = std::mem::replace(&mut fork.system_prompt, pre.system_prompt.clone());
        }
        Ok(())
    }

    /// `node` as a plain generator: the system prompt it would run under with
    /// no session to fork, which is its prewarm's files followed by its lens.
    pub fn unforked(&self, node: &Node) -> Node {
        let mut n = node.clone();
        if n.fork_from.take().is_some() {
            n.system_prompt.extend(std::mem::take(&mut n.lens));
        }
        n
    }

    /// The conventions mode `node`'s prompt was built with: a fork's rules
    /// came with its prewarm's prompt.
    pub fn conventions_of(&self, node: &Node) -> Mode {
        node.fork_from
            .as_ref()
            .and_then(|f| self.node(f))
            .map_or(node.conventions, |p| p.conventions)
    }

    fn validate(&self) -> Result<(), String> {
        if self.nodes.is_empty() {
            return Err("graph has no nodes".into());
        }
        let mut seen = HashSet::new();
        for n in &self.nodes {
            if !seen.insert(n.name.as_str()) {
                return Err(format!("node `{}` is defined twice", n.name));
            }
            if n.name.is_empty()
                || !n
                    .name
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
            {
                // The name becomes a file name and the prefix of a finding index.
                return Err(format!(
                    "node name `{}` must be alphanumeric, `-` or `_`",
                    n.name
                ));
            }
            if n.per_need && n.kind != Kind::Critic {
                return Err(format!("node `{}`: only a critic can be per_need", n.name));
            }
            if !matches!(
                n.kind,
                Kind::Generator | Kind::Critic | Kind::Mechanical | Kind::Merger | Kind::Prewarm
            ) {
                return Err(format!(
                    "node `{}`: kind `{}` is not built yet",
                    n.name,
                    n.kind.as_str()
                ));
            }
            if n.kind == Kind::Prewarm {
                if n.provider != ProviderKind::Claude {
                    return Err(format!(
                        "node `{}`: a prewarm node must use the claude provider",
                        n.name
                    ));
                }
                if !n.needs.is_empty() {
                    return Err(format!("node `{}`: a prewarm node waits only for the mechanical nodes, so it takes no needs", n.name));
                }
            }
            if n.kind == Kind::Merger && !n.needs.is_empty() {
                return Err(format!("node `{}`: a merger waits for every generator and critic, so it takes no needs", n.name));
            }
            if n.kind == Kind::Mechanical {
                if n.provider != ProviderKind::Command {
                    return Err(format!(
                        "node `{}`: a mechanical node must use the command provider",
                        n.name
                    ));
                }
                if !n.trigger.is_empty() {
                    return Err(format!(
                        "node `{}`: a mechanical node always runs, so it cannot have a trigger",
                        n.name
                    ));
                }
            } else if !n.emits.is_empty() {
                return Err(format!(
                    "node `{}`: only a mechanical node can emit triggers",
                    n.name
                ));
            }
            match n.provider {
                ProviderKind::Claude if n.system_prompt.is_empty() => {
                    return Err(format!(
                        "node `{}`: a claude node needs a system_prompt",
                        n.name
                    ));
                }
                ProviderKind::Command if n.command.is_empty() => {
                    return Err(format!("node `{}`: a command node needs a command", n.name));
                }
                _ => {}
            }
            for p in &n.system_prompt {
                if !p.is_file() {
                    return Err(format!(
                        "node `{}`: system prompt {} not found",
                        n.name,
                        p.display()
                    ));
                }
            }
        }
        for n in &self.nodes {
            for need in &n.needs {
                let Some(dep) = self.node(need) else {
                    return Err(format!("node `{}` needs unknown node `{need}`", n.name));
                };
                if n.kind == Kind::Critic && dep.kind != Kind::Generator {
                    return Err(format!(
                        "critic `{}` needs `{need}`, which is not a generator",
                        n.name
                    ));
                }
                if n.kind == Kind::Generator && dep.kind != Kind::Generator {
                    return Err(format!(
                        "generator `{}` needs `{need}`, which is not a generator",
                        n.name
                    ));
                }
                // Every other node already waits for all mechanical ones, so a
                // mechanical node waiting on anything else would be a cycle.
                if n.kind == Kind::Mechanical && dep.kind != Kind::Mechanical {
                    return Err(format!(
                        "mechanical node `{}` needs `{need}`, which is not mechanical",
                        n.name
                    ));
                }
            }
            if n.kind == Kind::Critic && n.needs.is_empty() {
                return Err(format!("critic `{}` needs at least one generator", n.name));
            }
            if n.per_need {
                for need in &n.needs {
                    let instance = n.instance_name(need);
                    if self.node(&instance).is_some() {
                        return Err(format!("critic `{}`: its instance for `{need}` would be named `{instance}`, which is a node", n.name));
                    }
                }
            }
        }
        self.validate_generator_cycles()?;
        self.validate_triggers()?;
        if self.nodes.iter().filter(|n| n.kind == Kind::Merger).count() > 1 {
            return Err("more than one merger is not allowed".into());
        }
        // Verdicts from two critics on one finding have no defined merge yet.
        if self.nodes.iter().filter(|n| n.kind == Kind::Critic).count() > 1 {
            return Err("more than one critic is not built yet".into());
        }
        self.waves().map(|_| ())
    }

    /// Names the generators that wait on one another in a loop, which `waves`
    /// can only report as everything left unscheduled.
    fn validate_generator_cycles(&self) -> Result<(), String> {
        fn visit<'a>(
            g: &'a Graph,
            node: &'a Node,
            path: &mut Vec<&'a str>,
            done: &mut HashSet<&'a str>,
        ) -> Result<(), String> {
            if let Some(at) = path.iter().position(|p| *p == node.name) {
                let mut cycle: Vec<&str> = path[at..].to_vec();
                cycle.push(&node.name);
                return Err(format!(
                    "generators need each other in a cycle: {}",
                    cycle.join(" -> ")
                ));
            }
            if !done.insert(node.name.as_str()) {
                return Ok(());
            }
            path.push(&node.name);
            for dep in node
                .needs
                .iter()
                .filter_map(|d| g.node(d))
                .filter(|d| d.kind == Kind::Generator)
            {
                visit(g, dep, path, done)?;
            }
            path.pop();
            Ok(())
        }
        let mut done = HashSet::new();
        for n in self.nodes.iter().filter(|n| n.kind == Kind::Generator) {
            visit(self, n, &mut Vec::new(), &mut done)?;
        }
        Ok(())
    }

    fn validate_triggers(&self) -> Result<(), String> {
        let emitted: HashSet<&str> = self
            .nodes
            .iter()
            .flat_map(|n| n.emits.iter().map(String::as_str))
            .collect();
        for t in &self.triggers {
            if emitted.contains(t.name.as_str()) {
                return Err(format!(
                    "trigger `{}` is defined in [triggers] and also emitted by a mechanical node",
                    t.name
                ));
            }
        }
        for n in &self.nodes {
            for name in &n.trigger {
                if !emitted.contains(name.as_str())
                    && !self.triggers.iter().any(|t| &t.name == name)
                {
                    return Err(format!("node `{}` has unknown trigger `{name}`", n.name));
                }
            }
        }
        Ok(())
    }

    pub fn node(&self, name: &str) -> Option<&Node> {
        self.nodes.iter().find(|n| n.name == name)
    }

    /// What a node waits for: its own needs, and for any node that is not
    /// mechanical, every mechanical node, which are always the first waves. A
    /// merger also waits for every generator and critic, since it reads
    /// what they leave.
    pub fn waits_for<'a>(&'a self, node: &'a Node) -> Vec<&'a str> {
        let mut deps: Vec<&str> = node.needs.iter().map(String::as_str).collect();
        deps.extend(node.fork_from.as_deref());
        if node.kind != Kind::Mechanical {
            let merges = node.kind == Kind::Merger;
            let implied = self
                .nodes
                .iter()
                .filter(|n| {
                    n.kind == Kind::Mechanical
                        || (merges && matches!(n.kind, Kind::Generator | Kind::Critic))
                })
                .map(|n| n.name.as_str());
            for name in implied {
                if !deps.contains(&name) {
                    deps.push(name);
                }
            }
        }
        deps
    }

    /// Nodes grouped so that everything a node waits for is in an earlier group.
    pub fn waves(&self) -> Result<Vec<Vec<&Node>>, String> {
        let mut done: HashSet<&str> = HashSet::new();
        let mut waves = Vec::new();
        while done.len() < self.nodes.len() {
            let wave: Vec<&Node> = self
                .nodes
                .iter()
                .filter(|n| {
                    !done.contains(n.name.as_str())
                        && self.waits_for(n).iter().all(|d| done.contains(d))
                })
                .collect();
            if wave.is_empty() {
                let stuck: Vec<&str> = self
                    .nodes
                    .iter()
                    .map(|n| n.name.as_str())
                    .filter(|n| !done.contains(n))
                    .collect();
                return Err(format!("dependency cycle among: {}", stuck.join(", ")));
            }
            done.extend(wave.iter().map(|n| n.name.as_str()));
            waves.push(wave);
        }
        Ok(waves)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(text: &str) -> Result<Graph, String> {
        Graph::parse(text, Path::new("."), "t")
    }

    const GEN: &str = "provider = \"command\"\ncommand = [\"true\"]\n";

    #[test]
    fn defaults_fill_what_a_node_leaves_out() {
        let g = parse(&format!(
            "[defaults]\nmodel = \"opus\"\ntimeout = 60\n[[node]]\nname = \"a\"\nkind = \"generator\"\n{GEN}\
             [[node]]\nname = \"b\"\nkind = \"generator\"\nmodel = \"sonnet\"\n{GEN}"
        ))
        .unwrap();
        assert_eq!(g.nodes[0].model.as_deref(), Some("opus"));
        assert_eq!(g.nodes[1].model.as_deref(), Some("sonnet"));
        assert_eq!(g.nodes[0].timeout, 60);
    }

    #[test]
    fn conventions_default_to_a_listing_and_a_node_may_override_the_graph() {
        let g = parse(&format!(
            "[[node]]\nname = \"a\"\nkind = \"generator\"\n{GEN}"
        ))
        .unwrap();
        assert_eq!(
            (g.nodes[0].conventions, g.conventions_budget),
            (Mode::List, 150_000)
        );
        let g = parse(&format!(
            "[defaults]\nconventions = \"inline\"\nconventions_budget = 99\n[[node]]\nname = \"a\"\nkind = \"generator\"\n{GEN}\
             [[node]]\nname = \"b\"\nkind = \"generator\"\nconventions = \"none\"\n{GEN}"
        ))
        .unwrap();
        assert_eq!(
            (
                g.nodes[0].conventions,
                g.nodes[1].conventions,
                g.conventions_budget
            ),
            (Mode::Inline, Mode::None, 99)
        );
        let bad = parse(&format!("[defaults]\nconventions = \"all\"\n[[node]]\nname = \"a\"\nkind = \"generator\"\n{GEN}"));
        assert!(bad.unwrap_err().contains("all"));
    }

    #[test]
    fn waves_put_a_critic_after_its_generators() {
        let g = parse(&format!(
            "[[node]]\nname = \"audit\"\nkind = \"critic\"\nneeds = [\"a\", \"b\"]\n{GEN}\
             [[node]]\nname = \"a\"\nkind = \"generator\"\n{GEN}[[node]]\nname = \"b\"\nkind = \"generator\"\n{GEN}"
        ))
        .unwrap();
        let waves: Vec<Vec<&str>> = g
            .waves()
            .unwrap()
            .iter()
            .map(|w| w.iter().map(|n| n.name.as_str()).collect())
            .collect();
        assert_eq!(waves, vec![vec!["a", "b"], vec!["audit"]]);
    }

    #[test]
    fn rejects_what_it_cannot_run() {
        let unknown = parse(&format!(
            "[[node]]\nname = \"c\"\nkind = \"critic\"\nneeds = [\"x\"]\n{GEN}"
        ));
        assert!(unknown.unwrap_err().contains("unknown node `x`"));
        let cycle = parse(&format!(
            "[[node]]\nname = \"a\"\nkind = \"generator\"\nneeds = [\"b\"]\n{GEN}\
             [[node]]\nname = \"b\"\nkind = \"generator\"\nneeds = [\"a\"]\n{GEN}"
        ));
        assert!(cycle.unwrap_err().contains("cycle"));
        let unbuilt = parse(&format!("[[node]]\nname = \"m\"\nkind = \"fixer\"\n{GEN}"));
        assert!(unbuilt.unwrap_err().contains("not built yet"));
        let typo = parse(&format!(
            "[[node]]\nname = \"a\"\nkind = \"generator\"\nefort = \"high\"\n{GEN}"
        ));
        assert!(typo.unwrap_err().contains("efort"));
    }

    fn merger(extra: &str) -> String {
        format!("[[node]]\nname = \"dedupe\"\nkind = \"merger\"\n{GEN}{extra}")
    }

    #[test]
    fn a_merger_waits_for_every_generator_and_critic() {
        let crit =
            "[[node]]\nname = \"audit\"\nkind = \"critic\"\nneeds = [\"a\"]\nper_need = true\n"
                .to_string()
                + GEN;
        let g = parse(&format!(
            "{}{}{}{}",
            merger(""),
            gen("a", ""),
            gen("b", ""),
            crit
        ))
        .unwrap();
        let waves: Vec<Vec<&str>> = g
            .waves()
            .unwrap()
            .iter()
            .map(|w| w.iter().map(|n| n.name.as_str()).collect())
            .collect();
        assert_eq!(waves, vec![vec!["a", "b"], vec!["audit"], vec!["dedupe"]]);
    }

    #[test]
    fn rejects_a_second_merger_and_a_merger_with_needs() {
        let err = |text: String| parse(&text).unwrap_err();
        let second = merger("").replace("dedupe", "again");
        assert!(err(format!("{}{}{}", gen("a", ""), merger(""), second))
            .contains("more than one merger"));
        assert!(
            err(format!("{}{}", gen("a", ""), merger("needs = [\"a\"]\n")))
                .contains("takes no needs")
        );
        assert!(err(format!(
            "{}{}",
            gen("a", "needs = [\"dedupe\"]\n"),
            merger("")
        ))
        .contains("which is not a generator"));
    }

    #[test]
    fn a_generator_may_need_generators_and_only_generators() {
        let err = |text: String| parse(&text).unwrap_err();
        let g = parse(&format!(
            "{}{}",
            gen("a", "needs = [\"b\"]\n"),
            gen("b", "")
        ))
        .unwrap();
        let waves: Vec<Vec<&str>> = g
            .waves()
            .unwrap()
            .iter()
            .map(|w| w.iter().map(|n| n.name.as_str()).collect())
            .collect();
        assert_eq!(waves, vec![vec!["b"], vec!["a"]]);
        let crit = format!("[[node]]\nname = \"c\"\nkind = \"critic\"\nneeds = [\"b\"]\n{GEN}");
        let msg = err(format!(
            "{}{}{}",
            gen("a", "needs = [\"c\"]\n"),
            gen("b", ""),
            crit
        ));
        assert!(
            msg.contains("generator `a` needs `c`, which is not a generator"),
            "{msg}"
        );
        let msg = err(format!(
            "{}{}",
            gen("a", "needs = [\"lint\"]\n"),
            mech("lint", "")
        ));
        assert!(
            msg.contains("generator `a` needs `lint`, which is not a generator"),
            "{msg}"
        );
    }

    #[test]
    fn generators_that_need_each_other_in_a_loop_are_named() {
        let err = |text: String| parse(&text).unwrap_err();
        let msg = err(format!(
            "{}{}{}",
            gen("a", "needs = [\"b\"]\n"),
            gen("b", "needs = [\"c\"]\n"),
            gen("c", "needs = [\"b\"]\n")
        ));
        assert!(msg.contains("cycle: b -> c -> b"), "{msg}");
        let msg = err(gen("a", "needs = [\"a\"]\n"));
        assert!(msg.contains("cycle: a -> a"), "{msg}");
    }

    fn mech(name: &str, extra: &str) -> String {
        format!("[[node]]\nname = \"{name}\"\nkind = \"mechanical\"\n{GEN}{extra}")
    }

    fn gen(name: &str, extra: &str) -> String {
        format!("[[node]]\nname = \"{name}\"\nkind = \"generator\"\n{GEN}{extra}")
    }

    #[test]
    fn a_command_with_a_relative_path_is_found_beside_the_graph() {
        let text = "[[node]]\nname = \"m\"\nkind = \"mechanical\"\nprovider = \"command\"\ncommand = [\"mechanical/lint\", \"mechanical/arg\"]\n\
                    [[node]]\nname = \"p\"\nkind = \"generator\"\nprovider = \"command\"\ncommand = [\"true\"]\n";
        let g = Graph::parse(text, Path::new("/g"), "t").unwrap();
        assert_eq!(
            g.nodes[0].command,
            ["/g/mechanical/lint", "mechanical/arg"],
            "only the program moves"
        );
        assert_eq!(g.nodes[1].command, ["true"]);
    }

    #[test]
    fn every_node_waits_for_the_mechanical_ones_without_saying_so() {
        let g = parse(&format!(
            "{}{}{}",
            gen("a", ""),
            mech("lint", ""),
            mech("types", "needs = [\"lint\"]\n")
        ))
        .unwrap();
        let waves: Vec<Vec<&str>> = g
            .waves()
            .unwrap()
            .iter()
            .map(|w| w.iter().map(|n| n.name.as_str()).collect())
            .collect();
        assert_eq!(waves, vec![vec!["lint"], vec!["types"], vec!["a"]]);
    }

    #[test]
    fn a_trigger_may_be_defined_or_emitted() {
        let g = parse(&format!(
            "[triggers]\nddl = {{ touches = [\"**/*.sql\"] }}\n{}{}",
            mech("lint", "emits = [\"exports-changed\"]\n"),
            gen("compat", "trigger = [\"ddl\", \"exports-changed\"]\n")
        ))
        .unwrap();
        assert_eq!(
            g.node("compat").unwrap().trigger,
            ["ddl", "exports-changed"]
        );
        assert_eq!(g.triggers.len(), 1);
    }

    #[test]
    fn rejects_bad_triggers_and_misplaced_mechanical_nodes() {
        let err = |text: String| parse(&text).unwrap_err();
        assert!(
            err(gen("a", "trigger = [\"nope\"]\n")).contains("node `a` has unknown trigger `nope`")
        );
        assert!(
            err("[triggers]\nddl = { touches = [\"[\"] }\n".to_string() + &gen("a", ""))
                .contains("trigger `ddl`: invalid glob")
        );
        assert!(
            err("[triggers]\nddl = { diff_matches = \"(\" }\n".to_string() + &gen("a", ""))
                .contains("trigger `ddl`: invalid regex")
        );
        assert!(err("[triggers]\nddl = {}\n".to_string() + &gen("a", ""))
            .contains("trigger `ddl` needs"));
        assert!(err(
            "[triggers]\nx = { touches = [\"a\"] }\n".to_string() + &mech("m", "emits = [\"x\"]\n")
        )
        .contains("also emitted"));
        assert!(err(format!(
            "{}{}",
            mech("m", "emits = [\"x\"]\n"),
            mech("n", "trigger = [\"x\"]\n")
        ))
        .contains("cannot have a trigger"));
        assert!(err(gen("a", "emits = [\"x\"]\n")).contains("only a mechanical node can emit"));
        assert!(err(gen("a", "per_need = true\n")).contains("only a critic can be per_need"));
        let crit = |extra: &str| {
            format!("[[node]]\nname = \"c\"\nkind = \"critic\"\nneeds = [\"a\"]\n{GEN}{extra}")
        };
        assert!(
            err(format!("{}{}", gen("a", ""), gen("c-a", "")) + &crit("per_need = true\n"))
                .contains("would be named `c-a`")
        );
        assert!(
            parse(&(gen("a", "") + &crit("per_need = true\n")))
                .unwrap()
                .node("c")
                .unwrap()
                .per_need
        );
        let claude = "[[node]]\nname = \"m\"\nkind = \"mechanical\"\nprovider = \"claude\"\nsystem_prompt = [\"x\"]\n";
        assert!(err(claude.to_string()).contains("must use the command provider"));
        assert!(err(format!(
            "{}{}",
            gen("a", ""),
            mech("m", "needs = [\"a\"]\n")
        ))
        .contains("which is not mechanical"));
    }

    #[test]
    fn a_fork_runs_under_its_prewarms_system_prompt_and_keeps_its_own_files_as_a_lens() {
        let dir =
            std::env::temp_dir().join(format!("review-run-graph-fork-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        for f in ["reader.md", "lens.md"] {
            std::fs::write(dir.join(f), "x").unwrap();
        }
        let node = |name: &str, kind: &str, extra: &str| {
            format!("[[node]]\nname = \"{name}\"\nkind = \"{kind}\"\nprovider = \"claude\"\ntools = [\"Read\"]\n{extra}")
        };
        let text = node("warm", "prewarm", "system_prompt = [\"reader.md\"]\n")
            + &node(
                "a",
                "generator",
                "fork_from = \"warm\"\nsystem_prompt = [\"lens.md\"]\n",
            );
        let g = Graph::parse(&text, &dir, "t").unwrap();
        let a = g.node("a").unwrap();
        assert_eq!(
            (a.system_prompt.clone(), a.lens.clone()),
            (vec![dir.join("reader.md")], vec![dir.join("lens.md")])
        );
        assert_eq!(g.waits_for(a), ["warm"]);
        assert_eq!(g.node("warm").unwrap().read_budget_bytes, 400_000);
        let plain = g.unforked(a);
        assert_eq!(
            (plain.system_prompt, plain.lens, plain.fork_from),
            (
                vec![dir.join("reader.md"), dir.join("lens.md")],
                vec![],
                None
            )
        );

        let err = |text: String| Graph::parse(&text, &dir, "t").unwrap_err();
        let warm = node("warm", "prewarm", "system_prompt = [\"reader.md\"]\n");
        assert!(err(warm.clone()
            + &node(
                "a",
                "generator",
                "fork_from = \"nope\"\nsystem_prompt = [\"lens.md\"]\n"
            ))
        .contains("forks from unknown node `nope`"));
        let other = node("w2", "generator", "system_prompt = [\"reader.md\"]\n");
        assert!(err(other + &node("a", "generator", "fork_from = \"w2\"\n"))
            .contains("which is not a prewarm node"));
        let msg = err(warm.clone() + "[[node]]\nname = \"a\"\nkind = \"generator\"\nprovider = \"claude\"\ntools = [\"Read\", \"Bash\"]\nfork_from = \"warm\"\nsystem_prompt = [\"lens.md\"]\n");
        assert!(
            msg.contains("tools [Read,Bash] but its prewarm `warm` has [Read]"),
            "{msg}"
        );
        assert!(
            err(format!("[[node]]\nname = \"w\"\nkind = \"prewarm\"\n{GEN}"))
                .contains("must use the claude provider")
        );
        assert!(err(warm
            + &node("g", "generator", "system_prompt = [\"lens.md\"]\n")
            + &node(
                "w3",
                "prewarm",
                "needs = [\"g\"]\nsystem_prompt = [\"reader.md\"]\n"
            ))
        .contains("takes no needs"));
        assert!(err(node(
            "c",
            "critic",
            "fork_from = \"c\"\nneeds = [\"c\"]\nsystem_prompt = [\"lens.md\"]\n"
        ))
        .contains("only a generator can be forked"));
    }
}
