use crate::{serve, serve_streams, ServerIdentity, ToolReply, ToolServer};
use blazingly_json::{from_str, Map, RawJson, Value};
use serde::de::DeserializeOwned;
use std::collections::HashMap;
use std::io::{self, BufRead, Write};

type ValueHandler<S> = Box<dyn FnMut(&mut S, Value) -> ToolReply>;
type RawHandler<S> = Box<dyn FnMut(&mut S, &str) -> ToolReply>;

enum Handler<S> {
    Value(ValueHandler<S>),
    Raw(RawHandler<S>),
}

/// Builder-style MCP server for applications that do not need a custom
/// [`ToolServer`] implementation.
pub struct McpServer<S = ()> {
    identity: ServerIdentity,
    state: S,
    catalog: Value,
    tools: HashMap<String, Handler<S>>,
}

impl McpServer<()> {
    /// Creates a stateless server.
    #[must_use]
    pub fn new(name: impl Into<String>, version: impl Into<String>) -> Self {
        Self::from_identity(ServerIdentity::new(name, version, ""))
    }

    /// Creates a stateless server from a complete identity.
    #[must_use]
    pub fn from_identity(identity: ServerIdentity) -> Self {
        Self::with_identity_and_state(identity, ())
    }

    /// Registers a stateless tool that receives an owned JSON value.
    #[must_use]
    pub fn tool(
        self,
        name: impl Into<String>,
        description: impl Into<String>,
        input_schema: Value,
        mut handler: impl FnMut(Value) -> ToolReply + 'static,
    ) -> Self {
        self.tool_with_state(name, description, input_schema, move |(), arguments| {
            handler(arguments)
        })
    }

    /// Registers a stateless tool that receives validated raw JSON.
    #[must_use]
    pub fn raw_tool(
        self,
        name: impl Into<String>,
        description: impl Into<String>,
        input_schema: Value,
        mut handler: impl FnMut(&str) -> ToolReply + 'static,
    ) -> Self {
        self.raw_tool_with_state(name, description, input_schema, move |(), arguments| {
            handler(arguments)
        })
    }

    /// Registers a stateless typed tool without constructing an intermediate
    /// JSON DOM for its arguments.
    #[must_use]
    pub fn typed_tool<I>(
        self,
        name: impl Into<String>,
        description: impl Into<String>,
        input_schema: Value,
        mut handler: impl FnMut(I) -> ToolReply + 'static,
    ) -> Self
    where
        I: DeserializeOwned + 'static,
    {
        self.typed_tool_with_state(name, description, input_schema, move |(), arguments| {
            handler(arguments)
        })
    }
}

impl<S> McpServer<S> {
    /// Creates a server whose tool handlers share mutable state.
    #[must_use]
    pub fn with_state(name: impl Into<String>, version: impl Into<String>, state: S) -> Self {
        Self::with_identity_and_state(ServerIdentity::new(name, version, ""), state)
    }

    /// Creates a stateful server from a complete identity.
    #[must_use]
    pub fn with_identity_and_state(identity: ServerIdentity, state: S) -> Self {
        Self {
            identity,
            state,
            catalog: Value::Array(Vec::new()),
            tools: HashMap::new(),
        }
    }

    /// Sets the instructions reported during initialization.
    #[must_use]
    pub fn instructions(mut self, instructions: impl Into<String>) -> Self {
        self.identity.instructions = instructions.into();
        self
    }

    /// Registers a tool with access to shared state and owned JSON arguments.
    #[must_use]
    pub fn tool_with_state(
        mut self,
        name: impl Into<String>,
        description: impl Into<String>,
        input_schema: Value,
        handler: impl FnMut(&mut S, Value) -> ToolReply + 'static,
    ) -> Self {
        let name = name.into();
        self.register_descriptor(&name, description.into(), input_schema);
        self.tools.insert(name, Handler::Value(Box::new(handler)));
        self
    }

    /// Registers a tool with access to shared state and validated raw JSON.
    #[must_use]
    pub fn raw_tool_with_state(
        mut self,
        name: impl Into<String>,
        description: impl Into<String>,
        input_schema: Value,
        handler: impl FnMut(&mut S, &str) -> ToolReply + 'static,
    ) -> Self {
        let name = name.into();
        self.register_descriptor(&name, description.into(), input_schema);
        self.tools.insert(name, Handler::Raw(Box::new(handler)));
        self
    }

