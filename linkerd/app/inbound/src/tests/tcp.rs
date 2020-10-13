use super::*;
use crate::TcpEndpoint;
use linkerd2_app_core::{
    drain, metrics, proxy, svc,
    transport::{listen, tls},
    Addr, Error, NameAddr,
};
use linkerd2_app_test as test_support;
use std::{net::SocketAddr, time::Duration};
use test_support::{connect::Connect, resolver};
use tls::HasPeerIdentity;
use tracing_futures::Instrument;

#[tokio::test(core_threads = 1)]
async fn plaintext_tcp() {
    let _trace = test_support::trace_init();

    // Since all of the actual IO in this test is mocked out, we won't actually
    // bind any of these addresses. Therefore, we don't need to use ephemeral
    // ports or anything. These will just be used so that the proxy has a socket
    // address to resolve, etc.
    let target_addr = SocketAddr::new([127, 0, 0, 1].into(), 666);
    let concrete = TcpConcrete {
        logical: TcpLogical {
            addr: target_addr,
            profile: Some(profile()),
        },
        resolve: Some(target_addr.into()),
    };

    let cfg = default_config(target_addr);

    // Configure mock IO for the upstream "server". It will read "hello" and
    // then write "world".
    let mut srv_io = test_support::io();
    srv_io.read(b"hello").write(b"world");
    // Build a mock "connector" that returns the upstream "server" IO.
    let connect = test_support::connect().endpoint_builder(target_addr, srv_io);

    // Configure mock IO for the "client".
    let client_io = test_support::io().write(b"hello").read(b"world").build();

    // Configure the mock destination resolver to just give us a single endpoint
    // for the target, which always exists and has no metadata.
    let resolver = test_support::resolver().endpoint_exists(
        concrete.resolve.clone().unwrap(),
        target_addr,
        test_support::resolver::Metadata::default(),
    );

    // Build the outbound TCP balancer stack.
    let forward = cfg
        .build_tcp_balance(connect, resolver)
        .new_service(concrete);

    forward
        .oneshot(client_io)
        .err_into::<Error>()
        .await
        .expect("conn should succeed");
}

#[tokio::test(core_threads = 1)]
async fn tls_when_hinted() {
    let _trace = test_support::trace_init();

    let tls_addr = SocketAddr::new([0, 0, 0, 0].into(), 5550);
    let tls_concrete = TcpConcrete {
        logical: TcpLogical {
            addr: tls_addr,
            profile: Some(profile()),
        },
        resolve: Some(tls_addr.into()),
    };

    let plain_addr = SocketAddr::new([0, 0, 0, 0].into(), 5551);
    let plain_concrete = TcpConcrete {
        logical: TcpLogical {
            addr: plain_addr,
            profile: Some(profile()),
        },
        resolve: Some(plain_addr.into()),
    };

    let cfg = default_config(plain_addr);
    let id_name = linkerd2_identity::Name::from_hostname(
        b"foo.ns1.serviceaccount.identity.linkerd.cluster.local",
    )
    .expect("hostname is valid");
    let mut srv_io = test_support::io();
    srv_io.write(b"hello").read(b"world");
    let id_name2 = id_name.clone();

    // Build a mock "connector" that returns the upstream "server" IO.
    let connect = test_support::connect()
        // The plaintext endpoint should use plaintext...
        .endpoint_builder(plain_addr, srv_io.clone())
        .endpoint_fn(tls_addr, move |endpoint: TcpEndpoint| {
            assert_eq!(
                endpoint.peer_identity(),
                tls::Conditional::Some(id_name2.clone())
            );
            let io = srv_io.build();
            Ok(io)
        });

    let tls_meta = test_support::resolver::Metadata::new(
        Default::default(),
        test_support::resolver::ProtocolHint::Unknown,
        Some(id_name),
        10_000,
        None,
    );

    // Configure the mock destination resolver to just give us a single endpoint
    // for the target, which always exists and has no metadata.
    let resolver = test_support::resolver()
        .endpoint_exists(
            plain_concrete.resolve.clone().unwrap(),
            plain_addr,
            test_support::resolver::Metadata::default(),
        )
        .endpoint_exists(tls_concrete.resolve.clone().unwrap(), tls_addr, tls_meta);

    // Configure mock IO for the "client".
    let mut client_io = test_support::io();
    client_io.read(b"hello").write(b"world");

    // Build the outbound TCP balancer stack.
    let mut balance = cfg.build_tcp_balance(connect, resolver);

    let plain = balance
        .new_service(plain_concrete)
        .oneshot(client_io.build())
        .err_into::<Error>();
    let tls = balance
        .new_service(tls_concrete)
        .oneshot(client_io.build())
        .err_into::<Error>();

    let res = tokio::try_join! { plain, tls };
    res.expect("neither connection should fail");
}

