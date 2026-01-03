use matrix_sdk::{
    Client,
    config::SyncSettings,
    room::Room,
    ruma::events::room::message::{MessageType, OriginalSyncRoomMessageEvent, RoomMessageEventContent, Relation},
};
use irc::client::prelude::*;
use futures::StreamExt;
use tokio::sync::mpsc;
use log::{info, error};
use std::env;
use dotenv::dotenv;
use std::time::Duration;
use url::Url;
use std::collections::{HashSet, HashMap};
use std::sync::{Arc, Mutex};
use std::path::Path;
use std::fs;
use std::io::{BufRead, BufReader, Write};
use tokio::task::JoinSet;

#[derive(Clone, Debug)]
struct BridgeConfig {
    matrix_room_id: String,
    irc_channel: String,
}

fn truncate_at_char_boundary(s: &str, max_bytes: usize) -> &str {
    if s.len() <= max_bytes {
        return s;
    }

    let mut idx = max_bytes;
    while idx > 0 && !s.is_char_boundary(idx) {
        idx -= 1;
    }

    &s[..idx]
}

fn load_bridge_configs() -> Result<Vec<BridgeConfig>, Box<dyn std::error::Error>> {
    let mut configs = Vec::new();
    let mut i = 1;

    loop {
        let room_key = format!("BRIDGE_{}_MATRIX_ROOM_ID", i);
        let channel_key = format!("BRIDGE_{}_IRC_CHANNEL", i);

        match (env::var(&room_key), env::var(&channel_key)) {
            (Ok(room), Ok(channel)) => {
                info!("Loaded bridge config {}: {} <-> {}", i, room, channel);
                configs.push(BridgeConfig {
                    matrix_room_id: room,
                    irc_channel: channel,
                });
                i += 1;
            }
            _ => break,
        }
    }

    if configs.is_empty() {
        return Err("No bridge configurations found. Set BRIDGE_1_MATRIX_ROOM_ID and BRIDGE_1_IRC_CHANNEL environment variables".into());
    }

    Ok(configs)
}

#[tokio::main(flavor = "multi_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    env_logger::init();

    dotenv().ok();

    let access_token = env::var("MATRIX_ACCESS_TOKEN")
    .map_err(|_| "MATRIX_ACCESS_TOKEN environment variable not set")?;

    let matrix_user_id = env::var("MATRIX_USER_ID")
    .map_err(|_| "MATRIX_USER_ID environment variable not set")?;

    let matrix_device_id = env::var("MATRIX_DEVICE_ID").ok();

    let matrix_homeserver = env::var("MATRIX_HOMESERVER")
    .map_err(|_| "MATRIX_HOMESERVER environment variable not set (e.g., https://matrix.org)")?;

    let irc_server_password = env::var("IRC_SERVER_PASSWORD").ok();

    let irc_server = env::var("IRC_SERVER")
    .map_err(|_| "IRC_SERVER environment variable not set")?;
    let irc_port = env::var("IRC_PORT")
    .map_err(|_| "IRC_PORT environment variable not set")?
    .parse::<u16>()
    .map_err(|_| "IRC_PORT must be a valid port number")?;
    let irc_use_tls = env::var("IRC_USE_TLS")
    .map_err(|_| "IRC_USE_TLS environment variable not set (true/false)")?
    .parse::<bool>()
    .map_err(|_| "IRC_USE_TLS must be true or false")?;

    let bridge_configs = load_bridge_configs()?;
    info!("Loaded {} bridge configuration(s)", bridge_configs.len());

    let (matrix_tx, matrix_rx) = mpsc::channel::<(String, String, String)>(100);
    let (irc_send_tx, irc_send_rx) = mpsc::channel::<(String, String, String)>(100);

    let bridge_configs_for_matrix = bridge_configs.clone();
    let matrix_handle = tokio::spawn(async move {
        if let Err(e) = run_matrix(
            matrix_rx,
            irc_send_tx,
            access_token,
            matrix_user_id,
            matrix_device_id,
            matrix_homeserver,
            bridge_configs_for_matrix,
        ).await {
            error!("Matrix client error: {:?}", e);
        }
    });

    let irc_handle = tokio::spawn(async move {
        if let Err(e) = run_irc_manager(
            matrix_tx,
            irc_send_rx,
            irc_server_password,
            irc_server,
            irc_port,
            irc_use_tls,
            bridge_configs,
        ).await {
            error!("IRC manager error: {:?}", e);
        }
    });

    tokio::select! {
        _ = matrix_handle => {},
        _ = irc_handle => {},
        _ = tokio::signal::ctrl_c() => {
            info!("Received Ctrl+C, shutting down...");
        }
    }

    Ok(())
}

