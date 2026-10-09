//! Settings › Sign-in providers (PX-22): `auth.oidc` as the tab's forms,
//! and back. The setting is one list, saved whole. A provider's client
//! secret comes from an environment variable (`${SF_ENV_…}`, all the
//! settings file holds), a secret pasted here (which the server keeps
//! apart from the settings, unencrypted), or a file on the server.

use serde_json::{Map, Value};

/// What an environment variable the server reads starts with.
pub const ENV_PREFIX: &str = "SF_ENV_";

/// What the settings API shows a client secret written into the config
/// file as: never the secret, and refused back.
pub const HIDDEN: &str = "(hidden)";

/// The scopes asked for when none are listed.
pub const DEFAULT_SCOPES: [&str; 3] = ["openid", "profile", "email"];

/// The most characters a provider's name has: it is a button's label.
pub const MAX_NAME: usize = 40;

/// Where a provider's client secret comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SecretFrom {
    /// `client_secret = "${SF_ENV_…}"`: the recommended way.
    #[default]
    Env,
    /// Pasted here: `secrets/oidc-<name>` in the state directory.
    Pasted,
    /// `client_secret_file`.
    File,
    /// A literal `client_secret` in the server's config file, which the
    /// API shows as [`HIDDEN`]: this page cannot write it back.
    ConfigFile,
}

/// One provider as the form has it.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Provider {
    pub name: String,
    pub issuer: String,
    pub client_id: String,
    pub scopes: Vec<String>,
    pub from: SecretFrom,
    /// The variable's name after `SF_ENV_`, for [`SecretFrom::Env`].
    pub env: String,
    /// The path, for [`SecretFrom::File`].
    pub file: String,
}

fn field(object: &Map<String, Value>, key: &str) -> String {
    object
        .get(key)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned()
}

impl Provider {
    /// A provider about to be added: the default scopes, the secret from
    /// an environment variable.
    pub fn new() -> Self {
        Self {
            scopes: DEFAULT_SCOPES.map(str::to_owned).to_vec(),
            ..Self::default()
        }
    }

    /// One entry of `auth.oidc` as the settings API shows it.
    pub fn from_value(value: &Value) -> Self {
        let Some(object) = value.as_object() else {
            return Self::new();
        };
        let scopes = match object.get("scopes") {
            Some(Value::Array(list)) => list
                .iter()
                .filter_map(|s| s.as_str().map(str::to_owned))
                .collect(),
            _ => DEFAULT_SCOPES.map(str::to_owned).to_vec(),
        };
        let file = field(object, "client_secret_file");
        let secret = object.get("client_secret").and_then(Value::as_str);
        let (from, env) = match secret {
            _ if !file.is_empty() => (SecretFrom::File, String::new()),
            Some(HIDDEN) => (SecretFrom::ConfigFile, String::new()),
            Some(s) => match env_variable(s) {
                Some(name) => (SecretFrom::Env, name.to_owned()),
                // Anything else is the file's too: the runtime layer takes
                // only a reference.
                None => (SecretFrom::ConfigFile, String::new()),
            },
            None => (SecretFrom::Pasted, String::new()),
        };
        Self {
            name: field(object, "name"),
            issuer: field(object, "issuer"),
            client_id: field(object, "client_id"),
            scopes,
            from,
            env,
            file,
        }
    }

    /// Whether its scopes are not the default ones: shown only with the
    /// advanced settings.
    pub fn custom_scopes(&self) -> bool {
        self.scopes != DEFAULT_SCOPES
    }

