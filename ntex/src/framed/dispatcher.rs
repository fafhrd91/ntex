//! Framed transport dispatcher
use std::task::{Context, Poll};
use std::{cell::Cell, cell::RefCell, pin::Pin, rc::Rc, time::Duration, time::Instant};

use either::Either;
use futures::{Future, FutureExt};

use crate::codec::{AsyncRead, AsyncWrite, Decoder, Encoder};
use crate::framed::{DispatchItem, ReadTask, State, Timer, WriteTask};
use crate::service::{IntoService, Service};

type Response<U> = <U as Encoder>::Item;

pin_project_lite::pin_project! {
    /// Framed dispatcher - is a future that reads frames from Framed object
    /// and pass then to the service.
    pub struct Dispatcher<S, U>
    where
        S: Service<Request = DispatchItem<U>, Response = Option<Response<U>>>,
        S::Error: 'static,
        S::Future: 'static,
        U: Encoder,
        U: Decoder,
       <U as Encoder>::Item: 'static,
    {
        service: S,
        inner: DispatcherInner<S, U>,
        #[pin]
        fut: Option<S::Future>,
    }
}

struct DispatcherInner<S, U>
where
    S: Service<Request = DispatchItem<U>, Response = Option<Response<U>>>,
    U: Encoder + Decoder,
{
    st: Cell<DispatcherState>,
    state: State,
    timer: Timer,
    ka_timeout: u16,
    ka_updated: Cell<Instant>,
    error: Cell<Option<S::Error>>,
    shared: Rc<DispatcherShared<S, U>>,
}

struct DispatcherShared<S, U>
where
    S: Service<Request = DispatchItem<U>, Response = Option<Response<U>>>,
    U: Encoder + Decoder,
{
    codec: U,
    error: Cell<Option<DispatcherError<S::Error, <U as Encoder>::Error>>>,
    inflight: Cell<usize>,
}

#[derive(Copy, Clone, Debug)]
enum DispatcherState {
    Processing,
    //WrEnable,
    //WrEnabled,
    Stop,
    Shutdown,
}

enum DispatcherError<S, U> {
    KeepAlive,
    Encoder(U),
    Service(S),
}

enum PollService<U: Encoder + Decoder> {
    Item(DispatchItem<U>),
    ServiceError,
    Pending,
    Ready,
}

impl<S, U> From<Either<S, U>> for DispatcherError<S, U> {
    fn from(err: Either<S, U>) -> Self {
        match err {
            Either::Left(err) => DispatcherError::Service(err),
            Either::Right(err) => DispatcherError::Encoder(err),
        }
    }
}

impl<S, U> Dispatcher<S, U>
where
    S: Service<Request = DispatchItem<U>, Response = Option<Response<U>>> + 'static,
    U: Decoder + Encoder + 'static,
    <U as Encoder>::Item: 'static,
{
    /// Construct new `Dispatcher` instance.
    pub fn new<T, F: IntoService<S>>(
        io: T,
        codec: U,
        state: State,
        service: F,
        timer: Timer,
    ) -> Self
    where
        T: AsyncRead + AsyncWrite + Unpin + 'static,
    {
        let io = Rc::new(RefCell::new(io));

        // start support tasks
        crate::rt::spawn(ReadTask::new(io.clone(), state.clone()));
        crate::rt::spawn(WriteTask::new(io, state.clone()));

        Self::from_state(codec, state, service, timer)
    }

    /// Construct new `Dispatcher` instance.
    pub fn from_state<F: IntoService<S>>(
        codec: U,
        state: State,
        service: F,
        timer: Timer,
    ) -> Self {
        let updated = timer.now();
        let ka_timeout: u16 = 30;

        // register keepalive timer
        let expire = updated + Duration::from_secs(ka_timeout as u64);
        timer.register(expire, expire, &state);

        Dispatcher {
            service: service.into_service(),
            fut: None,
            inner: DispatcherInner {
                state,
                timer,
                ka_timeout,
                ka_updated: Cell::new(updated),
                error: Cell::new(None),
                st: Cell::new(DispatcherState::Processing),
                shared: Rc::new(DispatcherShared {
                    codec,
                    error: Cell::new(None),
                    inflight: Cell::new(0),
                }),
            },
        }
    }

    /// Set keep-alive timeout in seconds.
    ///
    /// To disable timeout set value to 0.
    ///
    /// By default keep-alive timeout is set to 30 seconds.
    pub fn keepalive_timeout(mut self, timeout: u16) -> Self {
        // register keepalive timer
        let prev = self.inner.ka_updated.get() + self.inner.ka();
        if timeout == 0 {
            self.inner.timer.unregister(prev, &self.inner.state);
        } else {
            let expire =
                self.inner.ka_updated.get() + Duration::from_secs(timeout as u64);
            self.inner.timer.register(expire, prev, &self.inner.state);
        }
        self.inner.ka_timeout = timeout;

        self
    }

    /// Set connection disconnect timeout in milliseconds.
    ///
    /// Defines a timeout for disconnect connection. If a disconnect procedure does not complete
    /// within this time, the connection get dropped.
    ///
    /// To disable timeout set value to 0.
    ///
    /// By default disconnect timeout is set to 1 seconds.
    pub fn disconnect_timeout(self, val: u16) -> Self {
        self.inner.state.set_disconnect_timeout(val);
        self
    }
}