fn load_processed_events(db_path: &Path) -> HashSet<String> {
    let mut events = HashSet::new();

    if db_path.exists() {
        match fs::File::open(db_path) {
            Ok(file) => {
                let reader = BufReader::new(file);
                for line in reader.lines().flatten() {
                    events.insert(line.trim().to_string());
                }
                info!("Loaded {} processed events from database", events.len());
            }
            Err(e) => {
                error!("Failed to open database file: {:?}", e);
            }
        }
    } else {
        info!("No existing database found, starting fresh");
    }

    events
}

fn save_processed_event(db_path: &Path, event_id: &str) {
    if let Ok(mut file) = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(db_path)
        {
            if let Err(e) = writeln!(file, "{}", event_id) {
                error!("Failed to write to database: {:?}", e);
            }
        } else {
            error!("Failed to open database file for writing");
        }
}

async fn run_matrix(
    mut matrix_rx: mpsc::Receiver<(String, String, String)>,
                    irc_send_tx: mpsc::Sender<(String, String, String)>,
                    access_token: String,
                    matrix_user_id: String,
                    matrix_device_id: Option<String>,
                    matrix_homeserver: String,
                    bridge_configs: Vec<BridgeConfig>,
) -> Result<(), Box<dyn std::error::Error>> {
    let homeserver_url = Url::parse(&matrix_homeserver)?;

    let client = Client::new(homeserver_url).await?;

    let db_path = Path::new("processed_events.db");
    let processed_events = load_processed_events(db_path);
    info!("Starting with {} previously processed events", processed_events.len());

    let session = matrix_sdk::matrix_auth::MatrixSession {
        meta: matrix_sdk_base::SessionMeta {
            user_id: matrix_user_id.parse()?,
            device_id: matrix_device_id.unwrap_or_else(|| "BRIDGE_DEVICE".to_string()).into(),
        },
        tokens: matrix_sdk::matrix_auth::MatrixSessionTokens {
            access_token: access_token,
            refresh_token: None,
        },
    };

    info!("Restoring Matrix session...");
    client.restore_session(session).await?;

    let user_id = client.user_id().ok_or("Failed to get user ID")?.to_owned();
    info!("Logged into Matrix successfully as {}", user_id);

    info!("Performing initial sync (this may take a moment)...");
    let sync_settings = SyncSettings::default().timeout(Duration::from_secs(30));

    let sync_response = match client.sync_once(sync_settings.clone()).await {
        Ok(response) => {
            info!("Initial Matrix sync completed");
            response
        },
        Err(e) => {
            error!("Initial sync failed: {:?}", e);
            return Err(Box::new(e));
        }
    };

    let room_to_channel: Arc<Mutex<HashMap<String, String>>> = Arc::new(Mutex::new(HashMap::new()));
    let channel_to_room: Arc<Mutex<HashMap<String, String>>> = Arc::new(Mutex::new(HashMap::new()));

    for config in &bridge_configs {
        let room_id: matrix_sdk::ruma::OwnedRoomId = config.matrix_room_id.parse()?;

        let room = client.get_room(&room_id)
        .ok_or_else(|| format!("Room {} not found", config.matrix_room_id))?;

        info!("Successfully connected to room: {} (bridged to {})", room.room_id(), config.irc_channel);

        let mut r2c = room_to_channel.lock().unwrap();
        let mut c2r = channel_to_room.lock().unwrap();
        r2c.insert(config.matrix_room_id.clone(), config.irc_channel.clone());
        c2r.insert(config.irc_channel.clone(), config.matrix_room_id.clone());
    }

    let room_to_channel_for_sender = room_to_channel.clone();
    let client_for_sender = client.clone();

    tokio::spawn(async move {
        while let Some((irc_channel, username, msg)) = matrix_rx.recv().await {
            info!("Sending message from IRC channel {} user {} to Matrix: {}", irc_channel, username, msg);

            let room_id_str = {
                let c2r = channel_to_room.lock().unwrap();
                c2r.get(&irc_channel).cloned()
            };

            if let Some(room_id_str) = room_id_str {
                if let Ok(room_id) = room_id_str.parse::<matrix_sdk::ruma::OwnedRoomId>() {
                    if let Some(room) = client_for_sender.get_room(&room_id) {
                        let content = RoomMessageEventContent::text_plain(&msg);
                        match room.send(content).await {
                            Ok(_) => info!("Message sent to Matrix room {}", room_id),
                 Err(e) => error!("Failed to send to Matrix: {:?}", e),
                        }
                    }
                }
            } else {
                error!("No Matrix room mapped to IRC channel {}", irc_channel);
            }
        }
    });

    let seen_events = Arc::new(Mutex::new(processed_events));
    let bridge_user_id = user_id.clone();
    let db_path = Arc::new(Mutex::new(db_path.to_path_buf()));

    let event_to_sender: Arc<Mutex<HashMap<String, String>>> = Arc::new(Mutex::new(HashMap::new()));
    let event_to_message: Arc<Mutex<HashMap<String, String>>> = Arc::new(Mutex::new(HashMap::new()));

    client.add_event_handler({
        let seen_events = seen_events.clone();
        let db_path = db_path.clone();
        let event_to_sender = event_to_sender.clone();
        let event_to_message = event_to_message.clone();
        let room_to_channel = room_to_channel_for_sender.clone();

        move |event: OriginalSyncRoomMessageEvent, room: Room| {
            let irc_tx = irc_send_tx.clone();
            let seen_events = seen_events.clone();
            let bridge_user_id = bridge_user_id.clone();
            let db_path = db_path.clone();
            let event_to_sender = event_to_sender.clone();
            let event_to_message = event_to_message.clone();
            let room_to_channel = room_to_channel.clone();

            async move {
                let room_id_str = room.room_id().to_string();

                let irc_channel = {
                    let r2c = room_to_channel.lock().unwrap();
                    r2c.get(&room_id_str).cloned()
                };

                let irc_channel = match irc_channel {
                    Some(ch) => ch,
                             None => return,
                };

                if event.sender == *bridge_user_id {
                    return;
                }

                let event_id = event.event_id.to_string();
                {
                    let mut seen = seen_events.lock().unwrap();
                    if seen.contains(&event_id) {
                        return;
                    }
                    seen.insert(event_id.clone());
                }

                let db_path_clone = db_path.lock().unwrap().clone();
                let event_id_for_db = event_id.clone();
                tokio::task::spawn_blocking(move || {
                    save_processed_event(&db_path_clone, &event_id_for_db);
                });

                let MessageType::Text(text_content) = event.content.msgtype else {
                    return;
                };

                let sender = event.sender.to_string();
                let username = sender.trim_start_matches('@').split(':').next().unwrap_or("unknown").to_string();

                let message_body = text_content.body.clone();

                {
                    let mut sender_mapping = event_to_sender.lock().unwrap();
                    sender_mapping.insert(event_id.clone(), username.clone());

                    let mut message_mapping = event_to_message.lock().unwrap();

                    let preview = if message_body.len() > 50 {
                        format!("{}...", truncate_at_char_boundary(&message_body, 50))
                    } else {
                        message_body.clone()
                    };
                    message_mapping.insert(event_id.clone(), preview);

                    if sender_mapping.len() > 1000 {
                        let keys_to_remove: Vec<_> = sender_mapping.keys()
                        .take(sender_mapping.len() - 1000)
                        .cloned()
                        .collect();
                        for key in &keys_to_remove {
                            sender_mapping.remove(key);
                            message_mapping.remove(key);
                        }
                    }
                }

                let mut message = message_body.clone();
                let original_message = message_body;

                if let Some(relates_to) = &event.content.relates_to {
                    if let Relation::Reply { in_reply_to } = relates_to {
                        let replied_event_id = in_reply_to.event_id.to_string();

                        let (replied_to_user, replied_message) = {
                            let sender_mapping = event_to_sender.lock().unwrap();
                            let message_mapping = event_to_message.lock().unwrap();
                            (
                                sender_mapping.get(&replied_event_id).cloned(),
                             message_mapping.get(&replied_event_id).cloned()
                            )
                        };

                        let (replied_to_user, replied_message) = if replied_to_user.is_none() {
                            match room.event(&in_reply_to.event_id).await {
                                Ok(timeline_event) => {
                                    let event_item = timeline_event.event.deserialize();
                                    match event_item {
                                        Ok(matrix_sdk::ruma::events::AnyTimelineEvent::MessageLike(
                                            matrix_sdk::ruma::events::AnyMessageLikeEvent::RoomMessage(
                                                matrix_sdk::ruma::events::room::message::RoomMessageEvent::Original(orig)
                                            )
                                        )) => {
                                            let sender_str = orig.sender.to_string();
                                            let replied_username = sender_str.trim_start_matches('@')
                                            .split(':')
                                            .next()
                                            .unwrap_or("unknown")
                                            .to_string();

                                            let replied_msg = if let MessageType::Text(text) = orig.content.msgtype {
                                                let preview = if text.body.len() > 50 {
                                                    format!("{}...", truncate_at_char_boundary(&text.body, 50))
                                                } else {
                                                    text.body.clone()
                                                };

                                                {
                                                    let mut sender_mapping = event_to_sender.lock().unwrap();
                                                    let mut message_mapping = event_to_message.lock().unwrap();
                                                    sender_mapping.insert(replied_event_id.clone(), replied_username.clone());
                                                    message_mapping.insert(replied_event_id.clone(), preview.clone());
                                                }

                                                Some(preview)
                                            } else {
                                                None
                                            };

                                            (Some(replied_username), replied_msg)
                                        }
                                        _ => (None, None)
                                    }
                                }
                                Err(e) => {
                                    info!("Could not fetch replied-to event: {:?}", e);
                                    (None, None)
                                }
                            }
                        } else {
                            (replied_to_user, replied_message)
                        };

                        if let Some(replied_to) = replied_to_user {
                            if let Some(original_msg) = replied_message {
                                message = format!("@{} \"{}\": {}", replied_to, original_msg, original_message);
                                info!("Handling reply from {} to {} (original: {})", username, replied_to, original_msg);
                            } else {
                                message = format!("@{}: {}", replied_to, original_message);
                                info!("Handling reply from {} to {}", username, replied_to);
                            }
                        } else {
                            message = format!("[replying] {}", original_message);
                        }
                    }
                }

                info!("Received message from Matrix user {} in room {} (IRC: {}): {}", username, room_id_str, irc_channel, message);

                if let Err(e) = irc_tx.send((irc_channel, username, message)).await {
                    error!("Failed to forward to IRC: {:?}", e);
                }
            }
        }
    });

    info!("Matrix bridge ready - listening for messages in {} room(s)", bridge_configs.len());

    let settings = SyncSettings::default()
    .timeout(Duration::from_secs(30))
    .token(sync_response.next_batch);
    let _sync_result = client.sync(settings).await;

    Ok(())
}

