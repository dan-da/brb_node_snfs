use serde::{Deserialize, Serialize};

use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use tokio::sync::Mutex as TokioMutex;

use cmdr::*;

use log::{debug, error, info, trace, warn};
//use std::io::Write;
use std::env;
use std::ffi::OsStr;
use std::path::Path;

use qp2p::{
    self, Config, DisconnectionEvents, Endpoint, IncomingConnections, IncomingMessages, QuicP2p,
};
use std::{
    collections::{BTreeMap, BTreeSet, HashMap, VecDeque},
    net::{IpAddr, Ipv4Addr, SocketAddr},
};

use brb::membership::actor::ed25519::{Actor, Sig, SigningActor};
use brb::{BRBDataType, DeterministicBRB, Packet as BRBPacket};
use brb_dt_tree::{BRBTree, OpMoveTx};
use crdt_tree::Clock;
use openat::{Dir, SimpleType};
use std::ffi::OsString;

use sn_fs::{FsClock, FsOpMove, FsTreeNode, FsTreeStore, SnFs, TreeIdType, TreeMetaType};

//type TypeId = u64;
//type TypeMeta<'a> = &'static str;
//type Meta = u64;
//type Actor = u64;

type State = BRBTree<Actor, TreeIdType, TreeMetaType>;
type BRB = DeterministicBRB<Actor, SigningActor, Sig, State>;
type Packet = BRBPacket<Actor, Sig, <State as BRBDataType<Actor>>::Op>;

#[derive(Debug, Clone)]
struct SharedBRB {
    brb: Arc<Mutex<BRB>>,
    clock: Clock<Actor>,
}

#[derive(Debug, Clone)]
struct FsBrbTreeStore {
    state: SharedBRB,
    network_tx: Arc<Mutex<mpsc::Sender<RouterCmd>>>,
    pending_ops: Arc<Mutex<VecDeque<OpMoveTx<TreeIdType, TreeMetaType, Actor>>>>,
}

impl FsBrbTreeStore {
    fn new(state: SharedBRB, network_tx: mpsc::Sender<RouterCmd>) -> Self {
        let s = Self {
            state,
            network_tx: Arc::new(Mutex::new(network_tx)),
            pending_ops: Arc::new(Mutex::new(Default::default())),
        };

        // spawn a thread for sending ops to network in sequence.
        let c = s.clone();
        std::thread::spawn(move || Self::exec_ops_when_ready(c));

        s
    }

    fn exec_ops_when_ready(self) {
        debug!("[CLI] entering exec_ops_when_ready");

        loop {
            // Wait until there are no BRB ops from our actor in flight. (pending supermajority).
            loop {
                let packets_to_resend = self.state.resend_pending_msgs();
                if packets_to_resend.is_empty() {
                    break;
                }

                // fixme: We should periodically ( 30 secs?  60 secs? ) resend these packets in best
                //        effort to reach supermajority.

                // ... re-send pending packets
                // for p in packets_to_resend {
                //     self.network_tx.send(RouterCmd::Deliver(p)).expect("Failed to queue packet");
                // }

                std::thread::sleep(std::time::Duration::from_millis(30));
            }

            let op_option = {
                // fixme; unwrap()
                let mut pending_ops = self.pending_ops.lock().unwrap();
                pending_ops.pop_front()
            }; // Mutex guard drops out of scope here.

            if let Some(op) = op_option {
                // exec the op.  (generate packets for sending op to peers)
                let packets = self.state.exec_op(op);
                debug!("exec_op() returned packets: {:?}", packets);

                // Send/deliver packets to peers.
                let guard = self.network_tx.lock().unwrap(); // fixme: unwrap
                for packet in packets {
                    guard
                        .send(RouterCmd::Deliver(packet))
                        .expect("Failed to queue packet");
                }
            }

            std::thread::sleep(std::time::Duration::from_millis(30));
        }
    }
}

impl FsTreeStore for FsBrbTreeStore {
    fn opmove(&self, parent_id: TreeIdType, meta: TreeMetaType, child_id: TreeIdType) -> FsOpMove {
        self.state.opmove(parent_id, meta, child_id)
    }

