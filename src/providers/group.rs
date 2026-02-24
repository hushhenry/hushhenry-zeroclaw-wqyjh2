//! Provider group: routes requests across multiple providers by strategy.
//! Used when model name is `group/<group_name>`; the group is configured in config with
//! members (full model names) and strategy (round_robin, random, fallback).

use super::traits::{ChatRequest, ChatResponse, Provider, ProviderCapabilities};
use async_trait::async_trait;
use rand::Rng;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

/// Strategy for selecting a member of the group for each request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GroupStrategy {
    /// Rotate through members on each request.
    RoundRobin,
    /// Pick a random member per request.
    Random,
    /// Use first member; on failure try next, and so on.
    Fallback,
}

impl From<crate::config::ProviderGroupStrategy> for GroupStrategy {
    fn from(s: crate::config::ProviderGroupStrategy) -> Self {
        match s {
            crate::config::ProviderGroupStrategy::RoundRobin => GroupStrategy::RoundRobin,
            crate::config::ProviderGroupStrategy::Random => GroupStrategy::Random,
            crate::config::ProviderGroupStrategy::Fallback => GroupStrategy::Fallback,
        }
    }
}

/// A provider that delegates to one of several member providers according to a strategy.
pub struct GroupProvider {
    members: Vec<Arc<dyn Provider>>,
    strategy: GroupStrategy,
    round_robin_next: AtomicUsize,
}

impl GroupProvider {
    /// Build a group from a list of member providers and a strategy.
    pub fn new(members: Vec<Arc<dyn Provider>>, strategy: GroupStrategy) -> Self {
        Self {
            members,
            strategy,
            round_robin_next: AtomicUsize::new(0),
        }
    }

    fn len(&self) -> usize {
        self.members.len()
    }

    /// Choose member index for this request (round_robin or random). Fallback is not used here.
    fn select_index(&self) -> usize {
        let n = self.len();
        if n == 0 {
            return 0;
        }
        match self.strategy {
            GroupStrategy::RoundRobin => {
                let i = self.round_robin_next.fetch_add(1, Ordering::Relaxed);
                i % n
            }
            GroupStrategy::Random => rand::thread_rng().gen_range(0..n),
            GroupStrategy::Fallback => 0,
        }
    }
}

#[async_trait]
impl Provider for GroupProvider {
    async fn chat(
        &self,
        request: ChatRequest<'_>,
        model: &str,
        temperature: f64,
    ) -> anyhow::Result<ChatResponse> {
        if self.members.is_empty() {
            anyhow::bail!("Group has no members");
        }
        match self.strategy {
            GroupStrategy::Fallback => {
                let mut last_err = None;
                for provider in &self.members {
                    match provider.chat(request, model, temperature).await {
                        Ok(r) => return Ok(r),
                        Err(e) => last_err = Some(e),
                    }
                }
                Err(last_err.unwrap_or_else(|| anyhow::anyhow!("No members in group")))
            }
            GroupStrategy::RoundRobin | GroupStrategy::Random => {
                let idx = self.select_index();
                self.members[idx].chat(request, model, temperature).await
            }
        }
    }

    fn capabilities(&self) -> ProviderCapabilities {
        self.members
            .first()
            .map(|p| p.capabilities())
            .unwrap_or_default()
    }

    async fn warmup(&self) -> anyhow::Result<()> {
        for (i, p) in self.members.iter().enumerate() {
            if let Err(e) = p.warmup().await {
                tracing::warn!(member = i, "Group member warmup failed (non-fatal): {e}");
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::ChatMessage;

    struct EchoProvider;
    struct FailingProvider;

    #[async_trait::async_trait]
    impl Provider for EchoProvider {
        async fn chat(
            &self,
            request: super::super::traits::ChatRequest<'_>,
            _model: &str,
            _temperature: f64,
        ) -> anyhow::Result<ChatResponse> {
            let text = request
                .messages
                .iter()
                .find(|m| m.role == "user")
                .map(|m| m.content.as_str())
                .unwrap_or("");
            Ok(ChatResponse {
                text: Some(text.to_string()),
                tool_calls: vec![],
                usage: None,
            })
        }
    }

    #[async_trait::async_trait]
    impl Provider for FailingProvider {
        async fn chat(
            &self,
            _request: super::super::traits::ChatRequest<'_>,
            _model: &str,
            _temperature: f64,
        ) -> anyhow::Result<ChatResponse> {
            anyhow::bail!("always fails")
        }
    }

    #[tokio::test]
    async fn group_fallback_uses_second_on_first_failure() {
        let members: Vec<Arc<dyn Provider>> =
            vec![Arc::new(FailingProvider), Arc::new(EchoProvider)];
        let group = GroupProvider::new(members, GroupStrategy::Fallback);
        let req = super::super::traits::ChatRequest {
            messages: &[ChatMessage::user("hi")],
            tools: None,
        };
        let r = group.chat(req, "model", 0.0).await.unwrap();
        assert_eq!(r.text.as_deref(), Some("hi"));
    }

    #[tokio::test]
    async fn group_round_robin_rotates() {
        let members: Vec<Arc<dyn Provider>> = vec![Arc::new(EchoProvider), Arc::new(EchoProvider)];
        let group = GroupProvider::new(members, GroupStrategy::RoundRobin);
        let req = super::super::traits::ChatRequest {
            messages: &[ChatMessage::user("a")],
            tools: None,
        };
        let _ = group.chat(req, "m", 0.0).await.unwrap();
        let _ = group.chat(req, "m", 0.0).await.unwrap();
        // Both members used; no panic
    }
}
