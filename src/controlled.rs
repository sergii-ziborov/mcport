use crate::transport::{read_frame, FrameStatus, ResponseBuffer};
use crate::{
    schema, FlushPolicy, Map, MethodReply, RawJson, RawValue, SchemaDefect, ServerIdentity,
    ToolPage, ToolReply, ToolServer, TransportLimits, Value,
};
use serde::de::DeserializeOwned;
use std::collections::HashMap;
use std::io::{self, BufRead, Write};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, SyncSender, TrySendError};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

const INTERNAL_ERROR: i64 = -32_603;
const SERVER_BUSY: i64 = -32_000;
const POLL_INTERVAL: Duration = Duration::from_millis(5);

/// Scheduling, execution, and transport policy for the controlled runtime.
#[derive(Clone, Copy, Debug)]
pub struct RuntimeConfig {
    /// Per-message input and output byte budgets.
    pub transport: TransportLimits,
    /// Maximum number of handler threads that may remain active.
    pub max_in_flight: usize,
    /// Maximum requests waiting for an execution slot.
    pub queue_depth: usize,
    /// Maximum complete messages waiting for the writer.
    pub output_queue_depth: usize,
    /// When the dedicated writer flushes complete response frames.
    pub output_flush_policy: FlushPolicy,
    /// Maximum wall-clock time allowed for one handler result.
    pub handler_deadline: Option<Duration>,
}

impl RuntimeConfig {
    fn validate(&self) -> io::Result<()> {
        self.transport.validate()?;
        for (name, value) in [
            ("max_in_flight", self.max_in_flight),
            ("queue_depth", self.queue_depth),
            ("output_queue_depth", self.output_queue_depth),
        ] {
            if value == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("{name} must be greater than zero"),
                ));
            }
        }
        if self.handler_deadline == Some(Duration::ZERO) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "handler_deadline must be greater than zero",
            ));
        }
        if matches!(
            self.output_flush_policy,
            FlushPolicy::Batch { max_messages: 0 }
        ) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "batch max_messages must be greater than zero",
            ));
        }
        Ok(())
    }
}

impl Default for RuntimeConfig {
    fn default() -> Self {
        Self {
            transport: TransportLimits::default(),
            max_in_flight: 4,
            queue_depth: 32,
            output_queue_depth: 32,
            output_flush_policy: FlushPolicy::PerMessage,
            handler_deadline: Some(Duration::from_secs(30)),
        }
    }
}

#[derive(Debug)]
struct CancellationState {
    cancelled: AtomicBool,
}

/// Cloneable cooperative cancellation signal supplied to tool handlers.
#[derive(Clone, Debug)]
pub struct CancellationToken {
    state: Arc<CancellationState>,
}

impl CancellationToken {
    fn new() -> Self {
        Self {
            state: Arc::new(CancellationState {
                cancelled: AtomicBool::new(false),
            }),
        }
    }

    fn cancel(&self) {
        self.state.cancelled.store(true, Ordering::Release);
    }

    /// Returns whether the client or runtime cancelled the operation.
    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        self.state.cancelled.load(Ordering::Acquire)
    }
}

/// Context supplied to every handler running under [`serve_controlled`].
#[derive(Clone)]
pub struct RequestContext {
    cancellation: CancellationToken,
    deadline: Option<Instant>,
    progress: Option<ProgressReporter>,
}

impl RequestContext {
    /// Returns the cooperative cancellation token.
    #[must_use]
    pub const fn cancellation_token(&self) -> &CancellationToken {
        &self.cancellation
    }

    /// Returns the absolute handler deadline, when configured.
    #[must_use]
    pub const fn deadline(&self) -> Option<Instant> {
        self.deadline
    }

    /// Returns the remaining time before the deadline.
    #[must_use]
    pub fn remaining(&self) -> Option<Duration> {
        self.deadline
            .map(|deadline| deadline.saturating_duration_since(Instant::now()))
    }

    /// Returns whether cancellation was requested or the deadline elapsed.
    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        self.cancellation.is_cancelled()
            || self
                .deadline
                .is_some_and(|deadline| Instant::now() >= deadline)
    }

    /// Emits one bounded `notifications/progress` message.
    ///
    /// Returns `false` when the request did not contain a progress token.
    pub fn report_progress(
        &self,
        progress: f64,
        total: Option<f64>,
        message: Option<&str>,
    ) -> io::Result<bool> {
        if self.is_cancelled() {
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "request was cancelled",
            ));
        }
        let Some(reporter) = &self.progress else {
            return Ok(false);
        };
        reporter.report(progress, total, message)?;
        Ok(true)
    }
}

#[derive(Clone)]
struct ProgressReporter {
    output: SyncSender<OutputMessage>,
    token: String,
    max_response_bytes: usize,
    last_progress: Arc<Mutex<Option<f64>>>,
    cancellation: CancellationToken,
}

