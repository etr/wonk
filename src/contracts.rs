//! Contract extraction: canonical ID normalization, the `http`, `env`,
//! `queue`, `websocket`, and `job` contract kinds (TASK-082, TASK-087), the
//! RPC-family and schema kinds `grpc`, `graphql`, and `openapi` plus the
//! canonical join they require (TASK-088, PRD-CTR-REQ-001..004, 024).
//!
//! Contracts are detected by walking the tree-sitter tree that the symbol
//! indexer already parsed — no second parse or file read (PRD-CTR-REQ-011).
//! Detected candidates persist in the per-repo `contracts` table via the
//! indexing pipeline; [`list_contracts`] queries them back (TASK-083).
//!
//! Normalization core (AR-017):
//! - [`canonical_contract_id`] is the only place contract IDs are built.
//! - [`normalize_http_path`] runs a fixed 6-stage pipeline (PRD-CTR-REQ-003).
//! - [`normalize_method`] upper-cases verbs and maps router catch-alls to
//!   `ANY`.
//! - [`normalize_topic`] normalizes queue/websocket/job names; unlike the
//!   HTTP pipeline it REJECTS computed topics outright (`None`), because
//!   exact-ID matching is the only pairing mechanism for the message kinds.
//!
//! Role mappings are PER SPEC and deliberately inverted between kinds —
//! do not "fix" one to match the other:
//! - **queue (DR-031):** provider = the construct that registers a handler
//!   (subscriber, `@KafkaListener`, `ch.consume`), consumer = the code that
//!   initiates by publishing (`producer.send`, `ch.publish`). The reader of
//!   a topic serves it; the writer calls on it.
//! - **websocket:** provider = emit sites (`io.emit`, `@SendTo`), consumer =
//!   handler registrations (`socket.on`, `@MessageMapping`, `app.ws`). Here
//!   the writer serves and the reader subscribes — the mirror image of the
//!   queue rule, per the TASK-087 specification.
//! - **grpc:** provider = the serving side (proto `rpc` declarations,
//!   `XGrpc.XImplBase`/Servicer method impls, `Register<S>Server`,
//!   `addService`, `impl …::S for T`); consumer = generated-stub call sites
//!   (`stub.getUser`, `client.GetUser`, `pb.New<S>Client(conn).M(…)`). A
//!   service-level registration uses identifier `*` (`grpc::S::*`) and pairs
//!   with any method-level consumer of that service.
//! - **graphql:** provider = resolvers (SDL root-type fields, JS resolver
//!   maps, `@strawberry.*`, Ariadne `@Query.field`); consumer = operation
//!   call sites (`gql` tagged templates, Apollo `.query`/`.mutate` strings,
//!   Python `gql(…)`, operation documents).
//! - **openapi:** provider only — specification documents are file-level
//!   contracts with a NULL owning symbol by design (§4.24); there is no
//!   consumer side to detect.
//!
//! Document files (TASK-088, no-new-crates constraint): `.proto`,
//! `.graphql`/`.gql`, and `.yaml`/`.yml`/`.json` carry no grammar, so tiny
//! line-oriented scanners ([`extract_document_contracts`]) read them
//! instead. Only a document that yields candidates gets a `files` row
//! (empty symbols, language = [`DocumentKind::as_str`]) — that row is the
//! hash/re-index anchor, and the stored contracts carry a NULL symbol_id;
//! everything else stays un-indexed exactly as before. OpenAPI additionally
//! sniffs content (a top-level `openapi:`/`swagger:` key plus `paths:`), so
//! CI/compose/package files never index.
//!
//! RPC canonical join ([`canonical_rpc_join`], PRD-CTR-REQ-024): exact ID
//! equality is the first pass and is never overridden — candidates with an
//! opposite-role exact counterpart in their own workspace are excluded
//! entirely. The join is the SECOND pass, pure and in-memory over
//! workspace-scoped slices, tolerating package qualification (service
//! compared on the last dot-segment, case-folded), method casing and
//! snake/camel separators (`get_user` = `GetUser` = `getUser`), and
//! service-level `*` registration (method-level providers win). Workspace
//! equality on normalized identifiers ([`normalize_workspace_id`],
//! PRD-CTR-REQ-019) is the REQ-014 guard — the join relaxes names, never
//! scope. IDs keep the developer's spelling; only the comparison relaxes.
//!
//! RabbitMQ note: producers address exchange+routing-key while consumers
//! address queue names; the binding between them is broker config and
//! invisible to static analysis. Pairing therefore relies on aligned names
//! (topic-exchange convention); the Ruby `channel.queue` binding pre-pass
//! closes part of the gap where the queue name is declared inline.
//!
//! Verb collisions (`send`/`emit`) resolve by guard order: websocket
//! receivers first, then the queue generic arms, and the ambiguous HTTP
//! arm last behind its `is_path_like` gate.
//!
//! Out of scope by design (extraction, not validation): GraphQL servers in
//! Java/Go/C#/PHP/Rust; untracked gRPC receiver variables; JS grpc arms
//! without a file-level "grpc" marker; plain look-alike strings (only
//! `gql`/`graphql` tags, Apollo option objects, and Python `gql(…)` parse);
//! flow-style YAML maps and multi-document YAML; a GraphQL document's
//! second operation.

use std::collections::HashMap;

use tree_sitter::{Node, Tree};

use crate::indexer::Lang;
use crate::types::{ContractCandidate, ContractKind, ContractRole, PathParam};

mod document;
mod normalize;
mod source;
mod workspace;

pub use document::*;
pub use normalize::*;
pub use source::*;
pub use workspace::*;
