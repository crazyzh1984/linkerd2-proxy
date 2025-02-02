use indexmap::IndexMap;
use linkerd2_app_core::{
    classify, dst, http_request_authority_addr, http_request_host_addr,
    http_request_l5d_override_dst_addr, metrics,
    opaque_transport::Header,
    profiles,
    proxy::{http, identity, tap},
    stack_tracing, svc,
    transport::{self, listen, tls},
    Addr, Conditional, CANONICAL_DST_HEADER, DST_OVERRIDE_HEADER,
};
use std::{convert::TryInto, net::SocketAddr, str::FromStr, sync::Arc};
use tracing::debug;

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct TcpAccept {
    pub target_addr: SocketAddr,
    pub peer_addr: SocketAddr,
    pub peer_id: tls::PeerIdentity,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Target {
    pub dst: Addr,
    pub socket_addr: SocketAddr,
    pub http_version: http::Version,
    pub tls_client_id: tls::PeerIdentity,
}

#[derive(Clone, Debug)]
pub struct Logical {
    target: Target,
    profiles: Option<profiles::Receiver>,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct HttpEndpoint {
    pub port: u16,
    pub settings: http::client::Settings,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct TcpEndpoint {
    pub port: u16,
}

#[derive(Clone, Debug)]
pub struct RequestTarget {
    accept: TcpAccept,
}

#[derive(Copy, Clone, Debug)]
pub struct ProfileTarget;

// === impl TcpAccept ===

impl From<listen::Addrs> for TcpAccept {
    fn from(tcp: listen::Addrs) -> Self {
        Self {
            target_addr: tcp.target_addr(),
            peer_addr: tcp.peer(),
            peer_id: tls::Conditional::None(tls::ReasonForNoPeerName::PortSkipped),
        }
    }
}

impl From<tls::accept::Meta> for TcpAccept {
    fn from(tls: tls::accept::Meta) -> Self {
        Self {
            target_addr: tls.addrs.target_addr(),
            peer_addr: tls.addrs.peer(),
            peer_id: tls.peer_identity,
        }
    }
}

impl Into<SocketAddr> for &'_ TcpAccept {
    fn into(self) -> SocketAddr {
        self.target_addr
    }
}

impl Into<transport::labels::Key> for &'_ TcpAccept {
    fn into(self) -> transport::labels::Key {
        transport::labels::Key::accept(transport::labels::Direction::In, self.peer_id.clone())
    }
}

// === impl HttpEndpoint ===

impl Into<http::client::Settings> for &'_ HttpEndpoint {
    fn into(self) -> http::client::Settings {
        self.settings
    }
}

impl From<Target> for HttpEndpoint {
    fn from(target: Target) -> Self {
        Self {
            port: target.socket_addr.port(),
            settings: target.http_version.into(),
        }
    }
}

// === TcpEndpoint ===

impl From<TcpAccept> for TcpEndpoint {
    fn from(tcp: TcpAccept) -> Self {
        Self {
            port: tcp.target_addr.port(),
        }
    }
}

impl From<Header> for TcpEndpoint {
    fn from(Header { port, .. }: Header) -> Self {
        Self { port }
    }
}

impl From<HttpEndpoint> for TcpEndpoint {
    fn from(HttpEndpoint { port, .. }: HttpEndpoint) -> Self {
        Self { port }
    }
}

impl Into<u16> for TcpEndpoint {
    fn into(self) -> u16 {
        self.port
    }
}

impl Into<transport::labels::Key> for &'_ TcpEndpoint {
    fn into(self) -> transport::labels::Key {
        transport::labels::Key::Connect(transport::labels::EndpointLabels {
            direction: transport::labels::Direction::In,
            authority: None,
            labels: None,
            tls_id: tls::Conditional::None(tls::ReasonForNoPeerName::Loopback).into(),
        })
    }
}

// === impl Profile ===

pub(super) fn route((route, logical): (profiles::http::Route, Logical)) -> dst::Route {
    dst::Route {
        route,
        target: logical.target.dst,
        direction: metrics::Direction::In,
    }
}

// === impl Target ===

/// Used for profile discovery.
impl Into<Addr> for &'_ Target {
    fn into(self) -> Addr {
        self.dst.clone()
    }
}

/// Used for profile discovery.
impl Into<SocketAddr> for &'_ Target {
    fn into(self) -> SocketAddr {
        self.socket_addr
    }
}

impl tls::HasPeerIdentity for Target {
    fn peer_identity(&self) -> tls::PeerIdentity {
        Conditional::None(tls::ReasonForNoPeerName::Loopback)
    }
}

