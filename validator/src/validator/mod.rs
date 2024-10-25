use std::cell::UnsafeCell;
use std::cmp::min;
use std::fs;
use std::io::{Read, Write};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, TcpStream};
use std::ops::{Deref, DerefMut};
use std::path::{Path, PathBuf};
use std::simd::Simd;
use std::sync::Arc;

use anyhow::Result;
use dirs;
use serde::{Deserialize, Serialize};
use threadpool::ThreadPool;
use tracing::{error, info};

use neuron::{AccountId, config, hotkey_location, Keypair, load_key_seed, NeuronInfoLite, Subtensor};

use crate::validator::memory_storage::{MemoryMappedFile, MemoryMappedStorage};

mod memory_storage;

const VERSION_KEY: u64 = 1;

#[derive(Clone)]
struct CurrentRow(Arc<UnsafeCell<MemoryMappedStorage>>);

impl CurrentRow {
    fn new(path: impl AsRef<Path>) -> Result<Self> {
        Ok(Self(Arc::new(UnsafeCell::new(MemoryMappedStorage::new(path)?))))
    }
}

unsafe impl Send for CurrentRow {}

impl Deref for CurrentRow {
    type Target = MemoryMappedStorage;

    fn deref(&self) -> &Self::Target {
        unsafe { &*self.0.get() }
    }
}

impl DerefMut for CurrentRow {
    fn deref_mut(&mut self) -> &mut Self::Target {
        unsafe { &mut *self.0.get() }
    }
}

#[derive(Debug, Serialize, Deserialize, Default)]
struct ValidatorState {
    step: u64,
    hotkeys: Vec<AccountId>,
    scores: Vec<u16>,
}

pub struct Validator {
    keypair: Keypair,
    subtensor: Subtensor,
    neurons: Vec<NeuronInfoLite>,
    uid: u16,

    current_row: CurrentRow,
    center_column: MemoryMappedFile,
    state: ValidatorState,

    last_metagraph_sync: u64,

    thread_pool: ThreadPool,
}

