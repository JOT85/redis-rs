//! Adds experimental async IO support to redis.
use std::collections::VecDeque;
use std::io;
use std::mem;
use std::net::ToSocketAddrs;
use std::pin::Pin;
use std::task::{self, Poll};

use combine::{parser::combinator::AnySendPartialState, stream::PointerOffset};

#[cfg(unix)]
use tokio::net::UnixStream;

use tokio::{
    io::{AsyncRead, AsyncWrite, AsyncWriteExt},
    net::TcpStream,
    sync::{mpsc, oneshot},
};
use tokio_util::codec::Decoder;

#[cfg(unix)]
use futures_util::future::Either;
use futures_util::{
    future::{Future, FutureExt, TryFutureExt},
    ready,
    sink::Sink,
    stream::{Stream, StreamExt},
};

use pin_project_lite::pin_project;

use crate::cmd::{cmd, Cmd};
use crate::types::{ErrorKind, RedisError, RedisFuture, RedisResult, Value};

use crate::connection::{ConnectionAddr, ConnectionInfo};

use crate::parser::ValueCodec;

enum ActualConnection {
    Tcp(TcpStream),
    #[cfg(unix)]
    Unix(UnixStream),
}

impl AsyncWrite for ActualConnection {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut task::Context,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        match &mut *self {
            ActualConnection::Tcp(r) => Pin::new(r).poll_write(cx, buf),
            #[cfg(unix)]
            ActualConnection::Unix(r) => Pin::new(r).poll_write(cx, buf),
        }
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut task::Context) -> Poll<io::Result<()>> {
        match &mut *self {
            ActualConnection::Tcp(r) => Pin::new(r).poll_flush(cx),
            #[cfg(unix)]
            ActualConnection::Unix(r) => Pin::new(r).poll_flush(cx),
        }
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut task::Context) -> Poll<io::Result<()>> {
        match &mut *self {
            ActualConnection::Tcp(r) => Pin::new(r).poll_shutdown(cx),
            #[cfg(unix)]
            ActualConnection::Unix(r) => Pin::new(r).poll_shutdown(cx),
        }
    }
}

impl AsyncRead for ActualConnection {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut task::Context,
        buf: &mut [u8],
    ) -> Poll<io::Result<usize>> {
        match &mut *self {
            ActualConnection::Tcp(r) => Pin::new(r).poll_read(cx, buf),
            #[cfg(unix)]
            ActualConnection::Unix(r) => Pin::new(r).poll_read(cx, buf),
        }
    }
}

/// Represents a stateful redis TCP connection.
pub struct Connection {
    con: ActualConnection,
    buf: Vec<u8>,
    decoder: combine::stream::Decoder<AnySendPartialState, PointerOffset<[u8]>>,
    db: i64,
}

impl Connection {
    async fn read_response(&mut self) -> RedisResult<Value> {
        crate::parser::parse_redis_value_async(&mut self.decoder, &mut self.con).await
    }
}

/// Opens a connection.
pub async fn connect(connection_info: &ConnectionInfo) -> RedisResult<Connection> {
    let con = connect_simple(connection_info).await?;

    let mut rv = Connection {
        con,
        buf: Vec::new(),
        decoder: combine::stream::Decoder::new(),
        db: connection_info.db,
    };

    authenticate(connection_info, &mut rv).await?;

    Ok(rv)
}

async fn authenticate<C>(connection_info: &ConnectionInfo, con: &mut C) -> RedisResult<()>
where
    C: ConnectionLike,
{
    if let Some(passwd) = &connection_info.passwd {
        match cmd("AUTH").arg(&**passwd).query_async(con).await {
            Ok(Value::Okay) => (),
            _ => {
                fail!((
                    ErrorKind::AuthenticationFailed,
                    "Password authentication failed"
                ));
            }
        }
    }

    if connection_info.db != 0 {
        match cmd("SELECT").arg(connection_info.db).query_async(con).await {
            Ok(Value::Okay) => (),
            _ => fail!((
                ErrorKind::ResponseError,
                "Redis server refused to switch database"
            )),
        }
    }

    Ok(())
}

