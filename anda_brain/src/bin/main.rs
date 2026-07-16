use anda_core::{BoxError, ModelEffort, Principal, Usage};
use anda_db::{database::DBConfig, storage::StorageConfig};
use anda_engine::{
    management::{BaseManagement, Visibility},
    model::{ModelConfig, Models, Proxy, request_client_builder, reqwest},
};
use anda_object_store::MetaStoreBuilder;
use axum::{Router, error_handling::HandleErrorLayer, routing};
use clap::{Parser, Subcommand};
use http::StatusCode;
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
use tokio::{signal, task::JoinHandle};
use tokio_util::sync::CancellationToken;
use tower::{ServiceBuilder, limit::GlobalConcurrencyLimitLayer, load_shed::LoadShedLayer};
use tower_http::{
    compression::CompressionLayer,
    cors::{AllowHeaders, AllowMethods, CorsLayer},
};

use anda_brain::{
    agents::{SELF_USER_ID, prompts, prompts::PromptTarget},
    eval::{
        EvalExperimentReport, EvalFinding, EvalFindingKind, EvalGate, EvalGateReport, EvalProfile,
        EvalReport, EvalScenario, EvalScore, EvalSuiteReport, EvalTurnReport, EvalValidationReport,
        EvalValidationSeverity,
        mine::{MineConfig, mine_scenarios},
        optimize::{BoxedFitness, GenomeKind, OptimizeConfig, OptimizeReport, run_optimize},
        run_formation_phase, run_policy_phase, run_scenario, shared_formation_issues,
        validate_eval_plan,
    },
    handler::*,
    mcp::{McpHttpServerConfig, McpServerConfig, build_streamable_http_service, run_stdio_server},
    parse_ed25519_pubkeys,
    space::{AppState, copy_space_objects},
    types::{MemoryPolicy, ModelConfig as BrainModelConfig},
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

    /// CORS allowed origins, separated by comma. Use "*" to allow all
    /// origins — note that "*" maps to CorsLayer::very_permissive(), which
    /// answers any origin with Access-Control-Allow-Credentials: true, i.e.
    /// any website may issue credentialed cross-site requests to this API.
    /// The API carries credentials in Bearer headers (never cookies), so
    /// browsers won't attach them automatically, but prefer an explicit
    /// origin list on deployments that don't need fully open CORS.
    #[arg(long, env = "CORS_ORIGINS", default_value = "")]
    cors_origins: String,

    /// Global cap on in-flight HTTP requests; excess requests are shed with
    /// 503 instead of queueing without bound. The default is deliberately
    /// generous so normal multi-tenant traffic never hits it; it only bounds
    /// pathological floods.
    #[arg(long, env = "HTTP_MAX_CONCURRENCY", default_value_t = 1024)]
    http_max_concurrency: usize,

    /// Cap on concurrent LLM-billed requests (formation, recall,
    /// recall_structured, maintenance, shadow_eval, wiki digest); excess is
    /// shed with 429. Each such request can drive a full multi-turn model
    /// round (recall may run for over a minute), so this bounds worst-case
    /// model spend from anonymous callers on public spaces. The default is
    /// loose enough for normal multi-tenant use.
    #[arg(long, env = "LLM_MAX_CONCURRENCY", default_value_t = 64)]
    llm_max_concurrency: usize,

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

// A CLI enum is constructed exactly once; boxing the Eval variant would only
// obscure the clap derive.
#[allow(clippy::large_enum_variant)]
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

        /// Tier used when creating the run-scoped eval memory spaces
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

        /// Z multiplier for the --optimize accept noise band (default 1.0)
        #[arg(long = "confidence-z", env = "EVAL_CONFIDENCE_Z")]
        confidence_z: Option<f64>,

        /// Run the optimize loop with the eval suite as fitness. Genome:
        /// `formation` | `recall` | `maintenance` | `auto` (prompt edits;
        /// auto picks the target per generation from failure attribution)
        /// or `policy` (bounded numeric MemoryPolicy mutations)
        #[arg(long = "optimize", env = "EVAL_OPTIMIZE")]
        optimize: Option<String>,

        /// Number of propose→evaluate→select generations for --optimize
        #[arg(long = "generations", env = "EVAL_GENERATIONS", default_value_t = 3)]
        generations: usize,

        /// Held-out scenario file(s) for --optimize: a train win must not
        /// regress this suite (anti-overfitting gate). Repeatable.
        #[arg(
            long = "holdout-scenario",
            env = "EVAL_HOLDOUT_SCENARIO",
            value_delimiter = ',',
            num_args = 1..
        )]
        holdout_scenario: Vec<String>,

        /// Independent judge model family (used when the API key is set)
        #[arg(
            long = "judge-model-family",
            env = "JUDGE_MODEL_FAMILY",
            default_value = "openai"
        )]
        judge_model_family: String,

        /// Independent judge model name
        #[arg(
            long = "judge-model-name",
            env = "JUDGE_MODEL_NAME",
            default_value = ""
        )]
        judge_model_name: String,

        /// API key for the independent judge model; empty disables it (the
        /// judge then shares the evaluated system's model)
        #[arg(
            long = "judge-model-api-key",
            env = "JUDGE_MODEL_API_KEY",
            default_value = ""
        )]
        judge_model_api_key: String,

        /// API base for the independent judge model
        #[arg(
            long = "judge-model-api-base",
            env = "JUDGE_MODEL_API_BASE",
            default_value = ""
        )]
        judge_model_api_base: String,

        /// Mine eval scenarios from a real space's correction ledger instead
        /// of running scenarios (mutually exclusive with --scenario and
        /// --optimize; the space must already exist)
        #[arg(long = "mine", env = "EVAL_MINE", default_value_t = false)]
        mine: bool,

        /// Review directory mined scenarios are written to
        #[arg(
            long = "mine-out",
            env = "EVAL_MINE_OUT",
            default_value = "./anda_brain/evals/mined"
        )]
        mine_out: String,

        /// Only corrections observed within this many days are mined
        #[arg(long = "since-days", env = "EVAL_SINCE_DAYS", default_value_t = 30)]
        since_days: u32,

        /// Max scenarios produced per mining run
        #[arg(
            long = "max-scenarios",
            env = "EVAL_MAX_SCENARIOS",
            default_value_t = 8
        )]
        max_scenarios: usize,

        /// Directory for accepted prompts and the optimize report
        #[arg(
            long = "optimize-out",
            env = "EVAL_OPTIMIZE_OUT",
            default_value = "./eval_optimize"
        )]
        optimize_out: String,

        /// Keep the run-scoped eval spaces in the real object store after
        /// the run (by default each lives in a throwaway in-memory store
        /// that vanishes once its report is collected)
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