#[tokio::test]
async fn resolutions_are_reused() {
    let _trace = test_support::trace_init();

    let addr = SocketAddr::new([0, 0, 0, 0].into(), 5550);
    let cfg = default_config(addr);
    let id_name = linkerd2_identity::Name::from_hostname(
        b"foo.ns1.serviceaccount.identity.linkerd.cluster.local",
    )
    .expect("hostname is valid");
    let id_name2 = id_name.clone();

    // Build a mock "connector" that returns the upstream "server" IO.
    let connect = test_support::connect().endpoint_fn(addr, move |endpoint: TcpEndpoint| {
        assert_eq!(
            endpoint.peer_identity(),
            tls::Conditional::Some(id_name2.clone())
        );
        let io = test_support::io()
            .write(b"hello")
            .read(b"world")
            .read_error(std::io::ErrorKind::ConnectionReset.into())
            .build();
        Ok(io)
    });

    let meta = test_support::resolver::Metadata::new(
        Default::default(),
        test_support::resolver::ProtocolHint::Unknown,
        Some(id_name),
        10_000,
        None,
    );

    // Configure the mock destination resolver to just give us a single endpoint
    // for the target, which always exists and has no metadata.
    let resolver = test_support::resolver().endpoint_exists(Addr::from(addr), addr, meta);
    let resolve_state = resolver.handle();

    let profiles = test_support::profiles().profile(addr, Default::default());
    let profile_state = profiles.handle();

    // Build the outbound server
    let mut server = build_server(cfg, profiles, resolver, connect);

    let conns = (0..10)
        .map(|number| {
            tokio::spawn(
                hello_world_client(addr, &mut server)
                    .instrument(tracing::trace_span!("conn", number)),
            )
            .err_into::<Error>()
        })
        .collect::<Vec<_>>();

    for (i, res) in futures::future::join_all(conns).await.drain(..).enumerate() {
        if let Err(e) = res {
            panic!("task {} panicked: {:?}", i, e);
        }
    }
    assert!(resolve_state.is_empty());
    assert!(
        resolve_state.only_configured(),
        "destinations were resolved multiple times for the same address!"
    );
    assert!(profile_state.is_empty());
    assert!(
        profile_state.only_configured(),
        "profiles were resolved multiple times for the same address!"
    );
}

fn build_server<I>(
    cfg: Config,
    profiles: resolver::Profiles<NameAddr>,
    connect: Connect<SocketAddr>,
) -> impl svc::NewService<
    listen::Addrs,
    Service = impl tower::Service<
        I,
        Response = (),
        Error = impl Into<Error>,
        Future = impl Send + 'static,
    > + Send
                  + 'static,
> + Send
       + 'static
where
    I: tokio::io::AsyncRead + tokio::io::AsyncWrite + std::fmt::Debug + Unpin + Send + 'static,
{
    let (metrics, _) = metrics::Metrics::new(Duration::from_secs(10));
    let (_, drain) = drain::channel();
    let (_, tap, _) = proxy::tap::new();
    cfg.build_server(
        connect,
        crate::PreventLoop::from(4143),
        tls::Conditional::None(tls::ReasonForNoPeerName::LocalIdentityDisabled), // XXX(eliza): add local identity!
        test_support::service::no_http(),
        profiles,
        tap,
        metrics.inbound,
        None,
        drain,
    )
}

fn hello_world_client<N, S>(
    orig_dst: SocketAddr,
    new_svc: &mut N,
) -> impl Future<Output = ()> + Send
where
    N: svc::NewService<listen::Addrs, Service = S> + Send + 'static,
    S: svc::Service<test_support::io::Mock, Response = ()> + Send + 'static,
    S::Error: Into<Error>,
    S::Future: Send + 'static,
{
    let span = tracing::info_span!("hello_world_client", %orig_dst);
    let svc = {
        let _e = span.enter();
        let addrs = listen::Addrs::new(
            ([127, 0, 0, 1], 4140).into(),
            ([127, 0, 0, 1], 666).into(),
            Some(orig_dst),
        );
        let svc = new_svc.new_service(addrs);
        tracing::debug!("new service");
        svc
    };
    async move {
        let io = test_support::io()
            .read(b"hello\r\n")
            .write(b"world")
            .build();
        let res = svc.oneshot(io).err_into::<Error>().await;
        tracing::debug!(?res);
        if let Err(err) = res {
            if let Some(err) = err.downcast_ref::<std::io::Error>() {
                // did the pretend server hang up, or did something
                // actually unexpected happen?
                assert_eq!(err.kind(), std::io::ErrorKind::ConnectionReset);
            } else {
                panic!("connection failed unexpectedly: {}", err)
            }
        }
    }
    .instrument(span)
}
