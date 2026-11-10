use azalea::ClientBuilder;
use azalea::JoinOpts;
use azalea::account::Account;
use azalea::ecs::query::With;
use azalea::ecs::query::Without;
use azalea::entity::Dead;
use azalea::entity::LocalEntity;
use azalea::entity::metadata::AbstractMonster;
use azalea::entity::metadata::Player;
use azalea::inventory::components::Food;
use azalea::local_player::Hunger;
use azalea::pathfinder::PathfinderClientExt;
use azalea::pathfinder::PathfinderOpts;
use azalea::pathfinder::astar::PathfinderTimeout;
use azalea::pathfinder::goals::RadiusGoal;
use azalea::pathfinder::goals::XZGoal;
use azalea::player::GameProfileComponent;
use azalea::prelude::*;
use azalea::protocol::address::ServerAddr;
use azalea::protocol::resolve::resolve_address;
use azalea::registry::builtin::EntityKind;
use azalea::world::WorldName;
use eyre::Result;
use std::env;
use std::net::IpAddr;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;
use std::time::Instant;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;
use tokio::net::TcpStream;
use tokio::net::lookup_host;
use tokio::time::timeout;
use tracing_subscriber::EnvFilter;

const DEFAULT_USERNAME: &str = "RakaPratama";
const AUTH_COOLDOWN: Duration = Duration::from_secs(15);
const AUTH_FALLBACK_DELAY: Duration = Duration::from_secs(12);
const AI_TICK_INTERVAL: Duration = Duration::from_millis(300);
const CHAT_COOLDOWN: Duration = Duration::from_secs(6);
const HOME_RADIUS: i32 = 18;
const MAX_WANDER_DURATION: Duration = Duration::from_secs(15);

#[tokio::main]
async fn main() -> Result<()> {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::fmt().with_env_filter(filter).init();

    let username = env::var("MC_USERNAME")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| DEFAULT_USERNAME.to_string());
    let server_str = env::var("MC_SERVER").unwrap_or_else(|_| "localhost".to_string());
    let password = env::var("MC_PASSWORD").ok().filter(|p| !p.is_empty());

    tracing::info!(
        %username,
        %server_str,
        has_password = password.is_some(),
        "starting bot"
    );

    let account = Account::offline(&username);
    let opts = resolve_ipv4_fallback_opts(&server_str).await;
    let state = State::new(password);

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
        tracing::info!(
            ipv6 = %resolved,
            ipv4 = %v4,
            "IPv6 unreachable, falling back to IPv4"
        );
        return JoinOpts::new().custom_socket_addr(v4);
    }

    JoinOpts::default()
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum BotMode {
    Auto,
    Follow(String),
    Stay,
}

impl BotMode {
    fn label(&self) -> String {
        match self {
            BotMode::Auto => "auto".to_string(),
            BotMode::Follow(name) => format!("ikut {name}"),
            BotMode::Stay => "diam".to_string(),
        }
    }
}

struct AiRuntime {
    authenticated: bool,
    mode: BotMode,
    home_pos: Option<BlockPos>,
    spawned_at: Option<Instant>,
    last_auth_at: Option<Instant>,
    last_tick_at: Option<Instant>,
    last_eat_at: Option<Instant>,
    last_escape_at: Option<Instant>,
    last_follow_path_at: Option<Instant>,
    last_chat_at: Option<Instant>,
    last_crouch_tap_at: Option<Instant>,
    crouch_release_at: Option<Instant>,
    wander_started_at: Option<Instant>,
    next_idle_at: Instant,
    rng_seed: u64,
}

