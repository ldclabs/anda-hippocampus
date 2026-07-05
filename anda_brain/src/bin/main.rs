use anda_core::{BoxError, ModelEffort, Principal};
use anda_db::{database::DBConfig, storage::StorageConfig};
use anda_engine::{
    management::{BaseManagement, Visibility},
    model::{ModelConfig, Models, Proxy, request_client_builder, reqwest},
};
use anda_object_store::MetaStoreBuilder;
use axum::{Router, routing};
use clap::{Parser, Subcommand};
use mimalloc::MiMalloc;
use object_store::{
    ObjectStore,
    aws::{AmazonS3Builder, S3CopyIfNotExists},
    local::LocalFileSystem,
    memory::InMemory,
};
use std::{collections::BTreeSet, net::SocketAddr, sync::Arc, time::Duration};
use structured_logger::{Builder, async_json::new_writer, get_env_level};
use tokio::signal;
use tokio_util::sync::CancellationToken;
use tower_http::{
    compression::CompressionLayer,
    cors::{AllowHeaders, AllowMethods, CorsLayer},
};

use anda_brain::{
    agents::SELF_USER_ID,
    handler::*,
    mcp::{McpHttpServerConfig, McpServerConfig, build_streamable_http_service, run_stdio_server},
    parse_ed25519_pubkeys,
    space::AppState,
};

#[global_allocator]
static GLOBAL: MiMalloc = MiMalloc;

const APP_NAME: &str = env!("CARGO_PKG_NAME");
const APP_VERSION: &str = env!("CARGO_PKG_VERSION");

#[derive(Parser, Clone)]
#[command(author, version, about, long_about = None)]
struct Cli {
    /// Port to listen on
    #[clap(long, env = "LISTEN_ADDR", default_value = "127.0.0.1:8042")]
    addr: String,

    /// API key
    #[arg(long, env = "ED25519_PUBKEYS", default_value = "")]
    ed25519_pubkeys: String,

    /// AI model family (e.g., "gemini", "anthropic", "openai")
    #[arg(long, env = "MODEL_FAMILY", default_value = "anthropic")]
    model_family: String,

    /// AI model name (e.g., "gemini-3-flash-preview", "claude-sonnet-4-6")
    #[arg(long, env = "MODEL_NAME", default_value = "deepseek-v4-pro")]
    model_name: String,

    /// API key for AI model
    #[arg(long, env = "MODEL_API_KEY", default_value = "")]
    model_api_key: String,

    #[arg(long, env = "MODEL_CONTEXT_WINDOW", default_value_t = 400000)]
    model_context_window: usize,

    #[arg(long, env = "MODEL_MAX_OUTPUT", default_value_t = 384000)]
    model_max_output: usize,

    /// API base URL for AI model
    #[arg(
        long,
        env = "MODEL_API_BASE",
        default_value = "https://api.deepseek.com/anthropic"
    )]
    model_api_base: String,

    /// Optional HTTPS proxy URL (e.g., "http://localhost:8080")
    #[arg(long, env = "HTTPS_PROXY")]
    https_proxy: Option<String>,

    #[arg(long, env = "SHARDING_IDX", default_value_t = 0)]
    sharding_idx: u32,

    /// Manager principal IDs, separated by comma
    #[arg(long, env = "MANAGERS", default_value = "")]
    managers: String,

    /// CORS allowed origins, separated by comma. Use "*" to allow all origins.
    #[arg(long, env = "CORS_ORIGINS", default_value = "")]
    cors_origins: String,

    /// Enable the Streamable HTTP MCP endpoint mounted with the HTTP service
    #[arg(
        long,
        env = "MCP_HTTP_ENABLED",
        default_value_t = true,
        action = clap::ArgAction::Set
    )]
    mcp_http_enabled: bool,

    /// HTTP path prefix for remote MCP clients. Clients connect to {prefix}/{space_id}
    #[arg(long, env = "MCP_HTTP_PATH_PREFIX", default_value = "/mcp")]
    mcp_http_path_prefix: String,

    /// Allowed Host values for remote MCP requests, separated by comma. Use "*" to allow all.
    #[arg(long, env = "MCP_HTTP_ALLOWED_HOSTS", default_value = "")]
    mcp_http_allowed_hosts: String,

    /// Allowed browser Origin values for remote MCP requests, separated by comma. Use "*" to allow all.
    #[arg(long, env = "MCP_HTTP_ALLOWED_ORIGINS", default_value = "")]
    mcp_http_allowed_origins: String,

    /// Create remote MCP spaces on first use when they do not exist
    #[arg(long, env = "MCP_HTTP_AUTO_CREATE_SPACE", default_value_t = false)]
    mcp_http_auto_create_space: bool,

    /// Tier used when remote MCP auto-creates a memory space
    #[arg(long, env = "MCP_HTTP_AUTO_CREATE_TIER", default_value_t = 1)]
    mcp_http_auto_create_tier: u32,

    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Subcommand, Clone)]