async fn connect_simple(connection_info: &ConnectionInfo) -> RedisResult<ActualConnection> {
    Ok(match *connection_info.addr {
        ConnectionAddr::Tcp(ref host, port) => {
            let socket_addr = {
                let mut socket_addrs = (&host[..], port).to_socket_addrs()?;
                match socket_addrs.next() {
                    Some(socket_addr) => socket_addr,
                    None => {
                        return Err(RedisError::from((
                            ErrorKind::InvalidClientConfig,
                            "No address found for host",
                        )));
                    }
                }
            };

            TcpStream::connect(&socket_addr)
                .await
                .map(ActualConnection::Tcp)?
        }

        #[cfg(unix)]
        ConnectionAddr::Unix(ref path) => UnixStream::connect(path)
            .await
            .map(ActualConnection::Unix)?,

        #[cfg(not(unix))]
        ConnectionAddr::Unix(_) => {
            return Err(RedisError::from((
                ErrorKind::InvalidClientConfig,
                "Cannot connect to unix sockets \
                 on this platform",
            )))
        }
    })
}

/// An async abstraction over connections.
pub trait ConnectionLike: Sized {
    /// Sends an already encoded (packed) command into the TCP socket and
    /// reads the single response from it.
    fn req_packed_command<'a>(&'a mut self, cmd: &'a Cmd) -> RedisFuture<'a, Value>;

    /// Sends multiple already encoded (packed) command into the TCP socket
    /// and reads `count` responses from it.  This is used to implement
    /// pipelining.
    fn req_packed_commands<'a>(
        &'a mut self,
        cmd: &'a crate::Pipeline,
        offset: usize,
        count: usize,
    ) -> RedisFuture<'a, Vec<Value>>;

    /// Returns the database this connection is bound to.  Note that this
    /// information might be unreliable because it's initially cached and
    /// also might be incorrect if the connection like object is not
    /// actually connected.
    fn get_db(&self) -> i64;
}

impl ConnectionLike for Connection {
    fn req_packed_command<'a>(&'a mut self, cmd: &'a Cmd) -> RedisFuture<'a, Value> {
        (async move {
            self.buf.clear();
            cmd.write_packed_command(&mut self.buf);
            self.con.write_all(&self.buf).await?;
            self.read_response().await
        })
        .boxed()
    }

    fn req_packed_commands<'a>(
        &'a mut self,
        cmd: &'a crate::Pipeline,
        offset: usize,
        count: usize,
    ) -> RedisFuture<'a, Vec<Value>> {
        (async move {
            self.buf.clear();
            cmd.write_packed_pipeline(&mut self.buf);
            self.con.write_all(&self.buf).await?;

            for _ in 0..offset {
                self.read_response().await?;
            }

            let mut rv = Vec::with_capacity(count);
            for _ in 0..count {
                rv.push(self.read_response().await?);
            }

            Ok(rv)
        })
        .boxed()
    }

    fn get_db(&self) -> i64 {
        self.db
    }
}

// Senders which the result of a single request are sent through
type PipelineOutput<O, E> = oneshot::Sender<Result<Vec<O>, E>>;

struct InFlight<O, E> {
    output: PipelineOutput<O, E>,
    response_count: usize,
    buffer: Vec<O>,
}

// A single message sent through the pipeline
struct PipelineMessage<S, I, E> {
    input: S,
    output: PipelineOutput<I, E>,
    response_count: usize,
}

/// Wrapper around a `Stream + Sink` where each item sent through the `Sink` results in one or more
/// items being output by the `Stream` (the number is specified at time of sending). With the
/// interface provided by `Pipeline` an easy interface of request to response, hiding the `Stream`
/// and `Sink`.
struct Pipeline<SinkItem, I, E>(mpsc::Sender<PipelineMessage<SinkItem, I, E>>);

impl<SinkItem, I, E> Clone for Pipeline<SinkItem, I, E> {
    fn clone(&self) -> Self {
        Pipeline(self.0.clone())
    }
}

pin_project! {
    struct PipelineSink<T, I, E> {
        #[pin]
        sink_stream: T,
        in_flight: VecDeque<InFlight<I, E>>,
        error: Option<E>,
    }
}