impl Validator {
    fn find_neuron_info<'a>(
        neurons: &'a [NeuronInfoLite],
        account_id: &AccountId,
    ) -> Option<&'a NeuronInfoLite> {
        neurons.iter().find(|neuron| &neuron.hotkey == account_id)
    }

    fn not_registered(account_id: &AccountId) -> ! {
        panic!(
            "Hotkey {account_id} is not registered in sn{}",
            *config::NETUID
        );
    }

    pub async fn new() -> Self {
        let hotkey_location = hotkey_location(&*config::WALLET_NAME, &*config::HOTKEY_NAME).expect("No home directory found");
        let seed = load_key_seed(&hotkey_location).unwrap();

        let keypair = Keypair::from_seed(&seed).unwrap();

        let subtensor = Subtensor::new(&*config::CHAIN_ENDPOINT).await.unwrap();

        let neurons: Vec<NeuronInfoLite> = subtensor.get_neurons(*config::NETUID).await.unwrap();

        let last_metagraph_sync = subtensor.get_block_number().await.unwrap();
        let neuron_info = Self::find_neuron_info(&neurons, keypair.account_id());

        let uid = if let Some(neuron_info) = neuron_info {
            neuron_info.uid.0
        } else {
            Self::not_registered(keypair.account_id());
        };

        let hotkeys: Vec<AccountId> = neurons.iter().map(|neuron| neuron.hotkey.clone()).collect();
        let scores = vec![0; hotkeys.len()];

        let state = ValidatorState {
            step: 1,
            scores,
            hotkeys,
        };

        let current_row = CurrentRow::new("current_row.bin").unwrap();
        let center_column = MemoryMappedFile::open("center_column.bin").unwrap();

        let mut validator = Self {
            keypair,
            subtensor,
            neurons,
            uid,
            current_row,
            center_column,
            state,
            last_metagraph_sync,
            thread_pool: ThreadPool::new(256),
        };

        validator.load_state().unwrap();

        if validator.state.step == 1 {
            // Initial state
            validator.current_row[0] = 1;
            validator.center_column[0] = 1;
        }

        validator
    }

    fn state_path(&self) -> PathBuf {
        let mut dir = dirs::home_dir().expect("Could not find home directory");

        dir.push(".bittensor");
        dir.push("miners");
        dir.push(&*config::WALLET_NAME);
        dir.push(&*config::HOTKEY_NAME);
        dir.push(format!("netuid{}", *config::NETUID));
        dir.push("validator");
        dir.push("state.json");

        dir
    }

    fn save_state(&self) -> Result<()> {
        let path = self.state_path();

        self.center_column.flush()?;
        self.current_row.flush()?;

        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }

        let json = serde_json::to_string(&self.state)?;

        fs::write(&path, json)?;

        Ok(())
    }

    fn load_state(&mut self) -> Result<()> {
        let path = self.state_path();
        if !path.exists() {
            return Ok(());
        }

        let json = fs::read_to_string(&path)?;

        self.state = serde_json::from_str(&json)?;

        Ok(())
    }

    async fn sync(&mut self, block: Option<u64>) -> Result<()> {
        self.neurons = self.subtensor.get_neurons(*config::NETUID).await?;

        let block = if let Some(block) = block {
            block
        } else {
            self.subtensor.get_block_number().await?
        };

        self.last_metagraph_sync = block;

        let neuron_info = Self::find_neuron_info(&self.neurons, self.keypair.account_id());

        let neuron_info = if let Some(neuron_info) = neuron_info {
            neuron_info
        } else {
            Self::not_registered(self.keypair.account_id());
        };

        self.uid = neuron_info.uid.0;

        // Update scores array size if needed
        if self.state.hotkeys.len() != self.neurons.len() {
            let mut new_scores = vec![0; self.neurons.len()];
            new_scores[..self.state.scores.len()].copy_from_slice(&self.state.scores);
            self.state.scores = new_scores;
        }

        // Set weights if enough time has passed
        if block - neuron_info.last_update.0 >= *config::EPOCH_LENGTH {
            self.subtensor
                .set_weights(
                    &self.keypair,
                    *config::NETUID,
                    self.state
                        .scores
                        .iter()
                        .enumerate()
                        .map(|(uid, &score)| (uid as u16, score))
                        .collect(),
                    VERSION_KEY,
                )
                .await?;
        }

        Ok(())
    }

    fn handle_connection(mut current_row: CurrentRow, mut connection: TcpStream, start: usize, end: usize) {
        let buffer_size = min(end - start, 8 * 4 * 512);

        let iterations = (end - start) / buffer_size;

        for i in 0..iterations {
            let from = start + i * buffer_size;
            let to = start + (i + 1) * buffer_size;

            // TODO error handle
            connection.write(&current_row[from..to]).unwrap();
            connection.read(&mut current_row[from..to]).unwrap();
        }
    }

    async fn do_step(&mut self) -> Result<()> {
        info!("Evolution step {}", self.state.step);

        let current_block = self.subtensor.get_block_number().await?;
        let elapsed_blocks = current_block - self.last_metagraph_sync;

        if elapsed_blocks >= *config::EPOCH_LENGTH {
            self.sync(Some(current_block)).await?;
        }

        let mut connections = Vec::with_capacity(256);

        for neuron in &self.neurons {
            let ip: IpAddr = if neuron.axon_info.ip_type == 4 {
                Ipv4Addr::from(neuron.axon_info.ip as u32).into()
            } else {
                Ipv6Addr::from(neuron.axon_info.ip).into()
            };

            let address = SocketAddr::new(ip, neuron.axon_info.port);

            if let Ok(stream) = TcpStream::connect(address) {
                connections.push(stream);
            }
        }

        let connection_count = connections.len();
        let byte_count = (self.state.step / 4 + 1) as usize;

        let chunk_size = if connection_count % 2 == 0 {
            byte_count / connection_count + 1
        } else {
            byte_count / connection_count
        };

        // TODO Handle connection prematurely dying or giving invalid results
        for (index, connection) in connections.into_iter().enumerate() {
            let row = self.current_row.clone();

            self.thread_pool.execute(move || Self::handle_connection(row, connection, index * chunk_size, (index + 1) * chunk_size));
        }

        self.thread_pool.join();

        self.state.step += 1;
        self.save_state()?;

        Ok(())
    }

    pub(crate) async fn run(&mut self) {
        loop {
            if let Err(e) = self.do_step().await {
                error!(
                    "Error during evolution step {step}, {e}",
                    step = self.state.step
                );
            }
        }
    }

    fn normalize_response_data(lists: &mut [Simd<u8, 32>]) -> Vec<u8> {
        fn rule_30(a: u64) -> u64 {
            a ^ ((a << 1) | (a << 2))
        }

        fn normalize_pair(a: u8, b: u8) -> (u8, u8) {
            // Convert u8 to u64 for processing
            let (new_a, new_b) = {
                let a = a as u64;
                let b = b as u64;
                let carry = a & 1;
                let mut a = a >> 1;
                let mut b = (carry << 63) | b;
                a = rule_30(a);
                b = rule_30(b);
                let msb = b >> 63;
                b &= (1 << 63) - 1;
                a = (a << 1) | msb;
                (a as u8, b as u8) // Convert back to u8
            };
            (new_a, new_b)
        }

        let mut normalized_outputs = Vec::new();

        // Process lists
        for i in 0..lists.len() - 1 {
            let mut current_list = lists[i].to_array();
            let mut next_list = lists[i + 1].to_array();

            let (new_last, new_first) = normalize_pair(
                current_list[31], // last element of current list
                next_list[0],     // first element of next list
            );

            current_list[31] = new_last;
            next_list[0] = new_first;

            // Update the lists with normalized values
            lists[i] = Simd::from_array(current_list);
            lists[i + 1] = Simd::from_array(next_list);

            // Extend normalized_outputs with current list
            normalized_outputs.extend_from_slice(&current_list);
        }

        // Add the last list if there was more than one list
        if lists.len() > 1 {
            normalized_outputs.extend_from_slice(&lists[lists.len() - 1].to_array());
        }

        normalized_outputs
    }
}