impl ProgressReporter {
    fn report(&self, progress: f64, total: Option<f64>, message: Option<&str>) -> io::Result<()> {
        if !progress.is_finite() || total.is_some_and(|total| !total.is_finite()) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "progress values must be finite",
            ));
        }
        let mut last = self
            .last_progress
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if last.is_some_and(|last| progress <= last) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "progress must increase",
            ));
        }
        if self.cancellation.is_cancelled() {
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "request was cancelled",
            ));
        }

        let mut response = ResponseBuffer::new(self.max_response_bytes);
        response.write_all(
            br#"{"jsonrpc":"2.0","method":"notifications/progress","params":{"progressToken":"#,
        )?;
        response.write_all(self.token.as_bytes())?;
        response.write_all(br#","progress":"#)?;
        blazingly_json::to_writer(&mut response, &progress)?;
        if let Some(total) = total {
            response.write_all(br#","total":"#)?;
            blazingly_json::to_writer(&mut response, &total)?;
        }
        if let Some(message) = message {
            response.write_all(br#","message":"#)?;
            blazingly_json::to_writer(&mut response, &message)?;
        }
        response.write_all(b"}}\n")?;
        self.output
            .send(OutputMessage::Frame(response.into_bytes()))
            .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "output channel closed"))?;
        *last = Some(progress);
        Ok(())
    }
}

/// Thread-safe tool surface for the controlled runtime.
pub trait ConcurrentToolServer: Send + Sync + 'static {
    /// Identity reported by protocol discovery and legacy initialization.
    fn identity(&self) -> ServerIdentity;

    /// Borrows a stored identity when available.
    fn identity_ref(&self) -> Option<&ServerIdentity> {
        None
    }

    /// Server capabilities reported by initialization and discovery.
    fn capabilities(&self) -> Value {
        crate::json!({"tools": {"listChanged": false}})
    }

    /// Borrows stored server capabilities when available.
    fn capabilities_ref(&self) -> Option<&Value> {
        None
    }

    /// Returns the complete tool catalog.
    fn catalog(&self) -> Value;

    /// Borrows a stored catalog when available.
    fn catalog_ref(&self) -> Option<&Value> {
        None
    }

    /// Borrows a pre-serialized catalog when available.
    fn catalog_raw_ref(&self) -> Option<&RawValue> {
        None
    }

    /// Reports whether `tools/list` is cursor-paginated.
    fn catalog_is_paginated(&self) -> bool {
        false
    }

    /// Returns one cursor-addressed tool page.
    fn catalog_page(&self, cursor: Option<&str>) -> Result<ToolPage, String> {
        if cursor.is_some() {
            return Err("invalid tools/list cursor".to_owned());
        }
        Ok(ToolPage::complete(self.catalog()))
    }

    /// Reports whether a tool is registered.
    fn has_tool(&self, _name: &str) -> Option<bool> {
        None
    }

    /// Executes one tool call with runtime controls.
    fn call(&self, context: &RequestContext, name: &str, arguments: Value) -> ToolReply;

    /// Executes validated raw arguments without constructing a JSON DOM.
    fn call_raw(&self, context: &RequestContext, name: &str, arguments: RawJson<'_>) -> ToolReply {
        match arguments.deserialize::<Value>() {
            Ok(arguments) => self.call(context, name, arguments),
            Err(error) => ToolReply::error(format!("invalid arguments for {name}: {error}")),
        }
    }

    /// Handles a JSON-RPC method outside the built-in MCP tool surface.
    fn call_method(
        &self,
        _context: &RequestContext,
        _method: &str,
        _params: Value,
    ) -> Option<MethodReply> {
        None
    }

    /// Advertised schemas that do not describe what their tool accepts.
    ///
    /// [`serve_controlled_streams`] refuses to start when this is non-empty.
    /// Hand-written implementations inherit an empty slice; see
    /// [`crate::validate_tool_schema`] and
    /// [`ConcurrentMcpServer::strict_schemas`].
    fn strict_schema_defects(&self) -> &[SchemaDefect] {
        &[]
    }
}

type ValueHandler = Box<dyn Fn(&RequestContext, Value) -> ToolReply + Send + Sync + 'static>;
type RawHandler = Box<dyn Fn(&RequestContext, &str) -> ToolReply + Send + Sync + 'static>;

enum Handler {
    Value(ValueHandler),
    Raw(RawHandler),
}

/// Builder for thread-safe tools served by the controlled runtime.
pub struct ConcurrentMcpServer {
    identity: ServerIdentity,
    catalog: Value,
    catalog_raw: Option<Box<RawValue>>,
    tool_page_size: Option<usize>,
    tools: HashMap<String, Handler>,
    schema_defects: Vec<SchemaDefect>,
    strict_schemas: bool,
}

impl ConcurrentMcpServer {
    /// Creates an empty concurrent server.
    #[must_use]
    pub fn new(name: impl Into<String>, version: impl Into<String>) -> Self {
        Self {
            identity: ServerIdentity::new(name, version, ""),
            catalog: Value::Array(Vec::new()),
            catalog_raw: None,
            tool_page_size: None,
            tools: HashMap::new(),
            schema_defects: Vec::new(),
            strict_schemas: false,
        }
    }

    /// Sets server instructions.
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
    /// Registration stays infallible. [`ConcurrentMcpServer::serve`] returns
    /// [`io::ErrorKind::InvalidInput`] listing every defect before any worker
    /// starts. See [`crate::validate_tool_schema`] for the rules.
    #[must_use]
    pub fn strict_schemas(mut self) -> Self {
        self.strict_schemas = true;
        self
    }

