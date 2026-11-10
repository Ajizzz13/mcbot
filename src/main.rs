use azalea::ClientBuilder;
use azalea::JoinOpts;
use azalea::account::Account;
use azalea::prelude::*;
use azalea::protocol::address::ServerAddr;
use azalea::protocol::resolve::resolve_address;
use azalea::world::WorldName;
use eyre::Result;
use std::env;
use std::net::IpAddr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::net::TcpStream;
use tokio::net::lookup_host;
use tokio::time::timeout;
use tracing_subscriber::EnvFilter;

const AUTH_COOLDOWN: Duration = Duration::from_secs(15);

#[tokio::main]
async fn main() -> Result<()> {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::fmt().with_env_filter(filter).init();

    let username = env::var("MC_USERNAME").unwrap_or_else(|_| "mcbot".to_string());
    let server_str = env::var("MC_SERVER").unwrap_or_else(|_| "localhost".to_string());
    let password = env::var("MC_PASSWORD").ok().filter(|p| !p.is_empty());

    tracing::info!(%username, %server_str, has_password = password.is_some(), "starting bot");

    let account = Account::offline(&username);
    let opts = resolve_ipv4_fallback_opts(&server_str).await;
    let state = State {
        password,
        last_auth_at: Arc::new(Mutex::new(None)),
    };

    let _exit = ClientBuilder::new()
        .set_handler(handle)
        .set_state(state)
        .start_with_opts(account, server_str, opts)
        .await;

    Ok(())
}

async fn resolve_ipv4_fallback_opts(server_str: &str) -> JoinOpts {
    let Ok(addr) = ServerAddr::try_from(server_str) else {
        return JoinOpts::default();
    };
    if addr.host.parse::<IpAddr>().is_ok() {
        return JoinOpts::default();
    }

    let Ok(resolved) = resolve_address(&addr).await else {
        return JoinOpts::default();
    };
    if !resolved.is_ipv6() {
        return JoinOpts::default();
    }

    let ipv6_reachable = matches!(
        timeout(Duration::from_secs(2), TcpStream::connect(resolved)).await,
        Ok(Ok(_))
    );
    if ipv6_reachable {
        return JoinOpts::default();
    }

    if let Ok(addrs) = lookup_host((addr.host.as_str(), resolved.port())).await
        && let Some(v4) = addrs.into_iter().find(|a| a.is_ipv4())
    {
        tracing::info!(ipv6 = %resolved, ipv4 = %v4, "IPv6 unreachable, falling back to IPv4");
        return JoinOpts::new().custom_socket_addr(v4);
    }

    JoinOpts::default()
}

#[derive(Clone, Component, Default)]
pub struct State {
    password: Option<String>,
    last_auth_at: Arc<Mutex<Option<Instant>>>,
}

async fn handle(bot: Client, event: Event, state: State) -> Result<()> {
    match event {
        Event::Login => {
            if let Ok(mut guard) = state.last_auth_at.lock() {
                *guard = None;
            }
            tracing::info!(username = %bot.username(), "logged in");
        }
        Event::Spawn => {
            let dimension = bot
                .component::<WorldName>()
                .ok()
                .map(|w| w.to_string())
                .unwrap_or_else(|| "unknown".to_string());
            tracing::info!(username = %bot.username(), %dimension, "spawned in world");
        }
        Event::Disconnect(reason) => {
            if let Ok(mut guard) = state.last_auth_at.lock() {
                *guard = None;
            }
            let reason_text = reason
                .as_ref()
                .map(ToString::to_string)
                .unwrap_or_else(|| "none".to_string());
            tracing::warn!(reason = %reason_text, "disconnected");
        }
        Event::ConnectionFailed(err) => {
            tracing::warn!(%err, "connection failed");
        }
        Event::Chat(m) => {
            let (sender, content) = m.split_sender_and_content();
            tracing::info!(?sender, %content, "chat");

            match sender {
                None => {
                    if let Some(password) = &state.password
                        && let Some(cmd) = detect_auth_command(&content, password)
                    {
                        let should_send = state
                            .last_auth_at
                            .lock()
                            .map(|mut guard| {
                                let ready = guard.is_none_or(|t| t.elapsed() >= AUTH_COOLDOWN);
                                if ready {
                                    *guard = Some(Instant::now());
                                }
                                ready
                            })
                            .unwrap_or(false);

                        if should_send {
                            let verb = cmd.split_whitespace().next().unwrap_or("");
                            tracing::info!(command = %verb, "sending auth command");
                            bot.chat(cmd);
                        }
                    }
                }
                Some(sender) => {
                    if sender == bot.username()
                        || content.starts_with('/')
                        || content.starts_with("[echo] ")
                    {
                        return Ok(());
                    }
                    bot.chat(format!("[echo] {content}"));
                }
            }
        }
        _ => {}
    }
    Ok(())
}

fn detect_auth_command(system_msg: &str, password: &str) -> Option<String> {
    if password.is_empty() {
        return None;
    }
    let lower = system_msg.to_ascii_lowercase();
    if lower.contains("success")
        || lower.contains("berhasil")
        || lower.contains("logged in")
        || lower.contains("already")
    {
        return None;
    }
    if lower.contains("/register") {
        Some(format!("/register {password} {password}"))
    } else if lower.contains("/login") {
        Some(format!("/login {password}"))
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::detect_auth_command;

    #[test]
    fn detects_authme_register_prompt() {
        let prefix = "Please, register to the server with the command:";
        let msg = format!("{prefix} /register <password> <ConfirmPassword>");
        assert_eq!(
            detect_auth_command(&msg, "secret123"),
            Some("/register secret123 secret123".to_string())
        );
    }

    #[test]
    fn detects_authme_login_prompt() {
        let msg = "Please, login with the command: /login <password>";
        assert_eq!(
            detect_auth_command(msg, "secret123"),
            Some("/login secret123".to_string())
        );
    }

    #[test]
    fn ignores_auth_success_and_unrelated_messages() {
        assert_eq!(detect_auth_command("Successfully registered!", "secret123"), None);
        assert_eq!(detect_auth_command("You are already logged in.", "secret123"), None);
        assert_eq!(detect_auth_command("Welcome to the server!", "secret123"), None);
        assert_eq!(detect_auth_command("Use /register <pw>", ""), None);
    }
}
