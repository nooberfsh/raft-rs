//! `Server` is a Rust type which is responsible for coordinating with other remote `Server`
//! instances, responding to commands from the `Client`, and applying commands to a local
//! `StateMachine` consensus. A `Server` may be a `Leader`, `Follower`, or `Candidate` at any given
//! time as described by the Raft Consensus Algorithm.

use std::{fmt, io};
use std::str::FromStr;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::thread::{self, JoinHandle};
use std::rc::Rc;

use mio::tcp::TcpListener;
use mio::{Poll, Ready, PollOpt, Token};
use capnp::message::{Builder, HeapAllocator};
use slab;

use ClientId;
use Result;
use Error;
use RaftError;
use ServerId;
use messages;
use messages_capnp::connection_preamble;
use consensus::{Consensus, Actions, ConsensusTimeout, TimeoutConfiguration};
use state_machine::StateMachine;
use persistent_log::Log;
use connection::{Connection, ConnectionKind};

const LISTENER: Token = Token(0);

type Slab<T> = slab::Slab<T, Token>;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum ServerTimeout {
    Consensus(ConsensusTimeout),
    Reconnect(Token),
}

pub struct ServerBuilder<L, M>
where
    L: Log,
    M: StateMachine,
{
    id: ServerId,
    addr: SocketAddr,
    peers: Option<HashMap<ServerId, SocketAddr>>,
    store: L,
    state_machine: M,
    max_connections: usize,
    election_min_millis: u64,
    election_max_millis: u64,
    heartbeat_millis: u64,
}

impl <L, M> ServerBuilder<L, M>
where
    L: Log,
    M: StateMachine,
{
    fn new(id: ServerId, addr: SocketAddr, store: L, state_machine: M) -> ServerBuilder<L, M> {
        /// Create a ServerBuilder with default values
        /// for optional members.
        ServerBuilder {
            id: id,
            addr: addr,
            peers: None,
            store: store,
            state_machine: state_machine,
            max_connections: 128,
            election_min_millis: 150,
            election_max_millis: 350,
            heartbeat_millis: 60,
        }
    }

    pub fn finalize(self) -> Result<Server<L, M>> {
        Server::finalize(
            self.id,
            self.addr,
            self.peers.unwrap_or_else(HashMap::new),
            self.store,
            self.state_machine,
            self.election_min_millis,
            self.election_max_millis,
            self.heartbeat_millis,
            self.max_connections,
        )
    }

    pub fn run(self) -> Result<()> {
        let mut server = self.finalize()?;
        server.run()
    }

    pub fn with_max_connections(mut self, count: usize) -> ServerBuilder<L, M> {
        self.max_connections = count;
        self
    }

    pub fn with_election_min_millis(mut self, timeout: u64) -> ServerBuilder<L, M> {
        self.election_min_millis = timeout;
        self
    }

    pub fn with_election_max_millis(mut self, timeout: u64) -> ServerBuilder<L, M> {
        self.election_max_millis = timeout;
        self
    }

    pub fn with_heartbeat_millis(mut self, timeout: u64) -> ServerBuilder<L, M> {
        self.heartbeat_millis = timeout;
        self
    }

    pub fn with_peers(mut self, peers: HashMap<ServerId, SocketAddr>) -> ServerBuilder<L, M> {
        self.peers = Some(peers);
        self
    }
}

/// The `Server` is responsible for receiving ready from peer `Server` instance or clients,
/// as well as managing election and heartbeat timeouts. When an event is received, it is applied
/// to the local `Consensus`. The `Consensus` may optionally return a set of ready to be
/// dispatched to either remote peers or clients.
///
/// ## Logging
///
/// Server instances log ready according to frequency and importance. It is recommended to use at
/// least info level logging when running in production. The warn level is used for unexpected,
/// but recoverable ready. The info level is used for infrequent ready such as connection resets
/// and election results. The debug level is used for frequent ready such as client proposals and
/// heartbeats. The trace level is used for very high frequency debugging output.
pub struct Server<L, M>
    where L: Log,
          M: StateMachine
{
    /// Id of this server.
    id: ServerId,

    /// Raft state machine consensus.
    consensus: Consensus<L, M>,

    /// Connection listener.
    listener: TcpListener,

    /// Collection of connections indexed by token.
    connections: Slab<Connection>,

    /// Index of peer id to connection token.
    peer_tokens: HashMap<ServerId, Token>,

    /// Index of client id to connection token.
    client_tokens: HashMap<ClientId, Token>,

    /// Currently registered consensus timeouts.
    consensus_timeouts: HashMap<ConsensusTimeout, TimeoutHandle>,

    /// Currently registered reconnection timeouts.
    reconnection_timeouts: HashMap<Token, TimeoutHandle>,

    /// Configured timeouts
    timeout_config: TimeoutConfiguration,

    /// Poll
    poll: Poll,
}

