//! The gRPC surface: a thin translation of the kernel onto the ABI.
//!
//! Nothing here decides anything. It builds kernel types from wire types, calls the kernel, and
//! translates what comes back — which is why this file has no `if` that changes behaviour.

use crate::translate;
use nucleos::contract::{Principal, SessionId};
use nucleos::engine::Kernel;
use nucleos_proto::pb;
use std::pin::Pin;
use std::sync::Arc;
use tokio_stream::Stream;
use tonic::{Request, Response, Status};

/// The served ABI.
///
/// `kernel` is `None` until a deployment is configured. A daemon with no models cannot answer
/// `Start`, and pretending otherwise would produce an error from three layers down instead of
/// one sentence at the edge.
pub struct KernelService {
    kernel: Option<Arc<Kernel>>,
    /// The principal every call is attributed to.
    ///
    /// Single-tenant until per-connection authentication lands: the socket is a unix socket, so
    /// the peer is already the local user, and inventing a token scheme now would be a promise
    /// the daemon cannot keep yet.
    principal: Principal,
    models: Vec<String>,
}

impl KernelService {
    /// Serve a kernel.
    #[must_use]
    pub fn new(kernel: Arc<Kernel>, principal: Principal, models: Vec<String>) -> Self {
        Self {
            kernel: Some(kernel),
            principal,
            models,
        }
    }

    /// Serve nothing but `Describe`: the daemon is up, no deployment is configured.
    #[must_use]
    pub fn unconfigured() -> Self {
        Self {
            kernel: None,
            principal: Principal {
                tenant: "default".to_owned(),
                subject: "local".to_owned(),
            },
            models: Vec::new(),
        }
    }

    fn kernel(&self) -> Result<&Arc<Kernel>, Status> {
        self.kernel.as_ref().ok_or_else(|| {
            Status::unavailable("no deployment is configured: the daemon has no models or tools")
        })
    }
}

type ResponseStream = Pin<Box<dyn Stream<Item = Result<pb::Event, Status>> + Send>>;
type GatewayStream = Pin<Box<dyn Stream<Item = Result<pb::GenerateEvent, Status>> + Send>>;

#[tonic::async_trait]
impl pb::kernel_server::Kernel for KernelService {
    async fn describe(
        &self,
        _request: Request<pb::DescribeRequest>,
    ) -> Result<Response<pb::DescribeResponse>, Status> {
        Ok(Response::new(pb::DescribeResponse {
            version: env!("CARGO_PKG_VERSION").to_owned(),
            models: self.models.clone(),
            abi_versions: vec![nucleos_proto::ABI_VERSION.to_owned()],
            generation: 0,
            // The daemon advertises neither: a server that claims a capability it does not
            // implement is worse than one that makes the caller fall back.
            mcp_sampling: false,
            mcp_elicitation: false,
        }))
    }

    async fn start(
        &self,
        request: Request<pb::StartRequest>,
    ) -> Result<Response<pb::StartResponse>, Status> {
        let kernel = self.kernel()?.clone();
        let request = request.into_inner();
        let id = SessionId::new(request.session.clone());
        let spec = translate::spec_from_pb(&request);

        let outcome = if request.resume {
            kernel.resume(&self.principal, id.clone(), spec).await
        } else {
            kernel.start(&self.principal, id.clone(), spec).await
        }
        .map_err(|error| status(&error))?;

        Ok(Response::new(pb::StartResponse {
            error: None,
            session: outcome.session.as_str().to_owned(),
            generation: 0,
            state: state_code(outcome.state),
            seq: outcome.next_seq.saturating_sub(1),
        }))
    }

    type RunStream = ResponseStream;

    async fn run(
        &self,
        request: Request<pb::RunRequest>,
    ) -> Result<Response<Self::RunStream>, Status> {
        let kernel = self.kernel()?.clone();
        let request = request.into_inner();
        let id = SessionId::new(request.session);

        // Replay what the log holds, then end. A live tail needs a subscriber registry, which is
        // the daemon's next slice; resumption from `from_seq` is already the property that makes
        // a dropped connection harmless.
        let events = kernel
            .events(&id, request.from_seq)
            .await
            .map_err(|error| status(&error))?;
        let stream = tokio_stream::iter(
            events
                .iter()
                .map(|event| Ok(translate::event(event)))
                .collect::<Vec<_>>(),
        );
        Ok(Response::new(Box::pin(stream) as Self::RunStream))
    }

