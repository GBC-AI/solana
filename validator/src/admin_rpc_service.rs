use {
    jsonrpc_core::{MetaIoHandler, Metadata, Result},
    jsonrpc_core_client::{transports::ipc, RpcError},
    jsonrpc_derive::rpc,
    jsonrpc_ipc_server::{RequestContext, ServerBuilder},
    jsonrpc_server_utils::tokio,
    log::*,
    serde::{Deserialize, Serialize},
    solana_core::{
        consensus::Tower, tower_storage::TowerStorage, validator::ValidatorStartProgress,
    },
    solana_gossip::{cluster_info::ClusterInfo, contact_info::ContactInfo},
    solana_sdk::{
        exit::Exit,
        signature::{read_keypair_file, Keypair, Signer},
    },
    std::{
        fmt::{self, Display},
        net::SocketAddr,
        path::{Path, PathBuf},
        sync::{Arc, RwLock},
        thread::{self, Builder},
        time::{Duration, SystemTime},
    },
};

#[derive(Clone)]
pub struct AdminRpcRequestMetadata {
    pub rpc_addr: Option<SocketAddr>,
    pub start_time: SystemTime,
    pub start_progress: Arc<RwLock<ValidatorStartProgress>>,
    pub validator_exit: Arc<RwLock<Exit>>,
    pub authorized_voter_keypairs: Arc<RwLock<Vec<Arc<Keypair>>>>,
    pub cluster_info: Arc<RwLock<Option<Arc<ClusterInfo>>>>,
    pub tower_storage: Arc<dyn TowerStorage>,
}
impl Metadata for AdminRpcRequestMetadata {}

#[derive(Debug, Deserialize, Serialize)]
pub struct AdminRpcContactInfo {
    pub id: String,
    pub gossip: SocketAddr,
    pub tvu: SocketAddr,
    pub tvu_forwards: SocketAddr,
    pub repair: SocketAddr,
    pub tpu: SocketAddr,
    pub tpu_forwards: SocketAddr,
    pub tpu_vote: SocketAddr,
    pub rpc: SocketAddr,
    pub rpc_pubsub: SocketAddr,
    pub serve_repair: SocketAddr,
    pub last_updated_timestamp: u64,
    pub shred_version: u16,
}

impl From<ContactInfo> for AdminRpcContactInfo {
    fn from(contact_info: ContactInfo) -> Self {
        let ContactInfo {
            id,
            gossip,
            tvu,
            tvu_forwards,
            repair,
            tpu,
            tpu_forwards,
            tpu_vote,
            rpc,
            rpc_pubsub,
            serve_repair,
            wallclock,
            shred_version,
        } = contact_info;
        Self {
            id: id.to_string(),
            last_updated_timestamp: wallclock,
            gossip,
            tvu,
            tvu_forwards,
            repair,
            tpu,
            tpu_forwards,
            tpu_vote,
            rpc,
            rpc_pubsub,
            serve_repair,
            shred_version,
        }
    }
}

impl Display for AdminRpcContactInfo {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        writeln!(f, "Identity: {}", self.id)?;
        writeln!(f, "Gossip: {}", self.gossip)?;
        writeln!(f, "TVU: {}", self.tvu)?;
        writeln!(f, "TVU Forwards: {}", self.tvu_forwards)?;
        writeln!(f, "Repair: {}", self.repair)?;
        writeln!(f, "TPU: {}", self.tpu)?;
        writeln!(f, "TPU Forwards: {}", self.tpu_forwards)?;
        writeln!(f, "TPU Votes: {}", self.tpu_vote)?;
        writeln!(f, "RPC: {}", self.rpc)?;
        writeln!(f, "RPC Pubsub: {}", self.rpc_pubsub)?;
        writeln!(f, "Serve Repair: {}", self.serve_repair)?;
        writeln!(f, "Last Updated Timestamp: {}", self.last_updated_timestamp)?;
        writeln!(f, "Shred Version: {}", self.shred_version)
    }
}

#[rpc]
pub trait AdminRpc {
    type Metadata;

