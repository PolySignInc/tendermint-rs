//! Supervision of the p2p machinery managing peers and the flow of data from and to them.

use std::collections::{hash_map::Entry, HashMap, HashSet};
use std::convert::TryFrom as _;
use std::sync::Arc;
use std::sync::Mutex;
use std::thread;

use eyre::{eyre, Context, Report, Result};
use flume::{unbounded, Receiver, Sender};

use tendermint::node;

use crate::message;
use crate::peer;
use crate::transport::{self, Connection, Endpoint as _};

/// Indicates how a [`transport::Connection`] was established.
pub enum Direction {
    /// Established by accepting a new connection from the [`transport::Transport`].
    Incoming,
    /// Established by issuing a connect on the [`transport::Transport`].
    Outgoing,
}

/// Set of control instructions supported by the [`Supervisor`]. Intended to empower the caller to
/// instruct when to establish new connections and multiplex messages to peers.
pub enum Command {
    /// Accept next incoming connection. As it will unblock the subroutine which is responsible for
    /// accepting even when no incoming connection is pending, the accept can take place at a
    /// later point then when the command was issued. Protocols which rely on hard upper bounds
    /// like the number of concurrently connected peers should issue a disconnect to remedy the
    /// situation.
    Accept,
    /// Establishes a new connection to the remote end in [`transport::ConnectInfo`].
    Connect(transport::ConnectInfo),
    /// Disconnects the [`peer::Peer`] known by [`node::Id`]. This will tear down the entire tree of
    /// subroutines managing the peer in question.
    Disconnect(node::Id),
    /// Dispatch the given message to the peer known for [`node::Id`].
    Msg(node::Id, message::Send),
}

/// Set of significant events in the p2p subsystem.
pub enum Event {
    /// A new connection has been established.
    Connected(node::Id, Direction),
    /// A [`peer::Peer`] has been disconnected.
    Disconnected(node::Id, Report),
    /// A new [`message::Receive`] from the [`peer::Peer`] has arrived.
    Message(node::Id, message::Receive),
    /// A connection upgraded successfully to a [`peer::Peer`].
    Upgraded(node::Id),
    /// An upgrade from failed.
    UpgradeFailed(node::Id, Report),
    // TODO(xla): Add variant which expresses terminaation of the supervisor, so the caller can
    // drop it and possibly reconstruct it.
}

enum Internal {
    Accept,
    Connect(transport::ConnectInfo),
    SendMessage(node::Id, message::Send),
    Stop(node::Id),
    Upgrade(node::Id),
}

enum Output {
    Event(Event),
    Internal(Internal),
}

impl From<Event> for Output {
    fn from(event: Event) -> Self {
        Self::Event(event)
    }
}

impl From<Internal> for Output {
    fn from(internal: Internal) -> Self {
        Self::Internal(internal)
    }
}

enum Input {
    Accepted(node::Id),
    Command(Command),
    Connected(node::Id),
    DuplicateConnRejected(node::Id, Option<Report>),
    Receive(node::Id, message::Receive),
    Stopped(node::Id, Option<Report>),
    Upgraded(node::Id),
    UpgradeFailed(node::Id, Report),
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("state lock poisoned")]
    StateLockPoisoned,
}

struct State<Conn> {
    connected: HashMap<node::Id, transport::Direction<Conn>>,
    peers: HashMap<node::Id, peer::Peer<peer::Running<Conn>>>,
}

/// Wrapping a [`transport::Transport`] the `Supervisor` runs the p2p machinery to manage peers over
/// physical connections. Offering multiplexing of ingress and egress messages and a surface to
/// empower higher-level protocols to control the behaviour of the p2p substack.
///
/// TODO(xla): Document subroutine/thread hierarchy and data flow.
pub struct Supervisor {
    command_tx: Sender<Command>,
    event_rx: Receiver<Event>,
}

impl Supervisor {
    /// Takes the [`transport::Transport`] and sets up managed subroutines. When the `Supervisor`
    /// is returned the p2p subsystem has been successfully set up on the given network interface
    /// (as far as applicable for the transport) so the caller can use the command input and
    /// consume events.
    ///
    /// # Errors
    ///
    /// * If the bind of the transport fails
    pub fn run<T>(transport: T, info: transport::BindInfo) -> Result<Self>
    where
        T: transport::Transport + Send + 'static,
    {
        let (endpoint, incoming) = transport.bind(info)?;
        let (command_tx, command_rx) = unbounded();
        let (event_tx, event_rx) = unbounded();

        thread::spawn(move || Self::main::<T>(command_rx, event_tx, endpoint, incoming));

        Ok(Self {
            command_tx,
            event_rx,
        })
    }