    fn opmoves(&self, ops: Vec<(TreeIdType, TreeMetaType, TreeIdType)>) -> Vec<FsOpMove> {
        self.state.opmovetx_multi(ops)
    }

    fn apply_op(&mut self, op: FsOpMove) {
        self.apply_ops(vec![op])
    }

    fn apply_ops(&mut self, ops: Vec<FsOpMove>) {
        self.state.apply_op(ops.clone());

        debug!("[CLI] Adding op to pending_ops queue {:?}", ops);

        let mut pending_ops = self.pending_ops.lock().unwrap();
        pending_ops.push_back(ops);

        // note: the op is added to a queue, which the
        // exec_ops_when_ready() thread reads from
    }

    fn find(&self, child_id: &TreeIdType) -> Option<FsTreeNode> {
        self.state.find(child_id)
    }

    fn children(&self, parent_id: &TreeIdType) -> Vec<TreeIdType> {
        self.state.children(parent_id)
    }

    fn time(&self) -> FsClock {
        self.state.time()
    }

    fn truncate_log(&mut self) -> bool {
        self.state.truncate_log()
    }
}

impl SharedBRB {
    fn new() -> Self {
        let brb = BRB::new();
        let clock = Clock::new(brb.actor(), None);
        Self {
            brb: Arc::new(Mutex::new(brb)),
            clock,
        }
    }

    fn actor(&self) -> Actor {
        self.brb.lock().unwrap().actor() // fixme: unwrap
    }

    fn opmove(&self, parent_id: TreeIdType, meta: TreeMetaType, child_id: TreeIdType) -> FsOpMove {
        self.brb
            .lock()
            .unwrap() // fixme: unwrap
            .dt
            .opmove(parent_id, meta, child_id)
    }

    fn opmovetx_multi(&self, ops: Vec<(TreeIdType, TreeMetaType, TreeIdType)>) -> Vec<FsOpMove> {
        self.brb.lock().unwrap().dt.opmovetx_multi(ops) // fixme: unwrap
    }

    fn peers(&self) -> BTreeSet<Actor> {
        // fixme: unwrap
        self.brb.lock().unwrap().peers().unwrap_or_else(|err| {
            error!("[CLI] Failure while reading brb peers: {:?}", err);
            Default::default()
        })
    }

    fn trust_peer(&mut self, peer: Actor) {
        // fixme: unwrap
        self.brb.lock().unwrap().force_join(peer);
    }

    fn untrust_peer(&mut self, peer: Actor) {
        // fixme: unwrap
        self.brb.lock().unwrap().force_leave(peer);
    }

    fn request_join(&mut self, actor: Actor) -> Vec<Packet> {
        self.brb
            .lock()
            .unwrap() // fixme: unwrap
            .request_membership(actor)
            .unwrap_or_else(|err| {
                error!("[CLI] Failed to request join for {:?} : {:?}", actor, err);
                Default::default()
            })
    }

    fn request_leave(&mut self, actor: Actor) -> Vec<Packet> {
        self.brb
            .lock()
            .unwrap() // fixme: unwrap
            .kill_peer(actor)
            .unwrap_or_else(|err| {
                error!("[CLI] Failed to request leave for {:?}: {:?}", actor, err);
                Default::default()
            })
    }

    fn anti_entropy(&mut self, peer: Actor) -> Option<Packet> {
        // fixme: unwrap
        match self.brb.lock().unwrap().anti_entropy(peer) {
            Ok(packet) => Some(packet),
            Err(err) => {
                error!("[CLI] Failed initiating anti-entropy {:?}", err);
                None
            }
        }
    }

    fn exec_op(&self, op: <State as BRBDataType<Actor>>::Op) -> Vec<Packet> {
        // fixme: unwrap
        self.brb.lock().unwrap().exec_op(op).unwrap_or_else(|err| {
            error!("[CLI] Error executing datatype op: {:?}", err);
            Default::default()
        })
    }

    fn apply_op(&mut self, op: <State as BRBDataType<Actor>>::Op) {
        // fixme: unwrap
        self.brb.lock().unwrap().dt.apply(op)
    }

