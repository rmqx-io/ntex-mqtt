use std::{fmt, marker::PhantomData, pin::Pin, rc::Rc, time::Duration};

use ntex::codec::{AsyncRead, AsyncWrite};
use ntex::rt::time::Sleep;
use ntex::service::{apply_fn_factory, IntoServiceFactory, Service, ServiceFactory};
use ntex::util::{timeout::Timeout, timeout::TimeoutError, Either, Ready};

use crate::error::{MqttError, ProtocolError};
use crate::io::{DispatchItem, State};
use crate::service::{FactoryBuilder, FactoryBuilder2};

use super::codec as mqtt;
use super::control::{ControlMessage, ControlResult};
use super::default::{DefaultControlService, DefaultPublishService};
use super::dispatcher::factory;
use super::handshake::{Handshake, HandshakeAck};
use super::publish::PublishMessage;
use super::shared::{MqttShared, MqttSinkPool};
use super::sink::MqttSink;
use super::Session;

/// Mqtt v3.1.1 Server
pub struct MqttServer<Io, St, C: ServiceFactory, Cn: ServiceFactory, P: ServiceFactory> {
    handshake: C,
    control: Cn,
    publish: P,
    max_size: u32,
    inflight: usize,
    handshake_timeout: u16,
    disconnect_timeout: u16,
    max_awaiting_rel: usize,
    await_rel_timeout: Duration,
    pool: Rc<MqttSinkPool>,
    _t: PhantomData<(Io, St)>,
}

impl<Io, St, C>
    MqttServer<
        Io,
        St,
        C,
        DefaultControlService<St, C::Error>,
        DefaultPublishService<St, C::Error>,
    >
where
    St: 'static,
    C: ServiceFactory<Config = (), Request = Handshake<Io>, Response = HandshakeAck<Io, St>>
        + 'static,
    C::Error: fmt::Debug,
{
    /// Create server factory and provide handshake service
    pub fn new<F>(handshake: F) -> Self
    where
        F: IntoServiceFactory<C>,
    {
        MqttServer {
            handshake: handshake.into_factory(),
            control: DefaultControlService::default(),
            publish: DefaultPublishService::default(),
            max_size: 0,
            inflight: 16,
            handshake_timeout: 0,
            disconnect_timeout: 3000,
            max_awaiting_rel: 0,
            await_rel_timeout: Duration::default(),
            pool: Default::default(),
            _t: PhantomData,
        }
    }
}

