mod visualizer;

use serde::{Deserialize, Serialize};
use std::cell::RefCell;
use std::rc::Rc;
use tokio::sync::mpsc;
use tokio::time::Duration;

const BYTES_PER_SECOND: f64 = 1_000_000. / 8.;

type ChannelId = usize;
type NodeId = usize;

struct TransmissionGuard {
    network: Rc<RefCell<Network>>,
    channel_id: ChannelId,
}

impl Drop for TransmissionGuard {
    fn drop(&mut self) {
        self.network
            .borrow_mut()
            .decrement_channel_counter(self.channel_id);
    }
}

pub struct Channel {
    name: String,
    receivers: Vec<mpsc::UnboundedSender<Vec<u8>>>,
}

#[derive(Clone, Debug)]
pub struct ChannelState {
    pub counter: usize,
    pub conflict: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Color {
    pub r: u8,
    pub g: u8,
    pub b: u8,
}

#[derive(Clone, Debug)]
pub struct NodeState {
    pub colors: Vec<Color>,
    pub comm_state: CommState,
}

impl NodeState {
    fn new() -> Self {
        Self {
            colors: vec![Color { r: 0, g: 0, b: 0 }; 100],
            comm_state: CommState {
                seq_num: 0,
                type_: CommStateType::Nop,
            },
        }
    }
}

#[derive(Clone, Debug)]
pub struct NetworkSnapshot {
    pub timestamp: tokio::time::Instant,
    pub channels: Vec<ChannelState>,
    pub nodes: Vec<NodeState>,
}

#[derive(Clone)]
pub struct NodeInfo {
    pub name: String,
    pub north: Option<ChannelId>,
    pub south: Option<ChannelId>,
    pub east: Option<ChannelId>,
    pub west: Option<ChannelId>,
}

struct Node {
    network: Rc<RefCell<Network>>,
    node_id: NodeId,
}

impl Node {
    async fn send_all(&self, message: Vec<u8>) {
        let channel_ids: Vec<ChannelId> = {
            let network = self.network.borrow();
            let node_info = &network.nodes[self.node_id];
            [
                node_info.north,
                node_info.south,
                node_info.east,
                node_info.west,
            ]
            .into_iter()
            .filter_map(|ch| ch)
            .collect()
        };

        futures::future::join_all(channel_ids.into_iter().map(|channel_id| {
            let network = self.network.clone();
            let msg = message.clone();
            async move { Network::send_on_channel(network, channel_id, msg).await }
        }))
        .await;
    }

    async fn send_directional(&self, messages: [Vec<u8>; 4]) {
        let channel_ids = {
            let network = self.network.borrow();
            let node_info = &network.nodes[self.node_id];
            [
                node_info.north,
                node_info.south,
                node_info.east,
                node_info.west,
            ]
        };

        futures::future::join_all(channel_ids.into_iter().zip(messages).filter_map(
            |(channel_id, msg)| {
                channel_id.map(|ch_id| {
                    let network = self.network.clone();
                    async move { Network::send_on_channel(network, ch_id, msg).await }
                })
            },
        ))
        .await;
    }
}

fn create_grid<F, Fut>(
    name: &str,
    rows: usize,
    cols: usize,
    protocol: F,
) -> (Rc<RefCell<Network>>, Vec<Fut>)
where
    F: Fn(Node) -> Fut + 'static,
    Fut: std::future::Future<Output = ()> + 'static,
{
    let network = Rc::new(RefCell::new(Network::new()));

    // Create vertical channels (north-south connections)
    let mut vertical_channels = Vec::new();
    for r in 0..rows - 1 {
        let mut row_channels = Vec::new();
        for c in 0..cols {
            let channel_name = format!("{} r{}-{} c{}", name, r, r + 1, c);
            let channel_id = network.borrow_mut().add_channel(channel_name);
            row_channels.push(channel_id);
        }
        vertical_channels.push(row_channels);
    }

    // Create horizontal channels (east-west connections)
    let mut horizontal_channels = Vec::new();
    for r in 0..rows {
        let mut row_channels = Vec::new();
        for c in 0..cols - 1 {
            let channel_name = format!("{} r{} c{}-{}", name, r, c, c + 1);
            let channel_id = network.borrow_mut().add_channel(channel_name);
            row_channels.push(channel_id);
        }
        horizontal_channels.push(row_channels);
    }

    // Create and spawn nodes
    let mut handles = Vec::new();
    for row in 0..rows {
        for col in 0..cols {
            let north = if row > 0 {
                Some(vertical_channels[row - 1][col])
            } else {
                None
            };

            let south = if row < rows - 1 {
                Some(vertical_channels[row][col])
            } else {
                None
            };

            let west = if col > 0 {
                Some(horizontal_channels[row][col - 1])
            } else {
                None
            };

            let east = if col < cols - 1 {
                Some(horizontal_channels[row][col])
            } else {
                None
            };

            let node_name = format!("{name} r{row} c{col}");
            let node_id = network
                .borrow_mut()
                .add_node(node_name, north, south, east, west);

            let node = Node {
                network: network.clone(),
                node_id,
            };
            let handle = protocol(node);
            handles.push(handle);
        }
    }

    // Initialize state after topology is complete
    network.borrow_mut().initialize_state();

    (network, handles)
}

#[derive(PartialOrd, Ord, PartialEq, Eq, Serialize, Deserialize, Clone, Debug)]
pub struct CommState {
    pub seq_num: u32,
    pub type_: CommStateType,
}

impl CommState {
    fn is_consistent(&self, other: &Self) -> bool {
        self.seq_num == other.seq_num && self.type_.is_consistent(&other.type_)
    }

