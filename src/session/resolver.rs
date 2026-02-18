use std::fmt;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct SessionKey(String);

impl SessionKey {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for SessionKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

#[derive(Debug, Clone)]
pub struct SessionContext {
    pub channel: String,
    pub chat_type: String,
    pub sender_id: String,
    pub conversation_id: String,
    pub thread_id: Option<String>,
}

#[derive(Debug, Default, Clone)]
pub struct SessionResolver;

impl SessionResolver {
    pub fn new() -> Self {
        Self
    }

    pub fn resolve(&self, context: &SessionContext) -> SessionKey {
        if let Some(thread_id) = context.thread_id.as_deref() {
            return SessionKey::new(format!(
                "thread:{}:{}:{}",
                context.channel, context.conversation_id, thread_id
            ));
        }

        let chat_type = context.chat_type.to_ascii_lowercase();
        if matches!(chat_type.as_str(), "group" | "channel" | "supergroup") {
            return SessionKey::new(format!(
                "group:{}:{}",
                context.channel, context.conversation_id
            ));
        }

        SessionKey::new(format!("direct:{}:{}", context.channel, context.sender_id))
    }
}

#[cfg(test)]
mod tests {
    use super::{SessionContext, SessionResolver};

    #[test]
    fn resolver_uses_thread_format_when_thread_is_present() {
        let resolver = SessionResolver::new();
        let context = SessionContext {
            channel: "telegram".into(),
            chat_type: "group".into(),
            sender_id: "user-1".into(),
            conversation_id: "chat-42".into(),
            thread_id: Some("17".into()),
        };

        let key = resolver.resolve(&context);
        assert_eq!(key.as_str(), "thread:telegram:chat-42:17");
    }

    #[test]
    fn resolver_uses_group_format_for_group_chat_types() {
        let resolver = SessionResolver::new();
        let context = SessionContext {
            channel: "discord".into(),
            chat_type: "channel".into(),
            sender_id: "user-2".into(),
            conversation_id: "chan-88".into(),
            thread_id: None,
        };

        let key = resolver.resolve(&context);
        assert_eq!(key.as_str(), "group:discord:chan-88");
    }

    #[test]
    fn resolver_uses_direct_format_for_non_group_chats() {
        let resolver = SessionResolver::new();
        let context = SessionContext {
            channel: "imessage".into(),
            chat_type: "direct".into(),
            sender_id: "alice".into(),
            conversation_id: "alice".into(),
            thread_id: None,
        };

        let key = resolver.resolve(&context);
        assert_eq!(key.as_str(), "direct:imessage:alice");
    }
}
