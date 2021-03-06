//! Async version of connection.
//!
//! This module requires the `futures` feature to be enabled.
//!
//! Current status:
//!  * Basic client functionality is up and running, i e, you can make method calls.
//!  * Receiving messages (e g signals) is possible, but expect a simpler API later.
//!  * As for server side code, you can use the `tree` module with this connection, but it does not
//!    support async method handlers.
//!
//! You're probably going to need a companion crate - dbus-tokio - for this connection to make sense.
//! (Although you can also just call read_write and process_all at regular intervals, and possibly
//! set a timeout handler.)


use crate::{Error, Message};
use crate::channel::{MatchingReceiver, Channel, Sender, Token};
use crate::strings::{BusName, Path, Interface, Member};
use crate::arg::{AppendAll, ReadAll, IterAppend};
use crate::message::MatchRule;

use std::sync::{Arc, Mutex};
use std::{task, pin, mem};
use std::cell::RefCell;
use std::time::Duration;
use crate::filters::Filters;
use std::future::Future;
use std::time::Instant;
use std::collections::HashMap;


mod generated_org_freedesktop_notifications;
mod generated_org_freedesktop_dbus;


/// This module contains some standard interfaces and an easy way to call them.
///
/// See the [D-Bus specification](https://dbus.freedesktop.org/doc/dbus-specification.html#standard-interfaces) for more information about these standard interfaces.
///
/// The code was created by dbus-codegen.
pub mod stdintf {
    #[allow(missing_docs)]
    pub mod org_freedesktop_dbus {
        pub use super::super::generated_org_freedesktop_notifications::*;
        #[allow(unused_imports)]
        pub(crate) use super::super::generated_org_freedesktop_dbus::*;

        #[derive(Debug, PartialEq, Eq, Copy, Clone)]
        pub enum RequestNameReply {
            PrimaryOwner = 1,
            InQueue = 2,
            Exists = 3,
            AlreadyOwner = 4,
        }

        #[derive(Debug, PartialEq, Eq, Copy, Clone)]
        pub enum ReleaseNameReply {
            Released = 1,
            NonExistent = 2,
            NotOwner = 3,
        }

    }
}


type Replies<F> = HashMap<Token, F>;

/// A connection to D-Bus, thread local + async version
pub struct LocalConnection {
    channel: Channel,
    filters: RefCell<Filters<LocalFilterCb>>,
    replies: RefCell<Replies<LocalRepliesCb>>,
    timeout_maker: Option<TimeoutMakerCb>,
}

/// A connection to D-Bus, async version, which is Send but not Sync.
pub struct Connection {
    channel: Channel,
    filters: RefCell<Filters<FilterCb>>,
    replies: RefCell<Replies<RepliesCb>>,
    timeout_maker: Option<TimeoutMakerCb>,
}

/// A connection to D-Bus, Send + Sync + async version
pub struct SyncConnection {
    channel: Channel,
    filters: Mutex<Filters<SyncFilterCb>>,
    replies: Mutex<Replies<SyncRepliesCb>>,
    timeout_maker: Option<TimeoutMakerCb>,
}

use stdintf::org_freedesktop_dbus::DBus;