    /// Advertised schemas that do not describe what their tool accepts.
    ///
    /// Always populated, so a server can assert on it in its own tests
    /// without opting into [`ConcurrentMcpServer::strict_schemas`].
    #[must_use]
    pub fn schema_defects(&self) -> &[SchemaDefect] {
        &self.schema_defects
    }

    /// Registers a context-aware owned-value handler.
    #[must_use]
    pub fn tool(
        mut self,
        name: impl Into<String>,
        description: impl Into<String>,
        input_schema: Value,
        handler: impl Fn(&RequestContext, Value) -> ToolReply + Send + Sync + 'static,
    ) -> Self {
        let name = name.into();
        self.register_descriptor(&name, description.into(), input_schema);
        self.tools.insert(name, Handler::Value(Box::new(handler)));
        self
    }

    /// Registers a context-aware raw JSON handler.
    #[must_use]
    pub fn raw_tool(
        mut self,
        name: impl Into<String>,
        description: impl Into<String>,
        input_schema: Value,
        handler: impl Fn(&RequestContext, &str) -> ToolReply + Send + Sync + 'static,
    ) -> Self {
        let name = name.into();
        self.register_descriptor(&name, description.into(), input_schema);
        self.tools.insert(name, Handler::Raw(Box::new(handler)));
        self
    }

    /// Registers a context-aware typed handler.
    #[must_use]
    pub fn typed_tool<I>(
        self,
        name: impl Into<String>,
        description: impl Into<String>,
        input_schema: Value,
        handler: impl Fn(&RequestContext, I) -> ToolReply + Send + Sync + 'static,
    ) -> Self
    where
        I: DeserializeOwned + 'static,
    {
        let name = name.into();
        let error_name = name.clone();
        self.raw_tool(
            name,
            description,
            input_schema,
            move |context, arguments| match blazingly_json::from_str::<I>(arguments) {
                Ok(arguments) => handler(context, arguments),
                Err(error) => {
                    ToolReply::error(format!("invalid arguments for {error_name}: {error}"))
                }
            },
        )
    }

    /// Runs the controlled server on process stdin/stdout.
    ///
    /// # Errors
    ///
    /// Returns configuration, I/O, or worker lifecycle failures.
    pub fn serve(self, config: RuntimeConfig) -> io::Result<()> {
        serve_controlled(Arc::new(self), config)
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
            unreachable!("concurrent builder catalog is always an array");
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
}

impl ConcurrentToolServer for ConcurrentMcpServer {
    fn identity(&self) -> ServerIdentity {
        self.identity.clone()
    }

    fn identity_ref(&self) -> Option<&ServerIdentity> {
        Some(&self.identity)
    }

    fn catalog(&self) -> Value {
        self.catalog.clone()
    }

    fn catalog_ref(&self) -> Option<&Value> {
        Some(&self.catalog)
    }

    fn catalog_raw_ref(&self) -> Option<&RawValue> {
        self.catalog_raw.as_deref()
    }

    fn catalog_is_paginated(&self) -> bool {
        self.tool_page_size.is_some()
    }

    fn catalog_page(&self, cursor: Option<&str>) -> Result<ToolPage, String> {
        crate::builder::paginate_catalog(&self.catalog, self.tool_page_size, cursor)
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

    fn call(&self, context: &RequestContext, name: &str, arguments: Value) -> ToolReply {
        let Some(handler) = self.tools.get(name) else {
            return ToolReply::error(format!("unknown tool: {name}"));
        };
        match handler {
            Handler::Value(handler) => handler(context, arguments),
            Handler::Raw(handler) => match blazingly_json::to_string(&arguments) {
                Ok(arguments) => handler(context, &arguments),
                Err(error) => ToolReply::error(format!("invalid arguments for {name}: {error}")),
            },
        }
    }

    fn call_raw(&self, context: &RequestContext, name: &str, arguments: RawJson<'_>) -> ToolReply {
        let Some(handler) = self.tools.get(name) else {
            return ToolReply::error(format!("unknown tool: {name}"));
        };
        match handler {
            Handler::Value(handler) => match arguments.deserialize::<Value>() {
                Ok(arguments) => handler(context, arguments),
                Err(error) => ToolReply::error(format!("invalid arguments for {name}: {error}")),
            },
            Handler::Raw(handler) => handler(context, arguments.get()),
        }
    }
}

struct Adapter<'a, S> {
    server: &'a S,
    context: &'a RequestContext,
}

impl<S: ConcurrentToolServer> ToolServer for Adapter<'_, S> {
    fn identity(&self) -> ServerIdentity {
        self.server.identity()
    }

    fn identity_ref(&self) -> Option<&ServerIdentity> {
        self.server.identity_ref()
    }

    fn capabilities(&self) -> Value {
        self.server.capabilities()
    }

    fn capabilities_ref(&self) -> Option<&Value> {
        self.server.capabilities_ref()
    }

    fn catalog(&mut self) -> Value {
        self.server.catalog()
    }

    fn catalog_ref(&mut self) -> Option<&Value> {
        self.server.catalog_ref()
    }

    fn catalog_raw_ref(&mut self) -> Option<&RawValue> {
        self.server.catalog_raw_ref()
    }

    fn catalog_is_paginated(&self) -> bool {
        self.server.catalog_is_paginated()
    }

    fn catalog_page(&mut self, cursor: Option<&str>) -> Result<ToolPage, String> {
        self.server.catalog_page(cursor)
    }