    #[rpc(meta, name = "exit")]
    fn exit(&self, meta: Self::Metadata) -> Result<()>;

    #[rpc(meta, name = "rpcAddress")]
    fn rpc_addr(&self, meta: Self::Metadata) -> Result<Option<SocketAddr>>;

    #[rpc(name = "setLogFilter")]
    fn set_log_filter(&self, filter: String) -> Result<()>;

    #[rpc(meta, name = "startTime")]
    fn start_time(&self, meta: Self::Metadata) -> Result<SystemTime>;

    #[rpc(meta, name = "startProgress")]
    fn start_progress(&self, meta: Self::Metadata) -> Result<ValidatorStartProgress>;

    #[rpc(meta, name = "addAuthorizedVoter")]
    fn add_authorized_voter(&self, meta: Self::Metadata, keypair_file: String) -> Result<()>;

    #[rpc(meta, name = "removeAllAuthorizedVoters")]
    fn remove_all_authorized_voters(&self, meta: Self::Metadata) -> Result<()>;

    #[rpc(meta, name = "setIdentity")]
    fn set_identity(&self, meta: Self::Metadata, keypair_file: String) -> Result<()>;

    #[rpc(meta, name = "contactInfo")]
    fn contact_info(&self, meta: Self::Metadata) -> Result<AdminRpcContactInfo>;
}

pub struct AdminRpcImpl;
impl AdminRpc for AdminRpcImpl {
    type Metadata = AdminRpcRequestMetadata;

    fn exit(&self, meta: Self::Metadata) -> Result<()> {
        debug!("exit admin rpc request received");

        thread::spawn(move || {
            // Delay exit signal until this RPC request completes, otherwise the caller of `exit` might
            // receive a confusing error as the validator shuts down before a response is sent back.
            thread::sleep(Duration::from_millis(100));

            warn!("validator exit requested");
            meta.validator_exit.write().unwrap().exit();

            // TODO: Debug why Exit doesn't always cause the validator to fully exit
            // (rocksdb background processing or some other stuck thread perhaps?).
            //
            // If the process is still alive after five seconds, exit harder
            thread::sleep(Duration::from_secs(5));
            warn!("validator exit timeout");
            std::process::exit(0);
        });
        Ok(())
    }

    fn rpc_addr(&self, meta: Self::Metadata) -> Result<Option<SocketAddr>> {
        debug!("rpc_addr admin rpc request received");
        Ok(meta.rpc_addr)
    }

    fn set_log_filter(&self, filter: String) -> Result<()> {
        debug!("set_log_filter admin rpc request received");
        solana_logger::setup_with(&filter);
        Ok(())
    }

    fn start_time(&self, meta: Self::Metadata) -> Result<SystemTime> {
        debug!("start_time admin rpc request received");
        Ok(meta.start_time)
    }

    fn start_progress(&self, meta: Self::Metadata) -> Result<ValidatorStartProgress> {
        debug!("start_progress admin rpc request received");
        Ok(*meta.start_progress.read().unwrap())
    }

    fn add_authorized_voter(&self, meta: Self::Metadata, keypair_file: String) -> Result<()> {
        debug!("add_authorized_voter request received");

        let authorized_voter = read_keypair_file(keypair_file)
            .map_err(|err| jsonrpc_core::error::Error::invalid_params(format!("{}", err)))?;

        let mut authorized_voter_keypairs = meta.authorized_voter_keypairs.write().unwrap();

        if authorized_voter_keypairs
            .iter()
            .any(|x| x.pubkey() == authorized_voter.pubkey())
        {
            Err(jsonrpc_core::error::Error::invalid_params(
                "Authorized voter already present",
            ))
        } else {
            authorized_voter_keypairs.push(Arc::new(authorized_voter));
            Ok(())
        }
    }

    fn remove_all_authorized_voters(&self, meta: Self::Metadata) -> Result<()> {
        debug!("remove_all_authorized_voters received");
        meta.authorized_voter_keypairs.write().unwrap().clear();
        Ok(())
    }