impl<S, U> DispatcherShared<S, U>
where
    S: Service<Request = DispatchItem<U>, Response = Option<Response<U>>>,
    S::Error: 'static,
    S::Future: 'static,
    U: Encoder + Decoder,
    <U as Encoder>::Item: 'static,
{
    fn handle_result(
        &self,
        item: Result<S::Response, S::Error>,
        state: &State,
        wake: bool,
    ) {
        self.inflight.set(self.inflight.get() - 1);
        if let Err(err) = state.write_result(item, &self.codec) {
            self.error.set(Some(err.into()));
        }

        if wake {
            state.dsp_wake_task()
        }
    }
}

impl<S, U> Future for Dispatcher<S, U>
where
    S: Service<Request = DispatchItem<U>, Response = Option<Response<U>>> + 'static,
    U: Decoder + Encoder + 'static,
    <U as Encoder>::Item: 'static,
{
    type Output = Result<(), S::Error>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let mut this = self.as_mut().project();
        let slf = &this.inner;
        let state = &slf.state;

        // handle service response future
        if let Some(fut) = this.fut.as_mut().as_pin_mut() {
            match fut.poll(cx) {
                Poll::Pending => (),
                Poll::Ready(item) => {
                    slf.shared.handle_result(item, state, false);
                    this.fut.set(None);
                }
            }
        }

        loop {
            match slf.st.get() {
                DispatcherState::Processing => {
                    let item = match slf.poll_service(&this.service, cx) {
                        PollService::Ready => {
                            if state.is_read_ready() {
                                // decode incoming bytes if buffer is ready
                                match state.decode_item(&slf.shared.codec) {
                                    Ok(Some(el)) => {
                                        slf.update_keepalive();
                                        DispatchItem::Item(el)
                                    }
                                    Ok(None) => {
                                        log::trace!("not enough data to decode next frame, register dispatch task");
                                        state.dsp_read_more_data(cx.waker());
                                        return Poll::Pending;
                                    }
                                    Err(err) => {
                                        slf.st.set(DispatcherState::Stop);
                                        slf.unregister_keepalive();
                                        DispatchItem::DecoderError(err)
                                    }
                                }
                            } else {
                                // no new events
                                state.dsp_register_task(cx.waker());
                                return Poll::Pending;
                            }
                        }
                        PollService::Item(item) => item,
                        PollService::ServiceError => continue,
                        PollService::Pending => return Poll::Pending,
                    };

                    // call service
                    if this.fut.is_none() {
                        // optimize first service call
                        this.fut.set(Some(this.service.call(item)));
                        match this.fut.as_mut().as_pin_mut().unwrap().poll(cx) {
                            Poll::Ready(res) => {
                                let _ =
                                    state.write_result(res, &slf.shared.codec).map_err(
                                        |err| slf.shared.error.set(Some(err.into())),
                                    );
                                this.fut.set(None);
                            }
                            Poll::Pending => {
                                slf.shared.inflight.set(slf.shared.inflight.get() + 1)
                            }
                        }
                    } else {
                        // spawn service call
                        slf.shared.inflight.set(slf.shared.inflight.get() + 1);

                        let st = state.clone();
                        let shared = slf.shared.clone();
                        crate::rt::spawn(this.service.call(item).map(move |item| {
                            shared.handle_result(item, &st, true);
                        }));
                    }
                }
                // drain service responses
                DispatcherState::Stop => {
                    // service may relay on poll_ready for response results
                    let _ = this.service.poll_ready(cx);

                    if slf.shared.inflight.get() == 0 {
                        slf.st.set(DispatcherState::Shutdown);
                        state.shutdown_io();
                    } else {
                        state.dsp_register_task(cx.waker());
                        return Poll::Pending;
                    }
                }
                // shutdown service
                DispatcherState::Shutdown => {
                    let err = slf.error.take();

                    return if this.service.poll_shutdown(cx, err.is_some()).is_ready() {
                        log::trace!("service shutdown is completed, stop");

                        Poll::Ready(if let Some(err) = err {
                            Err(err)
                        } else {
                            Ok(())
                        })
                    } else {
                        slf.error.set(err);
                        Poll::Pending
                    };
                }
            }
        }
    }
}