pub enum Commands {
    Local {
        #[clap(long, env = "LOCAL_DB_PATH", default_value = "./db")]
        db: String,
    },
    Aws {
        #[arg(long, env = "AWS_BUCKET")]
        bucket: String,

        #[arg(long, env = "AWS_REGION")]
        region: String,
    },
    Mcp {
        /// Memory space exposed through MCP tools
        #[arg(long, env = "MCP_SPACE_ID")]
        space_id: String,

        /// Optional CWT or space token used to authorize MCP tool calls
        #[arg(long = "mcp-auth-token", env = "MCP_AUTH_TOKEN")]
        auth_token: Option<String>,

        /// Create the MCP memory space if it does not exist
        #[arg(
            long = "mcp-auto-create-space",
            env = "MCP_AUTO_CREATE_SPACE",
            default_value_t = false
        )]
        auto_create_space: bool,

        /// Tier used when --mcp-auto-create-space creates the memory space
        #[arg(
            long = "mcp-auto-create-tier",
            env = "MCP_AUTO_CREATE_TIER",
            default_value_t = 1
        )]
        auto_create_tier: u32,

        #[command(subcommand)]
        storage: Option<StorageCommand>,
    },
}

#[derive(Subcommand, Clone)]
pub enum StorageCommand {
    Local {
        #[clap(long, env = "LOCAL_DB_PATH", default_value = "./db")]
        db: String,
    },
    Aws {
        #[arg(long, env = "AWS_BUCKET")]
        bucket: String,

        #[arg(long, env = "AWS_REGION")]
        region: String,
    },
}

#[derive(Clone, Copy, Debug)]
struct AnyHost;

impl PartialEq<&str> for AnyHost {
    fn eq(&self, _other: &&str) -> bool {
        true
    }
}

fn build_http_client(cli: &Cli) -> Result<reqwest::Client, BoxError> {
    let mut http_client = request_client_builder()
        .https_only(false)
        .timeout(Duration::from_secs(600))
        // grcov-excl-start: reqwest retry classification is exercised by reqwest internals; unit tests cover client construction and proxy validation.
        .retry(
            reqwest::retry::for_host(AnyHost)
                .max_retries_per_request(2)
                .classify_fn(|req_rep| {
                    if req_rep.error().is_some() {
                        return req_rep.retryable();
                    }

                    match req_rep.status() {
                        Some(
                            http::StatusCode::REQUEST_TIMEOUT
                            | http::StatusCode::TOO_MANY_REQUESTS
                            | http::StatusCode::BAD_GATEWAY
                            | http::StatusCode::SERVICE_UNAVAILABLE
                            | http::StatusCode::GATEWAY_TIMEOUT,
                        ) => req_rep.retryable(),
                        _ => req_rep.success(),
                    }
                }),
        );
    // grcov-excl-stop
    if let Some(proxy) = &cli.https_proxy {
        http_client = http_client.proxy(Proxy::all(proxy)?);
    }
    Ok(http_client.build()?)
}

fn parse_managers(input: &str) -> Result<BTreeSet<Principal>, BoxError> {
    let mut managers = BTreeSet::new();
    if !input.is_empty() {
        for id in input.split(',') {
            managers.insert(Principal::from_text(id)?);
        }
    }
    Ok(managers)
}