    /// The provider as an entry of `auth.oidc`; `Err` says, in the words
    /// the form shows, what is missing or wrong. A pasted secret is not
    /// in it: the server keeps that apart.
    pub fn to_value(&self) -> Result<Value, String> {
        let name = self.name.trim();
        if name.is_empty() {
            return Err("Give the provider a name: its button says “Continue with” it.".into());
        }
        if name.chars().count() > MAX_NAME {
            return Err(format!(
                "A provider's name is at most {MAX_NAME} characters."
            ));
        }
        let issuer = self.issuer.trim().trim_end_matches('/');
        if !issuer_allowed(issuer) {
            return Err(
                "The issuer is the provider's https:// address, such as https://id.example.org."
                    .into(),
            );
        }
        let client_id = self.client_id.trim();
        if client_id.is_empty() {
            return Err("Type the client ID the provider gave you.".into());
        }
        let mut object = Map::new();
        object.insert("name".into(), name.into());
        object.insert("issuer".into(), issuer.into());
        object.insert("client_id".into(), client_id.into());
        match self.from {
            SecretFrom::Env => {
                object.insert("client_secret".into(), env_reference(&self.env)?.into());
            }
            SecretFrom::File => match self.file.trim() {
                "" => {
                    return Err(
                        "Type the path of the file on the server that holds the secret.".into(),
                    );
                }
                path => {
                    object.insert("client_secret_file".into(), path.into());
                }
            },
            SecretFrom::Pasted => {}
            SecretFrom::ConfigFile => {
                return Err(format!(
                    "{name}'s client secret is written in the server's config file, which this \
                     page cannot copy. Choose where it comes from here, or change the providers \
                     in the config file."
                ));
            }
        }
        let scopes: Vec<Value> = self
            .scopes
            .iter()
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
            .map(Value::from)
            .collect();
        if !scopes.is_empty() && scopes != DEFAULT_SCOPES.map(Value::from) {
            object.insert("scopes".into(), Value::Array(scopes));
        }
        Ok(Value::Object(object))
    }

    /// Where its secret comes from, as its row in the list says it.
    pub fn secret_line(&self) -> String {
        match self.from {
            SecretFrom::Env => format!(
                "Secret from the environment variable {ENV_PREFIX}{}",
                self.env
            ),
            SecretFrom::Pasted => {
                "Secret pasted here, kept unencrypted on the server's disk".into()
            }
            SecretFrom::File => format!("Secret read from {} on the server", self.file),
            SecretFrom::ConfigFile => "Secret set in the server's config file".into(),
        }
    }
}

/// HTTPS, or HTTP on this machine, as the server takes an issuer.
fn issuer_allowed(issuer: &str) -> bool {
    // The host, without its port: `[::1]` whole, else up to a colon.
    let host = |rest: &str| -> String {
        let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
        match authority.find(']') {
            Some(end) if authority.starts_with('[') => authority[..=end].to_owned(),
            _ => authority.split(':').next().unwrap_or_default().to_owned(),
        }
    };
    if let Some(rest) = issuer.strip_prefix("https://") {
        return !host(rest).is_empty();
    }
    issuer.strip_prefix("http://").is_some_and(|rest| {
        let host = host(rest);
        host == "localhost" || host.starts_with("127.") || host == "[::1]"
    })
}

/// The variable `${SF_ENV_NAME}` names, after its prefix: `NAME`.
fn env_variable(secret: &str) -> Option<&str> {
    secret
        .strip_prefix("${")?
        .strip_suffix('}')?
        .strip_prefix(ENV_PREFIX)
        .filter(|rest| valid_env_rest(rest))
}

fn valid_env_rest(rest: &str) -> bool {
    !rest.is_empty() && rest.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// A typed variable name as the reference saved: `OIDC_SECRET`,
/// `SF_ENV_OIDC_SECRET` and `${SF_ENV_OIDC_SECRET}` are all
/// `${SF_ENV_OIDC_SECRET}`. `Err` says what is wrong with it.
pub fn env_reference(typed: &str) -> Result<String, String> {
    let typed = typed.trim();
    let typed = typed
        .strip_prefix("${")
        .and_then(|t| t.strip_suffix('}'))
        .unwrap_or(typed);
    let rest = typed.strip_prefix(ENV_PREFIX).unwrap_or(typed);
    if rest.is_empty() {
        return Err("Type the environment variable's name.".into());
    }
    if !valid_env_rest(rest) {
        return Err("A variable's name is letters, digits and _ only.".into());
    }
    Ok(format!("${{{ENV_PREFIX}{rest}}}"))
}

/// `auth.oidc` as the tab lists it.
pub fn providers(value: Option<&Value>) -> Vec<Provider> {
    match value {
        Some(Value::Array(list)) => list.iter().map(Provider::from_value).collect(),
        _ => Vec::new(),
    }
}

/// The providers as `auth.oidc`'s entries; `Err` from the first that
/// cannot be saved.
pub fn entries(list: &[Provider]) -> Result<Vec<Value>, String> {
    list.iter().map(Provider::to_value).collect()
}

/// What a change to a provider does to the pasted secrets, besides the
/// setting: `forget`, the name whose pasted secret is deleted first (it
/// is kept under the name, and a provider whose secret comes from
/// elsewhere cannot have one); `paste`, the secret to keep after.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SecretSteps {
    pub forget: Option<String>,
    pub paste: Option<String>,
}