#[cfg(test)]
#[derive(Clone, Copy, Debug)]
struct AnyHost;

#[cfg(test)]
impl PartialEq<&str> for AnyHost {
    fn eq(&self, _other: &&str) -> bool {
        true
    }
}

fn build_http_client(cli: &Cli) -> Result<reqwest::Client, BoxError> {
    let mut http_client = request_client_builder()
        .https_only(false)
        .timeout(Duration::from_secs(600));
    // grcov-excl-stop
    if let Some(proxy) = &cli.https_proxy {
        http_client = http_client.proxy(Proxy::all(proxy)?);
    }
    Ok(http_client.build()?)
}

fn parse_managers(input: &str) -> Result<BTreeSet<Principal>, BoxError> {
    let mut managers = BTreeSet::new();
    // Tolerate whitespace around entries and stray commas ("a, b," would
    // otherwise fail on " b" and ""); a malformed id still fails startup.
    for id in input.split(',').map(str::trim).filter(|id| !id.is_empty()) {
        managers.insert(Principal::from_text(id)?);
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

/// Wraps `router` in a shared in-flight request cap: at most `max_in_flight`
/// requests run at once and excess requests are shed immediately with
/// `shed_status` instead of queueing without bound.
///
/// `GlobalConcurrencyLimitLayer` shares one semaphore across every route of
/// the router (axum applies a layer to each route separately, so the plain
/// `ConcurrencyLimitLayer` would be a per-route limit). `LoadShedLayer`
/// turns "no permit available" into an error and `HandleErrorLayer` maps
/// that error to an HTTP response, which axum requires: its services must
/// be infallible.
fn with_concurrency_limit<S>(
    router: Router<S>,
    max_in_flight: usize,
    shed_status: StatusCode,
) -> Router<S>
where
    S: Clone + Send + Sync + 'static,
{
    // `max(1)` keeps a misconfigured `0` from shedding every request.
    with_shared_concurrency_limit(
        router,
        Arc::new(tokio::sync::Semaphore::new(max_in_flight.max(1))),
        shed_status,
    )
}

/// [`with_concurrency_limit`] over a caller-owned semaphore, so the same
/// budget can be drained by non-router work (the MCP LLM tools share the
/// LLM routes' semaphore).
fn with_shared_concurrency_limit<S>(
    router: Router<S>,
    semaphore: Arc<tokio::sync::Semaphore>,
    shed_status: StatusCode,
) -> Router<S>
where
    S: Clone + Send + Sync + 'static,
{
    router.layer(
        ServiceBuilder::new()
            .layer(HandleErrorLayer::new(
                move |err: tower::BoxError| async move {
                    if err.is::<tower::load_shed::error::Overloaded>() {
                        (
                            shed_status,
                            "too many concurrent requests, retry later".to_string(),
                        )
                    } else {
                        (
                            StatusCode::INTERNAL_SERVER_ERROR,
                            format!("unhandled middleware error: {err}"),
                        )
                    }
                },
            ))
            .layer(LoadShedLayer::new())
            .layer(GlobalConcurrencyLimitLayer::with_semaphore(semaphore)),
    )
}

// grcov-excl-start: route registration is verified through direct handler tests; axum's builder chain gives low-value line coverage.
fn build_router(
    app_state: AppState,
    cli: &Cli,
    cancel_token: CancellationToken,
) -> Router<AppState> {
    // Endpoints that can each drive a full multi-turn LLM round. They share
    // a stricter concurrency cap so anonymous callers on public spaces
    // cannot turn unbounded request concurrency into unbounded model spend.
    // Default `LLM_MAX_CONCURRENCY=64` is loose: it never throttles normal
    // multi-tenant traffic, it only bounds floods. The semaphore lives in
    // the `AppState` because the MCP LLM tools (recall/maintenance) must
    // drain the same budget instead of bypassing this cap up to the global
    // HTTP limit.
    let llm_router = with_shared_concurrency_limit(
        Router::new()
            .route("/v1/{space_id}/formation", routing::post(post_formation))
            .route("/v1/{space_id}/recall", routing::post(post_recall))
            .route(
                "/v1/{space_id}/recall_structured",
                routing::post(post_recall_structured),
            )
            .route(
                "/v1/{space_id}/maintenance",
                routing::post(post_maintenance),
            )
            .route(
                "/v1/{space_id}/management/shadow_eval",
                routing::post(post_shadow_eval),
            )
            .route(
                "/v1/{space_id}/wiki/digest",
                routing::post(post_wiki_digest),
            ),
        app_state.llm_semaphore().clone(),
        StatusCode::TOO_MANY_REQUESTS,
    );

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
        .route("/v1/{space_id}/probe", routing::post(post_probe))
        .route("/v1/{space_id}/memory/pin", routing::post(post_memory_pin))
        .route(
            "/v1/{space_id}/memory/forget",
            routing::post(post_memory_forget),
        )
        .route(
            "/v1/{space_id}/memory_status",
            routing::get(get_memory_status),
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
        .merge(llm_router)
        // Error bodies are always JSON; only success bodies follow the
        // Accept header (content negotiation happens in `AppResponse`).
        .layer(CompressionLayer::new());

    if cli.mcp_http_enabled {
        let mcp_config = mcp_http_config_from_cli(cli);
        let path_prefix = mcp_config.path_prefix.clone();
        let mcp_service =
            build_streamable_http_service(app_state, mcp_config, cancel_token.child_token());
        router = router.nest_service(&path_prefix, mcp_service);
    }

    // Global backstop over every route, the nested MCP endpoint included
    // (MCP tool calls are only covered by this outer cap): beyond
    // `HTTP_MAX_CONCURRENCY` in-flight requests the service sheds with 503
    // instead of accumulating unbounded queued work.
    with_concurrency_limit(
        router,
        cli.http_max_concurrency,
        StatusCode::SERVICE_UNAVAILABLE,
    )
}
// grcov-excl-stop

fn build_cors(cors_origins: &str) -> Result<CorsLayer, BoxError> {
    if cors_origins.trim().is_empty() {
        Ok(CorsLayer::new())
    } else if cors_origins.trim() == "*" {
        Ok(CorsLayer::very_permissive())
    } else {
        // A silently dropped origin would ship a service that "has CORS
        // configured" but rejects the intended frontend; fail startup loudly
        // instead.
        let mut origins: Vec<http::HeaderValue> = Vec::new();
        for origin in cors_origins
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            origins.push(
                origin
                    .parse()
                    .map_err(|err| format!("invalid CORS origin {origin:?}: {err}"))?,
            );
        }
        if origins.is_empty() {
            return Err(format!("CORS_ORIGINS contains no valid origin: {cors_origins:?}").into());
        }
        Ok(CorsLayer::new()
            .allow_origin(origins)
            .allow_credentials(true)
            .max_age(Duration::from_secs(86400))
            .allow_headers(AllowHeaders::mirror_request())
            .allow_methods(AllowMethods::mirror_request()))
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
    )
    .with_judge_model(judge_model_from_env())
    // One LLM budget for the whole process: the HTTP LLM routes and the MCP
    // LLM tools (HTTP and stdio alike) drain this same semaphore.
    .with_llm_concurrency(cli.llm_max_concurrency);

    Ok((app_state, db_type))
}

/// Independent judge model for the service path (plan M9), from the same
/// `JUDGE_MODEL_*` variables the eval subcommand exposes as flags. Without
/// this, shadow-eval verdicts in service mode always fall back to the
/// evaluated space's own model — a self-grading blind spot.
fn judge_model_from_env() -> Option<BrainModelConfig> {
    let api_key = std::env::var("JUDGE_MODEL_API_KEY").unwrap_or_default();
    if api_key.is_empty() {
        return None;
    }
    Some(BrainModelConfig {
        family: std::env::var("JUDGE_MODEL_FAMILY").unwrap_or_else(|_| "openai".to_string()),
        model: std::env::var("JUDGE_MODEL_NAME").unwrap_or_default(),
        api_base: std::env::var("JUDGE_MODEL_API_BASE").unwrap_or_default(),
        api_key,
        ..Default::default()
    })
}

fn build_service_runtime(
    cli: &Cli,
    cancel_token: CancellationToken,
) -> Result<ServiceRuntime, BoxError> {
    let (app_state, db_type) = build_app_state(cli)?;
    let app = build_router(app_state.clone(), cli, cancel_token)
        .layer(build_cors(&cli.cors_origins)?)
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
    if db_type == "memory" {
        log::warn!(
            target: "brain",
            "WARNING: in-memory storage is active (no `local` or `aws` storage subcommand); ALL long-term memory will be LOST when the process exits. Configure persistent storage for production."
        );
    }

    join_service_tasks(server_handle, spaces_handle, global_cancel_token).await
}

/// Awaits both service tasks. Neither should finish on its own: the server
/// runs until the graceful-shutdown signal and the background tasks run
/// until the global cancel token fires. If one exits early (server accept
/// loop error, or a panic in either task), cancel the token so the peer
/// shuts down too, and propagate the failure so the process exits non-zero.
/// Without this the process would keep running with no listener — a zombie
/// an orchestrator never restarts.
async fn join_service_tasks(
    mut server_handle: JoinHandle<std::io::Result<()>>,
    mut spaces_handle: JoinHandle<()>,
    cancel_token: CancellationToken,
) -> Result<(), BoxError> {
    let (server_res, spaces_res) = tokio::select! {
        res = &mut server_handle => {
            if !cancel_token.is_cancelled() {
                log::error!(target: "brain", "server task exited before shutdown was requested; stopping background tasks");
            }
            cancel_token.cancel();
            (res, spaces_handle.await)
        }
        res = &mut spaces_handle => {
            if !cancel_token.is_cancelled() {
                log::error!(target: "brain", "space background task exited before shutdown was requested; stopping server");
            }
            cancel_token.cancel();
            (server_handle.await, res)
        }
    };

    server_res
        .map_err(|err| format!("server task failed: {err}"))?
        .map_err(|err| format!("server exited with error: {err}"))?;
    spaces_res.map_err(|err| format!("space background task failed: {err}"))?;
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
    auto_create_tier: u32,
    shared_formation: bool,
    checkpoint_samples: Option<usize>,
    /// Z multiplier for the --optimize accept noise band.
    confidence_z: Option<f64>,
    optimize: Option<String>,
    generations: usize,
    optimize_out: String,
    holdout_paths: Vec<String>,
    judge_model: Option<BrainModelConfig>,
    mine: bool,
    mine_out: String,
    since_days: u32,
    max_scenarios: usize,
    keep_spaces: bool,
}

/// Shared plumbing every eval run needs; cheap to clone (AppState is
/// internally shared).
#[derive(Clone)]
struct EvalRunEnv {
    app_state: AppState,
    auto_create_tier: u32,
    run_id: u64,
    keep_spaces: bool,
    /// Independent judge model (plan M9), installed on every run-scoped
    /// space (including shared-formation forks).
    judge_model: Option<BrainModelConfig>,
}

impl EvalRunEnv {
    /// Host for one run-scoped eval space. Default: a sibling `AppState`
    /// over a fresh in-memory store, so the space is fully isolated (no
    /// leftover memories can leak into scores) and cleanup is simply
    /// dropping the fork. `--keep-spaces`: the real store, with the run id
    /// appended so the kept space cannot collide with earlier runs.
    fn space_host(&self, parts: &[&str]) -> (AppState, String) {
        if self.keep_spaces {
            let run_id = self.run_id.to_string();
            let mut parts = parts.to_vec();
            parts.push(&run_id);
            (self.app_state.clone(), compose_space_id(&parts))
        } else {
            let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
            (
                self.app_state.fork_with_store(store),
                compose_space_id(parts),
            )
        }
    }
}

enum EvalCommandReport {
    Scenario(EvalReport),
    Suite(EvalSuiteReport),
    Experiment(EvalExperimentReport),
}

impl EvalCommandReport {
    fn score_parts(&self) -> (&EvalScore, &anda_brain::eval::AttributionSummary) {
        match self {
            Self::Scenario(report) => (&report.score, &report.attribution),
            Self::Suite(report) => (&report.score, &report.attribution),
            Self::Experiment(report) => (&report.score, &report.attribution),
        }
    }

    fn evaluate_gate(&self, gate: &EvalGate) -> EvalGateReport {
        let (score, attribution) = self.score_parts();
        gate.evaluate(score, attribution)
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
        auto_create_tier,
        shared_formation,
        checkpoint_samples,
        confidence_z,
        optimize,
        generations,
        optimize_out,
        holdout_paths,
        judge_model,
        mine,
        mine_out,
        since_days,
        max_scenarios,
        keep_spaces,
    } = config;

    // Mining is its own mode: it reads an existing space, runs no scenarios.
    if mine {
        if !scenario_paths.is_empty()
            || !holdout_paths.is_empty()
            || optimize.is_some()
            || shared_formation
            || gate.is_configured()
        {
            return Err(
                "--mine is exclusive: drop --scenario/--holdout-scenario/--optimize/--shared-formation/--min-score/--max-findings"
                    .into(),
            );
        }
        return run_mine_command(
            cli,
            &space_id,
            &mine_out,
            since_days,
            max_scenarios,
            output_path,
            summary_only,
        )
        .await;
    }

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
    let optimize_mode = optimize.as_deref().map(parse_optimize_mode).transpose()?;

    // Holdout scenarios (anti-overfitting gate for --optimize) validate
    // under the same profiles.
    let holdout_scenarios: Vec<EvalScenario> = holdout_paths
        .iter()
        .map(|path| read_json_file::<EvalScenario>(path))
        .collect::<Result<_, _>>()?;
    if !holdout_scenarios.is_empty() {
        if optimize_mode.is_none() {
            return Err("--holdout-scenario requires --optimize".into());
        }
        let holdout_validation = validate_eval_plan(&holdout_scenarios, &profile_values);
        if holdout_validation.has_errors() {
            return Err(eval_validation_error(&holdout_validation).into());
        }
    }

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
    let env = EvalRunEnv {
        app_state,
        auto_create_tier,
        run_id: anda_engine::unix_ms(),
        keep_spaces,
        judge_model,
    };

    if let Some((genome, target)) = optimize_mode {
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
            &env,
            &space_id,
            &profiles[0],
            &scenarios,
            &holdout_scenarios,
            genome,
            target,
            generations,
            confidence_z,
            &optimize_out,
            output_path,
            summary_only,
        )
        .await;
    }

    let mut report = if shared_formation {
        let experiment =
            run_shared_formation_experiment(&env, &space_id, &profiles, &scenarios).await?;
        EvalCommandReport::Experiment(experiment)
    } else if profiles.len() == 1 {
        let mut suite = run_eval_suite(&env, &space_id, &profiles[0], &scenarios).await?;
        if scenarios.len() == 1 {
            // One scenario, one profile: report the bare scenario shape.
            EvalCommandReport::Scenario(suite.reports.remove(0))
        } else {
            EvalCommandReport::Suite(suite)
        }
    } else {
        let mut suites = Vec::with_capacity(profiles.len());
        // Profiles run sequentially on purpose: each suite already keeps
        // EVAL_SCENARIO_CONCURRENCY scenarios in flight, and stacking
        // profile-level concurrency on top mostly buys provider rate-limit
        // errors that would surface as zero-score fallback reports.
        for profile in &profiles {
            let suite = run_eval_suite(&env, &space_id, profile, &scenarios).await?;
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

/// Concurrent-scenario budget for one suite run. Scenarios are fully
/// isolated (each in its own run-scoped space), so this only bounds how many
/// model conversations are in flight at once — enough to hide LLM latency
/// (the optimize loop re-runs the whole suite every generation) without
/// driving provider rate limits into the zero-score fallback reports.
const EVAL_SCENARIO_CONCURRENCY: usize = 4;

async fn run_eval_suite(
    env: &EvalRunEnv,
    base_space_id: &str,
    profile: &NamedEvalProfile,
    scenarios: &[EvalScenario],
) -> Result<EvalSuiteReport, BoxError> {
    use futures::StreamExt;

    // Each scenario runs in its own run-scoped space (see `space_host`), so
    // leftover memories from a previous run can never leak into scores and
    // scenarios can run concurrently; the default in-memory host vanishes
    // when `state` drops. `buffered` keeps the reports in scenario order.
    // The futures are collected eagerly (futures are inert until polled): a
    // lazy `map` closure inside the stream would poison this function's
    // future with rustc's "implementation of FnOnce is not general enough"
    // when it is later boxed 'static (the optimize loop does exactly that).
    let scenario_futures: Vec<_> = scenarios
        .iter()
        .map(|scenario| run_suite_scenario(env, base_space_id, profile, scenario))
        .collect();
    let reports: Vec<EvalReport> = futures::stream::iter(scenario_futures)
        .buffered(EVAL_SCENARIO_CONCURRENCY)
        .collect()
        .await;

    Ok(EvalSuiteReport::from_reports(profile.id.clone(), reports))
}

async fn run_suite_scenario(
    env: &EvalRunEnv,
    base_space_id: &str,
    profile: &NamedEvalProfile,
    scenario: &EvalScenario,
) -> EvalReport {
    let (state, scenario_space_id) = env.space_host(&[base_space_id, &profile.id, &scenario.id]);
    let result = match load_eval_space(env, &state, &scenario_space_id).await {
        Ok(space) => {
            // Close even when the scenario fails so `--keep-spaces` leaves a
            // flushed, inspectable space. Close failures only warn: on the
            // default path the store is dropped right after, so there is
            // nothing durable to lose.
            let result = run_scenario(space.as_ref(), scenario, &profile.profile).await;
            if let Err(err) = space.db.close().await {
                eprintln!("warning: failed to close eval space {scenario_space_id}: {err}");
            }
            result
        }
        Err(err) => Err(err),
    };
    // One scenario's failure must not discard the other (paid) scenario
    // reports: record it as a zero-score report with a finding so the suite
    // mean is not silently inflated either, and keep going.
    match result {
        Ok(report) => report,
        Err(err) => {
            eprintln!("warning: eval scenario `{}` aborted: {err}", scenario.id);
            failed_scenario_report(scenario, &profile.id, err.as_ref())
        }
    }
}

/// Zero-score stand-in for a scenario that aborted before producing a
/// report. Dropping the scenario instead would inflate the suite mean —
/// poisonous when the suite is an optimizer fitness function or gated.
fn failed_scenario_report(
    scenario: &EvalScenario,
    profile_id: &str,
    err: &(dyn std::error::Error + Send + Sync),
) -> EvalReport {
    let mut report = EvalReport {
        scenario_id: scenario.id.clone(),
        description: scenario.description.clone(),
        profile_id: Some(profile_id.to_string()),
        started_at: Some(anda_engine::rfc3339_datetime_now()),
        ..Default::default()
    };
    report.turns.push(EvalTurnReport {
        findings: vec![EvalFinding {
            kind: EvalFindingKind::JudgeError,
            expectation_id: None,
            message: format!("scenario aborted before completion: {err}"),
        }],
        ..Default::default()
    });
    report.attribution.judge_error = 1;
    report
}

fn parse_optimize_mode(target: &str) -> Result<(GenomeKind, Option<PromptTarget>), BoxError> {
    match target.trim().to_lowercase().as_str() {
        "auto" => Ok((GenomeKind::Prompt, None)),
        "formation" => Ok((GenomeKind::Prompt, Some(PromptTarget::Formation))),
        "recall" => Ok((GenomeKind::Prompt, Some(PromptTarget::Recall))),
        "maintenance" => Ok((GenomeKind::Prompt, Some(PromptTarget::Maintenance))),
        "policy" => Ok((GenomeKind::Policy, None)),
        other => Err(format!(
            "invalid --optimize target `{other}`; expected formation|recall|maintenance|auto|policy"
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
async fn run_shared_formation_experiment(
    env: &EvalRunEnv,
    base_space_id: &str,
    profiles: &[NamedEvalProfile],
    scenarios: &[EvalScenario],
) -> Result<EvalExperimentReport, BoxError> {
    let mut shared_reports = Vec::with_capacity(scenarios.len());
    let mut profile_reports: Vec<Vec<EvalReport>> = vec![Vec::new(); profiles.len()];

    for scenario in scenarios {
        let (base_state, base_id) = env.space_host(&[base_space_id, "form", &scenario.id]);
        // Scenario-level failure isolation, matching `run_eval_suite`: one
        // scenario's abort must not discard the other scenarios' already-paid
        // formation and policy results.
        let formation_result = match load_eval_space(env, &base_state, &base_id).await {
            Ok(space) => {
                // The formation phase only reads timeouts from the profile.
                // The base snapshot must be flushed and closed before the
                // profiles fork it; a close failure poisons every fork, so
                // it fails the scenario rather than the experiment.
                let formation_result =
                    run_formation_phase(space.as_ref(), scenario, &profiles[0].profile).await;
                let close_result = space.db.close().await;
                match (formation_result, close_result) {
                    (Ok(report), Ok(())) => Ok(report),
                    (Err(err), _) => Err(err),
                    (_, Err(err)) => Err(err.into()),
                }
            }
            Err(err) => Err(err),
        };
        match formation_result {
            Ok(report) => shared_reports.push(report),
            Err(err) => {
                eprintln!(
                    "warning: shared formation for scenario `{}` aborted: {err}",
                    scenario.id
                );
                // No profile can fork a snapshot that never materialized:
                // every suite gets the zero-score stand-in for this scenario.
                shared_reports.push(failed_scenario_report(
                    scenario,
                    &profiles[0].id,
                    err.as_ref(),
                ));
                for (index, profile) in profiles.iter().enumerate() {
                    profile_reports[index].push(failed_scenario_report(
                        scenario,
                        &profile.id,
                        err.as_ref(),
                    ));
                }
                continue;
            }
        }

        // Forks are fully isolated — each lives in its own in-memory store —
        // so every profile's policy phase can replay concurrently. Failures
        // stay per-profile: `join_all` (not `try_join_all`) so one fork's
        // abort cannot discard its siblings' finished reports.
        let base_store = base_state.object_store();
        let fork_results = futures::future::join_all(profiles.iter().map(|profile| {
            let base_id = base_id.clone();
            let base_store = base_store.clone();
            async move {
                let fork_store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
                copy_space_objects(&base_store, &fork_store, &base_id).await?;
                let fork_state = env.app_state.fork_with_store(fork_store);
                let fork_space = fork_state.load_space(&base_id, true).await?;
                if let Some(judge) = &env.judge_model {
                    fork_space.set_judge_model(judge.clone())?;
                }
                let result =
                    run_policy_phase(fork_space.as_ref(), scenario, &profile.profile).await;
                let close_result = fork_space.db.close().await;
                let report = result?;
                close_result?;
                Ok::<EvalReport, BoxError>(report)
            }
        }))
        .await;
        // The base snapshot is only needed until every profile has forked
        // it: `base_state` (and, unless --keep-spaces, its store) drops at
        // the end of this iteration.
        for ((index, profile), result) in profiles.iter().enumerate().zip(fork_results) {
            profile_reports[index].push(match result {
                Ok(report) => report,
                Err(err) => {
                    eprintln!(
                        "warning: policy phase for scenario `{}` profile `{}` aborted: {err}",
                        scenario.id, profile.id
                    );
                    failed_scenario_report(scenario, &profile.id, err.as_ref())
                }
            });
        }
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
    env: &EvalRunEnv,
    base_space_id: &str,
    profile: &NamedEvalProfile,
    scenarios: &[EvalScenario],
    holdout_scenarios: &[EvalScenario],
    genome: GenomeKind,
    target: Option<PromptTarget>,
    generations: usize,
    confidence_z: Option<f64>,
    out_dir: &str,
    output_path: Option<String>,
    summary_only: bool,
) -> Result<(), BoxError> {
    // Scratch space whose model powers the optimizer's proposal calls.
    let (proposer_state, proposer_id) = env.space_host(&[base_space_id, "optimizer"]);
    let proposer = load_eval_space(env, &proposer_state, &proposer_id).await?;

    let mut config = OptimizeConfig {
        generations,
        genome,
        target,
        ..Default::default()
    };
    if let Some(z) = confidence_z {
        config.confidence_z = z;
    }
    let fitness_env = env.clone();
    let fitness_profile = profile.clone();
    let fitness_scenarios = scenarios.to_vec();
    let fitness_base = base_space_id.to_string();
    // Anti-overfitting gate (plan M9): train wins must also hold on the
    // held-out suite, which runs in its own run-scoped spaces.
    let holdout: Option<BoxedFitness> = if holdout_scenarios.is_empty() {
        None
    } else {
        let env = env.clone();
        let profile = profile.clone();
        let scenarios = holdout_scenarios.to_vec();
        let base = base_space_id.to_string();
        Some(Box::new(move |generation: usize| {
            let env = env.clone();
            let profile = profile.clone();
            let scenarios = scenarios.clone();
            let base = format!("{base}_h{generation}");
            Box::pin(async move { run_eval_suite(&env, &base, &profile, &scenarios).await })
                as futures::future::BoxFuture<'static, Result<EvalSuiteReport, BoxError>>
        }))
    };
    let outcome = run_optimize(
        proposer.as_ref(),
        &config,
        move |generation| {
            let env = fitness_env.clone();
            let profile = fitness_profile.clone();
            let scenarios = fitness_scenarios.clone();
            let base = format!("{fitness_base}_g{generation}");
            async move { run_eval_suite(&env, &base, &profile, &scenarios).await }
        },
        holdout,
    )
    .await;
    // Leave the process with pristine prompts and policy regardless of the
    // outcome. (`proposer_state` and its throwaway store drop on return.)
    let close_result = proposer.db.close().await;
    prompts::clear_overrides();
    MemoryPolicy::set_eval_override(None);
    let report = outcome?;
    close_result?;

    write_optimize_artifacts(out_dir, &report)?;
    let report_output = if summary_only {
        optimize_summary(&report, out_dir)
    } else {
        serde_json::to_string_pretty(&report)?
    };
    match output_path {
        Some(path) => std::fs::write(path, report_output)?,
        None => println!("{report_output}"),
    }
    Ok(())
}

/// Writes the optimize report and accepted genomes for human review.
fn write_optimize_artifacts(out_dir: &str, report: &OptimizeReport) -> Result<(), BoxError> {
    std::fs::create_dir_all(out_dir)?;
    for accepted in &report.accepted_prompts {
        let filename = match accepted.target {
            PromptTarget::Formation => "BrainFormation.md",
            PromptTarget::Recall => "BrainRecall.md",
            PromptTarget::Maintenance => "BrainMaintenance.md",
        };
        std::fs::write(Path::new(out_dir).join(filename), &accepted.text)?;
    }
    if let Some(policy) = &report.accepted_policy {
        std::fs::write(
            Path::new(out_dir).join("memory_policy.json"),
            serde_json::to_string_pretty(policy)?,
        )?;
    }
    std::fs::write(
        Path::new(out_dir).join("optimize_report.json"),
        serde_json::to_string_pretty(report)?,
    )?;
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
        let holdout = generation
            .holdout_total
            .map(|total| format!(" holdout={total:.4}"))
            .unwrap_or_default();
        writeln!(
            out,
            "- gen {} target={} candidate={}{holdout} {} ({})",
            generation.generation,
            generation
                .target
                .map(|target| target.as_str())
                .unwrap_or("policy"),
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
    if report.accepted_policy.is_some() {
        writeln!(
            out,
            "accepted policy written to {out_dir}/memory_policy.json"
        )
        .ok();
    }
    out
}

/// Scenario mining (plan M9): distills a real space's correction ledger into
/// eval scenarios for human review. Read-only over the space; nothing is
/// added to any suite automatically.
async fn run_mine_command(
    cli: &Cli,
    space_id: &str,
    mine_out: &str,
    since_days: u32,
    max_scenarios: usize,
    output_path: Option<String>,
    summary_only: bool,
) -> Result<(), BoxError> {
    let (app_state, _) = build_app_state(cli)?;
    let space = app_state
        .load_space(space_id, true)
        .await
        .map_err(|err| format!("--mine requires an existing space `{space_id}`: {err}"))?;
    let now_ms = anda_engine::unix_ms();
    let config = MineConfig {
        since_ms: now_ms.saturating_sub(u64::from(since_days) * 86_400_000),
        max_scenarios,
    };
    let result = mine_scenarios(space.as_ref(), &config).await;
    space.db.close().await?;
    let (mined, usage) = result?;

    std::fs::create_dir_all(mine_out)?;
    let mut entries = Vec::with_capacity(mined.len());
    for (index, item) in mined.iter().enumerate() {
        // The LLM readily produces the same slug for the same class of
        // correction (this run or a previous one); the run timestamp plus
        // the in-run index keep every file awaiting human review unique.
        let stem = sanitize_space_id_part(&item.scenario.id);
        let path = Path::new(mine_out).join(format!("{stem}_{now_ms}_{index}.json"));
        std::fs::write(&path, serde_json::to_string_pretty(&item.scenario)?)?;
        entries.push(serde_json::json!({
            "id": item.scenario.id,
            "signal": item.signal,
            "path": path.display().to_string(),
        }));
    }

    let report_output = if summary_only {
        let mut out = String::new();
        writeln!(out, "Mined {} scenario(s) into {mine_out}", mined.len()).ok();
        for entry in &entries {
            writeln!(out, "- {} (from {})", entry["id"], entry["signal"]).ok();
        }
        writeln!(
            out,
            "usage: input_tokens={} output_tokens={}",
            usage.input_tokens, usage.output_tokens
        )
        .ok();
        writeln!(out, "review them before adding to train/holdout suites").ok();
        out
    } else {
        serde_json::to_string_pretty(&serde_json::json!({
            "mined": mined.len(),
            "out_dir": mine_out,
            "scenarios": entries,
            "usage": usage,
        }))?
    };
    match output_path {
        Some(path) => std::fs::write(path, report_output)?,
        None => println!("{report_output}"),
    }
    Ok(())
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

/// Creates and loads a run-scoped eval space inside `state` (a throwaway
/// in-memory fork by default, the real store under `--keep-spaces`; see
/// [`EvalRunEnv::space_host`]). The space id is unique per host, so creation
/// must succeed. The env's independent judge model, when configured, is
/// installed on every space this loads.
async fn load_eval_space(
    env: &EvalRunEnv,
    state: &AppState,
    space_id: &str,
) -> Result<Arc<anda_brain::space::Space>, BoxError> {
    state
        .admin_create_space(
            SELF_USER_ID,
            SELF_USER_ID,
            space_id.to_string(),
            env.auto_create_tier,
            anda_engine::unix_ms(),
        )
        .await?;

    let space = state.load_space(space_id, true).await?;
    if let Some(judge) = &env.judge_model {
        space.set_judge_model(judge.clone())?;
    }
    Ok(space)
}

fn profile_id_from_path(path: &str) -> String {
    Path::new(path)
        .file_stem()
        .and_then(|value| value.to_str())
        .map(sanitize_space_id_part)
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "profile".to_string())
}

/// AndaDB space names must match `[a-z0-9_]` (max 64 chars); anything else
/// fails at space creation. Lowercase and map every other character to `_`.
fn sanitize_space_id_part(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for ch in value.chars() {
        let ch = ch.to_ascii_lowercase();
        if ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '_' {
            out.push(ch);
        } else {
            out.push('_');
        }
    }

    let out = out.trim_matches('_');
    if out.is_empty() {
        "part".to_string()
    } else {
        out.to_string()
    }
}

/// AndaDB rejects database names longer than 64 characters.
const MAX_SPACE_ID_LEN: usize = 64;

/// Joins sanitized parts into a space id and caps it at AndaDB's name limit.
/// Over-long ids keep a readable prefix plus a hash of the full id, so two
/// distinct long ids can never collide after truncation.
fn compose_space_id(parts: &[&str]) -> String {
    let id = parts
        .iter()
        .map(|part| sanitize_space_id_part(part))
        .collect::<Vec<_>>()
        .join("_");
    if id.len() <= MAX_SPACE_ID_LEN {
        return id;
    }
    let suffix = format!("_{:016x}", fnv1a(id.as_bytes()));
    // The sanitized id is pure ASCII, so byte slicing cannot split a char.
    format!("{}{suffix}", &id[..MAX_SPACE_ID_LEN - suffix.len()])
}

/// FNV-1a: a tiny stable hash for id truncation; not security-sensitive.
fn fnv1a(bytes: &[u8]) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325u64;
    for &byte in bytes {
        hash ^= byte as u64;
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

fn read_json_file<T>(path: &str) -> Result<T, BoxError>
where
    T: serde::de::DeserializeOwned,
{
    let data = std::fs::read(path).map_err(|err| format!("failed to read {path}: {err}"))?;
    serde_json::from_slice(&data).map_err(|err| format!("failed to parse {path}: {err}").into())
}

/// ```bash
/// cargo run -p anda_brain
/// ```
// grcov-excl-start: main is a thin CLI/logging wrapper; build_service_runtime and run_service are unit-tested.
#[tokio::main]
async fn main() -> Result<(), BoxError> {
    dotenv::dotenv().ok();
    let cli = Cli::parse();

    match &cli.command {
        // MCP stdio keeps stdout reserved for JSON-RPC messages and eval
        // reserves stdout for reports; logs (auth failures, space setup,
        // judge fallbacks) go to stderr instead of being silently dropped.
        Some(Commands::Mcp { .. }) | Some(Commands::Eval { .. }) => {
            Builder::with_level(&get_env_level().to_string())
                .with_target_writer("*", new_writer(tokio::io::stderr()))
                .init();
        }
        _ => {
            // Structured JSON logging on stdout for the HTTP service.
            Builder::with_level(&get_env_level().to_string())
                .with_target_writer("*", new_writer(tokio::io::stdout()))
                .init();
        }
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
            auto_create_tier,
            shared_formation,
            checkpoint_samples,
            confidence_z,
            optimize,
            generations,
            optimize_out,
            holdout_scenario,
            judge_model_family,
            judge_model_name,
            judge_model_api_key,
            judge_model_api_base,
            mine,
            mine_out,
            since_days,
            max_scenarios,
            keep_spaces,
            ..
        }) => {
            // An empty API key means no independent judge: judge completions
            // share the evaluated system's model (documented caveat).
            let judge_model = (!judge_model_api_key.is_empty()).then(|| BrainModelConfig {
                family: judge_model_family,
                model: judge_model_name,
                api_key: judge_model_api_key,
                api_base: judge_model_api_base,
                ..Default::default()
            });
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
                    },
                    validate_only,
                    summary_only,
                    auto_create_tier,
                    shared_formation,
                    checkpoint_samples,
                    confidence_z,
                    optimize,
                    generations,
                    optimize_out,
                    holdout_paths: holdout_scenario,
                    judge_model,
                    mine,
                    mine_out,
                    since_days,
                    max_scenarios,
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
        AnyHost, Cli, Commands, EvalCommandConfig, EvalCommandReport, MAX_SPACE_ID_LEN,
        StorageCommand, build_cors, build_http_client, build_router, build_service_runtime,
        compose_space_id, create_reuse_port_listener, default_db_config, join_service_tasks,
        mcp_http_config_from_cli, model_config_from_cli, normalize_http_path_prefix,
        object_store_from_command, parse_ed25519_pubkeys, parse_managers, read_json_file,
        run_eval_command, run_service, sanitize_space_id_part, split_csv_values,
        with_concurrency_limit,
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
            http_max_concurrency: 1024,
            llm_max_concurrency: 64,
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
        let _ = build_cors("").unwrap();
        let _ = build_cors("  ").unwrap();
        let _ = build_cors("*").unwrap();
        let _ = build_cors("https://example.test, https://app.example.test,").unwrap();
        // An origin that fails HeaderValue parsing (embedded DEL byte) must
        // abort startup instead of being silently dropped.
        assert!(build_cors("https://example.test,bad\u{7f}origin").is_err());
        // Only separators and whitespace is a configuration mistake too.
        assert!(build_cors(",").is_err());

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

        // Whitespace around ids and stray commas are tolerated.
        let managers = parse_managers(&format!(" {SELF_USER_ID} , ,{SELF_USER_ID},")).unwrap();
        assert_eq!(managers.len(), 1);
        assert!(managers.contains(&SELF_USER_ID));
        assert!(parse_managers(" , ").unwrap().is_empty());

        assert!(parse_managers("not a principal").is_err());
        assert!(parse_managers(&format!("{SELF_USER_ID},not a principal")).is_err());
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
            auto_create_tier: 1,
            shared_formation: false,
            checkpoint_samples: None,
            confidence_z: None,
            optimize: None,
            generations: 3,
            optimize_out: "./eval_optimize".to_string(),
            holdout_scenario: Vec::new(),
            judge_model_family: "openai".to_string(),
            judge_model_name: String::new(),
            judge_model_api_key: String::new(),
            judge_model_api_base: String::new(),
            mine: false,
            mine_out: "./anda_brain/evals/mined".to_string(),
            since_days: 30,
            max_scenarios: 8,
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
                auto_create_tier: 1,
                holdout_paths: Vec::new(),
                judge_model: None,
                mine: false,
                mine_out: "./anda_brain/evals/mined".to_string(),
                since_days: 30,
                max_scenarios: 8,
                shared_formation: false,
                checkpoint_samples: None,
                confidence_z: None,
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
                auto_create_tier: 1,
                holdout_paths: Vec::new(),
                judge_model: None,
                mine: false,
                mine_out: "./anda_brain/evals/mined".to_string(),
                since_days: 30,
                max_scenarios: 8,
                shared_formation: false,
                checkpoint_samples: None,
                confidence_z: None,
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
    fn sanitize_space_id_part_matches_anda_db_charset() {
        // AndaDB names only allow [a-z0-9_]: lowercase and fold the rest.
        assert_eq!(
            sanitize_space_id_part("Style-Preference"),
            "style_preference"
        );
        assert_eq!(sanitize_space_id_part("__mixed 42__"), "mixed_42");
        assert_eq!(sanitize_space_id_part("汉字"), "part");
    }

    #[test]
    fn compose_space_id_caps_length_without_collisions() {
        let short = compose_space_id(&["eval", "Default-Profile", "scenario", "123"]);
        assert_eq!(short, "eval_default_profile_scenario_123");

        let long_a = compose_space_id(&["eval", &"a".repeat(80), "scenario", "123"]);
        let long_b = compose_space_id(&["eval", &"a".repeat(81), "scenario", "123"]);
        assert_eq!(long_a.len(), MAX_SPACE_ID_LEN);
        assert_eq!(long_b.len(), MAX_SPACE_ID_LEN);
        assert_ne!(long_a, long_b, "hash suffix must keep long ids distinct");
        assert!(
            long_a
                .chars()
                .all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '_')
        );
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

    #[tokio::test]
    async fn join_service_tasks_cancels_peer_and_propagates_server_error() {
        let cancel = CancellationToken::new();
        let server: tokio::task::JoinHandle<std::io::Result<()>> =
            tokio::spawn(async { Err(std::io::Error::other("accept loop died")) });
        // Models `start_background_tasks`: runs until the token fires.
        let peer_token = cancel.clone();
        let spaces = tokio::spawn(async move { peer_token.cancelled().await });

        let err = timeout(
            Duration::from_secs(2),
            join_service_tasks(server, spaces, cancel.clone()),
        )
        .await
        .expect("must not hang once the server task is gone")
        .unwrap_err();

        assert!(cancel.is_cancelled(), "peer task must be cancelled");
        assert!(err.to_string().contains("accept loop died"));
    }

    #[tokio::test]
    async fn join_service_tasks_cancels_server_when_background_task_dies() {
        let cancel = CancellationToken::new();
        let server_token = cancel.clone();
        let server: tokio::task::JoinHandle<std::io::Result<()>> = tokio::spawn(async move {
            server_token.cancelled().await;
            Ok(())
        });
        let spaces = tokio::spawn(async { panic!("background task crashed") });

        let err = timeout(
            Duration::from_secs(2),
            join_service_tasks(server, spaces, cancel.clone()),
        )
        .await
        .expect("must not hang once the background task is gone")
        .unwrap_err();

        assert!(cancel.is_cancelled(), "server must be told to shut down");
        assert!(err.to_string().contains("space background task failed"));
    }

    #[tokio::test]
    async fn join_service_tasks_is_clean_on_graceful_shutdown() {
        let cancel = CancellationToken::new();
        let server_token = cancel.clone();
        let server: tokio::task::JoinHandle<std::io::Result<()>> = tokio::spawn(async move {
            server_token.cancelled().await;
            Ok(())
        });
        let spaces_token = cancel.clone();
        let spaces = tokio::spawn(async move { spaces_token.cancelled().await });
        cancel.cancel();

        timeout(
            Duration::from_secs(2),
            join_service_tasks(server, spaces, cancel),
        )
        .await
        .unwrap()
        .unwrap();
    }

    #[tokio::test]
    async fn with_concurrency_limit_sheds_excess_requests() {
        use axum::body::Body;
        use tower::ServiceExt;

        let started = std::sync::Arc::new(tokio::sync::Notify::new());
        let handler_started = started.clone();
        let app = with_concurrency_limit(
            axum::Router::new()
                .route(
                    "/hang",
                    super::routing::get(move || {
                        let started = handler_started.clone();
                        async move {
                            started.notify_one();
                            // Hold the single permit forever.
                            std::future::pending::<String>().await
                        }
                    }),
                )
                .route("/other", super::routing::get(|| async { "ok" })),
            1,
            http::StatusCode::SERVICE_UNAVAILABLE,
        );

        let hanging = app.clone();
        let first = tokio::spawn(async move {
            let _ = hanging
                .oneshot(http::Request::get("/hang").body(Body::empty()).unwrap())
                .await;
        });
        // The permit is acquired before the handler body runs, so once the
        // handler has signalled, the sole permit is provably taken.
        timeout(Duration::from_secs(2), started.notified())
            .await
            .unwrap();

        let res = app
            .clone()
            .oneshot(http::Request::get("/hang").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(res.status(), http::StatusCode::SERVICE_UNAVAILABLE);

        // Requests to other routes are shed too: the cap is shared across
        // the whole router, not per route.
        let res = app
            .clone()
            .oneshot(http::Request::get("/other").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(res.status(), http::StatusCode::SERVICE_UNAVAILABLE);

        first.abort();
    }
}