fn all_interests() -> Ready {
    Ready::readable() | Ready::writable() | Ready::error() | Ready::hup()
}

/// The implementation of the Server.
impl<L, M> Server<L, M>
    where L: Log,
          M: StateMachine
{
    #[cfg_attr(feature = "cargo-clippy", allow(new_ret_no_self))]
    pub fn new(
        id: ServerId,
        addr: SocketAddr,
        store: L,
        state_machine: M,) -> ServerBuilder<L, M> {
        ServerBuilder::new(id, addr, store, state_machine)
    }

    /// Creates a new instance of the server.
    /// *Gotcha:* `peers` must not contain the local `id`.
    #[cfg_attr(feature = "cargo-clippy", allow(too_many_arguments))]
    fn finalize(
            id: ServerId,
            addr: SocketAddr,
            peers: HashMap<ServerId, SocketAddr>,
            store: L,
            state_machine: M,
            election_min_millis: u64,
            election_max_millis: u64,
            heartbeat_millis: u64,
            max_connections: usize)
            -> Result<Server<L, M>> {
        if peers.contains_key(&id) {
            return Err(Error::Raft(RaftError::InvalidPeerSet));
        }

        let timeout_config = TimeoutConfiguration {
            election_min_ms: election_min_millis,
            election_max_ms: election_max_millis,
            heartbeat_ms: heartbeat_millis,
        };
        let consensus = Consensus::new(id, peers.clone(), store, state_machine);
        let listener = try!(TcpListener::bind(&addr));

        let mut server = Server {
            id: id,
            consensus: consensus,
            listener: listener,
            connections: Slab::new_starting_at(Token(1), max_connections),
            peer_tokens: HashMap::new(),
            client_tokens: HashMap::new(),
            consensus_timeouts: HashMap::new(),
            reconnection_timeouts: HashMap::new(),
            timeout_config: timeout_config,
            poll: Poll::new()?,
        };

        for (peer_id, peer_addr) in peers {
            let token: Token = try!(server.connections
                                          .insert(try!(Connection::peer(peer_id, peer_addr)))
                                          .map_err(|_| {
                                              Error::Raft(RaftError::ConnectionLimitReached)
                                          }));
            scoped_assert!(server.peer_tokens.insert(peer_id, token).is_none());
        }
        Ok(server)
    }

    fn start_loop(&mut self) -> Result<()>
    where
        L: Log,
        M: StateMachine
    {
        self.poll.register(&self.listener, LISTENER, all_interests(), PollOpt::level())?;
        let mut tokens = vec![];
        for token in self.peer_tokens.values() {
            tokens.push(*token);
        }
        let id = self.id;
        let addr = self.listener.local_addr()?;
        for token in tokens {
            self.connections[token].register(&self.poll, token)?;
            self.send_message(
                                token,
                                messages::server_connection_preamble(id, &addr));
        }
        Ok(())
    }
    /// Runs a new Raft server in the current thread.
    ///
    /// # Arguments
    ///
    /// * `id` - The ID of the new node.
    /// * `addr` - The address of the new node.
    /// * `peers` - The ID and address of all peers in the Raft cluster.
    /// * `store` - The persistent log store.
    /// * `state_machine` - The client state machine to which client commands will be applied.
    pub fn run(&mut self) -> Result<()> {
        self.start_loop()?;
        let actions = self.consensus.init();
        self.execute_actions(actions);
        poll.run(self).map_err(From::from)
    }

    /// Spawns a new Raft server in a background thread.
    ///
    /// # Arguments
    ///
    /// * `id` - The ID of the new node.
    /// * `addr` - The address of the new node.
    /// * `peers` - The ID and address of all peers in the Raft cluster.
    /// * `store` - The persistent log store.
    /// * `state_machine` - The client state machine to which client commands will be applied.
    pub fn spawn(id: ServerId,
                 addr: SocketAddr,
                 peers: HashMap<ServerId, SocketAddr>,
                 store: L,
                 state_machine: M)
                 -> Result<JoinHandle<Result<()>>> {
        thread::Builder::new()
            .name(format!("raft::Server({})", id))
            .spawn(move || {
                let mut server = try!(Server::finalize(id, addr, peers, store, state_machine, 1500, 3000, 1000, 129));
                server.run()
            })
            .map_err(From::from)
    }
    /// Sends the message to the connection associated with the provided token.
    /// If sending the message fails, the connection is reset.
    fn send_message(&mut self,
                    token: Token,
                    message: Rc<Builder<HeapAllocator>>) {
        match self.connections[token].send_message(message) {
            Ok(false) => (),
            Ok(true) => {
                self.connections[token]
                    .reregister(&self.poll, token)
                    .unwrap_or_else(|_| self.reset_connection(token));
            }
            Err(error) => {
                scoped_warn!("{:?}: error while sending message: {:?}", self, error);
                self.reset_connection(token);
            }
        }
    }

    fn execute_actions(&mut self, actions: Actions) {
        scoped_trace!("executing actions: {:?}", actions);
        let Actions { peer_messages,
                      client_messages,
                      timeouts,
                      clear_timeouts,
                      clear_peer_messages } = actions;

        if clear_peer_messages {
            for &token in self.peer_tokens.values() {
                self.connections[token].clear_messages();
            }
        }
        for (peer, message) in peer_messages {
            let token = self.peer_tokens[&peer];
            self.send_message(token, message);
        }
        for (client, message) in client_messages {
            if let Some(&token) = self.client_tokens.get(&client) {
                self.send_message(token, message);
            }
        }
        if clear_timeouts {
            for (timeout, &handle) in &self.consensus_timeouts {
                scoped_assert!(&self.poll.clear_timeout(handle),
                               "unable to clear timeout: {:?}",
                               timeout);
            }
            self.consensus_timeouts.clear();
        }
        for timeout in timeouts {
            let duration = timeout.duration_ms(&self.timeout_config);

            // Registering a timeout may only fail if the maximum number of timeouts
            // is already registered, which is by default 65,536. We use a
            // maximum of one timeout per peer, so this unwrap should be safe.
            let handle = &self.poll.timeout_ms(ServerTimeout::Consensus(timeout), duration)
                                   .unwrap();
            self.consensus_timeouts
                .insert(timeout, handle)
                .map(|handle| {
                    //todo;
                    //scoped_assert!(&self.poll.clear_timeout(handle),
                                   //"unable to clear timeout: {:?}",
                                   //timeout)
                });
        }
    }

    /// Resets the connection corresponding to the provided token.
    ///
    /// If the connection is to a peer, the server will attempt to reconnect after a waiting
    /// period.
    ///
    /// If the connection is to a client or unknown it will be closed.
    fn reset_connection(&mut self, token: Token) {
        let kind = *self.connections[token].kind();
        match kind {
            ConnectionKind::Peer(..) => {
                // Crash if reseting the connection fails.
                let (timeout, handle) = self.connections[token]
                                            .reset_peer(&self.poll, token)
                                            .unwrap();

                scoped_assert!(self.reconnection_timeouts.insert(token, handle).is_none(),
                               "timeout already registered: {:?}",
                               timeout);
            }
            ConnectionKind::Client(ref id) => {
                self.connections.remove(token).expect("unable to find client connection");
                scoped_assert!(self.client_tokens.remove(id).is_some(),
                               "client {:?} not connected",
                               id);
            }
            ConnectionKind::Unknown => {
                self.connections.remove(token).expect("unable to find unknown connection");
            }
        }
    }

    /// Reads messages from the connection until no more are available.
    ///
    /// If the connection returns an error on any operation, or any message fails to be
    /// deserialized, an error result is returned.
    fn readable(&mut self, token: Token) -> Result<()> {
        scoped_trace!("{:?}: readable event", self.connections[token]);
        // Read messages from the connection until there are no more.
        while let Some(message) = try!(self.connections[token].readable()) {
            match *self.connections[token].kind() {
                ConnectionKind::Peer(id) => {
                    let mut actions = Actions::new();
                    self.consensus.apply_peer_message(id, &message, &mut actions);
                    self.execute_actions(&self.poll, actions);
                }
                ConnectionKind::Client(id) => {
                    let mut actions = Actions::new();
                    self.consensus.apply_client_message(id, &message, &mut actions);
                    self.execute_actions(&self.poll, actions);
                }
                ConnectionKind::Unknown => {
                    let preamble = try!(message.get_root::<connection_preamble::Reader>());
                    match try!(preamble.get_id().which()) {
                        connection_preamble::id::Which::Server(peer) => {
                            let peer = try!(peer);
                            let peer_id = ServerId(peer.get_id());

                            // Not the source address of this connection, but the
                            // address the peer tells us it's listening on.
                            let peer_addr = SocketAddr::from_str(try!(peer.get_addr())).unwrap();
                            scoped_debug!("received new connection from {:?} ({})",
                                          peer_id,
                                          peer_addr);

                            self.connections[token].set_kind(ConnectionKind::Peer(peer_id));
                            // Use the advertised address, not the remote's source
                            // address, for future retries in this connection.
                            self.connections[token].set_addr(peer_addr);

                            let prev_token = Some(self.peer_tokens
                                                      .insert(peer_id, token)
                                                      .expect("peer token not found"));

                            // Close the existing connection, if any.
                            // Currently, prev_token is never `None`; see above.
                            // With config changes, this will have to be handled.
                            match prev_token {
                                Some(tok) => {
                                    self.connections
                                        .remove(tok)
                                        .expect("peer connection not found");

                                    // Clear any timeouts associated with the existing connection.
                                    self.reconnection_timeouts
                                        .remove(&tok)
                                        .map(|handle| {
                                            scoped_assert!(&self.poll.clear_timeout(handle))
                                        });
                                }
                                _ => unreachable!(),
                            }
                            // Notify consensus that the connection reset.
                            let mut actions = Actions::new();
                            self.consensus.peer_connection_reset(peer_id, peer_addr, &mut actions);
                            self.execute_actions(&self.poll, actions);
                        }
                        connection_preamble::id::Which::Client(Ok(id)) => {
                            let client_id = try!(ClientId::from_bytes(id));
                            scoped_debug!("received new client connection from {}", client_id);
                            self.connections[token].set_kind(ConnectionKind::Client(client_id));
                            let prev_token = self.client_tokens
                                                 .insert(client_id, token);
                            scoped_assert!(prev_token.is_none(),
                                           "{:?}: two clients connected with the same id: {:?}",
                                           self,
                                           client_id);
                        }
                        _ => {
                            return Err(Error::Raft(RaftError::UnknownConnectionType));
                        }
                    }
                }
            }
        }
        Ok(())
    }

    /// Accepts a new TCP connection, adds it to the connection slab, and registers it with the
    /// event loop.
    fn accept_connection(&mut self) -> Result<()> {
        scoped_trace!("accept_connection");
        self.listener
            .accept()
            .map_err(From::from)
            .and_then(|stream_opt| {
                stream_opt.ok_or_else(|| {
                    Error::Io(io::Error::new(io::ErrorKind::WouldBlock,
                                             "listener.accept() returned None"))
                })
            })
            .and_then(|(stream, _)| Connection::unknown(stream))
            .and_then(|conn| {
                self.connections
                    .insert(conn)
                    .map_err(|_| Error::Raft(RaftError::ConnectionLimitReached))
            })
            .and_then(|token|
                // Until this point if any failures occur the connection is simply dropped. From
                // this point down, the connection is stored in the slab, so dropping it would
                // result in a leaked TCP stream and slab entry. Instead of dropping the
                // connection, it will be reset if an error occurs.
                self.connections[token]
                    .register(&self.poll, token)
                    .or_else(|_| {
                        self.reset_connection(&self.poll, token);
                        Err(Error::Raft(RaftError::ConnectionRegisterFailed))
                    })
                    .map(|_| scoped_debug!("new connection accepted from {}",
                                           self.connections[token].addr())))
    }
}

