use std::path::Path;
use std::time::Duration;

use models::meta_data::{VnodeAllInfo, VnodeInfo};
use protos::kv_service::tskv_service_client::TskvServiceClient;
use protos::kv_service::{
    DownloadFileRequest, FetchVnodeSummaryRequest, GetVnodeFilesMetaRequest,
    GetVnodeFilesMetaResponse,
};
use tokio::io::AsyncWriteExt;
use tokio_stream::StreamExt;
use tonic::transport::Channel;
use tower::timeout::Timeout;
use trace::info;

use crate::errors::{CoordinatorError, CoordinatorResult};
use crate::file_info::get_file_info;
use crate::SUCCESS_RESPONSE_CODE;

pub struct VnodeManager {
    node_id: u64,
    meta: meta::MetaRef,
    kv_inst: tskv::engine::EngineRef,
}

impl VnodeManager {
    pub fn new(meta: meta::MetaRef, kv_inst: tskv::engine::EngineRef, node_id: u64) -> Self {
        Self {
            node_id,
            meta,
            kv_inst,
        }
    }

    pub async fn move_vnode(&self, tenant: &str, vnode_id: u32) -> CoordinatorResult<()> {
        self.copy_vnode(tenant, vnode_id).await?;
        self.drop_vnode(tenant, vnode_id).await?;

        Ok(())
    }

    pub async fn copy_vnode(&self, tenant: &str, vnode_id: u32) -> CoordinatorResult<()> {
        let admin_meta = self.meta.admin_meta();
        let meta_client = self.meta.tenant_manager().tenant_meta(tenant).await.ok_or(
            CoordinatorError::TenantNotFound {
                name: tenant.to_string(),
            },
        )?;

        let new_id = admin_meta.retain_id(1).await?;
        let all_info = self.get_vnode_all_info(tenant, vnode_id).await?;
        info!(
            "Begin Copy Vnode:{} from: {} to: {}; new id: {}",
            vnode_id, all_info.node_id, self.node_id, new_id
        );

        let owner = models::schema::make_owner(&all_info.tenant, &all_info.db_name);
        let path = self
            .kv_inst
            .get_storage_options()
            .ts_family_dir(&owner, new_id);

        let node_id = all_info.node_id;
        let channel = self.meta.admin_meta().get_node_conn(node_id).await?;
        let timeout_channel = Timeout::new(channel, Duration::from_secs(60 * 60));
        let mut client = TskvServiceClient::<Timeout<Channel>>::new(timeout_channel);

        if let Err(err) = self
            .download_vnode_files(&all_info, &path, &mut client)
            .await
        {
            tokio::fs::remove_dir_all(&path).await?;
            return Err(err);
        }

        let add_repl = vec![VnodeInfo {
            id: new_id,
            node_id: self.node_id,
        }];
        meta_client
            .update_replication_set(
                &all_info.db_name,
                all_info.bucket_id,
                all_info.repl_set_id,
                &[],
                &add_repl,
            )
            .await?;

        let ve = self.fetch_vnode_summary(&all_info, &mut client).await?;
        self.kv_inst
            .apply_vnode_summary(tenant, &all_info.db_name, new_id, ve)
            .await?;

        Ok(())
    }

    pub async fn drop_vnode(&self, tenant: &str, vnode_id: u32) -> CoordinatorResult<()> {
        let all_info = self.get_vnode_all_info(tenant, vnode_id).await?;

        let meta_client = self.meta.tenant_manager().tenant_meta(tenant).await.ok_or(
            CoordinatorError::TenantNotFound {
                name: tenant.to_string(),
            },
        )?;

        self.kv_inst
            .remove_tsfamily(tenant, &all_info.db_name, vnode_id)
            .await?;

        let del_repl = vec![VnodeInfo {
            id: vnode_id,
            node_id: all_info.node_id,
        }];
        meta_client
            .update_replication_set(
                &all_info.db_name,
                all_info.bucket_id,
                all_info.repl_set_id,
                &del_repl,
                &[],
            )
            .await?;

        Ok(())
    }