    fn apply_packet(&mut self, packet: Packet) -> Vec<Packet> {
        // fixme: unwrap
        match self.brb.lock().unwrap().handle_packet(packet) {
            Ok(packets) => packets,
            Err(e) => {
                error!("[CLI] dropping packet: {:?}", e);
                Default::default()
            }
        }
    }

    pub fn resend_pending_msgs(&self) -> Vec<Packet> {
        // fixme: unwrap
        match self.brb.lock().unwrap().resend_pending_msgs() {
            Ok(packets) => packets,
            Err(e) => {
                error!("[CLI] resend_pending_deliveries failed: {:?}", e);
                Default::default()
            }
        }
    }

    fn find(&self, child_id: &TreeIdType) -> Option<FsTreeNode> {
        self.brb
            .lock()
            .unwrap() // fixme: unwrap
            .dt
            .treestate()
            .tree()
            .find(child_id)
            .cloned()
    }

    fn children(&self, parent_id: &TreeIdType) -> Vec<TreeIdType> {
        self.brb
            .lock()
            .unwrap() // fixme: unwrap
            .dt
            .treestate()
            .tree()
            .children(parent_id)
    }

    fn read(&self) -> String {
        // fixme: unwrap
        format!("{}", self.brb.lock().unwrap().dt.treestate().tree())
    }

    fn time(&self) -> FsClock {
        // fixme: unwrap
        self.brb.lock().unwrap().dt.treereplica().time().clone()
    }

    // fn op_applied(&self, op: &FsOpMove) -> bool {
    //     let guard = self.brb.lock().unwrap();
    //     let log = guard.dt.treestate().log();
    //     for o in log {
    //         if o.clone().op_into() == *op {
    //             return true;
    //         }
    //     }
    //     false
    // }

    fn truncate_log(&self) -> bool {
        false
        //self.brb.lock().unwrap().dt.state().truncate_log()
    }
}

#[derive(Debug)]
struct Repl {
    state: SharedBRB,
    network_tx: mpsc::Sender<RouterCmd>,
}

#[cmdr]
impl Repl {
    fn new(state: SharedBRB, network_tx: mpsc::Sender<RouterCmd>) -> Self {
        Self { state, network_tx }
    }

    // cmdr::Scope hook that is called after command execution is finished.  overriding.
    fn after_command(&mut self, _line: &Line, result: CommandResult) -> CommandResult {
        // Delay writing prompt by 1 second to reduce chance that P2P log output in
        // other thread overwrites it.
        std::thread::sleep(std::time::Duration::from_secs(1));
        result
    }

    #[cmd]
    fn peer(&mut self, args: &[String]) -> CommandResult {
        match args {
            [ip_port] => match ip_port.parse::<SocketAddr>() {
                Ok(addr) => {
                    info!("[REPL] parsed addr {:?}", addr);
                    self.network_tx
                        .send(RouterCmd::SayHello(addr))
                        .unwrap_or_else(|e| {
                            error!("[REPL] Failed to queue router command {:?}", e)
                        });
                }
                Err(e) => error!("[REPL] bad addr {:?}", e),
            },
            _ => println!("help: peer <ip>:<port>"),
        };
        Ok(Action::Done)
    }

    #[cmd]
    fn peers(&mut self, args: &[String]) -> CommandResult {
        match args {
            [] => self
                .network_tx
                .send(RouterCmd::ListPeers)
                .unwrap_or_else(|e| error!("[REPL] Failed to queue router command {:?}", e)),
            _ => println!("help: peers expects no arguments"),
        };
        Ok(Action::Done)
    }

    #[cmd]
    fn trust(&mut self, args: &[String]) -> CommandResult {
        match args {
            [actor_id] => {
                self.network_tx
                    .send(RouterCmd::Trust(actor_id.to_string()))
                    .unwrap_or_else(|e| error!("[REPL] Failed to queue router command {:?}", e));
            }
            _ => println!("help: trust id:8sdkgalsd | trust me"),
        };
        Ok(Action::Done)
    }