impl<L, M> Handler for Server<L, M>
    where L: Log,
          M: StateMachine
{
    type Message = ();
    type Timeout = ServerTimeout;

    fn ready(&mut self, token: Token, ready: Ready) {
        info!("{:?}", self);
        scoped_trace!("ready; token: {:?}; ready: {:?}", token, ready);

        if ready.is_error() {
            scoped_assert!(token != LISTENER, "unexpected error event from LISTENER");
            scoped_warn!("{:?}: error event", self.connections[token]);
            self.reset_connection(&self.poll, token);
            return;
        }

        if ready.is_hup() {
            scoped_assert!(token != LISTENER, "unexpected hup event from LISTENER");
            scoped_trace!("{:?}: hup event", self.connections[token]);
            self.reset_connection(&self.poll, token);
            return;
        }

        if ready.is_writable() {
            scoped_assert!(token != LISTENER, "unexpected writeable event for LISTENER");
            if let Err(error) = self.connections[token].writable() {
                scoped_warn!("{:?}: failed write: {}", self.connections[token], error);
                self.reset_connection(&self.poll, token);
                return;
            }
            if !ready.is_readable() {
                self.connections[token]
                    .reregister(&self.poll, token)
                    .unwrap_or_else(|_| self.reset_connection(&self.poll, token));
            }
        }

        if ready.is_readable() {
            if token == LISTENER {
                self.accept_connection(&self.poll)
                    .unwrap_or_else(|error| scoped_warn!("unable to accept connection: {}", error));
            } else {
                self.readable(&self.poll, token)
                    // Only reregister the connection with the event loop if no error occurs and
                    // the connection is *not* reset.
                    .and_then(|_| self.connections[token].reregister(&self.poll, token))
                    .unwrap_or_else(|error| {
                        scoped_warn!("{:?}: failed read: {}",
                                     self.connections[token], error);
                        self.reset_connection(&self.poll, token);
                    });
            }
        }
    }

    fn timeout(&mut self, timeout: ServerTimeout) {
        info!("{:?}", self);
        scoped_trace!("timeout: {:?}", &timeout);
        match timeout {
            ServerTimeout::Consensus(consensus) => {
                scoped_assert!(self.consensus_timeouts.remove(&consensus).is_some(),
                               "missing timeout: {:?}",
                               timeout);
                let mut actions = Actions::new();
                self.consensus.apply_timeout(consensus, &mut actions);
                self.execute_actions(&self.poll, actions);
            }

            ServerTimeout::Reconnect(token) => {
                scoped_assert!(self.reconnection_timeouts.remove(&token).is_some(),
                               "{:?} missing timeout: {:?}",
                               self.connections[token],
                               timeout);
                let local_addr = self.listener.local_addr();
                scoped_assert!(local_addr.is_ok(), "could not obtain listener address");
                let id = match *self.connections[token].kind() {
                    ConnectionKind::Peer(id) => id,
                    _ => unreachable!(),
                };
                let addr = *self.connections[token].addr();
                self.connections[token]
                    .reconnect_peer(self.id, &local_addr.unwrap())
                    .and_then(|_| self.connections[token].register(&self.poll, token))
                    .map(|_| {
                        let mut actions = Actions::new();
                        self.consensus.peer_connection_reset(id, addr, &mut actions);
                        self.execute_actions(&self.poll, actions);
                    })
                    .unwrap_or_else(|error| {
                        scoped_warn!("unable to reconnect connection {:?}: {}",
                                     self.connections[token],
                                     error);
                        self.reset_connection(&self.poll, token);
                    });
            }
        }
    }
}

