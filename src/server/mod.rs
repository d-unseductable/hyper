//! HTTP Server
//!
//! A `Server` is created to listen on a port, parse HTTP requests, and hand
//! them off to a `Handler`.
use std::cell::RefCell;
use std::fmt;
use std::io;
use std::marker::PhantomData;
use std::net::{SocketAddr, TcpListener as StdTcpListener};
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use futures::{Future, Async, Map};
use futures::stream::{Stream};

use tokio::io::Io;
use tokio::net::TcpListener;
use tokio::reactor::{Core, Handle};
use tokio_proto::BindServer;
use tokio_proto::streaming::Message;
use tokio_proto::streaming::pipeline::ServerProto;
pub use tokio_service::{NewService, Service};

pub use self::request::Request;
pub use self::response::Response;

//use self::conn::Conn;

use body::{Body, TokioBody};
use http;


//mod conn;
mod request;
mod response;

type ServerBody = Body;
type HttpListener = TcpListener;

/// A Server that can accept incoming network requests.
#[derive(Debug)]
pub struct Server<A> {
    //listeners: Vec<A>,
    _marker: PhantomData<A>,
    addr: SocketAddr,
    keep_alive: bool,
    idle_timeout: Option<Duration>,
    max_sockets: usize,
}

impl<A: Accept> Server<A> {
    /*
    /// Creates a new Server from one or more Listeners.
    ///
    /// Panics if listeners is an empty iterator.
    pub fn new<I: IntoIterator<Item = A>>(listeners: I) -> Server<A> {
        let listeners = listeners.into_iter().collect();

        Server {
            listeners: listeners,
            keep_alive: true,
            idle_timeout: Some(Duration::from_secs(10)),
            max_sockets: 4096,
        }
    }
    */

    /// Enables or disables HTTP keep-alive.
    ///
    /// Default is true.
    pub fn keep_alive(mut self, val: bool) -> Server<A> {
        self.keep_alive = val;
        self
    }

    /// Sets how long an idle connection will be kept before closing.
    ///
    /// Default is 10 seconds.
    pub fn idle_timeout(mut self, val: Option<Duration>) -> Server<A> {
        self.idle_timeout = val;
        self
    }

    /// Sets the maximum open sockets for this Server.
    ///
    /// Default is 4096, but most servers can handle much more than this.
    pub fn max_sockets(mut self, val: usize) -> Server<A> {
        self.max_sockets = val;
        self
    }
}

impl Server<HttpListener> { //<H: HandlerFactory<<HttpListener as Accept>::Output>> Server<HttpListener, H> {
    /// Creates a new HTTP server config listening on the provided address.
    pub fn http(addr: &SocketAddr) -> ::Result<Server<HttpListener>> {
        Ok(Server {
            _marker: PhantomData,
            addr: addr.clone(),
            keep_alive: true,
            idle_timeout: Some(Duration::from_secs(10)),
            max_sockets: 4096,
        })
    }
}


/*
impl<S: SslServer> Server<HttpsListener<S>> {
    /// Creates a new server config that will handle `HttpStream`s over SSL.
    ///
    /// You can use any SSL implementation, as long as it implements `hyper::net::Ssl`.
    pub fn https(addr: &SocketAddr, ssl: S) -> ::Result<Server<HttpsListener<S>>> {
        HttpsListener::new(addr, ssl)
            .map(Server::new)
            .map_err(From::from)
    }
}
*/


impl/*<A: Accept>*/ Server<HttpListener> {
    /// Binds to a socket and starts handling connections.
    pub fn handle<H>(mut self, factory: H, handle: &Handle) -> ::Result<SocketAddr>
    where H: NewService<Request=Request, Response=Response, Error=::Error> + Send + 'static {
        let listener = try!(StdTcpListener::bind(&self.addr));
        let addr = try!(listener.local_addr());
        let listener = try!(TcpListener::from_listener(listener, &addr, handle));
        let binder = HttpServer;