    fn set_identity(&self, meta: Self::Metadata, keypair_file: String) -> Result<()> {
        debug!("set_identity request received");

        let identity_keypair = read_keypair_file(&keypair_file).map_err(|err| {
            jsonrpc_core::error::Error::invalid_params(format!(
                "Failed to read identity keypair from {}: {}",
                keypair_file, err
            ))
        })?;

        // Ensure a Tower exists for the new identity and exit gracefully.
        // ReplayStage will be less forgiving if it fails to load the new tower.
        Tower::restore(meta.tower_storage.as_ref(), &identity_keypair.pubkey()).map_err(|err| {
            jsonrpc_core::error::Error::invalid_params(format!(
                "Unable to load tower file for new identity: {}",
                err
            ))
        })?;

        if let Some(cluster_info) = meta.cluster_info.read().unwrap().as_ref() {
            solana_metrics::set_host_id(identity_keypair.pubkey().to_string());
            cluster_info.set_keypair(Arc::new(identity_keypair));
            warn!("Identity set to {}", cluster_info.id());
            Ok(())
        } else {
            Err(jsonrpc_core::error::Error::invalid_params(
                "Retry once validator start up is complete",
            ))
        }
    }

    fn contact_info(&self, meta: Self::Metadata) -> Result<AdminRpcContactInfo> {
        if let Some(cluster_info) = meta.cluster_info.read().unwrap().as_ref() {
            Ok(cluster_info.my_contact_info().into())
        } else {
            Err(jsonrpc_core::error::Error::invalid_params(
                "Retry once validator start up is complete",
            ))
        }
    }
}

// Start the Admin RPC interface
pub fn run(ledger_path: &Path, metadata: AdminRpcRequestMetadata) {
    let admin_rpc_path = admin_rpc_path(ledger_path);

    let event_loop = tokio::runtime::Builder::new_multi_thread()
        .thread_name("sol-adminrpc-el")
        .enable_all()
        .build()
        .unwrap();

    Builder::new()
        .name("solana-adminrpc".to_string())
        .spawn(move || {
            let mut io = MetaIoHandler::default();
            io.extend_with(AdminRpcImpl.to_delegate());

            let validator_exit = metadata.validator_exit.clone();
            let server = ServerBuilder::with_meta_extractor(io, move |_req: &RequestContext| {
                metadata.clone()
            })
            .event_loop_executor(event_loop.handle().clone())
            .start(&format!("{}", admin_rpc_path.display()));

            match server {
                Err(err) => {
                    warn!("Unable to start admin rpc service: {:?}", err);
                }
                Ok(server) => {
                    let close_handle = server.close_handle();
                    validator_exit
                        .write()
                        .unwrap()
                        .register_exit(Box::new(move || {
                            close_handle.close();
                        }));

                    server.wait();
                }
            }
        })
        .unwrap();
}

fn admin_rpc_path(ledger_path: &Path) -> PathBuf {
    #[cfg(target_family = "windows")]
    {
        // More information about the wackiness of pipe names over at
        // https://docs.microsoft.com/en-us/windows/win32/ipc/pipe-names
        if let Some(ledger_filename) = ledger_path.file_name() {
            PathBuf::from(format!(
                "\\\\.\\pipe\\{}-admin.rpc",
                ledger_filename.to_string_lossy()
            ))
        } else {
            PathBuf::from("\\\\.\\pipe\\admin.rpc")
        }
    }
    #[cfg(not(target_family = "windows"))]
    {
        ledger_path.join("admin.rpc")
    }
}

// Connect to the Admin RPC interface
pub async fn connect(ledger_path: &Path) -> std::result::Result<gen_client::Client, RpcError> {
    let admin_rpc_path = admin_rpc_path(ledger_path);
    if !admin_rpc_path.exists() {
        Err(RpcError::Client(format!(
            "{} does not exist",
            admin_rpc_path.display()
        )))
    } else {
        ipc::connect::<_, gen_client::Client>(&format!("{}", admin_rpc_path.display())).await
    }
}

pub fn runtime() -> jsonrpc_server_utils::tokio::runtime::Runtime {
    jsonrpc_server_utils::tokio::runtime::Runtime::new().expect("new tokio runtime")
}
