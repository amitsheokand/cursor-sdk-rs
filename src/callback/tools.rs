//! Custom tools: Rust functions an agent can call.
//!
//! The declaration and the execution travel separately. Metadata (name,
//! description, JSON Schema) goes out with `CreateAgent` in
//! `LocalAgentOptions.custom_tools`; execution comes back to this process over
//! `CallCustomTool`. [`ToolRegistry`] holds both halves so callers only ever
//! "register a function with a schema".
//!
//! Custom tools are a local-agent feature.

use std::collections::BTreeMap;
use std::sync::{Arc, RwLock};

use serde_json::Value as JsonValue;

use super::{BoxFuture, HandlerResult};
use crate::options::CustomTool;

/// One invocation of a custom tool.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct ToolCall {
    /// The tool the model asked for.
    pub name: String,
    /// The arguments, matching the tool's input schema.
    pub args: JsonValue,
    /// Correlates this call with the `tool_call` events on the run stream.
    pub tool_call_id: Option<String>,
    /// The agent that owns the tool definition.
    pub agent_id: String,
}

impl ToolCall {
    /// A named argument, or `None` when absent.
    pub fn arg(&self, name: &str) -> Option<&JsonValue> {
        self.args.get(name)
    }

    /// A named string argument.
    pub fn string_arg(&self, name: &str) -> Option<&str> {
        self.args.get(name)?.as_str()
    }

    /// A required named argument, as an error when it is missing.
    pub fn require(&self, name: &str) -> HandlerResult<&JsonValue> {
        self.args.get(name).ok_or_else(|| {
            format!(
                "tool {:?} was called without the required argument {name:?}",
                self.name
            )
            .into()
        })
    }
}

type Handler = Arc<dyn Fn(ToolCall) -> BoxFuture<'static, HandlerResult<JsonValue>> + Send + Sync>;

struct Registered {
    definition: CustomTool,
    handler: Handler,
}

/// The custom tools this process offers, and the code behind them.
///
/// Cloned handles share one set of tools, so registering on a
/// [`Client`](crate::Client) after the bridge is running still works.
#[derive(Clone, Default)]
pub struct ToolRegistry {
    tools: Arc<RwLock<BTreeMap<String, Registered>>>,
}

impl std::fmt::Debug for ToolRegistry {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ToolRegistry")
            .field("tools", &self.names())
            .finish()
    }
}

impl ToolRegistry {
    /// An empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a tool and the async function that runs it.
    ///
    /// The handler returns a [`serde_json::Value`]. Scalars are wrapped as
    /// `{"value": …}` on the way out, because a tool result is a
    /// `google.protobuf.Struct` and can only encode an object.
    ///
    /// ```
    /// use cursor_sdk::{CustomTool, ToolRegistry};
    /// use serde_json::json;
    ///
    /// let tools = ToolRegistry::new();
    /// tools.register(
    ///     CustomTool::new(
    ///         "word_count",
    ///         "Count the words in a string",
    ///         json!({
    ///             "type": "object",
    ///             "properties": {"text": {"type": "string"}},
    ///             "required": ["text"],
    ///         }),
    ///     ),
    ///     |call| async move {
    ///         let text = call.string_arg("text").unwrap_or_default();
    ///         Ok(json!({"words": text.split_whitespace().count()}))
    ///     },
    /// );
    /// assert_eq!(tools.names(), vec!["word_count".to_string()]);
    /// ```
    pub fn register<F, Fut>(&self, definition: CustomTool, handler: F)
    where
        F: Fn(ToolCall) -> Fut + Send + Sync + 'static,
        Fut: std::future::Future<Output = HandlerResult<JsonValue>> + Send + 'static,
    {
        let handler: Handler = Arc::new(move |call| Box::pin(handler(call)));
        let name = definition.name.clone();
        if let Ok(mut tools) = self.tools.write() {
            tools.insert(
                name,
                Registered {
                    definition,
                    handler,
                },
            );
        }
    }

