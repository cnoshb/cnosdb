use std::collections::{HashMap, HashSet};
use std::fmt::Display;
use std::fs::{remove_file, rename};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use lru_cache::asynchronous::ShardedCache;
use memory_pool::{GreedyMemoryPool, MemoryPoolRef};
use meta::MetaRef;
use metrics::metric_register::MetricsRegister;
use models::Timestamp;
use serde::{Deserialize, Serialize};
use tokio::runtime::Runtime;
use tokio::sync::mpsc::Sender;
use tokio::sync::oneshot::Sender as OneShotSender;
use tokio::sync::RwLock;
use trace::error;
use utils::BloomFilter;

use crate::compaction::{CompactTask, FlushReq};
use crate::context::{GlobalContext, GlobalSequenceTask};
use crate::error::{Error, Result};
use crate::file_system::file_manager::try_exists;
use crate::kv_option::{Options, StorageOptions};
use crate::record_file::{Reader, RecordDataType, RecordDataVersion, Writer};
use crate::tseries_family::{ColumnFile, LevelInfo, Version};
use crate::tsm::TsmReader;
use crate::version_set::VersionSet;
use crate::{byte_utils, file_utils, ColumnFileId, LevelId, TseriesFamilyId};

const MAX_BATCH_SIZE: usize = 64;

#[derive(Serialize, Deserialize, PartialEq, Eq, Debug, Clone)]
pub struct CompactMeta {
    pub file_id: u64,
    pub file_size: u64,
    pub tsf_id: TseriesFamilyId,
    pub level: LevelId,
    pub min_ts: Timestamp,
    pub max_ts: Timestamp,
    pub high_seq: u64,
    pub low_seq: u64,
    pub is_delta: bool,
}

impl Default for CompactMeta {
    fn default() -> Self {
        Self {
            file_id: 0,
            file_size: 0,
            tsf_id: 0,
            level: 0,
            min_ts: Timestamp::MAX,
            max_ts: Timestamp::MIN,
            high_seq: u64::MIN,
            low_seq: u64::MIN,
            is_delta: false,
        }
    }
}

/// There are serial fields set with default value:
/// - tsf_id
/// - high_seq
/// - low_seq
impl From<&ColumnFile> for CompactMeta {
    fn from(file: &ColumnFile) -> Self {
        Self {
            file_id: file.file_id(),
            file_size: file.size(),
            level: file.level(),
            min_ts: file.time_range().min_ts,
            max_ts: file.time_range().max_ts,
            is_delta: file.is_delta(),
            ..Default::default()
        }
    }
}

impl CompactMeta {
    pub fn file_path(
        &self,
        storage_opt: &StorageOptions,
        database: &str,
        ts_family_id: TseriesFamilyId,
    ) -> PathBuf {
        if self.is_delta {
            let base_dir = storage_opt.delta_dir(database, ts_family_id);
            file_utils::make_delta_file_name(base_dir, self.file_id)
        } else {
            let base_dir = storage_opt.tsm_dir(database, ts_family_id);
            file_utils::make_tsm_file_name(base_dir, self.file_id)
        }
    }
}

pub struct CompactMetaBuilder {
    pub ts_family_id: TseriesFamilyId,
}

impl CompactMetaBuilder {
    pub fn new(ts_family_id: TseriesFamilyId) -> Self {
        Self { ts_family_id }
    }

    pub fn build_tsm(
        &self,
        file_id: u64,
        file_size: u64,
        level: LevelId,
        min_ts: Timestamp,
        max_ts: Timestamp,
    ) -> CompactMeta {
        CompactMeta {
            file_id,
            file_size,
            tsf_id: self.ts_family_id,
            level,
            min_ts,
            max_ts,
            is_delta: false,
            ..Default::default()
        }
    }

    pub fn build_delta(
        &self,
        file_id: u64,
        file_size: u64,
        level: LevelId,
        min_ts: Timestamp,
        max_ts: Timestamp,
    ) -> CompactMeta {
        CompactMeta {
            file_id,
            file_size,
            tsf_id: self.ts_family_id,
            level,
            min_ts,
            max_ts,
            is_delta: true,
            ..Default::default()
        }
    }
}

#[derive(Serialize, Deserialize, PartialEq, Eq, Debug, Clone)]
pub struct VersionEdit {
    pub has_seq_no: bool,
    pub seq_no: u64,
    pub has_file_id: bool,
    pub file_id: u64,
    pub max_level_ts: Timestamp,
    pub add_files: Vec<CompactMeta>,
    pub del_files: Vec<CompactMeta>,

    pub del_tsf: bool,
    pub add_tsf: bool,
    pub tsf_id: TseriesFamilyId,
    pub tsf_name: String,
}

impl Default for VersionEdit {
    fn default() -> Self {
        Self {
            has_seq_no: false,
            seq_no: 0,
            has_file_id: false,
            file_id: 0,
            max_level_ts: i64::MIN,
            add_files: vec![],
            del_files: vec![],
            del_tsf: false,
            add_tsf: false,
            tsf_id: 0,
            tsf_name: String::from(""),
        }
    }
}

impl VersionEdit {
    pub fn new(vnode_id: TseriesFamilyId) -> Self {
        Self {
            tsf_id: vnode_id,
            ..Default::default()
        }
    }