    /// Registers a typed tool with shared state and no intermediate JSON DOM.
    #[must_use]
    pub fn typed_tool_with_state<I>(
        self,
        name: impl Into<String>,
        description: impl Into<String>,
        input_schema: Value,
        mut handler: impl FnMut(&mut S, I) -> ToolReply + 'static,
    ) -> Self
    where
        I: DeserializeOwned + 'static,
    {
        let name = name.into();
        let error_name = name.clone();
        self.raw_tool_with_state(name, description, input_schema, move |state, arguments| {
            match from_str::<I>(arguments) {
                Ok(arguments) => handler(state, arguments),
                Err(error) => {
                    ToolReply::error(format!("invalid arguments for {error_name}: {error}"))
                }
            }
        })
    }

    /// Returns shared state.
    #[must_use]
    pub const fn state(&self) -> &S {
        &self.state
    }

    /// Returns mutable shared state.
    #[must_use]
    pub fn state_mut(&mut self) -> &mut S {
        &mut self.state
    }

    /// Runs the server over process stdin/stdout until EOF.
    ///
    /// # Errors
    ///
    /// Returns only stdio failures.
    pub fn serve(&mut self) -> io::Result<()> {
        serve(self)
    }

    /// Runs the server over injectable streams until EOF.
    ///
    /// # Errors
    ///
    /// Returns only stream failures.
    pub fn serve_streams(&mut self, reader: impl BufRead, writer: impl Write) -> io::Result<()> {
        serve_streams(self, reader, writer)
    }

    fn register_descriptor(&mut self, name: &str, description: String, input_schema: Value) {
        let mut descriptor = Map::new();
        descriptor.insert("name".to_owned(), Value::String(name.to_owned()));
        descriptor.insert("description".to_owned(), Value::String(description));
        descriptor.insert("inputSchema".to_owned(), input_schema);
        let descriptor = Value::Object(descriptor);

        let Value::Array(catalog) = &mut self.catalog else {
            unreachable!("builder catalog is always an array");
        };
        if let Some(existing) = catalog
            .iter_mut()
            .find(|tool| tool.get("name").and_then(Value::as_str) == Some(name))
        {
            *existing = descriptor;
        } else {
            catalog.push(descriptor);
        }
    }

    fn call_handler(&mut self, name: &str, arguments: Value) -> ToolReply {
        let Some(handler) = self.tools.get_mut(name) else {
            return ToolReply::error(format!("unknown tool: {name}"));
        };
        match handler {
            Handler::Value(handler) => handler(&mut self.state, arguments),
            Handler::Raw(handler) => match blazingly_json::to_string(&arguments) {
                Ok(arguments) => handler(&mut self.state, &arguments),
                Err(error) => ToolReply::error(format!("invalid arguments for {name}: {error}")),
            },
        }
    }

    fn call_raw_handler(&mut self, name: &str, arguments: RawJson<'_>) -> ToolReply {
        let Some(handler) = self.tools.get_mut(name) else {
            return ToolReply::error(format!("unknown tool: {name}"));
        };
        match handler {
            Handler::Value(handler) => match arguments.deserialize::<Value>() {
                Ok(arguments) => handler(&mut self.state, arguments),
                Err(error) => ToolReply::error(format!("invalid arguments for {name}: {error}")),
            },
            Handler::Raw(handler) => handler(&mut self.state, arguments.get()),
        }
    }
}

impl<S> ToolServer for McpServer<S> {
    fn identity(&self) -> ServerIdentity {
        self.identity.clone()
    }

    fn identity_ref(&self) -> Option<&ServerIdentity> {
        Some(&self.identity)
    }

    fn catalog(&mut self) -> Value {
        self.catalog.clone()
    }

    fn catalog_ref(&mut self) -> Option<&Value> {
        Some(&self.catalog)
    }

    fn has_tool(&self, name: &str) -> Option<bool> {
        Some(self.tools.contains_key(name))
    }

    fn call(&mut self, name: &str, arguments: Value) -> ToolReply {
        self.call_handler(name, arguments)
    }