impl<Io, St, C, Cn, P> MqttServer<Io, St, C, Cn, P>
where
    Io: AsyncRead + AsyncWrite + Unpin + 'static,
    St: 'static,
    C: ServiceFactory<Config = (), Request = Handshake<Io>, Response = HandshakeAck<Io, St>>
        + 'static,
    Cn: ServiceFactory<Config = Session<St>, Request = ControlMessage, Response = ControlResult>
        + 'static,
    P: ServiceFactory<Config = Session<St>, Request = PublishMessage, Response = ()> + 'static,
    C::Error: From<Cn::Error>
        + From<Cn::InitError>
        + From<P::Error>
        + From<P::InitError>
        + fmt::Debug,
{
    /// Set handshake timeout in millis.
    ///
    /// Handshake includes `connect` packet and response `connect-ack`.
    /// By default handshake timeuot is disabled.
    pub fn handshake_timeout(mut self, timeout: u16) -> Self {
        self.handshake_timeout = timeout;
        self
    }

    /// Set server connection disconnect timeout in milliseconds.
    ///
    /// Defines a timeout for disconnect connection. If a disconnect procedure does not complete
    /// within this time, the connection get dropped.
    ///
    /// To disable timeout set value to 0.
    ///
    /// By default disconnect timeout is set to 3 seconds.
    pub fn disconnect_timeout(mut self, val: u16) -> Self {
        self.disconnect_timeout = val;
        self
    }

    /// Set max inbound frame size.
    ///
    /// If max size is set to `0`, size is unlimited.
    /// By default max size is set to `0`
    pub fn max_size(mut self, size: u32) -> Self {
        self.max_size = size;
        self
    }

    /// Number of in-flight concurrent messages.
    ///
    /// By default in-flight is set to 16 messages
    pub fn inflight(mut self, val: usize) -> Self {
        self.inflight = val;
        self
    }

    ///
    pub fn max_awaiting_rel(mut self, val: usize) -> Self {
        self.max_awaiting_rel = val;
        self
    }

    ///
    pub fn await_rel_timeout(mut self, val: Duration) -> Self {
        self.await_rel_timeout = val;
        self
    }

    /// Service to handle control packets
    ///
    /// All control packets are processed sequentially, max buffered
    /// control packets is 16.
    pub fn control<F, Srv>(self, service: F) -> MqttServer<Io, St, C, Srv, P>
    where
        F: IntoServiceFactory<Srv>,
        Srv: ServiceFactory<
                Config = Session<St>,
                Request = ControlMessage,
                Response = ControlResult,
            > + 'static,
        C::Error: From<Srv::Error> + From<Srv::InitError>,
    {
        MqttServer {
            handshake: self.handshake,
            publish: self.publish,
            control: service.into_factory(),
            max_size: self.max_size,
            inflight: self.inflight,
            handshake_timeout: self.handshake_timeout,
            disconnect_timeout: self.disconnect_timeout,
            max_awaiting_rel: self.max_awaiting_rel,
            await_rel_timeout: self.await_rel_timeout,
            pool: self.pool,
            _t: PhantomData,
        }
    }

    /// Set service to handle publish packets and create mqtt server factory
    pub fn publish<F, Srv>(self, publish: F) -> MqttServer<Io, St, C, Cn, Srv>
    where
        F: IntoServiceFactory<Srv> + 'static,
        Srv: ServiceFactory<Config = Session<St>, Request = PublishMessage, Response = ()>
            + 'static,
        C::Error: From<Srv::Error> + From<Srv::InitError> + fmt::Debug,
    {
        MqttServer {
            handshake: self.handshake,
            publish: publish.into_factory(),
            control: self.control,
            max_size: self.max_size,
            inflight: self.inflight,
            handshake_timeout: self.handshake_timeout,
            disconnect_timeout: self.disconnect_timeout,
            max_awaiting_rel: self.max_awaiting_rel,
            await_rel_timeout: self.await_rel_timeout,
            pool: self.pool,
            _t: PhantomData,
        }
    }

    /// Set service to handle publish packets and create mqtt server factory
    pub fn finish(
        self,
    ) -> impl ServiceFactory<Config = (), Request = Io, Response = (), Error = MqttError<C::Error>>
    {
        let handshake = self.handshake;
        let publish = self
            .publish
            .into_factory()
            .map_err(|e| MqttError::Service(e.into()))
            .map_init_err(|e| MqttError::Service(e.into()));
        let control = self
            .control
            .map_err(|e| MqttError::Service(e.into()))
            .map_init_err(|e| MqttError::Service(e.into()));

        ntex::unit_config(
            FactoryBuilder::new(handshake_service_factory(
                handshake,
                self.max_size,
                self.handshake_timeout,
                self.pool,
            ))
            .disconnect_timeout(self.disconnect_timeout)
            .build(apply_fn_factory(
                factory(
                    publish,
                    control,
                    self.inflight,
                    self.max_awaiting_rel,
                    self.await_rel_timeout,
                ),
                |req: DispatchItem<Rc<MqttShared>>, srv| match req {
                    DispatchItem::Item(req) => Either::Left(srv.call(req)),
                    DispatchItem::KeepAliveTimeout => Either::Right(Ready::Err(
                        MqttError::Protocol(ProtocolError::KeepAliveTimeout),
                    )),
                    DispatchItem::EncoderError(e) => {
                        Either::Right(Ready::Err(MqttError::Protocol(ProtocolError::Encode(e))))
                    }
                    DispatchItem::DecoderError(e) => {
                        Either::Right(Ready::Err(MqttError::Protocol(ProtocolError::Decode(e))))
                    }
                    DispatchItem::IoError(e) => {
                        Either::Right(Ready::Err(MqttError::Protocol(ProtocolError::Io(e))))
                    }
                    DispatchItem::WBackPressureEnabled
                    | DispatchItem::WBackPressureDisabled => Either::Right(Ready::Ok(None)),
                },
            )),
        )
    }

    /// Set service to handle publish packets and create mqtt server factory
    pub(crate) fn inner_finish(
        self,
    ) -> impl ServiceFactory<
        Config = (),
        Request = (Io, State, Option<Pin<Box<Sleep>>>),
        Response = (),
        Error = MqttError<C::Error>,
        InitError = C::InitError,
    > {
        let handshake = self.handshake;
        let publish = self
            .publish
            .into_factory()
            .map_err(|e| MqttError::Service(e.into()))
            .map_init_err(|e| MqttError::Service(e.into()));
        let control = self
            .control
            .map_err(|e| MqttError::Service(e.into()))
            .map_init_err(|e| MqttError::Service(e.into()));

        ntex::unit_config(
            FactoryBuilder2::new(handshake_service_factory2(
                handshake,
                self.max_size,
                self.handshake_timeout,
                self.pool,
            ))
            .disconnect_timeout(self.disconnect_timeout)
            .build(apply_fn_factory(
                factory(
                    publish,
                    control,
                    self.inflight,
                    self.max_awaiting_rel,
                    self.await_rel_timeout,
                ),
                |req: DispatchItem<Rc<MqttShared>>, srv| match req {
                    DispatchItem::Item(req) => Either::Left(srv.call(req)),
                    DispatchItem::KeepAliveTimeout => Either::Right(Ready::Err(
                        MqttError::Protocol(ProtocolError::KeepAliveTimeout),
                    )),
                    DispatchItem::EncoderError(e) => {
                        Either::Right(Ready::Err(MqttError::Protocol(ProtocolError::Encode(e))))
                    }
                    DispatchItem::DecoderError(e) => {
                        Either::Right(Ready::Err(MqttError::Protocol(ProtocolError::Decode(e))))
                    }
                    DispatchItem::IoError(e) => {
                        Either::Right(Ready::Err(MqttError::Protocol(ProtocolError::Io(e))))
                    }
                    DispatchItem::WBackPressureEnabled
                    | DispatchItem::WBackPressureDisabled => Either::Right(Ready::Ok(None)),
                },
            )),
        )
    }
}