    pub fn new_add_vnode(vnode_id: u32, owner: String) -> Self {
        Self {
            tsf_id: vnode_id,
            tsf_name: owner,
            add_tsf: true,
            ..Default::default()
        }
    }

    pub fn new_del_vnode(vnode_id: u32) -> Self {
        Self {
            tsf_id: vnode_id,
            del_tsf: true,
            ..Default::default()
        }
    }

    pub fn encode(&self) -> Result<Vec<u8>> {
        bincode::serialize(self).map_err(|e| Error::Encode { source: (e) })
    }

    pub fn decode(buf: &[u8]) -> Result<Self> {
        bincode::deserialize(buf).map_err(|e| Error::Decode { source: (e) })
    }

    pub fn encode_vec(data: &[Self]) -> Result<Vec<u8>> {
        let mut buf: Vec<u8> = Vec::with_capacity(data.len() * 32);
        for ve in data {
            let ve_buf = bincode::serialize(ve).map_err(|e| Error::Encode { source: (e) })?;
            let pos = buf.len();
            buf.resize(pos + 4 + ve_buf.len(), 0_u8);
            buf[pos..pos + 4].copy_from_slice((ve_buf.len() as u32).to_be_bytes().as_slice());
            buf[pos + 4..].copy_from_slice(&ve_buf);
        }

        Ok(buf)
    }

    pub fn decode_vec(buf: &[u8]) -> Result<Vec<Self>> {
        let mut list: Vec<Self> = Vec::with_capacity(buf.len() / 32 + 1);
        let mut pos = 0_usize;
        while pos < buf.len() {
            if buf.len() - pos < 4 {
                break;
            }
            let len = byte_utils::decode_be_u32(&buf[pos..pos + 4]);
            pos += 4;
            if buf.len() - pos < len as usize {
                break;
            }
            let ve = Self::decode(&buf[pos..pos + len as usize])?;
            pos += len as usize;
            list.push(ve);
        }

        Ok(list)
    }

    pub fn add_file(&mut self, compact_meta: CompactMeta, max_level_ts: i64) {
        if compact_meta.high_seq != 0 {
            // ComapctMeta.seq_no only makes sense when flush.
            // In other processes, we set high_seq 0 .
            self.has_seq_no = true;
            self.seq_no = compact_meta.high_seq;
        }
        self.has_file_id = true;
        self.file_id = self.file_id.max(compact_meta.file_id);
        self.max_level_ts = max_level_ts;
        self.tsf_id = compact_meta.tsf_id;
        self.add_files.push(compact_meta);
    }

    pub fn del_file(&mut self, level: LevelId, file_id: u64, is_delta: bool) {
        self.del_files.push(CompactMeta {
            file_id,
            level,
            is_delta,
            ..Default::default()
        });
    }
}

impl Display for VersionEdit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "seq_no: {}, file_id: {}, add_files: {}, del_files: {}, del_tsf: {}, add_tsf: {}, tsf_id: {}, tsf_name: {}, has_seq_no: {}, has_file_id: {}, max_level_ts: {}",
               self.seq_no, self.file_id, self.add_files.len(), self.del_files.len(), self.del_tsf, self.add_tsf, self.tsf_id, self.tsf_name, self.has_seq_no, self.has_file_id, self.max_level_ts)
    }
}

pub struct Summary {
    file_no: u64,
    version_set: Arc<RwLock<VersionSet>>,
    ctx: Arc<GlobalContext>,
    writer: Writer,
    opt: Arc<Options>,
    runtime: Arc<Runtime>,
    sequence_task_sender: Sender<GlobalSequenceTask>,
    metrics_register: Arc<MetricsRegister>,
}

impl Summary {
    // create a new summary file
    pub async fn new(
        opt: Arc<Options>,
        runtime: Arc<Runtime>,
        memory_pool: MemoryPoolRef,
        sequence_task_sender: Sender<GlobalSequenceTask>,
        metrics_register: Arc<MetricsRegister>,
    ) -> Result<Self> {
        let db = VersionEdit::default();
        let path = file_utils::make_summary_file(opt.storage.summary_dir(), 0);
        let mut w = Writer::open(path, RecordDataType::Summary).await.unwrap();
        let buf = db.encode()?;
        let _ = w
            .write_record(
                RecordDataVersion::V1.into(),
                RecordDataType::Summary.into(),
                &[&buf],
            )
            .await?;
        w.sync().await?;

        Ok(Self {
            file_no: 0,
            version_set: Arc::new(RwLock::new(VersionSet::empty(
                opt.clone(),
                runtime.clone(),
                memory_pool,
                metrics_register.clone(),
            ))),
            ctx: Arc::new(GlobalContext::default()),
            writer: w,
            opt,
            runtime,
            sequence_task_sender,
            metrics_register,
        })
    }