    /// Returns the next available message from the underlying channel.
    ///
    /// A `None` signals that the supervisor is stopped and no further events will arrive.
    pub fn recv(&self) -> Option<Event> {
        self.event_rx.recv().ok()
    }

    /// Instruct to execute the given [`Command`].
    ///
    /// # Errors
    ///
    /// * If the underlying channels dropped and the receiver is gone and indicating that the
    /// handle the caller holds isn't any good anymore and should be dropped entirely.
    pub fn command(&self, cmd: Command) -> Result<()> {
        self.command_tx.send(cmd).wrap_err("command send failed")
    }
}

type Connected<T: transport::Transport + Send + 'static> =
    Arc<Mutex<HashMap<node::Id, transport::Direction<<T as transport::Transport>::Connection>>>>;
type Peers<T: transport::Transport + Send + 'static> = Arc<
    Mutex<HashMap<node::Id, peer::Peer<peer::Running<<T as transport::Transport>::Connection>>>>,
>;

impl Supervisor {
    fn main<T>(
        command_rx: Receiver<Command>,
        event_tx: Sender<Event>,
        endpoint: <T as transport::Transport>::Endpoint,
        incoming: <T as transport::Transport>::Incoming,
    ) where
        T: transport::Transport + Send + 'static,
    {
        let connected: Connected<T> = Arc::new(Mutex::new(HashMap::new()));
        let peers: Peers<T> = Arc::new(Mutex::new(HashMap::new()));

        let (input_tx, input_rx) = unbounded();

        let (accept_tx, accept_rx) = unbounded::<()>();
        let accept_handle = {
            let input_tx = input_tx.clone();
            let connected = connected.clone();
            thread::Builder::new()
                .name("supervisor-accept".to_string())
                .spawn(|| Self::accept::<T>(accept_rx, connected, incoming, input_tx))
        };

        let (connect_tx, connect_rx) = unbounded::<transport::ConnectInfo>();
        let connect_handle = {
            let input_tx = input_tx.clone();
            let connected = connected.clone();
            thread::Builder::new()
                .name("supervisor-connect".to_string())
                .spawn(|| Self::connect::<T>(connected, connect_rx, endpoint, input_tx))
        };

        let (msg_tx, msg_rx) = unbounded::<(node::Id, message::Send)>();
        let msg_handle = {
            let input_tx = input_tx.clone();
            let peers = peers.clone();
            thread::Builder::new()
                .name("supervisor-message".to_string())
                .spawn(move || Self::message::<T>(input_tx, msg_rx, peers))
        };

        let (stop_tx, stop_rx) = unbounded::<node::Id>();
        let stop_handle = {
            let input_tx = input_tx.clone();
            let peers = peers.clone();
            thread::Builder::new()
                .name("supervisor-stop".to_string())
                .spawn(move || Self::stop::<T>(input_tx, peers, stop_rx))
        };

        let (upgrade_tx, upgrade_rx) = unbounded();
        let upgrade_handle = {
            let connected = connected.clone();
            let peers = peers.clone();
            thread::Builder::new()
                .name("supervisor-upgrade".to_string())
                .spawn(move || Self::upgrade::<T>(connected, input_tx, peers, upgrade_rx))
        };

        let mut protocol = Protocol {
            connected: HashMap::new(),
            stopped: HashSet::new(),
            upgraded: HashSet::new(),
        };

        loop {
            let input = {
                let peers = match peers.lock() {
                    Ok(peers) => peers,
                    Err(_err) => {
                        // TODO(xla): The lock got poisoned and will stay in that state, we
                        // need to terminate, but should log an error here.
                        break;
                    }
                };

                let mut selector = flume::Selector::new()
                    .recv(&command_rx, |res| Input::Command(res.unwrap()))
                    .recv(&input_rx, |input| input.unwrap());

                for (id, peer) in &*peers {
                    selector = selector.recv(&peer.state.receiver, move |res| match res {
                        Ok(msg) => Input::Receive(*id, msg),
                        Err(flume::RecvError::Disconnected) => todo!(),
                    });
                }

                selector.wait()
            };

            for output in protocol.transition(input) {
                match output {
                    Output::Event(event) => event_tx.send(event).unwrap(),
                    Output::Internal(internal) => match internal {
                        Internal::Accept => accept_tx.send(()).unwrap(),
                        Internal::Connect(info) => connect_tx.send(info).unwrap(),
                        Internal::SendMessage(peer_id, msg) => msg_tx.send((peer_id, msg)).unwrap(),
                        Internal::Stop(peer_id) => stop_tx.send(peer_id).unwrap(),
                        Internal::Upgrade(peer_id) => upgrade_tx.send(peer_id).unwrap(),
                    },
                }
            }
        }

        // TODO(xla): Log the termination of main subroutine. Signal termination to the caller.
    }

