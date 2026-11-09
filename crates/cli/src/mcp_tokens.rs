//! Move one MCP OAuth token between machines.
//!
//! A headless host that cannot run a browser can be signed in by exporting the
//! token from a machine that can, then importing it here. The store is the one
//! `codape_mcp::oauth` reads (`~/.codape/mcp_tokens.json`, or the OS
//! keychain), so no second credential format exists.
//!
//! The exported document is a **credential**: the entry carries the access and
//! refresh tokens. `export-token` writes to stdout or to a `0600` file, and
//! never prints anything else that could be mistaken for safe output.
//!
//! Import merges one entry: an existing entry for the same server is replaced,
//! other servers and the shared issuer cache are left alone. An entry whose
//! `server_url` names a different server is refused, so a token cannot be
//! filed under the wrong endpoint by accident.

use std::io::Read as _;
use std::path::Path;

use codape_config::secret_store::TokenPersistence;
use serde_json::Value;

/// The token store the MCP client reads.
fn token_store_path() -> std::path::PathBuf {
    codape_mcp::oauth::OAuthManager::token_path()
}

/// Print (or write to `output`) the stored entry for `server`.
///
/// # Errors
///
/// Returns a message when the store or the entry does not exist, or when the
/// output file cannot be written.
pub fn export(server: &str, output: Option<&Path>) -> Result<(), String> {
    let persistence = TokenPersistence::new(token_store_path());
    let entry = export_from(&persistence, server)?;
    if let Some(path) = output {
        TokenPersistence::file_only(path)
            .save(&entry)
            .map_err(|error| format!("could not write {}: {error}", path.display()))
    } else {
        println!("{entry}");
        Ok(())
    }
}

/// Merge one exported entry for `server` from `file` (or stdin).
///
/// # Errors
///
/// Returns a message when the input is not a usable token entry, names a
/// different server, or the store cannot be written.
pub fn import(server: &str, file: Option<&Path>) -> Result<(), String> {
    let mut text = String::new();
    match file {
        Some(path) => {
            text = std::fs::read_to_string(path)
                .map_err(|error| format!("could not read {}: {error}", path.display()))?;
        }
        None => {
            std::io::stdin()
                .read_to_string(&mut text)
                .map_err(|error| format!("could not read stdin: {error}"))?;
        }
    }
    let persistence = TokenPersistence::new(token_store_path());
    import_into(&persistence, server, &text)
}

/// The exported entry for `server`, as pretty JSON.
fn export_from(persistence: &TokenPersistence, server: &str) -> Result<String, String> {
    let Some(content) = persistence
        .load()
        .map_err(|error| format!("could not read the token store: {error}"))?
    else {
        return Err("no MCP token store exists on this machine".to_string());
    };
    let store: Value = serde_json::from_str(&content)
        .map_err(|error| format!("the token store is not readable: {error}"))?;
    let states = store
        .get("states")
        .and_then(Value::as_array)
        .ok_or_else(|| "the token store has no `states` array".to_string())?;
    let Some(entry) = states
        .iter()
        .find(|entry| entry.get("server_url").and_then(Value::as_str) == Some(server))
    else {
        let known: Vec<&str> = states
            .iter()
            .filter_map(|entry| entry.get("server_url").and_then(Value::as_str))
            .collect();
        return Err(if known.is_empty() {
            format!("no token for {server}; the store has no token entries")
        } else {
            format!(
                "no token for {server}; stored servers: {}",
                known.join(", ")
            )
        });
    };
    serde_json::to_string_pretty(entry)
        .map_err(|error| format!("could not serialize the token: {error}"))
}

