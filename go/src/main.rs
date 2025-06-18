use std::collections::HashMap;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH, Instant};
use tokio::sync::RwLock;
use tokio::net::{UnixListener, TcpListener};
use tokio::io::{AsyncBufReadExt, BufReader};
use serde::{Deserialize, Serialize};
use regex::Regex;
use reqwest;
use axum::{
    extract::State,
    http::StatusCode,
    response::IntoResponse,
    routing::post,
    Router, Json,
};

// TermInfo represents terminal metadata
#[derive(Debug, Clone, Serialize, Deserialize)]
struct TermInfo {
    cols: i32,
    rows: i32,
    #[serde(rename = "type")]
    term_type: String,
    version: String,
    theme: HashMap<String, serde_json::Value>,
}

// CastHeader represents the header of an asciinema cast file
#[derive(Debug, Clone, Serialize, Deserialize)]
struct CastHeader {
    version: i32,
    term: TermInfo,
    timestamp: i64,
    env: HashMap<String, String>,
    child_pid: i32,
    username: Option<String>,
    directory: Option<String>,
    shell: Option<String>,
}

// CastEvent represents an event in an asciinema cast file
#[derive(Debug, Clone, Serialize, Deserialize)]
struct CastEvent {
    time: Option<f64>,
    #[serde(rename = "type")]
    event_type: Option<String>,
    data: Option<String>,
    pid: Option<i32>,
}

#[derive(Debug, Clone, PartialEq)]
enum SessionState {
    Idle,
    Prompt,
    Command,
}

#[derive(Debug, Clone)]
struct TerminalSession {
    pid: i32,
    state: SessionState,
    command_buffer: Vec<String>,
    prompt_buffer: Vec<String>,
    last_exit_code: i64,
    command_string: String,
    current_input: String,
    start_time: Option<Instant>,
    command_id: String,
    expecting_command: bool,
}