impl AiRuntime {
    fn new(has_password: bool) -> Self {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0x5EED_CAFE_BABE_1234);
        Self {
            authenticated: !has_password,
            mode: BotMode::Auto,
            home_pos: None,
            spawned_at: None,
            last_auth_at: None,
            last_tick_at: None,
            last_eat_at: None,
            last_escape_at: None,
            last_follow_path_at: None,
            last_chat_at: None,
            last_crouch_tap_at: None,
            crouch_release_at: None,
            wander_started_at: None,
            next_idle_at: Instant::now() + Duration::from_secs(4),
            rng_seed: nanos ^ 0x9E37_79B9_7F4A_7C15,
        }
    }

    fn next_u32(&mut self) -> u32 {
        self.rng_seed = self
            .rng_seed
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        (self.rng_seed >> 32) as u32
    }

    fn rand_range(&mut self, min: i32, max_inclusive: i32) -> i32 {
        let span = (max_inclusive - min + 1).max(1) as u32;
        min + (self.next_u32() % span) as i32
    }
}

#[derive(Clone, Component)]
pub struct State {
    password: Option<String>,
    runtime: Arc<Mutex<AiRuntime>>,
}

impl Default for State {
    fn default() -> Self {
        Self::new(None)
    }
}

impl State {
    fn new(password: Option<String>) -> Self {
        let has_password = password.is_some();
        Self {
            password,
            runtime: Arc::new(Mutex::new(AiRuntime::new(has_password))),
        }
    }
}

async fn handle(bot: Client, event: Event, state: State) -> Result<()> {
    match event {
        Event::Login => {
            if let Ok(mut rt) = state.runtime.lock() {
                rt.authenticated = state.password.is_none();
                rt.last_auth_at = None;
                rt.spawned_at = None;
                rt.last_tick_at = None;
                rt.wander_started_at = None;
                rt.crouch_release_at = None;
            }
            tracing::info!(username = %bot.username(), "logged in");
        }
        Event::Spawn => {
            let dimension = bot
                .component::<WorldName>()
                .ok()
                .map(|w| w.to_string())
                .unwrap_or_else(|| "unknown".to_string());
            let pos = bot.position().ok().map(|p| BlockPos::from(p.up(0.5)));
            if let Ok(mut rt) = state.runtime.lock() {
                rt.spawned_at = Some(Instant::now());
                rt.next_idle_at = Instant::now() + Duration::from_secs(4);
                if rt.home_pos.is_none() {
                    rt.home_pos = pos;
                }
                if state.password.is_none() {
                    rt.authenticated = true;
                }
            }
            tracing::info!(username = %bot.username(), %dimension, ?pos, "spawned in world");
        }
        Event::Disconnect(reason) => {
            if let Ok(mut rt) = state.runtime.lock() {
                rt.authenticated = state.password.is_none();
                rt.last_auth_at = None;
                rt.spawned_at = None;
                rt.last_tick_at = None;
                rt.wander_started_at = None;
                rt.crouch_release_at = None;
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
                None => handle_system_chat(&bot, &state, &content),
                Some(sender) => handle_player_chat(&bot, &state, &sender, &content),
            }
        }
        Event::Tick => run_ai_tick(&bot, &state),
        _ => {}
    }
    Ok(())
}