    /// Recover from summary file
    ///
    /// If `load_file_filter` is `true`, field_filter will be loaded from file,
    /// otherwise default `BloomFilter::default()`
    #[allow(clippy::too_many_arguments)]
    pub async fn recover(
        meta: MetaRef,
        opt: Arc<Options>,
        runtime: Arc<Runtime>,
        flush_task_sender: Sender<FlushReq>,
        sequence_task_sender: Sender<GlobalSequenceTask>,
        compact_task_sender: Sender<CompactTask>,
        load_field_filter: bool,
        metrics_register: Arc<MetricsRegister>,
    ) -> Result<Self> {
        let summary_path = opt.storage.summary_dir();
        let path = file_utils::make_summary_file(&summary_path, 0);
        let writer = Writer::open(path, RecordDataType::Summary).await.unwrap();
        let ctx = Arc::new(GlobalContext::default());
        let rd = Box::new(
            Reader::open(&file_utils::make_summary_file(&summary_path, 0))
                .await
                .unwrap(),
        );
        let vs = Self::recover_version(
            meta,
            rd,
            &ctx,
            opt.clone(),
            runtime.clone(),
            flush_task_sender,
            compact_task_sender,
            load_field_filter,
            metrics_register.clone(),
        )
        .await?;

        Ok(Self {
            file_no: 0,
            version_set: Arc::new(RwLock::new(vs)),
            ctx,
            writer,
            opt,
            runtime,
            sequence_task_sender,
            metrics_register,
        })
    }

    /// Recover from summary file
    ///
    /// If `load_file_filter` is `true`, field_filter will be loaded from file,
    /// otherwise default `BloomFilter::default()`
    #[allow(clippy::too_many_arguments)]
    pub async fn recover_version(
        meta: MetaRef,
        mut reader: Box<Reader>,
        ctx: &GlobalContext,
        opt: Arc<Options>,
        runtime: Arc<Runtime>,
        flush_task_sender: Sender<FlushReq>,
        compact_task_sender: Sender<CompactTask>,
        load_field_filter: bool,
        metrics_register: Arc<MetricsRegister>,
    ) -> Result<VersionSet> {
        let mut tsf_edits_map: HashMap<TseriesFamilyId, Vec<VersionEdit>> = HashMap::new();
        let mut database_map: HashMap<String, Arc<String>> = HashMap::new();
        let mut tsf_database_map: HashMap<TseriesFamilyId, Arc<String>> = HashMap::new();

        loop {
            let res = reader.read_record().await;
            match res {
                Ok(result) => {
                    let ed = VersionEdit::decode(&result.data)?;
                    if ed.add_tsf {
                        tsf_edits_map.insert(ed.tsf_id, vec![]);
                        let db_ref = database_map
                            .entry(ed.tsf_name.clone())
                            .or_insert_with(|| Arc::new(ed.tsf_name));
                        tsf_database_map.insert(ed.tsf_id, db_ref.clone());
                    } else if ed.del_tsf {
                        tsf_edits_map.remove(&ed.tsf_id);
                        tsf_database_map.remove(&ed.tsf_id);
                    } else if let Some(data) = tsf_edits_map.get_mut(&ed.tsf_id) {
                        data.push(ed);
                    }
                }
                Err(Error::Eof) => break,
                Err(e) => {
                    println!("Error reading record: {:?}", e);
                    break;
                }
            }
        }

        let mut versions = HashMap::new();
        let mut has_seq_no = false;
        let mut seq_no = 0_u64;
        let mut has_file_id = false;
        let mut file_id = 0_u64;
        for (tsf_id, edits) in tsf_edits_map {
            let database = tsf_database_map.remove(&tsf_id).unwrap();

            let mut files: HashMap<u64, CompactMeta> = HashMap::new();
            let mut max_log = 0;
            let mut max_level_ts = i64::MIN;
            for e in edits {
                if e.has_seq_no {
                    has_seq_no = true;
                    seq_no = e.seq_no;
                }
                if e.has_file_id {
                    has_file_id = true;
                    file_id = e.file_id;
                }
                max_log = std::cmp::max(max_log, e.seq_no);
                max_level_ts = std::cmp::max(max_level_ts, e.max_level_ts);
                for m in e.del_files {
                    files.remove(&m.file_id);
                }
                for m in e.add_files {
                    files.insert(m.file_id, m);
                }
            }
            let mut levels = LevelInfo::init_levels(database.clone(), tsf_id, opt.storage.clone());
            // according files map to recover levels_info;
            for (_file_id, meta) in files {
                let field_filter = if load_field_filter {
                    let tsm_path = meta.file_path(opt.storage.as_ref(), &database, tsf_id);
                    let tsm_reader = TsmReader::open(tsm_path).await?;
                    tsm_reader.bloom_filter()
                } else {
                    Arc::new(BloomFilter::default())
                };
                levels[meta.level as usize].push_compact_meta(&meta, field_filter);
            }
            let ver = Version::new(
                tsf_id,
                database,
                opt.storage.clone(),
                max_log,
                levels,
                max_level_ts,
                Arc::new(ShardedCache::default()),
            );
            versions.insert(tsf_id, Arc::new(ver));
        }

        if has_seq_no {
            ctx.set_last_seq(seq_no + 1);
        }
        if has_file_id {
            ctx.set_file_id(file_id + 1);
        }
        let memory_pool = Arc::new(GreedyMemoryPool::new(1024 * 1024 * 1024));
        let vs = VersionSet::new(
            meta,
            opt,
            runtime,
            memory_pool,
            versions,
            flush_task_sender,
            compact_task_sender,
            metrics_register,
        )
        .await?;
        Ok(vs)
    }

