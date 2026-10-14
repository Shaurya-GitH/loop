//! Interactions between backends and the user.
//!
//! [`InteractionQueue`] is Loop's [`HostServices`]: any backend's request
//! (Loop's own tool approvals, or an external agent's permission prompt)
//! becomes a [`PendingInteraction`] that the surface answers.
//!
//! The TUI's approval picker has three fixed choices. [`ApprovalReply`] maps
//! them onto whatever native options the backend offered, by kind.

use std::sync::atomic::{AtomicU64, Ordering};

use async_trait::async_trait;
use loop_protocol::{
    HostServices, InteractionId, InteractionOutcome, InteractionRequest, PermissionKind,
    PermissionOption, ToolCallId, ToolCallUpdate, ToolContent, ToolKind,
};
use tokio::sync::{mpsc, oneshot};

use crate::tool_approval::{ApprovalDecision, ApprovalKind};

/// Native option ids Loop's own approval bridge offers.
pub mod native_options {
    /// Accept this call.
    pub const ACCEPT: &str = "accept";
    /// Accept this and later calls in the same group for this session.
    pub const ACCEPT_SESSION: &str = "accept_session";
    /// Reject this call.
    pub const REJECT: &str = "reject";
}

/// A request waiting for the user.
#[derive(Debug)]
pub struct PendingInteraction {
    /// What the backend asks.
    pub request: InteractionRequest,
    /// Where the answer goes. Dropping it answers `Cancelled`.
    pub respond: oneshot::Sender<InteractionOutcome>,
}

/// [`HostServices`] that forwards every request to one surface.
#[derive(Clone)]
pub struct InteractionQueue {
    tx: mpsc::UnboundedSender<PendingInteraction>,
}

impl InteractionQueue {
    /// A queue and the receiver the surface drains.
    pub fn new() -> (Self, mpsc::UnboundedReceiver<PendingInteraction>) {
        let (tx, rx) = mpsc::unbounded_channel();
        (Self { tx }, rx)
    }
}

#[async_trait]
impl HostServices for InteractionQueue {
    async fn interact(&self, request: InteractionRequest) -> InteractionOutcome {
        let (respond, answer) = oneshot::channel();
        if self
            .tx
            .send(PendingInteraction { request, respond })
            .is_err()
        {
            return InteractionOutcome::Rejected {
                reason: "no surface is attached to answer".into(),
            };
        }
        answer.await.unwrap_or(InteractionOutcome::Cancelled)
    }
}

/// A permission request for one of Loop's own approval prompts.
pub fn native_permission_request(
    kind: ApprovalKind,
    tool_name: &str,
    summary: String,
    detail: String,
) -> InteractionRequest {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    let n = NEXT.fetch_add(1, Ordering::Relaxed);
    let mut tool_call = ToolCallUpdate::new(ToolCallId(format!("approval-{n}")));
    tool_call.name = Some(tool_name.to_owned());
    tool_call.title = Some(summary);
    tool_call.kind = Some(match kind {
        ApprovalKind::FileEdit => ToolKind::Edit,
        ApprovalKind::Bash => ToolKind::Execute,
    });
    if !detail.is_empty() {
        tool_call.content = Some(vec![ToolContent::text(detail)]);
    }
    let option = |id: &str, label: &str, kind_hint| PermissionOption {
        native_id: id.into(),
        label: label.into(),
        kind_hint,
    };
    InteractionRequest::Permission {
        id: InteractionId(format!("approval-{n}")),
        tool_call: Some(tool_call),
        options: vec![
            option(native_options::ACCEPT, "Accept", PermissionKind::AllowOnce),
            option(
                native_options::ACCEPT_SESSION,
                kind.accept_all_label(),
                PermissionKind::AllowAlways,
            ),
            option(native_options::REJECT, "Reject", PermissionKind::RejectOnce),
        ],
    }
}

/// The decision behind an answer to [`native_permission_request`].
pub fn native_decision(outcome: InteractionOutcome) -> ApprovalDecision {
    match outcome {
        InteractionOutcome::Selected { native_id, note } => match native_id.as_str() {
            native_options::ACCEPT => ApprovalDecision::Accept,
            native_options::ACCEPT_SESSION => ApprovalDecision::AcceptSession,
            _ => ApprovalDecision::Reject { reason: note },
        },
        InteractionOutcome::Answered { .. } => ApprovalDecision::Reject {
            reason: Some("unexpected answer to an approval".into()),
        },
        InteractionOutcome::Cancelled => ApprovalDecision::Reject {
            reason: Some("approval cancelled".into()),
        },
        InteractionOutcome::Rejected { reason } => ApprovalDecision::Reject {
            reason: Some(reason),
        },
    }
}

/// Which approval category a tool kind belongs to, for labels.
pub fn approval_kind(kind: Option<ToolKind>) -> ApprovalKind {
    match kind {
        Some(ToolKind::Execute) => ApprovalKind::Bash,
        _ => ApprovalKind::FileEdit,
    }
}