    fn has_tool(&self, name: &str) -> Option<bool> {
        self.server.has_tool(name)
    }

    fn call(&mut self, name: &str, arguments: Value) -> ToolReply {
        self.server.call(self.context, name, arguments)
    }

    fn call_raw(&mut self, name: &str, arguments: RawJson<'_>) -> ToolReply {
        self.server.call_raw(self.context, name, arguments)
    }

    fn call_method(&mut self, method: &str, params: Value) -> Option<MethodReply> {
        self.server.call_method(self.context, method, params)
    }
}

struct Job {
    line: String,
    id: Option<String>,
    cancellation: CancellationToken,
    progress_token: Option<String>,
}

enum HandlerOutcome {
    Response(Option<Vec<u8>>),
    Failed(String),
    Panicked,
}

enum OutputMessage {
    Frame(Vec<u8>),
    Shutdown,
}

/// Runs a thread-safe tool server with bounded scheduling and execution.
///
/// The existing blocking [`crate::serve`] path remains the minimum-overhead
/// single-request runtime. This function adds bounded queues, concurrent
/// handlers, cooperative cancellation, deadlines, panic isolation, progress,
/// and atomic response budgets without requiring an async executor.
///
/// # Errors
///
/// Returns configuration, stdio, or worker lifecycle failures.
pub fn serve_controlled<S: ConcurrentToolServer>(
    server: Arc<S>,
    config: RuntimeConfig,
) -> io::Result<()> {
    let stdin = io::stdin();
    match config.output_flush_policy {
        FlushPolicy::PerMessage => {
            serve_controlled_streams(server, stdin.lock(), io::stdout(), config)
        }
        FlushPolicy::Batch { .. } => serve_controlled_streams(
            server,
            stdin.lock(),
            io::BufWriter::new(io::stdout()),
            config,
        ),
    }
}

/// Controlled runtime with injectable input and an owned writer.
///
/// The writer is owned by a dedicated thread so handlers can never interleave
/// JSON fragments on the shared output stream.
///
/// # Errors
///
/// Returns configuration, stream, or worker lifecycle failures.
pub fn serve_controlled_streams<S, R, W>(
    server: Arc<S>,
    mut reader: R,
    writer: W,
    config: RuntimeConfig,
) -> io::Result<()>
where
    S: ConcurrentToolServer,
    R: BufRead,
    W: Write + Send + 'static,
{
    schema::reject_defects(server.strict_schema_defects())?;
    config.validate()?;
    let (output_tx, output_rx) = mpsc::sync_channel(config.output_queue_depth);
    let writer_handle =
        thread::spawn(move || writer_loop(writer, &output_rx, config.output_flush_policy));

    let registry = Arc::new(Mutex::new(HashMap::<String, CancellationToken>::new()));
    let active = Arc::new(AtomicUsize::new(0));
    let (job_tx, job_rx) = mpsc::sync_channel::<Job>(config.queue_depth);
    let job_rx = Arc::new(Mutex::new(job_rx));
    let mut workers = Vec::with_capacity(config.max_in_flight);
    for _ in 0..config.max_in_flight {
        let worker = Worker {
            server: Arc::clone(&server),
            jobs: Arc::clone(&job_rx),
            output: output_tx.clone(),
            registry: Arc::clone(&registry),
            active: Arc::clone(&active),
            config,
        };
        workers.push(thread::spawn(move || worker.run()));
    }
    drop(server);

    let read_result = read_jobs(&mut reader, &job_tx, &output_tx, &registry, &config);
    drop(job_tx);
    let mut worker_result = Ok(());
    for worker in workers {
        match worker.join() {
            Ok(Err(error)) if worker_result.is_ok() => worker_result = Err(error),
            Err(_) if worker_result.is_ok() => {
                worker_result = Err(io::Error::other("controlled runtime worker panicked"));
            }
            Ok(Ok(()) | Err(_)) | Err(_) => {}
        }
    }

    let _ = output_tx.send(OutputMessage::Shutdown);
    drop(output_tx);
    let writer_result = writer_handle
        .join()
        .map_err(|_| io::Error::other("writer thread panicked"))?;
    read_result.and(worker_result).and(writer_result)
}