impl<S, U> DispatcherInner<S, U>
where
    S: Service<Request = DispatchItem<U>, Response = Option<Response<U>>>,
    U: Decoder + Encoder,
{
    fn poll_service(&self, srv: &S, cx: &mut Context<'_>) -> PollService<U> {
        match srv.poll_ready(cx) {
            Poll::Ready(Ok(_)) => {
                // service is ready, wake io read task
                self.state.dsp_restart_read_task();

                // check keepalive timeout
                self.check_keepalive();

                // check for errors
                if let Some(err) = self.shared.error.take() {
                    log::trace!("error occured, stopping dispatcher");
                    self.unregister_keepalive();
                    self.st.set(DispatcherState::Stop);

                    match err {
                        DispatcherError::KeepAlive => {
                            PollService::Item(DispatchItem::KeepAliveTimeout)
                        }
                        DispatcherError::Encoder(err) => {
                            PollService::Item(DispatchItem::EncoderError(err))
                        }
                        DispatcherError::Service(err) => {
                            self.error.set(Some(err));
                            PollService::ServiceError
                        }
                    }
                } else if self.state.is_dsp_stopped() {
                    log::trace!("dispatcher is instructed to stop");

                    self.unregister_keepalive();
                    self.st.set(DispatcherState::Stop);

                    // get io error
                    if let Some(err) = self.state.take_io_error() {
                        PollService::Item(DispatchItem::IoError(err))
                    } else {
                        PollService::ServiceError
                    }
                } else {
                    PollService::Ready
                }
            }
            // pause io read task
            Poll::Pending => {
                log::trace!("service is not ready, register dispatch task");
                self.state.dsp_service_not_ready(cx.waker());
                PollService::Pending
            }
            // handle service readiness error
            Poll::Ready(Err(err)) => {
                log::trace!("service readiness check failed, stopping");
                self.st.set(DispatcherState::Stop);
                self.error.set(Some(err));
                self.unregister_keepalive();
                PollService::ServiceError
            }
        }
    }

    fn ka(&self) -> Duration {
        Duration::from_secs(self.ka_timeout as u64)
    }

    fn ka_enabled(&self) -> bool {
        self.ka_timeout > 0
    }

    /// check keepalive timeout
    fn check_keepalive(&self) {
        if self.state.is_keepalive() {
            log::trace!("keepalive timeout");
            if let Some(err) = self.shared.error.take() {
                self.shared.error.set(Some(err));
            } else {
                self.shared.error.set(Some(DispatcherError::KeepAlive));
            }
        }
    }

    /// update keep-alive timer
    fn update_keepalive(&self) {
        if self.ka_enabled() {
            let updated = self.timer.now();
            if updated != self.ka_updated.get() {
                let ka = self.ka();
                self.timer.register(
                    updated + ka,
                    self.ka_updated.get() + ka,
                    &self.state,
                );
                self.ka_updated.set(updated);
            }
        }
    }

    /// unregister keep-alive timer
    fn unregister_keepalive(&self) {
        if self.ka_enabled() {
            self.timer
                .unregister(self.ka_updated.get() + self.ka(), &self.state);
        }
    }
}

#[cfg(test)]
mod tests {
    use bytes::Bytes;
    use futures::future::FutureExt;

    use crate::codec::BytesCodec;
    use crate::rt::time::delay_for;
    use crate::testing::Io;

    use super::*;