    /// Applies version edit to summary file, and generates new version for TseriesFamily.
    pub async fn apply_version_edit(
        &mut self,
        version_edits: Vec<VersionEdit>,
        file_metas: HashMap<ColumnFileId, Arc<BloomFilter>>,
    ) -> Result<()> {
        self.write_summary(version_edits, file_metas).await?;
        self.roll_summary_file().await?;
        Ok(())
    }

    /// Write VersionEdits into summary file, generate and then apply new Versions for TseriesFamilies.
    async fn write_summary(
        &mut self,
        version_edits: Vec<VersionEdit>,
        mut file_metas: HashMap<ColumnFileId, Arc<BloomFilter>>,
    ) -> Result<()> {
        // Write VersionEdits into summary file and join VersionEdits by Database/TseriesFamilyId.
        let mut tsf_version_edits: HashMap<TseriesFamilyId, Vec<VersionEdit>> = HashMap::new();
        let mut tsf_min_seq: HashMap<TseriesFamilyId, u64> = HashMap::new();
        let mut del_tsf: HashSet<TseriesFamilyId> = HashSet::new();
        for edit in version_edits.into_iter() {
            let buf = edit.encode()?;
            let _ = self
                .writer
                .write_record(
                    RecordDataVersion::V1.into(),
                    RecordDataType::Summary.into(),
                    &[&buf],
                )
                .await?;
            self.writer.sync().await?;

            tsf_version_edits
                .entry(edit.tsf_id)
                .or_default()
                .push(edit.clone());
            if edit.has_seq_no {
                tsf_min_seq.insert(edit.tsf_id, edit.seq_no);
            }
            if edit.del_tsf {
                del_tsf.insert(edit.tsf_id);
            }
        }

        // For each TsereiesFamily - VersionEdits，generate a new Version and then apply it.
        let version_set = self.version_set.read().await;
        for (tsf_id, edits) in tsf_version_edits {
            let min_seq = tsf_min_seq.get(&tsf_id);
            if let Some(tsf) = version_set.get_tsfamily_by_tf_id(tsf_id).await {
                let new_version = tsf.read().await.version().copy_apply_version_edits(
                    edits,
                    &mut file_metas,
                    min_seq.copied(),
                );
                tsf.write().await.new_version(new_version);
            }
        }
        drop(version_set);

        // Send a GlobalSequenceTask to get a global min_sequence
        if let Err(_e) = self
            .sequence_task_sender
            .send(GlobalSequenceTask {
                del_ts_family: del_tsf,
                ts_family_min_seq: tsf_min_seq,
            })
            .await
        {
            error!("Failed to send AfterSummaryTask");
        }

        Ok(())
    }

    async fn roll_summary_file(&mut self) -> Result<()> {
        if self.writer.file_size() >= self.opt.storage.max_summary_size {
            let (edits, file_metas) = {
                let vs = self.version_set.read().await;
                vs.snapshot(self.ctx.last_seq()).await
            };

            let new_path = &file_utils::make_summary_file_tmp(self.opt.storage.summary_dir());
            let old_path = &self.writer.path().clone();
            if try_exists(new_path) {
                match remove_file(new_path) {
                    Ok(_) => (),
                    Err(e) => {
                        error!("failed remove file {:?}, in case {:?}", new_path, e);
                    }
                };
            }
            self.writer = Writer::open(new_path, RecordDataType::Summary)
                .await
                .unwrap();
            self.write_summary(edits, file_metas).await?;
            match rename(new_path, old_path) {
                Ok(_) => (),
                Err(e) => {
                    error!(
                        "failed remove old file {:?}, and create new file {:?},in case {:?}",
                        old_path, new_path, e
                    );
                }
            };
            self.writer = Writer::open(old_path, RecordDataType::Summary)
                .await
                .unwrap();
        }
        Ok(())
    }

    pub fn version_set(&self) -> Arc<RwLock<VersionSet>> {
        self.version_set.clone()
    }

    pub fn global_context(&self) -> Arc<GlobalContext> {
        self.ctx.clone()
    }
}

pub async fn print_summary_statistics(path: impl AsRef<Path>) {
    let mut reader = Reader::open(&path).await.unwrap();
    println!("============================================================");
    let mut i = 0_usize;
    loop {
        match reader.read_record().await {
            Ok(record) => {
                let ve = VersionEdit::decode(&record.data).unwrap();
                println!("VersionEdit #{}, vnode_id: {}", i, ve.tsf_id);
                println!("------------------------------------------------------------");
                i += 1;
                if ve.add_tsf {
                    println!("  Add ts_family: {}", ve.tsf_id);
                    println!("------------------------------------------------------------");
                }
                if ve.del_tsf {
                    println!("  Delete ts_family: {}", ve.tsf_id);
                    println!("------------------------------------------------------------");
                }
                if ve.has_seq_no {
                    println!("  Presist sequence: {}", ve.seq_no);
                    println!("------------------------------------------------------------");
                }
                if ve.has_file_id {
                    if ve.add_files.is_empty() && ve.del_files.is_empty() {
                        println!("  Add file: None. Delete file: None.");
                    }
                    if !ve.add_files.is_empty() {
                        let mut buffer = String::new();
                        ve.add_files.iter().for_each(|f| {
                            buffer.push_str(
                                format!("{} (level: {}, {} B), ", f.file_id, f.level, f.file_size)
                                    .as_str(),
                            )
                        });
                        if !buffer.is_empty() {
                            buffer.truncate(buffer.len() - 2);
                        }
                        println!("  Add file:[ {} ]", buffer);
                    }
                    if !ve.del_files.is_empty() {
                        let mut buffer = String::new();
                        ve.del_files.iter().for_each(|f| {
                            buffer
                                .push_str(format!("{} (level: {}), ", f.file_id, f.level).as_str())
                        });
                        if !buffer.is_empty() {
                            buffer.truncate(buffer.len() - 2);
                        }
                        println!("  Delete file:[ {} ]", buffer);
                    }
                }
            }
            Err(err) => match err {
                Error::Eof => break,
                _ => panic!("Errors when read summary file: {}", err),
            },
        }
        println!("============================================================");
    }
}