/// Merge `text` into `persistence` as the entry for `server`.
fn import_into(persistence: &TokenPersistence, server: &str, text: &str) -> Result<(), String> {
    let mut entry: Value =
        serde_json::from_str(text).map_err(|error| format!("the token is not JSON: {error}"))?;
    {
        let object = entry
            .as_object_mut()
            .ok_or_else(|| "the token must be a JSON object".to_string())?;
        match object.get("server_url").and_then(Value::as_str) {
            Some(existing) if existing == server => {}
            Some(existing) => {
                return Err(format!(
                    "the token belongs to {existing}, not {server}; import it under the \
                     endpoint it was issued for"
                ));
            }
            None => {
                object.insert("server_url".into(), Value::String(server.to_string()));
            }
        }
        let present = |key: &str| {
            object
                .get(key)
                .and_then(Value::as_str)
                .is_some_and(|value| !value.is_empty())
        };
        if !present("access_token") && !present("refresh_token") {
            return Err("the token has neither an access_token nor a refresh_token".to_string());
        }
    }

    let mut store: Value = match persistence
        .load()
        .map_err(|error| format!("could not read the token store: {error}"))?
    {
        Some(content) => serde_json::from_str(&content)
            .map_err(|error| format!("the token store is not readable: {error}"))?,
        None => serde_json::json!({ "states": [] }),
    };
    let root = store
        .as_object_mut()
        .ok_or_else(|| "the token store is not a JSON object".to_string())?;
    let states = root
        .entry("states")
        .or_insert_with(|| Value::Array(Vec::new()));
    let states = states
        .as_array_mut()
        .ok_or_else(|| "the token store's `states` is not an array".to_string())?;
    states.retain(|existing| existing.get("server_url").and_then(Value::as_str) != Some(server));
    states.push(entry);

    let content = serde_json::to_string_pretty(&store)
        .map_err(|error| format!("could not serialize the token store: {error}"))?;
    persistence
        .save(&content)
        .map_err(|error| format!("could not write the token store: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn store(path: &Path) -> TokenPersistence {
        TokenPersistence::file_only(path)
    }

    fn state_of(persistence: &TokenPersistence, server: &str) -> Value {
        let store: Value = serde_json::from_str(&persistence.load().unwrap().unwrap()).unwrap();
        store["states"]
            .as_array()
            .unwrap()
            .iter()
            .find(|entry| entry["server_url"] == server)
            .cloned()
            .expect("the entry is in the store")
    }

    #[test]
    fn export_returns_only_the_requested_entry() {
        let dir = tempfile::tempdir().unwrap();
        let persistence = store(&dir.path().join("mcp_tokens.json"));
        persistence
            .save(
                &json!({
                    "states": [
                        {"server_url": "https://a.example/mcp", "access_token": "a-token"},
                        {"server_url": "https://b.example/mcp", "access_token": "b-token"}
                    ]
                })
                .to_string(),
            )
            .unwrap();

        let exported = export_from(&persistence, "https://b.example/mcp").unwrap();
        let value: Value = serde_json::from_str(&exported).unwrap();
        assert_eq!(value["server_url"], "https://b.example/mcp");
        assert_eq!(value["access_token"], "b-token");

        let error = export_from(&persistence, "https://missing.example/mcp").unwrap_err();
        assert!(error.contains("https://a.example/mcp"), "{error}");
        assert!(error.contains("https://b.example/mcp"), "{error}");
    }

    #[test]
    fn import_replaces_only_that_server_and_keeps_the_rest() {
        let dir = tempfile::tempdir().unwrap();
        let persistence = store(&dir.path().join("mcp_tokens.json"));
        persistence
            .save(
                &json!({
                    "states": [
                        {"server_url": "https://a.example/mcp", "access_token": "old"},
                        {"server_url": "https://b.example/mcp", "access_token": "b-token"}
                    ],
                    "issuer_metadata": {"https://issuer": {"issuer": "https://issuer"}}
                })
                .to_string(),
            )
            .unwrap();

        import_into(
            &persistence,
            "https://a.example/mcp",
            &json!({
                "server_url": "https://a.example/mcp",
                "access_token": "new",
                "refresh_token": "refresh"
            })
            .to_string(),
        )
        .unwrap();

        assert_eq!(
            state_of(&persistence, "https://a.example/mcp")["access_token"],
            "new"
        );
        assert_eq!(
            state_of(&persistence, "https://b.example/mcp")["access_token"],
            "b-token"
        );
        let store: Value = serde_json::from_str(&persistence.load().unwrap().unwrap()).unwrap();
        assert_eq!(store["states"].as_array().unwrap().len(), 2);
        assert_eq!(
            store["issuer_metadata"]["https://issuer"]["issuer"],
            "https://issuer"
        );
    }

    #[test]
    fn import_refuses_a_foreign_server_or_a_tokenless_entry() {
        let dir = tempfile::tempdir().unwrap();
        let persistence = store(&dir.path().join("mcp_tokens.json"));

        let foreign = import_into(
            &persistence,
            "https://a.example/mcp",
            &json!({"server_url": "https://other.example/mcp", "access_token": "x"}).to_string(),
        )
        .unwrap_err();
        assert!(foreign.contains("https://other.example/mcp"), "{foreign}");

        let tokenless = import_into(
            &persistence,
            "https://a.example/mcp",
            &json!({"server_url": "https://a.example/mcp"}).to_string(),
        )
        .unwrap_err();
        assert!(tokenless.contains("access_token"), "{tokenless}");
    }

    #[test]
    fn import_adds_a_server_url_when_the_export_omitted_it() {
        let dir = tempfile::tempdir().unwrap();
        let persistence = store(&dir.path().join("mcp_tokens.json"));

        import_into(
            &persistence,
            "https://a.example/mcp",
            &json!({"access_token": "token"}).to_string(),
        )
        .unwrap();

        assert_eq!(
            state_of(&persistence, "https://a.example/mcp")["access_token"],
            "token"
        );
    }
}
