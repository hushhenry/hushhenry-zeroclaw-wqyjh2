use std::fmt;

use crate::channels::traits::ChatType;

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
    pub chat_type: ChatType,
    pub sender_id: String,
    pub chat_id: String,
    pub thread_id: Option<String>,
}

impl SessionContext {
    #[deprecated(note = "renamed to chat_id; use `chat_id` instead")]
    pub fn conversation_id(&self) -> &str {
        &self.chat_id
    }
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
                context.channel, context.chat_id, thread_id
            ));
        }

        if matches!(context.chat_type, ChatType::Group | ChatType::Thread) {
            return SessionKey::new(format!("group:{}:{}", context.channel, context.chat_id));
        }

        SessionKey::new(format!("direct:{}:{}", context.channel, context.sender_id))
    }
}

#[cfg(test)]
mod tests {
    use super::{SessionContext, SessionResolver};
    use crate::channels::traits::ChatType;

    #[test]
    fn resolver_uses_thread_format_when_thread_is_present() {
        let resolver = SessionResolver::new();
        let context = SessionContext {
            channel: "telegram".into(),
            chat_type: ChatType::Group,
            sender_id: "user-1".into(),
            chat_id: "chat-42".into(),
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
            chat_type: ChatType::Group,
            sender_id: "user-2".into(),
            chat_id: "chan-88".into(),
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
            chat_type: ChatType::Direct,
            sender_id: "alice".into(),
            chat_id: "alice".into(),
            thread_id: None,
        };

        let key = resolver.resolve(&context);
        assert_eq!(key.as_str(), "direct:imessage:alice");
    }
}