pub struct SummaryProcessor {
    summary: Box<Summary>,
    cbs: Vec<OneShotSender<Result<()>>>,
    edits: Vec<VersionEdit>,
    file_metas: HashMap<ColumnFileId, Arc<BloomFilter>>,
}

impl SummaryProcessor {
    pub fn new(summary: Box<Summary>) -> Self {
        Self {
            summary,
            cbs: vec![],
            edits: vec![],
            file_metas: HashMap::new(),
        }
    }

    pub fn batch(&mut self, task: SummaryTask) {
        let mut req = match task {
            SummaryTask::ColumnFile {
                file_metas,
                request,
            } => {
                self.file_metas.extend(file_metas.into_iter());
                request
            }
            SummaryTask::Vnode { request } => request,
            SummaryTask::ApplySummary { .. } => {
                // TODO append ts_family into this current vnode
                return;
            }
        };
        self.edits.append(&mut req.version_edits);
        self.cbs.push(req.call_back);
    }

    pub async fn apply(&mut self) {
        let edits = std::mem::take(&mut self.edits);
        let file_metas = std::mem::take(&mut self.file_metas);
        match self.summary.apply_version_edit(edits, file_metas).await {
            Ok(()) => {
                for cb in self.cbs.drain(..) {
                    let _ = cb.send(Ok(()));
                }
            }
            Err(_e) => {
                for cb in self.cbs.drain(..) {
                    let _ = cb.send(Err(Error::ErrApplyEdit));
                }
            }
        }
    }

    pub fn summary(&self) -> &Summary {
        &self.summary
    }
}

#[derive(Debug)]
pub enum SummaryTask {
    /// Vnode create / delete.
    Vnode { request: WriteSummaryRequest },
    /// Column file add / delete. Usually in compactions.
    ColumnFile {
        request: WriteSummaryRequest,
        file_metas: HashMap<ColumnFileId, Arc<BloomFilter>>,
    },
    /// TODO: Apply summary directly. Usually in vnode migrate.
    ApplySummary {
        request: WriteSummaryRequest,
        ts_family_id: TseriesFamilyId,
    },
}

impl SummaryTask {
    pub fn new_vnode_task(
        version_edits: Vec<VersionEdit>,
        call_back: OneShotSender<Result<()>>,
    ) -> Self {
        Self::Vnode {
            request: WriteSummaryRequest {
                version_edits,
                call_back,
            },
        }
    }

    pub fn new_column_file_task(
        file_metas: HashMap<ColumnFileId, Arc<BloomFilter>>,
        version_edits: Vec<VersionEdit>,
        call_back: OneShotSender<Result<()>>,
    ) -> Self {
        Self::ColumnFile {
            file_metas,
            request: WriteSummaryRequest {
                version_edits,
                call_back,
            },
        }
    }

    pub fn new_apply_task(
        ts_family_id: TseriesFamilyId,
        version_edits: Vec<VersionEdit>,
        call_back: OneShotSender<Result<()>>,
    ) -> Self {
        Self::ApplySummary {
            ts_family_id,
            request: WriteSummaryRequest {
                version_edits,
                call_back,
            },
        }
    }

    pub fn write_summary_request(self) -> WriteSummaryRequest {
        match self {
            SummaryTask::Vnode { request: req, .. } => req,
            SummaryTask::ColumnFile { request: req, .. } => req,
            SummaryTask::ApplySummary { request: req, .. } => req,
        }
    }
}

#[derive(Debug)]
pub struct WriteSummaryRequest {
    pub version_edits: Vec<VersionEdit>,
    pub call_back: OneShotSender<Result<()>>,
}

#[derive(Clone)]
pub struct SummaryScheduler {
    sender: Sender<SummaryTask>,
}

impl SummaryScheduler {
    pub fn new(sender: Sender<SummaryTask>) -> Self {
        Self { sender }
    }
}

#[cfg(test)]
mod test {
    use std::collections::HashMap;
    use std::fs;
    use std::sync::Arc;

    use config::{get_config, ClusterConfig};
    use memory_pool::GreedyMemoryPool;
    use meta::meta_manager::RemoteMetaManager;
    use meta::MetaRef;
    use metrics::metric_register::MetricsRegister;
    use models::schema::{make_owner, DatabaseSchema, TenantOptions};
    use tokio::runtime::Runtime;
    use tokio::sync::mpsc;
    use tokio::sync::mpsc::Sender;
    use utils::BloomFilter;

