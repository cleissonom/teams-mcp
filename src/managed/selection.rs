use super::{
    failure,
    state::{profile, read_small, validate_private_profile, write_private},
};
use crate::{
    cdp::Browser,
    config::{AuthSource, Config},
    error::Failure,
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::{path::Path, time::Duration};
use tokio::time::timeout;

const FILE: &str = ".teams-mcp-selection.json";

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Selection {
    email: String,
    tenant: String,
}

pub(super) fn effective(config: &Config) -> Result<Config, Failure> {
    let mut result = config.clone();
    if result
        .account
        .as_deref()
        .is_some_and(|email| !valid_email(email))
    {
        return Err(invalid());
    }
    result.tenant = result.tenant.as_deref().map(tenant).transpose()?;
    let path = profile(config)?;
    validate_private_profile(&path)?;
    if result.account.is_some() && result.tenant.is_some() {
        return Ok(result);
    }
    let Some(saved) = read_small(&path.join(FILE)).map_err(|_| invalid())? else {
        return Ok(result);
    };
    let mut saved: Selection = serde_json::from_str(&saved).map_err(|_| invalid())?;
    if !valid_email(&saved.email) {
        return Err(invalid());
    }
    saved.tenant = tenant(&saved.tenant)?;
    if result
        .account
        .as_deref()
        .is_some_and(|email| !email.eq_ignore_ascii_case(&saved.email))
        || result
            .tenant
            .as_deref()
            .is_some_and(|tenant| tenant != saved.tenant)
    {
        return Err(failure("ACCOUNT_SELECTION_CONFLICT"));
    }
    result.account = result.account.or(Some(saved.email));
    result.tenant = result.tenant.or(Some(saved.tenant));
    Ok(result)
}

pub(super) fn startup_url(config: &Config) -> Result<String, Failure> {
    let mut url = url::Url::parse("https://teams.microsoft.com/v2/").expect("fixed Teams URL");
    if let Some(value) = &config.tenant {
        url.query_pairs_mut()
            .append_pair("tenantId", &tenant(value)?);
    }
    if let Some(email) = &config.account {
        if !valid_email(email) {
            return Err(invalid());
        }
        url.query_pairs_mut().append_pair("login_hint", email);
    }
    Ok(url.into())
}

pub(super) async fn persist(config: &Config, path: &Path, endpoint: &str) {
    let Ok(mut snapshot) = effective(config) else {
        return;
    };
    snapshot.auth_source = AuthSource::Browser;
    snapshot.cdp_url = Some(endpoint.into());
    snapshot.target_id = None;
    let selected = snapshot.clone();
    let Ok(Ok(response)) = timeout(
        Duration::from_secs(3),
        Browser::new(snapshot).evaluate(json!({"action":"status"})),
    )
    .await
    else {
        return;
    };
    if response["ok"] != true {
        return;
    }
    let (Some(email), Some(raw_tenant)) = (
        response["profile"]["email"].as_str(),
        response["profile"]["tenantId"].as_str(),
    ) else {
        return;
    };
    let Ok(tenant) = tenant(raw_tenant) else {
        return;
    };
    if !valid_email(email)
        || selected
            .account
            .as_deref()
            .is_some_and(|value| !value.eq_ignore_ascii_case(email))
        || selected
            .tenant
            .as_deref()
            .is_some_and(|value| value != tenant)
    {
        return;
    }
    let selection = Selection {
        email: email.into(),
        tenant,
    };
    if let Ok(bytes) = serde_json::to_vec(&selection) {
        let _ = write_private(&path.join(FILE), &bytes);
    }
}

fn tenant(value: &str) -> Result<String, Failure> {
    let id = uuid::Uuid::parse_str(value).map_err(|_| invalid())?;
    if id.is_nil() || !id.to_string().eq_ignore_ascii_case(value) {
        return Err(invalid());
    }
    Ok(id.to_string())
}

fn valid_email(value: &str) -> bool {
    let Some((local, domain)) = value.split_once('@') else {
        return false;
    };
    !local.is_empty()
        && local.len() <= 64
        && value.len() <= 254
        && value.is_ascii()
        && !local.starts_with('.')
        && !local.ends_with('.')
        && !local.contains("..")
        && local
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || b"!#$%&'*+-/=?^_`{|}~.".contains(&c))
        && domain.split('.').all(|part| {
            !part.is_empty()
                && part.len() <= 63
                && part
                    .as_bytes()
                    .first()
                    .is_some_and(u8::is_ascii_alphanumeric)
                && part
                    .as_bytes()
                    .last()
                    .is_some_and(u8::is_ascii_alphanumeric)
                && part.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'-')
        })
}

