//! Route table: proto methods (built-in and user-supplied, compiled at startup
//! with protox) mapped onto engine actions.

use std::collections::BTreeMap;
use std::sync::Arc;

use prost_reflect::{DescriptorPool, MethodDescriptor};
use serde::Serialize;
use serde_json::Value;

use onto_intelli::config::Config;
use onto_intelli::{Error, Result, actions};

/// Encoded descriptor set of the built-in onto_intelli.v1 API.
pub const BUILTIN_DESCRIPTOR: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/onto_intelli_descriptor.bin"));

/// Special actions handled by the router itself.
pub const ASK_STREAM: &str = "intelli.ask_stream";
pub const CALL: &str = "$call";

const BUILTIN_ROUTES: &[(&str, &str)] = &[
    ("onto_intelli.v1.OntologyService/UpsertEntity", "entity.upsert"),
    ("onto_intelli.v1.OntologyService/PatchEntity", "entity.patch"),
    ("onto_intelli.v1.OntologyService/GetEntity", "entity.get"),
    ("onto_intelli.v1.OntologyService/DeleteEntity", "entity.delete"),
    ("onto_intelli.v1.OntologyService/FindEntities", "entity.find"),
    ("onto_intelli.v1.OntologyService/Search", "entity.search"),
    ("onto_intelli.v1.OntologyService/CreateLink", "link.create"),
    ("onto_intelli.v1.OntologyService/DeleteLink", "link.delete"),
    ("onto_intelli.v1.OntologyService/Apply", "graph.apply"),
    ("onto_intelli.v1.OntologyService/Neighbors", "graph.neighbors"),
    ("onto_intelli.v1.OntologyService/Traverse", "graph.traverse"),
    ("onto_intelli.v1.OntologyService/Path", "graph.path"),
    ("onto_intelli.v1.IntelliService/Ask", "intelli.ask"),
    ("onto_intelli.v1.IntelliService/AskStream", ASK_STREAM),
    ("onto_intelli.v1.IntelliService/BuildContext", "context.build"),
    ("onto_intelli.v1.IntelliService/Infer", "graph.infer"),
    ("onto_intelli.v1.IntelliService/Dependencies", "graph.dependencies"),
    ("onto_intelli.v1.IntelliService/Impact", "graph.impact"),
    ("onto_intelli.v1.IntelliService/RootCause", "graph.root_cause"),
    ("onto_intelli.v1.IntelliService/CriticalPath", "graph.critical_path"),
    ("onto_intelli.v1.IntelliService/Cycles", "graph.cycles"),
    ("onto_intelli.v1.MemoryService/Remember", "memory.remember"),
    ("onto_intelli.v1.MemoryService/Recall", "memory.recall"),
    ("onto_intelli.v1.MemoryService/Consolidate", "memory.consolidate"),
    ("onto_intelli.v1.EventService/Append", "event.append"),
    ("onto_intelli.v1.EventService/List", "event.list"),
    ("onto_intelli.v1.EventService/Ingest", "ingest.batch"),
    ("onto_intelli.v1.ActionService/List", "action.list"),
    ("onto_intelli.v1.ActionService/Plan", "action.plan"),
    ("onto_intelli.v1.ActionService/Apply", "action.apply"),
    ("onto_intelli.v1.ActionService/Undo", "action.undo"),
    ("onto_intelli.v1.ActionService/Receipt", "action.receipt"),
    ("onto_intelli.v1.AnalyticsService/Metric", "metric.query"),
    ("onto_intelli.v1.AnalyticsService/Metrics", "metric.list"),
    ("onto_intelli.v1.AnalyticsService/Derived", "entity.derived"),
    ("onto_intelli.v1.AnalyticsService/History", "entity.history"),
    ("onto_intelli.v1.AnalyticsService/AsOf", "entity.as_of"),
    ("onto_intelli.v1.AnalyticsService/Changes", "graph.changes"),
    ("onto_intelli.v1.AnalyticsService/Quality", "quality.report"),
    ("onto_intelli.v1.AnalyticsService/Duplicates", "entity.duplicates"),
    ("onto_intelli.v1.AnalyticsService/Merge", "entity.merge"),
    ("onto_intelli.v1.AnalyticsService/Scenario", "scenario.run"),
    ("onto_intelli.v1.AnalyticsService/Experts", "people.experts"),
    ("onto_intelli.v1.AnalyticsService/Profile", "schema.profile"),
    ("onto_intelli.v1.AnalyticsService/SuggestSchema", "schema.suggest"),
    ("onto_intelli.v1.AnalyticsService/Export", "export.graph"),
    ("onto_intelli.v1.AnalyticsService/Eval", "eval.run"),
    ("onto_intelli.v1.SchemaService/Get", "schema.get"),
    ("onto_intelli.v1.SchemaService/Status", "schema.status"),
    ("onto_intelli.v1.SchemaService/Plan", "schema.plan"),
    ("onto_intelli.v1.SchemaService/Apply", "schema.apply"),
    ("onto_intelli.v1.SchemaService/History", "schema.history"),
    ("onto_intelli.v1.SchemaService/Version", "schema.version"),
    ("onto_intelli.v1.SchemaService/AsOf", "schema.as_of"),
    ("onto_intelli.v1.SchemaService/Diff", "schema.diff"),
    ("onto_intelli.v1.SchemaService/Revert", "schema.revert"),
    ("onto_intelli.v1.SchemaService/Undo", "schema.undo"),
    ("onto_intelli.v1.SchemaService/Propose", "schema.propose"),
    ("onto_intelli.v1.SchemaService/ListProposals", "schema.proposals"),
    ("onto_intelli.v1.SchemaService/GetProposal", "schema.proposal"),
    ("onto_intelli.v1.SchemaService/Approve", "schema.approve"),
    ("onto_intelli.v1.SchemaService/Reject", "schema.reject"),
    ("onto_intelli.v1.SchemaService/Comment", "schema.comment"),
    ("onto_intelli.v1.SchemaService/Withdraw", "schema.withdraw"),
    ("onto_intelli.v1.SchemaService/Revise", "schema.revise"),
    ("onto_intelli.v1.ExecutionService/Graphs", "execution.graphs"),
    ("onto_intelli.v1.ExecutionService/Vocabulary", "execution.vocabulary"),
    ("onto_intelli.v1.ExecutionService/SetVocabulary", "execution.set_vocabulary"),
    ("onto_intelli.v1.ExecutionService/InitGraph", "execution.init_graph"),
    ("onto_intelli.v1.CatalogService/AddConnector", "connector.add"),
    ("onto_intelli.v1.CatalogService/ListConnectors", "connector.list"),
    ("onto_intelli.v1.CatalogService/RemoveConnector", "connector.remove"),
    ("onto_intelli.v1.CatalogService/Crawl", "connector.crawl"),
    ("onto_intelli.v1.CatalogService/FindData", "catalog.find"),
    ("onto_intelli.v1.CatalogService/JoinPath", "catalog.join"),
    ("onto_intelli.v1.CatalogService/TrustedMetric", "catalog.metric"),
    ("onto_intelli.v1.CatalogService/Rank", "catalog.rank"),
    ("onto_intelli.v1.CatalogService/Top", "catalog.top"),
    ("onto_intelli.v1.CatalogService/Rerank", "catalog.rerank"),
    ("onto_intelli.v1.CatalogService/LearnedFacts", "learned.list"),
    ("onto_intelli.v1.CatalogService/Learn", "learned.run"),
    ("onto_intelli.v1.CatalogService/Certify", "learned.certify"),
    ("onto_intelli.v1.CatalogService/Reject", "learned.reject"),
    ("onto_intelli.v1.OntologyBuilderService/DraftOntology", "ontology.draft"),
    ("onto_intelli.v1.OntologyBuilderService/RefineOntology", "ontology.refine"),
    ("onto_intelli.v1.OntologyBuilderService/AdviseOntology", "ontology.advise"),
    ("onto_intelli.v1.OntologyBuilderService/ApplyDraft", "ontology.apply_draft"),
    ("onto_intelli.v1.ObjectService/ObjectTypes", "objects.types"),
    ("onto_intelli.v1.ObjectService/Explore", "objects.explore"),
    ("onto_intelli.v1.AdminService/Health", "system.health"),
    ("onto_intelli.v1.AdminService/Stats", "system.stats"),
    ("onto_intelli.v1.AdminService/Schema", "schema.get"),
    ("onto_intelli.v1.AdminService/Actions", "system.actions"),
    ("onto_intelli.v1.AdminService/Call", CALL),
    ("onto_intelli.v1.ObservabilityService/ListTraces", "trace.list"),
    ("onto_intelli.v1.ObservabilityService/GetTrace", "trace.get"),
    ("onto_intelli.v1.ObservabilityService/Metrics", "trace.metrics"),
    ("onto_intelli.v1.ObservabilityService/ListSessions", "trace.sessions"),
    ("onto_intelli.v1.ObservabilityService/GetSession", "trace.session"),
    ("onto_intelli.v1.ObservabilityService/Score", "trace.score"),
    ("onto_intelli.v1.AssuranceService/Run", "reason.run"),
    ("onto_intelli.v1.AssuranceService/Query", "reason.query"),
    ("onto_intelli.v1.AssuranceService/Explain", "reason.explain"),
    ("onto_intelli.v1.AssuranceService/Provenance", "reason.provenance"),
    ("onto_intelli.v1.AssuranceService/Certify", "reason.certify"),
    ("onto_intelli.v1.AssuranceService/CheckCertificate", "reason.check_certificate"),
    ("onto_intelli.v1.AssuranceService/Binding", "reason.binding"),
    ("onto_intelli.v1.AssuranceService/Verify", "reason.verify"),
    ("onto_intelli.v1.AssuranceService/SliceCheck", "reason.slice_check"),
    ("onto_intelli.v1.AssuranceService/ValidateShapes", "shapes.validate"),
    ("onto_intelli.v1.AssuranceService/InduceShape", "shapes.induce"),
    ("onto_intelli.v1.AssuranceService/CheckShapes", "shapes.check"),
    ("onto_intelli.v1.AssuranceService/Lint", "schema.lint"),
    ("onto_intelli.v1.AssuranceService/LintFeedback", "schema.lint_feedback"),
    ("onto_intelli.v1.AssuranceService/Locks", "schema.locks"),
    ("onto_intelli.v1.AssuranceService/Lock", "schema.lock"),
    ("onto_intelli.v1.AssuranceService/Unlock", "schema.unlock"),
    ("onto_intelli.v1.AssuranceService/Align", "schema.align"),
    ("onto_intelli.v1.AssuranceService/AlignCertify", "schema.align_certify"),
    ("onto_intelli.v1.AssuranceService/ListPacks", "pack.list"),
    ("onto_intelli.v1.AssuranceService/ShowPack", "pack.show"),
    ("onto_intelli.v1.AssuranceService/PlanPack", "pack.plan"),
    ("onto_intelli.v1.AssuranceService/InstallPack", "pack.install"),
    ("onto_intelli.v1.AssuranceService/InduceSheet", "model.induce"),
    ("onto_intelli.v1.AssuranceService/IngestSheet", "ingest.sheet"),
    ("onto_intelli.v1.ChangeService/Poll", "changes.poll"),
    ("onto_intelli.v1.ChangeService/Head", "changes.head"),
    ("onto_intelli.v1.ChangeService/Capture", "changes.capture"),
    ("onto_intelli.v1.WebhookService/Add", "webhook.add"),
    ("onto_intelli.v1.WebhookService/Update", "webhook.update"),
    ("onto_intelli.v1.WebhookService/Remove", "webhook.remove"),
    ("onto_intelli.v1.WebhookService/List", "webhook.list"),
    ("onto_intelli.v1.WebhookService/Test", "webhook.test"),
    ("onto_intelli.v1.WebhookService/RotateSecret", "webhook.rotate_secret"),
    ("onto_intelli.v1.WebhookService/Deliveries", "webhook.deliveries"),
    ("onto_intelli.v1.WebhookService/Replay", "webhook.replay"),
    ("onto_intelli.v1.WebhookService/Resume", "webhook.resume"),
    ("onto_intelli.v1.WebhookService/Kill", "webhook.kill"),
    ("onto_intelli.v1.WebhookService/Outbox", "outbox.list"),
    ("onto_intelli.v1.AutomateService/List", "automation.list"),
    ("onto_intelli.v1.AutomateService/Get", "automation.get"),
    ("onto_intelli.v1.AutomateService/Runs", "automation.runs"),
    ("onto_intelli.v1.AutomateService/Test", "automation.test"),
    ("onto_intelli.v1.AutomateService/Enable", "automation.enable"),
    ("onto_intelli.v1.AutomateService/Disable", "automation.disable"),
    ("onto_intelli.v1.AutomateService/Rerun", "automation.rerun"),
    ("onto_intelli.v1.AutomateService/Kill", "automation.kill"),
];

