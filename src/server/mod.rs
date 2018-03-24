use {flush, Body, RecvBody};

use futures::{Async, Future, Poll, Stream};
use futures::future::{Executor, Either, Join, MapErr};
use h2::{self, Reason};
use h2::server::{Connection as Accept, Handshake, SendResponse};
use http::{self, Request, Response};
use tokio_io::{AsyncRead, AsyncWrite};
use tower::{NewService, Service};

use std::{error, fmt, mem};
use std::marker::PhantomData;

/// Attaches service implementations to h2 connections.
pub struct Server<S, E, B>
where S: NewService,
      B: Body,
{
    new_service: S,
    builder: h2::server::Builder,
    executor: E,
    _p: PhantomData<B>,
}

/// Drives connection-level I/O .
pub struct Connection<T, S, E, B, F>
where T: AsyncRead + AsyncWrite,
      S: NewService,
      B: Body,
{
    state: State<T, S, B>,
    executor: E,
    modify: F,
}

/// Modify a received request
pub trait Modify {
    /// Modify a request before calling the service.
    fn modify(&mut self, request: &mut Request<()>);
}

enum State<T, S, B>
where T: AsyncRead + AsyncWrite,
      S: NewService,
      B: Body,
{
    /// Establish the HTTP/2.0 connection and get a service to process inbound
    /// requests.
    Init(Init<T, B::Data, S::Future, S::InitError>),

    /// Both the HTTP/2.0 connection and the service are ready.
    Ready {
        connection: Accept<T, B::Data>,
        service: S::Service,
    },

    /// The service has closed, so poll until connection is closed.
    GoAway {
        connection: Accept<T, B::Data>,
        error: S::Error,
    },

    /// Everything is closed up.
    Done,
}

type Init<T, B, S, E> =
    Join<
        MapErr<Handshake<T, B>, MapErrA<E>>,
        MapErr<S, MapErrB<E>>>;

type MapErrA<E> = fn(h2::Error) -> Either<h2::Error, E>;
type MapErrB<E> = fn(E) -> Either<h2::Error, E>;

/// Task used to process requests
pub struct Background<T, B>
where B: Body,
{
    state: BackgroundState<T, B>,
}

enum BackgroundState<T, B>
where B: Body,
{
    Respond {
        respond: SendResponse<B::Data>,
        response: T,
    },
    Flush(flush::Flush<B>),
}

/// Error produced by a `Connection`.
#[derive(Debug)]
pub enum Error<S>
where S: NewService,
{
    /// Error produced during the HTTP/2.0 handshake.
    Handshake(h2::Error),

    /// Error produced by the HTTP/2.0 stream
    Protocol(h2::Error),

    /// Error produced when obtaining the service
    NewService(S::InitError),

    /// Error produced by the service
    Service(S::Error),

    /// Error produced when attempting to spawn a task
    Execute,
}

enum PollMain {
    Again,
    Done,
}

// ===== impl Server =====

impl<S, E, B> Server<S, E, B>
where S: NewService<Request = Request<RecvBody>, Response = Response<B>>,
      B: Body,
{
    pub fn new(new_service: S, builder: h2::server::Builder, executor: E) -> Self {
        Server {
            new_service,
            executor,
            builder,
            _p: PhantomData,
        }
    }
}


impl<S, E, B> Server<S, E, B>
where S: NewService<Request = http::Request<RecvBody>, Response = Response<B>>,
      B: Body,
      E: Clone,
{
    /// Produces a future that is satisfied once the h2 connection has been initialized.
    pub fn serve<T>(&self, io: T) -> Connection<T, S, E, B, ()>
    where T: AsyncRead + AsyncWrite,
    {
        self.serve_modified(io, ())
    }

    pub fn serve_modified<T, F>(&self, io: T, modify: F) -> Connection<T, S, E, B, F>
    where T: AsyncRead + AsyncWrite,
          F: Modify,
    {
        // Clone a handle to the executor so that it can be moved into the
        // connection handle
        let executor = self.executor.clone();

        let service = self.new_service.new_service()
            .map_err(Either::B as MapErrB<S::InitError>);

        // TODO we should specify initial settings here!
        let handshake = self.builder.handshake(io)
            .map_err(Either::A as MapErrA<S::InitError>);

        Connection {
            state: State::Init(handshake.join(service)),
            executor,
            modify,
        }
    }
}