    async fn fetch_vnode_summary(
        &self,
        all_info: &VnodeAllInfo,
        client: &mut TskvServiceClient<Timeout<Channel>>,
    ) -> CoordinatorResult<tskv::VersionEdit> {
        let request = tonic::Request::new(FetchVnodeSummaryRequest {
            tenant: all_info.tenant.clone(),
            database: all_info.db_name.clone(),
            vnode_id: all_info.vnode_id,
        });

        let resp = client.fetch_vnode_summary(request).await?.into_inner();
        if resp.code != SUCCESS_RESPONSE_CODE {
            return Err(CoordinatorError::GRPCRequest {
                msg: format!(
                    "server status: {}, {:?}",
                    resp.code,
                    String::from_utf8(resp.data)
                ),
            });
        }

        let ve = tskv::VersionEdit::decode(&resp.data)?;

        Ok(ve)
    }

    async fn download_vnode_files(
        &self,
        all_info: &VnodeAllInfo,
        data_path: &Path,
        client: &mut TskvServiceClient<Timeout<Channel>>,
    ) -> CoordinatorResult<()> {
        let files_meta = self.get_vnode_files_meta(all_info, client).await?;
        for info in files_meta.infos.iter() {
            let relative_filename = info
                .name
                .strip_prefix(&(files_meta.path.clone() + "/"))
                .unwrap();

            self.download_file(all_info, relative_filename, data_path, client)
                .await?;

            let filename = data_path.join(relative_filename);
            let filename = filename.to_string_lossy().to_string();
            let tmp_info = get_file_info(&filename).await?;
            if tmp_info.md5 != info.md5 {
                return Err(CoordinatorError::CommonError {
                    msg: "download file md5 not match ".to_string(),
                });
            }
        }

        Ok(())
    }

    async fn get_vnode_files_meta(
        &self,
        all_info: &VnodeAllInfo,
        client: &mut TskvServiceClient<Timeout<Channel>>,
    ) -> CoordinatorResult<GetVnodeFilesMetaResponse> {
        let request = tonic::Request::new(GetVnodeFilesMetaRequest {
            tenant: all_info.tenant.to_string(),
            db: all_info.db_name.to_string(),
            vnode_id: all_info.vnode_id,
        });

        let resp = client.get_vnode_files_meta(request).await?.into_inner();
        info!("node id: {}, files meta: {:?}", all_info.vnode_id, resp);

        Ok(resp)
    }

    async fn download_file(
        &self,
        req: &VnodeAllInfo,
        filename: &str,
        data_path: &Path,
        client: &mut TskvServiceClient<Timeout<Channel>>,
    ) -> CoordinatorResult<()> {
        let file_path = data_path.join(filename);
        tokio::fs::create_dir_all(file_path.parent().unwrap()).await?;
        let mut file = tokio::fs::OpenOptions::new()
            .create(true)
            .truncate(true)
            .read(true)
            .write(true)
            .open(&file_path)
            .await?;

        let request = tonic::Request::new(DownloadFileRequest {
            tenant: req.tenant.clone(),
            db: req.db_name.clone(),
            vnode_id: req.vnode_id,
            filename: filename.to_string(),
        });
        let mut resp_stream = client.download_file(request).await?.into_inner();
        while let Some(received) = resp_stream.next().await {
            let received = received?;
            if received.code != SUCCESS_RESPONSE_CODE {
                return Err(CoordinatorError::GRPCRequest {
                    msg: format!(
                        "server status: {}, {:?}",
                        received.code,
                        String::from_utf8(received.data)
                    ),
                });
            }

            file.write_all(&received.data).await?;
        }

        Ok(())
    }

    async fn get_vnode_all_info(
        &self,
        tenant: &str,
        vnode_id: u32,
    ) -> CoordinatorResult<VnodeAllInfo> {
        match self.meta.tenant_manager().tenant_meta(tenant).await {
            Some(meta_client) => match meta_client.get_vnode_all_info(vnode_id) {
                Some(all_info) => Ok(all_info),
                None => Err(CoordinatorError::VnodeNotFound { id: vnode_id }),
            },

            None => Err(CoordinatorError::TenantNotFound {
                name: tenant.to_string(),
            }),
        }
    }
}
