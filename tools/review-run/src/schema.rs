//! The shapes nodes return. Each has two forms that must agree: the JSON
//! Schema handed to the provider so the model is constrained while it writes,
//! and the serde type the harness deserializes into before anything
//! downstream sees the output.

use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum Severity {
    P0,
    P1,
    P2,
    P3,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Confidence {
    Low,
    Medium,
    High,
}

/// The canonical finding. Lens-specific fields ride in `extra`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Finding {
    pub file: Option<String>,
    pub line: Option<u64>,
    pub severity: Severity,
    pub confidence: Confidence,
    pub category: String,
    pub description: String,
    pub why_it_matters: String,
    /// The property that should hold and is violated at the cited site.
    /// Empty in bundles written before the field existed.
    #[serde(default)]
    pub invariant: String,
    pub recommendation: String,
    /// How an implementer confirms the fix landed. Empty in old bundles.
    #[serde(default)]
    pub verify: String,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UpstreamRead {
    pub path: String,
    pub told_me: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GeneratorOutput {
    pub findings: Vec<Finding>,
    #[serde(default)]
    pub upstream_reading: Vec<UpstreamRead>,
    #[serde(default)]
    pub meta: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum VerdictKind {
    Kept,
    Downgraded,
    Upgraded,
    Settled,
    PreExisting,
    FalsePositive,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Verdict {
    /// `<node>:<n>`, the index the harness assigned when it handed the
    /// finding to the critic.
    pub index: String,
    pub verdict: VerdictKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from: Option<Severity>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub to: Option<Severity>,
    pub reason: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CriticOutput {
    pub verdicts: Vec<Verdict>,
}

/// Findings the merger judged to describe one defect: `indices` are the
/// findings' `<node>:<n>` indices, and `reason` says why they are one.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MergeGroup {
    pub indices: Vec<String>,
    pub reason: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MergerOutput {
    pub groups: Vec<MergeGroup>,
}

/// One thing a tool reported. `file` and `line` are null for a complaint
/// about the change as a whole.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Diagnostic {
    pub tool: String,
    pub file: Option<String>,
    pub line: Option<u64>,
    pub message: String,
}

/// A pointer a mechanical node hands later generators, e.g. a file outside the
/// diff to open. Written as a bare string or as `{tool, message}`; an empty
/// `tool` is filled with the node's name once the output is accepted.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(from = "ContextWire")]
pub struct ContextLine {
    pub tool: String,
    pub message: String,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum ContextWire {
    Bare(String),
    Tagged {
        #[serde(default)]
        tool: String,
        message: String,
    },
}

impl From<ContextWire> for ContextLine {
    fn from(w: ContextWire) -> Self {
        match w {
            ContextWire::Bare(message) => ContextLine {
                tool: String::new(),
                message,
            },
            ContextWire::Tagged { tool, message } => ContextLine { tool, message },
        }
    }
}

/// What a mechanical node writes to stdout.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MechanicalOutput {
    #[serde(default)]
    pub diagnostics: Vec<Diagnostic>,
    /// Pointers rendered ahead of the diff in every generator's prompt.
    #[serde(default)]
    pub context: Vec<ContextLine>,
    /// Trigger names the node raises; each must be one it declares in `emits`.
    #[serde(default)]
    pub triggers: Vec<String>,
}

/// What a prewarm node leaves: the session its forks resume and the summary
/// it ended on.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PrewarmOutput {
    pub session_id: Option<String>,
    pub summary: String,
}

/// The headings a prewarm node's closing summary must carry, in order.
pub const SUMMARY_HEADINGS: [&str; 4] = [
    "## Files",
    "## Symbols changed",
    "## Conventions in scope",
    "## Open questions",
];

/// Accepts a prewarm node's closing `meta` if it has every heading of the
/// fixed shape, in order.
pub fn check_summary(text: &str) -> Result<String, String> {
    let text = text.trim().to_string();
    let mut from = 0;
    let mut missing = Vec::new();
    for h in SUMMARY_HEADINGS {
        match text[from..].find(h) {
            Some(at) => from += at + h.len(),
            None => missing.push(h),
        }
    }
    if missing.is_empty() {
        Ok(text)
    } else {
        Err(format!(
            "the summary lacks {} (all four headings are required, in order: {})",
            missing.join(", "),
            SUMMARY_HEADINGS.join(", ")
        ))
    }
}

const SEVERITIES: [&str; 4] = ["P0", "P1", "P2", "P3"];

pub fn generator_schema() -> Value {
    json!({
        "type": "object",
        "required": ["findings", "upstream_reading", "meta"],
        "properties": {
            "findings": {
                "type": "array",
                "items": {
                    "type": "object",
                    "required": ["file", "line", "severity", "confidence", "category",
                                 "description", "why_it_matters", "invariant", "recommendation", "verify"],
                    "properties": {
                        "file": {"type": ["string", "null"], "description": "path relative to the repo root; null for a change-level finding"},
                        "line": {"type": ["integer", "null"], "description": "1-indexed line in the head version of the file"},
                        "severity": {"type": "string", "enum": SEVERITIES},
                        "confidence": {"type": "string", "enum": ["low", "medium", "high"]},
                        "category": {"type": "string"},
                        "description": {"type": "string"},
                        "why_it_matters": {"type": "string"},
                        "invariant": {"type": "string", "minLength": 1, "description": "one sentence naming the property that should hold and is violated at the cited site, e.g. \"Every error from `fetchStack` reaches the caller; this branch swallows it.\" A legibility or style finding states the property it protects."},
                        "recommendation": {"type": "string"},
                        "verify": {"type": "string", "minLength": 1, "description": "how an implementer confirms the fix landed: a named existing test to run, a test to write (what it asserts, and that it must fail before the fix and pass after), a command to run, or the literal text `by inspection` when no automated check is sensible. Never empty."}
                    }
                }
            },
            "upstream_reading": {
                "type": "array",
                "items": {
                    "type": "object",
                    "required": ["path", "told_me"],
                    "properties": {"path": {"type": "string"}, "told_me": {"type": "string"}}
                }
            },
            "meta": {"type": "string", "description": "one paragraph: what was read, what was settled, what was inferred. A reading turn puts its whole summary here and leaves findings empty."}
        }
    })
}

pub fn mechanical_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "diagnostics": {
                "type": "array",
                "items": {
                    "type": "object",
                    "required": ["tool", "message"],
                    "properties": {
                        "tool": {"type": "string"},
                        "file": {"type": ["string", "null"]},
                        "line": {"type": ["integer", "null"]},
                        "message": {"type": "string"}
                    }
                }
            },
            "context": {
                "type": "array",
                "items": {
                    "anyOf": [
                        {"type": "string"},
                        {
                            "type": "object",
                            "required": ["message"],
                            "properties": {"tool": {"type": "string"}, "message": {"type": "string"}}
                        }
                    ]
                }
            },
            "triggers": {"type": "array", "items": {"type": "string"}}
        }
    })
}

