use anda_core::{BoxError, ModelEffort, Principal, Usage};
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
use std::{
    collections::BTreeSet, fmt::Write as _, net::SocketAddr, path::Path, sync::Arc, time::Duration,
};
use structured_logger::{Builder, async_json::new_writer, get_env_level};
use tokio::signal;
use tokio_util::sync::CancellationToken;
use tower_http::{
    compression::CompressionLayer,
    cors::{AllowHeaders, AllowMethods, CorsLayer},
};

use anda_brain::{
    agents::{SELF_USER_ID, prompts, prompts::PromptTarget},
    eval::{
        EvalExperimentReport, EvalGate, EvalGateReport, EvalProfile, EvalReport, EvalScenario,
        EvalScore, EvalSuiteReport, EvalValidationReport, EvalValidationSeverity,
        optimize::{OptimizeConfig, OptimizeReport, run_optimize},
        run_formation_phase, run_policy_phase, run_scenario, shared_formation_issues,
        validate_eval_plan,
    },
    handler::*,
    mcp::{McpHttpServerConfig, McpServerConfig, build_streamable_http_service, run_stdio_server},
    parse_ed25519_pubkeys,
    space::{AppState, copy_space_objects, delete_space_objects},
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
    Eval {
        /// Memory space used for this eval run
        #[arg(long, env = "EVAL_SPACE_ID", default_value = "eval")]
        space_id: String,

        /// Path to an EvalScenario JSON file. Repeat to run a suite.
        #[arg(long, env = "EVAL_SCENARIO", value_delimiter = ',', num_args = 1..)]
        scenario: Vec<String>,

        /// Optional path to an EvalProfile JSON file. Repeat to compare profiles.
        #[arg(long, env = "EVAL_PROFILE", value_delimiter = ',', num_args = 1..)]
        profile: Vec<String>,

        /// Optional path to write the EvalReport JSON. Defaults to stdout.
        #[arg(long, env = "EVAL_OUTPUT")]
        output: Option<String>,

        /// Fail the command if the aggregate total score is below this value
        #[arg(long = "min-score", env = "EVAL_MIN_SCORE")]
        min_score: Option<f64>,

        /// Fail the command if aggregate failure attribution exceeds this count
        #[arg(long = "max-findings", env = "EVAL_MAX_FINDINGS")]
        max_findings: Option<u64>,

        /// Validate scenario/profile inputs and print the planned eval without running models
        #[arg(
            long = "validate-only",
            env = "EVAL_VALIDATE_ONLY",
            default_value_t = false
        )]
        validate_only: bool,

        /// Print a compact human-readable summary instead of JSON
        #[arg(
            long = "summary-only",
            env = "EVAL_SUMMARY_ONLY",
            default_value_t = false
        )]
        summary_only: bool,

        /// Create the eval space before running if needed
        #[arg(long, env = "EVAL_AUTO_CREATE_SPACE", default_value_t = true)]
        auto_create_space: bool,

        /// Tier used when --auto-create-space creates the eval memory space
        #[arg(long, env = "EVAL_AUTO_CREATE_TIER", default_value_t = 1)]
        auto_create_tier: u32,

        /// Compare profiles on identical encoded memory: replay formation
        /// once per scenario, snapshot the space, and fork it per profile.
        /// Removes formation variance as a confound between profiles.
        #[arg(
            long = "shared-formation",
            env = "EVAL_SHARED_FORMATION",
            default_value_t = false
        )]
        shared_formation: bool,

        /// Override each profile's `checkpoint_samples` (recall samples per
        /// checkpoint; values above 1 enable mean±stddev reporting)
        #[arg(long = "checkpoint-samples", env = "EVAL_CHECKPOINT_SAMPLES")]
        checkpoint_samples: Option<usize>,

        /// Gate on `total - z * stddev` instead of the bare mean when a
        /// sample stddev is available
        #[arg(long = "confidence-z", env = "EVAL_CONFIDENCE_Z")]
        confidence_z: Option<f64>,

        /// Run the prompt optimize loop with the eval suite as fitness.
        /// Target: `formation` | `recall` | `maintenance` | `auto` (auto
        /// picks per generation from failure attribution)
        #[arg(long = "optimize", env = "EVAL_OPTIMIZE")]
        optimize: Option<String>,

        /// Number of propose→evaluate→select generations for --optimize
        #[arg(long = "generations", env = "EVAL_GENERATIONS", default_value_t = 3)]
        generations: usize,

        /// Directory for accepted prompts and the optimize report
        #[arg(
            long = "optimize-out",
            env = "EVAL_OPTIMIZE_OUT",
            default_value = "./eval_optimize"
        )]
        optimize_out: String,

        /// Keep the run-scoped eval spaces in the object store after the run
        /// (by default they are deleted once their report is collected)
        #[arg(
            long = "keep-spaces",
            env = "EVAL_KEEP_SPACES",
            default_value_t = false
        )]
        keep_spaces: bool,

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
            "/v1/{space_id}/wiki/docs",
            routing::post(post_wiki_commit).get(list_wiki_docs),
        )
        .route(
            "/v1/{space_id}/wiki/docs/{doc_id}",
            routing::get(get_wiki_doc),
        )
        .route(
            "/v1/{space_id}/wiki/docs/{doc_id}/content",
            routing::get(get_wiki_content),
        )
        .route(
            "/v1/{space_id}/wiki/docs/{doc_id}/versions",
            routing::get(list_wiki_versions),
        )
        .route(
            "/v1/{space_id}/wiki/docs/{doc_id}/archive",
            routing::post(post_wiki_archive),
        )
        .route(
            "/v1/{space_id}/wiki/docs/{doc_id}/restore",
            routing::post(post_wiki_restore),
        )
        .route(
            "/v1/{space_id}/wiki/search",
            routing::post(post_wiki_search),
        )
        .route(
            "/v1/{space_id}/wiki/verify",
            routing::post(post_wiki_verify),
        )
        .route("/v1/{space_id}/wiki/events", routing::get(list_wiki_events))
        .route(
            "/v1/{space_id}/wiki/import",
            routing::post(post_wiki_import),
        )
        .route("/v1/{space_id}/wiki/export", routing::get(get_wiki_export))
        .route(
            "/v1/{space_id}/wiki/digest",
            routing::post(post_wiki_digest),
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
        Some(Commands::Eval { storage, .. }) => storage,
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

#[derive(Clone)]
struct NamedEvalProfile {
    id: String,
    profile: EvalProfile,
}

struct EvalCommandConfig {
    space_id: String,
    scenario_paths: Vec<String>,
    profile_paths: Vec<String>,
    output_path: Option<String>,
    gate: EvalGate,
    validate_only: bool,
    summary_only: bool,
    auto_create_space: bool,
    auto_create_tier: u32,
    shared_formation: bool,
    checkpoint_samples: Option<usize>,
    optimize: Option<String>,
    generations: usize,
    optimize_out: String,
    keep_spaces: bool,
}

enum EvalCommandReport {
    Scenario(EvalReport),
    Suite(EvalSuiteReport),
    Experiment(EvalExperimentReport),
}

impl EvalCommandReport {
    fn evaluate_gate(&self, gate: &EvalGate) -> EvalGateReport {
        match self {
            Self::Scenario(report) => {
                gate.evaluate(&report.score, &report.attribution, report.total_stddev)
            }
            Self::Suite(report) => {
                gate.evaluate(&report.score, &report.attribution, report.total_stddev)
            }
            Self::Experiment(report) => {
                gate.evaluate(&report.score, &report.attribution, report.total_stddev)
            }
        }
    }

    fn attach_gate_report(&mut self, gate_report: EvalGateReport) {
        match self {
            Self::Scenario(report) => report.gate = Some(gate_report),
            Self::Suite(report) => report.gate = Some(gate_report),
            Self::Experiment(report) => report.gate = Some(gate_report),
        }
    }

    fn to_pretty_json(&self) -> Result<String, serde_json::Error> {
        match self {
            Self::Scenario(report) => serde_json::to_string_pretty(report),
            Self::Suite(report) => serde_json::to_string_pretty(report),
            Self::Experiment(report) => serde_json::to_string_pretty(report),
        }
    }

    fn to_summary(&self, gate_report: Option<&EvalGateReport>) -> String {
        let mut out = String::new();
        match self {
            Self::Scenario(report) => {
                writeln!(out, "Eval scenario {}", report.scenario_id).ok();
                append_score_summary(&mut out, &report.score);
                append_stddev_summary(&mut out, report.total_stddev);
                append_attribution_summary(&mut out, &report.attribution);
                append_usage_summary(&mut out, &report.usage);
                if !report.satisfaction_trajectory.is_empty() {
                    let trajectory: Vec<String> = report
                        .satisfaction_trajectory
                        .iter()
                        .map(|point| format!("{}:{:.2}", point.turn, point.satisfaction))
                        .collect();
                    writeln!(out, "satisfaction: {}", trajectory.join(" ")).ok();
                }
                writeln!(out, "turns: {}", report.turns.len()).ok();
            }
            Self::Suite(report) => {
                writeln!(out, "Eval suite {}", report.suite_id).ok();
                append_score_summary(&mut out, &report.score);
                append_stddev_summary(&mut out, report.total_stddev);
                append_attribution_summary(&mut out, &report.attribution);
                append_usage_summary(&mut out, &report.usage);
                writeln!(out, "scenarios: {}", report.reports.len()).ok();
                for scenario in &report.reports {
                    writeln!(
                        out,
                        "- {} total={:.4} findings={}",
                        scenario.scenario_id,
                        scenario.score.total,
                        scenario.attribution.total_findings()
                    )
                    .ok();
                }
            }
            Self::Experiment(report) => {
                writeln!(out, "Eval experiment {}", report.experiment_id).ok();
                append_score_summary(&mut out, &report.score);
                append_stddev_summary(&mut out, report.total_stddev);
                append_attribution_summary(&mut out, &report.attribution);
                append_usage_summary(&mut out, &report.usage);
                if !report.shared_formation.is_empty() {
                    let usage: u64 = report
                        .shared_formation
                        .iter()
                        .map(|shared| {
                            shared
                                .usage
                                .input_tokens
                                .saturating_add(shared.usage.output_tokens)
                        })
                        .sum();
                    writeln!(
                        out,
                        "shared_formation: {} scenario(s), {} tokens (excluded from suites)",
                        report.shared_formation.len(),
                        usage
                    )
                    .ok();
                }
                if let Some(best_suite_id) = &report.best_suite_id {
                    writeln!(out, "best_suite: {best_suite_id}").ok();
                }
                writeln!(out, "suites: {}", report.suites.len()).ok();
                for comparison in &report.comparisons {
                    writeln!(
                        out,
                        "- #{} {} total={:.4} delta={:.4} findings={} tokens={}",
                        comparison.rank,
                        comparison.suite_id,
                        comparison.score.total,
                        comparison.delta_from_best_total,
                        comparison.total_findings,
                        comparison.total_tokens
                    )
                    .ok();
                }
            }
        }

        if let Some(gate_report) = gate_report {
            append_gate_summary(&mut out, gate_report);
        }
        out
    }
}

async fn run_eval_command(cli: &Cli, config: EvalCommandConfig) -> Result<(), BoxError> {
    let EvalCommandConfig {
        space_id,
        scenario_paths,
        profile_paths,
        output_path,
        gate,
        validate_only,
        summary_only,
        auto_create_space,
        auto_create_tier,
        shared_formation,
        checkpoint_samples,
        optimize,
        generations,
        optimize_out,
        keep_spaces,
    } = config;

    if scenario_paths.is_empty() {
        return Err("at least one --scenario is required".into());
    }

    let scenarios: Vec<EvalScenario> = scenario_paths
        .iter()
        .map(|path| read_json_file::<EvalScenario>(path))
        .collect::<Result<_, _>>()?;
    let mut profiles = read_eval_profiles(&profile_paths)?;
    if let Some(samples) = checkpoint_samples {
        for profile in &mut profiles {
            profile.profile.checkpoint_samples = samples;
        }
    }
    let profile_values: Vec<EvalProfile> = profiles
        .iter()
        .map(|profile| profile.profile.clone())
        .collect();
    let mut validation = validate_eval_plan(&scenarios, &profile_values);
    if shared_formation {
        let issues = shared_formation_issues(&scenarios);
        if !issues.is_empty() {
            validation.issues.extend(issues);
            validation.passed = !validation.has_errors();
        }
    }
    let optimize_target = optimize.as_deref().map(parse_optimize_target).transpose()?;

    if validate_only {
        let report = if summary_only {
            validation_summary(&validation)
        } else {
            serde_json::to_string_pretty(&validation)?
        };
        match output_path {
            Some(path) => std::fs::write(path, report)?,
            None => println!("{report}"),
        }

        if !validation.passed {
            return Err(eval_validation_error(&validation).into());
        }

        return Ok(());
    }

    if !validation.passed {
        return Err(eval_validation_error(&validation).into());
    }

    let (app_state, _) = build_app_state(cli)?;

    if let Some(target) = optimize_target {
        if profiles.len() != 1 {
            return Err("--optimize requires exactly one --profile".into());
        }
        if gate.is_configured() {
            return Err(
                "--min-score/--max-findings are not applied by --optimize; gate a separate eval run on the accepted prompts instead"
                    .into(),
            );
        }
        return run_optimize_command(
            &app_state,
            &space_id,
            &profiles[0],
            &scenarios,
            auto_create_space,
            auto_create_tier,
            target,
            generations,
            gate.confidence_z,
            &optimize_out,
            output_path,
            summary_only,
            keep_spaces,
        )
        .await;
    }

    let run_id = anda_engine::unix_ms();
    let mut report = if shared_formation {
        let experiment = run_shared_formation_experiment(
            &app_state,
            &space_id,
            &profiles,
            &scenarios,
            auto_create_space,
            auto_create_tier,
            run_id,
            keep_spaces,
        )
        .await?;
        EvalCommandReport::Experiment(experiment)
    } else if scenarios.len() == 1 && profiles.len() == 1 {
        // Run-scoped id, matching the suite path: reusing a space across runs
        // would let leftover memories from a previous run leak into scores.
        let scenario_space_id = format!(
            "{}_{}_{}_{}",
            space_id,
            sanitize_space_id_part(&profiles[0].id),
            sanitize_space_id_part(&scenarios[0].id),
            run_id
        );
        let space = load_eval_space(
            &app_state,
            &scenario_space_id,
            auto_create_space,
            auto_create_tier,
        )
        .await?;
        let report = run_scenario(space.as_ref(), &scenarios[0], &profiles[0].profile).await?;
        space.db.close().await?;
        cleanup_eval_space(&app_state, &scenario_space_id, keep_spaces).await;
        EvalCommandReport::Scenario(report)
    } else if profiles.len() == 1 {
        let suite = run_eval_suite(
            &app_state,
            &space_id,
            &profiles[0],
            &scenarios,
            auto_create_space,
            auto_create_tier,
            run_id,
            keep_spaces,
        )
        .await?;
        EvalCommandReport::Suite(suite)
    } else {
        let mut suites = Vec::with_capacity(profiles.len());
        for profile in &profiles {
            let suite = run_eval_suite(
                &app_state,
                &space_id,
                profile,
                &scenarios,
                auto_create_space,
                auto_create_tier,
                run_id,
                keep_spaces,
            )
            .await?;
            suites.push(suite);
        }
        let experiment = EvalExperimentReport::from_suites(space_id, suites);
        EvalCommandReport::Experiment(experiment)
    };

    let gate_report = report.evaluate_gate(&gate);
    if gate.is_configured() {
        report.attach_gate_report(gate_report.clone());
    }
    let report_output = if summary_only {
        report.to_summary(gate.is_configured().then_some(&gate_report))
    } else {
        report.to_pretty_json()?
    };

    match output_path {
        Some(path) => std::fs::write(path, report_output)?,
        None => println!("{report_output}"),
    }

    if !gate_report.passed {
        return Err(format!("eval gate failed: {}", gate_report.failures.join("; ")).into());
    }

    Ok(())
}

fn eval_validation_error(report: &EvalValidationReport) -> String {
    let errors: Vec<String> = report
        .issues
        .iter()
        .filter(|issue| issue.severity == anda_brain::eval::EvalValidationSeverity::Error)
        .take(5)
        .map(|issue| format!("{}: {}", issue.path, issue.message))
        .collect();

    if errors.is_empty() {
        "eval validation failed".to_string()
    } else {
        format!("eval validation failed: {}", errors.join("; "))
    }
}

fn validation_summary(report: &EvalValidationReport) -> String {
    let mut out = String::new();
    writeln!(
        out,
        "Eval validation {}",
        if report.passed { "passed" } else { "failed" }
    )
    .ok();
    writeln!(out, "planned_runs: {}", report.planned_runs).ok();
    writeln!(out, "scenarios: {}", report.scenarios.len()).ok();
    for scenario in &report.scenarios {
        writeln!(
            out,
            "- {} normal={} checkpoint={} maintenance={} memories={} probes={} simulated={} noise={} assertions={}",
            scenario.id,
            scenario.normal_turns,
            scenario.checkpoint_turns,
            scenario.maintenance_turns,
            scenario.expected_memories,
            scenario.probes,
            scenario.simulated_turns,
            scenario.noise_turns,
            scenario.assertions
        )
        .ok();
    }
    writeln!(out, "profiles: {}", report.profiles.len()).ok();
    for profile in &report.profiles {
        let cadence = profile
            .maintenance_every_n_turns
            .map(|turns| format!("every_{turns}_turns"))
            .unwrap_or_else(|| "manual".to_string());
        writeln!(
            out,
            "- {} maintenance={} scope={} timeout_ms={} poll_ms={} samples={} judge={:?}",
            profile.id,
            cadence,
            profile.maintenance_scope,
            profile.wait_timeout_ms,
            profile.poll_interval_ms,
            profile.checkpoint_samples,
            profile.judge
        )
        .ok();
    }
    append_validation_issues_summary(&mut out, report);
    out
}

fn append_validation_issues_summary(out: &mut String, report: &EvalValidationReport) {
    let errors = report
        .issues
        .iter()
        .filter(|issue| issue.severity == EvalValidationSeverity::Error)
        .count();
    let warnings = report.issues.len().saturating_sub(errors);
    writeln!(out, "issues: errors={errors} warnings={warnings}").ok();
    for issue in &report.issues {
        writeln!(
            out,
            "- {:?} {}: {}",
            issue.severity, issue.path, issue.message
        )
        .ok();
    }
}

fn append_score_summary(out: &mut String, score: &EvalScore) {
    writeln!(
        out,
        "score: total={:.4} memory={:.4} evolution={:.4} uncertainty={:.4} forgetting={:.4} graph={:.4} latency_penalty={:.4} token_penalty={:.4}",
        score.total,
        score.memory_utility,
        score.evolution_quality,
        score.uncertainty_calibration,
        score.forgetting_quality,
        score.graph_health,
        score.latency_penalty,
        score.token_cost_penalty
    )
    .ok();
}

fn append_stddev_summary(out: &mut String, total_stddev: Option<f64>) {
    if let Some(stddev) = total_stddev {
        writeln!(out, "total_stddev: {stddev:.4}").ok();
    }
}

fn append_attribution_summary(
    out: &mut String,
    attribution: &anda_brain::eval::AttributionSummary,
) {
    writeln!(
        out,
        "findings: total={} formation_miss={} bad_consolidation={} bad_grounding={} bad_synthesis={} overconfidence={} graph_probe_error={} latency_cost={} token_cost={} judge_error={}",
        attribution.total_findings(),
        attribution.formation_miss,
        attribution.bad_consolidation,
        attribution.bad_grounding,
        attribution.bad_synthesis,
        attribution.overconfidence,
        attribution.graph_probe_error,
        attribution.latency_cost,
        attribution.token_cost,
        attribution.judge_error
    )
    .ok();
}

fn append_usage_summary(out: &mut String, usage: &Usage) {
    writeln!(
        out,
        "usage: input_tokens={} output_tokens={} cached_tokens={} requests={}",
        usage.input_tokens, usage.output_tokens, usage.cached_tokens, usage.requests
    )
    .ok();
}

fn append_gate_summary(out: &mut String, gate_report: &EvalGateReport) {
    writeln!(
        out,
        "gate: {} min_score={} max_findings={}",
        if gate_report.passed {
            "passed"
        } else {
            "failed"
        },
        gate_report
            .criteria
            .min_total_score
            .map(|score| format!("{score:.4}"))
            .unwrap_or_else(|| "none".to_string()),
        gate_report
            .criteria
            .max_total_findings
            .map(|findings| findings.to_string())
            .unwrap_or_else(|| "none".to_string())
    )
    .ok();
    for failure in &gate_report.failures {
        writeln!(out, "- {failure}").ok();
    }
}

#[allow(clippy::too_many_arguments)]
async fn run_eval_suite(
    app_state: &AppState,
    base_space_id: &str,
    profile: &NamedEvalProfile,
    scenarios: &[EvalScenario],
    auto_create_space: bool,
    auto_create_tier: u32,
    run_id: u64,
    keep_spaces: bool,
) -> Result<EvalSuiteReport, BoxError> {
    let mut reports = Vec::with_capacity(scenarios.len());
    for scenario in scenarios {
        let scenario_space_id = format!(
            "{}_{}_{}_{}",
            base_space_id,
            sanitize_space_id_part(&profile.id),
            sanitize_space_id_part(&scenario.id),
            run_id
        );
        let space = load_eval_space(
            app_state,
            &scenario_space_id,
            auto_create_space,
            auto_create_tier,
        )
        .await?;
        let report = run_scenario(space.as_ref(), scenario, &profile.profile).await?;
        space.db.close().await?;
        cleanup_eval_space(app_state, &scenario_space_id, keep_spaces).await;
        reports.push(report);
    }

    Ok(EvalSuiteReport::from_reports(profile.id.clone(), reports))
}

/// Best-effort removal of a run-scoped eval space once its report is
/// collected; a cleanup failure must never fail the eval itself.
async fn cleanup_eval_space(app_state: &AppState, space_id: &str, keep_spaces: bool) {
    if keep_spaces {
        return;
    }
    app_state.evict_space(space_id).await;
    if let Err(err) = delete_space_objects(&app_state.object_store(), space_id).await {
        // The eval command reserves stdout for reports and skips the
        // structured logger, so warn on stderr.
        eprintln!("warning: failed to clean up eval space {space_id}: {err}");
    }
}

fn parse_optimize_target(target: &str) -> Result<Option<PromptTarget>, BoxError> {
    match target.trim().to_lowercase().as_str() {
        "auto" => Ok(None),
        "formation" => Ok(Some(PromptTarget::Formation)),
        "recall" => Ok(Some(PromptTarget::Recall)),
        "maintenance" => Ok(Some(PromptTarget::Maintenance)),
        other => Err(format!(
            "invalid --optimize target `{other}`; expected formation|recall|maintenance|auto"
        )
        .into()),
    }
}

/// Shared-formation experiment: replay formation once per scenario into a
/// base space, snapshot its objects, then fork the snapshot into a fresh
/// in-memory store per profile and run only maintenance + checkpoints there.
/// Every profile is judged on the identical encoded memory, so differences
/// between suites measure the policy — not formation's LLM variance — and
/// the most expensive phase runs once instead of once per profile.
#[allow(clippy::too_many_arguments)]
async fn run_shared_formation_experiment(
    app_state: &AppState,
    base_space_id: &str,
    profiles: &[NamedEvalProfile],
    scenarios: &[EvalScenario],
    auto_create_space: bool,
    auto_create_tier: u32,
    run_id: u64,
    keep_spaces: bool,
) -> Result<EvalExperimentReport, BoxError> {
    let mut shared_reports = Vec::with_capacity(scenarios.len());
    let mut profile_reports: Vec<Vec<EvalReport>> = vec![Vec::new(); profiles.len()];

    for scenario in scenarios {
        let base_id = format!(
            "{}_form_{}_{}",
            base_space_id,
            sanitize_space_id_part(&scenario.id),
            run_id
        );
        let space =
            load_eval_space(app_state, &base_id, auto_create_space, auto_create_tier).await?;
        // The formation phase only reads timeouts from the profile.
        let report = run_formation_phase(space.as_ref(), scenario, &profiles[0].profile).await?;
        space.db.close().await?;
        shared_reports.push(report);

        for (index, profile) in profiles.iter().enumerate() {
            let fork_store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
            copy_space_objects(&app_state.object_store(), &fork_store, &base_id).await?;
            let fork_state = app_state.fork_with_store(fork_store);
            let fork_space = fork_state.load_space(&base_id, true).await?;
            let report = run_policy_phase(fork_space.as_ref(), scenario, &profile.profile).await?;
            fork_space.db.close().await?;
            profile_reports[index].push(report);
        }
        // The base snapshot is only needed until every profile has forked it.
        // (Forks live in their own in-memory stores and vanish on drop.)
        cleanup_eval_space(app_state, &base_id, keep_spaces).await;
    }

    let suites: Vec<EvalSuiteReport> = profiles
        .iter()
        .zip(profile_reports)
        .map(|(profile, reports)| EvalSuiteReport::from_reports(profile.id.clone(), reports))
        .collect();
    let mut experiment = EvalExperimentReport::from_suites(base_space_id.to_string(), suites);
    experiment.shared_formation = shared_reports;
    Ok(experiment)
}

/// The optimize loop: eval suite as fitness, agent prompts as genome. Each
/// generation proposes targeted edits from attributed failures, re-runs the
/// suite on fresh spaces, and keeps the edit only when it clears the noise
/// band. Accepted prompts and the full decision log land in `out_dir` for
/// human review; nothing is written back to `assets/`.
#[allow(clippy::too_many_arguments)]
async fn run_optimize_command(
    app_state: &AppState,
    base_space_id: &str,
    profile: &NamedEvalProfile,
    scenarios: &[EvalScenario],
    auto_create_space: bool,
    auto_create_tier: u32,
    target: Option<PromptTarget>,
    generations: usize,
    confidence_z: Option<f64>,
    out_dir: &str,
    output_path: Option<String>,
    summary_only: bool,
    keep_spaces: bool,
) -> Result<(), BoxError> {
    let run_id = anda_engine::unix_ms();
    // Scratch space whose model powers the optimizer's proposal calls.
    let proposer_id = format!("{base_space_id}_optimizer_{run_id}");
    let proposer =
        load_eval_space(app_state, &proposer_id, auto_create_space, auto_create_tier).await?;

    let mut config = OptimizeConfig {
        generations,
        target,
        ..Default::default()
    };
    if let Some(z) = confidence_z {
        config.confidence_z = z;
    }
    let fitness_state = app_state.clone();
    let fitness_profile = profile.clone();
    let fitness_scenarios = scenarios.to_vec();
    let fitness_base = base_space_id.to_string();
    let report = run_optimize(proposer.as_ref(), &config, move |generation| {
        let app_state = fitness_state.clone();
        let profile = fitness_profile.clone();
        let scenarios = fitness_scenarios.clone();
        let base = format!("{fitness_base}_g{generation}");
        async move {
            run_eval_suite(
                &app_state,
                &base,
                &profile,
                &scenarios,
                auto_create_space,
                auto_create_tier,
                run_id,
                keep_spaces,
            )
            .await
        }
    })
    .await;
    // Leave the process with pristine prompts regardless of the outcome.
    let close_result = proposer.db.close().await;
    prompts::clear_overrides();
    cleanup_eval_space(app_state, &proposer_id, keep_spaces).await;
    let report = match report {
        Ok(report) => report,
        Err(err) => return Err(err),
    };
    close_result?;

    std::fs::create_dir_all(out_dir)?;
    for accepted in &report.accepted_prompts {
        let filename = match accepted.target {
            PromptTarget::Formation => "BrainFormation.md",
            PromptTarget::Recall => "BrainRecall.md",
            PromptTarget::Maintenance => "BrainMaintenance.md",
        };
        std::fs::write(Path::new(out_dir).join(filename), &accepted.text)?;
    }
    let report_json = serde_json::to_string_pretty(&report)?;
    std::fs::write(
        Path::new(out_dir).join("optimize_report.json"),
        &report_json,
    )?;

    let report_output = if summary_only {
        optimize_summary(&report, out_dir)
    } else {
        report_json
    };
    match output_path {
        Some(path) => std::fs::write(path, report_output)?,
        None => println!("{report_output}"),
    }
    Ok(())
}

fn optimize_summary(report: &OptimizeReport, out_dir: &str) -> String {
    let mut out = String::new();
    writeln!(
        out,
        "Optimize: baseline={:.4} final={:.4} accepted={}/{}",
        report.baseline_total,
        report.final_total,
        report.accepted_generations,
        report.generations.len()
    )
    .ok();
    for generation in &report.generations {
        writeln!(
            out,
            "- gen {} target={} candidate={} {} ({})",
            generation.generation,
            generation.target.as_str(),
            generation
                .candidate_total
                .map(|total| format!("{total:.4}"))
                .unwrap_or_else(|| "-".to_string()),
            if generation.decision.accepted {
                "accepted"
            } else {
                "rejected"
            },
            generation.decision.reason
        )
        .ok();
    }
    if !report.accepted_prompts.is_empty() {
        writeln!(out, "accepted prompts written to {out_dir}").ok();
    }
    out
}

fn read_eval_profiles(paths: &[String]) -> Result<Vec<NamedEvalProfile>, BoxError> {
    if paths.is_empty() {
        let profile = EvalProfile {
            id: Some("default".to_string()),
            ..Default::default()
        };
        return Ok(vec![NamedEvalProfile {
            id: "default".to_string(),
            profile,
        }]);
    }

    paths
        .iter()
        .map(|path| {
            let mut profile = read_json_file::<EvalProfile>(path)?;
            let id = profile
                .id
                .clone()
                .unwrap_or_else(|| profile_id_from_path(path));
            profile.id = Some(id.clone());
            Ok(NamedEvalProfile { id, profile })
        })
        .collect()
}

async fn load_eval_space(
    app_state: &AppState,
    space_id: &str,
    auto_create_space: bool,
    auto_create_tier: u32,
) -> Result<Arc<anda_brain::space::Space>, BoxError> {
    if auto_create_space {
        match app_state
            .admin_create_space(
                SELF_USER_ID,
                SELF_USER_ID,
                space_id.to_string(),
                auto_create_tier,
                anda_engine::unix_ms(),
            )
            .await
        {
            Ok(_) => {}
            Err(err) => {
                // Existing local eval spaces are valid; `load_space` below is
                // the authority on whether this run can proceed.
                log::debug!(target: "brain", space_id = space_id; "eval space create skipped: {err:?}");
            }
        }
    }

    app_state.load_space(space_id, true).await
}

fn profile_id_from_path(path: &str) -> String {
    Path::new(path)
        .file_stem()
        .and_then(|value| value.to_str())
        .map(sanitize_space_id_part)
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "profile".to_string())
}

