//! The blockchain node
use super::*;
use serde_json::Deserializer;
use std::collections::HashSet;
use std::io::{stdout, Write};
use std::net::{TcpListener, TcpStream};
use std::thread;
use tokio::sync::mpsc::{channel, Receiver, Sender};

pub enum Event {
    Request(TcpStream, Request),
    _Response(Response),
    Broadcast(Request),
    Command(Command),
}

// TODO: add consensus protocol specification
pub struct Node {
    basic_info: PeerInfo,
    chain: Blockchain,
    peers: HashSet<PeerInfo>,
    broadcast_sender: Sender<Event>,
}

const EVENT_BUFFER_SIZE: usize = 10;

impl Node {
    pub async fn handle_events(addr: String) -> Result<()> {
        let listener = TcpListener::bind(&addr)?;

        let (sender, mut receiver) = channel(EVENT_BUFFER_SIZE);
        let sender1 = sender.clone();
        let sender2 = sender.clone();
        thread::spawn(move || message::handle_incoming_connections(listener, sender1));
        thread::spawn(move || command::handle_input_commands(sender2));

        let mut node = Node {
            basic_info: PeerInfo::new(addr)?,
            chain: Blockchain::new(),
            peers: HashSet::new(),
            broadcast_sender: sender,
        };

        loop {
            let event = receiver.recv().await.unwrap();
            // TODO: result not used
            let _result = match event {
                Event::Request(stream, request) => node.serve_request(stream, request),
                Event::_Response(_response) => unimplemented!(),
                Event::Broadcast(request) => node.broadcast_request(&request),
                Event::Command(command) => node.serve_command(command),
            };
        }
    }

    fn serve_request(&mut self, mut stream: TcpStream, request: Request) -> Result<()> {
        let peer_info = request.get_sender_peer_info();
        if self.add_peer(peer_info) {
            info!("Add one new peer: {:?}", peer_info);
        }
        let my_info = self.get_basic_info();
        let mut response = None;
        match request {
            Request::Hello(peer_info) => {
                info!("Get Hello from {:?}, simply ack it", peer_info);
                response = Some(Response::Ack(my_info));
            }
            Request::HowAreYou(peer_info) => {
                info!(
                    "Get HowAreYou from {:?}, will respond with all my blocks",
                    peer_info
                );
                response = Some(Response::MyBlocks(self.get_basic_info(), self.get_blocks()));
            }
            Request::NewTransaction(peer_info, transaction) => {
                info!(
                    "Get NewTransaction from {:?}, add the transaction and ack it",
                    peer_info
                );
                self.handle_incoming_transaction(transaction);
            }
            Request::NewBlock(peer_info, new_block) => {
                info!(
                    "Get NewBlock from {:?}, validate it and possibly add it to our chain",
                    peer_info
                );
                self.handle_incoming_block(new_block);
            }
            Request::NewPeer(peer_info, new_peer) => {
                info!(
                    "Get NewPeer from {:?}, new peer is {:?}",
                    peer_info, new_peer
                );
                self.handle_incoming_peer(new_peer);
            }
        };
        if let Some(response) = response {
            serde_json::to_writer(&mut stream, &response)?;
            stream.flush()?;
            debug!("response sent {:?}", response);
        };
        Ok(())
    }

    fn serve_command(&mut self, command: Command) -> Result<()> {
        match command {
            Command::NewTrans(sender, receiver, amount) => {
                self.create_and_add_new_transaction(&sender, &receiver, amount);
            }
            Command::Display => self.display(),
            Command::AddPeer(peer) => {
                // BLOCKING
                if !self.greet_and_add_peer(&peer) {
                    eprintln!("{}", "fail to add peer".color(ERR_COLOR));
                }
            }
            Command::DisplayPeers => self.display_peers(),
            Command::Resolve => {
                // BLOCKING
                if self.resolve_conflicts() {
                    println!("node updated");
                } else {
                    println!("node stays unchanged")
                }
            }
            Command::Mine => {
                self.mine();
                debug!("{}", "Mined!!!".color(MSG_COLOR))
            }
        }
        Ok(())
    }

    pub fn get_basic_info(&self) -> PeerInfo {
        self.basic_info.clone()
    }

    /// Returns a copy of the blocks the node owns
    pub fn get_blocks(&self) -> Vec<Block> {
        self.chain.get_blocks()
    }

