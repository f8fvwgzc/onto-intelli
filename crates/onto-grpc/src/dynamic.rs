//! Codec-level gRPC service: decodes any configured method with its runtime
//! descriptor, transcodes to JSON, runs the mapped engine action and encodes
//! the result as the method's output message. No generated stubs involved.

use std::convert::Infallible;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use futures::{Stream, StreamExt};
use prost::Message;
use prost_reflect::{DeserializeOptions, DynamicMessage, MessageDescriptor, SerializeOptions};
use serde_json::{Value, json};
use tonic::codec::{BufferSettings, Codec, DecodeBuf, Decoder, EncodeBuf, Encoder};
use tonic::server::{ClientStreamingService, Grpc, ServerStreamingService, UnaryService};
use tonic::{Request, Response, Status, Streaming};

use onto_intelli::trace::TraceCtx;
use onto_intelli::{AskRequest, Error, Hub, Intelli, Principal};

use crate::auth::{Auth, AuthError, Credential, guard_call, workspace_header};
use crate::routes::{ASK_STREAM, CALL, Route, RouteTable};
use crate::template;

type BoxFuture<T> = Pin<Box<dyn std::future::Future<Output = T> + Send>>;
type MsgStream = Pin<Box<dyn Stream<Item = Result<DynamicMessage, Status>> + Send>>;

// ── codec ──────────────────────────────────────────────────────────────

/// Protobuf codec for runtime descriptors. Decodes `decode`, encodes any message.
#[derive(Clone)]
pub struct DynCodec {
    decode: MessageDescriptor,
}

impl DynCodec {
    /// Server side: decode the method's request type.
    pub fn server(method: &prost_reflect::MethodDescriptor) -> Self {
        Self { decode: method.input() }
    }

    /// Client side: decode the method's response type.
    pub fn client(method: &prost_reflect::MethodDescriptor) -> Self {
        Self { decode: method.output() }
    }
}

impl Codec for DynCodec {
    type Encode = DynamicMessage;
    type Decode = DynamicMessage;
    type Encoder = DynEncoder;
    type Decoder = DynDecoder;

    fn encoder(&mut self) -> DynEncoder {
        DynEncoder
    }

    fn decoder(&mut self) -> DynDecoder {
        DynDecoder(self.decode.clone())
    }
}

pub struct DynEncoder;

impl Encoder for DynEncoder {
    type Item = DynamicMessage;
    type Error = Status;

    fn encode(&mut self, item: DynamicMessage, dst: &mut EncodeBuf<'_>) -> Result<(), Status> {
        item.encode(dst).map_err(|e| Status::internal(format!("encode: {e}")))
    }

    fn buffer_settings(&self) -> BufferSettings {
        BufferSettings::default()
    }
}

pub struct DynDecoder(MessageDescriptor);

impl Decoder for DynDecoder {
    type Item = DynamicMessage;
    type Error = Status;

    fn decode(&mut self, src: &mut DecodeBuf<'_>) -> Result<Option<DynamicMessage>, Status> {
        DynamicMessage::decode(self.0.clone(), src)
            .map(Some)
            .map_err(|e| Status::invalid_argument(format!("decode {}: {e}", self.0.full_name())))
    }
}

// ── JSON transcoding ───────────────────────────────────────────────────

pub fn to_json(msg: &DynamicMessage) -> Result<Value, Status> {
    let opts =
        SerializeOptions::new().use_proto_field_name(true).stringify_64_bit_integers(false).skip_default_fields(true);
    msg.serialize_with_options(serde_json::value::Serializer, &opts)
        .map(integral_numbers)
        .map_err(|e| Status::internal(format!("transcode: {e}")))
}

/// `google.protobuf.Struct` carries every number as a double; whole numbers
/// become integers again so `"stage": 2` reaches the engine as 2, not 2.0.
fn integral_numbers(v: Value) -> Value {
    match v {
        Value::Number(n) => match n.as_f64() {
            Some(f) if !n.is_i64() && !n.is_u64() && f.fract() == 0.0 && f.abs() < 9.0e15 => Value::from(f as i64),
            _ => Value::Number(n),
        },
        Value::Object(m) => Value::Object(m.into_iter().map(|(k, v)| (k, integral_numbers(v))).collect()),
        Value::Array(a) => Value::Array(a.into_iter().map(integral_numbers).collect()),
        other => other,
    }
}

pub fn from_json(desc: &MessageDescriptor, v: Value) -> Result<DynamicMessage, Status> {
    let v = strip_nulls(v);
    DynamicMessage::deserialize_with_options(desc.clone(), v, &DeserializeOptions::new().deny_unknown_fields(false))
        .map_err(|e| Status::internal(format!("result does not fit {}: {e}", desc.full_name())))
}

