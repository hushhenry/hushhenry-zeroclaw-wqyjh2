use super::traits::{Channel, ChannelMessage, ChannelMessageIngress, ChatType, SendMessage};
use async_trait::async_trait;
use tokio::io::{self, AsyncBufReadExt, BufReader};
use uuid::Uuid;

/// CLI channel — stdin/stdout, always available, zero deps
pub struct CliChannel;

impl CliChannel {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl Channel for CliChannel {
    fn name(&self) -> &str {
        "cli"
    }

    async fn send(&self, message: &SendMessage) -> anyhow::Result<()> {
        println!("{}", message.content);
        Ok(())
    }

    async fn listen(&self, tx: tokio::sync::mpsc::Sender<ChannelMessage>) -> anyhow::Result<()> {
        let stdin = io::stdin();
        let reader = BufReader::new(stdin);
        let mut lines = reader.lines();

        while let Ok(Some(line)) = lines.next_line().await {
            let line = line.trim().to_string();
            if line.is_empty() {
                continue;
            }
            if line == "/quit" || line == "/exit" {
                break;
            }

            let msg = ChannelMessage::new_ingress(ChannelMessageIngress {
                id: Uuid::new_v4().to_string(),
                account_id: None,
                sender: "user".to_string(),
                reply_target: "user".to_string(),
                content: line,
                channel: "cli".to_string(),
                title: None,
                chat_type: ChatType::Direct,
                raw_chat_type: None,
                chat_id: "user".to_string(),
                thread_id: None,
                timestamp: std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs(),
            });

            if tx.send(msg).await.is_err() {
                break;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cli_channel_name() {
        assert_eq!(CliChannel::new().name(), "cli");
    }

    #[tokio::test]
    async fn cli_channel_send_does_not_panic() {
        let ch = CliChannel::new();
        let result = ch
            .send(&SendMessage {
                content: "hello".into(),
                recipient: "user".into(),
                subject: None,
            })
            .await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn cli_channel_send_empty_message() {
        let ch = CliChannel::new();
        let result = ch
            .send(&SendMessage {
                content: String::new(),
                recipient: String::new(),
                subject: None,
            })
            .await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn cli_channel_health_check() {
        let ch = CliChannel::new();
        assert!(ch.health_check().await);
    }

    #[test]
    fn channel_message_struct() {
        let msg = ChannelMessage::new_ingress(ChannelMessageIngress {
            id: "test-id".to_string(),
            account_id: None,
            sender: "user".to_string(),
            reply_target: "user".to_string(),
            content: "hello".to_string(),
            channel: "cli".to_string(),
            title: None,
            chat_type: ChatType::Direct,
            raw_chat_type: None,
            chat_id: "user".to_string(),
            thread_id: None,
            timestamp: 1_234_567_890,
        });
        assert_eq!(msg.id, "test-id");
        assert_eq!(msg.sender, "user");
        assert_eq!(msg.reply_target, "user");
        assert_eq!(msg.content, "hello");
        assert_eq!(msg.channel, "cli");
        assert_eq!(msg.timestamp, 1_234_567_890);
    }

    #[test]
    fn channel_message_clone() {
        let msg = ChannelMessage::new_ingress(ChannelMessageIngress {
            id: "id".to_string(),
            account_id: None,
            sender: "s".to_string(),
            reply_target: "s".to_string(),
            content: "c".to_string(),
            channel: "ch".to_string(),
            title: None,
            chat_type: ChatType::Direct,
            raw_chat_type: None,
            chat_id: "s".to_string(),
            thread_id: None,
            timestamp: 0,
        });
        let cloned = msg.clone();
        assert_eq!(cloned.id, msg.id);
        assert_eq!(cloned.content, msg.content);
    }
}
