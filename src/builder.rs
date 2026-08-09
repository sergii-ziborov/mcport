use crate::{
    schema, serve, serve_streams, serve_streams_with_config, serve_streams_with_limits,
    serve_with_config, serve_with_limits, SchemaDefect, ServerIdentity, ToolReply, ToolServer,
    TransportConfig, TransportLimits,
};
use blazingly_json::{from_str, Map, RawJson, RawValue, Value};
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
    catalog_raw: Option<Box<RawValue>>,
    tool_page_size: Option<usize>,
    tools: HashMap<String, Handler<S>>,
    schema_defects: Vec<SchemaDefect>,
    strict_schemas: bool,
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
            catalog_raw: None,
            tool_page_size: None,
            tools: HashMap::new(),
            schema_defects: Vec::new(),
            strict_schemas: false,
        }
    }

    /// Sets the instructions reported during initialization.
    #[must_use]
    pub fn instructions(mut self, instructions: impl Into<String>) -> Self {
        self.identity.instructions = instructions.into();
        self
    }

    /// Enables cursor pagination for `tools/list`.
    #[must_use]
    pub fn tool_page_size(mut self, page_size: usize) -> Self {
        self.tool_page_size = Some(page_size.max(1));
        self
    }

    /// Refuses to serve a catalog whose advertised schemas do not describe
    /// what their tools accept.
    ///
    /// Registration stays infallible, so this cannot break an existing
    /// builder chain. The `serve*` methods return
    /// [`io::ErrorKind::InvalidInput`] listing every defect, which fails at
    /// startup rather than letting a client discover the gaps one rejected
    /// call at a time. See [`crate::validate_tool_schema`] for the rules.
    #[must_use]
    pub fn strict_schemas(mut self) -> Self {
        self.strict_schemas = true;
        self
    }

    /// Advertised schemas that do not describe what their tool accepts.
    ///
    /// Always populated, so a server can assert on it in its own tests
    /// without opting into [`McpServer::strict_schemas`].
    #[must_use]
    pub fn schema_defects(&self) -> &[SchemaDefect] {
        &self.schema_defects
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

    /// Runs the server with explicit request and response byte budgets.
    ///
    /// # Errors
    ///
    /// Returns stdio failures or an invalid limits configuration.
    pub fn serve_with_limits(&mut self, limits: TransportLimits) -> io::Result<()> {
        serve_with_limits(self, limits)
    }

    /// Runs the server with complete byte and flush policy.
    ///
    /// # Errors
    ///
    /// Returns transport or configuration failures.
    pub fn serve_with_config(&mut self, config: TransportConfig) -> io::Result<()> {
        serve_with_config(self, config)
    }

    /// Runs the server over injectable streams until EOF.
    ///
    /// # Errors
    ///
    /// Returns only stream failures.
    pub fn serve_streams(&mut self, reader: impl BufRead, writer: impl Write) -> io::Result<()> {
        serve_streams(self, reader, writer)
    }

    /// Runs the server over injectable streams with explicit byte budgets.
    ///
    /// # Errors
    ///
    /// Returns stream failures or an invalid limits configuration.
    pub fn serve_streams_with_limits(
        &mut self,
        reader: impl BufRead,
        writer: impl Write,
        limits: TransportLimits,
    ) -> io::Result<()> {
        serve_streams_with_limits(self, reader, writer, limits)
    }

    /// Runs the server over injectable streams with complete transport policy.
    ///
    /// # Errors
    ///
    /// Returns transport or configuration failures.
    pub fn serve_streams_with_config(
        &mut self,
        reader: impl BufRead,
        writer: impl Write,
        config: TransportConfig,
    ) -> io::Result<()> {
        serve_streams_with_config(self, reader, writer, config)
    }

    fn register_descriptor(&mut self, name: &str, description: String, input_schema: Value) {
        // A re-registration replaces the descriptor, so its defects go with it.
        self.schema_defects
            .retain(|defect| defect.tool.as_deref() != Some(name));
        self.schema_defects
            .extend(schema::defects_in(Some(name), &input_schema));

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
        self.catalog_raw = blazingly_json::to_raw_value(&self.catalog).ok();
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

    fn catalog_raw_ref(&mut self) -> Option<&RawValue> {
        self.catalog_raw.as_deref()
    }

    fn catalog_is_paginated(&self) -> bool {
        self.tool_page_size.is_some()
    }

    fn catalog_page(&mut self, cursor: Option<&str>) -> Result<crate::ToolPage, String> {
        paginate_catalog(&self.catalog, self.tool_page_size, cursor)
    }

    fn has_tool(&self, name: &str) -> Option<bool> {
        Some(self.tools.contains_key(name))
    }

    fn strict_schema_defects(&self) -> &[SchemaDefect] {
        if self.strict_schemas {
            &self.schema_defects
        } else {
            &[]
        }
    }

    fn call(&mut self, name: &str, arguments: Value) -> ToolReply {
        self.call_handler(name, arguments)
    }

    fn call_raw(&mut self, name: &str, arguments: RawJson<'_>) -> ToolReply {
        self.call_raw_handler(name, arguments)
    }
}

pub(crate) fn paginate_catalog(
    catalog: &Value,
    page_size: Option<usize>,
    cursor: Option<&str>,
) -> Result<crate::ToolPage, String> {
    let Value::Array(tools) = catalog else {
        return Err("tool catalog must be an array".to_owned());
    };
    let Some(page_size) = page_size else {
        if cursor.is_some() {
            return Err("invalid tools/list cursor".to_owned());
        }
        return Ok(crate::ToolPage::complete(catalog.clone()));
    };
    let offset = match cursor {
        None => 0,
        Some(cursor) => cursor
            .strip_prefix("mcport:")
            .and_then(|offset| offset.parse::<usize>().ok())
            .filter(|offset| *offset < tools.len())
            .ok_or_else(|| "invalid tools/list cursor".to_owned())?,
    };
    let end = offset.saturating_add(page_size).min(tools.len());
    let next_cursor = (end < tools.len()).then(|| format!("mcport:{end}"));
    Ok(crate::ToolPage {
        tools: Value::Array(tools[offset..end].to_vec()),
        next_cursor,
    })
}

#[cfg(test)]
mod tests {
    use super::McpServer;
    use crate::{json, serve_message, SchemaDefectKind, ToolReply, ToolServer, Value};
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
        let mut output = Vec::new();
        serve_message(
            &mut server,
            r#"{"jsonrpc":"2.0","id":7,"method":"tools/list"}"#,
            &mut output,
        )
        .unwrap();
        let listed = blazingly_json::from_slice::<Value>(&output).unwrap();
        assert_eq!(listed["result"]["tools"][0]["description"], "New.");
        let ToolReply::Serialized { value, .. } = server.call("echo", json!({})) else {
            panic!("echo must succeed");
        };
        assert_eq!(value.get(), r#""new""#);
    }

    #[test]
    fn builder_paginates_tools_with_opaque_cursors() {
        let mut server = McpServer::new("test", "1")
            .tool_page_size(1)
            .tool("first", "First.", json!({"type": "object"}), |_| {
                ToolReply::text("first")
            })
            .tool("second", "Second.", json!({"type": "object"}), |_| {
                ToolReply::text("second")
            });
        let mut first = Vec::new();
        serve_message(
            &mut server,
            r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#,
            &mut first,
        )
        .unwrap();
        let first = blazingly_json::from_slice::<Value>(&first).unwrap();
        assert_eq!(first["result"]["tools"].as_array().map(Vec::len), Some(1));
        assert_eq!(first["result"]["nextCursor"], "mcport:1");

        let mut second = Vec::new();
        serve_message(
            &mut server,
            r#"{"jsonrpc":"2.0","id":2,"method":"tools/list","params":{"cursor":"mcport:1"}}"#,
            &mut second,
        )
        .unwrap();
        let second = blazingly_json::from_slice::<Value>(&second).unwrap();
        assert_eq!(second["result"]["tools"].as_array().map(Vec::len), Some(1));
        assert!(second["result"].get("nextCursor").is_none());

        let mut invalid = Vec::new();
        serve_message(
            &mut server,
            r#"{"jsonrpc":"2.0","id":3,"method":"tools/list","params":{"cursor":"bad"}}"#,
            &mut invalid,
        )
        .unwrap();
        let invalid = blazingly_json::from_slice::<Value>(&invalid).unwrap();
        assert_eq!(invalid["error"]["code"], -32_602);
    }

    #[test]
    fn strict_schemas_refuse_to_serve_an_undescribed_catalog() {
        let mut dishonest = McpServer::new("planner", "1.0.0")
            .strict_schemas()
            .typed_tool(
                "plan",
                "Plans.",
                json!({
                    "type": "object",
                    "properties": {"budget": {"type": "object"}}
                }),
                |_: Value| ToolReply::text("planned"),
            );
        assert_eq!(dishonest.schema_defects().len(), 1);
        assert_eq!(dishonest.schema_defects()[0].tool.as_deref(), Some("plan"));
        assert_eq!(
            dishonest.schema_defects()[0].kind,
            SchemaDefectKind::UndescribedObject
        );

        let error = dishonest
            .serve_streams(&b""[..], Vec::new())
            .expect_err("strict schemas must refuse the catalog");
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
        assert!(error.to_string().contains("/properties/budget"));

        // Re-registering the tool honestly clears the defect it introduced.
        let mut honest = dishonest.typed_tool(
            "plan",
            "Plans.",
            json!({
                "type": "object",
                "properties": {
                    "budget": {
                        "type": "object",
                        "properties": {"ceiling_cents": {"type": "integer"}}
                    }
                }
            }),
            |_: Value| ToolReply::text("planned"),
        );
        assert!(honest.schema_defects().is_empty());
        assert!(honest.serve_streams(&b""[..], Vec::new()).is_ok());
    }

    #[test]
    fn schema_defects_are_reported_without_opting_into_strict_schemas() {
        let mut lenient = McpServer::new("planner", "1.0.0").tool(
            "plan",
            "Plans.",
            json!({"type": "object", "properties": {"tags": {"type": "array"}}}),
            ToolReply::structured,
        );
        assert_eq!(
            lenient.schema_defects()[0].kind,
            SchemaDefectKind::UndescribedArray
        );
        assert!(
            lenient.serve_streams(&b""[..], Vec::new()).is_ok(),
            "existing servers keep serving until they opt in"
        );
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