    use crate::compaction::{CompactTask, FlushReq};
    use crate::context::GlobalSequenceTask;
    use crate::file_system::file_manager;
    use crate::kv_option::Options;
    use crate::kvcore::{
        COMPACT_REQ_CHANNEL_CAP, GLOBAL_TASK_REQ_CHANNEL_CAP, SUMMARY_REQ_CHANNEL_CAP,
    };
    use crate::summary::{CompactMeta, Summary, SummaryTask, VersionEdit};

    #[test]
    fn test_version_edit() {
        let mut ve = VersionEdit::default();

        let add_file_100 = CompactMeta {
            file_id: 100,
            ..Default::default()
        };
        ve.add_file(add_file_100, 100_000_000);

        let del_file_101 = CompactMeta {
            file_id: 101,
            ..Default::default()
        };
        ve.del_files.push(del_file_101);

        let ve_buf = ve.encode().unwrap();
        let ve2 = VersionEdit::decode(&ve_buf).unwrap();
        assert_eq!(ve2, ve);

        let ves = vec![ve, ve2];
        let ves_buf = VersionEdit::encode_vec(&ves).unwrap();
        let ves_2 = VersionEdit::decode_vec(&ves_buf).unwrap();
        assert_eq!(ves, ves_2);
    }

    #[test]
    fn test_summary() {
        let mut config = get_config("../config/config_31001.toml");

        let runtime = Arc::new(
            tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .worker_threads(4)
                .build()
                .unwrap(),
        );

        // NOTICE: Make sure these channel senders are dropped before test_summary() finished.
        let (summary_task_sender, mut summary_task_receiver) =
            mpsc::channel::<SummaryTask>(SUMMARY_REQ_CHANNEL_CAP);
        let summary_job_mock = runtime.spawn(async move {
            println!("Mock summary job started (test_summary).");
            while let Some(t) = summary_task_receiver.recv().await {
                // Do nothing
                let _ = t.write_summary_request().call_back.send(Ok(()));
            }
            println!("Mock summary job finished (test_summary).");
        });
        let (flush_task_sender, mut flush_task_receiver) =
            mpsc::channel::<FlushReq>(config.storage.flush_req_channel_cap);
        let flush_job_mock = runtime.spawn(async move {
            println!("Mock flush job started (test_summary).");
            while flush_task_receiver.recv().await.is_some() {
                // Do nothing
            }
            println!("Mock flush job finished (test_summary).");
        });
        let (global_seq_task_sender, mut global_seq_task_receiver) =
            mpsc::channel::<GlobalSequenceTask>(GLOBAL_TASK_REQ_CHANNEL_CAP);
        let _global_seq_job_mock = runtime.spawn(async move {
            println!("Mock global sequence job started (test_summary).");
            while let Some(_t) = global_seq_task_receiver.recv().await {
                // Do nothing
            }
            println!("Mock global sequence job finished (test_summary).");
        });
        let (compact_task_sender, mut compact_task_receiver) =
            mpsc::channel::<CompactTask>(COMPACT_REQ_CHANNEL_CAP);
        let compact_job_mock = runtime.spawn(async move {
            println!("Mock compact job started (test_summary).");
            while compact_task_receiver.recv().await.is_some() {
                // Do nothing
            }
            println!("Mock compact job finished (test_summary).");
        });

        let runtime_ref = runtime.clone();
        runtime.block_on(async move {
            println!("Running test: test_summary_recover");
            let base_dir = "/tmp/test/summary/test_summary_recover".to_string();
            let _ = fs::remove_dir_all(&base_dir);
            config.storage.path = base_dir.clone();
            test_summary_recover(
                Arc::new(Options::from(&config)),
                config.cluster.clone(),
                runtime_ref.clone(),
                flush_task_sender.clone(),
                global_seq_task_sender.clone(),
                compact_task_sender.clone(),
            )
            .await;

            println!("Running test: test_tsf_num_recover");
            let base_dir = "/tmp/test/summary/test_tsf_num_recover".to_string();
            let _ = fs::remove_dir_all(&base_dir);
            config.storage.path = base_dir.clone();
            test_tsf_num_recover(
                Arc::new(Options::from(&config)),
                config.cluster.clone(),
                runtime_ref.clone(),
                flush_task_sender.clone(),
                global_seq_task_sender.clone(),
                compact_task_sender.clone(),
            )
            .await;

            println!("Running test: test_recover_summary_with_roll_0");
            let base_dir = "/tmp/test/summary/test_recover_summary_with_roll_0".to_string();
            let _ = fs::remove_dir_all(&base_dir);
            config.storage.path = base_dir.clone();
            test_recover_summary_with_roll_0(
                Arc::new(Options::from(&config)),
                config.cluster.clone(),
                runtime_ref.clone(),
                summary_task_sender.clone(),
                flush_task_sender.clone(),
                global_seq_task_sender.clone(),
                compact_task_sender.clone(),
            )
            .await;

            println!("Running test: test_recover_summary_with_roll_1");
            let base_dir = "/tmp/test/summary/test_recover_summary_with_roll_1".to_string();
            let _ = fs::remove_dir_all(&base_dir);
            config.storage.path = base_dir.clone();
            test_recover_summary_with_roll_1(
                Arc::new(Options::from(&config)),
                config.cluster.clone(),
                runtime_ref.clone(),
                summary_task_sender,
                flush_task_sender,
                global_seq_task_sender,
                compact_task_sender,
            )
            .await;

            let _ = tokio::join!(summary_job_mock, flush_job_mock, compact_job_mock);
        });
    }