/// The [`SecretSteps`] for `old` becoming `new` (`None`: removed), with
/// `typed` in the secret field. `Err` when a pasted secret is needed and
/// none was typed.
pub fn secret_steps(
    old: Option<&Provider>,
    new: Option<&Provider>,
    typed: &str,
) -> Result<SecretSteps, String> {
    let typed = typed.trim();
    let kept = old.filter(|o| o.from == SecretFrom::Pasted);
    let same_name = |a: &Provider, b: &Provider| a.name.trim() == b.name.trim();
    let forget = match (kept, new) {
        (Some(old), Some(new)) if new.from == SecretFrom::Pasted && same_name(old, new) => None,
        (Some(old), _) => Some(old.name.trim().to_owned()),
        (None, _) => None,
    };
    let paste = match new {
        Some(new) if new.from == SecretFrom::Pasted => match typed {
            "" if kept.is_some_and(|old| same_name(old, new)) => None,
            "" if kept.is_some() => {
                return Err("Paste the secret again: it is kept under the provider's name.".into());
            }
            "" => return Err("Paste the client secret.".into()),
            secret => Some(secret.to_owned()),
        },
        _ => None,
    };
    Ok(SecretSteps { forget, paste })
}

/// The address to register at each provider as its callback, under the
/// web UI's public address; `None` without one.
pub fn callback_url(public_url: &str) -> Option<String> {
    match public_url.trim().trim_end_matches('/') {
        "" => None,
        base => Some(format!("{base}/auth/oidc/callback")),
    }
}

