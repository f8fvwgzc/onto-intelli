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

use onto_intelli::{AskRequest, Error, Intelli, Principal};

use crate::auth::{Auth, workspace_header};
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
        .map_err(|e| Status::internal(format!("transcode: {e}")))
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
        _ => Status::internal(msg),
    }
}

// ── handler ────────────────────────────────────────────────────────────

struct Handler {
    route: Arc<Route>,
    intelli: Arc<Intelli>,
    principal: Principal,
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

    async fn run(&self, request: Value, ws_header: Option<String>) -> Result<DynamicMessage, Status> {
        let input = self.input(request.clone(), ws_header);
        let result = if self.route.action == CALL {
            let action = input.get("action").and_then(Value::as_str).unwrap_or_default().to_string();
            let inner = input.get("input").cloned().unwrap_or(Value::Null);
            let out = self.intelli.call(&self.principal, &action, inner).await.map_err(|e| status_of(&e))?;
            json!({ "output": out })
        } else {
            self.intelli.call(&self.principal, &self.route.action, input).await.map_err(|e| status_of(&e))?
        };
        self.output(result, &request)
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
            Ok(Response::new(h.run(json, ws).await?))
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
                let msg = h.run(json, ws).await?;
                let s: MsgStream = Box::pin(futures::stream::iter(vec![Ok(msg)]));
                return Ok(Response::new(s));
            }
            let input = h.input(json, ws);
            let ask: AskRequest = serde_json::from_value(input)
                .map_err(|e| Status::invalid_argument(format!("intelli.ask_stream: {e}")))?;
            let rx = h.intelli.ask_stream(h.principal.clone(), ask);
            let out = h.route.method.output();
            let s: MsgStream = Box::pin(tokio_stream::wrappers::ReceiverStream::new(rx).map(move |ev| {
                serde_json::to_value(&ev).map_err(|e| Status::internal(e.to_string())).and_then(|v| from_json(&out, v))
            }));
            Ok(Response::new(s))
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
            Ok(Response::new(h.run(json!({ "records": records }), ws).await?))
        })
    }
}

// ── tower service (axum fallback) ──────────────────────────────────────

#[derive(Clone)]
pub struct DynamicGrpc {
    pub table: Arc<RouteTable>,
    pub intelli: Arc<Intelli>,
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
            let principal = match this.auth.principal(req.headers()) {
                Ok(p) => p,
                Err(msg) => return Ok(Status::unauthenticated(msg).into_http()),
            };
            let streaming = (route.method.is_client_streaming(), route.method.is_server_streaming());
            let handler = Arc::new(Handler { route: route.clone(), intelli: this.intelli.clone(), principal });
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
