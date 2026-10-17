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