    fn call_raw(&mut self, name: &str, arguments: RawJson<'_>) -> ToolReply {
        self.call_raw_handler(name, arguments)
    }
}

#[cfg(test)]
mod tests {
    use super::McpServer;
    use crate::{json, serve_message, ToolReply, ToolServer, Value};
    use serde::Deserialize;

    #[derive(Deserialize)]
    struct Increment {
        amount: u64,
    }

    #[test]
    fn builder_registers_owned_raw_typed_and_stateful_tools() {
        let mut stateless = McpServer::new("demo", "1.0.0")
            .instructions("Demo.")
            .tool(
                "echo",
                "Echo.",
                json!({"type": "object"}),
                ToolReply::structured,
            )
            .raw_tool("raw", "Raw.", json!({"type": "object"}), |raw| {
                ToolReply::text(json!({"length": raw.len()}))
            });
        assert_eq!(stateless.catalog().as_array().map(Vec::len), Some(2));
        assert!(matches!(
            stateless.call("echo", json!({"ok": true})),
            ToolReply::Serialized { .. }
        ));

        let mut stateful = McpServer::with_state("count", "1.0.0", 0_u64).typed_tool_with_state(
            "increment",
            "Increment.",
            json!({
                "type": "object",
                "properties": {"amount": {"type": "integer"}},
                "required": ["amount"]
            }),
            |count, arguments: Increment| {
                *count += arguments.amount;
                ToolReply::structured(json!({"count": *count}))
            },
        );
        let reply = stateful.call("increment", json!({"amount": 3}));
        let ToolReply::Serialized { value, .. } = reply else {
            panic!("increment must succeed");
        };
        let value = blazingly_json::from_str::<Value>(value.get()).unwrap();
        assert_eq!(value["count"], 3);
        assert_eq!(*stateful.state(), 3);

        let mut output = Vec::new();
        serve_message(
            &mut stateful,
            r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"increment","arguments":{"amount":4}}}"#,
            &mut output,
        )
        .unwrap();
        let response = blazingly_json::from_slice::<Value>(&output).unwrap();
        assert_eq!(response["result"]["structuredContent"]["count"], 7);
        assert_eq!(*stateful.state(), 7);

        output.clear();
        serve_message(
            &mut stateful,
            r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"increment","arguments":{"amount":"wrong"}}}"#,
            &mut output,
        )
        .unwrap();
        let response = blazingly_json::from_slice::<Value>(&output).unwrap();
        assert_eq!(response["result"]["isError"], true);
    }

    #[test]
    fn duplicate_registration_replaces_descriptor_and_handler() {
        let mut server = McpServer::new("demo", "1.0.0")
            .tool("echo", "Old.", json!({"type": "null"}), |_| {
                ToolReply::text(json!("old"))
            })
            .tool("echo", "New.", json!({"type": "object"}), |_| {
                ToolReply::text(json!("new"))
            });

        let catalog = server.catalog();
        assert_eq!(catalog.as_array().map(Vec::len), Some(1));
        assert_eq!(catalog[0]["description"], "New.");
        let ToolReply::Serialized { value, .. } = server.call("echo", json!({})) else {
            panic!("echo must succeed");
        };
        assert_eq!(value.get(), r#""new""#);
    }

    #[test]
    fn unknown_tools_are_protocol_errors_but_handler_failures_are_tool_results() {
        let mut server = McpServer::new("demo", "1.0.0").tool(
            "fails",
            "Fails.",
            json!({"type": "object"}),
            |_| ToolReply::error("handler failed"),
        );

        let mut output = Vec::new();
        serve_message(
            &mut server,
            r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"missing"}}"#,
            &mut output,
        )
        .unwrap();
        let unknown = blazingly_json::from_slice::<Value>(&output).unwrap();
        assert_eq!(unknown["error"]["code"], -32_602);
        assert!(unknown.get("result").is_none());

        output.clear();
        serve_message(
            &mut server,
            r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"fails"}}"#,
            &mut output,
        )
        .unwrap();
        let failed = blazingly_json::from_slice::<Value>(&output).unwrap();
        assert_eq!(failed["result"]["isError"], true);
        assert!(failed.get("error").is_none());
    }
}