    async fn test_summary_recover(
        opt: Arc<Options>,
        cluster_options: ClusterConfig,
        runtime: Arc<Runtime>,
        flush_task_sender: Sender<FlushReq>,
        global_seq_task_sender: Sender<GlobalSequenceTask>,
        compact_task_sender: Sender<CompactTask>,
    ) {
        let meta_manager: MetaRef = RemoteMetaManager::new(cluster_options).await;
        meta_manager.admin_meta().add_data_node().await.unwrap();
        let _ = meta_manager
            .tenant_manager()
            .create_tenant("cnosdb".to_string(), TenantOptions::default())
            .await;
        let summary_dir = opt.storage.summary_dir();
        if !file_manager::try_exists(&summary_dir) {
            std::fs::create_dir_all(&summary_dir).unwrap();
        }
        let memory_pool = Arc::new(GreedyMemoryPool::new(1024 * 1024 * 1024));
        let mut summary = Summary::new(
            opt.clone(),
            runtime.clone(),
            memory_pool,
            global_seq_task_sender.clone(),
            Arc::new(MetricsRegister::default()),
        )
        .await
        .unwrap();
        let edit = VersionEdit::new_add_vnode(100, "cnosdb.hello".to_string());
        summary
            .apply_version_edit(vec![edit], HashMap::new())
            .await
            .unwrap();
        let _summary = Summary::recover(
            meta_manager,
            opt.clone(),
            runtime.clone(),
            flush_task_sender,
            global_seq_task_sender,
            compact_task_sender,
            false,
            Arc::new(MetricsRegister::default()),
        )
        .await
        .unwrap();
    }

    async fn test_tsf_num_recover(
        opt: Arc<Options>,
        cluster_options: ClusterConfig,
        runtime: Arc<Runtime>,
        flush_task_sender: Sender<FlushReq>,
        global_seq_task_sender: Sender<GlobalSequenceTask>,
        compact_task_sender: Sender<CompactTask>,
    ) {
        let meta_manager: MetaRef = RemoteMetaManager::new(cluster_options).await;
        meta_manager.admin_meta().add_data_node().await.unwrap();
        let _ = meta_manager
            .tenant_manager()
            .create_tenant("cnosdb".to_string(), TenantOptions::default())
            .await;
        let summary_dir = opt.storage.summary_dir();
        if !file_manager::try_exists(&summary_dir) {
            std::fs::create_dir_all(&summary_dir).unwrap();
        }
        let memory_pool = Arc::new(GreedyMemoryPool::new(1024 * 1024 * 1024));
        let mut summary = Summary::new(
            opt.clone(),
            runtime.clone(),
            memory_pool,
            global_seq_task_sender.clone(),
            Arc::new(MetricsRegister::default()),
        )
        .await
        .unwrap();

        let edit = VersionEdit::new_add_vnode(100, "cnosdb.hello".to_string());
        summary
            .apply_version_edit(vec![edit], HashMap::new())
            .await
            .unwrap();
        let mut summary = Summary::recover(
            meta_manager.clone(),
            opt.clone(),
            runtime.clone(),
            flush_task_sender.clone(),
            global_seq_task_sender.clone(),
            compact_task_sender.clone(),
            false,
            Arc::new(MetricsRegister::default()),
        )
        .await
        .unwrap();
        assert_eq!(summary.version_set.read().await.tsf_num().await, 1);
        let edit = VersionEdit::new_del_vnode(100);
        summary
            .apply_version_edit(vec![edit], HashMap::new())
            .await
            .unwrap();
        let summary = Summary::recover(
            meta_manager,
            opt.clone(),
            runtime,
            flush_task_sender,
            global_seq_task_sender,
            compact_task_sender,
            false,
            Arc::new(MetricsRegister::default()),
        )
        .await
        .unwrap();
        assert_eq!(summary.version_set.read().await.tsf_num().await, 0);
    }