fn model_config_from_cli(cli: &Cli) -> ModelConfig {
    ModelConfig {
        family: cli.model_family.clone(),
        model: cli.model_name.clone(),
        api_key: cli.model_api_key.clone(),
        api_base: cli.model_api_base.clone(),
        context_window: cli.model_context_window,
        max_output: cli.model_max_output,
        disabled: cli.model_api_key.is_empty(),
        labels: vec![],
        bearer_auth: false,
        stream: false,
        effort: Some(ModelEffort::High),
    }
}

fn default_db_config() -> DBConfig {
    DBConfig {
        name: "test".to_string(), // This is placeholder. The real name is space_id.
        description: "Anda Brain database".to_string(),
        storage: StorageConfig {
            cache_max_capacity: 100000,
            compress_level: 3,
            object_chunk_size: 256 * 1024,
            bucket_overload_size: 1024 * 1024,
            max_small_object_size: 1024 * 1024 * 10,
        },
        lock: None,
    }
}

fn split_csv_values(input: &str) -> Vec<String> {
    input
        .split(',')
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToString::to_string)
        .collect()
}

fn normalize_http_path_prefix(prefix: &str) -> String {
    let trimmed = prefix.trim().trim_end_matches('/');
    if trimmed.is_empty() {
        "/mcp".to_string()
    } else if trimmed.starts_with('/') {
        trimmed.to_string()
    } else {
        format!("/{trimmed}")
    }
}

fn mcp_http_config_from_cli(cli: &Cli) -> McpHttpServerConfig {
    McpHttpServerConfig {
        path_prefix: normalize_http_path_prefix(&cli.mcp_http_path_prefix),
        auto_create_space: cli.mcp_http_auto_create_space,
        auto_create_tier: cli.mcp_http_auto_create_tier,
        allowed_hosts: split_csv_values(&cli.mcp_http_allowed_hosts),
        allowed_origins: split_csv_values(&cli.mcp_http_allowed_origins),
        stateful_mode: true,
        json_response: false,
        sse_keep_alive_secs: Some(15),
    }
}

// grcov-excl-start: route registration is verified through direct handler tests; axum's builder chain gives low-value line coverage.
fn build_router(
    app_state: AppState,
    cli: &Cli,
    cancel_token: CancellationToken,
) -> Router<AppState> {
    let mut router = Router::new()
        .route("/favicon.ico", routing::get(favicon))
        .route("/apple-touch-icon.webp", routing::get(apple_touch_icon))
        .route("/info", routing::get(get_information))
        .route("/SKILL.md", routing::get(get_skill))
        .route("/v1/{space_id}/info", routing::get(get_info))
        .route("/v1/{space_id}/status", routing::get(get_info))
        .route(
            "/v1/{space_id}/formation_status",
            routing::get(get_formation_status),
        )
        .route("/v1/{space_id}/formation", routing::post(post_formation))
        .route("/v1/{space_id}/recall", routing::post(post_recall))
        .route(
            "/v1/{space_id}/maintenance",
            routing::post(post_maintenance),
        )
        .route(
            "/v1/{space_id}/execute_kip_readonly",
            routing::post(execute_kip_readonly),
        )
        .route(
            "/v1/{space_id}/get_or_init_user",
            routing::post(get_or_init_user),
        )
        .route(
            "/v1/{space_id}/conversations/{conversation_id}",
            routing::get(get_conversation),
        )
        .route(
            "/v1/{space_id}/conversations/{conversation_id}/delta",
            routing::get(get_conversation_delta),
        )
        .route(
            "/v1/{space_id}/conversations",
            routing::get(list_conversations),
        )
        .route(
            "/v1/{space_id}/management/space_tokens",
            routing::get(list_space_tokens),
        )
        .route(
            "/v1/{space_id}/management/add_space_token",
            routing::post(add_space_token),
        )
        .route(
            "/v1/{space_id}/management/revoke_space_token",
            routing::post(revoke_space_token),
        )
        .route(
            "/v1/{space_id}/management/update_space",
            routing::patch(update_space),
        )
        .route(
            "/v1/{space_id}/management/restart_formation",
            routing::patch(restart_formation),
        )
        .route(
            "/v1/{space_id}/management/space_byok",
            routing::patch(update_byok),
        )
        .route(
            "/v1/{space_id}/management/space_byok",
            routing::get(get_byok),
        )
        .route(
            "/admin/{space_id}/update_space_tier",
            routing::post(update_space_tier),
        )
        .route("/admin/create_space", routing::post(create_space))
        .layer(CompressionLayer::new());

    if cli.mcp_http_enabled {
        let mcp_config = mcp_http_config_from_cli(cli);
        let path_prefix = mcp_config.path_prefix.clone();
        let mcp_service =
            build_streamable_http_service(app_state, mcp_config, cancel_token.child_token());
        router = router.nest_service(&path_prefix, mcp_service);
    }

    router
}
// grcov-excl-stop