    #[cmd]
    fn untrust(&mut self, args: &[String]) -> CommandResult {
        match args {
            [actor_id] => {
                self.network_tx
                    .send(RouterCmd::Untrust(actor_id.to_string()))
                    .unwrap_or_else(|e| error!("[REPL] Failed to queue router command {:?}", e));
            }
            _ => println!("help: untrust id:8f4e"),
        };
        Ok(Action::Done)
    }

    #[cmd]
    fn join(&mut self, args: &[String]) -> CommandResult {
        match args {
            [actor_id] => {
                self.network_tx
                    .send(RouterCmd::RequestJoin(actor_id.to_string()))
                    .unwrap_or_else(|e| error!("[REPL] Failed to queue router command {:?}", e));
            }
            _ => println!("help: join takes one arguments, the actor to add to the network"),
        };
        Ok(Action::Done)
    }

    #[cmd]
    fn leave(&mut self, args: &[String]) -> CommandResult {
        match args {
            [actor_id] => {
                self.network_tx
                    .send(RouterCmd::RequestLeave(actor_id.to_string()))
                    .unwrap_or_else(|e| error!("[REPL] Failed to queue router command {:?}", e));
            }
            _ => println!("help: leave takes one arguments, the actor to leave the network"),
        };
        Ok(Action::Done)
    }

    #[cmd]
    fn anti_entropy(&mut self, args: &[String]) -> CommandResult {
        match args {
            [actor_id] => {
                self.network_tx
                    .send(RouterCmd::AntiEntropy(actor_id.to_string()))
                    .unwrap_or_else(|e| error!("[REPL] Failed to queue router command {:?}", e));
            }
            _ => println!("help: anti_entropy takes one arguments, the actor to request data from"),
        };

        Ok(Action::Done)
    }

    #[cmd]
    fn retry(&mut self, _args: &[String]) -> CommandResult {
        self.network_tx
            .send(RouterCmd::Retry)
            .expect("Failed to queue router cmd");
        Ok(Action::Done)
    }

    #[cmd]
    fn read(&mut self, _args: &[String]) -> CommandResult {
        println!("{}", self.state.read());
        Ok(Action::Done)
    }

    #[cmd]
    fn dbg(&mut self, _args: &[String]) -> CommandResult {
        self.network_tx
            .send(RouterCmd::Debug)
            .expect("Failed to queue router cmd");
        Ok(Action::Done)
    }
}

#[derive(Debug)]
struct Router {
    state: SharedBRB,
    addr: SocketAddr,
    endpoint: Endpoint,
    peers: HashMap<Actor, SocketAddr>,
    unacked_packets: VecDeque<Packet>,
}

#[derive(Debug)]
enum RouterCmd {
    Retry,
    Debug,
    AntiEntropy(String),
    ListPeers,
    RequestJoin(String),
    RequestLeave(String),
    Trust(String),
    Untrust(String),
    SayHello(SocketAddr),
    AddPeer(Actor, SocketAddr),
    Deliver(Packet),
    Apply(Packet),
    Acked(Packet),
}

#[allow(clippy::large_enum_variant)]
#[derive(Debug, Serialize, Deserialize)]
enum NetworkMsg {
    Peer(Actor, SocketAddr),
    Packet(Packet),
    Ack(Packet),
}

#[allow(dead_code)]
struct EndpointInfo {
    shared_endpoint: Endpoint,
    incoming_connections: IncomingConnections,
    incoming_messages: IncomingMessages,
    disconnection_events: DisconnectionEvents,
}

impl Router {
    async fn new(state: SharedBRB) -> (Self, EndpointInfo) {
        let qp2p = QuicP2p::with_config(
            Some(Config {
                local_port: None,
                local_ip: Some(IpAddr::V4(Ipv4Addr::LOCALHOST)),
                idle_timeout_msec: Some(1000 * 86400 * 365), // 1 year idle timeout.
                ..Default::default()
            }),
            Default::default(),
            true,
        )
        .expect("Error creating QuicP2p object");

        // fixme: unwrap
        let epmeta = qp2p.new_endpoint().await.unwrap();
        let endpoint_info = EndpointInfo {
            shared_endpoint: epmeta.0,
            incoming_connections: epmeta.1,
            incoming_messages: epmeta.2,
            disconnection_events: epmeta.3,
        };

        let addr = endpoint_info.shared_endpoint.socket_addr();

        let router = Self {
            state,
            addr,
            endpoint: endpoint_info.shared_endpoint.clone(),
            peers: Default::default(),
            unacked_packets: Default::default(),
        };

        (router, endpoint_info)
    }