fn read_jobs(
    reader: &mut impl BufRead,
    jobs: &SyncSender<Job>,
    output: &SyncSender<OutputMessage>,
    registry: &Arc<Mutex<HashMap<String, CancellationToken>>>,
    config: &RuntimeConfig,
) -> io::Result<()> {
    let mut frame = Vec::with_capacity(512);
    loop {
        match read_frame(reader, &mut frame, config.transport.max_request_bytes)? {
            FrameStatus::EndOfStream => return Ok(()),
            FrameStatus::Incomplete => {
                send_error(
                    output,
                    "null",
                    -32_700,
                    "incomplete JSON-RPC message at EOF",
                    config.transport.max_response_bytes,
                )?;
                return Ok(());
            }
            FrameStatus::Oversized => {
                send_error(
                    output,
                    "null",
                    SERVER_BUSY,
                    &format!(
                        "request exceeds max_request_bytes ({})",
                        config.transport.max_request_bytes
                    ),
                    config.transport.max_response_bytes,
                )?;
            }
            FrameStatus::Complete => {
                let Ok(line) = std::str::from_utf8(&frame) else {
                    send_error(
                        output,
                        "null",
                        -32_700,
                        "JSON-RPC message is not valid UTF-8",
                        config.transport.max_response_bytes,
                    )?;
                    continue;
                };
                if line.trim().is_empty() {
                    continue;
                }
                let control = inspect_request(line);
                if let Some(cancelled) = control.cancelled {
                    let registry = registry
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    if let Some(token) = registry.get(&cancelled) {
                        token.cancel();
                    }
                    continue;
                }
                let cancellation = CancellationToken::new();
                if let Some(id) = &control.id {
                    let mut registry = registry
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    if registry.contains_key(id) {
                        drop(registry);
                        send_error(
                            output,
                            id,
                            -32_600,
                            "duplicate in-flight request id",
                            config.transport.max_response_bytes,
                        )?;
                        continue;
                    }
                    registry.insert(id.clone(), cancellation.clone());
                }
                let job = Job {
                    line: line.to_owned(),
                    id: control.id,
                    cancellation,
                    progress_token: control.progress_token,
                };
                match jobs.try_send(job) {
                    Ok(()) => {}
                    Err(TrySendError::Full(job)) => {
                        remove_request(registry, job.id.as_deref());
                        if let Some(id) = job.id {
                            send_error(
                                output,
                                &id,
                                SERVER_BUSY,
                                "request queue is full",
                                config.transport.max_response_bytes,
                            )?;
                        }
                    }
                    Err(TrySendError::Disconnected(job)) => {
                        remove_request(registry, job.id.as_deref());
                        return Err(io::Error::new(
                            io::ErrorKind::BrokenPipe,
                            "request workers stopped",
                        ));
                    }
                }
            }
        }
    }
}

struct RequestControl {
    id: Option<String>,
    cancelled: Option<String>,
    progress_token: Option<String>,
}

fn inspect_request(line: &str) -> RequestControl {
    let Ok(request) = blazingly_json::from_str::<Value>(line) else {
        return RequestControl {
            id: None,
            cancelled: None,
            progress_token: None,
        };
    };
    let id = request.get("id").and_then(serialize_value);
    let cancelled = (request.get("method").and_then(Value::as_str)
        == Some("notifications/cancelled"))
    .then(|| {
        request
            .pointer("/params/requestId")
            .and_then(serialize_value)
    })
    .flatten();
    let progress_token = request
        .pointer("/params/_meta/progressToken")
        .and_then(serialize_value);
    RequestControl {
        id,
        cancelled,
        progress_token,
    }
}

fn serialize_value(value: &Value) -> Option<String> {
    blazingly_json::to_string(value).ok()
}

struct Worker<S> {
    server: Arc<S>,
    jobs: Arc<Mutex<Receiver<Job>>>,
    output: SyncSender<OutputMessage>,
    registry: Arc<Mutex<HashMap<String, CancellationToken>>>,
    active: Arc<AtomicUsize>,
    config: RuntimeConfig,
}

impl<S: ConcurrentToolServer> Worker<S> {
    fn run(self) -> io::Result<()> {
        loop {
            let job = {
                let jobs = self
                    .jobs
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                jobs.recv()
            };
            let Ok(job) = job else {
                return Ok(());
            };
            self.run_job(job)?;
        }
    }

    fn run_job(&self, mut job: Job) -> io::Result<()> {
        if job.cancellation.is_cancelled() {
            remove_request(&self.registry, job.id.as_deref());
            return Ok(());
        }
        if !try_acquire(&self.active, self.config.max_in_flight) {
            remove_request(&self.registry, job.id.as_deref());
            if let Some(id) = job.id {
                send_error(
                    &self.output,
                    &id,
                    SERVER_BUSY,
                    "all handler slots are occupied",
                    self.config.transport.max_response_bytes,
                )?;
            }
            return Ok(());
        }

        let deadline = self
            .config
            .handler_deadline
            .and_then(|duration| Instant::now().checked_add(duration));
        let progress = job.progress_token.as_ref().map(|token| ProgressReporter {
            output: self.output.clone(),
            token: token.clone(),
            max_response_bytes: self.config.transport.max_response_bytes,
            last_progress: Arc::new(Mutex::new(None)),
            cancellation: job.cancellation.clone(),
        });
        let context = RequestContext {
            cancellation: job.cancellation.clone(),
            deadline,
            progress,
        };
        let line = std::mem::take(&mut job.line);
        let result_rx = spawn_handler(
            Arc::clone(&self.server),
            Arc::clone(&self.active),
            context,
            line,
            self.config.transport.max_response_bytes,
        );
        self.wait_for_handler(&job, deadline, &result_rx)
    }