fn build_cors(cors_origins: &str) -> CorsLayer {
    if cors_origins.is_empty() {
        CorsLayer::new()
    } else if cors_origins.trim() == "*" {
        CorsLayer::very_permissive()
    } else {
        let origins: Vec<http::HeaderValue> = cors_origins
            .split(',')
            .filter_map(|s| s.trim().parse().ok())
            .collect();
        CorsLayer::new()
            .allow_origin(origins)
            .allow_credentials(true)
            .max_age(Duration::from_secs(86400))
            .allow_headers(AllowHeaders::mirror_request())
            .allow_methods(AllowMethods::mirror_request())
    }
}

fn object_store_from_command(
    command: Option<Commands>,
) -> Result<(Arc<dyn ObjectStore>, String), BoxError> {
    let command = match command {
        Some(Commands::Local { db }) => Some(StorageCommand::Local { db }),
        Some(Commands::Aws { bucket, region }) => Some(StorageCommand::Aws { bucket, region }),
        Some(Commands::Mcp { storage, .. }) => storage,
        None => None,
    };

    object_store_from_storage_command(command)
}

fn object_store_from_storage_command(
    command: Option<StorageCommand>,
) -> Result<(Arc<dyn ObjectStore>, String), BoxError> {
    match command {
        Some(StorageCommand::Local { db }) => {
            let os = LocalFileSystem::new_with_prefix(db)?;
            let os = MetaStoreBuilder::new(os, 100000).build();
            Ok((Arc::new(os), "local".to_string()))
        }
        Some(StorageCommand::Aws { bucket, region }) => {
            let os = AmazonS3Builder::from_env()
                .with_bucket_name(bucket)
                .with_region(region)
                .with_copy_if_not_exists(S3CopyIfNotExists::Multipart)
                .build()?;
            Ok((Arc::new(os), "aws".to_string()))
        }
        None => Ok((Arc::new(InMemory::new()), "memory".to_string())),
    }
}

struct ServiceRuntime {
    app_state: AppState,
    app: Router,
    addr: SocketAddr,
    db_type: String,
    sharding_idx: u32,
    managers: String,
    model_name: String,
}

fn build_app_state(cli: &Cli) -> Result<(AppState, String), BoxError> {
    let http_client = build_http_client(cli)?;
    let managers = parse_managers(&cli.managers)?;
    let management = Arc::new(BaseManagement {
        controller: SELF_USER_ID,
        managers,
        visibility: Visibility::Public,
    });

    let models = Models::default();
    let model_config = model_config_from_cli(cli);
    models.set_model(model_config.model(http_client.clone())?);

    let (object_store, db_type) = object_store_from_command(cli.command.clone())?;
    let db_config = default_db_config();
    let ed25519_pubkeys = parse_ed25519_pubkeys(&cli.ed25519_pubkeys)?;

    let app_state = AppState::new(
        object_store,
        Arc::new(db_config),
        management,
        http_client,
        Arc::new(models),
        Arc::new(ed25519_pubkeys),
        APP_NAME.to_string(),
        APP_VERSION.to_string(),
        cli.sharding_idx,
    );

    Ok((app_state, db_type))
}

fn build_service_runtime(
    cli: &Cli,
    cancel_token: CancellationToken,
) -> Result<ServiceRuntime, BoxError> {
    let (app_state, db_type) = build_app_state(cli)?;
    let app = build_router(app_state.clone(), cli, cancel_token)
        .layer(build_cors(&cli.cors_origins))
        .with_state(app_state.clone());
    let addr: SocketAddr = cli.addr.parse()?;

    Ok(ServiceRuntime {
        app_state,
        app,
        addr,
        db_type,
        sharding_idx: cli.sharding_idx,
        managers: cli.managers.clone(),
        model_name: cli.model_name.clone(),
    })
}