    /// Displays the full blockchain
    pub fn display(&self) {
        self.chain.display();
        println!();
    }

    /// Displays the peers
    pub fn display_peers(&self) {
        serde_json::to_writer_pretty(stdout(), &self.peers).expect("fail to display peers");
        println!();
    }

    /// Mines a new block
    pub fn mine(&mut self) {
        let last_block = self.chain.last_block();
        let proof = self.chain.run_pow();
        let last_hash = last_block.get_hash();
        // receive a reward for finding the proof.
        // The sender is "0" to signify that this node has mined a new coin.
        let bonus_trans = Transaction::new("0", self.basic_info.get_id(), 1);
        self.chain.add_new_transaction(&bonus_trans);

        let block = self.chain.create_new_block(proof, last_hash);
        info!(
            "A new block {} is forged, will broadcast it to all peers",
            block.get_index()
        );
        // broadcast the newly mined block
        self.async_broadcast_latest_block();
    }

    /// Adds a new transaction
    pub fn create_and_add_new_transaction(&mut self, sender: &str, receiver: &str, amount: i64) {
        let transaction = Transaction::new(sender, receiver, amount);
        if !self.chain.add_new_transaction(&transaction) {
            info!("Transaction already exists");
            return;
        }
        info!(
            "A new transaction is added: {} -> {}, amount: {}",
            sender, receiver, amount
        );
        self.async_broadcast_transaction(transaction);
    }

    pub fn handle_incoming_peer(&mut self, peer: PeerInfo) {
        if !self.add_peer(&peer) {
            debug!("Redundant incoming peer, simply drop it");
            return;
        }
        self.async_broadcast_peer(peer);
    }

    /// Take an incoming transaction and try to add it.
    /// If it already exists, drop it and do nothing.
    /// Else, add and broadcast it.
    pub fn handle_incoming_transaction(&mut self, transaction: Transaction) {
        if !self.chain.add_new_transaction(&transaction) {
            debug!("Redundant incoming transaction, simply drop it");
            return;
        }
        self.async_broadcast_transaction(transaction);
    }

    /// When a new block comes, check its index:
    ///
    /// If its index is lower than or equal to that of out latest block, drop it and do nothing.
    ///
    /// If its index is exactly one plus our latest block's index and its previous block is our
    /// latest block, then append it to the end of my chain.
    ///
    /// Else, do nothing to this block but then we need to resolve conflicts.
    pub fn handle_incoming_block(&mut self, block: Block) {
        if self.chain.add_new_block(&block) {
            // broadcast this good news to my friends~
            self.async_broadcast_latest_block();
        } else {
            // TODO: asynchronously resolve conflicts
        };
    }

    async fn async_broadcast_transaction(&self, transaction: Transaction) {
        // add this transaction to broadcast channel
        // which will then send it asynchronously
        self.broadcast_sender
            .send(Event::Broadcast(Request::NewTransaction(
                self.basic_info.clone(),
                transaction,
            )))
            .await;
    }

    async fn async_broadcast_block(&self, block: Block) {
        self.broadcast_sender
            .send(Event::Broadcast(Request::NewBlock(
                self.get_basic_info(),
                block,
            )))
            .await;
    }

    async fn async_broadcast_latest_block(&self) {
        self.async_broadcast_block(self.chain.last_block().to_owned())
            .await
    }

    async fn async_broadcast_peer(&self, peer: PeerInfo) {
        self.broadcast_sender
            .send(Event::Broadcast(Request::NewPeer(
                self.get_basic_info(),
                peer,
            )))
            .await;
    }

    fn broadcast_request(&self, req: &Request) -> Result<()> {
        debug!("{}", "broadcast begins".color(PROMINENT_COLOR));
        let peers = self.peers.clone();
        debug!("broadcasts request {:?} to peers :{:?}", req, peers);
        for peer in peers.iter() {
            debug!("Connecting {:?}", peer);
            match TcpStream::connect(peer.get_address()) {
                Ok(mut stream) => {
                    serde_json::to_writer(stream.try_clone()?, req)?;
                    stream.flush()?;
                    debug!("Request broadcast");
                }
                Err(e) => {
                    debug!("Connection to {:?} failed: {}", peer, e);
                    // Err(failure::err_msg("Failed to connect"))
                }
            };
            debug!("broadcast to one peer finished");
        }
        // Err(failure::err_msg("No peer to connect"))
        debug!("{}", "broadcast finished".color(PROMINENT_COLOR));
        Ok(())
    }