fn handle_system_chat(bot: &Client, state: &State, content: &str) {
    if is_auth_success(content)
        && let Ok(mut rt) = state.runtime.lock()
    {
        if !rt.authenticated {
            tracing::info!("auth confirmed, AI loop enabled");
        }
        rt.authenticated = true;
    }

    let Some(password) = &state.password else {
        return;
    };
    let Some(cmd) = detect_auth_command(content, password) else {
        return;
    };

    let should_send = state
        .runtime
        .lock()
        .map(|mut rt| {
            let ready = rt
                .last_auth_at
                .is_none_or(|t| t.elapsed() >= AUTH_COOLDOWN);
            if ready {
                rt.last_auth_at = Some(Instant::now());
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

#[derive(Debug, PartialEq, Eq)]
enum ChatAction {
    Reply(String),
    Status,
    Come,
    Follow,
    Stay,
    Wander,
}

fn handle_player_chat(bot: &Client, state: &State, sender: &str, content: &str) {
    let my_name = bot.username();
    if sender == my_name || content.starts_with("[echo] ") {
        return;
    }

    let is_authed = state
        .runtime
        .lock()
        .map(|rt| rt.authenticated)
        .unwrap_or(false);
    if !is_authed {
        return;
    }

    let Some(action) = parse_chat_action(content, &my_name) else {
        return;
    };

    match action {
        ChatAction::Reply(msg) => {
            if can_send_chat(state) {
                bot.chat(msg);
            }
        }
        ChatAction::Status => {
            if can_send_chat(state) {
                let msg = format_status_line(bot, state);
                bot.chat(msg);
            }
        }
        ChatAction::Come => {
            let target_pos = find_player_pos(bot, sender);
            if let Ok(mut rt) = state.runtime.lock() {
                rt.mode = BotMode::Auto;
                if let Some(pos) = target_pos {
                    rt.home_pos = Some(BlockPos::from(pos.up(0.5)));
                }
            }
            if let Some(pos) = target_pos {
                bot.start_goto_with_opts(
                    RadiusGoal::new(pos, 2.5),
                    PathfinderOpts::new()
                        .allow_mining(false)
                        .max_timeout(PathfinderTimeout::Time(Duration::from_secs(3))),
                );
                if can_send_chat(state) {
                    bot.chat("oke otw");
                }
            } else if can_send_chat(state) {
                bot.chat("posisimu kejauhan");
            }
        }
        ChatAction::Follow => {
            if let Ok(mut rt) = state.runtime.lock() {
                rt.mode = BotMode::Follow(sender.to_string());
            }
            if can_send_chat(state) {
                bot.chat("oke gue ikut");
            }
        }
        ChatAction::Stay => {
            if let Ok(mut rt) = state.runtime.lock() {
                rt.mode = BotMode::Stay;
                rt.wander_started_at = None;
            }
            bot.stop_pathfinding();
            if can_send_chat(state) {
                bot.chat("oke diam di sini");
            }
        }
        ChatAction::Wander => {
            let cur_pos = bot.position().ok().map(|p| BlockPos::from(p.up(0.5)));
            if let Ok(mut rt) = state.runtime.lock() {
                rt.mode = BotMode::Auto;
                if cur_pos.is_some() {
                    rt.home_pos = cur_pos;
                }
                rt.next_idle_at = Instant::now();
            }
            if can_send_chat(state) {
                bot.chat("oke lanjut jalan");
            }
        }
    }
}

fn can_send_chat(state: &State) -> bool {
    state
        .runtime
        .lock()
        .map(|mut rt| {
            let ready = rt
                .last_chat_at
                .is_none_or(|t| t.elapsed() >= CHAT_COOLDOWN);
            if ready {
                rt.last_chat_at = Some(Instant::now());
            }
            ready
        })
        .unwrap_or(false)
}

fn format_status_line(bot: &Client, state: &State) -> String {
    let hp = bot.health().unwrap_or(0.0).round() as i32;
    let food = bot.component::<Hunger>().map(|h| h.food).unwrap_or(20);
    let pos = bot.position().map_or_else(
        |_| "?".to_string(),
        |p| {
            let (x, y, z) = (p.x.floor() as i32, p.y.floor() as i32, p.z.floor() as i32);
            format!("{x} {y} {z}")
        },
    );
    let mode = state
        .runtime
        .lock()
        .map(|rt| rt.mode.label())
        .unwrap_or_else(|_| "auto".to_string());
    format!("HP {hp}/20 | lapar {food}/20 | mode: {mode} | pos: {pos}")
}

fn find_player_pos(bot: &Client, username: &str) -> Option<azalea::Vec3> {
    let entity = bot
        .any_entity_by::<&GameProfileComponent, (With<Player>, Without<LocalEntity>)>(
            |p: &GameProfileComponent| p.name.eq_ignore_ascii_case(username),
        )
        .ok()??;
    entity.position().ok()
}

fn run_ai_tick(bot: &Client, state: &State) {
    let (authenticated, mode, home_pos, release_crouch, should_run) = {
        let Ok(mut rt) = state.runtime.lock() else {
            return;
        };
        if !rt.authenticated
            && rt
                .spawned_at
                .is_some_and(|t| t.elapsed() >= AUTH_FALLBACK_DELAY)
        {
            tracing::info!("auth timeout fallback reached, enabling AI loop");
            rt.authenticated = true;
        }
        let release_crouch = rt
            .crouch_release_at
            .is_some_and(|until| Instant::now() >= until);
        if release_crouch {
            rt.crouch_release_at = None;
        }

        let ready = rt
            .last_tick_at
            .is_none_or(|t| t.elapsed() >= AI_TICK_INTERVAL);
        if ready {
            rt.last_tick_at = Some(Instant::now());
        }
        (
            rt.authenticated,
            rt.mode.clone(),
            rt.home_pos,
            release_crouch,
            ready,
        )
    };

    if release_crouch {
        let _ = bot.set_crouching(false);
    }

    if !should_run || !authenticated || !bot.is_alive() {
        return;
    }

    if try_eat_food(bot, state) {
        return;
    }

    if handle_threats(bot, state, &mode) {
        return;
    }

    match mode {
        BotMode::Follow(target_name) => handle_follow(bot, state, &target_name),
        BotMode::Stay => handle_stay_look(bot, state),
        BotMode::Auto => handle_auto_idle(bot, state, home_pos),
    }
}

fn try_eat_food(bot: &Client, state: &State) -> bool {
    let hunger_food = bot.component::<Hunger>().map(|h| h.food).unwrap_or(20);
    let hp = bot.health().unwrap_or(20.0);
    if hunger_food > 16 && !(hp < 18.0 && hunger_food < 20) {
        return false;
    }

    let eat_ready = state
        .runtime
        .lock()
        .map(|rt| {
            rt.last_eat_at
                .is_none_or(|t| t.elapsed() >= Duration::from_secs(3))
        })
        .unwrap_or(false);
    if !eat_ready {
        return false;
    }

    let Ok(menu) = bot.menu() else {
        return false;
    };
    let hotbar_range = menu.hotbar_slots_range();
    let slots = menu.slots();
    let food_slot = slots[hotbar_range]
        .iter()
        .position(|stack| stack.get_component::<Food>().is_some());

    let Some(slot) = food_slot else {
        return false;
    };

    if let Ok(mut rt) = state.runtime.lock() {
        rt.last_eat_at = Some(Instant::now());
    }
    bot.set_selected_hotbar_slot(slot as u8);
    bot.start_use_item();
    true
}

fn handle_threats(bot: &Client, state: &State, mode: &BotMode) -> bool {
    let Ok(monsters) = bot.nearest_entities::<(With<AbstractMonster>, Without<Dead>)>() else {
        return false;
    };
    let Some(nearest) = monsters.first() else {
        return false;
    };
    let Ok(dist) = nearest.distance_to_client() else {
        return false;
    };
    let kind = nearest.kind().ok();

    if kind == Some(EntityKind::Creeper) && dist < 6.0 {
        let should_escape = state
            .runtime
            .lock()
            .map(|mut rt| {
                let ready = rt
                    .last_escape_at
                    .is_none_or(|t| t.elapsed() >= Duration::from_secs(2));
                if ready {
                    rt.last_escape_at = Some(Instant::now());
                }
                ready
            })
            .unwrap_or(false);
        if should_escape
            && let (Ok(my_pos), Ok(creeper_pos)) = (bot.position(), nearest.position())
        {
            let dx = my_pos.x - creeper_pos.x;
            let dz = my_pos.z - creeper_pos.z;
            let len = (dx * dx + dz * dz).sqrt().max(0.01);
            let target_x = (my_pos.x + (dx / len) * 7.0).floor() as i32;
            let target_z = (my_pos.z + (dz / len) * 7.0).floor() as i32;
            bot.start_goto_with_opts(
                XZGoal {
                    x: target_x,
                    z: target_z,
                },
                PathfinderOpts::new()
                    .allow_mining(false)
                    .max_timeout(PathfinderTimeout::Time(Duration::from_secs(2))),
            );
        }
        return true;
    }

    if dist <= 4.2 {
        if *mode == BotMode::Auto && bot.is_executing_path() {
            bot.stop_pathfinding();
            if let Ok(mut rt) = state.runtime.lock() {
                rt.wander_started_at = None;
            }
        }
        let _ = nearest.look_at();
        if !bot.has_attack_cooldown() {
            nearest.attack();
        }
        return true;
    }

    false
}

fn handle_follow(bot: &Client, state: &State, target_name: &str) {
    let target = bot
        .any_entity_by::<&GameProfileComponent, (With<Player>, Without<LocalEntity>)>(
            |p: &GameProfileComponent| p.name.eq_ignore_ascii_case(target_name),
        )
        .ok()
        .flatten();
    let Some(target) = target else {
        return;
    };
    if !target.is_alive() {
        return;
    }
    let Ok(dist) = target.distance_to_client() else {
        return;
    };
    if dist <= 12.0 {
        let _ = target.look_at();
    }
    if dist > 4.0 && !bot.is_calculating_path() {
        let ready = state
            .runtime
            .lock()
            .map(|mut rt| {
                let ok = rt
                    .last_follow_path_at
                    .is_none_or(|t| t.elapsed() >= Duration::from_millis(1500));
                if ok {
                    rt.last_follow_path_at = Some(Instant::now());
                }
                ok
            })
            .unwrap_or(false);
        if ready && let Ok(tpos) = target.position() {
            bot.start_goto_with_opts(
                RadiusGoal::new(tpos, 2.5),
                PathfinderOpts::new()
                    .allow_mining(false)
                    .max_timeout(PathfinderTimeout::Time(Duration::from_secs(3))),
            );
        }
    } else if dist <= 3.0 && bot.is_executing_path() {
        bot.stop_pathfinding();
    }
}

fn handle_stay_look(bot: &Client, state: &State) {
    if let Some(player) = nearest_alive_player(bot, 9.0) {
        let _ = player.look_at();
        return;
    }
    let look_angles = state.runtime.lock().ok().and_then(|mut rt| {
        if Instant::now() < rt.next_idle_at {
            return None;
        }
        let secs = rt.rand_range(8, 16) as u64;
        rt.next_idle_at = Instant::now() + Duration::from_secs(secs);
        let yaw = rt.rand_range(-180, 180) as f32;
        let pitch = rt.rand_range(-10, 10) as f32;
        Some((yaw, pitch))
    });
    if let Some((yaw, pitch)) = look_angles {
        let _ = bot.set_direction(yaw, pitch);
    }
}

fn handle_auto_idle(bot: &Client, state: &State, home_pos: Option<BlockPos>) {
    if bot.is_executing_path() || bot.is_calculating_path() {
        let timed_out = state
            .runtime
            .lock()
            .map(|mut rt| {
                let expired = rt
                    .wander_started_at
                    .is_some_and(|t| t.elapsed() >= MAX_WANDER_DURATION);
                if expired {
                    rt.wander_started_at = None;
                }
                expired
            })
            .unwrap_or(false);
        if timed_out {
            bot.stop_pathfinding();
        }
        return;
    }

    if let Some(player) = nearest_alive_player(bot, 8.0) {
        let _ = player.look_at();
        let tap_crouch = state
            .runtime
            .lock()
            .map(|mut rt| {
                let dist_close = player.distance_to_client().is_ok_and(|d| d <= 5.0);
                let cooldown_ok = rt
                    .last_crouch_tap_at
                    .is_none_or(|t| t.elapsed() >= Duration::from_secs(20));
                if dist_close && cooldown_ok {
                    rt.last_crouch_tap_at = Some(Instant::now());
                    rt.crouch_release_at = Some(Instant::now() + Duration::from_millis(350));
                    true
                } else {
                    false
                }
            })
            .unwrap_or(false);
        if tap_crouch {
            let _ = bot.set_crouching(true);
        }
    }

    enum IdleChoice {
        Wait,
        Wander(i32, i32),
        Look(f32, f32),
        JumpLook(f32, f32),
    }

    let Ok(cur_pos) = bot.position() else {
        return;
    };
    let anchor = home_pos.unwrap_or_else(|| BlockPos::from(cur_pos.up(0.5)));

    let choice = state
        .runtime
        .lock()
        .map(|mut rt| {
            if Instant::now() < rt.next_idle_at {
                return IdleChoice::Wait;
            }
            let delay = rt.rand_range(6, 15) as u64;
            rt.next_idle_at = Instant::now() + Duration::from_secs(delay);

            let roll = rt.rand_range(0, 99);
            if roll < 60 {
                let dx = rt.rand_range(-9, 9);
                let dz = rt.rand_range(-9, 9);
                let raw_x = cur_pos.x.floor() as i32 + dx;
                let raw_z = cur_pos.z.floor() as i32 + dz;
                let clamped_x = raw_x.clamp(anchor.x - HOME_RADIUS, anchor.x + HOME_RADIUS);
                let clamped_z = raw_z.clamp(anchor.z - HOME_RADIUS, anchor.z + HOME_RADIUS);
                rt.wander_started_at = Some(Instant::now());
                IdleChoice::Wander(clamped_x, clamped_z)
            } else if roll < 85 {
                let yaw = rt.rand_range(-180, 180) as f32;
                let pitch = rt.rand_range(-12, 12) as f32;
                IdleChoice::Look(yaw, pitch)
            } else {
                let yaw = rt.rand_range(-180, 180) as f32;
                let pitch = rt.rand_range(-8, 8) as f32;
                IdleChoice::JumpLook(yaw, pitch)
            }
        })
        .unwrap_or(IdleChoice::Wait);

    match choice {
        IdleChoice::Wait => {}
        IdleChoice::Wander(x, z) => {
            bot.start_goto_with_opts(
                XZGoal { x, z },
                PathfinderOpts::new()
                    .allow_mining(false)
                    .max_timeout(PathfinderTimeout::Time(Duration::from_secs(3))),
            );
        }
        IdleChoice::Look(yaw, pitch) => {
            let _ = bot.set_direction(yaw, pitch);
        }
        IdleChoice::JumpLook(yaw, pitch) => {
            let _ = bot.set_direction(yaw, pitch);
            bot.jump();
        }
    }
}

fn nearest_alive_player(bot: &Client, max_dist: f64) -> Option<azalea::EntityRef> {
    let players = bot.nearby_players().ok()?;
    players
        .iter()
        .find(|p| p.is_alive() && p.distance_to_client().is_ok_and(|d| d <= max_dist))
        .cloned()
}

fn is_auth_success(system_msg: &str) -> bool {
    let lower = system_msg.to_ascii_lowercase();
    lower.contains("successfully registered")
        || lower.contains("successful login")
        || lower.contains("logged in")
        || lower.contains("berhasil login")
        || lower.contains("berhasil daftar")
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

fn parse_chat_action(content: &str, bot_username: &str) -> Option<ChatAction> {
    let trimmed = content.trim();
    if trimmed.is_empty() {
        return None;
    }
    let lower = trimmed.to_ascii_lowercase();

    match lower.as_str() {
        "!help" | "!bantuan" => {
            return Some(ChatAction::Reply(
                "cmd: !status, !sini, !ikut, !diam, !jalan".to_string(),
            ));
        }
        "!status" => return Some(ChatAction::Status),
        "!sini" | "!come" => return Some(ChatAction::Come),
        "!ikut" | "!follow" => return Some(ChatAction::Follow),
        "!diam" | "!stop" | "!stay" => return Some(ChatAction::Stay),
        "!jalan" | "!auto" | "!wander" => return Some(ChatAction::Wander),
        _ => {}
    }

    if lower.starts_with('/') || lower.starts_with('!') {
        return None;
    }

    let bot_lower = bot_username.to_ascii_lowercase();
    let short_name = "raka";
    let mentioned = lower.contains(&bot_lower)
        || lower
            .split(|c: char| !c.is_ascii_alphanumeric())
            .any(|w| w == short_name);

    let first_word = lower
        .split(|c: char| !c.is_ascii_alphanumeric())
        .next()
        .unwrap_or("");
    let is_greeting = matches!(
        first_word,
        "halo" | "hai" | "hi" | "hello" | "pagi" | "siang" | "sore" | "malam" | "permisi"
    );

    if is_greeting && (mentioned || lower.split_whitespace().count() <= 3) {
        return Some(ChatAction::Reply("halo juga".to_string()));
    }

    if !mentioned {
        return None;
    }

    if lower.contains("sini") || lower.contains("kesini") {
        Some(ChatAction::Come)
    } else if lower.contains("ikut") {
        Some(ChatAction::Follow)
    } else if lower.contains("diam") || lower.contains("stop") {
        Some(ChatAction::Stay)
    } else if lower.contains("jalan") || lower.contains("muter") {
        Some(ChatAction::Wander)
    } else if lower.contains("lagi apa") || lower.contains("ngapain") {
        Some(ChatAction::Reply("lagi keliling santai aja".to_string()))
    } else if lower.contains("status") || lower.contains("aman") || lower.contains("hidup") {
        Some(ChatAction::Status)
    } else {
        Some(ChatAction::Reply("iya kenapa?".to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::ChatAction;
    use super::DEFAULT_USERNAME;
    use super::detect_auth_command;
    use super::is_auth_success;
    use super::parse_chat_action;

    #[test]
    fn default_username_is_human_like_and_valid() {
        assert_ne!(DEFAULT_USERNAME, "mcbot");
        assert!((3..=16).contains(&DEFAULT_USERNAME.len()));
        assert!(
            DEFAULT_USERNAME
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_')
        );
    }

    #[test]
    fn detects_authme_register_and_login_prompts() {
        let prefix = "Please, register to the server with the command:";
        let reg_msg = format!("{prefix} /register <password> <ConfirmPassword>");
        assert_eq!(
            detect_auth_command(&reg_msg, "secret123"),
            Some("/register secret123 secret123".to_string())
        );
        assert_eq!(
            detect_auth_command(
                "Please, login with the command: /login <password>",
                "secret123"
            ),
            Some("/login secret123".to_string())
        );
        assert!(is_auth_success("Successfully registered!"));
        assert!(is_auth_success("Successful login!"));
    }

    #[test]
    fn ignores_auth_success_and_unrelated_messages() {
        assert_eq!(
            detect_auth_command("Successfully registered!", "secret123"),
            None
        );
        assert_eq!(
            detect_auth_command("You are already logged in.", "secret123"),
            None
        );
        assert_eq!(
            detect_auth_command("Welcome to the server!", "secret123"),
            None
        );
        assert_eq!(detect_auth_command("Use /register <pw>", ""), None);
    }

    #[test]
    fn parses_commands_and_natural_mentions() {
        assert_eq!(
            parse_chat_action("!status", DEFAULT_USERNAME),
            Some(ChatAction::Status)
        );
        assert_eq!(
            parse_chat_action("!ikut", DEFAULT_USERNAME),
            Some(ChatAction::Follow)
        );
        assert_eq!(
            parse_chat_action("!sini", DEFAULT_USERNAME),
            Some(ChatAction::Come)
        );
        assert_eq!(
            parse_chat_action("!diam", DEFAULT_USERNAME),
            Some(ChatAction::Stay)
        );
        assert_eq!(
            parse_chat_action("!jalan", DEFAULT_USERNAME),
            Some(ChatAction::Wander)
        );
        assert_eq!(
            parse_chat_action("halo raka", DEFAULT_USERNAME),
            Some(ChatAction::Reply("halo juga".to_string()))
        );
        assert_eq!(
            parse_chat_action("raka lagi ngapain?", DEFAULT_USERNAME),
            Some(ChatAction::Reply("lagi keliling santai aja".to_string()))
        );
        assert_eq!(
            parse_chat_action("guys ayo mining diamond", DEFAULT_USERNAME),
            None
        );
    }
}