/// Proto messages have no `null`: drop null object members (array items stay).
fn strip_nulls(v: Value) -> Value {
    match v {
        Value::Object(m) => {
            Value::Object(m.into_iter().filter(|(_, v)| !v.is_null()).map(|(k, v)| (k, strip_nulls(v))).collect())
        }
        Value::Array(a) => Value::Array(a.into_iter().map(strip_nulls).collect()),
        other => other,
    }
}

pub fn status_of(e: &Error) -> Status {
    let msg = e.to_string();
    match e {
        Error::NotFound(_) => Status::not_found(msg),
        Error::Invalid(_) => Status::invalid_argument(msg),
        Error::Denied(_) => Status::permission_denied(msg),
        Error::Conflict(_) => Status::already_exists(msg),
        Error::Unavailable(_) => Status::unavailable(msg),
        Error::Config(_) => Status::failed_precondition(msg),
        Error::Limited(_) => Status::resource_exhausted(msg),
        _ => Status::internal(msg),
    }
}

// ── handler ────────────────────────────────────────────────────────────

struct Handler {
    route: Arc<Route>,
    intelli: Arc<Intelli>,
    principal: Principal,
    credential: Credential,
    /// `traceparent`, `x-onto-session-id` and the API key of the request.
    trace: TraceCtx,
    /// `console.allow_private_urls` (the guard on git URLs).
    allow_private: bool,
}

/// Add `x-onto-trace-id` to a response's metadata when the call was traced.
fn with_trace_id<T>(mut resp: Response<T>, trace_id: Option<&str>) -> Response<T> {
    if let Some(v) = trace_id.and_then(|id| id.parse().ok()) {
        resp.metadata_mut().insert(crate::observe::TRACE_ID, v);
    }
    resp
}

impl Handler {
    fn input(&self, request: Value, ws_header: Option<String>) -> Value {
        let ctx = json!({ "$principal": { "id": self.principal.id }, "$request": request.clone() });
        let mut input = match &self.route.request {
            Some(t) => {
                let mut scope = request.clone();
                if let (Value::Object(s), Value::Object(c)) = (&mut scope, ctx) {
                    s.extend(c);
                }
                template::render(t, &scope).unwrap_or(Value::Null)
            }
            None => request,
        };
        if let Value::Object(m) = &mut input
            && !m.contains_key("workspace")
            && let Some(ws) = self.route.workspace.clone().or(ws_header)
        {
            m.insert("workspace".into(), Value::String(ws));
        }
        input
    }

    fn output(&self, result: Value, request: &Value) -> Result<DynamicMessage, Status> {
        let shaped = match &self.route.response {
            Some(t) => {
                let mut scope = result;
                if let Value::Object(s) = &mut scope {
                    s.insert("$request".into(), request.clone());
                }
                template::render(t, &scope).unwrap_or(Value::Null)
            }
            None => result,
        };
        let shaped = if shaped.is_object() { shaped } else { json!({}) };
        from_json(&self.route.method.output(), shaped)
    }

    /// Run the route's action; also returns the id of its trace, if traced.
    async fn run(&self, request: Value, ws_header: Option<String>) -> Result<(DynamicMessage, Option<String>), Status> {
        let input = self.input(request.clone(), ws_header);
        let (result, trace_id) = if self.route.action == CALL {
            let action = input.get("action").and_then(Value::as_str).unwrap_or_default().to_string();
            let mut inner = input.get("input").cloned().unwrap_or(Value::Null);
            guard_call(&self.credential, &self.principal, &action, &mut inner, self.allow_private)
                .await
                .map_err(|e| status_of(&e))?;
            let t = self.intelli.call_traced(&self.principal, &action, inner, self.trace.clone()).await;
            (json!({ "output": t.result.map_err(|e| status_of(&e))? }), t.trace_id)
        } else {
            let mut input = input;
            guard_call(&self.credential, &self.principal, &self.route.action, &mut input, self.allow_private)
                .await
                .map_err(|e| status_of(&e))?;
            let t = self.intelli.call_traced(&self.principal, &self.route.action, input, self.trace.clone()).await;
            (t.result.map_err(|e| status_of(&e))?, t.trace_id)
        };
        Ok((self.output(result, &request)?, trace_id))
    }
}

struct Unary(Arc<Handler>);

impl UnaryService<DynamicMessage> for Unary {
    type Response = DynamicMessage;
    type Future = BoxFuture<Result<Response<DynamicMessage>, Status>>;

    fn call(&mut self, req: Request<DynamicMessage>) -> Self::Future {
        let h = self.0.clone();
        Box::pin(async move {
            let ws = workspace_header(req.metadata().as_ref());
            let json = to_json(req.get_ref())?;
            let (msg, trace_id) = h.run(json, ws).await?;
            Ok(with_trace_id(Response::new(msg), trace_id.as_deref()))
        })
    }
}