    async fn send(
        &self,
        request: Request<pb::SendRequest>,
    ) -> Result<Response<pb::SendResponse>, Status> {
        let kernel = self.kernel()?.clone();
        let request = request.into_inner();
        let Some(pb::send_request::Input::Verdict(verdict)) = request.input else {
            return Err(Status::unimplemented(
                "a conversational `Send` needs the live tail; only a HITL verdict is accepted",
            ));
        };
        let id = SessionId::new(request.session);
        let spec = nucleos::engine::SessionSpec::default();
        let outcome = kernel
            .answer(
                &self.principal,
                id,
                spec,
                translate::verdict_from_pb(&verdict),
            )
            .await
            .map_err(|error| status(&error))?;
        Ok(Response::new(pb::SendResponse {
            error: None,
            accepted_seq: outcome.next_seq.saturating_sub(1),
        }))
    }

    async fn credit(
        &self,
        _request: Request<pb::CreditRequest>,
    ) -> Result<Response<pb::CreditResponse>, Status> {
        Err(Status::unimplemented(
            "credit lives in the checkpoint; the kernel half lands with the store slice",
        ))
    }

    async fn cancel(
        &self,
        _request: Request<pb::CancelRequest>,
    ) -> Result<Response<pb::CancelResponse>, Status> {
        Err(Status::unimplemented(
            "cancellation needs the live session handle, which the daemon does not hold yet",
        ))
    }

    async fn close(
        &self,
        _request: Request<pb::CloseRequest>,
    ) -> Result<Response<pb::CloseResponse>, Status> {
        Err(Status::unimplemented(
            "a session ends when its driving call returns; there is nothing to close yet",
        ))
    }

    async fn list(
        &self,
        _request: Request<pb::ListRequest>,
    ) -> Result<Response<pb::ListResponse>, Status> {
        Err(Status::unimplemented(
            "listing needs the session table, which the daemon does not hold yet",
        ))
    }

    async fn register(
        &self,
        _request: Request<pb::RegisterRequest>,
    ) -> Result<Response<pb::RegisterResponse>, Status> {
        Err(Status::unimplemented(
            "tool sources are declared in the daemon's configuration; discovery lands with the MCP client",
        ))
    }

    async fn deregister(
        &self,
        _request: Request<pb::DeregisterRequest>,
    ) -> Result<Response<pb::DeregisterResponse>, Status> {
        Err(Status::unimplemented(
            "tool sources are declared in the daemon's configuration",
        ))
    }

    async fn sources(
        &self,
        _request: Request<pb::SourcesRequest>,
    ) -> Result<Response<pb::SourcesResponse>, Status> {
        Err(Status::unimplemented(
            "there are no discovered tool sources to report yet",
        ))
    }

    async fn reload(
        &self,
        _request: Request<pb::ReloadRequest>,
    ) -> Result<Response<pb::ReloadResponse>, Status> {
        Err(Status::unimplemented(
            "reload swaps discovered tool sources; that lands with the MCP client",
        ))
    }

    type GenerateStream = GatewayStream;

    async fn generate(
        &self,
        _request: Request<pb::GenerateRequest>,
    ) -> Result<Response<Self::GenerateStream>, Status> {
        // The kernel already has the route walk and the ledger; what is missing is a stateless
        // entry point that does not write to a session's log. Half a gateway would be worse than
        // none, so this stays explicitly unimplemented until that entry point exists.
        Err(Status::unimplemented(
            "the gateway needs a stateless kernel entry point; the session path is served",
        ))
    }
}

fn status(error: &nucleos::Error) -> Status {
    let message = error.reason().to_owned();
    match error.code() {
        nucleos::contract::ErrorCode::Invalid => Status::invalid_argument(message),
        nucleos::contract::ErrorCode::NotFound => Status::not_found(message),
        nucleos::contract::ErrorCode::Denied => Status::permission_denied(message),
        nucleos::contract::ErrorCode::Budget => Status::resource_exhausted(message),
        nucleos::contract::ErrorCode::Timeout => Status::deadline_exceeded(message),
        nucleos::contract::ErrorCode::Unavailable => Status::unavailable(message),
        nucleos::contract::ErrorCode::Internal => Status::internal(message),
    }
}

fn state_code(state: nucleos::contract::SessionState) -> i32 {
    use nucleos::contract::SessionState;
    let value = match state {
        SessionState::Created => pb::SessionState::Created,
        SessionState::Running => pb::SessionState::Running,
        SessionState::AwaitingTool => pb::SessionState::AwaitingTool,
        SessionState::AwaitingHuman => pb::SessionState::AwaitingHuman,
        SessionState::Cancelling => pb::SessionState::Cancelling,
        SessionState::Done => pb::SessionState::Done,
        SessionState::Failed => pb::SessionState::Failed,
        SessionState::Cancelled => pb::SessionState::Cancelled,
    };
    value as i32
}
