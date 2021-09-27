use std::sync::{atomic::AtomicBool, atomic::Ordering::Relaxed, Arc};
use std::{num::NonZeroU16, time::Duration};

use futures::{future::ok, FutureExt, SinkExt, StreamExt};
use ntex::codec::Framed;
use ntex::server;
use ntex::time::{sleep, Seconds};
use ntex::util::{poll_fn, ByteString, Bytes};

use ntex_mqtt::v3::{
    client, codec, ControlMessage, Handshake, HandshakeAck, MqttServer, Publish, Session,
};

struct St;

async fn handshake<Io>(mut packet: Handshake<Io>) -> Result<HandshakeAck<Io, St>, ()> {
    packet.packet();
    packet.packet_mut();
    packet.io();
    packet.sink();
    Ok(packet.ack(St, false).idle_timeout(Seconds(16)))
}

#[ntex::test]
async fn test_simple() -> std::io::Result<()> {
    let srv = server::test_server(|| MqttServer::new(handshake).publish(|_t| ok(())).finish());

    // connect to server
    let client =
        client::MqttConnector::new(srv.addr()).client_id("user").connect().await.unwrap();

    let sink = client.sink();

    ntex::rt::spawn(client.start_default());

    let res =
        sink.publish(ByteString::from_static("#"), Bytes::new()).send_at_least_once().await;
    assert!(res.is_ok());

    sink.close();
    Ok(())
}

#[ntex::test]
async fn test_connect_fail() -> std::io::Result<()> {
    // bad user name or password
    let srv = server::test_server(|| {
        MqttServer::new(|conn: Handshake<_>| ok::<_, ()>(conn.bad_username_or_pwd::<St>()))
            .publish(|_t| ok(()))
            .finish()
    });
    let err =
        client::MqttConnector::new(srv.addr()).client_id("user").connect().await.err().unwrap();
    if let client::ClientError::Ack { session_present, return_code } = err {
        assert!(!session_present);
        assert_eq!(return_code, codec::ConnectAckReason::BadUserNameOrPassword);
    }

    // identifier rejected
    let srv = server::test_server(|| {
        MqttServer::new(|conn: Handshake<_>| ok::<_, ()>(conn.identifier_rejected::<St>()))
            .publish(|_t| ok(()))
            .finish()
    });
    let err =
        client::MqttConnector::new(srv.addr()).client_id("user").connect().await.err().unwrap();
    if let client::ClientError::Ack { session_present, return_code } = err {
        assert!(!session_present);
        assert_eq!(return_code, codec::ConnectAckReason::IdentifierRejected);
    }

    // not authorized
    let srv = server::test_server(|| {
        MqttServer::new(|conn: Handshake<_>| ok::<_, ()>(conn.not_authorized::<St>()))
            .publish(|_t| ok(()))
            .finish()
    });
    let err =
        client::MqttConnector::new(srv.addr()).client_id("user").connect().await.err().unwrap();
    if let client::ClientError::Ack { session_present, return_code } = err {
        assert!(!session_present);
        assert_eq!(return_code, codec::ConnectAckReason::NotAuthorized);
    }

    // service unavailable
    let srv = server::test_server(|| {
        MqttServer::new(|conn: Handshake<_>| ok::<_, ()>(conn.service_unavailable::<St>()))
            .publish(|_t| ok(()))
            .finish()
    });
    let err =
        client::MqttConnector::new(srv.addr()).client_id("user").connect().await.err().unwrap();
    if let client::ClientError::Ack { session_present, return_code } = err {
        assert!(!session_present);
        assert_eq!(return_code, codec::ConnectAckReason::ServiceUnavailable);
    }

    Ok(())
}

#[ntex::test]
async fn test_ping() -> std::io::Result<()> {
    let ping = Arc::new(AtomicBool::new(false));
    let ping2 = ping.clone();

    let srv = server::test_server(move || {
        let ping = ping2.clone();
        MqttServer::new(handshake)
            .publish(|_| ok(()))
            .control(move |msg| {
                let ping = ping.clone();
                match msg {
                    ControlMessage::Ping(msg) => {
                        ping.store(true, Relaxed);
                        ok(msg.ack())
                    }
                    _ => ok(msg.disconnect()),
                }
            })
            .finish()
    });

    let io = srv.connect().await.unwrap();
    let mut framed = Framed::new(io, codec::Codec::default());
    framed
        .send(codec::Packet::Connect(codec::Connect::default().client_id("user").into()))
        .await
        .unwrap();
    framed.next().await.unwrap().unwrap();

    framed.send(codec::Packet::PingRequest).await.unwrap();
    let pkt = framed.next().await.unwrap().unwrap();
    assert_eq!(pkt, codec::Packet::PingResponse);
    assert!(ping.load(Relaxed));

    Ok(())
}

