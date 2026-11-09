use azalea::ClientBuilder;
use azalea::account::Account;
use azalea::prelude::*;
use eyre::Result;
use std::env;
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .init();

    let username = env::var("MC_USERNAME").unwrap_or_else(|_| "mcbot".to_string());
    let server_str = env::var("MC_SERVER").unwrap_or_else(|_| "localhost".to_string());

    tracing::info!(%username, %server_str, "starting bot");

    let account = Account::offline(&username);

    ClientBuilder::new()
        .set_handler(handle)
        .start(account, server_str)
        .await
        .map_err(|e| eyre::eyre!(e))
}

#[derive(Clone, Component, Default)]
pub struct State {}

async fn handle(bot: Client, event: Event, _state: State) -> Result<()> {
    if let Event::Chat(m) = event
        && let (Some(sender), content) = m.split_sender_and_content()
    {
        if sender == bot.username() {
            return Ok(());
        }
        bot.chat(format!("[echo] {content}"));
    }
    Ok(())
}