fn invalid() -> Failure {
    failure("AUTH_SELECTION_INVALID")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{fs, path::PathBuf};
    struct Fixture(PathBuf);
    impl Fixture {
        fn new() -> Self {
            let path =
                std::env::temp_dir().join(format!("teams-selection-{}", uuid::Uuid::new_v4()));
            super::super::state::private_directory(&path).unwrap();
            Self(path)
        }
        fn config(&self) -> Config {
            Config {
                browser_profile: Some(self.0.clone()),
                ..Default::default()
            }
        }
        fn save(&self, value: &str) {
            fs::write(self.0.join(FILE), value).unwrap();
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }
    const TENANT: &str = "22222222-2222-4222-8222-222222222222";
    fn saved() -> String {
        json!({"email":"guest@example.test","tenant":TENANT}).to_string()
    }

    #[test]
    fn missing_selection_is_harmless_and_saved_pair_fills_missing_fields() {
        let f = Fixture::new();
        assert!(effective(&f.config()).unwrap().account.is_none());
        f.save(&saved());
        let loaded = effective(&f.config()).unwrap();
        assert_eq!(loaded.account.as_deref(), Some("guest@example.test"));
        assert_eq!(loaded.tenant.as_deref(), Some(TENANT));
    }

    #[test]
    fn partial_configuration_cannot_mix_a_new_identity_with_saved_selection() {
        let f = Fixture::new();
        f.save(&saved());
        for (account, tenant) in [
            (Some("other@example.test"), None),
            (None, Some("33333333-3333-4333-8333-333333333333")),
        ] {
            let config = Config {
                account: account.map(str::to_owned),
                tenant: tenant.map(str::to_owned),
                ..f.config()
            };
            assert_eq!(
                effective(&config).err().unwrap().code,
                "ACCOUNT_SELECTION_CONFLICT"
            );
        }
        let config = Config {
            account: Some("other@example.test".into()),
            tenant: Some(TENANT.into()),
            ..f.config()
        };
        assert_eq!(
            effective(&config).unwrap().account.as_deref(),
            Some("other@example.test")
        );
    }

    #[test]
    fn malformed_metadata_and_untrusted_selector_values_have_fixed_errors() {
        let f = Fixture::new();
        for value in [
            "invalid-private-metadata".into(),
            json!({"email":"guest@example.test","tenant":"common"}).to_string(),
            json!({"email":"guest@example.test","tenant":TENANT,"token":"never-echo"}).to_string(),
            "x".repeat(4097),
        ] {
            f.save(&value);
            let error = effective(&f.config()).err().unwrap();
            assert_eq!(error.code, "AUTH_SELECTION_INVALID");
            assert!(!error.message.contains("never-echo"));
        }
        for tenant in [
            "common",
            "https://evil.test/",
            "22222222222242228222222222222222",
        ] {
            let config = Config {
                tenant: Some(tenant.into()),
                ..f.config()
            };
            assert_eq!(
                startup_url(&config).unwrap_err().code,
                "AUTH_SELECTION_INVALID"
            );
        }
        let config = Config {
            account: Some("private\r\n@example.test".into()),
            ..f.config()
        };
        assert_eq!(
            startup_url(&config).unwrap_err().code,
            "AUTH_SELECTION_INVALID"
        );
    }
}
