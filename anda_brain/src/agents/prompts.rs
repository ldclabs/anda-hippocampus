//! Agent system prompts with a process-wide override layer.
//!
//! The three system prompts are the evolvable genome of Anda Brain: the eval
//! optimizer (`anda_brain::eval::optimize`) proposes targeted edits, installs
//! them here, and re-runs the eval suite as a fitness function. Agents read
//! the active prompt at completion time, so overrides take effect immediately
//! without rebuilding spaces. Production runs never set overrides.

use serde::{Deserialize, Serialize};
use std::sync::{Arc, OnceLock, RwLock};

/// Compiled-in default prompts.
pub const FORMATION_DEFAULT: &str = include_str!("../../assets/BrainFormation.md");
pub const RECALL_DEFAULT: &str = include_str!("../../assets/BrainRecall.md");
pub const MAINTENANCE_DEFAULT: &str = include_str!("../../assets/BrainMaintenance.md");

/// Which agent prompt an override or optimizer edit targets.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum PromptTarget {
    Formation,
    #[default]
    Recall,
    Maintenance,
}

impl PromptTarget {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Formation => "formation",
            Self::Recall => "recall",
            Self::Maintenance => "maintenance",
        }
    }

    pub fn default_prompt(self) -> &'static str {
        match self {
            Self::Formation => FORMATION_DEFAULT,
            Self::Recall => RECALL_DEFAULT,
            Self::Maintenance => MAINTENANCE_DEFAULT,
        }
    }
}

static OVERRIDES: RwLock<[Option<Arc<str>>; 3]> = RwLock::new([None, None, None]);

/// Shared `Arc` copies of the compiled defaults, built once per slot so the
/// hot completion path never re-copies a multi-KB prompt string.
static DEFAULTS: [OnceLock<Arc<str>>; 3] = [OnceLock::new(), OnceLock::new(), OnceLock::new()];

fn slot(target: PromptTarget) -> usize {
    match target {
        PromptTarget::Formation => 0,
        PromptTarget::Recall => 1,
        PromptTarget::Maintenance => 2,
    }
}

/// Returns the active prompt: the installed override, or the compiled default.
pub fn active_prompt(target: PromptTarget) -> Arc<str> {
    {
        let overrides = OVERRIDES.read().expect("prompt overrides lock poisoned");
        if let Some(text) = &overrides[slot(target)] {
            return text.clone();
        }
    }
    DEFAULTS[slot(target)]
        .get_or_init(|| Arc::from(target.default_prompt()))
        .clone()
}

/// Installs (`Some`) or clears (`None`) a process-wide prompt override.
pub fn set_override(target: PromptTarget, text: Option<String>) {
    let mut overrides = OVERRIDES.write().expect("prompt overrides lock poisoned");
    overrides[slot(target)] = text.map(Arc::from);
}

/// Clears all overrides; used between optimizer generations and in tests.
pub fn clear_overrides() {
    let mut overrides = OVERRIDES.write().expect("prompt overrides lock poisoned");
    *overrides = [None, None, None];
}

/// Serializes tests that touch the process-wide override state.
#[cfg(test)]
pub(crate) static TEST_GUARD: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn overrides_replace_and_restore_defaults() {
        let _guard = TEST_GUARD.lock().unwrap_or_else(|err| err.into_inner());
        clear_overrides();
        assert_eq!(&*active_prompt(PromptTarget::Recall), RECALL_DEFAULT);

        set_override(PromptTarget::Recall, Some("custom recall".to_string()));
        assert_eq!(&*active_prompt(PromptTarget::Recall), "custom recall");
        // Other targets are untouched.
        assert_eq!(&*active_prompt(PromptTarget::Formation), FORMATION_DEFAULT);

        set_override(PromptTarget::Recall, None);
        assert_eq!(&*active_prompt(PromptTarget::Recall), RECALL_DEFAULT);

        set_override(PromptTarget::Maintenance, Some("m".to_string()));
        clear_overrides();
        assert_eq!(
            &*active_prompt(PromptTarget::Maintenance),
            MAINTENANCE_DEFAULT
        );
    }
}