    fn resolve_actor(&self, actor_id: &str) -> Option<Actor> {
        if actor_id == "me" {
            return Some(self.state.actor());
        }

        let matching_actors: Vec<Actor> = self
            .peers
            .iter()
            .map(|(actor, _)| actor)
            .cloned()
            .filter(|actor| format!("{:?}", actor).starts_with(&actor_id))
            .collect();

        if matching_actors.len() > 1 {
            println!("Ambiguous actor id, more than one actor matches:");
            for actor in matching_actors {
                println!("{:?}", actor);
            }
            None
        } else if matching_actors.is_empty() {
            println!("No actors with that actor id");
            None
        } else {
            Some(matching_actors[0])
        }
    }

    async fn listen_for_cmds(mut self, net_rx: Arc<TokioMutex<mpsc::Receiver<RouterCmd>>>) {
        loop {
            // fixme: unwrap
            let net_cmd = net_rx.lock().await.recv().unwrap();
            self.apply(net_cmd).await;
        }
    }

    async fn deliver_network_msg(&mut self, network_msg: &NetworkMsg, dest_addr: &SocketAddr) {
        // fixme: unwrap
        let msg = bincode::serialize(&network_msg).unwrap();

        if let Err(e) = self.endpoint.connect_to(&dest_addr).await {
            error!("[P2P] Failed to connect. {:?}", e);
            return;
        }

        let logmsg = format!(
            "[P2P] Sending message to {:?} --> {:?}",
            dest_addr, network_msg
        );
        match network_msg {
            NetworkMsg::Ack(_) => trace!("{}", logmsg),
            _ => debug!("{}", logmsg),
        }

        match self.endpoint.send_message(msg.into(), &dest_addr).await {
            Ok(()) => trace!("[P2P] Sent network msg successfully."),
            Err(e) => error!("[P2P] Failed to send network msg: {:?}", e),
        }
    }

    async fn deliver_packet(&mut self, packet: Packet) {
        match self.peers.clone().get(&packet.dest) {
            Some(peer_addr) => {
                info!(
                    "[P2P] delivering packet to {:?} at addr {:?}: {:?}",
                    packet.dest, peer_addr, packet
                );
                self.unacked_packets.push_back(packet.clone());
                self.deliver_network_msg(&NetworkMsg::Packet(packet), &peer_addr)
                    .await;
            }
            None => warn!(
                "[P2P] we don't have a peer matching the destination for packet {:?}",
                packet
            ),
        }
    }

