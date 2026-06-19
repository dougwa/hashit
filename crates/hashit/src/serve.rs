//! The `watch --serve` gRPC FileOps server.
//!
//! Every RPC routes through the exact same core file-ops and metadata code the
//! CLI uses, so the manifests and metadata files stay consistent regardless of
//! whether a change came from the command line or a client. Bound to localhost
//! only; this is a mutation API (reads/search are `hashit-idx`'s job).

use std::net::SocketAddr;
use std::path::PathBuf;

use tonic::{transport::Server, Request, Response, Status};

use hashit_core::fileops::{self, OpOptions};
use hashit_core::hash::HashAlgo;
use hashit_core::scan::ScanOptions;

use crate::metacmd::{self, MetaView};
use crate::pb_fileops::file_ops_server::{FileOps, FileOpsServer};
use crate::pb_fileops::{
    CpRequest, GetMetaRequest, GetMetaResponse, MetaEntry, MvRequest, OpResponse, PutMetaRequest,
    RmRequest,
};

/// Configuration captured once at server start.
pub struct ServeConfig {
    pub meta_folder: PathBuf,
    pub algo: HashAlgo,
}

struct FileOpsService {
    cfg: ServeConfig,
}

impl FileOpsService {
    fn scan_opts(&self) -> ScanOptions {
        crate::fileop_scan_opts(self.cfg.algo, true, Vec::new())
    }
}

fn to_status(e: anyhow::Error) -> Status {
    Status::internal(format!("{e:#}"))
}

fn paths(strings: &[String]) -> Vec<PathBuf> {
    strings.iter().map(PathBuf::from).collect()
}

#[tonic::async_trait]
impl FileOps for FileOpsService {
    async fn cp(&self, req: Request<CpRequest>) -> Result<Response<OpResponse>, Status> {
        let r = req.into_inner();
        if r.sources.is_empty() {
            return Err(Status::invalid_argument("no sources given"));
        }
        let mut p = paths(&r.sources);
        p.push(PathBuf::from(&r.dest));
        let opts = OpOptions {
            recursive: r.recursive,
            force: r.force,
            quiet: true,
        };
        fileops::cp(&p, &opts, &self.scan_opts()).map_err(to_status)?;
        Ok(Response::new(OpResponse {
            message: format!("copied {} item(s) to {}", r.sources.len(), r.dest),
        }))
    }

    async fn mv(&self, req: Request<MvRequest>) -> Result<Response<OpResponse>, Status> {
        let r = req.into_inner();
        if r.sources.is_empty() {
            return Err(Status::invalid_argument("no sources given"));
        }
        let mut p = paths(&r.sources);
        p.push(PathBuf::from(&r.dest));
        let opts = OpOptions {
            recursive: true,
            force: r.force,
            quiet: true,
        };
        fileops::mv(&p, &opts, &self.scan_opts()).map_err(to_status)?;
        Ok(Response::new(OpResponse {
            message: format!("moved {} item(s) to {}", r.sources.len(), r.dest),
        }))
    }

    async fn rm(&self, req: Request<RmRequest>) -> Result<Response<OpResponse>, Status> {
        let r = req.into_inner();
        let opts = OpOptions {
            recursive: r.recursive,
            force: r.force,
            quiet: true,
        };
        fileops::rm(&paths(&r.paths), &opts, &self.scan_opts()).map_err(to_status)?;
        Ok(Response::new(OpResponse {
            message: format!("removed {} path(s)", r.paths.len()),
        }))
    }

    async fn get_meta(
        &self,
        req: Request<GetMetaRequest>,
    ) -> Result<Response<GetMetaResponse>, Status> {
        let r = req.into_inner();
        let views = metacmd::gather(&self.cfg.meta_folder, &paths(&r.paths)).map_err(to_status)?;
        let entries = views.into_iter().map(view_to_entry).collect();
        Ok(Response::new(GetMetaResponse { entries }))
    }