impl<L, M> fmt::Debug for Server<L, M>
    where L: Log,
          M: StateMachine
{
    fn fmt(&self, fmt: &mut fmt::Formatter) -> fmt::Result {
        write!(fmt, "Server({})", self.id)
    }
}

#[cfg(test)]
mod tests {

    extern crate env_logger;

    use std::collections::HashMap;
    use std::io::{self, Read, Write};
    use std::net::{SocketAddr, TcpListener, TcpStream};
    use std::str::FromStr;

    use capnp::message::ReaderOptions;
    use capnp::serialize;
    use mio::EventLoop;

    use ClientId;
    use Result;
    use ServerId;
    use messages;
    use messages_capnp::connection_preamble;
    use consensus::Actions;
    use state_machine::NullStateMachine;
    use persistent_log::MemLog;
    use super::*;

    type TestServer = Server<MemLog, NullStateMachine>;

    fn new_test_server(peers: HashMap<ServerId, SocketAddr>)
                       -> Result<(TestServer, EventLoop<TestServer>)> {
        let mut server = try!(Server::new(ServerId::from(0),
                                          SocketAddr::from_str("127.0.0.1:0").unwrap(),
                                          MemLog::new(),
                                          NullStateMachine)
                                          .with_peers(peers)
                                          .with_election_min_millis(1500)
                                          .with_election_max_millis(3000)
                                          .with_heartbeat_millis(1000)
                                          .with_max_connections(129)
                                          .finalize());
        let poll = try!(server.start_loop());
        Ok((server, poll))
    }