/// Answers a permission request from a three-choice picker
/// (accept / accept for session / reject), using the backend's own option
/// ids. A choice the backend did not offer falls back to the nearest one it
/// did; with no matching option the request is cancelled.
#[derive(Debug)]
pub struct ApprovalReply {
    respond: oneshot::Sender<InteractionOutcome>,
    accept: Option<String>,
    accept_session: Option<String>,
    reject: Option<String>,
}

impl ApprovalReply {
    /// A reply for `options`, answered through `respond`.
    pub fn new(options: &[PermissionOption], respond: oneshot::Sender<InteractionOutcome>) -> Self {
        let find = |kinds: &[PermissionKind]| {
            kinds.iter().find_map(|kind| {
                options
                    .iter()
                    .find(|o| o.kind_hint == *kind)
                    .map(|o| o.native_id.clone())
            })
        };
        Self {
            respond,
            accept: find(&[PermissionKind::AllowOnce, PermissionKind::AllowAlways]),
            accept_session: find(&[PermissionKind::AllowAlways]),
            reject: find(&[PermissionKind::RejectOnce, PermissionKind::RejectAlways]),
        }
    }

    /// Whether the backend offers a "for this session" choice.
    pub fn offers_accept_session(&self) -> bool {
        self.accept_session.is_some()
    }

    /// Send the user's decision.
    pub fn send(self, decision: ApprovalDecision) {
        let (choice, note) = match decision {
            ApprovalDecision::Accept => (self.accept, None),
            ApprovalDecision::AcceptSession => (self.accept_session.or(self.accept), None),
            ApprovalDecision::Reject { reason } => (self.reject, reason),
        };
        let outcome = match choice {
            Some(native_id) => InteractionOutcome::Selected { native_id, note },
            None => InteractionOutcome::Cancelled,
        };
        let _ = self.respond.send(outcome);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reply_for(
        request: &InteractionRequest,
    ) -> (ApprovalReply, oneshot::Receiver<InteractionOutcome>) {
        let InteractionRequest::Permission { options, .. } = request else {
            panic!("permission request expected");
        };
        let (tx, rx) = oneshot::channel();
        (ApprovalReply::new(options, tx), rx)
    }

    #[tokio::test]
    async fn native_request_round_trips_every_decision() {
        let decisions = [
            ApprovalDecision::Accept,
            ApprovalDecision::AcceptSession,
            ApprovalDecision::Reject {
                reason: Some("not that file".into()),
            },
        ];
        for decision in decisions {
            let request =
                native_permission_request(ApprovalKind::Bash, "bash", "ls".into(), "ls -la".into());
            let (reply, rx) = reply_for(&request);
            reply.send(decision.clone());
            assert_eq!(native_decision(rx.await.unwrap()), decision);
        }
    }

    #[tokio::test]
    async fn foreign_options_are_matched_by_kind() {
        let options = vec![
            PermissionOption {
                native_id: "approved".into(),
                label: "Yes".into(),
                kind_hint: PermissionKind::AllowOnce,
            },
            PermissionOption {
                native_id: "abort".into(),
                label: "No".into(),
                kind_hint: PermissionKind::RejectOnce,
            },
        ];
        let (tx, rx) = oneshot::channel();
        let reply = ApprovalReply::new(&options, tx);
        assert!(!reply.offers_accept_session());
        // No "always" option: accepting for the session accepts once.
        reply.send(ApprovalDecision::AcceptSession);
        assert_eq!(
            rx.await.unwrap(),
            InteractionOutcome::Selected {
                native_id: "approved".into(),
                note: None
            }
        );
    }

    #[tokio::test]
    async fn queue_delivers_and_returns_the_answer() {
        let (queue, mut rx) = InteractionQueue::new();
        let asking = tokio::spawn(async move {
            queue
                .interact(native_permission_request(
                    ApprovalKind::FileEdit,
                    "write",
                    "a.txt".into(),
                    String::new(),
                ))
                .await
        });
        let pending = rx.recv().await.unwrap();
        let (reply, _) = reply_for(&pending.request);
        drop(reply);
        let _ = pending.respond.send(InteractionOutcome::Selected {
            native_id: native_options::ACCEPT.into(),
            note: None,
        });
        assert_eq!(
            native_decision(asking.await.unwrap()),
            ApprovalDecision::Accept
        );
    }

    #[tokio::test]
    async fn dropped_surface_cancels() {
        let (queue, rx) = InteractionQueue::new();
        drop(rx);
        let outcome = queue
            .interact(native_permission_request(
                ApprovalKind::Bash,
                "bash",
                "x".into(),
                String::new(),
            ))
            .await;
        assert!(matches!(outcome, InteractionOutcome::Rejected { .. }));
    }
}
