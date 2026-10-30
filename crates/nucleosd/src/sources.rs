//! Tool sources: the MCP servers a deployment runs, and the tool set they add up to.
//!
//! A source is a child process. Adding one connects it, asks what it advertises, and publishes a new
//! **generation** — the kernel's existing mechanism for changing the tool set without disturbing a
//! session that is mid-call. Removing one does the same in reverse. Both are configuration changes, so
//! both are signed, gated and recorded, exactly like a model.
//!
//! Three decisions worth stating:
//!
//! - **Tools are rebuilt from the sources, never edited in place.** The published set is always "what
//!   every configured source currently advertises", so a source cannot leave a tool behind that its
//!   server no longer lists.
//! - **A source that cannot start at boot does not stop the daemon.** It is a warning and a missing
//!   tool, not a refusal: sessions pinned to older generations keep working, and the operator can fix
//!   the server without the deployment being down. Adding one *by request* is the opposite — the caller
//!   is waiting for an answer, so a failed connection fails the request.
//! - **The stored form is the command, not the tools.** Tools are discovered, so a store that kept them
//!   would be a cache that goes stale the moment the server is upgraded.

use crate::admin::Admin;
use crate::http::{ApiError, App};
use axum::extract::{Path, State};
use axum::Json;
use nucleos::adapter::{ConfigScope, Tool, Verdict};
use nucleos::contract::Principal;
use nucleos::error::{Error, Result};
use nucleos::wire::api::{SourceListResponse, SourceRequest, SourceView};
use nucleos_sqlite::{Change, SystemStore};
use std::collections::BTreeMap;
use std::sync::Arc;

/// Where source definitions live in the system store.
pub const SOURCES_KEY: &str = "config/sources";

/// One MCP server, as the store keeps it.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SourceSpec {
    /// The name a listing and a removal use.
    pub name: String,
    /// The command that starts it.
    pub command: String,
    /// Its arguments.
    #[serde(default)]
    pub args: Vec<String>,
    /// Extra environment.
    #[serde(default)]
    pub env: BTreeMap<String, String>,
}

/// A source that is connected right now.
struct Connected {
    spec: SourceSpec,
    tools: Vec<Arc<dyn Tool>>,
}

/// The sources a deployment runs.
#[derive(Default)]
pub struct SourceSet {
    entries: Vec<Connected>,
}

impl SourceSet {
    /// The tools every connected source advertises.
    #[must_use]
    pub fn tools(&self) -> Vec<Arc<dyn Tool>> {
        self.entries
            .iter()
            .flat_map(|entry| entry.tools.iter().cloned())
            .collect()
    }

    /// One row per source, with the tools it contributed.
    #[must_use]
    pub fn views(&self) -> Vec<SourceView> {
        self.entries
            .iter()
            .map(|entry| SourceView {
                name: entry.spec.name.clone(),
                command: entry.spec.command.clone(),
                args: entry.spec.args.clone(),
                tools: entry.tools.iter().map(|tool| tool.spec().name).collect(),
            })
            .collect()
    }

    /// Connect everything the store names.
    ///
    /// A source that cannot start is logged and skipped: a daemon that refuses to boot because one MCP
    /// server is down turns a missing tool into an outage.
    pub async fn load(system: &SystemStore) -> Result<Self> {
        let Some(raw) = system.get(SOURCES_KEY)? else {
            return Ok(Self::default());
        };
        let specs: Vec<SourceSpec> = serde_json::from_str(&raw)
            .map_err(|error| Error::internal(format!("stored sources are unreadable: {error}")))?;

        let mut set = Self::default();
        for spec in specs {
            match connect(&spec).await {
                Ok(entry) => set.entries.push(entry),
                Err(error) => tracing::warn!(
                    event = "source_unavailable",
                    source = %spec.name,
                    detail = %error.reason(),
                    consequence = "the daemon keeps running; this source's tools are missing",
                ),
            }
        }
        Ok(set)
    }

    /// Connect a source, replace any entry with the same name, and persist.
    ///
    /// Connected before it is stored: a source that cannot start is a request that failed, not a row
    /// that will fail on every boot.
    pub async fn add(
        &mut self,
        system: &SystemStore,
        spec: SourceSpec,
        by: &Principal,
    ) -> Result<SourceView> {
        if spec.name.trim().is_empty() || spec.command.trim().is_empty() {
            return Err(Error::invalid("a source needs a name and a command"));
        }
        let entry = connect(&spec).await?;
        let view = SourceView {
            name: entry.spec.name.clone(),
            command: entry.spec.command.clone(),
            args: entry.spec.args.clone(),
            tools: entry.tools.iter().map(|tool| tool.spec().name).collect(),
        };
        self.entries.retain(|existing| existing.spec.name != spec.name);
        self.entries.push(entry);
        self.persist(system, by, &format!("add {}", spec.name)).await?;
        Ok(view)
    }

    /// Disconnect a source and forget it.
    pub async fn remove(&mut self, system: &SystemStore, name: &str, by: &Principal) -> Result<bool> {
        let before = self.entries.len();
        self.entries.retain(|entry| entry.spec.name != name);
        if self.entries.len() == before {
            return Ok(false);
        }
        self.persist(system, by, &format!("remove {name}")).await?;
        Ok(true)
    }