    /// Attempts to grab a local, unbound socket address for testing.
    fn get_unbound_address() -> SocketAddr {
        TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap()
    }

    /// Verifies that the proved stream has been sent a valid connection
    /// preamble.
    fn read_server_preamble<R>(read: &mut R) -> ServerId
        where R: Read
    {
        let message = serialize::read_message(read, ReaderOptions::new()).unwrap();
        let preamble = message.get_root::<connection_preamble::Reader>().unwrap();

        match preamble.get_id().which().unwrap() {
            connection_preamble::id::Which::Server(peer) => ServerId::from(peer.unwrap().get_id()),
            _ => {
                panic!("unexpected preamble id");
            }
        }
    }

    /// Returns true if the server has an open connection with the peer.
    fn peer_connected(server: &TestServer, peer: ServerId) -> bool {
        let token = server.peer_tokens[&peer];
        server.reconnection_timeouts.get(&token).is_none()
    }

    /// Returns true if the server has an open connection with the client.
    fn client_connected(server: &TestServer, client: ClientId) -> bool {
        server.client_tokens.contains_key(&client)
    }

    /// Returns true if the provided TCP connection has been shutdown.
    ///
    /// TODO: figure out a more robust way to implement this, the current check
    /// will block the thread indefinitely if the stream is not shutdown.
    fn stream_shutdown(stream: &mut TcpStream) -> bool {
        let mut buf = [0u8; 128];
        // OS X returns a read of 0 length for closed sockets.
        // Linux returns an errcode 104: Connection reset by peer.
        match stream.read(&mut buf) {
            Ok(0) => true,
            Err(ref error) if error.kind() == io::ErrorKind::ConnectionReset => true,
            Err(ref error) => panic!("unexpected error: {}", error),
            _ => false,
        }
    }