impl<T, I, E> PipelineSink<T, I, E>
where
    T: Stream<Item = Result<I, E>> + 'static,
{
    fn new<SinkItem>(sink_stream: T) -> Self
    where
        T: Sink<SinkItem, Error = E> + Stream<Item = Result<I, E>> + 'static,
    {
        PipelineSink {
            sink_stream,
            in_flight: VecDeque::new(),
            error: None,
        }
    }

    // Read messages from the stream and send them back to the caller
    fn poll_read(mut self: Pin<&mut Self>, cx: &mut task::Context) -> Poll<Result<(), ()>> {
        loop {
            let item = match ready!(self.as_mut().project().sink_stream.poll_next(cx)) {
                Some(Ok(item)) => Ok(item),
                Some(Err(err)) => Err(err),
                // The redis response stream is not going to produce any more items so we `Err`
                // to break out of the `forward` combinator and stop handling requests
                None => return Poll::Ready(Err(())),
            };
            self.as_mut().send_result(item);
        }
    }

    fn send_result(self: Pin<&mut Self>, result: Result<I, E>) {
        let self_ = self.project();
        let response = {
            let entry = match self_.in_flight.front_mut() {
                Some(entry) => entry,
                None => return,
            };
            match result {
                Ok(item) => {
                    entry.buffer.push(item);
                    if entry.response_count > entry.buffer.len() {
                        // Need to gather more response values
                        return;
                    }
                    Ok(mem::replace(&mut entry.buffer, Vec::new()))
                }
                // If we fail we must respond immediately
                Err(err) => Err(err),
            }
        };

        let entry = self_.in_flight.pop_front().unwrap();
        // `Err` means that the receiver was dropped in which case it does not
        // care about the output and we can continue by just dropping the value
        // and sender
        entry.output.send(response).ok();
    }
}

impl<SinkItem, T, I, E> Sink<PipelineMessage<SinkItem, I, E>> for PipelineSink<T, I, E>
where
    T: Sink<SinkItem, Error = E> + Stream<Item = Result<I, E>> + 'static,
{
    type Error = ();

    // Retrieve incoming messages and write them to the sink
    fn poll_ready(
        mut self: Pin<&mut Self>,
        cx: &mut task::Context,
    ) -> Poll<Result<(), Self::Error>> {
        match ready!(self.as_mut().project().sink_stream.poll_ready(cx)) {
            Ok(()) => Ok(()).into(),
            Err(err) => {
                *self.project().error = Some(err);
                Ok(()).into()
            }
        }
    }

    fn start_send(
        mut self: Pin<&mut Self>,
        PipelineMessage {
            input,
            output,
            response_count,
        }: PipelineMessage<SinkItem, I, E>,
    ) -> Result<(), Self::Error> {
        let self_ = self.as_mut().project();
        if let Some(err) = self_.error.take() {
            let _ = output.send(Err(err));
            return Err(());
        }
        match self_.sink_stream.start_send(input) {
            Ok(()) => {
                self_.in_flight.push_back(InFlight {
                    output,
                    response_count,
                    buffer: Vec::new(),
                });
                Ok(())
            }
            Err(err) => {
                let _ = output.send(Err(err));
                Err(())
            }
        }
    }

    fn poll_flush(
        mut self: Pin<&mut Self>,
        cx: &mut task::Context,
    ) -> Poll<Result<(), Self::Error>> {
        ready!(self
            .as_mut()
            .project()
            .sink_stream
            .poll_flush(cx)
            .map_err(|err| {
                self.as_mut().send_result(Err(err));
            }))?;
        self.poll_read(cx)
    }

    fn poll_close(
        mut self: Pin<&mut Self>,
        cx: &mut task::Context,
    ) -> Poll<Result<(), Self::Error>> {
        // No new requests will come in after the first call to `close` but we need to complete any
        // in progress requests before closing
        if !self.in_flight.is_empty() {
            ready!(self.as_mut().poll_flush(cx))?;
        }
        let this = self.as_mut().project();
        this.sink_stream.poll_close(cx).map_err(|err| {
            self.send_result(Err(err));
        })
    }
}

impl<SinkItem, I, E> Pipeline<SinkItem, I, E>
where
    SinkItem: Send + 'static,
    I: Send + 'static,
    E: Send + 'static,
{
    fn new<T>(sink_stream: T) -> (Self, impl Future<Output = ()>)
    where
        T: Sink<SinkItem, Error = E> + Stream<Item = Result<I, E>> + 'static,
        T: Send + 'static,
        T::Item: Send,
        T::Error: Send,
        T::Error: ::std::fmt::Debug,
    {
        const BUFFER_SIZE: usize = 50;
        let (sender, receiver) = mpsc::channel(BUFFER_SIZE);
        let f = receiver
            .map(Ok)
            .forward(PipelineSink::new::<SinkItem>(sink_stream))
            .map(|_| ());
        (Pipeline(sender), f)
    }

    // `None` means that the stream was out of items causing that poll loop to shut down.
    async fn send(&mut self, item: SinkItem) -> Result<I, Option<E>> {
        self.send_recv_multiple(item, 1)
            // We can unwrap since we do a request for `1` item
            .map_ok(|mut item| item.pop().unwrap())
            .await
    }

    async fn send_recv_multiple(
        &mut self,
        input: SinkItem,
        count: usize,
    ) -> Result<Vec<I>, Option<E>> {
        let (sender, receiver) = oneshot::channel();

        self.0
            .send(PipelineMessage {
                input,
                response_count: count,
                output: sender,
            })
            .map_err(|_| None)
            .and_then(|_| {
                receiver.map(|result| {
                    match result {
                        Ok(result) => result.map_err(Some),
                        Err(_) => {
                            // The `sender` was dropped which likely means that the stream part
                            // failed for one reason or another
                            Err(None)
                        }
                    }
                })
            })
            .await
    }
}