    impl<S, U> Dispatcher<S, U>
    where
        S: Service<Request = DispatchItem<U>, Response = Option<Response<U>>>,
        S::Error: 'static,
        S::Future: 'static,
        U: Decoder + Encoder + 'static,
        <U as Encoder>::Item: 'static,
    {
        /// Construct new `Dispatcher` instance
        pub(crate) fn debug<T, F: IntoService<S>>(
            io: T,
            codec: U,
            service: F,
        ) -> (Self, State)
        where
            T: AsyncRead + AsyncWrite + Unpin + 'static,
        {
            let timer = Timer::default();
            let ka_timeout = 30;
            let updated = timer.now();
            let state = State::new();
            let io = Rc::new(RefCell::new(io));
            let shared = Rc::new(DispatcherShared {
                codec: codec,
                error: Cell::new(None),
                inflight: Cell::new(0),
            });

            crate::rt::spawn(ReadTask::new(io.clone(), state.clone()));
            crate::rt::spawn(WriteTask::new(io.clone(), state.clone()));

            (
                Dispatcher {
                    service: service.into_service(),
                    response: None,
                    inner: DispatcherInner {
                        shared,
                        timer,
                        updated,
                        ka_timeout,
                        state: state.clone(),
                        st: DispatcherState::Processing,
                    },
                },
                state,
            )
        }
    }

    #[ntex_rt::test]
    async fn test_basic() {
        let (client, server) = Io::create();
        client.remote_buffer_cap(1024);
        client.write("GET /test HTTP/1\r\n\r\n");

        let (disp, _) = Dispatcher::debug(
            server,
            BytesCodec,
            crate::fn_service(|msg: DispatchItem<BytesCodec>| async move {
                delay_for(Duration::from_millis(50)).await;
                if let DispatchItem::Item(msg) = msg {
                    Ok::<_, ()>(Some(msg.freeze()))
                } else {
                    panic!()
                }
            }),
        );
        crate::rt::spawn(disp.map(|_| ()));

        let buf = client.read().await.unwrap();
        assert_eq!(buf, Bytes::from_static(b"GET /test HTTP/1\r\n\r\n"));

        client.close().await;
        assert!(client.is_server_dropped());
    }

    #[ntex_rt::test]
    async fn test_sink() {
        let (client, server) = Io::create();
        client.remote_buffer_cap(1024);
        client.write("GET /test HTTP/1\r\n\r\n");

        let (disp, st) = Dispatcher::debug(
            server,
            BytesCodec,
            crate::fn_service(|msg: DispatchItem<BytesCodec>| async move {
                if let DispatchItem::Item(msg) = msg {
                    Ok::<_, ()>(Some(msg.freeze()))
                } else {
                    panic!()
                }
            }),
        );
        crate::rt::spawn(disp.disconnect_timeout(25).map(|_| ()));

        let buf = client.read().await.unwrap();
        assert_eq!(buf, Bytes::from_static(b"GET /test HTTP/1\r\n\r\n"));

        assert!(st
            .write_item(Bytes::from_static(b"test"), &mut BytesCodec)
            .is_ok());
        let buf = client.read().await.unwrap();
        assert_eq!(buf, Bytes::from_static(b"test"));

        st.close();
        delay_for(Duration::from_millis(200)).await;
        assert!(client.is_server_dropped());
    }

    #[ntex_rt::test]
    async fn test_err_in_service() {
        let (client, server) = Io::create();
        client.remote_buffer_cap(0);
        client.write("GET /test HTTP/1\r\n\r\n");

        let (disp, state) = Dispatcher::debug(
            server,
            BytesCodec,
            crate::fn_service(|_: DispatchItem<BytesCodec>| async move {
                Err::<Option<Bytes>, _>(())
            }),
        );
        crate::rt::spawn(disp.map(|_| ()));

        state
            .write_item(
                Bytes::from_static(b"GET /test HTTP/1\r\n\r\n"),
                &mut BytesCodec,
            )
            .unwrap();

        let buf = client.read_any();
        assert_eq!(buf, Bytes::from_static(b""));
        delay_for(Duration::from_millis(25)).await;

        // buffer should be flushed
        client.remote_buffer_cap(1024);
        let buf = client.read().await.unwrap();
        assert_eq!(buf, Bytes::from_static(b"GET /test HTTP/1\r\n\r\n"));

        // write side must be closed, dispatcher waiting for read side to close
        assert!(client.is_closed());

        // close read side
        client.close().await;
        assert!(client.is_server_dropped());
    }
}
