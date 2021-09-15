use std::marker::PhantomData;
use std::task::{Context, Poll};

use ntex::service::{Service, ServiceFactory};
use ntex::util::Ready;

use super::control::{ControlMessage, ControlResult};
use super::publish::PublishMessage;
use super::Session;

/// Default publish service
pub struct DefaultPublishService<St, Err> {
    _t: PhantomData<(St, Err)>,
}

impl<St, Err> Default for DefaultPublishService<St, Err> {
    fn default() -> Self {
        Self { _t: PhantomData }
    }
}

impl<St, Err> ServiceFactory for DefaultPublishService<St, Err> {
    type Config = Session<St>;
    type Request = PublishMessage;
    type Response = ();
    type Error = Err;
    type Service = DefaultPublishService<St, Err>;
    type InitError = Err;
    type Future = Ready<Self::Service, Self::InitError>;

    fn new_service(&self, _: Session<St>) -> Self::Future {
        Ready::Ok(DefaultPublishService { _t: PhantomData })
    }
}

impl<St, Err> Service for DefaultPublishService<St, Err> {
    type Request = PublishMessage;
    type Response = ();
    type Error = Err;
    type Future = Ready<Self::Response, Self::Error>;

    fn poll_ready(&self, _: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&self, _: PublishMessage) -> Self::Future {
        log::warn!("Publish service is disabled");
        Ready::Ok(())
    }
}

/// Default control service
pub struct DefaultControlService<S, E>(PhantomData<(S, E)>);

impl<S, E> Default for DefaultControlService<S, E> {
    fn default() -> Self {
        DefaultControlService(PhantomData)
    }
}

impl<S, E> ServiceFactory for DefaultControlService<S, E> {
    type Config = Session<S>;
    type Request = ControlMessage;
    type Response = ControlResult;
    type Error = E;
    type InitError = E;
    type Service = DefaultControlService<S, E>;
    type Future = Ready<Self::Service, Self::InitError>;

    fn new_service(&self, _: Session<S>) -> Self::Future {
        Ready::Ok(DefaultControlService(PhantomData))
    }
}

impl<S, E> Service for DefaultControlService<S, E> {
    type Request = ControlMessage;
    type Response = ControlResult;
    type Error = E;
    type Future = Ready<Self::Response, Self::Error>;

    #[inline]
    fn poll_ready(&self, _: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    #[inline]
    fn call(&self, subs: ControlMessage) -> Self::Future {
        log::warn!("MQTT Subscribe is not supported");

        Ready::Ok(match subs {
            ControlMessage::Ping(ping) => ping.ack(),
            ControlMessage::Disconnect(disc) => disc.ack(),
            ControlMessage::Subscribe(subs) => {
                log::warn!("MQTT Subscribe is not supported");
                subs.ack()
            }
            ControlMessage::Unsubscribe(unsubs) => {
                log::warn!("MQTT Unsubscribe is not supported");
                unsubs.ack()
            }
            ControlMessage::Closed(msg) => msg.ack(),
        })
    }
}