    /// Reconnect every source, returning how many came back.
    ///
    /// Nothing is persisted: the stored spec is the truth, and a reconnect that rewrote it would let a
    /// server's current mood become configuration.
    pub async fn reconnect(&mut self) -> usize {
        let mut alive = Vec::new();
        for entry in &self.entries {
            match connect(&entry.spec).await {
                Ok(connected) => alive.push(connected),
                Err(error) => tracing::warn!(
                    event = "source_reconnect_failed",
                    source = %entry.spec.name,
                    detail = %error.reason(),
                ),
            }
        }
        let count = alive.len();
        self.entries = alive;
        count
    }

    async fn persist(&self, system: &SystemStore, by: &Principal, what: &str) -> Result<()> {
        let specs: Vec<&SourceSpec> = self.entries.iter().map(|entry| &entry.spec).collect();
        let text = serde_json::to_string(&specs)
            .map_err(|error| Error::internal(format!("cannot encode the sources: {error}")))?;
        system.set(SOURCES_KEY, &text).await?;
        system
            .record(Change {
                at_unix_millis: now(),
                tenant: by.tenant.clone(),
                subject: by.subject.clone(),
                key_id: String::new(),
                key: "sources".to_owned(),
                value: what.to_owned(),
            })
            .await
    }
}

/// Start one server and take what it advertises.
async fn connect(spec: &SourceSpec) -> Result<Connected> {
    let mut source = nucleos_mcp::McpSource::new(&spec.name, &spec.command)
        .with_args(spec.args.iter().cloned());
    source.env = spec.env.clone();
    let tools: Vec<Arc<dyn Tool>> = source
        .connect()
        .await?
        .into_iter()
        .map(|tool| Arc::new(tool) as Arc<dyn Tool>)
        .collect();
    Ok(Connected {
        spec: spec.clone(),
        tools,
    })
}

/// The sources this deployment runs.
pub async fn list_sources(
    State(app): State<Arc<App>>,
    _admin: Admin,
) -> std::result::Result<Json<SourceListResponse>, ApiError> {
    Ok(Json(SourceListResponse {
        sources: app.sources().read().await.views(),
    }))
}

/// Add or replace a source, and publish the tool set it makes.
pub async fn post_source(
    State(app): State<Arc<App>>,
    admin: Admin,
) -> std::result::Result<Json<SourceView>, ApiError> {
    let request: SourceRequest = serde_json::from_slice(&admin.body).map_err(|error| {
        ApiError::from_error(&Error::invalid(format!(
            "the source definition is not valid JSON: {error}"
        )))
    })?;
    permit(&app, &admin, "sources").await?;
    let kernel = app.kernel()?.clone();
    let system = store(&app)?;

    let spec = SourceSpec {
        name: request.name,
        command: request.command,
        args: request.args,
        env: request.env,
    };
    let view = app
        .sources()
        .write()
        .await
        .add(&system, spec, &admin.principal)
        .await
        .map_err(|error| ApiError::from_error(&error))?;
    let generation = publish(&app, &kernel).await?;
    tracing::info!(
        event = "source_added",
        source = %view.name,
        tools = view.tools.len(),
        generation = generation.0,
        by = %admin.principal.subject,
    );
    Ok(Json(view))
}

/// Remove a source, and publish the tool set that leaves behind.
pub async fn delete_source(
    State(app): State<Arc<App>>,
    Path(name): Path<String>,
    admin: Admin,
) -> std::result::Result<Json<SourceListResponse>, ApiError> {
    permit(&app, &admin, "sources").await?;
    let kernel = app.kernel()?.clone();
    let system = store(&app)?;

    let removed = app
        .sources()
        .write()
        .await
        .remove(&system, &name, &admin.principal)
        .await
        .map_err(|error| ApiError::from_error(&error))?;
    if !removed {
        return Err(ApiError::from_error(&Error::not_found(format!(
            "no source named {name:?}"
        ))));
    }
    let generation = publish(&app, &kernel).await?;
    tracing::info!(
        event = "source_removed",
        source = %name,
        generation = generation.0,
        by = %admin.principal.subject,
    );
    Ok(Json(SourceListResponse {
        sources: app.sources().read().await.views(),
    }))
}

/// Publish whatever the sources currently advertise, and update the advertised list.
pub async fn publish(
    app: &Arc<App>,
    kernel: &nucleos::Kernel,
) -> std::result::Result<nucleos::contract::Generation, ApiError> {
    let tools = app.sources().read().await.tools();
    let names: Vec<String> = tools.iter().map(|tool| tool.spec().name).collect();
    let generation = kernel
        .publish(tools)
        .map_err(|error| ApiError::from_error(&error))?;
    app.set_tools(names);
    Ok(generation)
}

fn store(app: &Arc<App>) -> std::result::Result<Arc<SystemStore>, ApiError> {
    app.system()
        .cloned()
        .ok_or_else(|| ApiError::from_error(&Error::unavailable("this daemon has no system store")))
}

async fn permit(app: &Arc<App>, admin: &Admin, subject: &str) -> std::result::Result<(), ApiError> {
    let kernel = app.kernel()?;
    match kernel
        .check_config(&admin.principal, ConfigScope::System, subject)
        .await
        .map_err(|error| ApiError::from_error(&error))?
    {
        Verdict::Allow => Ok(()),
        Verdict::Deny { code, reason } => Err(ApiError::from_error(&Error::new(code, reason))),
        Verdict::AskHuman { question, .. } => Err(ApiError::from_error(&Error::denied(format!(
            "changing {subject} needs a human ({question}), and configuration cannot wait for one"
        )))),
    }
}

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as u64)
        .unwrap_or(0)
}