    async fn put_meta(&self, req: Request<PutMetaRequest>) -> Result<Response<OpResponse>, Status> {
        let r = req.into_inner();
        let sets: Vec<(String, String)> = r.set.into_iter().collect();
        let updated = metacmd::apply_put(&self.cfg.meta_folder, &paths(&r.paths), &sets, &r.remove)
            .map_err(to_status)?;
        Ok(Response::new(OpResponse {
            message: format!("updated {} hash(es)", updated.len()),
        }))
    }
}

fn view_to_entry(v: MetaView) -> MetaEntry {
    MetaEntry {
        path: v.path,
        error: v.error.unwrap_or_default(),
        hash: v.hash.unwrap_or_default(),
        algo: v.algo.unwrap_or_default(),
        size: v.size.unwrap_or_default(),
        file_type: v.file_type.unwrap_or_default(),
        ext: v.ext.unwrap_or_default(),
        tags: v.tags.into_iter().collect(),
        properties: v.properties.into_iter().collect(),
        thumbnail: v.thumbnail.unwrap_or_default(),
        preview: v.preview.unwrap_or_default(),
    }
}

/// Serve the FileOps API on `addr` until the process exits.
pub async fn serve(addr: SocketAddr, cfg: ServeConfig) -> anyhow::Result<()> {
    Server::builder()
        .add_service(FileOpsServer::new(FileOpsService { cfg }))
        .serve(addr)
        .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::{BTreeMap, HashMap};
    use std::time::Duration;

    use hashit_core::manifest::{FileEntry, Manifest};

    use crate::pb_fileops::file_ops_client::FileOpsClient;
    use crate::pb_fileops::{GetMetaRequest, PutMetaRequest};

    fn unique_dir(tag: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let p =
            std::env::temp_dir().join(format!("hashit-serve-{tag}-{}-{nanos}", std::process::id()));
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    /// Drive the FileOps server in-process: set a property over gRPC, then read
    /// it back. Validates the full tonic server/client + handler path.
    #[tokio::test(flavor = "multi_thread")]
    async fn put_then_get_meta_roundtrip() {
        let root = unique_dir("root");
        let meta_folder = unique_dir("meta");
        std::fs::write(root.join("a.txt"), b"hello").unwrap();

        let mut files = BTreeMap::new();
        files.insert(
            "a.txt".to_string(),
            FileEntry {
                size: 5,
                mtime_ns: 1,
                hash: "abcd1234ef567890".to_string(),
                algo: "blake3".to_string(),
                flags: vec![],
                hashed_at: "t".to_string(),
            },
        );
        Manifest::new(files).save(&root).unwrap();

        // Reserve a free port, then serve on it.
        let port = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let addr: SocketAddr = ([127, 0, 0, 1], port).into();
        tokio::spawn(serve(
            addr,
            ServeConfig {
                meta_folder,
                algo: HashAlgo::Blake3,
            },
        ));

        // Connect (retry until the server is accepting).
        let url = format!("http://127.0.0.1:{port}");
        let mut client = loop {
            match FileOpsClient::connect(url.clone()).await {
                Ok(c) => break c,
                Err(_) => tokio::time::sleep(Duration::from_millis(50)).await,
            }
        };

        let path = root.join("a.txt").display().to_string();

        client
            .put_meta(PutMetaRequest {
                paths: vec![path.clone()],
                set: HashMap::from([("rating".to_string(), "5".to_string())]),
                remove: vec![],
            })
            .await
            .unwrap();

        let resp = client
            .get_meta(GetMetaRequest {
                paths: vec![path.clone()],
            })
            .await
            .unwrap()
            .into_inner();

        assert_eq!(resp.entries.len(), 1);
        let e = &resp.entries[0];
        assert_eq!(e.hash, "abcd1234ef567890");
        assert_eq!(e.properties.get("rating").map(String::as_str), Some("5"));

        std::fs::remove_dir_all(&root).ok();
    }
}