pub struct Route {
    pub method: MethodDescriptor,
    pub action: String,
    pub request: Option<Value>,
    pub response: Option<Value>,
    pub workspace: Option<String>,
    pub builtin: bool,
}

pub struct RouteTable {
    /// Keyed by HTTP/2 path: `/package.Service/Method`.
    pub routes: BTreeMap<String, Arc<Route>>,
    pub pool: DescriptorPool,
}

#[derive(Debug, Serialize)]
pub struct RouteInfo {
    pub path: String,
    pub action: String,
    pub input: String,
    pub output: String,
    pub streaming: &'static str,
    pub builtin: bool,
    pub templated: bool,
}

fn method_by_name(pool: &DescriptorPool, full: &str) -> Result<MethodDescriptor> {
    let (service, method) = full
        .rsplit_once('/')
        .ok_or_else(|| Error::Config(format!("route method '{full}' must look like package.Service/Method")))?;
    let svc = pool.get_service_by_name(service).ok_or_else(|| {
        Error::Config(format!("service {service} not found in the configured proto files (grpc.proto_files)"))
    })?;
    svc.methods()
        .find(|m| m.name() == method)
        .ok_or_else(|| Error::Config(format!("method {method} not found on {service}")))
}

impl RouteTable {
    pub fn build(cfg: &Config) -> Result<Self> {
        let mut pool = DescriptorPool::new();
        if cfg.grpc.builtin_api {
            pool.decode_file_descriptor_set(BUILTIN_DESCRIPTOR)
                .map_err(|e| Error::Config(format!("built-in descriptor: {e}")))?;
        }
        if !cfg.grpc.proto_files.is_empty() {
            let includes: Vec<std::path::PathBuf> = if cfg.grpc.proto_dirs.is_empty() {
                vec![cfg.resolve_path(".")]
            } else {
                cfg.grpc.proto_dirs.iter().map(|d| cfg.resolve_path(d)).collect()
            };
            let fds = protox::compile(&cfg.grpc.proto_files, &includes)
                .map_err(|e| Error::Config(format!("compiling proto files: {e}")))?;
            let fresh = prost_types::FileDescriptorSet {
                file: fds.file.into_iter().filter(|f| pool.get_file_by_name(f.name()).is_none()).collect(),
            };
            pool.add_file_descriptor_set(fresh).map_err(|e| Error::Config(format!("loading proto files: {e}")))?;
        }

        let mut routes = BTreeMap::new();
        if cfg.grpc.builtin_api {
            for (method, action) in BUILTIN_ROUTES {
                let md = method_by_name(&pool, method)?;
                routes.insert(
                    format!("/{method}"),
                    Arc::new(Route {
                        method: md,
                        action: action.to_string(),
                        request: None,
                        response: None,
                        workspace: None,
                        builtin: true,
                    }),
                );
            }
        }
        for r in &cfg.grpc.routes {
            let md = method_by_name(&pool, &r.method)?;
            if r.action != ASK_STREAM && r.action != CALL && actions::find(&r.action).is_none() {
                return Err(Error::Config(format!(
                    "route {}: unknown action '{}' (see `onto actions`)",
                    r.method, r.action
                )));
            }
            if md.is_client_streaming() && md.is_server_streaming() {
                return Err(Error::Config(format!("route {}: bidirectional streaming is not supported", r.method)));
            }
            routes.insert(
                format!("/{}", r.method),
                Arc::new(Route {
                    method: md,
                    action: r.action.clone(),
                    request: r.request.clone(),
                    response: r.response.clone(),
                    workspace: r.workspace.clone(),
                    builtin: false,
                }),
            );
        }
        Ok(Self { routes, pool })
    }

    /// Everything loaded, for server reflection.
    pub fn file_descriptor_set(&self) -> prost_types::FileDescriptorSet {
        prost_types::FileDescriptorSet { file: self.pool.file_descriptor_protos().cloned().collect() }
    }

    pub fn describe(&self) -> Vec<RouteInfo> {
        self.routes
            .iter()
            .map(|(path, r)| RouteInfo {
                path: path.clone(),
                action: r.action.clone(),
                input: r.method.input().full_name().to_string(),
                output: r.method.output().full_name().to_string(),
                streaming: match (r.method.is_client_streaming(), r.method.is_server_streaming()) {
                    (false, false) => "unary",
                    (false, true) => "server",
                    (true, false) => "client",
                    (true, true) => "bidi",
                },
                builtin: r.builtin,
                templated: r.request.is_some() || r.response.is_some(),
            })
            .collect()
    }
}