    fn accept<T>(
        accept_rx: Receiver<()>,
        connected: Connected<T>,
        mut incoming: <T as transport::Transport>::Incoming,
        input_tx: Sender<Input>,
    ) -> Result<()>
    where
        T: transport::Transport + Send + 'static,
    {
        loop {
            accept_rx.recv()?;

            match incoming.next() {
                // Incoming stream is finished, there is nothing left to do for this
                // subroutine.
                None => return Ok(()),
                Some(Err(_err)) => todo!(),
                Some(Ok(conn)) => match node::Id::try_from(conn.public_key()) {
                    Err(_err) => todo!(),
                    Ok(id) => {
                        let mut connected =
                            connected.lock().map_err(|_| Error::StateLockPoisoned)?;

                        let msg = match connected.entry(id) {
                            Entry::Vacant(entry) => {
                                entry.insert(transport::Direction::Incoming(conn));
                                Input::Accepted(id)
                            }
                            // If the id in question is already connected we terminate
                            // the duplicate one and inform the protocol of it.
                            Entry::Occupied(_entry) => {
                                Input::DuplicateConnRejected(id, conn.close().err())
                            }
                        };

                        input_tx.try_send(msg)?;
                    }
                },
            }
        }
    }

    fn connect<T>(
        connected: Connected<T>,
        connect_rx: Receiver<transport::ConnectInfo>,
        endpoint: <T as transport::Transport>::Endpoint,
        input_tx: Sender<Input>,
    ) -> Result<()>
    where
        T: transport::Transport + Send + 'static,
    {
        loop {
            let info = connect_rx.recv()?;

            match endpoint.connect(info) {
                Err(_err) => todo!(),
                Ok(conn) => {
                    match node::Id::try_from(conn.public_key()) {
                        Err(_err) => todo!(),
                        Ok(id) => {
                            let mut connected =
                                connected.lock().map_err(|_| Error::StateLockPoisoned)?;

                            let msg = match connected.entry(id) {
                                Entry::Vacant(entry) => {
                                    entry.insert(transport::Direction::Outgoing(conn));
                                    Input::Connected(id)
                                }
                                Entry::Occupied(_entry) => {
                                    // TODO(xla): Define and account for the case where a connection is present for
                                    // the id.
                                    todo!()
                                }
                            };

                            input_tx.try_send(msg)?;
                        }
                    }
                }
            };
        }
    }

    fn message<T>(
        input_tx: Sender<Input>,
        msg_rx: Receiver<(node::Id, message::Send)>,
        peers: Peers<T>,
    ) -> Result<()>
    where
        T: transport::Transport + Send + 'static,
    {
        loop {
            let (id, msg) = msg_rx.recv()?;

            let peers = peers.lock().map_err(|_| Error::StateLockPoisoned)?;

            match peers.get(&id) {
                // TODO(xla): Ideally acked that the message passed to the peer.
                // FIXME(xla): As the state lock is held up top, it's dangerous if send is
                // ever blocking for any amount of time, which makes this call sensitive to the
                // implementation details of send.
                Some(peer) => peer.send(msg).unwrap(),
                // TODO(xla): A missing peer needs to be bubbled up as that indicates there is
                // a mismatch between the tracked peers in the protocol and the ones the supervisor holds
                // onto. Something is afoot and it needs to be reconciled asap.
                None => todo!(),
            }
        }
    }

    fn stop<T>(input_tx: Sender<Input>, peers: Peers<T>, stop_rx: Receiver<node::Id>) -> Result<()>
    where
        T: transport::Transport + Send + 'static,
    {
        loop {
            let id = stop_rx.recv()?;

            // To avoid that the lock is held for too long this block is significant.
            let peer = {
                let mut peers = peers.lock().map_err(|_| Error::StateLockPoisoned)?;
                peers.remove(&id)
            };

            let msg = match peer {
                Some(peer) => Input::Stopped(id, peer.stop().err()),
                None => {
                    // TOOD(xla): A missing peer needs to be bubbled up as that indicates there is
                    // a mismatch between the protocol tracked peers and the ones the supervisor holds
                    // onto. Something is afoot and it needs to be reconciled asap.
                    todo!()
                }
            };

            input_tx.try_send(msg)?
        }
    }

