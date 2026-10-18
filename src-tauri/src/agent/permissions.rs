// Two-level permission model (SPEC 5.2, 6.4): static per-tool policy plus a
// human confirmation queue; full autopilot short-circuits the queue.
use serde_json::Value;

#[derive(Debug, Clone)]
pub struct ConfirmRequest {
    pub tool: String,
    pub args: Value,
    pub project_id: Option<i64>,
    pub danger_reason: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfirmDecision {
    Allow,
    Deny,
    AllowAlways,
}

pub trait Confirmer: Send + Sync {
    fn confirm(&self, req: &ConfirmRequest) -> ConfirmDecision;
}

// Test/dev confirmer with a fixed verdict.
#[derive(Debug, Clone, Copy)]
pub struct AutoConfirmer {
    pub allow: bool,
}

impl AutoConfirmer {
    pub fn allowing(allow: bool) -> Self {
        AutoConfirmer { allow }
    }
}

impl Confirmer for AutoConfirmer {
    fn confirm(&self, _req: &ConfirmRequest) -> ConfirmDecision {
        if self.allow {
            ConfirmDecision::Allow
        } else {
            ConfirmDecision::Deny
        }
    }
}

pub struct DenyAllConfirmer;

impl Confirmer for DenyAllConfirmer {
    fn confirm(&self, _req: &ConfirmRequest) -> ConfirmDecision {
        ConfirmDecision::Deny
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // Table-driven: pins the fixed verdicts of the test/dev confirmers; the
    // request shape must never change what they answer.
    #[test]
    fn confirmer_decision_matrix() {
        let allowing = AutoConfirmer::allowing(true);
        let denying = AutoConfirmer::allowing(false);
        let cases: Vec<(&str, &dyn Confirmer, ConfirmDecision)> = vec![
            ("AutoConfirmer::allowing(true)", &allowing, ConfirmDecision::Allow),
            ("AutoConfirmer::allowing(false)", &denying, ConfirmDecision::Deny),
            ("DenyAllConfirmer", &DenyAllConfirmer, ConfirmDecision::Deny),
        ];
        let requests = [
            ConfirmRequest {
                tool: "fs_write".into(),
                args: json!({ "path": "a.txt" }),
                project_id: Some(3),
                danger_reason: Some("outside the territory".into()),
            },
            ConfirmRequest {
                tool: "bash".into(),
                args: json!({}),
                project_id: None,
                danger_reason: None,
            },
        ];
        for (label, confirmer, want) in cases {
            for req in &requests {
                assert_eq!(confirmer.confirm(req), want, "{label} on {}", req.tool);
            }
        }
    }
}