        let inner_handle = handle.clone();
        handle.spawn(listener.incoming().for_each(move |(socket, _)| {
            let service = HttpService { inner: try!(factory.new_service()) };
            binder.bind_server(&inner_handle, socket, service);
            Ok(())
        }).map_err(|e| {
            error!("listener io error: {:?}", e);
            ()
        }));

        Ok(addr)
    }

    pub fn standalone<H>(mut self, factory: H) -> ::Result<(Listening, ServerLoop)>
    where H: NewService<Request=Request, Response=Response, Error=::Error> + Send + 'static {
        let mut core = try!(Core::new());
        let handle = core.handle();
        let addr = try!(self.handle(factory, &handle));
        let (shutdown_tx, shutdown_rx) = try!(::tokio::channel::channel(&handle));
        Ok((
            Listening {
                addr: addr,
                shutdown: shutdown_tx,
            },
             ServerLoop {
                inner: Some((core, shutdown_rx)),
            }
        ))

    }
}

/// A configured `Server` ready to run.
pub struct ServerLoop {
    inner: Option<(Core, ::tokio::channel::Receiver<()>)>,
}

impl fmt::Debug for ServerLoop {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.pad("ServerLoop")
    }
}

impl ServerLoop {
    /// Runs the server forever in this loop.
    ///
    /// This will block the current thread.
    pub fn run(self) {
        // drop will take care of it.
    }
}

impl Drop for ServerLoop {
    fn drop(&mut self) {
        self.inner.take().map(|(mut loop_, work)| {
            let _ = loop_.run(work.into_future());
            debug!("server closed");
        });
    }
}

/// A handle of the running server.
pub struct Listening {
    addr: SocketAddr,
    shutdown: ::tokio::channel::Sender<()>,
}

impl fmt::Debug for Listening {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_struct("Listening")
            .field("addr", &self.addr)
            .finish()
    }
}

impl fmt::Display for Listening {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        fmt::Display::fmt(&self.addr, f)
    }
}

impl Listening {
    /// The addresses this server is listening on.
    pub fn addr(&self) -> &SocketAddr {
        &self.addr
    }

    /// Stop the server from listening to its socket address.
    pub fn close(self) {
        debug!("closing server {}", self);
        let _ = self.shutdown.send(());
    }
}

struct HttpServer;

impl<T: Io + 'static> ServerProto<T> for HttpServer {
    type Request = http::RequestHead;
    type RequestBody = http::Chunk;
    type Response = ResponseHead;
    type ResponseBody = http::Chunk;
    type Error = ::Error;
    type Transport = http::Conn<T, http::ServerTransaction>;
    type BindTransport = io::Result<http::Conn<T, http::ServerTransaction>>;

    fn bind_transport(&self, io: T) -> Self::BindTransport {
        Ok(http::Conn::new(io))
    }
}

struct HttpService<T> {
    inner: T,
}

fn map_response_to_message(res: Response) -> Message<ResponseHead, TokioBody> {
    let (head, body) = response::split(res);
    if let Some(body) = body {
        Message::WithBody(head, body.into())
    } else {
        Message::WithoutBody(head)
    }
}

type ResponseHead = http::MessageHead<::StatusCode>;

impl<T> Service for HttpService<T>
    where T: Service<Request=Request, Response=Response, Error=::Error>,
{
    type Request = Message<http::RequestHead, TokioBody>;
    type Response = Message<ResponseHead, TokioBody>;
    type Error = ::Error;
    type Future = Map<T::Future, fn(Response) -> Message<ResponseHead, TokioBody>>;

    fn call(&self, message: Self::Request) -> Self::Future {
        let (head, body) = match message {
            Message::WithoutBody(head) => (head, Body::empty()),
            Message::WithBody(head, body) => (head, body.into()),
        };
        let req = request::new(head, body);
        self.inner.call(req).map(map_response_to_message)
    }
}

trait Accept: Stream {

}
