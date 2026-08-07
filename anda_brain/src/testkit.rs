//! Shared test fixtures: the canonical in-memory `AppState` wiring and
//! space bootstrap that every test module used to copy verbatim. Mock
//! completers stay with their test modules — they encode per-scenario
//! model behaviour, not shared wiring.

use anda_core::Principal;
use anda_db::{database::DBConfig, storage::StorageConfig};
use anda_engine::{
    management::{BaseManagement, Visibility},
    model::{CompletionFeaturesDyn, Model, Models, reqwest},
    unix_ms,
};
use ic_cose_types::cose::ed25519::VerifyingKey;
use object_store::memory::InMemory;
use std::{collections::BTreeSet, sync::Arc};

use crate::{
    agents::SELF_USER_ID,
    space::{AppState, Space},
};

pub(crate) fn db_config(name: &str) -> DBConfig {
    DBConfig {
        name: name.to_string(),
        description: "test database".to_string(),
        storage: StorageConfig::default(),
        lock: None,
    }
}

/// The canonical test `AppState`: in-memory object store, a public
/// `BaseManagement` controlled by `SELF_USER_ID`, and a default HTTP
/// client. Per-module helpers wrap this with their preferred models,
/// pubkeys, version string, and sharding.
pub(crate) fn app_state_core(
    name: &str,
    models: Arc<Models>,
    ed25519_pubkeys: Vec<VerifyingKey>,
    app_version: &str,
    sharding: u32,
) -> AppState {
    let management = Arc::new(BaseManagement {
        controller: SELF_USER_ID,
        managers: BTreeSet::new(),
        visibility: Visibility::Public,
    });
    let http_client = reqwest::Client::builder().build().unwrap();

    AppState::new(
        Arc::new(InMemory::new()),
        Arc::new(db_config(name)),
        management,
        http_client,
        models,
        Arc::new(ed25519_pubkeys),
        "anda_brain".to_string(),
        app_version.to_string(),
        sharding,
    )
}

/// `Models` with the given mock completer installed as the default model.
pub(crate) fn models_with_completer(completer: impl CompletionFeaturesDyn) -> Arc<Models> {
    models_with_configured_completer(completer, |_| {})
}

/// `models_with_completer` with a hook to tweak the `Model` (e.g. token
/// limits) before it is installed.
pub(crate) fn models_with_configured_completer(
    completer: impl CompletionFeaturesDyn,
    configure: impl FnOnce(&mut Model),
) -> Arc<Models> {
    let models = Models::default();
    let mut model = Model::with_completer(Arc::new(completer));
    configure(&mut model);
    models.set_model(model);
    Arc::new(models)
}

/// Admin-creates space `id` (creator `[1]`, owner `[2]`, tier 1) and loads
/// it unpinned.
pub(crate) async fn create_loaded_space(app: &AppState, id: &str) -> Arc<Space> {
    app.admin_create_space(
        Principal::from_slice(&[1]),
        Principal::from_slice(&[2]),
        id.to_string(),
        1,
        unix_ms(),
    )
    .await
    .unwrap();

    app.load_space(id, false).await.unwrap()
}
