//! The gRPC Search service: a thin mapping between the proto types and the
//! store's query layer. Read-only.

use std::sync::{Arc, Mutex};

use tonic::{Request, Response, Status};

use hashit_core::manifest::ns_to_rfc3339;

use crate::pb::search_server::Search;
use crate::pb::{
    FileResult, QueryRequest, QueryResponse, StatsRequest, StatsResponse,
};
use crate::store::{QueryParams, Store};

pub struct SearchService {
    pub store: Arc<Mutex<Store>>,
}

#[tonic::async_trait]
impl Search for SearchService {
    async fn query(
        &self,
        request: Request<QueryRequest>,
    ) -> Result<Response<QueryResponse>, Status> {
        let r = request.into_inner();
        let params = QueryParams {
            name: r.name,
            hash: r.hash,
            min_size: nonzero(r.min_size),
            max_size: nonzero(r.max_size),
            min_mtime_ns: parse_rfc3339_ns(&r.min_date),
            max_mtime_ns: parse_rfc3339_ns(&r.max_date),
            tags: r
                .tags
                .into_iter()
                .map(|t| {
                    let value = if t.value.is_empty() { None } else { Some(t.value) };
                    (t.key, value)
                })
                .collect(),
            limit: r.limit,
            offset: r.offset,
        };

        // The query is synchronous and fast; run it while briefly holding the
        // lock, never across an await.
        let (rows, total) = {
            let store = self
                .store
                .lock()
                .map_err(|_| Status::internal("index lock poisoned"))?;
            store
                .query(&params)
                .map_err(|e| Status::internal(format!("{e:#}")))?
        };

        let files = rows
            .into_iter()
            .map(|f| FileResult {
                path: f.path,
                name: f.name,
                hash: f.hash,
                algo: f.algo,
                size: f.size,
                mtime: ns_to_rfc3339(f.mtime_ns),
                file_type: f.file_type.unwrap_or_default(),
                ext: f.ext.unwrap_or_default(),
            })
            .collect();
        Ok(Response::new(QueryResponse { files, total }))
    }

    async fn stats(
        &self,
        _request: Request<StatsRequest>,
    ) -> Result<Response<StatsResponse>, Status> {
        let (files, hashes, tags) = {
            let store = self
                .store
                .lock()
                .map_err(|_| Status::internal("index lock poisoned"))?;
            store
                .stats()
                .map_err(|e| Status::internal(format!("{e:#}")))?
        };
        Ok(Response::new(StatsResponse {
            files,
            hashes,
            tags,
        }))
    }
}

/// Treat 0 as "unset" for the size bounds.
fn nonzero(v: u64) -> Option<u64> {
    (v != 0).then_some(v)
}

/// Parse an RFC3339 timestamp into nanoseconds since the epoch (None if empty
/// or unparseable).
fn parse_rfc3339_ns(s: &str) -> Option<u64> {
    if s.is_empty() {
        return None;
    }
    chrono::DateTime::parse_from_rfc3339(s)
        .ok()
        .and_then(|dt| dt.timestamp_nanos_opt())
        .map(|ns| ns.max(0) as u64)
}