pub fn critic_schema() -> Value {
    json!({
        "type": "object",
        "required": ["verdicts"],
        "properties": {
            "verdicts": {
                "type": "array",
                "items": {
                    "type": "object",
                    "required": ["index", "verdict", "reason"],
                    "properties": {
                        "index": {"type": "string"},
                        "verdict": {"type": "string", "enum": ["kept", "downgraded", "upgraded", "settled", "pre-existing", "false-positive"]},
                        "from": {"type": "string", "enum": SEVERITIES},
                        "to": {"type": "string", "enum": SEVERITIES},
                        "reason": {"type": "string"}
                    }
                }
            }
        }
    })
}

pub fn merger_schema() -> Value {
    json!({
        "type": "object",
        "required": ["groups"],
        "properties": {
            "groups": {
                "type": "array",
                "items": {
                    "type": "object",
                    "required": ["indices", "reason"],
                    "properties": {
                        "indices": {"type": "array", "items": {"type": "string"}, "minItems": 2},
                        "reason": {"type": "string"}
                    }
                }
            }
        }
    })
}

/// Checks a merger's output beyond its shape: every index is one it was
/// given, none is in two groups, and a group is at least two findings.
pub fn check_groups(out: &MergerOutput, indices: &[String]) -> Result<(), String> {
    let mut problems = Vec::new();
    let mut seen: Vec<&String> = Vec::new();
    for g in &out.groups {
        if g.indices.len() < 2 {
            problems.push(format!("group {:?} has fewer than two indices", g.indices));
        }
        for i in &g.indices {
            if !indices.contains(i) {
                problems.push(format!("unknown index {i}"));
            } else if seen.contains(&i) {
                problems.push(format!("{i} appears in more than one group"));
            }
            seen.push(i);
        }
    }
    if problems.is_empty() {
        Ok(())
    } else {
        Err(problems.join("; "))
    }
}