    fn wait_for_handler(
        &self,
        job: &Job,
        deadline: Option<Instant>,
        result_rx: &Receiver<HandlerOutcome>,
    ) -> io::Result<()> {
        let max_response_bytes = self.config.transport.max_response_bytes;
        loop {
            if job.cancellation.is_cancelled() {
                remove_request(&self.registry, job.id.as_deref());
                return Ok(());
            }
            let wait = deadline.map_or(POLL_INTERVAL, |deadline| {
                deadline
                    .saturating_duration_since(Instant::now())
                    .min(POLL_INTERVAL)
            });
            if wait.is_zero() {
                job.cancellation.cancel();
                remove_request(&self.registry, job.id.as_deref());
                if let Some(id) = &job.id {
                    send_error(
                        &self.output,
                        id,
                        SERVER_BUSY,
                        "handler deadline exceeded",
                        max_response_bytes,
                    )?;
                }
                return Ok(());
            }
            match result_rx.recv_timeout(wait) {
                Ok(outcome) => {
                    if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
                        job.cancellation.cancel();
                        remove_request(&self.registry, job.id.as_deref());
                        if let Some(id) = &job.id {
                            send_error(
                                &self.output,
                                id,
                                SERVER_BUSY,
                                "handler deadline exceeded",
                                max_response_bytes,
                            )?;
                        }
                        return Ok(());
                    }
                    remove_request(&self.registry, job.id.as_deref());
                    if job.cancellation.is_cancelled() {
                        return Ok(());
                    }
                    match outcome {
                        HandlerOutcome::Response(Some(response)) => self
                            .output
                            .send(OutputMessage::Frame(response))
                            .map_err(|_| {
                                io::Error::new(io::ErrorKind::BrokenPipe, "output channel closed")
                            })?,
                        HandlerOutcome::Response(None) => {}
                        HandlerOutcome::Failed(message) => {
                            if let Some(id) = &job.id {
                                send_error(
                                    &self.output,
                                    id,
                                    INTERNAL_ERROR,
                                    &format!("handler failed: {message}"),
                                    max_response_bytes,
                                )?;
                            }
                        }
                        HandlerOutcome::Panicked => {
                            if let Some(id) = &job.id {
                                send_error(
                                    &self.output,
                                    id,
                                    INTERNAL_ERROR,
                                    "handler panicked",
                                    max_response_bytes,
                                )?;
                            }
                        }
                    }
                    return Ok(());
                }
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => {
                    remove_request(&self.registry, job.id.as_deref());
                    return Err(io::Error::new(
                        io::ErrorKind::BrokenPipe,
                        "handler result channel closed",
                    ));
                }
            }
        }
    }
}

fn spawn_handler<S: ConcurrentToolServer>(
    server: Arc<S>,
    active: Arc<AtomicUsize>,
    context: RequestContext,
    line: String,
    max_response_bytes: usize,
) -> Receiver<HandlerOutcome> {
    let (result_tx, result_rx) = mpsc::sync_channel(1);
    thread::spawn(move || {
        let result = catch_unwind(AssertUnwindSafe(|| {
            run_handler(&*server, &context, &line, max_response_bytes)
        }));
        active.fetch_sub(1, Ordering::AcqRel);
        let outcome = match result {
            Ok(Ok(response)) => HandlerOutcome::Response(response),
            Ok(Err(error)) => HandlerOutcome::Failed(error.to_string()),
            Err(_) => HandlerOutcome::Panicked,
        };
        let _ = result_tx.send(outcome);
    });
    result_rx
}

fn run_handler<S: ConcurrentToolServer>(
    server: &S,
    context: &RequestContext,
    line: &str,
    max_response_bytes: usize,
) -> io::Result<Option<Vec<u8>>> {
    let mut response = ResponseBuffer::new(max_response_bytes);
    let mut adapter = Adapter { server, context };
    match crate::serve_message(&mut adapter, line, &mut response) {
        Ok(true) => Ok(Some(response.into_bytes())),
        Ok(false) => Ok(None),
        Err(_) if response.exceeded() => {
            let id = inspect_request(line)
                .id
                .unwrap_or_else(|| "null".to_owned());
            Ok(Some(error_frame(
                &id,
                SERVER_BUSY,
                &format!("response exceeds max_response_bytes ({max_response_bytes})"),
                max_response_bytes,
            )?))
        }
        Err(error) => Err(error),
    }
}

fn try_acquire(active: &AtomicUsize, limit: usize) -> bool {
    let mut current = active.load(Ordering::Acquire);
    loop {
        if current >= limit {
            return false;
        }
        match active.compare_exchange_weak(
            current,
            current + 1,
            Ordering::AcqRel,
            Ordering::Acquire,
        ) {
            Ok(_) => return true,
            Err(actual) => current = actual,
        }
    }
}

fn remove_request(registry: &Arc<Mutex<HashMap<String, CancellationToken>>>, id: Option<&str>) {
    if let Some(id) = id {
        registry
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(id);
    }
}

fn send_error(
    output: &SyncSender<OutputMessage>,
    id: &str,
    code: i64,
    message: &str,
    max_response_bytes: usize,
) -> io::Result<()> {
    let frame = error_frame(id, code, message, max_response_bytes)?;
    output
        .send(OutputMessage::Frame(frame))
        .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "output channel closed"))
}

fn error_frame(
    id: &str,
    code: i64,
    message: &str,
    max_response_bytes: usize,
) -> io::Result<Vec<u8>> {
    let mut response = ResponseBuffer::new(max_response_bytes);
    write_error_frame(&mut response, id, code, message).or_else(|_| {
        response.clear();
        write_error_frame(&mut response, "null", code, "runtime rejected the request")
    })?;
    Ok(response.into_bytes())
}

