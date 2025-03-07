use std::fmt::Display;
use std::sync::Arc;

use meta_admin::AdminMeta;
use meta_client::MetaClient;
use meta_manager::MetaManager;
use openraft::{Config, Raft};
use tenant_manager::TenantManager;
use user_manager::UserManager;

use crate::service::connection::Connections;
use crate::store::command::WriteCommand;
use crate::store::config::MetaInit;
use crate::store::state_machine::CommandResp;
use crate::store::Store;
pub mod client;
pub mod error;
pub mod limiter;
pub mod meta_admin;
pub mod meta_client;
pub mod meta_client_mock;
pub mod meta_manager;
pub mod service;
pub mod store;
pub mod tenant_manager;
pub mod user_manager;
pub mod user_manager_mock;

pub type UserManagerRef = Arc<dyn UserManager>;
pub type TenantManagerRef = Arc<dyn TenantManager>;
pub type MetaClientRef = Arc<dyn MetaClient>;
pub type AdminMetaRef = Arc<dyn AdminMeta>;
pub type MetaRef = Arc<dyn MetaManager>;

pub type ClusterNodeId = u64;
#[derive(Debug, Clone, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub struct ClusterNode {
    pub rpc_addr: String,
    pub api_addr: String,
}

impl Display for ClusterNode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "ClusterNode {{ rpc_addr: {}, api_addr: {} }}",
            self.rpc_addr, self.api_addr
        )
    }
}

openraft::declare_raft_types!(
    /// Declare the type configuration for example K/V store.
    pub TypeConfig: D = WriteCommand, R = CommandResp, NodeId = ClusterNodeId, Node = ClusterNode
);

pub type RaftStore = Raft<TypeConfig, Connections, Arc<Store>>;
pub struct MetaApp {
    pub id: ClusterNodeId,
    pub http_addr: String,
    pub rpc_addr: String,
    pub raft: RaftStore,
    pub store: Arc<Store>,
    pub config: Arc<Config>,
    // todo: Maybe we can remove this configuration that's only used in init
    pub meta_init: Arc<MetaInit>,
}