fn handshake_service_factory<Io, St, C>(
    factory: C,
    max_size: u32,
    handshake_timeout: u16,
    pool: Rc<MqttSinkPool>,
) -> impl ServiceFactory<
    Config = (),
    Request = Io,
    Response = (Io, State, Rc<MqttShared>, Session<St>, u16),
    Error = MqttError<C::Error>,
>
where
    Io: AsyncRead + AsyncWrite + Unpin,
    C: ServiceFactory<Config = (), Request = Handshake<Io>, Response = HandshakeAck<Io, St>>,
    C::Error: fmt::Debug,
{
    ntex::apply(
        Timeout::new(Duration::from_millis(handshake_timeout as u64)),
        ntex::fn_factory(move || {
            let pool = pool.clone();
            let fut = factory.new_service(());
            async move {
                let service = fut.await?;
                let pool = pool.clone();
                let service = Rc::new(service.map_err(MqttError::Service));
                Ok::<_, C::InitError>(ntex::apply_fn(service, move |conn: Io, service| {
                    handshake(conn, None, service.clone(), max_size, pool.clone())
                }))
            }
        }),
    )
    .map_err(|e| match e {
        TimeoutError::Service(e) => e,
        TimeoutError::Timeout => MqttError::HandshakeTimeout,
    })
}