    /// Serialize CommState to bytes with CRC checksum and COBS framing
    /// Returns a Vec<u8> containing the COBS-encoded frame
    fn serialize(&self) -> Result<Vec<u8>, postcard::Error> {
        // 1. Serialize to bytes using postcard
        let serialized = postcard::to_allocvec(self)?;

        // 2. Calculate CRC32 checksum
        let crc = crc::Crc::<u32>::new(&crc::CRC_32_ISO_HDLC);
        let checksum = crc.checksum(&serialized);

        // 3. Append checksum to serialized data
        let mut data_with_crc = serialized;
        data_with_crc.extend_from_slice(&checksum.to_le_bytes());

        // 4. COBS encode the data
        // COBS encoding adds 1 byte overhead plus potential padding
        let mut encoded =
            vec![0u8; data_with_crc.len() + cobs::max_encoding_length(data_with_crc.len())];
        let encoded_len = cobs::encode(&data_with_crc, &mut encoded);
        encoded.truncate(encoded_len);

        // Add delimiter (0x00) at the end
        encoded.push(0x00);

        Ok(encoded)
    }

    /// Deserialize CommState from COBS-framed bytes with CRC verification
    /// The input should include the COBS encoding and trailing delimiter
    fn deserialize(encoded: &[u8]) -> Result<Self, DeserializeError> {
        // 1. Remove trailing delimiter if present
        let encoded = if encoded.last() == Some(&0x00) {
            &encoded[..encoded.len() - 1]
        } else {
            encoded
        };

        // 2. COBS decode
        let mut decoded = vec![0u8; encoded.len()];
        let decoded_len =
            cobs::decode(encoded, &mut decoded).map_err(|_| DeserializeError::CobsDecoding)?;
        decoded.truncate(decoded_len);

        // 3. Extract and verify CRC checksum
        if decoded.len() < 4 {
            return Err(DeserializeError::TooShort);
        }

        let (data, crc_bytes) = decoded.split_at(decoded.len() - 4);
        let received_crc =
            u32::from_le_bytes([crc_bytes[0], crc_bytes[1], crc_bytes[2], crc_bytes[3]]);

        let crc = crc::Crc::<u32>::new(&crc::CRC_32_ISO_HDLC);
        let computed_crc = crc.checksum(data);

        if received_crc != computed_crc {
            return Err(DeserializeError::CrcMismatch {
                expected: computed_crc,
                received: received_crc,
            });
        }

        // 4. Deserialize with postcard
        let state = postcard::from_bytes(data).map_err(DeserializeError::Postcard)?;

        Ok(state)
    }