    /// Tests that a Server will reject an invalid peer configuration set.
    #[test]
    fn test_illegal_peer_set() {
        setup_test!("test_illegal_peer_set");
        let peer_id = ServerId::from(0);
        let mut peers = HashMap::new();
        peers.insert(peer_id, SocketAddr::from_str("127.0.0.1:0").unwrap());
        assert!(new_test_server(peers).is_err());
    }

    /// Tests that a Server connects to peer at startup, and reconnects when the
    /// connection is dropped.
    #[test]
    fn test_peer_connect() {
        setup_test!("test_peer_connect");
        let peer_id = ServerId::from(1);

        let peer_listener = TcpListener::bind("127.0.0.1:0").unwrap();

        let mut peers = HashMap::new();
        peers.insert(peer_id, peer_listener.local_addr().unwrap());
        let (mut server, mut poll) = new_test_server(peers).unwrap();

        // Accept the server's connection.
        let (mut stream, _) = peer_listener.accept().unwrap();

        // Check that the server sends a valid preamble.
        assert_eq!(ServerId::from(0), read_server_preamble(&mut stream));
        assert!(peer_connected(&server, peer_id));

        // Drop the connection.
        drop(stream);
        poll.run_once(&mut server, None).unwrap();
        assert!(!peer_connected(&server, peer_id));

        // Check that the server reconnects after a timeout.
        poll.run_once(&mut server, None).unwrap();
        assert!(peer_connected(&server, peer_id));
        let (mut stream, _) = peer_listener.accept().unwrap();

        // Check that the server sends a valid preamble after the connection is
        // established.
        assert_eq!(ServerId::from(0), read_server_preamble(&mut stream));
        assert!(peer_connected(&server, peer_id));
    }