struct IrcUserConnection {
    sender: irc::client::Sender,
    _handle: tokio::task::JoinHandle<()>,
}

async fn run_irc_manager(
    matrix_tx: mpsc::Sender<(String, String, String)>,
                         mut irc_send_rx: mpsc::Receiver<(String, String, String)>,
                         irc_server_password: Option<String>,
                         irc_server: String,
                         irc_port: u16,
                         irc_use_tls: bool,
                         bridge_configs: Vec<BridgeConfig>,
) -> Result<(), Box<dyn std::error::Error>> {
    info!("Starting IRC connection manager");

    let all_channels: Vec<String> = bridge_configs.iter()
    .map(|c| c.irc_channel.clone())
    .collect();

    let user_connections: Arc<Mutex<HashMap<String, IrcUserConnection>>> = Arc::new(Mutex::new(HashMap::new()));
    let user_connections_clone = user_connections.clone();

    let matrix_users: Arc<Mutex<HashSet<String>>> = Arc::new(Mutex::new(HashSet::new()));
    let matrix_users_for_sender = matrix_users.clone();

    let recent_messages: Arc<Mutex<Vec<(String, String, String, std::time::Instant)>>> = Arc::new(Mutex::new(Vec::new()));
    let recent_messages_for_sender = recent_messages.clone();
    let recent_messages_for_listener = recent_messages.clone();

    let irc_server_clone = irc_server.clone();
    let all_channels_clone = all_channels.clone();
    let irc_server_password_clone = irc_server_password.clone();

    let _connection_tasks: JoinSet<()> = JoinSet::new();

    let _sender_handle = tokio::spawn(async move {
        while let Some((irc_channel, username, message)) = irc_send_rx.recv().await {
            info!("Sending message from Matrix user {} to IRC channel {}", username, irc_channel);

            {
                let mut users = matrix_users_for_sender.lock().unwrap();
                users.insert(username.clone());
            }

            let needs_new_connection = {
                let connections = user_connections_clone.lock().unwrap();
                !connections.contains_key(&username)
            };

            if needs_new_connection {
                info!("Creating new IRC connection for user: {}", username);

                let config = Config {
                    nickname: Some(username.clone()),
                                      server: Some(irc_server_clone.clone()),
                                      port: Some(irc_port),
                                      use_tls: Some(irc_use_tls),
                                      channels: all_channels_clone.clone(),
                                      password: irc_server_password_clone.clone(),
                                      ..Default::default()
                };

                match irc::client::Client::from_config(config).await {
                    Ok(mut client) => {
                        if let Err(e) = client.identify() {
                            error!("Failed to identify IRC client for {}: {:?}", username, e);
                            continue;
                        }

                        let sender = client.sender();
                        let stream = match client.stream() {
                            Ok(s) => s,
                                      Err(e) => {
                                          error!("Failed to get stream for {}: {:?}", username, e);
                                          continue;
                                      }
                        };

                        let handle = tokio::spawn(async move {
                            let mut stream = stream;
                            while let Some(_) = stream.next().await {
                            }
                        });

                        let mut connections = user_connections_clone.lock().unwrap();
                        connections.insert(username.clone(), IrcUserConnection {
                            sender: sender.clone(),
                                           _handle: handle,
                        });

                        info!("IRC connection established for user: {} (joined {} channels)", username, all_channels_clone.len());
                    }
                    Err(e) => {
                        error!("Failed to create IRC client for {}: {:?}", username, e);
                        continue;
                    }
                }
            }

            {
                let connections = user_connections_clone.lock().unwrap();
                if let Some(conn) = connections.get(&username) {
                    if let Err(e) = conn.sender.send_privmsg(&irc_channel, &message) {
                        error!("Failed to send IRC message for {} to {}: {:?}", username, irc_channel, e);
                    } else {
                        let mut recent = recent_messages_for_sender.lock().unwrap();
                        recent.push((irc_channel.clone(), username.clone(), message.clone(), std::time::Instant::now()));

                        if recent.len() > 100 {
                            recent.remove(0);
                        }
                    }
                }
            }
        }
    });

    let listener_config = Config {
        nickname: Some("bridge_listener".to_string()),
        server: Some(irc_server.clone()),
        port: Some(irc_port),
        use_tls: Some(irc_use_tls),
        channels: all_channels.clone(),
        password: irc_server_password.clone(),
        ..Default::default()
    };

    info!("Connecting listener to IRC server {}:{} (TLS: {}) for {} channel(s)",
          irc_server, irc_port, irc_use_tls, all_channels.len());

    let mut irc_client = irc::client::Client::from_config(listener_config).await?;
    irc_client.identify()?;
    info!("IRC listener connected and identified");

    let mut stream = irc_client.stream()?;
    let matrix_tx_clone = matrix_tx.clone();

    let matrix_users_for_listener = matrix_users.clone();

    let _listener_handle = tokio::spawn(async move {
        while let Some(result) = stream.next().await {
            match result {
                Ok(message) => {
                    match message.command {
                        Command::PRIVMSG(ref target, ref msg) => {
                            let sender = message.source_nickname().unwrap_or("Unknown");

                            let is_matrix_user = {
                                let users = matrix_users_for_listener.lock().unwrap();
                                users.contains(sender)
                            };

                            if is_matrix_user {
                                let is_recent = {
                                    let mut recent = recent_messages_for_listener.lock().unwrap();

                                    let now = std::time::Instant::now();
                                    recent.retain(|(_, _, _, time)| now.duration_since(*time).as_secs() < 5);

                                    recent.iter().any(|(channel, user, message, _)| channel == target && user == sender && message == msg)
                                };

                                if is_recent {
                                    info!("[IRC] Ignoring echo from Matrix user {} in {}: {}", sender, target, msg);
                                    continue;
                                }
                            }

                            let formatted_msg = format!("<{}> {}", sender, msg);
                            info!("[IRC] Message from {} in {}: {}", sender, target, msg);

                            if let Err(e) = matrix_tx_clone.send((target.to_string(), sender.to_string(), formatted_msg)).await {
                                error!("Failed to forward to Matrix: {:?}", e);
                            }
                        }
                        _ => {}
                    }
                }
                Err(e) => {
                    error!("Error receiving IRC message: {:?}", e);
                }
            }
        }
    });

    info!("IRC manager ready - will create per-user connections as needed");

    loop {
        tokio::time::sleep(Duration::from_secs(60)).await;

        let mut connections = user_connections.lock().unwrap();
        let initial_count = connections.len();
        connections.retain(|username, conn| {
            if conn._handle.is_finished() {
                info!("Removing dead connection for user: {}", username);
                false
            } else {
                true
            }
        });
        let removed = initial_count - connections.len();
        if removed > 0 {
            info!("Cleaned up {} dead IRC connections", removed);
        }
    }
}
