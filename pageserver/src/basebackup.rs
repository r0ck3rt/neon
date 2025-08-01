//!
//! Generate a tarball with files needed to bootstrap ComputeNode.
//!
//! TODO: this module has nothing to do with PostgreSQL pg_basebackup.
//! It could use a better name.
//!
//! Stateless Postgres compute node is launched by sending a tarball
//! which contains non-relational data (multixacts, clog, filenodemaps, twophase files),
//! generated pg_control and dummy segment of WAL.
//! This module is responsible for creation of such tarball
//! from data stored in object storage.
//!
use std::fmt::Write as FmtWrite;
use std::sync::Arc;
use std::time::{Instant, SystemTime};

use anyhow::{Context, anyhow};
use async_compression::tokio::write::GzipEncoder;
use bytes::{BufMut, Bytes, BytesMut};
use fail::fail_point;
use pageserver_api::key::{Key, rel_block_to_key};
use pageserver_api::reltag::{RelTag, SlruKind};
use postgres_ffi::pg_constants::{PG_HBA, PGDATA_SPECIAL_FILES};
use postgres_ffi::{
    BLCKSZ, PG_TLI, PgMajorVersion, RELSEG_SIZE, WAL_SEGMENT_SIZE, XLogFileName,
    dispatch_pgversion, pg_constants,
};
use postgres_ffi_types::constants::{DEFAULTTABLESPACE_OID, GLOBALTABLESPACE_OID};
use postgres_ffi_types::forknum::{INIT_FORKNUM, MAIN_FORKNUM};
use tokio::io::{self, AsyncWrite, AsyncWriteExt as _};
use tokio_tar::{Builder, EntryType, Header};
use tracing::*;
use utils::lsn::Lsn;

use crate::context::RequestContext;
use crate::pgdatadir_mapping::Version;
use crate::tenant::storage_layer::IoConcurrency;
use crate::tenant::timeline::{GetVectoredError, VersionedKeySpaceQuery};
use crate::tenant::{PageReconstructError, Timeline};