// B doesn't need to be Clone, it's just a marker type.
impl<S, E, B> Clone for Server<S, E, B>
where
    S: NewService + Clone,
    E: Clone,
    B: Body,
{
    fn clone(&self) -> Self {
        Server {
            new_service: self.new_service.clone(),
            executor: self.executor.clone(),
            builder: self.builder.clone(),
            _p: PhantomData,
        }
    }
}

// ===== impl Connection =====

impl<T, S, E, B, F> Future for Connection<T, S, E, B, F>
where T: AsyncRead + AsyncWrite,
      S: NewService<Request = http::Request<RecvBody>, Response = Response<B>>,
      E: Executor<Background<<S::Service as Service>::Future, B>>,
      B: Body + 'static,
      F: Modify,
{
    type Item = ();
    type Error = Error<S>;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        let ret = (|| loop {
            match self.state {
                State::Init(..) => try_ready!(self.poll_init()),
                State::Ready { .. } => {
                    match try_ready!(self.poll_main()) {
                        PollMain::Again => continue,
                        PollMain::Done => {
                            self.state = State::Done;
                            return Ok(().into());
                        }
                    }
                },
                State::GoAway { .. } => try_ready!(self.poll_goaway()),
                State::Done => return Ok(().into()),
            }
        })();
        ret.map_err(|e| {
            self.state = State::Done;
            e
        })
    }
}

impl<T, S, E, B, F> Connection<T, S, E, B, F>
where T: AsyncRead + AsyncWrite,
      S: NewService<Request = http::Request<RecvBody>, Response = Response<B>>,
      E: Executor<Background<<S::Service as Service>::Future, B>>,
      B: Body + 'static,
      F: Modify,
{
    fn poll_init(&mut self) -> Poll<(), Error<S>> {
        use self::State::*;

        let (connection, service) = match self.state {
            Init(ref mut join) => try_ready!(join.poll().map_err(Error::from_init)),
            _ => unreachable!(),
        };

        self.state = Ready { connection, service };

        Ok(().into())
    }

    fn poll_main(&mut self) -> Poll<PollMain, Error<S>> {
        let error = match self.state {
            State::Ready { ref mut connection, ref mut service } => loop {
                // Make sure the service is ready
                match service.poll_ready() {
                    Ok(Async::Ready(())) => (),
                    Ok(Async::NotReady) => return Ok(Async::NotReady),
                    Err(err) => {
                        trace!("service closed");
                        // service is closed, transition to goaway state
                        break err;
                    }
                }

                let next = connection.poll()
                    .map_err(Error::Protocol);

                let (request, respond) = match try_ready!(next) {
                    Some(next) => next,
                    None => return Ok(PollMain::Done.into()),
                };

                let (parts, body) = request.into_parts();

                // This is really unfortunate, but the `http` currently lacks the
                // APIs to do this better :(
                let mut request = Request::from_parts(parts, ());
                self.modify.modify(&mut request);

                let (parts, _) = request.into_parts();
                let request = Request::from_parts(parts, RecvBody::new(body));

                // Dispatch the request to the service
                let response = service.call(request);

                // Spawn a new task to process the response future
                if let Err(_) = self.executor.execute(Background::new(respond, response)) {
                    return Err(Error::Execute)
                }
            }
            _ => unreachable!(),
        };

        // We only break out of the loop on a service error, which means we
        // should transition to GOAWAY.
        match mem::replace(&mut self.state, State::Done) {
            State::Ready { mut connection, .. } => {
                // this sends a GOAWAY, and starts a graceful shutdown.
                connection.close_connection();

                self.state = State::GoAway {
                    connection,
                    error,
                };

                Ok(Async::Ready(PollMain::Again))
            },
            _ => unreachable!(),
        }
    }

    fn poll_goaway(&mut self) -> Poll<(), Error<S>> {
        match self.state {
            State::GoAway { ref mut connection, .. } => {
                try_ready!(connection.poll_close().map_err(Error::Protocol));
            }
            _ => unreachable!(),
        }

        // Once here, the connection has finished successfully. Time to just
        // return the service error.
        match mem::replace(&mut self.state, State::Done) {
            State::GoAway { error, .. } => {
                trace!("goaway completed");
                Err(Error::Service(error))
            },
            _ => unreachable!(),
        }
    }
}