    /// Tests that a Server will replace a peer's TCP connection if the peer
    /// connects through another TCP connection.
    #[test]
    fn test_peer_accept() {
        setup_test!("test_peer_accept");
        let peer_id = ServerId::from(1);

        let peer_listener = TcpListener::bind("127.0.0.1:0").unwrap();

        let mut peers = HashMap::new();
        peers.insert(peer_id, peer_listener.local_addr().unwrap());
        let (mut server, mut poll) = new_test_server(peers).unwrap();

        // Accept the server's connection.
        let (mut in_stream, _) = peer_listener.accept().unwrap();

        // Check that the server sends a valid preamble.
        assert_eq!(ServerId::from(0), read_server_preamble(&mut in_stream));
        assert!(peer_connected(&server, peer_id));

        let server_addr = server.listener.local_addr().unwrap();

        // Open a replacement connection to the server.
        let mut out_stream = TcpStream::connect(server_addr).unwrap();
        poll.run_once(&mut server, None).unwrap();

        // This is what the new peer tells the server is listening address is.
        let fake_peer_addr = SocketAddr::from_str("192.168.0.1:12345").unwrap();
        // Send server the preamble message to the server.
        serialize::write_message(&mut out_stream,
                                 &*messages::server_connection_preamble(peer_id, &fake_peer_addr))
            .unwrap();
        out_stream.flush().unwrap();
        poll.run_once(&mut server, None).unwrap();

        // Make sure that reconnecting updated the peer address
        // known to `Consensus` with the one given in the preamble.
        assert_eq!(server.consensus.peers()[&peer_id], fake_peer_addr);
        // Check that the server has closed the old connection.
        assert!(stream_shutdown(&mut in_stream));
        // Check that there's a connection which has the fake address
        // stored for reconnection purposes.
        assert!(server.connections.iter().any(|conn| conn.addr().port() == 12345))
    }

    /// Tests that the server will accept a client connection, then disposes of
    /// it when the client disconnects.
    #[test]
    fn test_client_accept() {
        setup_test!("test_client_accept");

        let (mut server, mut poll) = new_test_server(HashMap::new()).unwrap();

        // Connect to the server.
        let server_addr = server.listener.local_addr().unwrap();
        let mut stream = TcpStream::connect(server_addr).unwrap();
        poll.run_once(&mut server, None).unwrap();

        let client_id = ClientId::new();

        // Send the client preamble message to the server.
        serialize::write_message(&mut stream,
                                 &*messages::client_connection_preamble(client_id))
            .unwrap();
        stream.flush().unwrap();
        poll.run_once(&mut server, None).unwrap();

        // Check that the server holds on to the client connection.
        assert!(client_connected(&server, client_id));

        // Check that the server disposes of the client connection when the TCP
        // stream is dropped.
        drop(stream);
        poll.run_once(&mut server, None).unwrap();
        assert!(!client_connected(&server, client_id));
    }

    /// Tests that the server will throw away connections that do not properly
    /// send a preamble.
    #[test]
    fn test_invalid_accept() {
        setup_test!("test_invalid_accept");

        let (mut server, mut poll) = new_test_server(HashMap::new()).unwrap();

        // Connect to the server.
        let server_addr = server.listener.local_addr().unwrap();
        let mut stream = TcpStream::connect(server_addr).unwrap();
        poll.run_once(&mut server, None).unwrap();

        // Send an invalid preamble.
        stream.write(b"foo bar baz").unwrap();
        stream.flush().unwrap();
        poll.run_once(&mut server, None).unwrap();

        // Check that the server disposes of the connection.
        assert!(stream_shutdown(&mut stream));
    }