    /// Remove a tool. Returns whether one was registered under that name.
    pub fn unregister(&self, name: &str) -> bool {
        self.tools
            .write()
            .map(|mut tools| tools.remove(name).is_some())
            .unwrap_or(false)
    }

    /// Whether any tool is registered.
    pub fn is_empty(&self) -> bool {
        self.tools
            .read()
            .map(|tools| tools.is_empty())
            .unwrap_or(true)
    }

    /// The registered tool names, sorted.
    pub fn names(&self) -> Vec<String> {
        self.tools
            .read()
            .map(|tools| tools.keys().cloned().collect())
            .unwrap_or_default()
    }

    /// The declarations to send with `CreateAgent`.
    pub(crate) fn definitions(&self) -> Vec<CustomTool> {
        self.tools
            .read()
            .map(|tools| {
                tools
                    .values()
                    .map(|entry| entry.definition.clone())
                    .collect()
            })
            .unwrap_or_default()
    }

    pub(crate) fn handler(&self, name: &str) -> Option<Handler> {
        self.tools
            .read()
            .ok()?
            .get(name)
            .map(|entry| entry.handler.clone())
    }

    /// Execute a tool call. Used by the callback server.
    pub(crate) async fn call(&self, call: ToolCall) -> HandlerResult<JsonValue> {
        let Some(handler) = self.handler(&call.name) else {
            let known = self.names().join(", ");
            return Err(format!(
                "no custom tool named {:?} is registered with this SDK client (registered: [{known}])",
                call.name
            )
            .into());
        };
        handler(call).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn schema() -> JsonValue {
        json!({"type": "object"})
    }

    fn call(name: &str, args: JsonValue) -> ToolCall {
        ToolCall {
            name: name.to_string(),
            args,
            tool_call_id: Some("call_1".into()),
            agent_id: "agent_1".into(),
        }
    }

    #[tokio::test]
    async fn runs_a_registered_tool() {
        let registry = ToolRegistry::new();
        registry.register(CustomTool::new("echo", "echo", schema()), |call| async move {
            Ok(json!({"echoed": call.string_arg("text").unwrap_or_default()}))
        });

        let result = registry
            .call(call("echo", json!({"text": "hello"})))
            .await
            .unwrap();
        assert_eq!(result, json!({"echoed": "hello"}));
    }

    #[tokio::test]
    async fn an_unknown_tool_names_what_is_registered() {
        let registry = ToolRegistry::new();
        registry.register(CustomTool::new("known", "d", schema()), |_| async {
            Ok(json!({}))
        });
        let error = registry.call(call("missing", json!({}))).await.unwrap_err();
        assert!(error.to_string().contains("missing"));
        assert!(error.to_string().contains("known"));
    }

    #[tokio::test]
    async fn a_handler_error_propagates() {
        let registry = ToolRegistry::new();
        registry.register(CustomTool::new("fails", "d", schema()), |_| async {
            Err("the upstream service is down".into())
        });
        let error = registry.call(call("fails", json!({}))).await.unwrap_err();
        assert_eq!(error.to_string(), "the upstream service is down");
    }

    #[tokio::test]
    async fn required_arguments_are_checked() {
        let registry = ToolRegistry::new();
        registry.register(CustomTool::new("needs", "d", schema()), |call| async move {
            let value = call.require("id")?;
            Ok(json!({"id": value}))
        });
        let error = registry.call(call("needs", json!({}))).await.unwrap_err();
        assert!(error.to_string().contains("required argument \"id\""));
    }

    #[test]
    fn definitions_travel_with_agent_options() {
        let registry = ToolRegistry::new();
        assert!(registry.is_empty());
        registry.register(CustomTool::new("a", "d", schema()), |_| async {
            Ok(json!({}))
        });
        assert!(!registry.is_empty());
        assert_eq!(registry.definitions().len(), 1);
        assert!(registry.unregister("a"));
        assert!(!registry.unregister("a"));
    }
}