/// How many providers use what only the advanced settings show: a secret
/// from a file, or scopes of their own.
pub fn advanced_set(value: Option<&Value>) -> usize {
    providers(value)
        .iter()
        .filter(|p| p.from == SecretFrom::File || p.custom_scopes())
        .count()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn pocket() -> Provider {
        Provider {
            name: "Pocket ID".into(),
            issuer: "https://id.example.org".into(),
            client_id: "spoolframe".into(),
            env: "OIDC_SECRET".into(),
            ..Provider::new()
        }
    }

    #[test]
    fn a_variable_name_is_saved_as_an_sf_env_reference() {
        for typed in [
            "OIDC_SECRET",
            " SF_ENV_OIDC_SECRET ",
            "${SF_ENV_OIDC_SECRET}",
            "${OIDC_SECRET}",
        ] {
            assert_eq!(
                env_reference(typed).unwrap(),
                "${SF_ENV_OIDC_SECRET}",
                "{typed}"
            );
        }
    }

    #[test]
    fn a_variable_name_is_letters_digits_and_underscores() {
        assert_eq!(
            env_reference("OIDC-SECRET").unwrap_err(),
            "A variable's name is letters, digits and _ only."
        );
        assert!(env_reference("OIDC SECRET").is_err());
        assert!(env_reference("SF_ENV_").is_err());
        assert!(env_reference("").is_err());
    }

    #[test]
    fn an_env_provider_saves_its_reference_and_leaves_the_default_scopes_out() {
        assert_eq!(
            pocket().to_value().unwrap(),
            json!({
                "name": "Pocket ID",
                "issuer": "https://id.example.org",
                "client_id": "spoolframe",
                "client_secret": "${SF_ENV_OIDC_SECRET}",
            })
        );
    }

    #[test]
    fn a_pasted_secret_is_not_in_the_setting() {
        let pasted = Provider {
            from: SecretFrom::Pasted,
            ..pocket()
        };
        let value = pasted.to_value().unwrap();
        assert!(value.get("client_secret").is_none(), "{value}");
        assert!(value.get("client_secret_file").is_none(), "{value}");
    }

    #[test]
    fn a_file_provider_saves_only_its_path() {
        let file = Provider {
            from: SecretFrom::File,
            file: " /run/secrets/oidc ".into(),
            ..pocket()
        };
        let value = file.to_value().unwrap();
        assert_eq!(value["client_secret_file"], json!("/run/secrets/oidc"));
        assert!(value.get("client_secret").is_none(), "{value}");
    }

    #[test]
    fn scopes_of_its_own_are_saved() {
        let custom = Provider {
            scopes: vec!["openid".into(), "groups".into()],
            ..pocket()
        };
        assert_eq!(
            custom.to_value().unwrap()["scopes"],
            json!(["openid", "groups"])
        );
        assert!(custom.custom_scopes());
        assert!(!pocket().custom_scopes());
    }

    #[test]
    fn a_secret_from_the_config_file_is_never_written_back() {
        let shown = json!({
            "name": "Pocket ID", "issuer": "https://id.example.org",
            "client_id": "spoolframe", "client_secret": HIDDEN,
        });
        let read = Provider::from_value(&shown);
        assert_eq!(read.from, SecretFrom::ConfigFile);
        assert_eq!(read.secret_line(), "Secret set in the server's config file");
        assert!(read.to_value().unwrap_err().contains("config file"));
    }

    #[test]
    fn each_source_reads_back_from_the_setting() {
        for p in [
            pocket(),
            Provider {
                from: SecretFrom::Pasted,
                env: String::new(),
                ..pocket()
            },
            Provider {
                from: SecretFrom::File,
                file: "/run/secrets/oidc".into(),
                env: String::new(),
                ..pocket()
            },
        ] {
            assert_eq!(Provider::from_value(&p.to_value().unwrap()), p);
        }
    }

    #[test]
    fn a_name_is_one_to_forty_characters() {
        let blank = Provider {
            name: "  ".into(),
            ..pocket()
        };
        assert!(blank.to_value().is_err());
        let long = Provider {
            name: "x".repeat(41),
            ..pocket()
        };
        assert_eq!(
            long.to_value().unwrap_err(),
            "A provider's name is at most 40 characters."
        );
    }

    #[test]
    fn the_issuer_is_https_or_http_on_this_machine() {
        for ok in [
            "https://id.example.org",
            "http://localhost:8080",
            "http://127.0.0.1:1411/",
        ] {
            let p = Provider {
                issuer: ok.into(),
                ..pocket()
            };
            assert!(p.to_value().is_ok(), "{ok}");
        }
        for bad in ["http://id.example.org", "id.example.org", "https://"] {
            let p = Provider {
                issuer: bad.into(),
                ..pocket()
            };
            assert!(p.to_value().is_err(), "{bad}");
        }
    }

    #[test]
    fn leaving_a_pasted_secret_deletes_it_first() {
        let pasted = Provider {
            from: SecretFrom::Pasted,
            ..pocket()
        };
        let steps = secret_steps(Some(&pasted), Some(&pocket()), "").unwrap();
        assert_eq!(steps.forget.as_deref(), Some("Pocket ID"));
        assert_eq!(steps.paste, None);
        let removed = secret_steps(Some(&pasted), None, "").unwrap();
        assert_eq!(removed.forget.as_deref(), Some("Pocket ID"));
    }

    #[test]
    fn a_pasted_secret_is_kept_unless_another_is_pasted() {
        let pasted = Provider {
            from: SecretFrom::Pasted,
            ..pocket()
        };
        assert_eq!(
            secret_steps(Some(&pasted), Some(&pasted), "").unwrap(),
            SecretSteps::default()
        );
        assert_eq!(
            secret_steps(Some(&pasted), Some(&pasted), " s3cret ").unwrap(),
            SecretSteps {
                forget: None,
                paste: Some("s3cret".into())
            }
        );
    }

    #[test]
    fn a_new_or_renamed_pasted_provider_needs_the_secret() {
        let pasted = Provider {
            from: SecretFrom::Pasted,
            ..pocket()
        };
        assert_eq!(
            secret_steps(None, Some(&pasted), "").unwrap_err(),
            "Paste the client secret."
        );
        let renamed = Provider {
            name: "Pocket".into(),
            ..pasted.clone()
        };
        assert!(secret_steps(Some(&pasted), Some(&renamed), "").is_err());
        assert_eq!(
            secret_steps(Some(&pasted), Some(&renamed), "s3cret").unwrap(),
            SecretSteps {
                forget: Some("Pocket ID".into()),
                paste: Some("s3cret".into())
            }
        );
    }

    #[test]
    fn the_callback_is_under_the_public_address() {
        assert_eq!(
            callback_url("https://media.example.org/").as_deref(),
            Some("https://media.example.org/auth/oidc/callback")
        );
        assert_eq!(callback_url("  "), None);
    }

    #[test]
    fn files_and_scopes_of_their_own_count_as_advanced() {
        let value = json!([
            pocket().to_value().unwrap(),
            {"name": "A", "issuer": "https://a.example.org", "client_id": "x",
             "client_secret_file": "/run/secrets/a"},
            {"name": "B", "issuer": "https://b.example.org", "client_id": "x",
             "scopes": ["openid"]},
        ]);
        assert_eq!(advanced_set(Some(&value)), 2);
        assert_eq!(advanced_set(None), 0);
    }
}
