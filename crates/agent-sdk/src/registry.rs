//! The plugin interface and the registry that routes requests to plugins.

use controldeck_proto::{Event, RpcError};
use futures_util::Stream;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use std::{collections::BTreeMap, pin::Pin, sync::Arc};

/// Events produced by a streaming method.
pub type EventStream = Pin<Box<dyn Stream<Item = Event> + Send>>;

/// A privileged module compiled into the agent.
///
/// A plugin owns a namespace (its [`name`](Plugin::name)) and answers
/// `call`s (one JSON result) and `stream`s (many events). Everything the
/// plugin receives came from a peer the agent already authenticated, but it
/// is still *untrusted input*: a compromised web server is exactly the threat
/// this process boundary exists for. Plugins must therefore validate every
/// parameter themselves and expose narrow, typed operations, never "run this
/// command".
#[async_trait::async_trait]
pub trait Plugin: Send + Sync + 'static {
    /// Unique module name: lowercase ASCII letters, digits, `-` and `_`.
    fn name(&self) -> &'static str;

    /// Short description for `agent.modules`.
    fn description(&self) -> &'static str {
        ""
    }

    async fn call(&self, method: &str, params: Value) -> Result<Value, RpcError>;

    /// Streaming methods. The default has none.
    async fn stream(&self, method: &str, _params: Value) -> Result<EventStream, RpcError> {
        Err(unknown_method(self.name(), method))
    }
}

/// The standard error for a method a plugin does not have.
pub fn unknown_method(module: &str, method: &str) -> RpcError {
    RpcError::not_found(format!("unknown method {module}.{method}"))
}

/// Deserializes request parameters; a missing (`null`) value counts as `{}`.
pub fn params<T: DeserializeOwned>(value: Value) -> Result<T, RpcError> {
    let value = if value.is_null() { json!({}) } else { value };
    serde_json::from_value(value).map_err(|e| RpcError::bad_request(format!("invalid parameters: {e}")))
}

fn valid_module_name(n: &str) -> bool {
    !n.is_empty()
        && n.len() <= 32
        && n.starts_with(|c: char| c.is_ascii_lowercase())
        && n.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_')
}

/// Name of the built-in module (`ping`, `modules`).
pub const BUILTIN: &str = "agent";

#[derive(Default)]
pub struct Registry {
    plugins: BTreeMap<&'static str, Arc<dyn Plugin>>,
}

impl Registry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Registers a plugin.
    ///
    /// # Panics
    /// On an invalid or duplicate name: that is a programming error found at
    /// startup, not a runtime condition.
    pub fn register(&mut self, plugin: impl Plugin) -> &mut Self {
        let name = plugin.name();
        assert!(valid_module_name(name) && name != BUILTIN, "invalid plugin name {name:?}");
        assert!(self.plugins.insert(name, Arc::new(plugin)).is_none(), "plugin {name:?} registered twice");
        self
    }

    pub fn names(&self) -> Vec<&'static str> {
        self.plugins.keys().copied().collect()
    }

    fn builtin(&self, method: &str) -> Result<Value, RpcError> {
        match method {
            "ping" => Ok(json!({"pong": true})),
            "modules" => Ok(json!({
                "modules": self.plugins.values().map(|p| json!({"name": p.name(), "description": p.description()})).collect::<Vec<_>>()
            })),
            m => Err(unknown_method(BUILTIN, m)),
        }
    }

    pub async fn call(&self, module: &str, method: &str, params: Value) -> Result<Value, RpcError> {
        if module == BUILTIN {
            return self.builtin(method);
        }
        match self.plugins.get(module) {
            Some(p) => p.call(method, params).await,
            None => Err(RpcError::not_found(format!("unknown module {module}"))),
        }
    }

    pub async fn stream(&self, module: &str, method: &str, params: Value) -> Result<EventStream, RpcError> {
        match self.plugins.get(module) {
            Some(p) => p.stream(method, params).await,
            None => Err(RpcError::not_found(format!("unknown module {module}"))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Echo;

    #[async_trait::async_trait]
    impl Plugin for Echo {
        fn name(&self) -> &'static str {
            "echo"
        }
        async fn call(&self, method: &str, params: Value) -> Result<Value, RpcError> {
            match method {
                "say" => Ok(params),
                m => Err(unknown_method("echo", m)),
            }
        }
    }

    #[tokio::test]
    async fn routes_to_plugins_and_builtin() {
        let mut r = Registry::new();
        r.register(Echo);
        assert_eq!(r.call("echo", "say", json!({"a": 1})).await.unwrap(), json!({"a": 1}));
        assert_eq!(r.call("agent", "ping", Value::Null).await.unwrap(), json!({"pong": true}));
        let m = r.call("agent", "modules", Value::Null).await.unwrap();
        assert_eq!(m["modules"][0]["name"], "echo");
        assert_eq!(r.call("nope", "x", Value::Null).await.unwrap_err().code, controldeck_proto::ErrorCode::NotFound);
        assert_eq!(r.call("echo", "nope", Value::Null).await.unwrap_err().code, controldeck_proto::ErrorCode::NotFound);
        // Default `stream` has no methods.
        assert!(r.stream("echo", "say", Value::Null).await.is_err());
    }

    #[test]
    #[should_panic(expected = "registered twice")]
    fn duplicate_names_panic() {
        let mut r = Registry::new();
        r.register(Echo).register(Echo);
    }

    #[test]
    fn module_names_are_restricted() {
        for ok in ["packages", "a", "svc-mgr", "x_1"] {
            assert!(valid_module_name(ok), "{ok}");
        }
        for bad in ["", "Packages", "1a", "a b", "a/b", "../x", &"a".repeat(33)] {
            assert!(!valid_module_name(bad), "{bad}");
        }
    }

    #[test]
    fn params_accepts_null_as_empty_object() {
        #[derive(serde::Deserialize, Default)]
        struct P {
            #[serde(default)]
            x: bool,
        }
        assert!(!params::<P>(Value::Null).unwrap().x);
        assert!(params::<P>(json!({"x": true})).unwrap().x);
        assert!(params::<P>(json!([1])).is_err());
    }
}
