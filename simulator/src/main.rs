use std::cell::RefCell;
use std::rc::Rc;
use tokio::sync::mpsc;
use tokio::time::Duration;

const BYTES_PER_SECOND: f64 = 1_000_000. / 8.;

struct TransmissionGuard {
    counter: Rc<RefCell<usize>>,
    conflict: Rc<RefCell<bool>>,
}

impl Drop for TransmissionGuard {
    fn drop(&mut self) {
        *self.counter.borrow_mut() -= 1;
        if *self.counter.borrow() == 0 {
            *self.conflict.borrow_mut() = false;
        }
    }
}

#[derive(Clone)]
struct Channel {
    counter: Rc<RefCell<usize>>,
    conflict: Rc<RefCell<bool>>,
    receivers: Rc<RefCell<Vec<mpsc::UnboundedSender<Vec<u8>>>>>,
}

impl Channel {
    fn new() -> Self {
        Self {
            counter: Rc::new(RefCell::new(0)),
            conflict: Rc::new(RefCell::new(false)),
            receivers: Rc::new(RefCell::new(Vec::new())),
        }
    }

    fn create_receiver(&self) -> mpsc::UnboundedReceiver<Vec<u8>> {
        let (tx, rx) = mpsc::unbounded_channel();
        self.receivers.borrow_mut().push(tx);
        rx
    }

    async fn send(&self, message: Vec<u8>) {
        let message_time = Duration::from_secs_f64(message.len() as f64 / BYTES_PER_SECOND);

        // If counter >= 1, set conflict flag, then increment counter
        {
            let mut counter = self.counter.borrow_mut();
            if *counter >= 1 {
                *self.conflict.borrow_mut() = true;
            }
            *counter += 1;
        }

        // RAII guard ensures counter is decremented even if future is dropped
        let _guard = TransmissionGuard {
            counter: self.counter.clone(),
            conflict: self.conflict.clone(),
        };

        // Sleep for message duration
        tokio::time::sleep(message_time).await;

        // If conflict flag is clear, deliver message
        if !*self.conflict.borrow() {
            let receivers = self.receivers.borrow();
            for tx in receivers.iter() {
                let _ = tx.send(message.clone());
            }
        } else {
            println!("Message was lost in collision");
        }

        // Counter decrement and conflict clear happens in Drop
    }
}

struct NodeChannels {
    north: Option<Channel>,
    south: Option<Channel>,
    east: Option<Channel>,
    west: Option<Channel>,
}

impl NodeChannels {
    async fn send_all(&self, message: Vec<u8>) {
        futures::future::join_all(
            [&self.north, &self.south, &self.east, &self.west]
                .into_iter()
                .filter_map(|ch| ch.as_ref())
                .map(|c| c.send(message.clone())),
        )
        .await;
    }
}

fn create_grid<F, Fut>(name: &str, rows: usize, cols: usize, protocol: F) -> Vec<Fut>
where
    F: Fn(String, NodeChannels) -> Fut + 'static,
    Fut: std::future::Future<Output = ()> + 'static,
{
    // Create vertical channels (north-south connections)
    let mut vertical_channels = Vec::new();
    for _ in 0..rows - 1 {
        let mut row_channels = Vec::new();
        for _ in 0..cols {
            row_channels.push(Channel::new());
        }
        vertical_channels.push(row_channels);
    }

    // Create horizontal channels (east-west connections)
    let mut horizontal_channels = Vec::new();
    for _ in 0..rows {
        let mut row_channels = Vec::new();
        for _ in 0..cols - 1 {
            row_channels.push(Channel::new());
        }
        horizontal_channels.push(row_channels);
    }

    // Create and spawn nodes
    let mut handles = Vec::new();
    for row in 0..rows {
        for col in 0..cols {
            let north = if row > 0 {
                Some(vertical_channels[row - 1][col].clone())
            } else {
                None
            };

            let south = if row < rows - 1 {
                Some(vertical_channels[row][col].clone())
            } else {
                None
            };

            let west = if col > 0 {
                Some(horizontal_channels[row][col - 1].clone())
            } else {
                None
            };

            let east = if col < cols - 1 {
                Some(horizontal_channels[row][col].clone())
            } else {
                None
            };

            let channels = NodeChannels {
                north,
                south,
                east,
                west,
            };
            let handle = protocol(format!("{name} r{row} c{col}"), channels);
            handles.push(handle);
        }
    }

    handles
}

async fn random_delay_test(name: String, channels: NodeChannels) {
    loop {
        let t = rand::random_range(0.5..1.0);
        tokio::time::sleep(tokio::time::Duration::from_secs_f64(t)).await;
        println!(
            "Node {} fired at time {:?}",
            name,
            tokio::time::Instant::now()
        );
        channels.send_all(vec![0; 1024]).await;
    }
}

fn main() {
    // Create the runtime
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .start_paused(true)
        .build()
        .unwrap();

    let grid = create_grid("grid", 10, 10, random_delay_test);

    // Spawn the root task
    rt.block_on(async {
        let _ = tokio::time::timeout(
            tokio::time::Duration::from_secs(5),
            futures::future::join_all(grid),
        )
        .await;
    })
}