macro_rules! connimpl {
     ($c: ident, $cb: ident, $rcb: ident $(, $ss:tt)*) =>  {

type
    $cb = Box<dyn FnMut(Message, &$c) -> bool $(+ $ss)* + 'static>;
type
    $rcb = Box<dyn FnOnce(Message, &$c) $(+ $ss)* + 'static>;

impl From<Channel> for $c {
    fn from(x: Channel) -> Self {
        $c {
            channel: x,
            replies: Default::default(),
            filters: Default::default(),
            timeout_maker: None,
        }
    }
}

impl AsRef<Channel> for $c {
    fn as_ref(&self) -> &Channel { &self.channel }
}

impl Sender for $c {
    fn send(&self, msg: Message) -> Result<u32, ()> { self.channel.send(msg) }
}

impl MatchingReceiver for $c {
    type F = $cb;
    fn start_receive(&self, m: MatchRule<'static>, f: Self::F) -> Token {
        self.filters_mut().add(m, f)
    }
    fn stop_receive(&self, id: Token) -> Option<(MatchRule<'static>, Self::F)> {
        self.filters_mut().remove(id)
    }
}

impl NonblockReply for $c {
    type F = $rcb;
    fn send_with_reply(&self, msg: Message, f: Self::F) -> Result<Token, ()> {
        self.channel.send(msg).map(|x| {
            let t = Token(x as usize);
            self.replies_mut().insert(t, f);
            t
        })
    }
    fn cancel_reply(&self, id: Token) -> Option<Self::F> { self.replies_mut().remove(&id) }
    fn make_f<G: FnOnce(Message, &Self) + Send + 'static>(g: G) -> Self::F { Box::new(g) }
    fn timeout_maker(&self) -> Option<TimeoutMakerCb> { self.timeout_maker }
    fn set_timeout_maker(&mut self, f: Option<TimeoutMakerCb>) -> Option<TimeoutMakerCb> {
        mem::replace(&mut self.timeout_maker, f)
    }
}


impl Process for $c {
    fn process_one(&self, msg: Message) {
        if let Some(serial) = msg.get_reply_serial() {
            if let Some(f) = self.replies_mut().remove(&Token(serial as usize)) {
                f(msg, self);
                return;
            }
        }
        let ff = self.filters_mut().remove_matching(&msg);
        if let Some(mut ff) = ff {
            if ff.2(msg, self) {
                self.filters_mut().insert(ff);
            }
        } else if let Some(reply) = crate::channel::default_reply(&msg) {
            let _ = self.send(reply);
        }
    }
}

impl $c {
    fn dbus_proxy(&self) -> Proxy<&Self> {
        Proxy::new("org.freedesktop.DBus", "/org/freedesktop/DBus", Duration::from_secs(10), self)
    }

    /// Get the connection's unique name.
    ///
    /// It's usually something like ":1.54"
    pub fn unique_name(&self) -> BusName { self.channel.unique_name().unwrap().into() }

    /// Request a name on the D-Bus.
    ///
    /// For detailed information on the flags and return values, see the libdbus documentation.
    pub async fn request_name<'a, N: Into<BusName<'a>>>(&self, name: N, allow_replacement: bool, replace_existing: bool, do_not_queue: bool)
    -> Result<stdintf::org_freedesktop_dbus::RequestNameReply, Error> {
        let flags: u32 =
            if allow_replacement { 1 } else { 0 } +
            if replace_existing { 2 } else { 0 } +
            if do_not_queue { 4 } else { 0 };
        let r = self.dbus_proxy().request_name(&name.into(), flags).await?;
        use stdintf::org_freedesktop_dbus::RequestNameReply::*;
        let all = [PrimaryOwner, InQueue, Exists, AlreadyOwner];
        all.iter().find(|x| **x as u32 == r).copied().ok_or_else(||
            crate::Error::new_failed("Invalid reply from DBus server")
        )
    }

    /// Release a previously requested name on the D-Bus.
    pub async fn release_name<'a, N: Into<BusName<'a>>>(&self, name: N) -> Result<stdintf::org_freedesktop_dbus::ReleaseNameReply, Error> {
        let r = self.dbus_proxy().release_name(&name.into()).await?;
        use stdintf::org_freedesktop_dbus::ReleaseNameReply::*;
        let all = [Released, NonExistent, NotOwner];
        all.iter().find(|x| **x as u32 == r).copied().ok_or_else(||
            crate::Error::new_failed("Invalid reply from DBus server")
        )
    }

    /// Adds a new match to the connection, without setting up a callback when this message arrives.
    pub async fn add_match_no_cb(&self, match_str: &str) -> Result<(), Error> {
        self.dbus_proxy().add_match(match_str).await
    }

    /// Removes a match from the connection, without removing any callbacks.
    pub async fn remove_match_no_cb(&self, match_str: &str) -> Result<(), Error> {
        self.dbus_proxy().remove_match(match_str).await
    }
}


    }
}

connimpl!(Connection, FilterCb, RepliesCb, Send);
connimpl!(LocalConnection, LocalFilterCb, LocalRepliesCb);
connimpl!(SyncConnection, SyncFilterCb, SyncRepliesCb, Send);