    fn propagate(&self) -> [Self; 4] {
        self.type_.propagate().map(|type_| Self {
            seq_num: self.seq_num,
            type_,
        })
    }
}

#[derive(Debug)]
pub enum DeserializeError {
    CobsDecoding,
    TooShort,
    CrcMismatch { expected: u32, received: u32 },
    Postcard(postcard::Error),
}

impl std::fmt::Display for DeserializeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DeserializeError::CobsDecoding => write!(f, "COBS decoding failed"),
            DeserializeError::TooShort => write!(f, "Data too short to contain CRC"),
            DeserializeError::CrcMismatch { expected, received } => {
                write!(
                    f,
                    "CRC mismatch: expected {:#x}, received {:#x}",
                    expected, received
                )
            }
            DeserializeError::Postcard(e) => write!(f, "Postcard deserialization error: {}", e),
        }
    }
}

impl std::error::Error for DeserializeError {}

#[derive(PartialOrd, Ord, PartialEq, Eq, Serialize, Deserialize, Clone, Debug)]
pub enum CommStateType {
    Nop,
}

impl CommStateType {
    fn is_consistent(&self, other: &Self) -> bool {
        match (self, other) {
            (CommStateType::Nop, CommStateType::Nop) => true,
            //_ => false,
        }
    }
    fn propagate(&self) -> [Self; 4] {
        match self {
            CommStateType::Nop => std::array::from_fn(|_| CommStateType::Nop),
        }
    }
}

async fn trickle_protocol(node: Node, mut event_rx: mpsc::UnboundedReceiver<CommState>) {
    // Trickle algorithm constants
    const IMIN: f64 = 0.01; // 10ms minimum interval
    const IMAX: f64 = 10.0; // 10 second maximum interval
    const K: usize = 1; // redundancy constant

    // Create receivers for each direction (north, south, east, west)
    let (mut north_rx, mut south_rx, mut east_rx, mut west_rx) = {
        let mut network = node.network.borrow_mut();
        (
            network.nodes[node.node_id]
                .north
                .map(|ch| network.create_receiver(ch)),
            network.nodes[node.node_id]
                .south
                .map(|ch| network.create_receiver(ch)),
            network.nodes[node.node_id]
                .east
                .map(|ch| network.create_receiver(ch)),
            network.nodes[node.node_id]
                .west
                .map(|ch| network.create_receiver(ch)),
        )
    };

    // Trickle algorithm state
    let mut interval = IMIN;

    'interval_loop: loop {
        // 1. Start of interval
        let mut counter = 0;
        let t = rand::random_range(interval / 2.0..interval);

        // Read current state from network
        let mut current_state = node.network.borrow().current_node_state()[node.node_id]
            .comm_state
            .clone();

        // Helper to update state in both local variable and network
        let update_state = |new_state: CommState| {
            let node_id = node.node_id;
            node.network
                .borrow_mut()
                .mutate_nodes(|nodes| nodes[node_id].comm_state = new_state.clone());
            new_state
        };

        // Macro to handle message/event with timeout
        macro_rules! wait_for_event_or_timeout {
            ($deadline:expr) => {{
                let mut should_reset = false;
                loop {
                    let remaining =
                        $deadline.saturating_duration_since(tokio::time::Instant::now());
                    if remaining.is_zero() {
                        break;
                    }

                    let result = tokio::time::timeout(remaining, async {
                        tokio::select! {
                            Some(msg) = async {
                                tokio::select! {
                                    Some(msg) = async {
                                        match &mut north_rx {
                                            Some(rx) => rx.recv().await,
                                            None => futures::future::pending().await,
                                        }
                                    } => Some(msg),
                                    Some(msg) = async {
                                        match &mut south_rx {
                                            Some(rx) => rx.recv().await,
                                            None => futures::future::pending().await,
                                        }
                                    } => Some(msg),
                                    Some(msg) = async {
                                        match &mut east_rx {
                                            Some(rx) => rx.recv().await,
                                            None => futures::future::pending().await,
                                        }
                                    } => Some(msg),
                                    Some(msg) = async {
                                        match &mut west_rx {
                                            Some(rx) => rx.recv().await,
                                            None => futures::future::pending().await,
                                        }
                                    } => Some(msg),
                                }
                            } => {
                                // Handle incoming message
                                match CommState::deserialize(&msg) {
                                    Ok(received_state) => {
                                        if received_state > current_state {
                                            // Received message takes precedence
                                            current_state = update_state(received_state);
                                            interval = IMIN;
                                            Some(true)
                                        } else if received_state < current_state {
                                            if !current_state.is_consistent(&received_state) {
                                                // Received message is outdated
                                                interval = IMIN;
                                                Some(true)
                                            } else {
                                                // Received message is consistent
                                                Some(false)
                                            }
                                        } else {
                                            counter += 1;
                                            Some(false)
                                        }
                                    }
                                    Err(_) => Some(false),
                                }
                            }
                            Some(new_state) = event_rx.recv() => {
                                current_state = update_state(new_state);
                                interval = IMIN;
                                Some(true)
                            }
                        }
                    })
                    .await;

                    match result {
                        Ok(Some(true)) => {
                            should_reset = true;
                            break;
                        }
                        Ok(Some(false)) | Ok(None) => continue,
                        Err(_) => break,
                    }
                }
                should_reset
            }};
        }

        // 2. Wait for timer t expiry (possibly interrupted by msg recv or event)
        let t_deadline = tokio::time::Instant::now() + Duration::from_secs_f64(t);
        if wait_for_event_or_timeout!(t_deadline) {
            continue 'interval_loop;
        }

        // 3. Handle timer t expiry
        if counter < K {
            let states = current_state.propagate();
            let messages: [Vec<u8>; 4] =
                states.map(|state| state.serialize().unwrap_or_else(|_| vec![]));
            node.send_directional(messages).await;
        }

        // 4. Wait for interval expiry (possibly interrupted by msg recv or event)
        let interval_deadline = tokio::time::Instant::now() + Duration::from_secs_f64(interval - t);
        if wait_for_event_or_timeout!(interval_deadline) {
            continue 'interval_loop;
        }

        // 5. Handle interval expiry - double interval and loop
        interval = (interval * 2.0).min(IMAX);
    }
}

