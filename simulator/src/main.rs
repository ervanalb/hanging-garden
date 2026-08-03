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
        self.network.borrow_mut().decrement_channel_counter(self.channel_id);
    }
}

struct Channel {
    name: String,
    receivers: Vec<mpsc::UnboundedSender<Vec<u8>>>,
}

#[derive(Clone, Debug)]
struct ChannelState {
    counter: usize,
    conflict: bool,
}

#[derive(Clone, Debug)]
struct NetworkSnapshot {
    timestamp: tokio::time::Instant,
    channels: Vec<ChannelState>,
}

struct NodeInfo {
    name: String,
    north: Option<ChannelId>,
    south: Option<ChannelId>,
    east: Option<ChannelId>,
    west: Option<ChannelId>,
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
            [node_info.north, node_info.south, node_info.east, node_info.west]
                .into_iter()
                .filter_map(|ch| ch)
                .collect()
        };

        futures::future::join_all(
            channel_ids
                .into_iter()
                .map(|channel_id| {
                    let network = self.network.clone();
                    let msg = message.clone();
                    async move {
                        Network::send_on_channel(network, channel_id, msg).await
                    }
                }),
        )
        .await;
    }
}

fn create_grid<F, Fut>(name: &str, rows: usize, cols: usize, protocol: F) -> (Rc<RefCell<Network>>, Vec<Fut>)
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
            let node_id = network.borrow_mut().add_node(node_name, north, south, east, west);

            let node = Node {
                network: network.clone(),
                node_id,
            };
            let handle = protocol(node);
            handles.push(handle);
        }
    }

    (network, handles)
}

async fn random_delay_test(node: Node) {
    let node_name = {
        let network = node.network.borrow();
        network.nodes[node.node_id].name.clone()
    };

    loop {
        let t = rand::random_range(0.5..1.0);
        tokio::time::sleep(tokio::time::Duration::from_secs_f64(t)).await;
        println!(
            "Node {} fired at time {:?}",
            node_name,
            tokio::time::Instant::now()
        );
        node.send_all(vec![0; 1024]).await;
    }
}

struct Network {
    nodes: Vec<NodeInfo>,
    channels: Vec<Channel>,
    history: Vec<NetworkSnapshot>,
}

impl Network {
    fn new() -> Self {
        Self {
            nodes: Vec::new(),
            channels: Vec::new(),
            history: vec![NetworkSnapshot {
                timestamp: tokio::time::Instant::now(),
                channels: Vec::new(),
            }],
        }
    }

    fn current_state(&self) -> &[ChannelState] {
        &self.history.last().unwrap().channels
    }

    fn mutate_state<F>(&mut self, f: F)
    where
        F: FnOnce(&mut Vec<ChannelState>),
    {
        let mut new_channels = self.history.last().unwrap().channels.clone();
        f(&mut new_channels);
        let snapshot = NetworkSnapshot {
            timestamp: tokio::time::Instant::now(),
            channels: new_channels,
        };
        self.history.push(snapshot);
    }

    fn get_history(&self) -> &[NetworkSnapshot] {
        &self.history
    }

    fn history_size(&self) -> usize {
        self.history.len()
    }

    fn add_channel(&mut self, name: String) -> ChannelId {
        let id = self.channels.len();
        self.channels.push(Channel {
            name,
            receivers: Vec::new(),
        });
        // Add new channel state to current snapshot
        self.mutate_state(|channels| {
            channels.push(ChannelState {
                counter: 0,
                conflict: false,
            });
        });
        id
    }

    fn add_node(&mut self, name: String, north: Option<ChannelId>, south: Option<ChannelId>, east: Option<ChannelId>, west: Option<ChannelId>) -> NodeId {
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
        self.mutate_state(|channels| {
            let state = &mut channels[channel_id];
            state.counter -= 1;
            if state.counter == 0 {
                state.conflict = false;
            }
        });
    }

    async fn send_on_channel(network: Rc<RefCell<Network>>, channel_id: ChannelId, message: Vec<u8>) {
        let message_time = Duration::from_secs_f64(message.len() as f64 / BYTES_PER_SECOND);

        // If counter >= 1, set conflict flag, then increment counter
        {
            let mut net = network.borrow_mut();
            net.mutate_state(|channels| {
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
            !net.current_state()[channel_id].conflict
        };

        if should_deliver {
            let net = network.borrow();
            let receivers = &net.channels[channel_id].receivers;
            for tx in receivers.iter() {
                let _ = tx.send(message.clone());
            }
        } else {
            println!("Message was lost in collision");
        }

        // Counter decrement and conflict clear happens in Drop
    }
}

fn main() {
    // Create the runtime
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .start_paused(true)
        .build()
        .unwrap();

    let (network, grid) = create_grid("grid", 10, 10, random_delay_test);

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
    }

    if let Some(last) = net.get_history().last() {
        println!("Last snapshot at: {:?}", last.timestamp);
        println!("\nLast snapshot channel states:");
        for (i, ch) in last.channels.iter().enumerate() {
            if ch.counter > 0 || ch.conflict {
                println!("  Channel {}: counter={}, conflict={}", i, ch.counter, ch.conflict);
            }
        }
    }
}