async fn run_service(
    runtime: ServiceRuntime,
    global_cancel_token: CancellationToken,
) -> Result<(), BoxError> {
    let ServiceRuntime {
        app_state,
        app,
        addr,
        db_type,
        sharding_idx,
        managers,
        model_name,
    } = runtime;

    let listener = create_reuse_port_listener(addr).await?;
    let shutdown_token = global_cancel_token.clone();
    let server_handle = tokio::spawn(
        axum::serve(listener, app)
            .with_graceful_shutdown(shutdown_signal(shutdown_token))
            .into_future(),
    );

    let cancel_token = global_cancel_token.clone();
    let spaces_handle = tokio::spawn(async move {
        app_state.start_background_tasks(cancel_token).await;
    });

    log::warn!(
        target: "brain",
        "start service {}@{} on {:?}, sharding: {}, managers: {}, DB type: {}, Model: {}.",
        APP_NAME,
        APP_VERSION,
        addr,
        sharding_idx,
        managers,
        db_type,
        model_name
    );

    let _ = tokio::join!(server_handle, spaces_handle);
    Ok(())
}

/// ```bash
/// cargo run -p anda_brain
/// ```
// grcov-excl-start: main is a thin CLI/logging wrapper; build_service_runtime and run_service are unit-tested.
#[tokio::main]
async fn main() -> Result<(), BoxError> {
    dotenv::dotenv().ok();
    let cli = Cli::parse();

    if !matches!(cli.command, Some(Commands::Mcp { .. })) {
        // Initialize structured logging with JSON format. MCP stdio keeps stdout reserved
        // for JSON-RPC messages, so the MCP subcommand intentionally skips this logger.
        Builder::with_level(&get_env_level().to_string())
            .with_target_writer("*", new_writer(tokio::io::stdout()))
            .init();
    }

    // Create global cancellation token for graceful shutdown
    let global_cancel_token = CancellationToken::new();
    match cli.command.clone() {
        Some(Commands::Mcp {
            space_id,
            auth_token,
            auto_create_space,
            auto_create_tier,
            ..
        }) => {
            let (app_state, _) = build_app_state(&cli)?;
            let mut mcp_config =
                McpServerConfig::stdio(space_id, auth_token.filter(|token| !token.is_empty()));
            mcp_config.auto_create_space = auto_create_space;
            mcp_config.auto_create_tier = auto_create_tier;
            run_stdio_server(app_state, mcp_config).await
        }
        _ => {
            let runtime = build_service_runtime(&cli, global_cancel_token.child_token())?;
            run_service(runtime, global_cancel_token).await
        }
    }
}
// grcov-excl-stop

