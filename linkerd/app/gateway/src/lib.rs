#![deny(warnings, rust_2018_idioms)]

mod config;
mod gateway;
mod make;

pub use self::config::Config;

#[cfg(test)]
mod test {
    use super::*;
    use linkerd2_app_core::{
        dns,
        errors::HttpError,
        profiles,
        proxy::{http, identity},
        svc::NewService,
        transport::tls,
        Error, NameAddr,
    };
    use linkerd2_app_inbound::endpoint as inbound;
    use linkerd2_app_outbound::endpoint as outbound;
    use std::{convert::TryFrom, net::SocketAddr};
    use tokio::sync::watch;
    use tower::util::ServiceExt;
    use tower_test::mock;

    #[tokio::test]
    async fn gateway() {
        assert_eq!(
            Test::default().run().await.unwrap().status(),
            http::StatusCode::NO_CONTENT
        );
    }

    #[tokio::test]
    async fn bad_domain() {
        let test = Test {
            profile: None,
            ..Default::default()
        };
        let status = test
            .run()
            .await
            .unwrap_err()
            .downcast_ref::<HttpError>()
            .unwrap()
            .status();
        assert_eq!(status, http::StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn no_authority() {
        let test = Test {
            dst_name: None,
            profile: None,
            ..Default::default()
        };
        let status = test
            .run()
            .await
            .unwrap_err()
            .downcast_ref::<HttpError>()
            .unwrap()
            .status();
        assert_eq!(status, http::StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn no_identity() {
        let peer_id = tls::PeerIdentity::None(tls::ReasonForNoPeerName::NoPeerIdFromRemote);
        let test = Test {
            peer_id,
            ..Default::default()
        };
        let status = test
            .run()
            .await
            .unwrap_err()
            .downcast_ref::<HttpError>()
            .unwrap()
            .status();
        assert_eq!(status, http::StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn forward_loop() {
        let test = Test {
            orig_fwd: Some(
                "by=gateway.id.test;for=client.id.test;host=dst.test.example.com:4321;proto=https",
            ),
            ..Default::default()
        };
        let status = test
            .run()
            .await
            .unwrap_err()
            .downcast_ref::<HttpError>()
            .unwrap()
            .status();
        assert_eq!(status, http::StatusCode::LOOP_DETECTED);
    }

    struct Test {
        dst_name: Option<&'static str>,
        peer_id: tls::PeerIdentity,
        orig_fwd: Option<&'static str>,
        profile: Option<profiles::Receiver>,
    }

    impl Default for Test {
        fn default() -> Self {
            let (mut tx, rx) = watch::channel(profiles::Profile {
                name: dns::Name::try_from("dst.test.example.com".as_bytes()).ok(),
                ..profiles::Profile::default()
            });
            tokio::spawn(async move { tx.closed().await });
            Self {
                dst_name: Some("dst.test.example.com:4321"),
                peer_id: tls::PeerIdentity::Some(identity::Name::from(
                    dns::Name::try_from("client.id.test".as_bytes()).unwrap(),
                )),
                orig_fwd: None,
                profile: Some(rx),
            }
        }
    }

    impl Test {
        async fn run(self) -> Result<http::Response<http::boxed::Payload>, Error> {
            let Self {
                dst_name,
                peer_id,
                orig_fwd,
                profile,
            } = self;

            let (outbound, mut handle) = mock::pair::<
                http::Request<http::boxed::Payload>,
                http::Response<http::boxed::Payload>,
            >();
            let mut make_gateway = {
                make::MakeGateway::new(
                    move |_: outbound::HttpLogical| outbound.clone(),
                    ([127, 0, 0, 1], 4180).into(),
                    tls::PeerIdentity::Some(identity::Name::from(
                        dns::Name::try_from("gateway.id.test".as_bytes()).unwrap(),
                    )),
                )
            };

            let socket_addr = SocketAddr::from(([127, 0, 0, 1], 4143));
            let target = inbound::Target {
                socket_addr,
                dst: dst_name
                    .map(|n| NameAddr::from_str(n).unwrap().into())
                    .unwrap_or_else(|| socket_addr.into()),
                http_version: http::Version::Http1,
                tls_client_id: peer_id,
            };
            let gateway = make_gateway.new_service((profile, target));

            let bg = tokio::spawn(async move {
                handle.allow(1);
                let (req, rsp) = handle.next_request().await.unwrap();
                assert_eq!(
                    req.headers().get(http::header::FORWARDED).unwrap(),
                    "by=gateway.id.test;for=client.id.test;host=dst.test.example.com:4321;proto=https"
                );
                rsp.send_response(
                    http::Response::builder()
                        .status(http::StatusCode::NO_CONTENT)
                        .body(Default::default())
                        .unwrap(),
                );
            });

            let req = http::Request::builder()
                .uri(format!("http://{}", dst_name.unwrap_or("127.0.0.1:4321")));
            let req = orig_fwd
                .into_iter()
                .fold(req, |req, fwd| req.header(http::header::FORWARDED, fwd))
                .body(Default::default())
                .unwrap();
            let rsp = gateway.oneshot(req).await?;
            bg.await?;
            Ok(rsp)
        }
    }
}