#[derive(Debug, thiserror::Error)]
pub enum BasebackupError {
    #[error("basebackup pageserver error {0:#}")]
    Server(#[from] anyhow::Error),
    #[error("basebackup client error {0:#} when {1}")]
    Client(#[source] io::Error, &'static str),
    #[error("basebackup during shutdown")]
    Shutdown,
}

impl From<PageReconstructError> for BasebackupError {
    fn from(value: PageReconstructError) -> Self {
        match value {
            PageReconstructError::Cancelled => BasebackupError::Shutdown,
            err => BasebackupError::Server(err.into()),
        }
    }
}

impl From<GetVectoredError> for BasebackupError {
    fn from(value: GetVectoredError) -> Self {
        match value {
            GetVectoredError::Cancelled => BasebackupError::Shutdown,
            err => BasebackupError::Server(err.into()),
        }
    }
}

impl From<BasebackupError> for postgres_backend::QueryError {
    fn from(err: BasebackupError) -> Self {
        use postgres_backend::QueryError;
        use pq_proto::framed::ConnectionError;
        match err {
            BasebackupError::Client(err, _) => QueryError::Disconnected(ConnectionError::Io(err)),
            BasebackupError::Server(err) => QueryError::Other(err),
            BasebackupError::Shutdown => QueryError::Shutdown,
        }
    }
}

impl From<BasebackupError> for tonic::Status {
    fn from(err: BasebackupError) -> Self {
        use tonic::Code;
        let code = match &err {
            BasebackupError::Client(_, _) => Code::Cancelled,
            BasebackupError::Server(_) => Code::Internal,
            BasebackupError::Shutdown => Code::Unavailable,
        };
        tonic::Status::new(code, err.to_string())
    }
}

/// Create basebackup with non-rel data in it.
/// Only include relational data if 'full_backup' is true.
///
/// Currently we use empty 'req_lsn' in two cases:
///  * During the basebackup right after timeline creation
///  * When working without safekeepers. In this situation it is important to match the lsn
///    we are taking basebackup on with the lsn that is used in pageserver's walreceiver
///    to start the replication.
#[allow(clippy::too_many_arguments)]
pub async fn send_basebackup_tarball<'a, W>(
    write: &'a mut W,
    timeline: &'a Timeline,
    req_lsn: Option<Lsn>,
    prev_lsn: Option<Lsn>,
    full_backup: bool,
    replica: bool,
    gzip_level: Option<async_compression::Level>,
    ctx: &'a RequestContext,
) -> Result<(), BasebackupError>
where
    W: AsyncWrite + Send + Sync + Unpin,
{
    // Compute postgres doesn't have any previous WAL files, but the first
    // record that it's going to write needs to include the LSN of the
    // previous record (xl_prev). We include prev_record_lsn in the
    // "neon.signal" file, so that postgres can read it during startup.
    //
    // We don't keep full history of record boundaries in the page server,
    // however, only the predecessor of the latest record on each
    // timeline. So we can only provide prev_record_lsn when you take a
    // base backup at the end of the timeline, i.e. at last_record_lsn.
    // Even at the end of the timeline, we sometimes don't have a valid
    // prev_lsn value; that happens if the timeline was just branched from
    // an old LSN and it doesn't have any WAL of its own yet. We will set
    // prev_lsn to Lsn(0) if we cannot provide the correct value.
    let (backup_prev, lsn) = if let Some(req_lsn) = req_lsn {
        // Backup was requested at a particular LSN. The caller should've
        // already checked that it's a valid LSN.

        // If the requested point is the end of the timeline, we can
        // provide prev_lsn. (get_last_record_rlsn() might return it as
        // zero, though, if no WAL has been generated on this timeline
        // yet.)
        let end_of_timeline = timeline.get_last_record_rlsn();
        if req_lsn == end_of_timeline.last {
            (end_of_timeline.prev, req_lsn)
        } else {
            (Lsn(0), req_lsn)
        }
    } else {
        // Backup was requested at end of the timeline.
        let end_of_timeline = timeline.get_last_record_rlsn();
        (end_of_timeline.prev, end_of_timeline.last)
    };

    // Consolidate the derived and the provided prev_lsn values
    let prev_record_lsn = if let Some(provided_prev_lsn) = prev_lsn {
        if backup_prev != Lsn(0) && backup_prev != provided_prev_lsn {
            return Err(BasebackupError::Server(anyhow!(
                "backup_prev {backup_prev} != provided_prev_lsn {provided_prev_lsn}"
            )));
        }
        provided_prev_lsn
    } else {
        backup_prev
    };

    info!(
        "taking basebackup lsn={lsn}, prev_lsn={prev_record_lsn} \
        (full_backup={full_backup}, replica={replica}, gzip={gzip_level:?})",
    );
    let span = info_span!("send_tarball", backup_lsn=%lsn);

    let io_concurrency = IoConcurrency::spawn_from_conf(
        timeline.conf.get_vectored_concurrent_io,
        timeline
            .gate
            .enter()
            .map_err(|_| BasebackupError::Shutdown)?,
    );

    if let Some(gzip_level) = gzip_level {
        let mut encoder = GzipEncoder::with_quality(write, gzip_level);
        Basebackup {
            ar: Builder::new_non_terminated(&mut encoder),
            timeline,
            lsn,
            prev_record_lsn,
            full_backup,
            replica,
            ctx,
            io_concurrency,
        }
        .send_tarball()
        .instrument(span)
        .await?;
        encoder
            .shutdown()
            .await
            .map_err(|err| BasebackupError::Client(err, "gzip"))?;
    } else {
        Basebackup {
            ar: Builder::new_non_terminated(write),
            timeline,
            lsn,
            prev_record_lsn,
            full_backup,
            replica,
            ctx,
            io_concurrency,
        }
        .send_tarball()
        .instrument(span)
        .await?;
    }

    Ok(())
}

/// This is short-living object only for the time of tarball creation,
/// created mostly to avoid passing a lot of parameters between various functions
/// used for constructing tarball.
struct Basebackup<'a, W>
where
    W: AsyncWrite + Send + Sync + Unpin,
{
    ar: Builder<&'a mut W>,
    timeline: &'a Timeline,
    lsn: Lsn,
    prev_record_lsn: Lsn,
    full_backup: bool,
    replica: bool,
    ctx: &'a RequestContext,
    io_concurrency: IoConcurrency,
}

/// A sink that accepts SLRU blocks ordered by key and forwards
/// full segments to the archive.
struct SlruSegmentsBuilder<'a, 'b, W>
where
    W: AsyncWrite + Send + Sync + Unpin,
{
    ar: &'a mut Builder<&'b mut W>,
    buf: Vec<u8>,
    current_segment: Option<(SlruKind, u32)>,
    total_blocks: usize,
}

impl<'a, 'b, W> SlruSegmentsBuilder<'a, 'b, W>
where
    W: AsyncWrite + Send + Sync + Unpin,
{
    fn new(ar: &'a mut Builder<&'b mut W>) -> Self {
        Self {
            ar,
            buf: Vec::new(),
            current_segment: None,
            total_blocks: 0,
        }
    }

    async fn add_block(&mut self, key: &Key, block: Bytes) -> Result<(), BasebackupError> {
        let (kind, segno, _) = key.to_slru_block()?;

        match kind {
            SlruKind::Clog => {
                if !(block.len() == BLCKSZ as usize || block.len() == BLCKSZ as usize + 8) {
                    return Err(BasebackupError::Server(anyhow!(
                        "invalid SlruKind::Clog record: block.len()={}",
                        block.len()
                    )));
                }
            }
            SlruKind::MultiXactMembers | SlruKind::MultiXactOffsets => {
                if block.len() != BLCKSZ as usize {
                    return Err(BasebackupError::Server(anyhow!(
                        "invalid {:?} record: block.len()={}",
                        kind,
                        block.len()
                    )));
                }
            }
        }

        let segment = (kind, segno);
        match self.current_segment {
            None => {
                self.current_segment = Some(segment);
                self.buf
                    .extend_from_slice(block.slice(..BLCKSZ as usize).as_ref());
            }
            Some(current_seg) if current_seg == segment => {
                self.buf
                    .extend_from_slice(block.slice(..BLCKSZ as usize).as_ref());
            }
            Some(_) => {
                self.flush().await?;

                self.current_segment = Some(segment);
                self.buf
                    .extend_from_slice(block.slice(..BLCKSZ as usize).as_ref());
            }
        }

        Ok(())
    }

    async fn flush(&mut self) -> Result<(), BasebackupError> {
        let nblocks = self.buf.len() / BLCKSZ as usize;
        let (kind, segno) = self.current_segment.take().unwrap();
        let segname = format!("{kind}/{segno:>04X}");
        let header = new_tar_header(&segname, self.buf.len() as u64)?;
        self.ar
            .append(&header, self.buf.as_slice())
            .await
            .map_err(|e| BasebackupError::Client(e, "flush"))?;

        self.total_blocks += nblocks;
        debug!("Added to basebackup slru {} relsize {}", segname, nblocks);

        self.buf.clear();

        Ok(())
    }

    async fn finish(mut self) -> Result<(), BasebackupError> {
        let res = if self.current_segment.is_none() || self.buf.is_empty() {
            Ok(())
        } else {
            self.flush().await
        };

        info!("Collected {} SLRU blocks", self.total_blocks);

        res
    }
}

impl<W> Basebackup<'_, W>
where
    W: AsyncWrite + Send + Sync + Unpin,
{
    async fn send_tarball(mut self) -> Result<(), BasebackupError> {
        // TODO include checksum

        // Construct the pg_control file from the persisted checkpoint and pg_control
        // information. But we only add this to the tarball at the end, so that if the
        // writing is interrupted half-way through, the resulting incomplete tarball will
        // be missing the pg_control file, which prevents PostgreSQL from starting up on
        // it. With proper error handling, you should never try to start up from an
        // incomplete basebackup in the first place, of course, but this is a nice little
        // extra safety measure.
        let checkpoint_bytes = self
            .timeline
            .get_checkpoint(self.lsn, self.ctx)
            .await
            .context("failed to get checkpoint bytes")?;
        let pg_control_bytes = self
            .timeline
            .get_control_file(self.lsn, self.ctx)
            .await
            .context("failed to get control bytes")?;
        let (pg_control_bytes, system_identifier, was_shutdown) =
            postgres_ffi::generate_pg_control(
                &pg_control_bytes,
                &checkpoint_bytes,
                self.lsn,
                self.timeline.pg_version,
            )?;

        let lazy_slru_download = self.timeline.get_lazy_slru_download() && !self.full_backup;

        let pgversion = self.timeline.pg_version;
        let subdirs = dispatch_pgversion!(pgversion, &pgv::bindings::PGDATA_SUBDIRS[..]);

        // Create pgdata subdirs structure
        for dir in subdirs.iter() {
            let header = new_tar_header_dir(dir)?;
            self.ar
                .append(&header, io::empty())
                .await
                .map_err(|e| BasebackupError::Client(e, "send_tarball"))?;
        }

        // Send config files.
        for filepath in PGDATA_SPECIAL_FILES.iter() {
            if *filepath == "pg_hba.conf" {
                let data = PG_HBA.as_bytes();
                let header = new_tar_header(filepath, data.len() as u64)?;
                self.ar
                    .append(&header, data)
                    .await
                    .map_err(|e| BasebackupError::Client(e, "send_tarball,pg_hba.conf"))?;
            } else {
                let header = new_tar_header(filepath, 0)?;
                self.ar
                    .append(&header, io::empty())
                    .await
                    .map_err(|e| BasebackupError::Client(e, "send_tarball,add_config_file"))?;
            }
        }
        if !lazy_slru_download {
            // Gather non-relational files from object storage pages.
            let slru_partitions = self
                .timeline
                .get_slru_keyspace(Version::at(self.lsn), self.ctx)
                .await?
                .partition(
                    self.timeline.get_shard_identity(),
                    self.timeline.conf.max_get_vectored_keys.get() as u64 * BLCKSZ as u64,
                    BLCKSZ as u64,
                );

            let mut slru_builder = SlruSegmentsBuilder::new(&mut self.ar);

            for part in slru_partitions.parts {
                let query = VersionedKeySpaceQuery::uniform(part, self.lsn);
                let blocks = self
                    .timeline
                    .get_vectored(query, self.io_concurrency.clone(), self.ctx)
                    .await?;

                for (key, block) in blocks {
                    let block = block?;
                    slru_builder.add_block(&key, block).await?;
                }
            }
            slru_builder.finish().await?;
        }

        let mut min_restart_lsn: Lsn = Lsn::MAX;

        let mut dbdir_cnt = 0;
        let mut rel_cnt = 0;

        // Create tablespace directories
        for ((spcnode, dbnode), has_relmap_file) in
            self.timeline.list_dbdirs(self.lsn, self.ctx).await?
        {
            self.add_dbdir(spcnode, dbnode, has_relmap_file).await?;
            dbdir_cnt += 1;
            // If full backup is requested, include all relation files.
            // Otherwise only include init forks of unlogged relations.
            let rels = self
                .timeline
                .list_rels(spcnode, dbnode, Version::at(self.lsn), self.ctx)
                .await?;
            for &rel in rels.iter() {
                rel_cnt += 1;
                // Send init fork as main fork to provide well formed empty
                // contents of UNLOGGED relations. Postgres copies it in
                // `reinit.c` during recovery.
                if rel.forknum == INIT_FORKNUM {
                    // I doubt we need _init fork itself, but having it at least
                    // serves as a marker relation is unlogged.
                    self.add_rel(rel, rel).await?;
                    self.add_rel(rel, rel.with_forknum(MAIN_FORKNUM)).await?;
                    continue;
                }

                if self.full_backup {
                    if rel.forknum == MAIN_FORKNUM && rels.contains(&rel.with_forknum(INIT_FORKNUM))
                    {
                        // skip this, will include it when we reach the init fork
                        continue;
                    }
                    self.add_rel(rel, rel).await?;
                }
            }
        }

        self.timeline
            .db_rel_count
            .store(Some(Arc::new((dbdir_cnt, rel_cnt))));

        let start_time = Instant::now();
        let aux_files = self
            .timeline
            .list_aux_files(self.lsn, self.ctx, self.io_concurrency.clone())
            .await?;
        let aux_scan_time = start_time.elapsed();
        let aux_estimated_size = aux_files
            .values()
            .map(|content| content.len())
            .sum::<usize>();
        info!(
            "Scanned {} aux files in {}ms, aux file content size = {}",
            aux_files.len(),
            aux_scan_time.as_millis(),
            aux_estimated_size
        );

        for (path, content) in aux_files {
            if path.starts_with("pg_replslot") {
                // Do not create LR slots at standby because they are not used but prevent WAL truncation
                if self.replica {
                    continue;
                }
                let offs = pg_constants::REPL_SLOT_ON_DISK_OFFSETOF_RESTART_LSN;
                let restart_lsn = Lsn(u64::from_le_bytes(
                    content[offs..offs + 8].try_into().unwrap(),
                ));
                info!("Replication slot {} restart LSN={}", path, restart_lsn);
                min_restart_lsn = Lsn::min(min_restart_lsn, restart_lsn);
            } else if path == "pg_logical/replorigin_checkpoint" {
                // replorigin_checkoint is written only on compute shutdown, so it contains
                // deteriorated values. So we generate our own version of this file for the particular LSN
                // based on information about replorigins extracted from transaction commit records.
                // In future we will not generate AUX record for "pg_logical/replorigin_checkpoint" at all,
                // but now we should handle (skip) it for backward compatibility.
                continue;
            } else if path == "pg_stat/pgstat.stat" && !was_shutdown {
                // Drop statistic in case of abnormal termination, i.e. if we're not starting from the exact LSN
                // of a shutdown checkpoint.
                continue;
            }
            let header = new_tar_header(&path, content.len() as u64)?;
            self.ar
                .append(&header, &*content)
                .await
                .map_err(|e| BasebackupError::Client(e, "send_tarball,add_aux_file"))?;
        }

        if min_restart_lsn != Lsn::MAX {
            info!(
                "Min restart LSN for logical replication is {}",
                min_restart_lsn
            );
            let data = min_restart_lsn.0.to_le_bytes();
            let header = new_tar_header("restart.lsn", data.len() as u64)?;
            self.ar
                .append(&header, &data[..])
                .await
                .map_err(|e| BasebackupError::Client(e, "send_tarball,restart.lsn"))?;
        }
        for xid in self
            .timeline
            .list_twophase_files(self.lsn, self.ctx)
            .await?
        {
            self.add_twophase_file(xid).await?;
        }
        let repl_origins = self
            .timeline
            .get_replorigins(self.lsn, self.ctx, self.io_concurrency.clone())
            .await?;
        let n_origins = repl_origins.len();
        if n_origins != 0 {
            //
            // Construct "pg_logical/replorigin_checkpoint" file based on information about replication origins
            // extracted from transaction commit record. We are using this file to pass information about replication
            // origins to compute to allow logical replication to restart from proper point.
            //
            let mut content = Vec::with_capacity(n_origins * 16 + 8);
            content.extend_from_slice(&pg_constants::REPLICATION_STATE_MAGIC.to_le_bytes());
            for (origin_id, origin_lsn) in repl_origins {
                content.extend_from_slice(&origin_id.to_le_bytes());
                content.extend_from_slice(&[0u8; 6]); // align to 8 bytes
                content.extend_from_slice(&origin_lsn.0.to_le_bytes());
            }
            let crc32 = crc32c::crc32c(&content);
            content.extend_from_slice(&crc32.to_le_bytes());
            let header = new_tar_header("pg_logical/replorigin_checkpoint", content.len() as u64)?;
            self.ar.append(&header, &*content).await.map_err(|e| {
                BasebackupError::Client(e, "send_tarball,pg_logical/replorigin_checkpoint")
            })?;
        }

        fail_point!("basebackup-before-control-file", |_| {
            Err(BasebackupError::Server(anyhow!(
                "failpoint basebackup-before-control-file"
            )))
        });

        // Last, add the pg_control file and bootstrap WAL segment.
        self.add_pgcontrol_file(pg_control_bytes, system_identifier)
            .await?;
        self.ar
            .finish()
            .await
            .map_err(|e| BasebackupError::Client(e, "send_tarball,finish"))?;
        debug!("all tarred up!");
        Ok(())
    }

    /// Add contents of relfilenode `src`, naming it as `dst`.
    async fn add_rel(&mut self, src: RelTag, dst: RelTag) -> Result<(), BasebackupError> {
        let nblocks = self
            .timeline
            .get_rel_size(src, Version::at(self.lsn), self.ctx)
            .await?;

        // If the relation is empty, create an empty file
        if nblocks == 0 {
            let file_name = dst.to_segfile_name(0);
            let header = new_tar_header(&file_name, 0)?;
            self.ar
                .append(&header, io::empty())
                .await
                .map_err(|e| BasebackupError::Client(e, "add_rel,empty"))?;
            return Ok(());
        }

        // Add a file for each chunk of blocks (aka segment)
        let mut startblk = 0;
        let mut seg = 0;
        while startblk < nblocks {
            let endblk = std::cmp::min(startblk + RELSEG_SIZE, nblocks);

            let mut segment_data: Vec<u8> = vec![];
            for blknum in startblk..endblk {
                let img = self
                    .timeline
                    // TODO: investigate using get_vectored for the entire startblk..endblk range.
                    // But this code path is not on the critical path for most basebackups (?).
                    .get(rel_block_to_key(src, blknum), self.lsn, self.ctx)
                    .await?;
                segment_data.extend_from_slice(&img[..]);
            }

            let file_name = dst.to_segfile_name(seg as u32);
            let header = new_tar_header(&file_name, segment_data.len() as u64)?;
            self.ar
                .append(&header, segment_data.as_slice())
                .await
                .map_err(|e| BasebackupError::Client(e, "add_rel,segment"))?;

            seg += 1;
            startblk = endblk;
        }

        Ok(())
    }

    //
    // Include database/tablespace directories.
    //
    // Each directory contains a PG_VERSION file, and the default database
    // directories also contain pg_filenode.map files.
    //
    async fn add_dbdir(
        &mut self,
        spcnode: u32,
        dbnode: u32,
        has_relmap_file: bool,
    ) -> Result<(), BasebackupError> {
        let relmap_img = if has_relmap_file {
            let img = self
                .timeline
                .get_relmap_file(spcnode, dbnode, Version::at(self.lsn), self.ctx)
                .await?;

            if img.len()
                != dispatch_pgversion!(self.timeline.pg_version, pgv::bindings::SIZEOF_RELMAPFILE)
            {
                return Err(BasebackupError::Server(anyhow!(
                    "img.len() != SIZE_OF_RELMAPFILE, img.len()={}",
                    img.len(),
                )));
            }

            Some(img)
        } else {
            None
        };

        if spcnode == GLOBALTABLESPACE_OID {
            let pg_version_str = self.timeline.pg_version.versionfile_string();
            let header = new_tar_header("PG_VERSION", pg_version_str.len() as u64)?;
            self.ar
                .append(&header, pg_version_str.as_bytes())
                .await
                .map_err(|e| BasebackupError::Client(e, "add_dbdir,PG_VERSION"))?;

            info!("timeline.pg_version {}", self.timeline.pg_version);

            if let Some(img) = relmap_img {
                // filenode map for global tablespace
                let header = new_tar_header("global/pg_filenode.map", img.len() as u64)?;
                self.ar
                    .append(&header, &img[..])
                    .await
                    .map_err(|e| BasebackupError::Client(e, "add_dbdir,global/pg_filenode.map"))?;
            } else {
                warn!("global/pg_filenode.map is missing");
            }
        } else {
            // User defined tablespaces are not supported. However, as
            // a special case, if a tablespace/db directory is
            // completely empty, we can leave it out altogether. This
            // makes taking a base backup after the 'tablespace'
            // regression test pass, because the test drops the
            // created tablespaces after the tests.
            //
            // FIXME: this wouldn't be necessary, if we handled
            // XLOG_TBLSPC_DROP records. But we probably should just
            // throw an error on CREATE TABLESPACE in the first place.
            if !has_relmap_file
                && self
                    .timeline
                    .list_rels(spcnode, dbnode, Version::at(self.lsn), self.ctx)
                    .await?
                    .is_empty()
            {
                return Ok(());
            }
            // User defined tablespaces are not supported
            if spcnode != DEFAULTTABLESPACE_OID {
                return Err(BasebackupError::Server(anyhow!(
                    "spcnode != DEFAULTTABLESPACE_OID, spcnode={spcnode}"
                )));
            }

            // Append dir path for each database
            let path = format!("base/{dbnode}");
            let header = new_tar_header_dir(&path)?;
            self.ar
                .append(&header, io::empty())
                .await
                .map_err(|e| BasebackupError::Client(e, "add_dbdir,base"))?;

            if let Some(img) = relmap_img {
                let dst_path = format!("base/{dbnode}/PG_VERSION");

                let pg_version_str = self.timeline.pg_version.versionfile_string();
                let header = new_tar_header(&dst_path, pg_version_str.len() as u64)?;
                self.ar
                    .append(&header, pg_version_str.as_bytes())
                    .await
                    .map_err(|e| BasebackupError::Client(e, "add_dbdir,base/PG_VERSION"))?;

                let relmap_path = format!("base/{dbnode}/pg_filenode.map");
                let header = new_tar_header(&relmap_path, img.len() as u64)?;
                self.ar
                    .append(&header, &img[..])
                    .await
                    .map_err(|e| BasebackupError::Client(e, "add_dbdir,base/pg_filenode.map"))?;
            }
        };
        Ok(())
    }

    //
    // Extract twophase state files
    //
    async fn add_twophase_file(&mut self, xid: u64) -> Result<(), BasebackupError> {
        let img = self
            .timeline
            .get_twophase_file(xid, self.lsn, self.ctx)
            .await?;

        let mut buf = BytesMut::new();
        buf.extend_from_slice(&img[..]);
        let crc = crc32c::crc32c(&img[..]);
        buf.put_u32_le(crc);
        let path = if self.timeline.pg_version < PgMajorVersion::PG17 {
            format!("pg_twophase/{xid:>08X}")
        } else {
            format!("pg_twophase/{xid:>016X}")
        };
        let header = new_tar_header(&path, buf.len() as u64)?;
        self.ar
            .append(&header, &buf[..])
            .await
            .map_err(|e| BasebackupError::Client(e, "add_twophase_file"))?;

        Ok(())
    }

    //
    // Add generated pg_control file and bootstrap WAL segment.
    // Also send neon.signal and zenith.signal file with extra bootstrap data.
    //
    async fn add_pgcontrol_file(
        &mut self,
        pg_control_bytes: Bytes,
        system_identifier: u64,
    ) -> Result<(), BasebackupError> {
        // add neon.signal file
        let mut neon_signal = String::new();
        if self.prev_record_lsn == Lsn(0) {
            if self.timeline.is_ancestor_lsn(self.lsn) {
                write!(neon_signal, "PREV LSN: none")
                    .map_err(|e| BasebackupError::Server(e.into()))?;
            } else {
                write!(neon_signal, "PREV LSN: invalid")
                    .map_err(|e| BasebackupError::Server(e.into()))?;
            }
        } else {
            write!(neon_signal, "PREV LSN: {}", self.prev_record_lsn)
                .map_err(|e| BasebackupError::Server(e.into()))?;
        }

        // TODO: Remove zenith.signal once all historical computes have been replaced
        // ... and thus support the neon.signal file.
        for signalfilename in ["neon.signal", "zenith.signal"] {
            self.ar
                .append(
                    &new_tar_header(signalfilename, neon_signal.len() as u64)?,
                    neon_signal.as_bytes(),
                )
                .await
                .map_err(|e| BasebackupError::Client(e, "add_pgcontrol_file,neon.signal"))?;
        }

        //send pg_control
        let header = new_tar_header("global/pg_control", pg_control_bytes.len() as u64)?;
        self.ar
            .append(&header, &pg_control_bytes[..])
            .await
            .map_err(|e| BasebackupError::Client(e, "add_pgcontrol_file,pg_control"))?;

        //send wal segment
        let segno = self.lsn.segment_number(WAL_SEGMENT_SIZE);
        let wal_file_name = XLogFileName(PG_TLI, segno, WAL_SEGMENT_SIZE);
        let wal_file_path = format!("pg_wal/{wal_file_name}");
        let header = new_tar_header(&wal_file_path, WAL_SEGMENT_SIZE as u64)?;

        let wal_seg = postgres_ffi::generate_wal_segment(
            segno,
            system_identifier,
            self.timeline.pg_version,
            self.lsn,
        )
        .map_err(|e| anyhow!(e).context("Failed generating wal segment"))?;
        if wal_seg.len() != WAL_SEGMENT_SIZE {
            return Err(BasebackupError::Server(anyhow!(
                "wal_seg.len() != WAL_SEGMENT_SIZE, wal_seg.len()={}",
                wal_seg.len()
            )));
        }
        self.ar
            .append(&header, &wal_seg[..])
            .await
            .map_err(|e| BasebackupError::Client(e, "add_pgcontrol_file,wal_segment"))?;
        Ok(())
    }
}

//
// Create new tarball entry header
//
fn new_tar_header(path: &str, size: u64) -> anyhow::Result<Header> {
    let mut header = Header::new_gnu();
    header.set_size(size);
    header.set_path(path)?;
    header.set_mode(0b110000000); // -rw-------
    header.set_mtime(
        // use currenttime as last modified time
        SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_secs(),
    );
    header.set_cksum();
    Ok(header)
}

fn new_tar_header_dir(path: &str) -> anyhow::Result<Header> {
    let mut header = Header::new_gnu();
    header.set_size(0);
    header.set_path(path)?;
    header.set_mode(0o755); // -rw-------
    header.set_entry_type(EntryType::dir());
    header.set_mtime(
        // use currenttime as last modified time
        SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_secs(),
    );
    header.set_cksum();
    Ok(header)
}