fn handshake_service_factory2<Io, St, C>(
    factory: C,
    max_size: u32,
    handshake_timeout: u16,
    pool: Rc<MqttSinkPool>,
) -> impl ServiceFactory<
    Config = (),
    Request = (Io, State),
    Response = (Io, State, Rc<MqttShared>, Session<St>, u16),
    Error = MqttError<C::Error>,
    InitError = C::InitError,
>
where
    Io: AsyncRead + AsyncWrite + Unpin,
    C: ServiceFactory<Config = (), Request = Handshake<Io>, Response = HandshakeAck<Io, St>>,
    C::Error: fmt::Debug,
{
    ntex::apply(
        Timeout::new(Duration::from_millis(handshake_timeout as u64)),
        ntex::fn_factory(move || {
            let pool = pool.clone();
            let fut = factory.new_service(());
            async move {
                let service = fut.await?;
                let pool = pool.clone();
                let service = Rc::new(service.map_err(MqttError::Service));
                Ok(ntex::apply_fn(service, move |(io, state), service| {
                    handshake(io, Some(state), service.clone(), max_size, pool.clone())
                }))
            }
        }),
    )
    .map_err(|e| match e {
        TimeoutError::Service(e) => e,
        TimeoutError::Timeout => MqttError::HandshakeTimeout,
    })
}

async fn handshake<Io, S, St, E>(
    mut io: Io,
    state: Option<State>,
    service: S,
    max_size: u32,
    pool: Rc<MqttSinkPool>,
) -> Result<(Io, State, Rc<MqttShared>, Session<St>, u16), S::Error>
where
    Io: AsyncRead + AsyncWrite + Unpin,
    S: Service<Request = Handshake<Io>, Response = HandshakeAck<Io, St>, Error = MqttError<E>>,
{
    log::trace!("Starting mqtt handshake");

    let state = state.unwrap_or_else(State::new);
    let shared = Rc::new(MqttShared::new(
        state.clone(),
        mqtt::Codec::default().max_size(max_size),
        16,
        pool,
    ));

    // read first packet
    let packet = state
        .next(&mut io, &shared.codec)
        .await
        .map_err(|err| {
            log::trace!("Error is received during mqtt handshake: {:?}", err);
            MqttError::from(err)
        })
        .and_then(|res| {
            res.ok_or_else(|| {
                log::trace!("Server mqtt is disconnected during handshake");
                MqttError::Disconnected
            })
        })?;

    match packet {
        mqtt::Packet::Connect(connect) => {
            // authenticate mqtt connection
            let mut ack = service.call(Handshake::new(connect, io, shared)).await?;

            match ack.session {
                Some(session) => {
                    let pkt = mqtt::Packet::ConnectAck {
                        session_present: ack.session_present,
                        return_code: mqtt::ConnectAckReason::ConnectionAccepted,
                    };

                    log::trace!("Sending success handshake ack: {:#?}", pkt);

                    state.set_buffer_params(ack.read_hw, ack.write_hw, ack.lw);
                    state.send(&mut ack.io, &ack.shared.codec, pkt).await?;

                    Ok((
                        ack.io,
                        ack.shared.state.clone(),
                        ack.shared.clone(),
                        Session::new(session, MqttSink::new(ack.shared)),
                        ack.keepalive,
                    ))
                }
                None => {
                    let pkt = mqtt::Packet::ConnectAck {
                        session_present: false,
                        return_code: ack.return_code,
                    };

                    log::trace!("Sending failed handshake ack: {:#?}", pkt);
                    ack.shared.state.send(&mut ack.io, &ack.shared.codec, pkt).await?;

                    Err(MqttError::Disconnected)
                }
            }
        }
        packet => {
            log::info!("MQTT-3.1.0-1: Expected CONNECT packet, received {:?}", packet);
            Err(MqttError::Protocol(ProtocolError::Unexpected(
                packet.packet_type(),
                "MQTT-3.1.0-1: Expected CONNECT packet",
            )))
        }
    }
}