    fn upgrade<T>(
        connected: Connected<T>,
        input_tx: Sender<Input>,
        peers: Peers<T>,
        upgrade_rx: Receiver<node::Id>,
    ) -> Result<()>
    where
        T: transport::Transport + Send + 'static,
    {
        loop {
            let peer_id = upgrade_rx.recv()?;
            let mut connected = connected.lock().map_err(|_| Error::StateLockPoisoned)?;

            let msg = match connected.remove(&peer_id) {
                None => Input::UpgradeFailed(peer_id, Report::msg("connection not found")),
                Some(conn) => {
                    match peer::Peer::try_from(conn) {
                        Err(_err) => todo!(),
                        // TODO(xla): Provide actual (possibly configured) list of streams.
                        Ok(peer) => match peer.run(vec![]) {
                            Ok(peer) => {
                                let mut peers =
                                    peers.lock().map_err(|_| Error::StateLockPoisoned)?;
                                match peers.entry(peer.id) {
                                    Entry::Vacant(entry) => {
                                        entry.insert(peer);
                                        Input::Upgraded(peer_id)
                                    }
                                    Entry::Occupied(_entry) => todo!(),
                                }
                            }
                            Err(err) => Input::UpgradeFailed(peer_id, err),
                        },
                    }
                }
            };

            input_tx.try_send(msg)?;
        }
    }
}

struct Protocol {
    connected: HashMap<node::Id, Direction>,
    stopped: HashSet<node::Id>,
    upgraded: HashSet<node::Id>,
}

impl Protocol {
    fn transition(&mut self, input: Input) -> Vec<Output> {
        match input {
            Input::Accepted(id) => self.handle_accepted(id),
            Input::Command(command) => self.handle_command(command),
            Input::Connected(id) => self.handle_connected(id),
            Input::DuplicateConnRejected(_id, _report) => todo!(),
            Input::Receive(id, msg) => self.handle_receive(id, msg),
            Input::Stopped(id, report) => self.handle_stopped(id, report),
            Input::Upgraded(id) => self.handle_upgraded(id),
            Input::UpgradeFailed(id, err) => self.handle_upgrade_failed(id, err),
        }
    }

    fn handle_accepted(&mut self, id: node::Id) -> Vec<Output> {
        // TODO(xla): Ensure we only allow one connection per node. Unless a higher-level protocol
        // like PEX is taking care of it.
        self.connected.insert(id, Direction::Incoming);

        vec![
            Output::from(Event::Connected(id, Direction::Incoming)),
            Output::from(Internal::Upgrade(id)),
        ]
    }

    fn handle_command(&mut self, command: Command) -> Vec<Output> {
        match command {
            Command::Accept => vec![Output::from(Internal::Accept)],
            Command::Connect(info) => vec![Output::from(Internal::Connect(info))],
            Command::Disconnect(id) => {
                vec![Output::Internal(Internal::Stop(id))]
            }
            Command::Msg(peer_id, msg) => match self.upgraded.get(&peer_id) {
                Some(peer_id) => vec![Output::from(Internal::SendMessage(*peer_id, msg))],
                None => vec![],
            },
        }
    }

    fn handle_connected(&mut self, id: node::Id) -> Vec<Output> {
        // TODO(xla): Ensure we only allow one connection per node. Unless a higher-level protocol
        // like PEX is taking care of it.
        self.connected.insert(id, Direction::Outgoing);

        vec![
            Output::from(Event::Connected(id, Direction::Outgoing)),
            Output::from(Internal::Upgrade(id)),
        ]
    }

    fn handle_receive(&self, id: node::Id, msg: message::Receive) -> Vec<Output> {
        vec![Output::from(Event::Message(id, msg))]
    }

    fn handle_stopped(&mut self, id: node::Id, report: Option<Report>) -> Vec<Output> {
        self.upgraded.remove(&id);
        self.stopped.insert(id);

        vec![Output::from(Event::Disconnected(
            id,
            report.unwrap_or(Report::msg("successfully disconected")),
        ))]
    }

    fn handle_upgraded(&mut self, id: node::Id) -> Vec<Output> {
        self.upgraded.insert(id);

        vec![Output::from(Event::Upgraded(id))]
    }

    fn handle_upgrade_failed(&mut self, id: node::Id, err: Report) -> Vec<Output> {
        self.connected.remove(&id);

        vec![Output::from(Event::UpgradeFailed(id, err))]
    }
}