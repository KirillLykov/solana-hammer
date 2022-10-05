#![allow(dead_code)]
#![allow(unused_variables)]

use rand::RngCore;
//use solana_sdk::instruction::Instruction;
use solana_client::rpc_client::RpcClient;
use solana_sdk::commitment_config::CommitmentConfig;
use solana_sdk::hash::Hash;
use solana_sdk::instruction::{AccountMeta, Instruction};
use solana_sdk::message::Message;
use solana_sdk::native_token::LAMPORTS_PER_SOL;
use solana_sdk::pubkey::Pubkey;
use solana_sdk::signature::{read_keypair_file, Keypair, Signer};
use solana_sdk::system_instruction;
use solana_sdk::transaction::Transaction;
use std::collections::HashMap;
use std::collections::HashSet;
use std::net::{SocketAddr, UdpSocket};
use std::rc::Rc;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::{Duration, SystemTime};

// Perform up to N concurrent transactions

// The implementation will cycle between a minimum number of open accounts and a maximum number:
// As long as it is below the minimum number, it will have a greater chance to create accounts than to
// delete.  It will continue in this mode until it hits the maximum number.  Then it will switch and start
// deleting more accounts than it creates, until it hits the minimum number, and then it will start creating
// more.  In this way it will cycle between its minimum and maximum number of accounts.

// These values are tuned from actual compute units costs of commands run by the hammer on-chain program.  They
// should be as close as possible to the actual compute unit cost, and should err on the side of over-estimating
// costs if necessary.  These are hardcoded from observed values and could be made overridable by command line
// parameters if that ends up being useful.
const SMALL_TX_MAX_COMPUTE_UNITS : u32 = 40_000;
const MEDIUM_TX_MAX_COMPUTE_UNITS : u32 = 1_100_000;
const LARGE_TX_MAX_COMPUTE_UNITS : u32 = 1_400_000;
const MAX_INSTRUCTION_COMPUTE_UNITS : u32 = 1_400_000;
const FAIL_COMMAND_COST : u32 = 1000;
const CPU_COMMAND_COST_PER_ITERATION : u32 = 5000;
const ALLOC_COMMAND_COST : u32 = 100;
const FREE_COMMAND_COST : u32 = 100;
const SYSVAR_COMMAND_COST : u32 = 500;

// These govern the shape of transactions; these could be made into command line parameters if that was useful
const FAIL_COMMAND_CHANCE : f32 = 0.001;
const MAX_ALLOC_BYTES : u32 = 1000;
const RECENT_BLOCKHASH_REFRESH_INTERVAL_SECS : u64 = 10;
const CONTENTION_ACCOUNT_COUNT : u32 = 20;
const LAMPORTS_PER_TRANSFER : u64 = LAMPORTS_PER_SOL / 5;

struct RpcWrapper
{
    rpc_client : RpcClient
}

struct RecentBlockhashFetcher
{
    recent_blockhash : Arc<Mutex<Option<Hash>>>
}

impl RecentBlockhashFetcher
{
    pub fn new(rpc_clients : &Arc<Mutex<RpcClients>>) -> Self
    {
        let recent_blockhash : Arc<Mutex<Option<Hash>>> = Arc::new(Mutex::new(None));

        {
            let rpc_clients = rpc_clients.clone();
            let recent_blockhash = recent_blockhash.clone();

            std::thread::spawn(move || {
                loop {
                    let rpc_client = { rpc_clients.lock().unwrap().get() };
                    match rpc_client.get_latest_blockhash() {
                        Ok(next_recent_blockhash) => {
                            // Hacky, but it's really hard to coordinate recent blockhash across rpc servers.  So just
                            // don't present a block hash until 5 seconds after it is fetched
                            std::thread::sleep(Duration::from_secs(5));
                            *(recent_blockhash.lock().unwrap()) = Some(next_recent_blockhash);
                            // Wait 30 seconds to fetch again
                            std::thread::sleep(Duration::from_secs(RECENT_BLOCKHASH_REFRESH_INTERVAL_SECS - 5));
                        },
                        Err(err) => {
                            eprintln!("Failed to get recent blockhash: {}", err);
                            std::thread::sleep(Duration::from_millis(500));
                        }
                    }
                }
            });
        }

        Self { recent_blockhash }
    }

    pub fn get(&mut self) -> Hash
    {
        loop {
            {
                let rb = self.recent_blockhash.lock().unwrap();

                if let Some(rb) = *rb {
                    return rb.clone();
                }
            }

            std::thread::sleep(Duration::from_millis(100));
        }
    }
}

struct SlotFetcher
{
    // (fetch_time, slot_at_fetch_time)
    slots : Arc<Mutex<(SystemTime, u64)>>
}

impl SlotFetcher
{
    pub fn new(rpc_clients : &Arc<Mutex<RpcClients>>) -> Self
    {
        let slots = Arc::new(Mutex::new((SystemTime::now(), 0)));

        {
            let rpc_clients = rpc_clients.clone();
            let slots = slots.clone();
            let updater = Self { slots };
            let rpc_client = { rpc_clients.lock().unwrap().get() };
            updater.update(&rpc_client);

            std::thread::spawn(move || {
                loop {
                    // Sleep 10 seconds before fetching again
                    std::thread::sleep(Duration::from_secs(10));
                    let rpc_client = { rpc_clients.lock().unwrap().get() };
                    updater.update(&rpc_client);
                }
            });
        }

        Self { slots }
    }

    // returns (fetch_time, slot_at_fetch_time)
    pub fn get(&self) -> (SystemTime, u64)
    {
        self.slots.lock().unwrap().clone()
    }