#[ntex::test]
async fn test_ack_order() -> std::io::Result<()> {
    let srv = server::test_server(move || {
        MqttServer::new(handshake)
            .publish(|_| sleep(Duration::from_millis(100)).map(|_| Ok::<_, ()>(())))
            .control(move |msg| match msg {
                ControlMessage::Subscribe(mut msg) => {
                    for mut sub in &mut msg {
                        assert_eq!(sub.qos(), codec::QoS::AtLeastOnce);
                        sub.topic();
                        sub.subscribe(codec::QoS::AtLeastOnce);
                    }
                    ok(msg.ack())
                }
                _ => ok(msg.disconnect()),
            })
            .finish()
    });

    let io = srv.connect().await.unwrap();
    let mut framed = Framed::new(io, codec::Codec::default());
    framed.send(codec::Connect::default().client_id("user").into()).await.unwrap();
    let _ = framed.next().await.unwrap().unwrap();

    framed
        .send(
            codec::Publish {
                dup: false,
                retain: false,
                qos: codec::QoS::AtLeastOnce,
                topic: ByteString::from("test"),
                packet_id: Some(NonZeroU16::new(1).unwrap()),
                payload: Bytes::new(),
            }
            .into(),
        )
        .await
        .unwrap();
    framed
        .send(codec::Packet::Subscribe {
            packet_id: NonZeroU16::new(2).unwrap(),
            topic_filters: vec![(ByteString::from("topic1"), codec::QoS::AtLeastOnce)],
        })
        .await
        .unwrap();
    framed
        .send(
            codec::Publish {
                dup: false,
                retain: false,
                qos: codec::QoS::AtLeastOnce,
                topic: ByteString::from("test"),
                packet_id: Some(NonZeroU16::new(3).unwrap()),
                payload: Bytes::new(),
            }
            .into(),
        )
        .await
        .unwrap();

    let pkt = framed.next().await.unwrap().unwrap();
    assert_eq!(pkt, codec::Packet::PublishAck { packet_id: NonZeroU16::new(1).unwrap() });

    let pkt = framed.next().await.unwrap().unwrap();
    assert_eq!(
        pkt,
        codec::Packet::SubscribeAck {
            packet_id: NonZeroU16::new(2).unwrap(),
            status: vec![codec::SubscribeReturnCode::Success(codec::QoS::AtLeastOnce)],
        }
    );

    let pkt = framed.next().await.unwrap().unwrap();
    assert_eq!(pkt, codec::Packet::PublishAck { packet_id: NonZeroU16::new(3).unwrap() });

    Ok(())
}

#[ntex::test]
async fn test_ack_order_sink() -> std::io::Result<()> {
    let srv = server::test_server(move || {
        MqttServer::new(handshake)
            .publish(|_| sleep(Duration::from_millis(100)).map(|_| Ok::<_, ()>(())))
            .finish()
    });

    // connect to server
    let client =
        client::MqttConnector::new(srv.addr()).client_id("user").connect().await.unwrap();
    let sink = client.sink();

    ntex::rt::spawn(client.start_default());

    let topic = ByteString::from_static("test");
    let fut1 = sink.publish(topic.clone(), Bytes::from_static(b"pkt1")).send_at_least_once();
    let fut2 = sink.publish(topic.clone(), Bytes::from_static(b"pkt2")).send_at_least_once();
    let fut3 = sink.publish(topic.clone(), Bytes::from_static(b"pkt3")).send_at_least_once();

    let (res1, res2, res3) = futures::future::join3(fut1, fut2, fut3).await;
    assert!(res1.is_ok());
    assert!(res2.is_ok());
    assert!(res3.is_ok());

    Ok(())
}

#[ntex::test]
async fn test_disconnect() -> std::io::Result<()> {
    let srv = server::test_server(|| {
        MqttServer::new(handshake)
            .publish(ntex::service::fn_factory_with_config(|session: Session<St>| {
                ok(ntex::service::fn_service(move |_: Publish| {
                    session.sink().force_close();
                    async {
                        sleep(Duration::from_millis(100)).await;
                        Ok(())
                    }
                }))
            }))
            .finish()
    });

    // connect to server
    let client =
        client::MqttConnector::new(srv.addr()).client_id("user").connect().await.unwrap();

    let sink = client.sink();

    ntex::rt::spawn(client.start_default());

    let res =
        sink.publish(ByteString::from_static("#"), Bytes::new()).send_at_least_once().await;
    assert!(res.is_err());

    Ok(())
}

#[ntex::test]
async fn test_handle_incoming() -> std::io::Result<()> {
    let publish = Arc::new(AtomicBool::new(false));
    let publish2 = publish.clone();
    let disconnect = Arc::new(AtomicBool::new(false));
    let disconnect2 = disconnect.clone();

    let srv = server::test_server(move || {
        let publish = publish2.clone();
        let disconnect = disconnect2.clone();
        MqttServer::new(handshake)
            .publish(move |_| {
                publish.store(true, Relaxed);
                async {
                    sleep(Duration::from_millis(100)).await;
                    Ok(())
                }
            })
            .control(move |msg| match msg {
                ControlMessage::Disconnect(msg) => {
                    disconnect.store(true, Relaxed);
                    ok(msg.ack())
                }
                _ => ok(msg.disconnect()),
            })
            .finish()
    });

    let io = srv.connect().await.unwrap();
    let mut framed = Framed::new(io, codec::Codec::default());
    framed.write(codec::Connect::default().client_id("user").into()).unwrap();
    framed
        .write(
            codec::Publish {
                dup: false,
                retain: false,
                qos: codec::QoS::AtLeastOnce,
                topic: ByteString::from("test"),
                packet_id: Some(NonZeroU16::new(3).unwrap()),
                payload: Bytes::new(),
            }
            .into(),
        )
        .unwrap();
    framed.write(codec::Packet::Disconnect).unwrap();
    poll_fn(|cx| framed.flush(cx)).await.unwrap();
    drop(framed);
    sleep(Duration::from_millis(500)).await;

    assert!(publish.load(Relaxed));
    assert!(disconnect.load(Relaxed));

    Ok(())
}