    async fn apply(&mut self, cmd: RouterCmd) {
        let logmsg = format!("[P2P] router cmd {:?}", cmd);
        match cmd {
            RouterCmd::Acked(_) => trace!("{}", logmsg),
            _ => debug!("{}", logmsg),
        }

        match cmd {
            RouterCmd::Retry => {
                let packets_to_retry = self.unacked_packets.clone();
                self.unacked_packets = Default::default();
                for packet in packets_to_retry {
                    info!("Retrying packet: {:#?}", packet);
                    self.deliver_packet(packet).await
                }
            }
            RouterCmd::Debug => {
                debug!("{:#?}", self);
            }
            RouterCmd::AntiEntropy(actor_id) => {
                if let Some(actor) = self.resolve_actor(&actor_id) {
                    info!("[P2P] Starting anti-entropy with actor: {:?}", actor);
                    if let Some(packet) = self.state.anti_entropy(actor) {
                        self.deliver_packet(packet).await;
                    }
                }
            }
            RouterCmd::ListPeers => {
                let voting_peers = self.state.peers();

                let peer_addrs: BTreeMap<_, _> =
                    self.peers.iter().map(|(p, addr)| (*p, *addr)).collect();

                let identities: BTreeSet<_> = voting_peers
                    .iter()
                    .cloned()
                    .chain(peer_addrs.keys().cloned())
                    .collect();

                for id in identities {
                    let mut line = format!("{:?}", id);
                    line.push('@');
                    match peer_addrs.get(&id) {
                        Some(addr) => line.push_str(&format!("{:?}", addr)),
                        None => line.push_str("<unknown>"),
                    };
                    if voting_peers.contains(&id) {
                        line.push_str("\t(voting)");
                    }
                    if id == self.state.actor() {
                        line.push_str("\t(self)");
                    }
                    println!("{}", line);
                }
            }
            RouterCmd::RequestJoin(actor_id) => {
                if let Some(actor) = self.resolve_actor(&actor_id) {
                    info!("[P2P] Starting join for actor: {:?}", actor);
                    for packet in self.state.request_join(actor) {
                        self.deliver_packet(packet).await;
                    }
                }
            }
            RouterCmd::RequestLeave(actor_id) => {
                if let Some(actor) = self.resolve_actor(&actor_id) {
                    info!("[P2P] Starting leave for actor: {:?}", actor);
                    for packet in self.state.request_leave(actor) {
                        self.deliver_packet(packet).await;
                    }
                }
            }
            RouterCmd::Trust(actor_id) => {
                if let Some(actor) = self.resolve_actor(&actor_id) {
                    info!("[P2P] Trusting actor: {:?}", actor);
                    self.state.trust_peer(actor);
                }
            }
            RouterCmd::Untrust(actor_id) => {
                if let Some(actor) = self.resolve_actor(&actor_id) {
                    info!("[P2P] Trusting actor: {:?}", actor);
                    self.state.untrust_peer(actor);
                }
            }
            RouterCmd::SayHello(addr) => {
                self.deliver_network_msg(&NetworkMsg::Peer(self.state.actor(), self.addr), &addr)
                    .await
            }
            RouterCmd::AddPeer(actor, addr) =>
            {
                #[allow(clippy::map_entry)]
                if !self.peers.contains_key(&actor) {
                    for (peer_actor, peer_addr) in self.peers.clone().iter() {
                        self.deliver_network_msg(&NetworkMsg::Peer(*peer_actor, *peer_addr), &addr)
                            .await;
                    }
                    self.peers.insert(actor, addr);
                }
            }
            RouterCmd::Deliver(packet) => {
                self.deliver_packet(packet).await;
            }
            RouterCmd::Apply(op_packet) => {
                for packet in self.state.apply_packet(op_packet.clone()) {
                    self.deliver_packet(packet).await;
                }

                if let Some(peer_addr) = self.peers.clone().get(&op_packet.source) {
                    trace!(
                        "[P2P] delivering Ack(packet) to {:?} at addr {:?}: {:?}",
                        op_packet.dest,
                        peer_addr,
                        op_packet
                    );
                    self.deliver_network_msg(&NetworkMsg::Ack(op_packet), &peer_addr)
                        .await;
                } else {
                    warn!(
                        "[P2P] we don't have a peer matching the destination for packet {:?}",
                        op_packet
                    );
                }
            }
            RouterCmd::Acked(packet) => {
                self.unacked_packets
                    .iter()
                    .position(|p| p == &packet)
                    .map(|idx| {
                        trace!("[P2P] Got ack for packet {:?}", packet);
                        self.unacked_packets.remove(idx)
                    });
            }
        }
    }
}

async fn listen_for_network_msgs(
    mut endpoint_info: EndpointInfo,
    router_tx: mpsc::Sender<RouterCmd>,
) {
    let listen_addr = endpoint_info.shared_endpoint.socket_addr();
    info!("[P2P] listening on {:?}", listen_addr);

    router_tx
        .send(RouterCmd::SayHello(listen_addr))
        .expect("Failed to send command to add self as peer");

    while let Some((socket_addr, bytes)) = endpoint_info.incoming_messages.next().await {
        // fixme: unwrap
        let net_msg: NetworkMsg = bincode::deserialize(&bytes).unwrap();

        let msg = format!("[P2P] received from {:?} --> {:?}", socket_addr, net_msg);
        match net_msg {
            NetworkMsg::Ack(_) => trace!("{}", msg),
            _ => debug!("{}", msg),
        }

        let cmd = match net_msg {
            NetworkMsg::Peer(actor, addr) => RouterCmd::AddPeer(actor, addr),
            NetworkMsg::Packet(packet) => RouterCmd::Apply(packet),
            NetworkMsg::Ack(packet) => RouterCmd::Acked(packet),
        };

        router_tx.send(cmd).expect("Failed to send router command");
    }

    info!("[P2P] Finished listening for incoming messages");
}