    pub fn update(
        &self,
        rpc_client : &RpcClient
    )
    {
        loop {
            let before = SystemTime::now();
            match rpc_client.get_epoch_info() {
                Ok(epoch_info) => {
                    // Pick a timestamp halfway between when the epoch info started being fetched, and the result
                    // came back, to try to get closer to the actual time that the epoch info was gathered
                    let elapsed = before.elapsed().unwrap_or(Duration::from_millis(0)).div_f32(2_f32);
                    *(self.slots.lock().unwrap()) =
                        (before.checked_add(elapsed).unwrap_or(before), epoch_info.absolute_slot);
                    break;
                },
                Err(err) => {
                    eprintln!("Failed to fetch epoch info: {}", err);
                }
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }
}

struct LeadersFetcher
{
    // (first_slot, leaders_starting_at_first_slot)
    leaders : Arc<Mutex<(u64, Vec<String>)>>
}

impl LeadersFetcher
{
    pub fn new(rpc_clients : &Arc<Mutex<RpcClients>>) -> Self
    {
        let leaders = Arc::new(Mutex::new((0_u64, Vec::<String>::new())));

        {
            let rpc_clients = rpc_clients.clone();
            let leaders = leaders.clone();
            let updater = Self { leaders };
            let rpc_client = { rpc_clients.lock().unwrap().get() };
            updater.update(&rpc_client);

            std::thread::spawn(move || {
                loop {
                    // Sleep 15 minutes before fetching again
                    std::thread::sleep(Duration::from_secs(60 * 60));
                    let rpc_client = { rpc_clients.lock().unwrap().get() };
                    updater.update(&rpc_client);
                }
            });
        }

        Self { leaders }
    }

    // returns (slot of first leader, leader pubkey strings starting at that slot)
    pub fn get(
        &self,
        target_slot : u64
    ) -> Option<String>
    {
        let leaders = self.leaders.lock().unwrap();

        let slot = leaders.0;
        let leaders = &leaders.1;

        if (target_slot < slot) || (target_slot >= (slot + (leaders.len() as u64))) {
            None
        }
        else {
            Some(leaders[(target_slot - slot) as usize].clone())
        }
    }

    pub fn update(
        &self,
        rpc_client : &RpcClient
    )
    {
        loop {
            match rpc_client.get_slot() {
                Ok(slot) => match rpc_client.get_slot_leaders(slot, 4000) {
                    Ok(new_leaders) => {
                        *(self.leaders.lock().unwrap()) =
                            (slot, new_leaders.into_iter().map(|p| format!("{}", p)).collect());
                        println!("AAA");
                        break;
                    },
                    Err(err) => eprintln!("Failed to fetch slot leaders: {}", err)
                },
                Err(err) => eprintln!("Failed to fetch slot: {}", err)
            }

            std::thread::sleep(Duration::from_millis(500));
        }
    }
}

struct TpuFetcher
{
    // Hash from pubkey (as string) to tvu port address
    validators : Arc<Mutex<HashMap<String, SocketAddr>>>
}

impl TpuFetcher
{
    // Loads from a file that a separate program writes, because using gossip at the same time as RPC causes no end of
    // problems.  So let some other machine fetch tpu info about the cluster and write it out periodically.  This
    // implementation will re-load the tpu file once every 5 minutes.
    pub fn new(tpu_file : &String) -> Self
    {
        let validators = Arc::new(Mutex::new(Self::load_validators(tpu_file)));

        {
            let validators = validators.clone();
            let tpu_file = tpu_file.clone();

            std::thread::spawn(move || loop {
                // Wait 5 minutes
                std::thread::sleep(Duration::from_secs(60 * 5));

                *(validators.lock().unwrap()) = Self::load_validators(&tpu_file);
            });
        }

        Self { validators }
    }

    // key is pubkey string
    pub fn get(
        &self,
        key : &String
    ) -> Option<SocketAddr>
    {
        self.validators.lock().unwrap().get(key).map(|tpu| tpu.clone())
    }

    fn load_validators(from : &str) -> HashMap<String, SocketAddr>
    {
        match std::fs::read(from) {
            Ok(data) => bincode::deserialize(data.as_slice()).unwrap_or_else(|err| {
                eprintln!("Failed to deserialize {}: {}", from, err);
                HashMap::<String, SocketAddr>::new()
            }),
            Err(err) => {
                eprintln!("Failed to read {}: {}", from, err);
                HashMap::<String, SocketAddr>::new()
            }
        }
    }
}

#[derive(Clone)]
struct CurrentTpus
{
    current_tpus : Arc<Mutex<(SocketAddr, SocketAddr, SocketAddr)>>
}

impl CurrentTpus
{
    pub fn new(
        tpu_file : &String,
        rpc_clients : &Arc<Mutex<RpcClients>>
    ) -> Self
    {
        println!("Creating TPU fetcher");

        let tpu_fetcher = TpuFetcher::new(tpu_file);

        println!("Creating leaders fetcher");

        let leaders_fetcher = LeadersFetcher::new(&rpc_clients);

        println!("Creating slot fetcher");

        let slot_fetcher = SlotFetcher::new(&rpc_clients);

        println!("Done creating current TPU fetcher");

        let current_tpus = loop {
            let rpc_client = { rpc_clients.lock().unwrap().get() };
            if let Some(tpus) = Self::update(&rpc_client, &slot_fetcher, &leaders_fetcher, &tpu_fetcher) {
                break tpus;
            }
            else {
                std::thread::sleep(Duration::from_millis(500));
            }
            println!("Looping");
        };
        println!("Done fetching TPUs");

        let current_tpus = Arc::new(Mutex::new(current_tpus));

        // Start a thread to keep current_tpu up-to-date
        {
            let rpc_clients = rpc_clients.clone();
            let current_tpus = current_tpus.clone();

            std::thread::spawn(move || {
                loop {
                    // Wait 500 ms to re-update leader TPU
                    std::thread::sleep(Duration::from_millis(500));
                    let rpc_client = { rpc_clients.lock().unwrap().get() };
                    if let Some(tpus) = Self::update(&rpc_client, &slot_fetcher, &leaders_fetcher, &tpu_fetcher) {
                        *(current_tpus.lock().unwrap()) = tpus;
                    }
                }
            });
        }

        Self { current_tpus }
    }

    // Returns (prev, current, next) leader TPU socket address
    pub fn get(&self) -> (SocketAddr, SocketAddr, SocketAddr)
    {
        self.current_tpus.lock().unwrap().clone()
    }

    fn update(
        rpc_client : &RpcClient,
        slot_fetcher : &SlotFetcher,
        leaders_fetcher : &LeadersFetcher,
        tpu_fetcher : &TpuFetcher
    ) -> Option<(SocketAddr, SocketAddr, SocketAddr)>
    {
        // Update tpus

        // Get the current slot and the timestamp of when it was fetched
        let (slot_at, slot) = slot_fetcher.get();

        // Estimate the slot based on current_slot, current_slot_at, and elapsed time (500 ms per slot)
        let slot =
            slot + (SystemTime::now().duration_since(slot_at).map(|d| d.as_millis() as u64).unwrap_or(u64::MAX) / 500);

        // Get the upcoming slot leaders
        let prev = leaders_fetcher.get(slot - 4);
        let current = leaders_fetcher.get(slot);
        let next = leaders_fetcher.get(slot + 4);

        //println!("prev = {}, current = {}, next = {}", prev.is_some(), current.is_some(), next.is_some());
        if prev.is_some() || current.is_some() || next.is_some() {
            let prev = prev.map_or(None, |prev| tpu_fetcher.get(&prev));
            let current = current.map_or(None, |current| tpu_fetcher.get(&current));
            let next = next.map_or(None, |next| tpu_fetcher.get(&next));
            println!("prev = {}, current = {}, next = {}", prev.is_some(), current.is_some(), next.is_some());
            if prev.is_none() {
                if current.is_none() {
                    if next.is_none() {
                        None
                    }
                    else {
                        Some((next.unwrap().clone(), next.unwrap().clone(), next.unwrap()))
                    }
                }
                else if next.is_none() {
                    Some((current.unwrap().clone(), current.unwrap().clone(), current.unwrap()))
                }
                else {
                    Some((current.unwrap().clone(), current.unwrap(), next.unwrap()))
                }
            }
            else if current.is_none() {
                if next.is_none() {
                    Some((prev.unwrap().clone(), prev.unwrap().clone(), prev.unwrap()))
                }
                else {
                    Some((prev.unwrap().clone(), prev.unwrap().clone(), next.unwrap()))
                }
            }
            else if next.is_none() {
                Some((prev.unwrap(), current.unwrap().clone(), current.unwrap()))
            }
            else {
                Some((prev.unwrap(), current.unwrap(), next.unwrap()))
            }
        }
        else {
            // The slot that we believe is the current leader slot is not bounded by the leader slots
            // that we know about.  So re-fetch the current slots and also the upcoming leader slots,
            // and try again.
            println!("BLA");
            slot_fetcher.update(&rpc_client);
            leaders_fetcher.update(&rpc_client);
            None
        }
    }
}

struct Args
{
    pub keys_dir : String,

    // file to read tpu info from
    pub tpu_file : String,

    // File to stop iterations, when it exists (defaults to "stop")
    pub stop_file : String,

    // Explicitly named RPC servers
    pub rpc_servers : Vec<String>,

    // Source of fee payer funds.  An external process must keep the balance up-to-date in this account.
    // The clients will pull SOL from this account as needed.  It should have enough SOL to support all of
    // the concurrent connections.
    pub funds_source : Keypair,

    // The set of program ids that are all duplicates of the test program.  Multiple can be used to better simulate
    // multiple programs running.  The first one must have at least [contention_account_count] accounts to use for
    // contention purposes.
    pub program_ids : Vec<Pubkey>,

    // Number of accounts to use for transaction contention purposes; these accounts must exist and be PDAs of
    // the first program_id account.  If not provided on command line, defaults to min(num_threads / 2, 1).
    pub contention_accounts_count : u32,

    // Total number of transactions to run before the "cleanup" step of deleting all accounts that were created.  If
    // None, will run indefinitely.  If running indefinitely, it would be the responsibility of an external program to
    // delete old accounts after this program has exited.
    pub total_transactions : Option<u64>,

    // Total number of threads to run at once, which implies a total number of concurrent transactions to run
    // at one time since each transaction is handled in a blocking manner.  Each thread will use its own
    // fee payer for maximum concurrency
    pub num_threads : u32
}

#[derive(Clone)]
struct Account
{
    pub address : Pubkey,

    seed : Vec<u8>,

    pub size : u16
}

// This structure keeps all state associated with transactions executed by this instance
struct Accounts
{
    pub map : HashMap<String, Arc<Account>>
}

impl Accounts
{
    pub fn new() -> Self
    {
        Self { map : HashMap::<String, Arc<Account>>::new() }
    }

    pub fn count(&self) -> usize
    {
        return self.map.len();
    }

    pub fn add_new_account(
        &mut self,
        account : Account
    )
    {
        self.add_existing_account(Arc::new(account));
    }

    pub fn add_existing_account(
        &mut self,
        rc : Arc<Account>
    )
    {
        let key = format!("{}", rc.address);

        self.map.insert(key, rc.clone());
    }

    pub fn get_random_account(
        &mut self,
        rng : &mut rand::rngs::ThreadRng
    ) -> Option<Arc<Account>>
    {
        self.random_key(rng).map(|k| self.map.get(&k).unwrap().clone())
    }

    pub fn take_random_account(
        &mut self,
        rng : &mut rand::rngs::ThreadRng
    ) -> Option<Arc<Account>>
    {
        self.random_key(rng).map(|k| self.map.remove(&k).unwrap())
    }

    fn random_key(
        &self,
        rng : &mut rand::rngs::ThreadRng
    ) -> Option<String>
    {
        let len = self.map.len();

        if len == 0 {
            None
        }
        else {
            Some(self.map.keys().nth((rng.next_u32() as usize) % len).unwrap().clone())
        }
    }
}

struct State
{
    pub accounts : Arc<Mutex<Accounts>>
}

struct CommandAccount
{
    pub address : Pubkey,

    pub is_write : bool,

    pub is_signer : bool
}

struct CommandAccounts
{
    accounts : HashMap<String, CommandAccount>
}

impl CommandAccounts
{
    pub fn new() -> Self
    {
        Self { accounts : HashMap::<String, CommandAccount>::new() }
    }

    pub fn count(&self) -> usize
    {
        self.accounts.len()
    }

    pub fn add_command_accounts(
        &mut self,
        other : &CommandAccounts
    )
    {
        for (key, command_account) in &other.accounts {
            self.add(&command_account.address, command_account.is_write, command_account.is_signer);
        }
    }

    pub fn add(
        &mut self,
        pubkey : &Pubkey,
        is_write : bool,
        is_signer : bool
    )
    {
        let key = pubkey_to_string(pubkey);

        if let Some(existing) = self.accounts.get_mut(&key) {
            if is_write {
                existing.is_write = true;
            }
            if is_signer {
                existing.is_signer = true;
            }
        }
        else {
            self.accounts.insert(key, CommandAccount { address : pubkey.clone(), is_write, is_signer });
        }
    }

    pub fn get_account_metas(&self) -> Vec<AccountMeta>
    {
        self.accounts
            .values()
            .map(|a| {
                if a.is_write {
                    AccountMeta::new(a.address.clone(), a.is_signer)
                }
                else {
                    AccountMeta::new_readonly(a.address.clone(), a.is_signer)
                }
            })
            .collect()
    }
}

struct RpcClients
{
    clients : Vec<Rc<RpcClient>>,

    last_index : usize
}

unsafe impl Send for RpcClients
{
}

impl RpcClients
{
    pub fn new(rpc_servers : Vec<String>) -> Self
    {
        Self {
            clients : rpc_servers
                .iter()
                .map(|s| Rc::new(RpcClient::new_with_commitment(s, CommitmentConfig::confirmed())))
                .collect(),
            last_index : 0
        }
    }

    // Obtains the next client that uses confirmed committment, in a round-robin fashion
    pub fn get(&mut self) -> Rc<RpcClient>
    {
        self.last_index = (self.last_index + 1) % self.clients.len();
        self.clients[self.last_index].clone()
    }
}

fn make_pubkey(s : &str) -> Result<Pubkey, String>
{
    let mut bytes = [0_u8; 32];

    match bs58::decode(s).into_vec().map_err(|e| format!("{}", e)) {
        Ok(v) => {
            if v.len() == 32 {
                bytes.copy_from_slice(v.as_slice());
                return Ok(Pubkey::new(&bytes));
            }
        },
        Err(_) => ()
    }

    // Couldn't decode it as base58, try reading it as a file
    let key = std::fs::read_to_string(s).map_err(|e| format!("Failed to read key file '{}': {}", s, e))?;

    let mut v : Vec<&str> = key.split(",").into_iter().collect();

    if v.len() < 2 {
        return Err("Short key file".to_string());
    }

    v[0] = &v[0][1..];
    let last = v.last().unwrap().clone();
    v.pop();
    v.push(&last[..(last.len() - 1)]);

    let v : Vec<u8> = v.into_iter().map(|s| u8::from_str_radix(s, 10).unwrap()).collect();

    let dalek_keypair = ed25519_dalek::Keypair::from_bytes(v.as_slice())
        .map_err(|e| format!("Invalid key file '{}' contents: {}", s, e))?;
    Ok(Pubkey::new(&dalek_keypair.public.to_bytes()))
}

fn locked_println(
    lock : &Arc<Mutex<()>>,
    msg : String
)
{
    let _ = lock.lock();

    println!("{}", msg);
}

fn pubkey_to_string(pubkey : &Pubkey) -> String
{
    format!("{}", pubkey)
}

fn parse_args() -> Result<Args, String>
{
    let mut args = std::env::args();

    let mut keys_dir = None;

    let mut tpu_file = None;

    let mut stop_file = None;

    let mut rpc_servers = Vec::<String>::new();

    let mut funds_source = None;

    let mut program_ids = Vec::<Pubkey>::new();

    let mut contention_accounts_count = None;

    let mut total_transactions = None;

    let mut num_threads = None;

    args.nth(0);

    while let Some(arg) = args.nth(0) {
        match arg.as_str() {
            "--keys-dir" => {
                if keys_dir.is_some() {
                    return Err("Duplicate --keys-dir argument".to_string());
                }
                else {
                    keys_dir = Some(args.nth(0).ok_or("--keys-dir requires a value".to_string())?);
                }
            },

            "--tpu-file" => {
                if tpu_file.is_some() {
                    return Err("Duplicate --tpu-file".to_string());
                }
                let file = args.nth(0).ok_or("--tpu-file requires a value".to_string())?;
                tpu_file = Some(file);
            },

            "--stop-file" => {
                if stop_file.is_some() {
                    return Err("Duplicate --stop-file".to_string());
                }
                let file = args.nth(0).ok_or("--stop-file requires a value".to_string())?;
                stop_file = Some(file);
            },

            "--rpc-server" => {
                let rpc_server = args.nth(0).ok_or("--rpc-server requires a value".to_string())?;
                rpc_servers.push(rpc_server);
            },

            "--funds-source" => {
                if funds_source.is_some() {
                    return Err("Duplicate --funds-source argument".to_string());
                }
                let file = args.nth(0).ok_or("--funds-source requires a value".to_string())?;
                funds_source = Some(read_keypair_file(file.clone()).unwrap_or_else(|e| {
                    eprintln!("Failed to read {}", file);
                    std::process::exit(-1)
                }));
            },

            "--program-id" => {
                let program_id = make_pubkey(args.nth(0).as_ref().ok_or("--program-id requires a value".to_string())?)?;
                if program_ids.iter().find(|&e| e == &program_id).is_some() {
                    return Err(format!("Duplicate program id: {}", program_id));
                }
                program_ids.push(program_id);
            },

            "--contention-accounts-count" => {
                if contention_accounts_count.is_some() {
                    return Err("Duplicate --contention-accounts-count argument".to_string());
                }
                else {
                    contention_accounts_count = Some(
                        args.nth(0)
                            .ok_or("--contention-accounts-count requires a value".to_string())?
                            .parse::<u32>()
                            .map_err(|e| e.to_string())?
                    )
                }
            },

            "--total-transactions" => {
                if total_transactions.is_some() {
                    return Err("Duplicate --total-transactions argument".to_string());
                }
                else {
                    total_transactions = Some(
                        args.nth(0)
                            .ok_or("--total-transactions requires a value".to_string())?
                            .parse::<u64>()
                            .map_err(|e| e.to_string())?
                    )
                }
            },

            "--num-threads" => {
                if num_threads.is_some() {
                    return Err("Duplicate --num-threads argument".to_string());
                }
                else {
                    num_threads = Some(
                        args.nth(0)
                            .ok_or("--num-threads requires a value".to_string())?
                            .parse::<u32>()
                            .map_err(|e| e.to_string())?
                    )
                }
            },

            _ => return Err(format!("Invalid argument: {}", arg))
        }
    }

    let keys_dir = keys_dir.unwrap_or("./keys".to_string());

    if tpu_file.is_none() {
        return Err("--tpu-file argument is required".to_string());
    }

    let tpu_file = tpu_file.unwrap();

    if stop_file.is_none() {
        stop_file = Some("stop".to_string());
    }

    let stop_file = stop_file.unwrap();

    if funds_source.is_none() {
        return Err("--funds-source argument is required".to_string());
    }

    let funds_source = funds_source.unwrap();

    if program_ids.is_empty() {
        return Err("At least one program id specified via --program-id option is required".to_string());
    }

    let num_threads = num_threads.unwrap_or(8);

    if num_threads == 0 {
        return Err("--num-threads must takea nonzero argument".to_string());
    }

    if contention_accounts_count.is_none() {
        contention_accounts_count = Some(std::cmp::max((num_threads as u32) / 2, 1));
    }

    let contention_accounts_count = contention_accounts_count.unwrap();

    Ok(Args {
        keys_dir,
        tpu_file,
        stop_file,
        rpc_servers,
        funds_source,
        program_ids,
        contention_accounts_count,
        total_transactions,
        num_threads
    })
}

fn write(
    w : &mut dyn std::io::Write,
    b : &[u8]
)
{
    w.write(b).expect("Internal fail to write");
}

fn stop_file_exists(stop_file : &str) -> bool
{
    std::fs::metadata(stop_file).is_ok()
}

fn add_create_account_command(
    fee_payer : &Pubkey,
    account : &Account,
    command_accounts : &mut CommandAccounts,
    w : &mut dyn std::io::Write
)
{
    command_accounts.add(fee_payer, true, true);
    command_accounts.add(&account.address, true, false);
    command_accounts.add(&Pubkey::new(&[0_u8; 32]), false, false);

    write(w, &[0_u8]);

    write(w, &fee_payer.to_bytes());

    write(w, &account.address.to_bytes());

    write(w, &account.seed);

    write(w, &account.size.to_le_bytes())
}

fn add_delete_account_command(
    fee_payer : &Pubkey,
    account : &Account,
    command_accounts : &mut CommandAccounts,
    w : &mut dyn std::io::Write
)
{
    command_accounts.add(fee_payer, true, true);
    command_accounts.add(&account.address, true, false);
    command_accounts.add(&Pubkey::new(&[0_u8; 32]), false, false);

    write(w, &[1_u8]);

    write(w, &fee_payer.to_bytes());
    write(w, &account.address.to_bytes());
}

fn add_cpu_command(
    loop_count : u32,
    _command_accounts : &mut CommandAccounts,
    w : &mut dyn std::io::Write
)
{
    write(w, &[2_u8]);

    write(w, &loop_count.to_le_bytes())
}

fn add_alloc_command(
    amount : u32,
    index : u8,
    _command_accounts : &mut CommandAccounts,
    w : &mut dyn std::io::Write
)
{
    write(w, &[3_u8]);

    write(w, &amount.to_le_bytes());

    write(w, &[index])
}

fn add_free_command(
    index : u8,
    _command_accounts : &mut CommandAccounts,
    w : &mut dyn std::io::Write
)
{
    write(w, &[4_u8]);

    write(w, &[index])
}

fn add_cpi_command(
    program_id : &Pubkey,
    accounts : &Vec<AccountMeta>,
    data : &Vec<u8>,
    seed : Option<Vec<u8>>,
    command_accounts : &mut CommandAccounts,
    w : &mut dyn std::io::Write
)
{
    command_accounts.add(program_id, false, false);

    write(w, &[5_u8]);

    write(w, &program_id.to_bytes());

    write(w, &[accounts.len() as u8]);

    for account in accounts {
        write(w, &account.pubkey.to_bytes());

        write(w, &[if account.is_writable { 1_u8 } else { 0_u8 }]);

        write(w, &[if account.is_signer { 1_u8 } else { 0_u8 }]);

        command_accounts.add(&account.pubkey, account.is_writable, account.is_signer);
    }

    write(w, &(data.len() as u16).to_le_bytes());

    write(w, &data.as_slice());

    if let Some(seed) = seed {
        write(w, &[seed.len() as u8]);
        write(w, &seed.as_slice())
    }
    else {
        write(w, &[0_u8])
    }
}

fn add_sysvar_command(
    _command_accounts : &mut CommandAccounts,
    w : &mut dyn std::io::Write
)
{
    write(w, &[6_u8])
}

fn add_fail_command(
    error_code : u8,
    _command_accounts : &mut CommandAccounts,
    w : &mut dyn std::io::Write
)
{
    write(w, &[7_u8]);

    write(w, &[error_code])
}

fn random_chance(
    rng : &mut rand::rngs::ThreadRng,
    pct : f32
) -> bool
{
    ((rng.next_u32() as f64) / (u32::MAX as f64)) < (pct as f64)
}

// Turns an Instruction into a CPI of the same instruction, calling into some other pubkey
fn make_cpi(
    rng : &mut rand::rngs::ThreadRng,
    instruction : &Instruction,
    program_ids : &Vec<Pubkey>
) -> Instruction
{
    let new_program_id = program_ids[(rng.next_u32() % (program_ids.len() as u32)) as usize];

    let mut new_data = vec![];

    let mut command_accounts = CommandAccounts::new();

    add_cpi_command(
        &new_program_id,
        &instruction.accounts,
        &instruction.data,
        None,
        &mut command_accounts,
        &mut new_data
    );

    Instruction::new_with_bytes(new_program_id, new_data.as_slice(), command_accounts.get_account_metas())
}

// Create an instruction that creates an account, deletes an account, or does some amount of other random stuff.
// Returns:
// (data_buffer,          // buffer of command data
//  actual_create_count,  // number of accounts created
//  actual_delete_count,  // number of accounts deleted
//  actual_compute_usage) // compute units used
fn make_command(
    rng : &mut rand::rngs::ThreadRng,
    fee_payer : &Pubkey,
    accounts : &mut Arc<Mutex<Accounts>>,
    allocated_indices : &mut HashSet<u8>,
    compute_budget : u32,
    command_accounts : &mut CommandAccounts
) -> (Vec<u8>, u32)
{
    let mut data = vec![];

    // Chance of straight up fail
    if random_chance(rng, FAIL_COMMAND_CHANCE) {
        add_fail_command(((rng.next_u32() % 255) + 1) as u8, command_accounts, &mut data);
        (data, FAIL_COMMAND_COST)
    }
    else {
        let mut v = vec![];
        if compute_budget >= CPU_COMMAND_COST_PER_ITERATION {
            v.push(0); // cpu
        }
        if (compute_budget >= ALLOC_COMMAND_COST) && (allocated_indices.len() < 256) {
            v.push(1); // alloc
        }
        if (compute_budget >= FREE_COMMAND_COST) && (allocated_indices.len() > 0) {
            v.push(2); // free
        }
        if compute_budget >= SYSVAR_COMMAND_COST {
            v.push(4); // sysvar
        }
        if v.len() == 0 {
            // No command can fit, so do nothing but use all compute budget
            return (data, compute_budget);
        }
        match v[(rng.next_u32() as usize) % v.len()] {
            0 => {
                // cpu
                let mut max_iterations = compute_budget / (2 * CPU_COMMAND_COST_PER_ITERATION);
                if max_iterations == 0 {
                    max_iterations = 1;
                }
                // Pick some number between 1 and half of max_iterations
                let mut iterations = (rng.next_u32() % max_iterations) + 1;
                // 50% of the time, iterate half of max_iterations PLUS that number
                if (rng.next_u32() % 2) == 0 {
                    iterations = max_iterations + iterations;
                }
                // 50% of the time, iterate half of max_iterations MINUS that number
                else {
                    iterations = max_iterations - iterations;
                }
                add_cpu_command(iterations, command_accounts, &mut data);
                (data, iterations * CPU_COMMAND_COST_PER_ITERATION)
            },
            1 => {
                // alloc
                // Find first unallocated index
                let mut index = 0;
                loop {
                    if !allocated_indices.contains(&index) {
                        break;
                    }
                    index += 1;
                }
                add_alloc_command((rng.next_u32() % MAX_ALLOC_BYTES) + 1, index, command_accounts, &mut data);
                allocated_indices.insert(index);
                (data, ALLOC_COMMAND_COST)
            },
            2 => {
                // free
                let index = *allocated_indices.iter().nth((rng.next_u32() as usize) % allocated_indices.len()).unwrap();
                add_free_command(index, command_accounts, &mut data);
                allocated_indices.remove(&index);
                (data, FREE_COMMAND_COST)
            },
            _ => {
                // sysvar
                add_sysvar_command(command_accounts, &mut data);
                (data, SYSVAR_COMMAND_COST)
            }
        }
    }
}

fn transaction_thread_function(
    thread_number : u32,
    print_lock : Arc<Mutex<()>>,
    rpc_clients : Arc<Mutex<RpcClients>>,
    recent_blockhash_fetcher : Arc<Mutex<RecentBlockhashFetcher>>,
    program_ids : Vec<Pubkey>,
    funds_source : Keypair,
    mut accounts : Arc<Mutex<Accounts>>,
    current_tpus : CurrentTpus,
    total_transactions : Arc<Mutex<Option<u64>>>,
    stop_file : &str
)
{
    // Make a fee payer for this thread
    let fee_payer = Keypair::new();

    let fee_payer_pubkey = fee_payer.pubkey();

    //let fee_payer = Keypair::from_bytes(&funds_source.to_bytes()).expect("");

    //let fee_payer_pubkey = fee_payer.pubkey();

    // Make a random number generator for this thread
    let mut rng = rand::thread_rng();

    let mut iterations = 0;

    loop {
        // When the stop file exists, stop the loop
        if stop_file_exists(stop_file) {
            break;
        }
        // Make sure there are still transactions to complete before doing balance transfer
        {
            let total_transactions = { *(total_transactions.lock().unwrap()) };
            if let Some(total_transactions) = total_transactions {
                if total_transactions == 0 {
                    break;
                }
            }
        }

        let rpc_client = { rpc_clients.lock().unwrap().get() };

        let mut recent_blockhash = recent_blockhash_fetcher.lock().unwrap().get();

        // Only check balance once every 1,000 iterations
        if (iterations % 1000) == 0 {
            let total_transactions = { *(total_transactions.lock().unwrap()) };
            if let Some(total_transactions) = total_transactions {
                println!("Thread {}: iteration {} ({} remaining)", thread_number, iterations, total_transactions);
            }
            else {
                println!("Thread {}: iteration {}", thread_number, iterations);
            }
            // When balance falls below 1 SOL, take 1 SOL from funds source
            loop {
                if rpc_client.get_balance(&fee_payer_pubkey).unwrap_or(0) < (LAMPORTS_PER_TRANSFER / 2) {
                    transfer_lamports(
                        &rpc_client,
                        &funds_source,
                        &funds_source,
                        &fee_payer_pubkey,
                        LAMPORTS_PER_TRANSFER,
                        &recent_blockhash
                    );
                    // If the balance is still too low, continue the loop to try again
                    println!("TRANSFERED = {}", LAMPORTS_PER_TRANSFER);
                    if rpc_client.get_balance(&fee_payer_pubkey).unwrap_or(0) < LAMPORTS_PER_TRANSFER {
                        println!("TRANSFERED FAILED");
                        // Wait until the recent blockhash has changed so as not to repeat the request
                        loop {
                            std::thread::sleep(Duration::from_millis(250));
                            let new_recent_blockhash = recent_blockhash_fetcher.lock().unwrap().get();
                            if new_recent_blockhash == recent_blockhash {
                                continue;
                            }
                            recent_blockhash = new_recent_blockhash;
                            break;
                        }
                        continue;
                    }
                }
                break;
            }
        }

        iterations += 1;

        {
            let mut total_transactions = total_transactions.lock().unwrap();
            if let Some(total_transactions_value) = *total_transactions {
                if total_transactions_value == 0 {
                    break;
                }
                *total_transactions = Some(total_transactions_value - 1)
            }
        }

        let program_id = &program_ids[(rng.next_u32() as usize) % program_ids.len()];

        let mut allocated_indices = HashSet::<u8>::new();

        // 1/3 chance of "small" transaction, 1/3 chance of "medium" transaction, 1/3 chance of "large" transaction
        let (min, max) = match rng.next_u32() % 3 {
            0 => (0, SMALL_TX_MAX_COMPUTE_UNITS),
            1 => (SMALL_TX_MAX_COMPUTE_UNITS, MEDIUM_TX_MAX_COMPUTE_UNITS),
            _ => (MEDIUM_TX_MAX_COMPUTE_UNITS, LARGE_TX_MAX_COMPUTE_UNITS)
        };
        let mut compute_budget = (rng.next_u32() % (max - min)) + min + CPU_COMMAND_COST_PER_ITERATION;

        // Vector to hold data for each individual command:
        // (data, command_accounts, compute_usage)
        let mut command_data = vec![];

        let mut total_data_size = 0;

        // Create the commands one by one
        while (total_data_size < 1100) && (compute_budget > 0) {
            let mut command_accounts = CommandAccounts::new();
            let (data, actual_compute_usage) = make_command(
                &mut rng,
                &fee_payer_pubkey,
                &mut accounts,
                &mut allocated_indices,
                compute_budget,
                &mut command_accounts
            );
            total_data_size += data.len();
            command_data.push((data, command_accounts, actual_compute_usage));
            compute_budget -= actual_compute_usage;
        }

        // Group commands together into instructions
        let mut instructions = vec![];

        while command_data.len() > 0 {
            let mut instruction_compute_units = 0;
            let mut command_count = (rng.next_u32() % (command_data.len() as u32)) + 1;
            let mut data = vec![];
            let mut instruction_accounts = CommandAccounts::new();
            while command_count > 0 {
                let (command_data, command_accounts, compute_units) = command_data.remove(0);
                instruction_compute_units += compute_units;
                if instruction_compute_units > MAX_INSTRUCTION_COMPUTE_UNITS {
                    break;
                }
                data.extend(command_data);
                instruction_accounts.add_command_accounts(&command_accounts);
                command_count -= 1;
            }
            // Some commands may be empty, so it's possible for the command data to be empty
            if data.len() > 0 {
                // Add some random accounts to the instruction to create account contention, up to 12 accounts
                if instruction_accounts.count() < 12 {
                    let total_accounts = rng.next_u32() % ((12 - instruction_accounts.count()) as u32);
                    if total_accounts > 0 {
                        let read_only_accounts = rng.next_u32() % total_accounts;
                        let read_write_accounts = total_accounts - read_only_accounts;
                        for _ in 0..read_only_accounts {
                            if let Some(account) = accounts.lock().unwrap().get_random_account(&mut rng) {
                                instruction_accounts.add(&account.address, false, false);
                            }
                        }
                        for _ in 0..read_write_accounts {
                            if let Some(account) = accounts.lock().unwrap().get_random_account(&mut rng) {
                                instruction_accounts.add(&account.address, true, false);
                            }
                        }
                    }
                }
                instructions.push(Instruction::new_with_bytes(
                    program_id.clone(),
                    data.as_slice(),
                    instruction_accounts.get_account_metas()
                ));
            }
        }

        // Now turn some subset of commands into cross-program invokes, if there is more than one program
        if (program_ids.len() > 1) && (instructions.len() > 0) {
            loop {
                // 75% chance of not doing CPI this loop
                if random_chance(&mut rng, 0.75) {
                    break;
                }

                // Pick an instruction to turn into a CPI
                let index = (rng.next_u32() % (instructions.len() as u32)) as usize;

                instructions[index] = make_cpi(&mut rng, &instructions[index], &program_ids);
            }
        }

        // Execute the transaction

        // Refresh recent blockhash
        let recent_blockhash = recent_blockhash_fetcher.lock().unwrap().get();

        let transaction =
            Transaction::new(&vec![&fee_payer], Message::new(&instructions, Some(&fee_payer_pubkey)), recent_blockhash);

        let tx_bytes = bincode::serialize(&transaction).expect("encode");

        let current_tpus = current_tpus.get();

        //        locked_println(
        //            &print_lock,
        //            format!(
        //                "Thread {}: Submitting transaction to {}\n  Signature: {}",
        //                thread_number,
        //                //base64::encode(&tx_bytes),
        //                current_tpu,
        //                transaction.signatures[0]
        //            )
        //        );

        // Send to prev, current, and next leader
        // Actually just send to 'current'.  Sending to others doesn't seem to improve the rate at which
        // transactions land.
        //let _ = UdpSocket::bind("0.0.0.0:0").unwrap().send_to(tx_bytes.as_slice(), current_tpus.0);
        //let _ = UdpSocket::bind("0.0.0.0:0").unwrap().send_to(tx_bytes.as_slice(), current_tpus.1);
        //let _ = UdpSocket::bind("0.0.0.0:0").unwrap().send_to(tx_bytes.as_slice(), current_tpus.2);
        
        let rpc_client = { rpc_clients.lock().unwrap().get() };
        let res = rpc_client.send_transaction(&transaction);
        println!("RESULT = {:?}", res);
    }

    // Take back all SOL from the fee payer
    let rpc_client = { rpc_clients.lock().unwrap().get() };

    loop {
        if let Some(balance) = rpc_client.get_balance(&fee_payer_pubkey).ok() {
            if balance == 0 {
                break;
            }
            if funds_source.pubkey() == fee_payer.pubkey() {
                break;
            }
            match rpc_client.get_latest_blockhash() {
                Ok(recent_blockhash) => {
                    transfer_lamports(
                        &rpc_client,
                        &funds_source,
                        &fee_payer,
                        &funds_source.pubkey(),
                        balance,
                        &recent_blockhash
                    );
                },
                Err(_) => ()
            }
        }
    }
}

fn main()
{
    let args = parse_args().unwrap_or_else(|e| {
        eprintln!("{}", e);
        std::process::exit(-1)
    });

    if stop_file_exists(args.stop_file.as_str()) {
        eprintln!("ERROR: stop file \"{}\" exists", args.stop_file);
        std::process::exit(-1)
    }

    let rng = rand::thread_rng();

    let rpc_clients = Arc::new(Mutex::new(RpcClients::new(args.rpc_servers.clone())));

    let current_tpus = CurrentTpus::new(&args.tpu_file, &rpc_clients);

    let program_ids = args.program_ids;

    // Recent blockhash is updated every 30 seconds
    let recent_blockhash_fetcher = Arc::new(Mutex::new(RecentBlockhashFetcher::new(&rpc_clients)));

    let accounts = Arc::new(Mutex::new(Accounts::new()));

    let funds_source = Keypair::from_bytes(&args.funds_source.to_bytes()).unwrap();

    // Turns out that it's just a pain to manage the creation and deletion of accounts, so instead just going to have
    // the accounts created by some other process and passed in as command line arguments

    // Create accounts, so that there is some chance of account contention
    //    let mut accounts_to_create = CONTENTION_ACCOUNT_COUNT;
    //    if accounts_to_create > (args.num_threads * 4) {
    //        accounts_to_create = args.num_threads * 4;
    //    }
    //    while accounts_to_create > 0 {
    //        println!("({} accounts remain to be created)", accounts_to_create);
    //        let rpc_client = { rpc_clients.lock().unwrap().get() };
    //        let mut create_count = accounts_to_create;
    //        if create_count > 8 {
    //            create_count = 8;
    //        }
    //        let mut command_accounts = CommandAccounts::new();
    //        let mut data = vec![];
    //        for _ in 0..create_count {
    //            // Generate a seed to create from
    //            let seed = &rng.next_u64().to_le_bytes()[0..7];
    //            let (address, bump_seed) = Pubkey::find_program_address(&[seed], &program_ids[0]);
    //            let mut seed = seed.to_vec();
    //            seed.push(bump_seed);
    //            let size = (rng.next_u32() % ((10 * 1024) + 1)) as u16;
    //            let account = Account { address, seed, size };
    //            add_create_account_command(&funds_source.pubkey(), &account, &mut command_accounts, &mut data);
    //            accounts.lock().unwrap().add_new_account(account);
    //        }
    //
    //        let recent_blockhash = recent_blockhash_fetcher.lock().unwrap().get();
    //
    //        let program_id = &program_ids[0];
    //
    //        let instructions = vec![Instruction::new_with_bytes(
    //            program_id.clone(),
    //            data.as_slice(),
    //            command_accounts.get_account_metas()
    //        )];
    //
    //        let transaction = Transaction::new(
    //            &vec![&funds_source],
    //            Message::new(&instructions, Some(&funds_source.pubkey())),
    //            recent_blockhash
    //        );
    //
    //        println!(
    //            "Submitting transaction {} to RPC\n  Signature: {}",
    //            base64::encode(bincode::serialize(&transaction).expect("encode")),
    //            transaction.signatures[0]
    //        );
    //
    //        match rpc_client.send_and_confirm_transaction(&transaction) {
    //            Ok(_) => accounts_to_create -= create_count,
    //            Err(e) => println!("TX failed: {}", e)
    //        }
    //    }

    // Fill the Accounts vector with the contention accounts
    let contention_accounts_program_id = &program_ids[0];
    for index in 0..args.contention_accounts_count {
        let seed = &(index as u64).to_le_bytes()[0..7];
        let (address, bump_seed) = Pubkey::find_program_address(&[seed], contention_accounts_program_id);
        let mut seed = seed.to_vec();
        seed.push(bump_seed);
        accounts.lock().unwrap().add_new_account(Account { address, seed, size : 100 });
    }

    let iterations = Arc::new(Mutex::new(args.total_transactions));

    let print_lock = Arc::new(Mutex::new(()));

    let mut threads = vec![];

    for thread_number in 0..args.num_threads {
        let print_lock = print_lock.clone();
        let rpc_clients = rpc_clients.clone();
        let recent_blockhash_fetcher = recent_blockhash_fetcher.clone();
        let program_ids = program_ids.clone();
        let funds_source = Keypair::from_bytes(&funds_source.to_bytes()).expect("");
        let accounts = accounts.clone();
        let current_tpus = current_tpus.clone();
        let iterations = iterations.clone();
        let stop_file = args.stop_file.clone();

        threads.push(std::thread::spawn(move || {
            transaction_thread_function(
                thread_number,
                print_lock,
                rpc_clients,
                recent_blockhash_fetcher,
                program_ids,
                funds_source,
                accounts,
                current_tpus,
                iterations,
                &stop_file.as_str()
            )
        }));
    }

    // Join the threads
    for j in threads {
        j.join().expect("Failed to join");
    }

    // Delete accounts that were created

    //    let mut accounts = accounts.lock().unwrap();
    //
    //    loop {
    //        if accounts.count() == 0 {
    //            break;
    //        }
    //
    //        println!("({} accounts remain to be deleted)", accounts.map.len());
    //
    //        // Delete 8 at a time
    //        let mut v = vec![];
    //
    //        for i in 0..8 {
    //            if let Some(account) = accounts.take_random_account(&mut rng) {
    //                v.push(account);
    //            }
    //            else {
    //                break;
    //            }
    //        }
    //
    //        let program_id = &program_ids[0];
    //
    //        let mut data = vec![];
    //
    //        let mut command_accounts = CommandAccounts::new();
    //
    //        for account in &v {
    //            add_delete_account_command(&funds_source.pubkey(), account, &mut command_accounts, &mut data);
    //        }
    //
    //        let instructions = vec![Instruction::new_with_bytes(
    //            program_id.clone(),
    //            data.as_slice(),
    //            command_accounts.get_account_metas()
    //        )];
    //
    //        //let rpc_client = rpc_clients.get_finalized();
    //        let rpc_client = { rpc_clients.lock().unwrap().get() };
    //
    //        let recent_blockhash = recent_blockhash_fetcher.lock().unwrap().get();
    //
    //        let transaction = Transaction::new(
    //            &vec![&funds_source],
    //            Message::new(&instructions, Some(&funds_source.pubkey())),
    //            recent_blockhash
    //        );
    //
    //        println!(
    //            "Submitting transaction {} to RPC\n  Signature: {}",
    //            base64::encode(bincode::serialize(&transaction).expect("encode")),
    //            transaction.signatures[0]
    //        );
    //
    //        match rpc_client.send_and_confirm_transaction(&transaction) {
    //            Ok(_) => (),
    //            Err(e) => {
    //                println!("TX failed: {}", e);
    //                // Failed transaction, so all accounts it tried to delete are back
    //                v.into_iter().for_each(|account| accounts.add_existing_account(account));
    //            }
    //        }
    //    }
}

fn transfer_lamports(
    rpc_client : &RpcClient,
    fee_payer : &Keypair,
    funds_source : &Keypair,
    target : &Pubkey,
    amount : u64,
    recent_blockhash : &Hash
)
{
    if *target == funds_source.pubkey() {
        return;
    }

    println!("Transferring {} from {} to {}", amount, funds_source.pubkey(), target);

    let transaction = Transaction::new(
        &vec![fee_payer, funds_source],
        Message::new(
            &vec![system_instruction::transfer(&funds_source.pubkey(), target, amount)],
            Some(&fee_payer.pubkey())
        ),
        recent_blockhash.clone()
    );

    // Ignore the result.  If take_funds fails, the check for balance will try again.
    match rpc_client.send_and_confirm_transaction(&transaction) {
        Ok(_) => (),
        Err(err) => println!("Failed transfer: {}", err)
    }
}
