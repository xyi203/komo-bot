use wechatbot::{BotOptions, WeChatBot};

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt::init();

    let bot = WeChatBot::new(BotOptions {
        on_qr_url: Some(Box::new(|url| {
            println!("\nScan this URL in WeChat:\n{}\n", url);
        })),
        on_error: Some(Box::new(|err| {
            eprintln!("Error: {}", err);
        })),
        ..Default::default()
    });

    let creds = bot.login(false).await.expect("login failed");
    println!("Logged in: {} ({})", creds.account_id, creds.user_id);

    bot.on_message(Box::new(|msg| {
        println!("[{}] {}: {}", msg.content_type_str(), msg.user_id, msg.text);
    }))
    .await;

    println!("Listening for messages (Ctrl+C to stop)");
    bot.run().await.expect("run failed");
}

trait ContentTypeStr {
    fn content_type_str(&self) -> &str;
}

impl ContentTypeStr for wechatbot::IncomingMessage {
    fn content_type_str(&self) -> &str {
        match self.content_type {
            wechatbot::ContentType::Text => "text",
            wechatbot::ContentType::Image => "image",
            wechatbot::ContentType::Voice => "voice",
            wechatbot::ContentType::File => "file",
            wechatbot::ContentType::Video => "video",
        }
    }
}
