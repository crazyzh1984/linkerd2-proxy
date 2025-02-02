use super::opaque_transport::OpaqueTransport;
use crate::target::Endpoint;
use linkerd2_app_core::{
    config::ConnectConfig,
    metrics,
    proxy::identity,
    svc,
    transport::{io, tls, ConnectTcp},
    Error,
};
use tracing::debug_span;

// Establishes connections to remote peers (for both TCP forwarding and HTTP
// proxying).
pub fn stack<P>(
    config: &ConnectConfig,
    server_port: u16,
    local_identity: tls::Conditional<identity::Local>,
    metrics: &metrics::Proxy,
) -> impl svc::Service<
    Endpoint<P>,
    Response = impl tokio::io::AsyncRead + tokio::io::AsyncWrite + Send + Unpin,
    Error = Error,
    Future = impl Send,
> + Clone {
    svc::stack(ConnectTcp::new(config.keepalive))
        // Initiates mTLS if the target is configured with identity.
        .push(tls::Client::layer(local_identity))
        // If the endpoint has an opaque transport hint, this layer ensures the
        // transport header is written on the connection as soon as the
        // connection is established.
        .push(OpaqueTransport::layer())
        // Limits the time we wait for a connection to be established.
        .push_timeout(config.timeout)
        .push(metrics.transport.layer_connect())
        .push_request_filter(PreventLoop { port: server_port })
        .into_inner()
}

pub fn forward<P, I, C>(
    connect: C,
) -> impl svc::NewService<
    Endpoint<P>,
    Service = impl svc::Service<I, Response = (), Error = Error, Future = impl Send> + Clone,
> + Clone
where
    P: Clone + Send + 'static,
    I: io::AsyncRead + io::AsyncWrite + io::PeerAddr + std::fmt::Debug + Send + Unpin + 'static,
    C: svc::Service<Endpoint<P>> + Clone + Send + 'static,
    C::Response: tokio::io::AsyncRead + tokio::io::AsyncWrite + Send + Unpin,
    C::Error: Into<Error>,
    C::Future: Send,
{
    svc::stack(connect)
        .push_make_thunk()
        .push_on_response(super::Forward::layer())
        .instrument(|_: &Endpoint<P>| debug_span!("tcp.forward"))
        .check_new_service::<Endpoint<P>, I>()
        .into_inner()
}

/// A connection policy that fails connections that target the outbound listener.
#[derive(Clone)]
struct PreventLoop {
    port: u16,
}

#[derive(Clone, Debug)]
struct LoopPrevented {
    port: u16,
}

// === impl PreventLoop ===

impl<P> svc::stack::FilterRequest<Endpoint<P>> for PreventLoop {
    type Request = Endpoint<P>;

    fn filter(&self, ep: Endpoint<P>) -> Result<Endpoint<P>, Error> {
        let addr = ep.addr;

        tracing::trace!(%addr, self.port, "PreventLoop");
        if addr.ip().is_loopback() && addr.port() == self.port {
            return Err(LoopPrevented { port: self.port }.into());
        }

        Ok(ep)
    }
}

// === impl LoopPrevented ===

pub fn is_loop(err: &(dyn std::error::Error + 'static)) -> bool {
    err.is::<LoopPrevented>() || err.source().map(is_loop).unwrap_or(false)
}

impl std::fmt::Display for LoopPrevented {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "outbound requests must not target localhost:{}",
            self.port
        )
    }
}

impl std::error::Error for LoopPrevented {}