    // tips : we can use a small max_summary_size
    #[allow(clippy::too_many_arguments)]
    async fn test_recover_summary_with_roll_0(
        opt: Arc<Options>,
        cluster_options: ClusterConfig,
        runtime: Arc<Runtime>,
        summary_task_sender: Sender<SummaryTask>,
        flush_task_sender: Sender<FlushReq>,
        global_seq_task_sender: Sender<GlobalSequenceTask>,
        compact_task_sender: Sender<CompactTask>,
    ) {
        let meta_manager: MetaRef = RemoteMetaManager::new(cluster_options).await;
        meta_manager.admin_meta().add_data_node().await.unwrap();
        let _ = meta_manager
            .tenant_manager()
            .create_tenant("cnosdb".to_string(), TenantOptions::default())
            .await;
        let database = "test".to_string();
        let summary_dir = opt.storage.summary_dir();
        if !file_manager::try_exists(&summary_dir) {
            std::fs::create_dir_all(&summary_dir).unwrap();
        }
        let memory_pool = Arc::new(GreedyMemoryPool::new(1024 * 1024 * 1024));
        let mut summary = Summary::new(
            opt.clone(),
            runtime.clone(),
            memory_pool.clone(),
            global_seq_task_sender.clone(),
            Arc::new(MetricsRegister::default()),
        )
        .await
        .unwrap();

        let mut edits = vec![];
        let db = summary
            .version_set
            .write()
            .await
            .create_db(
                DatabaseSchema::new("cnosdb", &database),
                meta_manager.clone(),
                memory_pool,
            )
            .await
            .unwrap();
        for i in 0..40 {
            db.write()
                .await
                .add_tsfamily(
                    i,
                    0,
                    None,
                    summary_task_sender.clone(),
                    flush_task_sender.clone(),
                    compact_task_sender.clone(),
                )
                .await;
            let edit = VersionEdit::new_add_vnode(i, make_owner("cnosdb", &database));
            edits.push(edit.clone());
        }

        for _ in 0..100 {
            for i in 0..20 {
                db.write()
                    .await
                    .del_tsfamily(i, summary_task_sender.clone())
                    .await;
                edits.push(VersionEdit::new_del_vnode(i));
            }
        }
        summary
            .apply_version_edit(edits, HashMap::new())
            .await
            .unwrap();

        let summary = Summary::recover(
            meta_manager.clone(),
            opt.clone(),
            runtime,
            flush_task_sender.clone(),
            global_seq_task_sender.clone(),
            compact_task_sender,
            false,
            Arc::new(MetricsRegister::default()),
        )
        .await
        .unwrap();

        assert_eq!(summary.version_set.read().await.tsf_num().await, 20);
    }

    #[allow(clippy::too_many_arguments)]
    async fn test_recover_summary_with_roll_1(
        opt: Arc<Options>,
        cluster_options: ClusterConfig,
        runtime: Arc<Runtime>,
        summary_task_sender: Sender<SummaryTask>,
        flush_task_sender: Sender<FlushReq>,
        global_seq_task_sender: Sender<GlobalSequenceTask>,
        compact_task_sender: Sender<CompactTask>,
    ) {
        let memory_pool = Arc::new(GreedyMemoryPool::new(1024 * 1024 * 1024));
        let meta_manager: MetaRef = RemoteMetaManager::new(cluster_options).await;
        meta_manager.admin_meta().add_data_node().await.unwrap();
        let _ = meta_manager
            .tenant_manager()
            .create_tenant("cnosdb".to_string(), TenantOptions::default())
            .await;
        let database = "test".to_string();
        let summary_dir = opt.storage.summary_dir();
        if !file_manager::try_exists(&summary_dir) {
            std::fs::create_dir_all(&summary_dir).unwrap();
        }
        let mut summary = Summary::new(
            opt.clone(),
            runtime.clone(),
            memory_pool.clone(),
            global_seq_task_sender.clone(),
            Arc::new(MetricsRegister::default()),
        )
        .await
        .unwrap();

        let db = summary
            .version_set
            .write()
            .await
            .create_db(
                DatabaseSchema::new("cnosdb", &database),
                meta_manager.clone(),
                memory_pool,
            )
            .await
            .unwrap();
        db.write()
            .await
            .add_tsfamily(
                10,
                0,
                None,
                summary_task_sender.clone(),
                flush_task_sender.clone(),
                compact_task_sender.clone(),
            )
            .await;

        let mut edits = vec![];
        let edit = VersionEdit::new_add_vnode(10, "cnosdb.hello".to_string());
        edits.push(edit);

        for _ in 0..100 {
            db.write()
                .await
                .del_tsfamily(0, summary_task_sender.clone())
                .await;
            let edit = VersionEdit::new_del_vnode(0);
            edits.push(edit);
        }

        {
            let vs = summary.version_set.write().await;
            let tsf = vs.get_tsfamily_by_tf_id(10).await.unwrap();
            let mut version = tsf.read().await.version().copy_apply_version_edits(
                edits.clone(),
                &mut HashMap::new(),
                None,
            );

            summary.ctx.set_last_seq(1);
            let mut edit = VersionEdit::new(10);
            let meta = CompactMeta {
                file_id: 15,
                is_delta: false,
                file_size: 100,
                level: 1,
                min_ts: 1,
                max_ts: 1,
                tsf_id: 10,
                high_seq: 1,
                ..Default::default()
            };
            version.levels_info[1].push_compact_meta(&meta, Arc::new(BloomFilter::default()));
            tsf.write().await.new_version(version);
            edit.add_file(meta, 1);
            edits.push(edit);
        }

        summary
            .apply_version_edit(edits, HashMap::new())
            .await
            .unwrap();

        let summary = Summary::recover(
            meta_manager,
            opt,
            runtime,
            flush_task_sender,
            global_seq_task_sender,
            compact_task_sender,
            false,
            Arc::new(MetricsRegister::default()),
        )
        .await
        .unwrap();

        let vs = summary.version_set.read().await;
        let tsf = vs.get_tsfamily_by_tf_id(10).await.unwrap();
        assert_eq!(tsf.read().await.version().last_seq, 1);
        assert_eq!(tsf.read().await.version().levels_info[1].tsf_id, 10);
        assert!(!tsf.read().await.version().levels_info[1].files[0].is_delta());
        assert_eq!(
            tsf.read().await.version().levels_info[1].files[0].file_id(),
            15
        );
        assert_eq!(
            tsf.read().await.version().levels_info[1].files[0].size(),
            100
        );
        assert_eq!(summary.ctx.file_id(), 16);
    }
}