async fn node_protocol(node: Node) {
    // Create a channel for node-generated events
    let (event_tx, event_rx) = mpsc::unbounded_channel::<CommState>();

    // Clone network and node_id for event generator
    let network = node.network.clone();
    let node_id = node.node_id;

    // Spawn a task to generate events periodically
    tokio::select! {
        () = async move {
            loop {
                tokio::time::sleep(Duration::from_secs(3)).await;
                let net = network.borrow();
                let seq_num = net.current_node_state()[node_id].comm_state.seq_num;
                let new_state = CommState {
                    seq_num: seq_num + 1,
                    type_: CommStateType::Nop,
                };
                let _ = event_tx.send(new_state);
            }
        } => {}
        () = trickle_protocol(node, event_rx) => {}
    };
}

pub struct Network {
    pub nodes: Vec<NodeInfo>,
    channels: Vec<Channel>,
    history: Vec<NetworkSnapshot>,
}

impl Network {
    fn new() -> Self {
        Self {
            nodes: Vec::new(),
            channels: Vec::new(),
            history: Vec::new(),
        }
    }

    fn current_channel_state(&self) -> &[ChannelState] {
        &self.history.last().unwrap().channels
    }

    fn current_node_state(&self) -> &[NodeState] {
        &self.history.last().unwrap().nodes
    }

    fn initialize_state(&mut self) {
        // Create initial snapshot with all nodes and channels in default state
        let initial_snapshot = NetworkSnapshot {
            timestamp: tokio::time::Instant::now(),
            channels: vec![
                ChannelState {
                    counter: 0,
                    conflict: false,
                };
                self.channels.len()
            ],
            nodes: vec![NodeState::new(); self.nodes.len()],
        };
        self.history.push(initial_snapshot);
    }

    fn mutate<F>(&mut self, f: F)
    where
        F: FnOnce(&mut Vec<ChannelState>, &mut Vec<NodeState>),
    {
        let last = self.history.last().unwrap();
        let mut new_channels = last.channels.clone();
        let mut new_nodes = last.nodes.clone();
        f(&mut new_channels, &mut new_nodes);
        let snapshot = NetworkSnapshot {
            timestamp: tokio::time::Instant::now(),
            channels: new_channels,
            nodes: new_nodes,
        };
        self.history.push(snapshot);
    }

    fn mutate_channels<F>(&mut self, f: F)
    where
        F: FnOnce(&mut Vec<ChannelState>),
    {
        self.mutate(|channels, _nodes| f(channels));
    }

    fn mutate_nodes<F>(&mut self, f: F)
    where
        F: FnOnce(&mut Vec<NodeState>),
    {
        self.mutate(|_channels, nodes| f(nodes));
    }

    pub fn get_history(&self) -> &[NetworkSnapshot] {
        &self.history
    }