impl Connection {
    fn filters_mut(&self) -> std::cell::RefMut<Filters<FilterCb>> { self.filters.borrow_mut() }
    fn replies_mut(&self) -> std::cell::RefMut<Replies<RepliesCb>> { self.replies.borrow_mut() }
}

impl LocalConnection {
    fn filters_mut(&self) -> std::cell::RefMut<Filters<LocalFilterCb>> { self.filters.borrow_mut() }
    fn replies_mut(&self) -> std::cell::RefMut<Replies<LocalRepliesCb>> { self.replies.borrow_mut() }
}

impl SyncConnection {
    fn filters_mut(&self) -> std::sync::MutexGuard<Filters<SyncFilterCb>> { self.filters.lock().unwrap() }
    fn replies_mut(&self) -> std::sync::MutexGuard<Replies<SyncRepliesCb>> { self.replies.lock().unwrap() }
}

/// Internal callback for the executor when a timeout needs to be made.
pub type TimeoutMakerCb = fn(timeout: Instant) -> pin::Pin<Box<dyn Future<Output=()> + Send + Sync + 'static>>;

/// Internal helper trait for async method replies.
pub trait NonblockReply {
    /// Callback type
    type F;
    /// Sends a message and calls the callback when a reply is received.
    fn send_with_reply(&self, msg: Message, f: Self::F) -> Result<Token, ()>;
    /// Cancels a pending reply.
    fn cancel_reply(&self, id: Token) -> Option<Self::F>;
    /// Internal helper function that creates a callback.
    fn make_f<G: FnOnce(Message, &Self) + Send + 'static>(g: G) -> Self::F where Self: Sized;
    /// Set the internal timeout maker
    fn set_timeout_maker(&mut self, f: Option<TimeoutMakerCb>) -> Option<TimeoutMakerCb>;
    /// Get the internal timeout maker
    fn timeout_maker(&self) -> Option<TimeoutMakerCb>;
}


/// Internal helper trait, implemented for connections that process incoming messages.
pub trait Process: Sender + AsRef<Channel> {
    /// Dispatches all pending messages, without blocking.
    ///
    /// This is usually called from the reactor only, after read_write.
    /// Despite this taking &self and not "&mut self", it is a logic error to call this
    /// recursively or from more than one thread at a time.
    fn process_all(&self) {
        let c: &Channel = self.as_ref();
        while let Some(msg) = c.pop_message() {
            self.process_one(msg);
        }
    }

    /// Dispatches a message.
    fn process_one(&self, msg: Message);
}

/// A struct that wraps a connection, destination and path.
///
/// A D-Bus "Proxy" is a client-side object that corresponds to a remote object on the server side.
/// Calling methods on the proxy object calls methods on the remote object.
/// Read more in the [D-Bus tutorial](https://dbus.freedesktop.org/doc/dbus-tutorial.html#proxies)
#[derive(Clone, Debug)]
pub struct Proxy<'a, C> {
    /// Destination, i e what D-Bus service you're communicating with
    pub destination: BusName<'a>,
    /// Object path on the destination
    pub path: Path<'a>,
    /// Some way to send and/or receive messages, non-blocking.
    pub connection: C,
    /// Timeout for method calls
    pub timeout: Duration,
}

impl<'a, C> Proxy<'a, C> {
    /// Creates a new proxy struct.
    pub fn new<D: Into<BusName<'a>>, P: Into<Path<'a>>>(dest: D, path: P, timeout: Duration, connection: C) -> Self {
        Proxy { destination: dest.into(), path: path.into(), timeout, connection }
    }
}

struct MRAwait {
    mrouter: MROuter,
    token: Result<Token, ()>,
    timeout: Instant,
    timeoutfn: Option<TimeoutMakerCb>
}