async fn listen_for_fuse_events(store: FsBrbTreeStore, mountpoint: OsString) {
    // We use Dir::open() to get access to the mountpoint directory
    // before the mount occurs.  This handle enables us to later create/write/read
    // "real" files beneath the mountpoint even though other processes will only
    // see the filesystem view that our SnFs provides.
    let mountpoint_fd = match Dir::open(Path::new(&mountpoint)) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("Unable to open {:?}.  {:?}", mountpoint, e);
            return;
        }
    };

    // Notes:
    //  1. todo: these options should come from command line.
    //  2. allow_other enables other users to read/write.  Required for testing chown.
    //  3. allow_other requires that `user_allow_other` is in /etc/fuse.conf.
    //    let options = ["-o", "ro", "-o", "fsname=safefs"]    // -o ro = mount read only
    let options = ["-o", "fsname=sn_fs"]
        .iter()
        .map(|o| o.as_ref())
        .collect::<Vec<&OsStr>>();

    // mount the filesystem.
    let filecontent_on_disk = false;
    let sn_fs = SnFs::<FsBrbTreeStore>::new(store, mountpoint_fd, filecontent_on_disk);
    if let Err(e) = fuse::mount(sn_fs, &mountpoint, &options) {
        eprintln!("Mount failed.  {:?}", e);
        return;
    }

    // Delete all "real" files (each file representing content of 1 inode) under mount point.
    // this code should be in SnFs::destroy(), but its not getting called.
    // Seems like a fuse bug/issue.
    let mountpoint_fd = Dir::open(Path::new(&mountpoint)).unwrap(); // fixme: unwrap
    if let Ok(entries) = mountpoint_fd.list_dir(".") {
        for result in entries {
            if let Ok(entry) = result {
                if entry.simple_type() == Some(SimpleType::File)
                    && mountpoint_fd
                        .remove_file(Path::new(entry.file_name()))
                        .is_err()
                {
                    error!("Unable to remove file {:?}", entry.file_name());
                }
            }
        }
    }

    warn!("Filesystem unmounted.  FUSE thread exiting!");
}

#[tokio::main]
async fn main() {
    // Customize logger to:
    //  1. display messages from brb crates only.  (filter)
    //  2. omit timestamp, etc.  display each log message string + newline.
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or(
//        "brb=debug,brb_membership=debug,brb_dt_orswot=debug,brb_node=debug,sn_fs=debug,qp2p=warn,quinn=warn",
        "brb=info,brb_membership=info,brb_dt_orswot=info,brb_node=debug,sn_fs=debug,qp2p=warn,quinn=warn",
    ))
//    .format(|buf, record| writeln!(buf, "{}\n", record.args()))
    .init();

    let mountpoint = match env::args_os().nth(1) {
        Some(v) => v,
        None => {
            print_usage();
            return;
        }
    };

    let state = SharedBRB::new();
    let (router, endpoint_info) = Router::new(state.clone()).await;
    let (router_tx, router_rx) = mpsc::channel();

    let router_rx_arc = Arc::new(TokioMutex::new(router_rx));

    tokio::spawn(listen_for_network_msgs(endpoint_info, router_tx.clone()));
    tokio::spawn(router.listen_for_cmds(router_rx_arc));
    tokio::spawn(listen_for_fuse_events(
        FsBrbTreeStore::new(state.clone(), router_tx.clone()),
        mountpoint,
    ));

    // Delay by 1 second to prevent P2P startup from overwriting user prompt.
    std::thread::sleep(std::time::Duration::from_secs(1));
    cmd_loop(&mut Repl::new(state, router_tx)).expect("Failure in REPL");
}

fn print_usage() {
    eprintln!("Usage: brb_node_snfs <mountpoint_path>");
}