    /// Tries to greet and add a new peer at the given address.
    /// Returns false if `addr` is not a valid socket addr
    pub fn greet_and_add_peer(&mut self, addr: &str) -> bool {
        if let Ok(addr) = parse_addr(addr.to_owned()) {
            match TcpStream::connect(addr) {
                Ok(stream) => {
                    if let Ok(true) = self.say_hello(stream) {
                        true
                    } else {
                        false
                    }
                }
                Err(e) => {
                    error!("Error when communicating with {:?}: {}", addr, e);
                    false
                }
            }
        } else {
            error!("Invalid peer address {}", addr);
            false
        }
    }

    fn say_hello(&mut self, mut stream: TcpStream) -> Result<bool> {
        serde_json::to_writer(
            stream.try_clone()?,
            &Request::Hello(self.basic_info.clone()),
        )?;
        stream.flush()?;
        debug!("Request sent");
        // There should be only one response, but we have to deserialize from a stream in this way
        for response in Deserializer::from_reader(stream.try_clone()?).into_iter::<Response>() {
            let response =
                response.map_err(|e| failure::err_msg(format!("Deserializing error {}", e)))?;
            return if let Response::Ack(peer_info) = response {
                debug!("Ack for Hello received from: {:?}", peer_info);
                self.async_broadcast_peer(peer_info.clone());
                Ok(self.add_peer(&peer_info))
            } else {
                Err(failure::err_msg("Invalid response"))
            };
        }
        Err(failure::err_msg("No response"))
    }

    /// Adds a given `PeerInfo` to the peer list. Returns `false` if the peer already exists.
    pub fn add_peer(&mut self, peer: &PeerInfo) -> bool {
        if &self.basic_info == peer {
            debug!("Peer is myself");
            false
        } else if self.peers.contains(peer) {
            debug!("Peer already exists: {:?}", peer);
            false
        } else {
            debug!("New peer added: {:?}", peer);
            self.peers.insert(peer.clone());
            true
        }
    }

    pub fn update_chain(&mut self, new_blocks: Vec<Block>) -> bool {
        if new_blocks.len() <= self.chain.len() {
            return false;
        }
        let mut new_chain = Blockchain::from_blocks(new_blocks);
        if !Blockchain::valid_chain(&new_chain) {
            return false;
        }
        // add current transactions that are not on the chain yet
        // otherwise, these transaction would be lost!
        for t in self.chain.get_current_transactions() {
            new_chain.add_new_transaction(&t);
        }
        self.chain = new_chain;
        // broadcast only the latest block
        self.async_broadcast_latest_block();
        true
    }

    /// This is our Consensus Algorithm, it resolves conflicts (explicitly)
    /// by replacing our chain with the longest one in the network.
    /// Returns `true` if the chain is replaced
    pub fn resolve_conflicts(&mut self) -> bool {
        let mut ret = false;
        let peers = self.peers.clone();
        debug!("Resolve conflict with peers :{:?}", peers);
        for peer in peers.iter() {
            debug!("Connecting {:?}", peer);
            match TcpStream::connect(peer.get_address()) {
                Ok(stream) => {
                    debug!("Resolve conflict with peer :{:?}", peer);
                    match self.resolve_conflict(stream) {
                        Ok(flag) => {
                            ret = ret || flag;
                        }
                        Err(e) => {
                            error!("Error when communicating with {:?}: {}", peer, e);
                        }
                    }
                }
                Err(e) => error!("Connection to {:?} failed: {}", peer, e),
            }
        }
        ret
    }

    fn resolve_conflict(&mut self, mut stream: TcpStream) -> Result<bool> {
        serde_json::to_writer(
            stream.try_clone()?,
            &Request::HowAreYou(self.basic_info.clone()),
        )?;
        stream.flush()?;
        debug!("Request sent");
        // There should be only one response, but we have to deserialize from a stream in this way
        for response in Deserializer::from_reader(stream.try_clone()?).into_iter::<Response>() {
            let response =
                response.map_err(|e| failure::err_msg(format!("Deserializing error {}", e)))?;
            return if let Response::MyBlocks(_, blocks) = response {
                debug!("Response received");
                Ok(self.update_chain(blocks))
            } else {
                Err(failure::err_msg("Invalid response"))
            };
        }
        Err(failure::err_msg("No response"))
    }
}