async fn shutdown_signal(cancel_token: CancellationToken) {
    let external_cancel = cancel_token.cancelled();
    // grcov-excl-start: OS signal futures require process-level signals; cancellation-driven shutdown is tested.
    let ctrl_c = async {
        signal::ctrl_c()
            .await
            .expect("failed to install Ctrl+C handler");
    };

    #[cfg(unix)]
    let terminate = async {
        signal::unix::signal(signal::unix::SignalKind::terminate())
            .expect("failed to install signal handler")
            .recv()
            .await;
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();
    // grcov-excl-stop

    tokio::select! {
        _ = external_cancel => {},
        _ = ctrl_c => {},
        _ = terminate => {},
    }

    log::warn!(target: "brain", "received termination signal, starting graceful shutdown");
    cancel_token.cancel();
}

async fn create_reuse_port_listener(addr: SocketAddr) -> Result<tokio::net::TcpListener, BoxError> {
    let socket = match &addr {
        SocketAddr::V4(_) => tokio::net::TcpSocket::new_v4()?,
        SocketAddr::V6(_) => tokio::net::TcpSocket::new_v6()?,
    };

    #[cfg(unix)]
    let _ = socket.set_reuseport(true);

    socket.bind(addr)?;
    let listener = socket.listen(1024)?;
    Ok(listener)
}

#[cfg(test)]
mod tests {
    use super::{
        AnyHost, Cli, Commands, build_cors, build_http_client, build_router, build_service_runtime,
        create_reuse_port_listener, default_db_config, mcp_http_config_from_cli,
        model_config_from_cli, normalize_http_path_prefix, object_store_from_command,
        parse_ed25519_pubkeys, parse_managers, run_service, split_csv_values,
    };
    use anda_brain::agents::SELF_USER_ID;
    use cose2::{Key as CoseKey, iana};
    use ic_auth_types::ByteBufB64;
    use std::time::{SystemTime, UNIX_EPOCH};
    use tokio::time::{Duration, sleep, timeout};
    use tokio_util::sync::CancellationToken;

    fn test_cli() -> Cli {
        Cli {
            addr: "127.0.0.1:0".to_string(),
            ed25519_pubkeys: String::new(),
            model_family: "openai".to_string(),
            model_name: "gpt-test".to_string(),
            model_api_key: "test-key".to_string(),
            model_context_window: 128,
            model_max_output: 64,
            model_api_base: "https://api.example.test".to_string(),
            https_proxy: None,
            sharding_idx: 7,
            managers: String::new(),
            cors_origins: String::new(),
            mcp_http_enabled: true,
            mcp_http_path_prefix: "/mcp".to_string(),
            mcp_http_allowed_hosts: String::new(),
            mcp_http_allowed_origins: String::new(),
            mcp_http_auto_create_space: false,
            mcp_http_auto_create_tier: 1,
            command: None,
        }
    }

    fn ed25519_basepoint_bytes() -> [u8; 32] {
        let mut bytes = [0x66; 32];
        bytes[0] = 0x58;
        bytes
    }

    #[test]
    fn any_host_matches_every_host_name() {
        assert_eq!(AnyHost, "api.example.com");
        assert_eq!(AnyHost, "localhost");
        assert_eq!(AnyHost, "");
    }

    #[test]
    fn cli_helpers_build_runtime_configuration() {
        let mut cli = test_cli();

        let model = model_config_from_cli(&cli);
        assert_eq!(model.family, "openai");
        assert_eq!(model.model, "gpt-test");
        assert_eq!(model.context_window, 128);
        assert_eq!(model.max_output, 64);
        assert!(!model.disabled);

        cli.model_api_key.clear();
        assert!(model_config_from_cli(&cli).disabled);

        let db = default_db_config();
        assert_eq!(db.name, "test");
        assert_eq!(db.storage.cache_max_capacity, 100000);
        assert_eq!(db.storage.object_chunk_size, 256 * 1024);

        let (app_state, _) = super::build_app_state(&test_cli()).unwrap();
        let _ = build_router(app_state, &test_cli(), CancellationToken::new());
        let _ = build_cors("");
        let _ = build_cors("*");
        let _ = build_cors("https://example.test, https://app.example.test");

        assert_eq!(normalize_http_path_prefix("mcp/"), "/mcp");
        assert_eq!(
            split_csv_values("localhost, brain.example.com, "),
            vec!["localhost", "brain.example.com"]
        );
        cli.mcp_http_path_prefix = "brain-mcp/".to_string();
        cli.mcp_http_allowed_hosts = "brain.example.com,127.0.0.1".to_string();
        cli.mcp_http_allowed_origins = "https://agents.example.com".to_string();
        cli.mcp_http_auto_create_space = true;
        let mcp = mcp_http_config_from_cli(&cli);
        assert_eq!(mcp.path_prefix, "/brain-mcp");
        assert_eq!(mcp.allowed_hosts.len(), 2);
        assert_eq!(mcp.allowed_origins, vec!["https://agents.example.com"]);
        assert!(mcp.auto_create_space);
    }

    #[test]
    fn build_service_runtime_wires_cli_into_app_state_and_router() {
        let mut cli = test_cli();
        cli.managers = SELF_USER_ID.to_string();
        cli.cors_origins = "*".to_string();

        let runtime = build_service_runtime(&cli, CancellationToken::new()).unwrap();

        assert_eq!(runtime.addr, "127.0.0.1:0".parse().unwrap());
        assert_eq!(runtime.db_type, "memory");
        assert_eq!(runtime.sharding_idx, 7);
        assert_eq!(runtime.managers, SELF_USER_ID.to_string());
        assert_eq!(runtime.model_name, "gpt-test");
        assert_eq!(runtime.app_state.app_name, "anda_brain");
        assert_eq!(runtime.app_state.sharding, 7);
        let _ = runtime.app;

        let mut invalid_addr = cli;
        invalid_addr.addr = "not an address".to_string();
        assert!(build_service_runtime(&invalid_addr, CancellationToken::new()).is_err());
    }

    #[test]
    fn parse_managers_accepts_empty_and_rejects_invalid_ids() {
        assert!(parse_managers("").unwrap().is_empty());

        let managers = parse_managers(&SELF_USER_ID.to_string()).unwrap();
        assert_eq!(managers.len(), 1);
        assert!(managers.contains(&SELF_USER_ID));

        assert!(parse_managers("not a principal").is_err());
    }

    #[test]
    fn build_http_client_accepts_default_config_and_rejects_bad_proxy() {
        let cli = test_cli();
        let _ = build_http_client(&cli).unwrap();

        let mut cli = test_cli();
        cli.https_proxy = Some("not a proxy url".to_string());
        assert!(build_http_client(&cli).is_err());
    }

    #[test]
    fn object_store_helper_builds_memory_and_local_backends() {
        let (_, db_type) = object_store_from_command(None).unwrap();
        assert_eq!(db_type, "memory");

        let suffix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!("anda-brain-local-store-{suffix}"));
        std::fs::create_dir_all(&path).unwrap();
        let (_, db_type) = object_store_from_command(Some(Commands::Local {
            db: path.to_string_lossy().to_string(),
        }))
        .unwrap();
        assert_eq!(db_type, "local");

        let aws = object_store_from_command(Some(Commands::Aws {
            bucket: "anda-brain-test-bucket".to_string(),
            region: "us-east-1".to_string(),
        }));
        if let Ok((_, db_type)) = aws {
            assert_eq!(db_type, "aws");
        }
    }

    #[test]
    fn parse_ed25519_pubkeys_accepts_comma_separated_raw_keys() {
        let key_bytes = ed25519_basepoint_bytes();
        let encoded = ByteBufB64(key_bytes.to_vec()).to_string();
        let keys = parse_ed25519_pubkeys(&format!("{encoded}, {encoded}")).unwrap();

        assert_eq!(keys.len(), 2);
        assert_eq!(keys[0].to_bytes(), key_bytes);
        assert_eq!(keys[1].to_bytes(), key_bytes);
    }

    #[test]
    fn parse_ed25519_pubkeys_accepts_cose_key_entries() {
        let key_bytes = ed25519_basepoint_bytes();
        let mut cose_key = CoseKey::new();
        cose_key.set_kty(iana::KeyTypeOKP);
        cose_key.insert(iana::OKPKeyParameterX, key_bytes.to_vec());
        let encoded = ByteBufB64(cose_key.to_vec().unwrap()).to_string();

        let keys = parse_ed25519_pubkeys(&encoded).unwrap();

        assert_eq!(keys.len(), 1);
        assert_eq!(keys[0].to_bytes(), key_bytes);
    }

    #[test]
    fn parse_ed25519_pubkeys_rejects_bad_binary_config() {
        let short_key = ByteBufB64(vec![1, 2, 3]).to_string();

        assert!(parse_ed25519_pubkeys("bad key").is_err());
        assert!(parse_ed25519_pubkeys(&short_key).is_err());
    }

    #[tokio::test]
    async fn create_reuse_port_listener_binds_ephemeral_port() {
        let listener = create_reuse_port_listener("127.0.0.1:0".parse().unwrap())
            .await
            .unwrap();
        let addr = listener.local_addr().unwrap();

        assert_eq!(addr.ip().to_string(), "127.0.0.1");
        assert_ne!(addr.port(), 0);
    }

    #[tokio::test]
    async fn run_service_exits_when_cancelled() {
        let cancel = CancellationToken::new();
        let runtime = build_service_runtime(&test_cli(), cancel.child_token()).unwrap();
        let cancel_after_start = cancel.clone();
        tokio::spawn(async move {
            sleep(Duration::from_millis(50)).await;
            cancel_after_start.cancel();
        });

        timeout(Duration::from_secs(2), run_service(runtime, cancel))
            .await
            .unwrap()
            .unwrap();
    }
}