    /// Tests that the server will reset a peer connection when an invalid
    /// message is received.
    #[test]
    fn test_invalid_peer_message() {
        setup_test!("test_invalid_peer_message");

        let peer_id = ServerId::from(1);

        let peer_listener = TcpListener::bind("127.0.0.1:0").unwrap();

        let mut peers = HashMap::new();
        peers.insert(peer_id, peer_listener.local_addr().unwrap());
        let (mut server, mut poll) = new_test_server(peers).unwrap();

        // Accept the server's connection.
        let (mut stream_a, _) = peer_listener.accept().unwrap();

        // Read the server's preamble.
        assert_eq!(ServerId::from(0), read_server_preamble(&mut stream_a));

        // Send an invalid message.
        stream_a.write(b"foo bar baz").unwrap();
        stream_a.flush().unwrap();
        poll.run_once(&mut server, None).unwrap();

        // Check that the server resets the connection.
        assert!(!peer_connected(&server, peer_id));

        // Check that the server reconnects after a timeout.
        poll.run_once(&mut server, None).unwrap();
        assert!(peer_connected(&server, peer_id));
    }

    /// Tests that the server will reset a client connection when an invalid
    /// message is received.
    #[test]
    fn test_invalid_client_message() {
        setup_test!("test_invalid_client_message");

        let (mut server, mut poll) = new_test_server(HashMap::new()).unwrap();

        // Connect to the server.
        let server_addr = server.listener.local_addr().unwrap();
        let mut stream = TcpStream::connect(server_addr).unwrap();
        poll.run_once(&mut server, None).unwrap();

        let client_id = ClientId::new();

        // Send the client preamble message to the server.
        serialize::write_message(&mut stream,
                                 &*messages::client_connection_preamble(client_id))
            .unwrap();
        stream.flush().unwrap();
        poll.run_once(&mut server, None).unwrap();

        // Check that the server holds on to the client connection.
        assert!(client_connected(&server, client_id));

        // Send an invalid client message to the server.
        stream.write(b"foo bar baz").unwrap();
        stream.flush().unwrap();
        poll.run_once(&mut server, None).unwrap();

        // Check that the server disposes of the client connection.
        assert!(!client_connected(&server, client_id));
    }

    /// Tests that a Server will attempt to connect to peers on startup, and
    /// immediately reset the connection if unreachable.
    #[test]
    fn test_unreachable_peer() {
        setup_test!("test_unreachable_peer_reconnect");
        let peer_id = ServerId::from(1);
        let mut peers = HashMap::new();
        peers.insert(peer_id, get_unbound_address());

        // Creates the Server, which registers the peer connection, and
        // immediately resets it.
        let (mut server, _) = new_test_server(peers).unwrap();
        assert!(!peer_connected(&mut server, peer_id));
    }

    /// Tests that the server will send a message to a peer connection.
    #[test]
    fn test_connection_send() {
        setup_test!("test_connection_send");
        let peer_id = ServerId::from(1);

        let peer_listener = TcpListener::bind("127.0.0.1:0").unwrap();

        let mut peers = HashMap::new();
        let peer_addr = peer_listener.local_addr().unwrap();
        peers.insert(peer_id, peer_addr);
        let (mut server, mut poll) = new_test_server(peers).unwrap();

        // Accept the server's connection.
        let (mut in_stream, _) = peer_listener.accept().unwrap();

        // Accept the preamble.
        assert_eq!(ServerId::from(0), read_server_preamble(&mut in_stream));

        // Send a test message (the type is not important).
        let mut actions = Actions::new();
        actions.peer_messages
               .push((peer_id, messages::server_connection_preamble(peer_id, &peer_addr)));
        server.execute_actions(&mut poll, actions);

        assert_eq!(peer_id, read_server_preamble(&mut in_stream));
    }
}