/// Checks a critic's output beyond its shape: one verdict per finding it was
/// given, none for findings it was not, and a target severity wherever it
/// claims to have moved one.
pub fn check_verdicts(out: &CriticOutput, indices: &[String]) -> Result<(), String> {
    let mut problems = Vec::new();
    for want in indices {
        match out.verdicts.iter().filter(|v| &v.index == want).count() {
            1 => {}
            0 => problems.push(format!("no verdict for {want}")),
            n => problems.push(format!("{n} verdicts for {want}")),
        }
    }
    for v in &out.verdicts {
        if !indices.contains(&v.index) {
            problems.push(format!("verdict for unknown index {}", v.index));
        }
        if matches!(v.verdict, VerdictKind::Downgraded | VerdictKind::Upgraded) && v.to.is_none() {
            problems.push(format!(
                "{} is {:?} without a `to` severity",
                v.index, v.verdict
            ));
        }
    }
    if problems.is_empty() {
        Ok(())
    } else {
        Err(problems.join("; "))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mechanical_output_without_context_has_none() {
        let o: MechanicalOutput =
            serde_json::from_value(json!({"diagnostics": [], "triggers": []})).unwrap();
        assert!(o.context.is_empty());
    }

    #[test]
    fn context_accepts_bare_strings_and_tagged_objects() {
        let o: MechanicalOutput = serde_json::from_value(json!({
            "context": ["open build/header.mk", {"tool": "context-pack", "message": "see lib.rs"}, {"message": "no tool"}]
        }))
        .unwrap();
        let got: Vec<(&str, &str)> = o
            .context
            .iter()
            .map(|c| (c.tool.as_str(), c.message.as_str()))
            .collect();
        assert_eq!(
            got,
            [
                ("", "open build/header.mk"),
                ("context-pack", "see lib.rs"),
                ("", "no tool")
            ]
        );
        assert!(
            serde_json::from_value::<MechanicalOutput>(json!({"context": [{"tool": "x"}]}))
                .is_err()
        );
        assert!(serde_json::from_value::<MechanicalOutput>(json!({"context": [3]})).is_err());
    }

    fn finding_json() -> Value {
        json!({"file": "a.rs", "line": 1, "severity": "P2", "confidence": "high", "category": "x",
               "description": "d", "why_it_matters": "w", "recommendation": "r"})
    }

    #[test]
    fn generator_schema_requires_invariant_and_verify() {
        let item = &generator_schema()["properties"]["findings"]["items"];
        let required: Vec<&str> = item["required"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        for field in ["invariant", "verify"] {
            assert!(required.contains(&field), "{field} is not required");
            assert_eq!(
                item["properties"][field]["minLength"], 1,
                "{field} may be empty"
            );
        }
    }

    #[test]
    fn a_finding_without_the_brief_fields_still_deserializes() {
        let f: Finding = serde_json::from_value(finding_json()).unwrap();
        assert_eq!((f.invariant.as_str(), f.verify.as_str()), ("", ""));
        assert!(!f.extra.contains_key("invariant"));
    }
}