fn sanitize_space_id_part(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for ch in value.chars() {
        if ch.is_ascii_alphanumeric() || ch == '_' || ch == '-' {
            out.push(ch);
        } else {
            out.push('_');
        }
    }

    let out = out.trim_matches('_');
    if out.is_empty() {
        "scenario".to_string()
    } else {
        out.to_string()
    }
}

fn read_json_file<T>(path: &str) -> Result<T, BoxError>
where
    T: serde::de::DeserializeOwned,
{
    let data = std::fs::read(path)?;
    Ok(serde_json::from_slice(&data)?)
}

/// ```bash
/// cargo run -p anda_brain
/// ```
// grcov-excl-start: main is a thin CLI/logging wrapper; build_service_runtime and run_service are unit-tested.
#[tokio::main]
async fn main() -> Result<(), BoxError> {
    dotenv::dotenv().ok();
    let cli = Cli::parse();

    if !matches!(
        cli.command,
        Some(Commands::Mcp { .. } | Commands::Eval { .. })
    ) {
        // Initialize structured logging with JSON format. MCP stdio keeps stdout reserved
        // for JSON-RPC messages. Eval uses stdout for JSON reports when no output path is set.
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
        Some(Commands::Eval {
            space_id,
            scenario,
            profile,
            output,
            min_score,
            max_findings,
            validate_only,
            summary_only,
            auto_create_space,
            auto_create_tier,
            shared_formation,
            checkpoint_samples,
            confidence_z,
            optimize,
            generations,
            optimize_out,
            keep_spaces,
            ..
        }) => {
            run_eval_command(
                &cli,
                EvalCommandConfig {
                    space_id,
                    scenario_paths: scenario,
                    profile_paths: profile,
                    output_path: output,
                    gate: EvalGate {
                        min_total_score: min_score,
                        max_total_findings: max_findings,
                        confidence_z,
                    },
                    validate_only,
                    summary_only,
                    auto_create_space,
                    auto_create_tier,
                    shared_formation,
                    checkpoint_samples,
                    optimize,
                    generations,
                    optimize_out,
                    keep_spaces,
                },
            )
            .await
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
        AnyHost, Cli, Commands, EvalCommandConfig, EvalCommandReport, StorageCommand, build_cors,
        build_http_client, build_router, build_service_runtime, create_reuse_port_listener,
        default_db_config, mcp_http_config_from_cli, model_config_from_cli,
        normalize_http_path_prefix, object_store_from_command, parse_ed25519_pubkeys,
        parse_managers, read_json_file, run_eval_command, run_service, split_csv_values,
    };
    use anda_brain::agents::SELF_USER_ID;
    use anda_brain::eval::{AttributionSummary, EvalGate, EvalReport, EvalScenario, EvalScore};
    use cose2::{Key as CoseKey, iana};
    use ic_auth_types::ByteBufB64;
    use serde_json::Value;
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

        let (_, db_type) = object_store_from_command(Some(Commands::Eval {
            space_id: "eval_space".to_string(),
            scenario: vec!["scenario.json".to_string()],
            profile: Vec::new(),
            output: None,
            min_score: None,
            max_findings: None,
            validate_only: false,
            summary_only: false,
            auto_create_space: true,
            auto_create_tier: 1,
            shared_formation: false,
            checkpoint_samples: None,
            confidence_z: None,
            optimize: None,
            generations: 3,
            optimize_out: "./eval_optimize".to_string(),
            keep_spaces: false,
            storage: Some(StorageCommand::Local {
                db: path.to_string_lossy().to_string(),
            }),
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
    fn read_json_file_loads_eval_scenario() {
        let suffix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!("anda-brain-eval-scenario-{suffix}.json"));
        std::fs::write(
            &path,
            r#"{"id":"scenario","hidden_profile":{},"timeline":[]}"#,
        )
        .unwrap();

        let scenario: EvalScenario = read_json_file(path.to_str().unwrap()).unwrap();

        assert_eq!(scenario.id, "scenario");
        assert!(scenario.timeline.is_empty());
    }

    #[test]
    fn eval_command_report_serializes_gate_artifact() {
        let gate = EvalGate {
            min_total_score: Some(0.9),
            max_total_findings: Some(0),
            confidence_z: None,
        };
        let mut command_report = EvalCommandReport::Scenario(EvalReport {
            scenario_id: "scenario".to_string(),
            score: EvalScore {
                total: 0.5,
                ..Default::default()
            },
            attribution: AttributionSummary {
                bad_grounding: 1,
                ..Default::default()
            },
            ..Default::default()
        });
        let gate_report = command_report.evaluate_gate(&gate);

        assert!(!gate_report.passed);
        command_report.attach_gate_report(gate_report);
        let json: Value = serde_json::from_str(&command_report.to_pretty_json().unwrap()).unwrap();

        assert_eq!(json["gate"]["passed"], false);
        assert_eq!(json["gate"]["criteria"]["min_total_score"], 0.9);
        assert_eq!(json["gate"]["criteria"]["max_total_findings"], 0);
        assert_eq!(json["gate"]["failures"].as_array().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn eval_validate_only_writes_validation_report_without_running_models() {
        let suffix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("anda-brain-eval-validate-{suffix}"));
        std::fs::create_dir_all(&dir).unwrap();
        let scenario_path = dir.join("scenario.json");
        let profile_path = dir.join("profile.json");
        let output_path = dir.join("validation.json");
        std::fs::write(
            &scenario_path,
            r#"{
              "id": "invalid",
              "hidden_profile": {},
              "timeline": [{
                "turn": 1,
                "type": "checkpoint_synthetic",
                "query": "What do I prefer?",
                "evaluation": {
                  "expected_memories": [{
                    "id": "pref",
                    "probe": {
                      "command": "SEARCH CONCEPT \"preference\" MODE \"semantic\" LIMIT 1"
                    }
                  }]
                }
              }]
            }"#,
        )
        .unwrap();
        std::fs::write(
            &profile_path,
            r#"{"id":"bad_profile","maintenance_every_n_turns":0}"#,
        )
        .unwrap();

        let mut cli = test_cli();
        cli.model_api_key = String::new();
        let result = run_eval_command(
            &cli,
            EvalCommandConfig {
                space_id: "validate".to_string(),
                scenario_paths: vec![scenario_path.to_string_lossy().to_string()],
                profile_paths: vec![profile_path.to_string_lossy().to_string()],
                output_path: Some(output_path.to_string_lossy().to_string()),
                gate: EvalGate::default(),
                validate_only: true,
                summary_only: false,
                auto_create_space: false,
                auto_create_tier: 1,
                shared_formation: false,
                checkpoint_samples: None,
                optimize: None,
                generations: 3,
                optimize_out: "./eval_optimize".to_string(),
                keep_spaces: false,
            },
        )
        .await;

        assert!(result.is_err());
        let json: Value = serde_json::from_slice(&std::fs::read(output_path).unwrap()).unwrap();
        assert_eq!(json["passed"], false);
        assert_eq!(json["planned_runs"], 1);
        assert_eq!(json["scenarios"][0]["id"], "invalid");
        assert!(json["issues"].as_array().unwrap().len() >= 2);
    }

    #[tokio::test]
    async fn eval_validate_only_summary_outputs_human_readable_plan() {
        let suffix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("anda-brain-eval-summary-{suffix}"));
        std::fs::create_dir_all(&dir).unwrap();
        let scenario_path = dir.join("scenario.json");
        let output_path = dir.join("summary.txt");
        std::fs::write(
            &scenario_path,
            r#"{
              "id": "summary",
              "hidden_profile": {},
              "timeline": [{
                "turn": 1,
                "type": "checkpoint_synthetic",
                "query": "What should I remember?",
                "evaluation": {
                  "required_answer_terms": ["direct"]
                }
              }]
            }"#,
        )
        .unwrap();

        let mut cli = test_cli();
        cli.model_api_key = String::new();
        run_eval_command(
            &cli,
            EvalCommandConfig {
                space_id: "validate".to_string(),
                scenario_paths: vec![scenario_path.to_string_lossy().to_string()],
                profile_paths: Vec::new(),
                output_path: Some(output_path.to_string_lossy().to_string()),
                gate: EvalGate::default(),
                validate_only: true,
                summary_only: true,
                auto_create_space: false,
                auto_create_tier: 1,
                shared_formation: false,
                checkpoint_samples: None,
                optimize: None,
                generations: 3,
                optimize_out: "./eval_optimize".to_string(),
                keep_spaces: false,
            },
        )
        .await
        .unwrap();

        let summary = std::fs::read_to_string(output_path).unwrap();
        assert!(summary.contains("Eval validation passed"));
        assert!(summary.contains("planned_runs: 1"));
        assert!(summary.contains("- summary normal=0 checkpoint=1"));
        assert!(summary.contains("- default maintenance=manual"));
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