// ===== impl Modify =====

impl<T> Modify for T
where T: FnMut(&mut Request<()>)
{
    fn modify(&mut self, request: &mut Request<()>) {
        (*self)(request);
    }
}

impl Modify for () {
    fn modify(&mut self, _: &mut Request<()>) {
    }
}

// ===== impl Background =====

impl<T, B> Background<T, B>
where T: Future,
      B: Body,
{
    fn new(respond: SendResponse<B::Data>, response: T) -> Self {
        Background {
            state: BackgroundState::Respond {
                respond,
                response,
            },
        }
    }
}

impl<T, B> Future for Background<T, B>
where T: Future<Item = Response<B>>,
      B: Body,
{
    type Item = ();
    type Error = ();

    fn poll(&mut self) -> Poll<(), ()> {
        use self::BackgroundState::*;

        loop {
            let flush = match self.state {
                Respond { ref mut respond, ref mut response } => {
                    use flush::Flush;

                    let response = try_ready!(response.poll().map_err(|_| {
                        // TODO: do something better the error?
                        let reason = Reason::INTERNAL_ERROR;
                        respond.send_reset(reason);
                    }));

                    let (parts, body) = response.into_parts();

                    // Check if the response is immediately an end-of-stream.
                    let end_stream = body.is_end_stream();
                    trace!("send_response eos={} {:?}", end_stream, parts);

                    // Try sending the response.
                    let response = Response::from_parts(parts, ());
                    match respond.send_response(response, end_stream) {
                        Ok(stream) => {
                            if end_stream {
                                // Nothing more to do
                                return Ok(().into());
                            }

                            // Transition to flushing the body
                            Flush::new(body, stream)
                        }
                        Err(_) => {
                            // TODO: Do something with the error?
                            return Ok(().into());
                        }
                    }
                }
                Flush(ref mut flush) => return flush.poll(),
            };

            self.state = Flush(flush);
        }
    }
}

// ===== impl Error =====

impl<S> Error<S>
where S: NewService,
{
    fn from_init(err: Either<h2::Error, S::InitError>) -> Self {
        match err {
            Either::A(err) => Error::Handshake(err),
            Either::B(err) => Error::NewService(err),
        }
    }
}

impl<S> fmt::Display for Error<S>
where
    Error<S>: error::Error,
    S: NewService,
    S: fmt::Debug,
    S::InitError: error::Error,
    S::Error: error::Error,
{
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match *self {
            Error::Handshake(ref why) =>
                write!(f, "Error occurred during HTTP/2.0 handshake: {}", why),
            Error::Protocol(ref why) =>
                write!(f, "Error produced by HTTP/2.0 stream: {}", why),
            Error::NewService(ref why) =>
                write!(f, "Error occurred while obtaining service: {}", why),
            Error::Service(ref why) =>
                write!(f, "Error returned by service: {}", why),
            Error::Execute =>
                write!(f, "Error occurred while attempting to spawn a task"),
        }
    }
}

impl<S> error::Error for Error<S>
where
    S: NewService,
    S: fmt::Debug,
    S::InitError: error::Error,
    S::Error: error::Error,
{
    fn cause(&self) -> Option<&error::Error> {
        match *self {
            Error::Handshake(ref why) => Some(why),
            Error::Protocol(ref why) => Some(why),
            Error::NewService(ref why) => Some(why),
            Error::Service(ref why) => Some(why),
            Error::Execute => None,
        }
    }

    fn description(&self) -> &str {
        match *self {
            Error::Handshake(_) =>  "error occurred during HTTP/2.0 handshake",
            Error::Protocol(_) => "error produced by HTTP/2.0 stream",
            Error::NewService(_) => "error occured while obtaining service",
            Error::Service(_) => "error returned by service",
            Error::Execute => "error occurred while attempting to spawn a task",
        }
    }

}
