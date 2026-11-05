use crate::{config::Config, error::Failure};
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};

/// Session identity outlives CDP connections and access tokens, including failures.
#[derive(Clone, Default)]
pub(crate) struct IdentityBinding(Arc<Mutex<Option<Identity>>>);

struct Identity {
    id: String,
    email: Option<String>,
    tenant: String,
}

impl IdentityBinding {
    pub(crate) fn selection(&self, config: &Config) -> (Option<String>, Option<String>) {
        let bound = self.0.lock().unwrap();
        (
            bound
                .as_ref()
                .and_then(|identity| identity.email.clone())
                .or(config.account.clone()),
            bound
                .as_ref()
                .map(|identity| identity.tenant.clone())
                .or(config.tenant.clone()),
        )
    }

    pub(crate) fn apply(&self, config: &Config, input: &mut Value) -> Result<(), Failure> {
        if self
            .0
            .lock()
            .unwrap()
            .as_ref()
            .is_some_and(|identity| !identity.matches(config))
        {
            return Err(mismatch());
        }
        let (account, tenant) = self.selection(config);
        input["expectedAccount"] = json!(account);
        input["tenantId"] = json!(tenant);
        Ok(())
    }

    pub(crate) fn check(&self, config: &Config, response: &Value) -> Result<(), Failure> {
        let Some(identity) = Identity::from_response(response) else {
            return Ok(());
        };
        let mut bound = self.0.lock().unwrap();
        if !identity.matches(config)
            || bound.as_ref().is_some_and(|previous| {
                previous.id != identity.id || previous.tenant != identity.tenant
            })
        {
            return Err(mismatch());
        }
        bound.get_or_insert(identity);
        Ok(())
    }
}

impl Identity {
    fn from_response(response: &Value) -> Option<Self> {
        if !response["ok"].is_boolean() {
            return None;
        }
        let profile = &response["profile"];
        Some(Self {
            id: profile["id"].as_str().filter(|id| !id.is_empty())?.into(),
            email: profile["email"]
                .as_str()
                .filter(|email| !email.is_empty())
                .map(String::from),
            tenant: profile["tenantId"]
                .as_str()
                .filter(|tenant| !tenant.is_empty())?
                .into(),
        })
    }

    fn matches(&self, config: &Config) -> bool {
        config.account.as_deref().is_none_or(|account| {
            self.email
                .as_deref()
                .is_some_and(|email| account.eq_ignore_ascii_case(email))
        }) && config
            .tenant
            .as_deref()
            .is_none_or(|tenant| tenant == self.tenant)
    }
}

fn mismatch() -> Failure {
    Failure::new(
        "ACCOUNT_MISMATCH",
        "The Teams session does not match the selected account and organization. Sign in to the original account and tenant; the connector did not switch identities.",
    )
}