struct ServerStream(Arc<Handler>);

impl ServerStreamingService<DynamicMessage> for ServerStream {
    type Response = DynamicMessage;
    type ResponseStream = MsgStream;
    type Future = BoxFuture<Result<Response<MsgStream>, Status>>;

    fn call(&mut self, req: Request<DynamicMessage>) -> Self::Future {
        let h = self.0.clone();
        Box::pin(async move {
            let ws = workspace_header(req.metadata().as_ref());
            let json = to_json(req.get_ref())?;
            if h.route.action != ASK_STREAM {
                // Any other action on a server-streaming method: one message.
                let (msg, trace_id) = h.run(json, ws).await?;
                let s: MsgStream = Box::pin(futures::stream::iter(vec![Ok(msg)]));
                return Ok(with_trace_id(Response::new(s), trace_id.as_deref()));
            }
            let input = h.input(json, ws);
            let ask: AskRequest = serde_json::from_value(input)
                .map_err(|e| Status::invalid_argument(format!("intelli.ask_stream: {e}")))?;
            let (rx, trace_id) = h
                .intelli
                .try_ask_stream_traced(h.principal.clone(), ask, h.trace.clone())
                .map_err(|e| status_of(&e))?;
            let out = h.route.method.output();
            let s: MsgStream = Box::pin(tokio_stream::wrappers::ReceiverStream::new(rx).map(move |ev| {
                serde_json::to_value(&ev).map_err(|e| Status::internal(e.to_string())).and_then(|v| from_json(&out, v))
            }));
            Ok(with_trace_id(Response::new(s), trace_id.as_deref()))
        })
    }
}

struct ClientStream(Arc<Handler>);

impl ClientStreamingService<DynamicMessage> for ClientStream {
    type Response = DynamicMessage;
    type Future = BoxFuture<Result<Response<DynamicMessage>, Status>>;

    fn call(&mut self, req: Request<Streaming<DynamicMessage>>) -> Self::Future {
        let h = self.0.clone();
        Box::pin(async move {
            let ws = workspace_header(req.metadata().as_ref());
            let mut stream = req.into_inner();
            let mut records = Vec::new();
            while let Some(msg) = stream.next().await {
                records.push(to_json(&msg?)?);
            }
            let (msg, trace_id) = h.run(json!({ "records": records }), ws).await?;
            Ok(with_trace_id(Response::new(msg), trace_id.as_deref()))
        })
    }
}

// ── tower service (axum fallback) ──────────────────────────────────────

#[derive(Clone)]
pub struct DynamicGrpc {
    pub table: Arc<RouteTable>,
    pub hub: Arc<Hub>,
    pub auth: Arc<Auth>,
}

impl tower::Service<http::Request<axum::body::Body>> for DynamicGrpc {
    type Response = http::Response<tonic::body::Body>;
    type Error = Infallible;
    type Future = BoxFuture<Result<Self::Response, Infallible>>;

    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Infallible>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, req: http::Request<axum::body::Body>) -> Self::Future {
        let this = self.clone();
        Box::pin(async move {
            let path = req.uri().path().to_string();
            let Some(route) = this.table.routes.get(&path).cloned() else {
                return Ok(Status::unimplemented(format!("{path} is not served (configure it in [[grpc.routes]])"))
                    .into_http());
            };
            let caller = match this.auth.authenticate(req.headers()).await {
                Ok(c) => c,
                Err(AuthError::Unauthenticated(m)) => return Ok(Status::unauthenticated(m).into_http()),
                Err(AuthError::Forbidden(m)) => return Ok(Status::permission_denied(m).into_http()),
                Err(AuthError::Unavailable(m)) => return Ok(Status::unavailable(m).into_http()),
            };
            let intelli = match this.hub.resolve(caller.tenant.as_deref()).await {
                Ok(i) => i,
                Err(e) => return Ok(status_of(&e).into_http()),
            };
            let streaming = (route.method.is_client_streaming(), route.method.is_server_streaming());
            let trace = crate::observe::trace_ctx(req.headers(), &caller, "grpc");
            let handler = Arc::new(Handler {
                route: route.clone(),
                intelli,
                principal: caller.principal,
                credential: caller.credential,
                trace,
                allow_private: this.auth.allow_private_urls(),
            });
            let mut grpc = Grpc::new(DynCodec::server(&route.method));
            let resp = match streaming {
                (false, false) => grpc.unary(Unary(handler), req).await,
                (false, true) => grpc.server_streaming(ServerStream(handler), req).await,
                (true, false) => grpc.client_streaming(ClientStream(handler), req).await,
                (true, true) => Status::unimplemented("bidirectional streaming is not supported").into_http(),
            };
            Ok(resp)
        })
    }
}