fn write_error_frame(
    response: &mut ResponseBuffer,
    id: &str,
    code: i64,
    message: &str,
) -> io::Result<()> {
    response.write_all(br#"{"jsonrpc":"2.0","id":"#)?;
    response.write_all(id.as_bytes())?;
    response.write_all(br#","error":{"code":"#)?;
    blazingly_json::to_writer(&mut *response, &code)?;
    response.write_all(br#","message":"#)?;
    blazingly_json::to_writer(&mut *response, &message)?;
    response.write_all(b"}}\n")
}

fn writer_loop(
    writer: impl Write,
    output: &Receiver<OutputMessage>,
    flush_policy: FlushPolicy,
) -> io::Result<()> {
    match flush_policy {
        FlushPolicy::PerMessage => writer_loop_impl::<true>(writer, output, 1),
        FlushPolicy::Batch { max_messages } => {
            writer_loop_impl::<false>(writer, output, max_messages)
        }
    }
}

fn writer_loop_impl<const FLUSH_EACH: bool>(
    mut writer: impl Write,
    output: &Receiver<OutputMessage>,
    max_messages: usize,
) -> io::Result<()> {
    let mut pending_messages = 0;
    while let Ok(message) = output.recv() {
        match message {
            OutputMessage::Frame(frame) => {
                writer.write_all(&frame)?;
                pending_messages += 1;
                if FLUSH_EACH || pending_messages >= max_messages {
                    writer.flush()?;
                    pending_messages = 0;
                }
            }
            OutputMessage::Shutdown => break,
        }
    }
    if pending_messages > 0 {
        writer.flush()?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{serve_controlled_streams, ConcurrentMcpServer, RuntimeConfig, SERVER_BUSY};
    use crate::{json, ToolReply, TransportLimits, Value};
    use std::fmt::Write as _;
    use std::io::{self, Write};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};
    use std::thread;
    use std::time::Duration;

    #[derive(Clone, Default)]
    struct SharedWriter(Arc<Mutex<Vec<u8>>>);

    impl Write for SharedWriter {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.0
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    fn lines(writer: &SharedWriter) -> Vec<Value> {
        let bytes = writer
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        String::from_utf8(bytes)
            .unwrap()
            .lines()
            .map(|line| blazingly_json::from_str(line).unwrap())
            .collect()
    }

    fn tool_requests(count: usize) -> String {
        (0..count).fold(String::new(), |mut input, id| {
            writeln!(
                input,
                "{{\"jsonrpc\":\"2.0\",\"id\":{id},\"method\":\"tools/call\",\
                 \"params\":{{\"name\":\"work\",\"arguments\":{{}}}}}}"
            )
            .expect("write request fixture");
            input
        })
    }

    #[test]
    fn controlled_runtime_reports_progress_and_results() {
        let server = ConcurrentMcpServer::new("test", "1").tool(
            "work",
            "work",
            json!({"type":"object"}),
            |context, arguments| {
                assert!(context
                    .report_progress(1.0, Some(2.0), Some("half"))
                    .unwrap());
                assert!(context
                    .report_progress(2.0, Some(2.0), Some("done"))
                    .unwrap());
                ToolReply::structured(arguments)
            },
        );
        let input = concat!(
            "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"tools/call\",",
            "\"params\":{\"name\":\"work\",\"arguments\":{\"ok\":true},",
            "\"_meta\":{\"progressToken\":\"p1\"}}}\n"
        );
        let writer = SharedWriter::default();
        serve_controlled_streams(
            Arc::new(server),
            input.as_bytes(),
            writer.clone(),
            RuntimeConfig::default(),
        )
        .unwrap();
        let output = lines(&writer);
        assert_eq!(output.len(), 3);
        assert_eq!(output[0]["method"], "notifications/progress");
        assert_eq!(output[1]["params"]["progress"], 2.0);
        assert_eq!(output[2]["result"]["structuredContent"]["ok"], true);
    }

    #[test]
    fn strict_schemas_refuse_to_start_the_controlled_runtime() {
        let server = ConcurrentMcpServer::new("test", "1").strict_schemas().tool(
            "work",
            "work",
            json!({"type": "object", "properties": {"steps": {"type": "array"}}}),
            |_, arguments| ToolReply::structured(arguments),
        );
        assert_eq!(server.schema_defects().len(), 1);
        let error = serve_controlled_streams(
            Arc::new(server),
            &b""[..],
            SharedWriter::default(),
            RuntimeConfig::default(),
        )
        .expect_err("strict schemas must refuse the catalog");
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
        assert!(error.to_string().contains("/properties/steps"));
    }

    #[test]
    fn controlled_runtime_isolates_panics_and_deadlines() {
        let panic_server = ConcurrentMcpServer::new("test", "1").tool(
            "panic",
            "panic",
            json!({"type":"object"}),
            |_, _| panic!("boom"),
        );
        let panic_input = concat!(
            "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"tools/call\",",
            "\"params\":{\"name\":\"panic\",\"arguments\":{}}}\n"
        );
        let panic_writer = SharedWriter::default();
        serve_controlled_streams(
            Arc::new(panic_server),
            panic_input.as_bytes(),
            panic_writer.clone(),
            RuntimeConfig::default(),
        )
        .unwrap();
        let panic_output = lines(&panic_writer);
        assert_eq!(panic_output.len(), 1);
        assert_eq!(panic_output[0]["id"], 1);
        assert_eq!(panic_output[0]["error"]["message"], "handler panicked");

        let deadline_server = ConcurrentMcpServer::new("test", "1").tool(
            "wait",
            "wait",
            json!({"type":"object"}),
            |context, _| {
                while !context.is_cancelled() {
                    std::thread::yield_now();
                }
                ToolReply::text("late")
            },
        );
        let deadline_input = concat!(
            "{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"tools/call\",",
            "\"params\":{\"name\":\"wait\",\"arguments\":{}}}\n"
        );
        let deadline_writer = SharedWriter::default();
        let config = RuntimeConfig {
            handler_deadline: Some(Duration::from_millis(30)),
            ..RuntimeConfig::default()
        };
        serve_controlled_streams(
            Arc::new(deadline_server),
            deadline_input.as_bytes(),
            deadline_writer.clone(),
            config,
        )
        .unwrap();
        let deadline_output = lines(&deadline_writer);
        assert_eq!(deadline_output.len(), 1);
        assert_eq!(deadline_output[0]["id"], 2);
        assert_eq!(
            deadline_output[0]["error"]["message"],
            "handler deadline exceeded"
        );
    }

    #[test]
    fn controlled_runtime_cancels_queued_or_running_work_without_a_response() {
        let server = ConcurrentMcpServer::new("test", "1").tool(
            "wait",
            "wait",
            json!({"type":"object"}),
            |context, _| {
                while !context.is_cancelled() {
                    std::thread::yield_now();
                }
                ToolReply::text("late")
            },
        );
        let input = concat!(
            "{\"jsonrpc\":\"2.0\",\"id\":\"cancel-me\",\"method\":\"tools/call\",",
            "\"params\":{\"name\":\"wait\",\"arguments\":{}}}\n",
            "{\"jsonrpc\":\"2.0\",\"method\":\"notifications/cancelled\",",
            "\"params\":{\"requestId\":\"cancel-me\"}}\n"
        );
        let writer = SharedWriter::default();
        serve_controlled_streams(
            Arc::new(server),
            input.as_bytes(),
            writer.clone(),
            RuntimeConfig::default(),
        )
        .unwrap();
        assert!(lines(&writer).is_empty());
    }

    #[test]
    fn controlled_runtime_converts_response_overflow_to_an_atomic_error() {
        let server = ConcurrentMcpServer::new("test", "1").tool(
            "large",
            "large",
            json!({"type":"object"}),
            |_, _| ToolReply::text("x".repeat(1024)),
        );
        let input = concat!(
            "{\"jsonrpc\":\"2.0\",\"id\":7,\"method\":\"tools/call\",",
            "\"params\":{\"name\":\"large\",\"arguments\":{}}}\n"
        );
        let writer = SharedWriter::default();
        let config = RuntimeConfig {
            transport: TransportLimits::new(1024, 256),
            ..RuntimeConfig::default()
        };
        serve_controlled_streams(Arc::new(server), input.as_bytes(), writer.clone(), config)
            .unwrap();
        let output = lines(&writer);
        assert_eq!(output.len(), 1);
        assert_eq!(output[0]["id"], 7);
        assert_eq!(output[0]["error"]["code"], SERVER_BUSY);
    }

    #[test]
    fn controlled_runtime_never_exceeds_the_configured_concurrency() {
        let active = Arc::new(AtomicUsize::new(0));
        let maximum = Arc::new(AtomicUsize::new(0));
        let handler_active = Arc::clone(&active);
        let handler_maximum = Arc::clone(&maximum);
        let server = ConcurrentMcpServer::new("test", "1").tool(
            "work",
            "work",
            json!({"type":"object"}),
            move |_, _| {
                let active = handler_active.fetch_add(1, Ordering::AcqRel) + 1;
                handler_maximum.fetch_max(active, Ordering::AcqRel);
                thread::sleep(Duration::from_millis(20));
                handler_active.fetch_sub(1, Ordering::AcqRel);
                ToolReply::text("done")
            },
        );
        let input = tool_requests(8);
        let writer = SharedWriter::default();
        let config = RuntimeConfig {
            max_in_flight: 3,
            queue_depth: 8,
            output_queue_depth: 8,
            ..RuntimeConfig::default()
        };
        serve_controlled_streams(Arc::new(server), input.as_bytes(), writer.clone(), config)
            .unwrap();
        assert_eq!(maximum.load(Ordering::Acquire), 3);
        assert_eq!(lines(&writer).len(), 8);
    }

    #[derive(Clone, Default)]
    struct SlowWriter(SharedWriter);

    impl Write for SlowWriter {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            thread::sleep(Duration::from_millis(1));
            self.0.write(bytes)
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn controlled_runtime_applies_bounded_backpressure_without_losing_requests() {
        let server = ConcurrentMcpServer::new("test", "1").tool(
            "work",
            "work",
            json!({"type":"object"}),
            |_, _| ToolReply::text("done"),
        );
        let input = tool_requests(50);
        let writer = SlowWriter::default();
        let output = writer.0.clone();
        let config = RuntimeConfig {
            max_in_flight: 2,
            queue_depth: 2,
            output_queue_depth: 2,
            ..RuntimeConfig::default()
        };
        serve_controlled_streams(Arc::new(server), input.as_bytes(), writer, config).unwrap();
        let messages = lines(&output);
        assert_eq!(messages.len(), 50);
        assert!(messages
            .iter()
            .any(|message| message["error"]["message"] == "request queue is full"));
    }
}