    pub fn history_size(&self) -> usize {
        self.history.len()
    }

    fn add_channel(&mut self, name: String) -> ChannelId {
        let id = self.channels.len();
        self.channels.push(Channel {
            name,
            receivers: Vec::new(),
        });
        id
    }

    fn add_node(
        &mut self,
        name: String,
        north: Option<ChannelId>,
        south: Option<ChannelId>,
        east: Option<ChannelId>,
        west: Option<ChannelId>,
    ) -> NodeId {
        let id = self.nodes.len();
        self.nodes.push(NodeInfo {
            name,
            north,
            south,
            east,
            west,
        });
        id
    }

    fn create_receiver(&mut self, channel_id: ChannelId) -> mpsc::UnboundedReceiver<Vec<u8>> {
        let (tx, rx) = mpsc::unbounded_channel();
        self.channels[channel_id].receivers.push(tx);
        rx
    }

    fn decrement_channel_counter(&mut self, channel_id: ChannelId) {
        self.mutate_channels(|channels| {
            let state = &mut channels[channel_id];
            state.counter -= 1;
            if state.counter == 0 {
                state.conflict = false;
            }
        });
    }

    async fn send_on_channel(
        network: Rc<RefCell<Network>>,
        channel_id: ChannelId,
        message: Vec<u8>,
    ) {
        let message_time = Duration::from_secs_f64(message.len() as f64 / BYTES_PER_SECOND);

        // If counter >= 1, set conflict flag, then increment counter
        {
            let mut net = network.borrow_mut();
            net.mutate_channels(|channels| {
                let state = &mut channels[channel_id];
                if state.counter >= 1 {
                    state.conflict = true;
                }
                state.counter += 1;
            });
        }

        // RAII guard ensures counter is decremented even if future is dropped
        let _guard = TransmissionGuard {
            network: network.clone(),
            channel_id,
        };

        // Sleep for message duration
        tokio::time::sleep(message_time).await;

        // If conflict flag is clear, deliver message
        let should_deliver = {
            let net = network.borrow();
            !net.current_channel_state()[channel_id].conflict
        };

        if should_deliver {
            let net = network.borrow();
            let receivers = &net.channels[channel_id].receivers;
            for tx in receivers.iter() {
                let _ = tx.send(message.clone());
            }
        } else {
            // Message was lost in collision
        }

        // Counter decrement and conflict clear happens in Drop
    }
}

fn main() {
    let grid_rows = 10;
    let grid_cols = 10;

    // Create the runtime
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .start_paused(true)
        .build()
        .unwrap();

    let (network, grid) = create_grid("grid", grid_rows, grid_cols, node_protocol);

    // Spawn the root task
    rt.block_on(async {
        let _ = tokio::time::timeout(
            tokio::time::Duration::from_secs(5),
            futures::future::join_all(grid),
        )
        .await;
    });

    // Print history statistics
    let net = network.borrow();
    println!("\n=== Network History Statistics ===");
    println!("Total snapshots recorded: {}", net.history_size());

    if let Some(first) = net.get_history().first() {
        println!("First snapshot at: {:?}", first.timestamp);
        println!("Nodes in first snapshot: {}", first.nodes.len());
    }

    if let Some(last) = net.get_history().last() {
        println!("Last snapshot at: {:?}", last.timestamp);
        println!("Nodes in last snapshot: {}", last.nodes.len());

        println!("\nLast snapshot channel states:");
        for (i, ch) in last.channels.iter().enumerate() {
            if ch.counter > 0 || ch.conflict {
                println!(
                    "  Channel {}: counter={}, conflict={}",
                    i, ch.counter, ch.conflict
                );
            }
        }
    }

    // Launch visualizer (check for --no-viz flag)
    let args: Vec<String> = std::env::args().collect();
    if !args.contains(&"--no-viz".to_string()) {
        println!("\nLaunching visualizer...");
        println!("(Use --no-viz flag to skip visualization)");
        let visualizer_data = visualizer::VisualizerData {
            snapshots: net.get_history().to_vec(),
            nodes: net.nodes.clone(),
            grid_rows,
            grid_cols,
        };
        drop(net); // Release the borrow

        let vis = visualizer::Visualizer::new(visualizer_data);
        vis.run();
    } else {
        println!("\nSkipping visualization (--no-viz flag set)");
    }
}