async fn method_call_await(mra: MRAwait) -> Result<Message, Error> {
    use futures::future;
    let MRAwait { mrouter, token, timeout, timeoutfn } = mra;
    if token.is_err() { return Err(Error::new_failed("Failed to send message")) };
    let timeout = if let Some(tfn) = timeoutfn { tfn(timeout) } else { Box::pin(future::pending()) };
    match future::select(mrouter, timeout).await {
        future::Either::Left((r, _)) => r,
        future::Either::Right(_) => Err(Error::new_custom("org.freedesktop.DBus.Error.Timeout", "Timeout waiting for reply")),
    }
}

impl<'a, T, C> Proxy<'a, C>
where
    T: NonblockReply,
    C: std::ops::Deref<Target=T>
{

    fn method_call_setup(&self, msg: Message) -> MRAwait {
        let mr = Arc::new(Mutex::new(MRInner::Neither));
        let mrouter = MROuter(mr.clone());
        let f = T::make_f(move |msg: Message, _: &T| {
            let mut inner = mr.lock().unwrap();
            let old = mem::replace(&mut *inner, MRInner::Ready(Ok(msg)));
            if let MRInner::Pending(waker) = old { waker.wake() }
        });

        let timeout = Instant::now() + self.timeout;
        let token = self.connection.send_with_reply(msg, f);
        let timeoutfn = self.connection.timeout_maker();
        MRAwait { mrouter, token, timeout, timeoutfn }
    }

    /// Make a method call using typed input argument, returns a future that resolves to the typed output arguments.
    pub fn method_call<'i, 'm, R: ReadAll + 'static, A: AppendAll, I: Into<Interface<'i>>, M: Into<Member<'m>>>(&self, i: I, m: M, args: A)
    -> MethodReply<R> {
        let mut msg = Message::method_call(&self.destination, &self.path, &i.into(), &m.into());
        args.append(&mut IterAppend::new(&mut msg));
        let mra = self.method_call_setup(msg);
        let r = method_call_await(mra);
        let r = futures::FutureExt::map(r, |r| -> Result<R, Error> { r.and_then(|rmsg| rmsg.read_all()) } );
        MethodReply::new(r)
    }
}

enum MRInner {
    Ready(Result<Message, Error>),
    Pending(task::Waker),
    Neither,
}

struct MROuter(Arc<Mutex<MRInner>>);

impl Future for MROuter {
    type Output = Result<Message, Error>;
    fn poll(self: pin::Pin<&mut Self>, ctx: &mut task::Context) -> task::Poll<Self::Output> {
        let mut inner = self.0.lock().unwrap();
        let r = mem::replace(&mut *inner, MRInner::Neither);
        if let MRInner::Ready(r) = r { task::Poll::Ready(r) }
        else {
            mem::replace(&mut *inner, MRInner::Pending(ctx.waker().clone()));
            return task::Poll::Pending
        }
    }
}

/// Future method reply, used while waiting for a method call reply from the server.
pub struct MethodReply<T>(pin::Pin<Box<dyn Future<Output=Result<T, Error>> + Send + 'static>>);

impl<T> MethodReply<T> {
    /// Creates a new method reply from a future.
    fn new<Fut: Future<Output=Result<T, Error>> + Send + 'static>(fut: Fut) -> Self {
        MethodReply(Box::pin(fut))
    }
}

impl<T> Future for MethodReply<T> {
    type Output = Result<T, Error>;
    fn poll(mut self: pin::Pin<&mut Self>, ctx: &mut task::Context) -> task::Poll<Result<T, Error>> {
        self.0.as_mut().poll(ctx)
    }
}

impl<T: 'static> MethodReply<T> {
    /// Convenience combinator in case you want to post-process the result after reading it
    pub fn and_then<T2>(self, f: impl FnOnce(T) -> Result<T2, Error> + Send + Sync + 'static) -> MethodReply<T2> {
        MethodReply(Box::pin(async move {
            let x = self.0.await?;
            f(x)
        }))
    }
}

#[test]
fn test_conn_send_sync() {
    fn is_send<T: Send>(_: &T) {}
    fn is_sync<T: Sync>(_: &T) {}
    let c = SyncConnection::from(Channel::get_private(crate::channel::BusType::Session).unwrap());
    is_send(&c);
    is_sync(&c);

    let c = Connection::from(Channel::get_private(crate::channel::BusType::Session).unwrap());
    is_send(&c);
}