/// A connection object which can be cloned, allowing requests to be be sent concurrently
/// on the same underlying connection (tcp/unix socket).
#[derive(Clone)]
pub struct MultiplexedConnection {
    pipeline: Pipeline<Vec<u8>, Value, RedisError>,
    db: i64,
}

impl MultiplexedConnection {
    /// Creates a multiplexed connection from a connection and executor.
    pub(crate) async fn new(
        connection_info: &ConnectionInfo,
    ) -> RedisResult<(Self, impl Future<Output = ()>)> {
        let con = connect_simple(connection_info).await?;
        let (pipeline, driver) = match con {
            #[cfg(not(unix))]
            ActualConnection::Tcp(tcp) => {
                let codec = ValueCodec::default().framed(tcp);
                let (pipeline, driver) = Pipeline::new(codec);
                (pipeline, driver)
            }

            #[cfg(unix)]
            ActualConnection::Tcp(tcp) => {
                let codec = ValueCodec::default().framed(tcp);
                let (pipeline, driver) = Pipeline::new(codec);
                (pipeline, Either::Left(driver))
            }
            #[cfg(unix)]
            ActualConnection::Unix(unix) => {
                let codec = ValueCodec::default().framed(unix);
                let (pipeline, driver) = Pipeline::new(codec);
                (pipeline, Either::Right(driver))
            }
        };
        let mut con = MultiplexedConnection {
            pipeline,
            db: connection_info.db,
        };
        authenticate(connection_info, &mut con).await?;
        Ok((con, driver))
    }
}

impl ConnectionLike for MultiplexedConnection {
    fn req_packed_command<'a>(&'a mut self, cmd: &'a Cmd) -> RedisFuture<'a, Value> {
        (async move {
            let value = self
                .pipeline
                .send(cmd.get_packed_command())
                .await
                .map_err(|err| {
                    err.unwrap_or_else(|| {
                        RedisError::from(io::Error::from(io::ErrorKind::BrokenPipe))
                    })
                })?;
            Ok(value)
        })
        .boxed()
    }

    fn req_packed_commands<'a>(
        &'a mut self,
        cmd: &'a crate::Pipeline,
        offset: usize,
        count: usize,
    ) -> RedisFuture<'a, Vec<Value>> {
        (async move {
            let mut value = self
                .pipeline
                .send_recv_multiple(cmd.get_packed_pipeline(), offset + count)
                .await
                .map_err(|err| {
                    err.unwrap_or_else(|| {
                        RedisError::from(io::Error::from(io::ErrorKind::BrokenPipe))
                    })
                })?;

            value.drain(..offset);
            Ok(value)
        })
        .boxed()
    }

    fn get_db(&self) -> i64 {
        self.db
    }
}

#[cfg(feature = "connection-manager")]
mod connection_manager {
    use super::*;

    use std::sync::Arc;

    use arc_swap::{self, ArcSwap};
    use futures::future::{self, Shared};
    use futures_util::future::BoxFuture;

    /// A `ConnectionManager` is a proxy that wraps a multiplexed connection and
    /// automatically reconnects to the server when necessary.
    ///
    /// ## Behavior
    ///
    /// - When creating an instance of the `ConnectionManager`, an initial
    ///   connection will be established and awaited. Connection errors will be
    ///   returned directly.
    /// - When a command sent to the server fails with an error that represents
    ///   a "connection dropped" condition, that error will be passed on to the
    ///   user, but it will trigger a reconnection in the background.
    /// - The reconnect code will atomically swap the current (dead) connection
    ///   with a future that will eventually resolve to a `MultiplexedConnection`
    ///   or to a `RedisError`
    /// - All commands that are issued after the reconnect process has been
    ///   initiated, will have to await the connection future.
    /// - If reconnecting fails, all pending commands will be failed as well. A
    ///   new reconnection attempt will be triggered if the error is an I/O error.
    #[derive(Clone)]
    pub struct ConnectionManager {
        /// Information used for the connection. This is needed to be able to reconnect.
        connection_info: ConnectionInfo,
        /// The connection future.
        ///
        /// The `ArcSwap` is required to be able to replace the connection
        /// without making the `ConnectionManager` mutable.
        connection: Arc<ArcSwap<SharedRedisFuture<MultiplexedConnection>>>,
    }