impl TerminalSession {
    fn new(pid: i32) -> Self {
        Self {
            pid,
            state: SessionState::Idle,
            command_buffer: Vec::new(),
            prompt_buffer: Vec::new(),
            last_exit_code: 0,
            command_string: String::new(),
            current_input: String::new(),
            start_time: None,
            command_id: String::new(),
            expecting_command: false,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct RegexFilter {
    #[serde(rename = "type")]
    filter_type: String,
    name: String,
    pattern: String,
    detail: Option<String>,
    #[serde(skip)]
    regex: Option<Regex>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct EventPayload {
    event: String,
    command: Option<String>,
    #[serde(rename = "commandId")]
    command_id: Option<String>,
    shell: Option<String>,
    username: Option<String>,
    directory: Option<String>,
    #[serde(rename = "exitCode")]
    exit_code: Option<i64>,
    duration: Option<i64>,
    name: Option<String>,
    detail: Option<HashMap<String, String>>,
    #[serde(rename = "shouldEnd")]
    should_end: Option<bool>,
    #[serde(rename = "sourceName")]
    source_name: Option<String>,
    #[serde(rename = "sourceVersion")]
    source_version: Option<String>,
}

type SharedState = Arc<RwLock<Vec<RegexFilter>>>;
type TerminalInfoMap = Arc<RwLock<HashMap<String, CastHeader>>>;

const SOURCE_NAME: &str = "Rust Terminal Server";

fn get_binary_mod_time() -> String {
    std::env::current_exe()
        .and_then(|path| std::fs::metadata(path))
        .and_then(|metadata| metadata.modified())
        .map(|time| {
            time.duration_since(SystemTime::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs()
        })
        .map(|secs| {
            chrono::DateTime::from_timestamp(secs as i64, 0)
                .unwrap_or_default()
                .to_rfc3339()
        })
        .unwrap_or_default()
}

fn looks_like_json(s: &str) -> bool {
    let s = s.trim();
    if s.len() < 2 {
        return false;
    }
    (s.starts_with('{') && s.ends_with('}')) || (s.starts_with('[') && s.ends_with(']'))
}

fn extract_exit_code(data: &str) -> i64 {
    let re = Regex::new(r"\x1b]133;D;(\d+)\x07").unwrap();
    if let Some(captures) = re.captures(data) {
        if let Some(code_str) = captures.get(1) {
            if let Ok(code) = code_str.as_str().parse::<i64>() {
                return code;
            }
        }
    }
    -1
}

fn extract_command_from_osc133b(line: &str) -> String {
    let osc133b = "\x1b]133;B\x07";
    if let Some(start) = line.find(osc133b) {
        let after_b = &line[start + osc133b.len()..];
        let erase_to_end = "\x1b[K";
        let result = if let Some(end) = after_b.find(erase_to_end) {
            &after_b[..end]
        } else {
            after_b
        };
        result.trim().to_string()
    } else {
        String::new()
    }
}

fn is_real_output(data: &str) -> bool {
    let osc_pattern = Regex::new(r"^\x1b\](133;[CD]|1337;RemoteHost=|1337;CurrentDir=)").unwrap();
    !osc_pattern.is_match(data) && !data.trim().is_empty()
}

async fn send_event(payload: EventPayload) {
    if payload.command.as_ref().map_or(true, |c| c.is_empty()) {
        return;
    }

    let mut full_payload = payload;
    full_payload.source_name = Some(SOURCE_NAME.to_string());
    full_payload.source_version = Some(get_binary_mod_time());

    let url = "http://127.0.0.1:51645/";
    println!("Sending {} event: {:?}", full_payload.event, full_payload);

    let client = reqwest::Client::new();
    tokio::spawn(async move {
        if let Err(e) = client.post(url).json(&full_payload).send().await {
            eprintln!("Failed to send {} event: {}", full_payload.event, e);
        }
    });
}

fn match_step_event(line: &str, filters: &[RegexFilter]) -> (String, Option<HashMap<String, String>>) {
    for filter in filters {
        if filter.filter_type != "step" {
            continue;
        }
        if let Some(ref regex) = filter.regex {
            if let Some(captures) = regex.captures(line) {
                println!("Found match for {}: {:?}", filter.name, captures);
                let mut result = HashMap::new();
                for (i, name) in regex.capture_names().enumerate() {
                    if i != 0 {
                        if let Some(name) = name {
                            if let Some(capture) = captures.get(i) {
                                result.insert(name.to_string(), capture.as_str().to_string());
                            }
                        }
                    }
                }
                return (filter.name.clone(), if result.is_empty() { None } else { Some(result) });
            }
        }
    }
    ("".to_string(), None)
}

async fn regex_filters_handler(
    State(state): State<SharedState>,
    Json(incoming): Json<Vec<RegexFilter>>,
) -> impl IntoResponse {
    println!("Received regex filters: {:?}", incoming);
    
    let mut compiled = Vec::new();
    for mut filter in incoming {
        match Regex::new(&filter.pattern) {
            Ok(regex) => {
                filter.regex = Some(regex);
                compiled.push(filter);
            }
            Err(_) => {
                return (StatusCode::BAD_REQUEST, format!("Invalid regex: {}", filter.pattern));
            }
        }
    }
    
    *state.write().await = compiled;
    (StatusCode::OK, "OK".to_string())
}

async fn handle_connection(
    mut stream: tokio::net::UnixStream,
    terminal_info: TerminalInfoMap,
    regex_filters: SharedState,
) {
    let conn_id = format!("{:p}", &stream as *const _);
    println!("New connection established: {}", conn_id);

    let reader = BufReader::new(&mut stream);
    let mut lines = reader.lines();

    let mut header_parsed = false;
    let mut header = CastHeader {
        version: 0,
        term: TermInfo {
            cols: 0,
            rows: 0,
            term_type: String::new(),
            version: String::new(),
            theme: HashMap::new(),
        },
        timestamp: 0,
        env: HashMap::new(),
        child_pid: 0,
        username: None,
        directory: None,
        shell: None,
    };
    let mut username = String::new();
    let mut directory = String::new();
    let mut shell = String::new();
    let mut sessions: HashMap<i32, TerminalSession> = HashMap::new();

    while let Ok(Some(line)) = lines.next_line().await {
        let trimmed = line.trim();
        
        if !header_parsed && looks_like_json(trimmed) && trimmed.starts_with('{') {
            if let Ok(parsed_header) = serde_json::from_str::<CastHeader>(trimmed) {
                header_parsed = true;
                username = parsed_header.username.clone().unwrap_or_default();
                directory = parsed_header.directory.clone().unwrap_or_default();
                shell = parsed_header.shell.clone().unwrap_or_default();
                header = parsed_header;
                
                terminal_info.write().await.insert(conn_id.clone(), header.clone());
                println!("[header] username={:?} directory={:?} shell={:?}", username, directory, shell);
                continue;
            }
        }

        if looks_like_json(trimmed) && trimmed.starts_with('[') {
            if let Ok(arr) = serde_json::from_str::<Vec<serde_json::Value>>(trimmed) {
                if arr.len() >= 4 {
                    if let (Some(pid_val), Some(data_val)) = (arr.get(3), arr.get(2)) {
                        if let (Some(pid), Some(data)) = (pid_val.as_f64(), data_val.as_str()) {
                            let pid_int = pid as i32;
                            let session = sessions.entry(pid_int).or_insert_with(|| TerminalSession::new(pid_int));

                            println!("[raw {}] {:?}", pid_int, data);

                            // Check for OSC 1337;CurrentDir= in the data field
                            let current_dir_pattern = Regex::new(r"\x1b]1337;CurrentDir=([^\x07]*)\x07").unwrap();
                            if let Some(captures) = current_dir_pattern.captures(data) {
                                if let Some(dir) = captures.get(1) {
                                    directory = dir.as_str().to_string();
                                    println!("[directory changed] {}", directory);
                                }
                            }

                            if data.contains("\x1b]133;B\x07") {
                                let cmd = extract_command_from_osc133b(data);
                                if !cmd.is_empty() {
                                    println!("[COMMAND START] Just entered: {:?}", cmd);
                                    session.command_string = cmd.clone();
                                    session.state = SessionState::Command;
                                    session.command_buffer.clear();
                                    session.start_time = Some(Instant::now());
                                    session.command_id = format!("{}N-{}", 
                                        SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs(), 
                                        pid_int
                                    );
                                    session.expecting_command = false;

                                    send_event(EventPayload {
                                        event: "start".to_string(),
                                        command: Some(cmd),
                                        command_id: Some(session.command_id.clone()),
                                        shell: Some(shell.clone()),
                                        username: Some(username.clone()),
                                        directory: Some(directory.clone()),
                                        exit_code: None,
                                        duration: None,
                                        name: None,
                                        detail: None,
                                        should_end: None,
                                        source_name: None,
                                        source_version: None,
                                    }).await;
                                } else {
                                    session.expecting_command = true;
                                    session.state = SessionState::Command;
                                    session.command_buffer.clear();
                                    session.start_time = Some(Instant::now());
                                    session.command_id = format!("{}N-{}", 
                                        SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs(), 
                                        pid_int
                                    );
                                }
                            } else if data.contains("\x1b]133;D") {
                                println!("[debug] OSC 133;D: CommandBuffer={:?}", session.command_buffer);
                                session.state = SessionState::Prompt;
                                let exit_code = extract_exit_code(data);
                                session.last_exit_code = exit_code;

                                println!("[COMMAND END] PID {}, exit={}", session.pid, session.last_exit_code);
                                println!("  Command: {:?}", session.command_string);
                                for l in &session.command_buffer {
                                    println!("    {:?}", l);
                                }
                                println!("---");

                                let duration = session.start_time
                                    .map(|start| start.elapsed().as_millis() as i64)
                                    .unwrap_or(100);

                                send_event(EventPayload {
                                    event: "end".to_string(),
                                    command: Some(session.command_string.clone()),
                                    command_id: Some(session.command_id.clone()),
                                    shell: Some(shell.clone()),
                                    username: Some(username.clone()),
                                    directory: Some(directory.clone()),
                                    exit_code: Some(exit_code),
                                    duration: Some(duration),
                                    name: None,
                                    detail: None,
                                    should_end: None,
                                    source_name: None,
                                    source_version: None,
                                }).await;

                                session.command_buffer.clear();
                                session.command_string.clear();
                                session.start_time = None;
                                session.current_input.clear();
                            } else {
                                if session.expecting_command {
                                    session.expecting_command = false;
                                    let trimmed_data = data.trim();
                                    if trimmed_data.ends_with("\x1b[K") {
                                        let cmd = trimmed_data.trim_end_matches("\x1b[K");
                                        if !cmd.is_empty() {
                                            session.command_string = cmd.to_string();
                                            println!("[COMMAND START +] Found on next line: {:?}", cmd);
                                            
                                            send_event(EventPayload {
                                                event: "start".to_string(),
                                                command: Some(cmd.to_string()),
                                                command_id: Some(session.command_id.clone()),
                                                shell: Some(shell.clone()),
                                                username: Some(username.clone()),
                                                directory: Some(directory.clone()),
                                                exit_code: None,
                                                duration: None,
                                                name: None,
                                                detail: None,
                                                should_end: None,
                                                source_name: None,
                                                source_version: None,
                                            }).await;
                                        }
                                    }
                                } else if session.state == SessionState::Command {
                                    if is_real_output(data) {
                                        let filters = regex_filters.read().await;
                                        let (step_name, detail) = match_step_event(data, &filters);
                                        
                                        if !step_name.is_empty() {
                                            if let Some(ref detail_map) = detail {
                                                if !detail_map.is_empty() {
                                                    if let Ok(detail_json) = serde_json::to_string(detail_map) {
                                                        println!("[step {}] {:?} detail={}", step_name, data, detail_json);
                                                    }
                                                }
                                            } else {
                                                println!("[step {}] {:?}", step_name, data);
                                            }

                                            send_event(EventPayload {
                                                event: "step".to_string(),
                                                command: Some(session.command_string.clone()),
                                                command_id: Some(session.command_id.clone()),
                                                shell: Some(shell.clone()),
                                                username: Some(username.clone()),
                                                directory: Some(directory.clone()),
                                                exit_code: None,
                                                duration: None,
                                                name: Some(step_name),
                                                detail,
                                                should_end: Some(false),
                                                source_name: None,
                                                source_version: None,
                                            }).await;
                                        }
                                        session.command_buffer.push(data.to_string());
                                    }
                                } else if session.state == SessionState::Prompt {
                                    session.prompt_buffer.push(data.to_string());
                                }
                            }
                        }
                    }
                }
            } else {
                println!("[unknown] {}", trimmed);
            }
        } else {
            println!("[unknown] {}", trimmed);
        }
    }

    // Clean up terminal info when connection closes
    terminal_info.write().await.remove(&conn_id);
    println!("Connection {} closed", conn_id);
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let socket_path = "/tmp/focusbase.sock";
    
    // Remove socket if it already exists
    if tokio::fs::remove_file(socket_path).await.is_ok() {
        println!("Removed existing socket");
    }
    
    // Create the Unix socket
    let listener = UnixListener::bind(socket_path)?;
    println!("Unix socket server listening on {}", socket_path);
    
    // Shared state for regex filters and terminal info
    let regex_filters: SharedState = Arc::new(RwLock::new(Vec::new()));
    let terminal_info: TerminalInfoMap = Arc::new(RwLock::new(HashMap::new()));
    
    // Start HTTP server for regex filters
    let http_regex_filters = regex_filters.clone();
    tokio::spawn(async move {
        let app = Router::new()
            .route("/regexfilters", post(regex_filters_handler))
            .with_state(http_regex_filters);
        
        let tcp_listener = TcpListener::bind("0.0.0.0:51646").await.unwrap();
        println!("HTTP server listening on port 51646");
        
        axum::serve(tcp_listener, app).await.unwrap();
    });
    
    // Accept connections
    loop {
        match listener.accept().await {
            Ok((stream, _)) => {
                let terminal_info_clone = terminal_info.clone();
                let regex_filters_clone = regex_filters.clone();
                
                tokio::spawn(async move {
                    handle_connection(stream, terminal_info_clone, regex_filters_clone).await;
                });
            }
            Err(e) => {
                eprintln!("Error accepting connection: {}", e);
            }
        }
    }
}