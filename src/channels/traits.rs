use async_trait::async_trait;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChatType {
    Direct,
    Group,
    Thread,
    Unknown,
}

impl ChatType {
    pub fn from_raw(raw_chat_type: &str) -> Self {
        match raw_chat_type.to_ascii_lowercase().as_str() {
            "direct" | "dm" | "private" | "p2p" | "c2c" => Self::Direct,
            "group" | "channel" | "supergroup" | "room" => Self::Group,
            "thread" => Self::Thread,
            _ => Self::Unknown,
        }
    }
}

/// A message received from or sent to a channel
#[derive(Debug, Clone)]
pub struct ChannelMessage {
    pub id: String,
    pub agent_id: Option<String>,
    pub account_id: Option<String>,
    pub sender: String,
    pub reply_target: String,
    pub content: String,
    pub channel: String,
    pub title: Option<String>,
    pub chat_type: ChatType,
    pub raw_chat_type: Option<String>,
    pub chat_id: String,
    pub thread_id: Option<String>,
    pub timestamp: u64,
}

impl ChannelMessage {
    /// Builds a message for channel ingress. Agent routing is not set here;
    /// the session/router layer assigns `agent_id` when resolving the route.
    /// `account_id` can be provided if the adapter knows which account received the message.
    #[allow(clippy::too_many_arguments)]
    pub fn new_ingress(
        id: impl Into<String>,
        account_id: Option<String>,
        sender: impl Into<String>,
        reply_target: impl Into<String>,
        content: impl Into<String>,
        channel: impl Into<String>,
        title: Option<String>,
        chat_type: ChatType,
        raw_chat_type: Option<String>,
        chat_id: impl Into<String>,
        thread_id: Option<String>,
        timestamp: u64,
    ) -> Self {
        Self {
            id: id.into(),
            agent_id: None,
            account_id,
            sender: sender.into(),
            reply_target: reply_target.into(),
            content: content.into(),
            channel: channel.into(),
            title,
            chat_type,
            raw_chat_type,
            chat_id: chat_id.into(),
            thread_id,
            timestamp,
        }
    }

    #[deprecated(note = "renamed to chat_id; use `chat_id` instead")]
    pub fn conversation_id(&self) -> &str {
        &self.chat_id
    }
}

/// Message to send through a channel
#[derive(Debug, Clone)]
pub struct SendMessage {
    pub content: String,
    pub recipient: String,
    pub subject: Option<String>,
}

impl SendMessage {
    /// Create a new message with content and recipient
    pub fn new(content: impl Into<String>, recipient: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            recipient: recipient.into(),
            subject: None,
        }
    }

    /// Create a new message with content, recipient, and subject
    pub fn with_subject(
        content: impl Into<String>,
        recipient: impl Into<String>,
        subject: impl Into<String>,
    ) -> Self {
        Self {
            content: content.into(),
            recipient: recipient.into(),
            subject: Some(subject.into()),
        }
    }
}

/// Core channel trait — implement for any messaging platform
#[async_trait]
pub trait Channel: Send + Sync {
    /// Human-readable channel name
    fn name(&self) -> &str;

    /// Send a message through this channel
    async fn send(&self, message: &SendMessage) -> anyhow::Result<()>;

    /// Start listening for incoming messages (long-running)
    async fn listen(&self, tx: tokio::sync::mpsc::Sender<ChannelMessage>) -> anyhow::Result<()>;

    /// Check if channel is healthy
    async fn health_check(&self) -> bool {
        true
    }

    /// Signal that the bot is processing a response (e.g. "typing" indicator).
    /// Implementations should repeat the indicator as needed for their platform.
    async fn start_typing(&self, _recipient: &str) -> anyhow::Result<()> {
        Ok(())
    }

    /// Stop any active typing indicator.
    async fn stop_typing(&self, _recipient: &str) -> anyhow::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct DummyChannel;

    #[async_trait]
    impl Channel for DummyChannel {
        fn name(&self) -> &str {
            "dummy"
        }

        async fn send(&self, _message: &SendMessage) -> anyhow::Result<()> {
            Ok(())
        }

        async fn listen(
            &self,
            tx: tokio::sync::mpsc::Sender<ChannelMessage>,
        ) -> anyhow::Result<()> {
            tx.send(ChannelMessage::new_ingress(
                "1",
                None,
                "tester",
                "tester",
                "hello",
                "dummy",
                None,
                ChatType::Direct,
                None,
                "tester",
                None,
                123,
            ))
            .await
            .map_err(|e| anyhow::anyhow!(e.to_string()))
        }
    }

    #[test]
    fn channel_message_clone_preserves_fields() {
        let message = ChannelMessage::new_ingress(
            "42",
            None,
            "alice",
            "alice",
            "ping",
            "dummy",
            Some("Example Chat".into()),
            ChatType::Direct,
            None,
            "alice",
            None,
            999,
        );

        let cloned = message.clone();
        assert_eq!(cloned.id, "42");
        assert!(cloned.agent_id.is_none());
        assert!(cloned.account_id.is_none());
        assert_eq!(cloned.sender, "alice");
        assert_eq!(cloned.reply_target, "alice");
        assert_eq!(cloned.content, "ping");
        assert_eq!(cloned.channel, "dummy");
        assert_eq!(cloned.title.as_deref(), Some("Example Chat"));
        assert_eq!(cloned.chat_type, ChatType::Direct);
        assert!(cloned.raw_chat_type.is_none());
        assert_eq!(cloned.chat_id, "alice");
        assert!(cloned.thread_id.is_none());
        assert_eq!(cloned.timestamp, 999);
    }

    #[tokio::test]
    async fn default_trait_methods_return_success() {
        let channel = DummyChannel;

        assert!(channel.health_check().await);
        assert!(channel.start_typing("bob").await.is_ok());
        assert!(channel.stop_typing("bob").await.is_ok());
        assert!(channel
            .send(&SendMessage::new("hello", "bob"))
            .await
            .is_ok());
    }

    #[tokio::test]
    async fn listen_sends_message_to_channel() {
        let channel = DummyChannel;
        let (tx, mut rx) = tokio::sync::mpsc::channel(1);

        channel.listen(tx).await.unwrap();

        let received = rx.recv().await.expect("message should be sent");
        assert_eq!(received.sender, "tester");
        assert_eq!(received.content, "hello");
        assert_eq!(received.channel, "dummy");
    }
}