    /// A `RedisResult` that can be cloned because `RedisError` is behind an `Arc`.
    type CloneableRedisResult<T> = Result<T, Arc<RedisError>>;

    /// Type alias for a shared boxed future that will resolve to a `CloneableRedisResult`.
    type SharedRedisFuture<T> = Shared<BoxFuture<'static, CloneableRedisResult<T>>>;

    impl ConnectionManager {
        /// Connect to the server and store the connection inside the returned `ConnectionManager`.
        ///
        /// This requires the `connection-manager` feature, which will also pull in
        /// the Tokio executor.
        pub async fn new(connection_info: ConnectionInfo) -> RedisResult<Self> {
            // Create a MultiplexedConnection and wait for it to be established
            let (connection, driver) = MultiplexedConnection::new(&connection_info).await?;

            // Spawn the driver that drives the connection future
            tokio::spawn(driver);

            // Wrap the connection in an `ArcSwap` instance for fast atomic access
            Ok(Self {
                connection_info,
                connection: Arc::new(ArcSwap::from_pointee(
                    future::ok(connection).boxed().shared(),
                )),
            })
        }

        /// Reconnect and overwrite the old connection.
        ///
        /// The `current` guard points to the shared future that was active
        /// when the connection loss was detected.
        fn reconnect(
            &self,
            current: arc_swap::Guard<'_, Arc<SharedRedisFuture<MultiplexedConnection>>>,
        ) {
            let connection_info = self.connection_info.clone();
            let new_connection: SharedRedisFuture<MultiplexedConnection> = async move {
                let (new_connection, driver) = MultiplexedConnection::new(&connection_info).await?;
                tokio::spawn(driver);
                Ok(new_connection)
            }
            .boxed()
            .shared();

            // Update the connection in the connection manager
            let new_connection_arc = Arc::new(new_connection.clone());
            let prev = self
                .connection
                .compare_and_swap(&current, new_connection_arc);

            // If the swap happened...
            if Arc::ptr_eq(&prev, &current) {
                // ...start the connection attempt immediately but do not wait on it.
                tokio::spawn(new_connection);
            }
        }
    }

    /// Handle a command result. If the connection was dropped, reconnect.
    macro_rules! reconnect_if_dropped {
        ($self:expr, $result:expr, $current:expr) => {
            if let Err(ref e) = $result {
                if e.is_connection_dropped() {
                    $self.reconnect($current);
                }
            }
        };
    }

    /// Handle a connection result. If there's an I/O error, reconnect.
    /// Propagate any error.
    macro_rules! reconnect_if_io_error {
        ($self:expr, $result:expr, $current:expr) => {
            if let Err(e) = $result {
                if e.is_io_error() {
                    $self.reconnect($current);
                }
                return Err(e);
            }
        };
    }

    impl ConnectionLike for ConnectionManager {
        fn req_packed_command<'a>(&'a mut self, cmd: &'a Cmd) -> RedisFuture<'a, Value> {
            (async move {
                // Clone connection to avoid having to lock the ArcSwap in write mode
                let guard = self.connection.load();
                let connection_result = (**guard)
                    .clone()
                    .await
                    .map_err(|e| e.clone_mostly(Some("Reconnecting failed")));
                reconnect_if_io_error!(self, connection_result, guard);
                let result = connection_result?.req_packed_command(cmd).await;
                reconnect_if_dropped!(self, &result, guard);
                result
            })
            .boxed()
        }

        fn req_packed_commands<'a>(
            &'a mut self,
            cmd: &'a crate::Pipeline,
            offset: usize,
            count: usize,
        ) -> RedisFuture<'a, Vec<Value>> {
            (async move {
                // Clone shared connection future to avoid having to lock the ArcSwap in write mode
                let guard = self.connection.load();
                let connection_result = (**guard)
                    .clone()
                    .await
                    .map_err(|e| e.clone_mostly(Some("Reconnecting failed")));
                reconnect_if_io_error!(self, connection_result, guard);
                let result = connection_result?
                    .req_packed_commands(cmd, offset, count)
                    .await;
                reconnect_if_dropped!(self, &result, guard);
                result
            })
            .boxed()
        }

        fn get_db(&self) -> i64 {
            self.connection_info.db
        }
    }
}

#[cfg(feature = "connection-manager")]
pub use connection_manager::ConnectionManager;