impl Into<transport::labels::Key> for &'_ Target {
    fn into(self) -> transport::labels::Key {
        transport::labels::Key::Connect(self.into())
    }
}

impl Into<metrics::EndpointLabels> for &'_ Target {
    fn into(self) -> metrics::EndpointLabels {
        metrics::EndpointLabels {
            authority: self.dst.name_addr().map(|d| d.as_http_authority()),
            direction: metrics::Direction::In,
            tls_id: self
                .tls_client_id
                .clone()
                .map(metrics::TlsId::ClientId)
                .into(),
            labels: None,
        }
    }
}

impl classify::CanClassify for Target {
    type Classify = classify::Request;

    fn classify(&self) -> classify::Request {
        classify::Request::default()
    }
}

impl tap::Inspect for Target {
    fn src_addr<B>(&self, req: &http::Request<B>) -> Option<SocketAddr> {
        req.extensions().get::<TcpAccept>().map(|s| s.peer_addr)
    }

    fn src_tls<'a, B>(
        &self,
        req: &'a http::Request<B>,
    ) -> Conditional<&'a identity::Name, tls::ReasonForNoPeerName> {
        req.extensions()
            .get::<TcpAccept>()
            .map(|s| s.peer_id.as_ref())
            .unwrap_or_else(|| Conditional::None(tls::ReasonForNoPeerName::LocalIdentityDisabled))
    }

    fn dst_addr<B>(&self, _: &http::Request<B>) -> Option<SocketAddr> {
        Some(self.socket_addr)
    }

    fn dst_labels<B>(&self, _: &http::Request<B>) -> Option<&IndexMap<String, String>> {
        None
    }

    fn dst_tls<B>(
        &self,
        _: &http::Request<B>,
    ) -> Conditional<&identity::Name, tls::ReasonForNoPeerName> {
        Conditional::None(tls::ReasonForNoPeerName::Loopback)
    }

    fn route_labels<B>(&self, req: &http::Request<B>) -> Option<Arc<IndexMap<String, String>>> {
        req.extensions()
            .get::<dst::Route>()
            .map(|r| r.route.labels().clone())
    }

    fn is_outbound<B>(&self, _: &http::Request<B>) -> bool {
        false
    }
}

impl stack_tracing::GetSpan<()> for Target {
    fn get_span(&self, _: &()) -> tracing::Span {
        use tracing::debug_span;

        match self.http_version {
            http::Version::H2 => match self.dst.name_addr() {
                None => debug_span!("http2"),
                Some(name) => debug_span!("http2", %name),
            },
            http::Version::Http1 => match self.dst.name_addr() {
                None => debug_span!("http1"),
                Some(name) => debug_span!("http1", %name),
            },
        }
    }
}

// === impl RequestTarget ===

impl From<TcpAccept> for RequestTarget {
    fn from(accept: TcpAccept) -> Self {
        Self { accept }
    }
}

impl<A> svc::stack::RecognizeRoute<http::Request<A>> for RequestTarget {
    type Key = Target;

    fn recognize(&self, req: &http::Request<A>) -> Self::Key {
        let dst = req
            .headers()
            .get(CANONICAL_DST_HEADER)
            .and_then(|dst| {
                dst.to_str().ok().and_then(|d| {
                    Addr::from_str(d).ok().map(|a| {
                        debug!("using {}", CANONICAL_DST_HEADER);
                        a
                    })
                })
            })
            .or_else(|| {
                http_request_l5d_override_dst_addr(req)
                    .ok()
                    .map(|override_addr| {
                        debug!("using {}", DST_OVERRIDE_HEADER);
                        override_addr
                    })
            })
            .or_else(|| http_request_authority_addr(req).ok())
            .or_else(|| http_request_host_addr(req).ok())
            .unwrap_or_else(|| self.accept.target_addr.into());

        Target {
            dst,
            socket_addr: self.accept.target_addr,
            tls_client_id: self.accept.peer_id.clone(),
            http_version: req
                .version()
                .try_into()
                .expect("HTTP version must be valid"),
        }
    }
}

impl From<Logical> for Target {
    fn from(Logical { target, .. }: Logical) -> Self {
        target
    }
}

// === impl Logical ===

impl From<(Option<profiles::Receiver>, Target)> for Logical {
    fn from((profiles, target): (Option<profiles::Receiver>, Target)) -> Self {
        Self { profiles, target }
    }
}

impl Into<Option<profiles::Receiver>> for &'_ Logical {
    fn into(self) -> Option<profiles::Receiver> {
        self.profiles.clone()
    }
}
