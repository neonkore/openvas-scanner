use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

use async_trait::async_trait;
use nasl_interpreter::FSPluginLoader;
use notus::loader::{hashsum::HashsumAdvisoryLoader, AdvisoryLoader};
use redis_storage::{
    CacheDispatcher, RedisCtx, RedisGetNvt, RedisWrapper, FEEDUPDATE_SELECTOR, NOTUSUPDATE_SELECTOR,
};
use storage::{item::PerItemDispatcher, Dispatcher, Field};
use tokio::sync::RwLock;

use crate::controller::ClientHash;
use models::scanner::ScanResults;

use super::{AppendFetchResult, Error, NVTStorer, ProgressGetter, ScanIDClientMapper, ScanStorer};

pub struct Storage<T> {
    hash: RwLock<String>,
    nasl_feed_path: Arc<PathBuf>,
    notus_feed_path: Arc<PathBuf>,
    url: String,
    underlying: T,
}

impl<T> Storage<T> {
    pub fn new<P>(underlying: T, url: String, nasl_feed_path: P, notus_feed_path: P) -> Storage<T>
    where
        P: AsRef<Path>,
    {
        Storage {
            hash: RwLock::new(String::new()),
            nasl_feed_path: Arc::new(nasl_feed_path.as_ref().to_path_buf()),
            notus_feed_path: Arc::new(notus_feed_path.as_ref().to_path_buf()),
            url,
            underlying,
        }
    }
}

#[async_trait]
impl<T> ScanIDClientMapper for Storage<T>
where
    T: super::Storage + std::marker::Sync,
{
    async fn add_scan_client_id(
        &self,
        scan_id: String,
        client_id: ClientHash,
    ) -> Result<(), Error> {
        self.underlying.add_scan_client_id(scan_id, client_id).await
    }
    async fn remove_scan_id<I>(&self, scan_id: I) -> Result<(), Error>
    where
        I: AsRef<str> + Send + 'static,
    {
        self.underlying.remove_scan_id(scan_id).await
    }

    async fn get_scans_of_client_id(&self, client_id: &ClientHash) -> Result<Vec<String>, Error> {
        self.underlying.get_scans_of_client_id(client_id).await
    }
}

#[async_trait]
impl<T> ProgressGetter for Storage<T>
where
    T: super::Storage + std::marker::Sync,
{
    async fn get_scan(&self, id: &str) -> Result<(models::Scan, models::Status), Error> {
        self.underlying.get_scan(id).await
    }

    async fn get_decrypted_scan(&self, id: &str) -> Result<(models::Scan, models::Status), Error> {
        self.underlying.get_decrypted_scan(id).await
    }

    async fn get_scan_ids(&self) -> Result<Vec<String>, Error> {
        self.underlying.get_scan_ids().await
    }

    async fn get_status(&self, id: &str) -> Result<models::Status, Error> {
        self.underlying.get_status(id).await
    }

    async fn get_results(
        &self,
        id: &str,
        from: Option<usize>,
        to: Option<usize>,
    ) -> Result<Box<dyn Iterator<Item = Vec<u8>> + Send>, Error> {
        self.underlying.get_results(id, from, to).await
    }
}

impl From<redis_storage::dberror::DbError> for super::Error {
    fn from(value: redis_storage::dberror::DbError) -> Self {
        super::Error::Storage(Box::new(value))
    }
}

impl From<storage::StorageError> for super::Error {
    fn from(value: storage::StorageError) -> Self {
        super::Error::Storage(Box::new(value))
    }
}

#[async_trait]
impl<T> NVTStorer for Storage<T>
where
    T: super::Storage + std::marker::Sync,
{
    async fn synchronize_feeds(&self, hash: String) -> Result<(), Error> {
        let mut h = self.hash.write().await;
        *h = hash;
        let url = Arc::new(self.url.to_string());
        let notus_feed_path = self.notus_feed_path.clone();
        let notus_feed = tokio::task::spawn_blocking(move || {
            tracing::debug!("starting notus feed update");
            let loader = FSPluginLoader::new(notus_feed_path.as_ref());
            let advisories_files = HashsumAdvisoryLoader::new(loader.clone())?;

            let redis_cache: CacheDispatcher<RedisCtx, String> =
                redis_storage::CacheDispatcher::init(&url, NOTUSUPDATE_SELECTOR)?;
            let store = PerItemDispatcher::new(redis_cache);
            for filename in advisories_files.get_advisories()?.iter() {
                let advisories = advisories_files.load_advisory(filename)?;

                for adv in advisories.advisories {
                    let data = models::VulnerabilityData {
                        adv,
                        famile: advisories.family.clone(),
                        filename: filename.to_owned(),
                    };
                    store.dispatch(&"".to_string(), Field::NotusAdvisory(Box::new(Some(data))))?;
                }
            }
            store.dispatch(&"".to_string(), Field::NotusAdvisory(Box::new(None)))?;
            tracing::debug!("finished notus feed update");
            Ok(())
        });

        let nasl_feed_path = self.nasl_feed_path.clone();
        let url = Arc::new(self.url.to_string());
        let active_feed = tokio::task::spawn_blocking(move || {
            tracing::debug!("starting nasl feed update");
            let oversion = "0.1";
            let loader = FSPluginLoader::new(nasl_feed_path.as_ref());
            let verifier = feed::HashSumNameLoader::sha256(&loader)?;

            let redis_cache: CacheDispatcher<RedisCtx, String> =
                redis_storage::CacheDispatcher::init(&url, FEEDUPDATE_SELECTOR)?;
            let store = PerItemDispatcher::new(redis_cache);
            let mut fu = feed::Update::init(oversion, 5, loader.clone(), store, verifier);
            if let Some(x) = fu.find_map(|x| x.err()) {
                Err(Error::from(x))
            } else {
                tracing::debug!("finished nasl feed update");
                Ok(())
            }
        });

        let nasl_result = active_feed.await.unwrap();
        let notus_result: Result<(), Error> = notus_feed.await.unwrap();
        tracing::debug!("finished feed update");

        match (nasl_result, notus_result) {
            (Ok(_), Ok(_)) => Ok(()),
            (Ok(_), Err(err)) => Err(err),
            (Err(err), Ok(_)) => Err(err),
            (Err(err), Err(_)) => Err(err),
        }
    }

    async fn vt_by_oid(&self, oid: &str) -> Result<Option<storage::item::Nvt>, Error> {
        let url = self.url.to_string();
        let aoid = oid.to_owned();
        let nr = tokio::task::spawn_blocking(move || {
            let mut notus_redis = RedisCtx::open(&url, NOTUSUPDATE_SELECTOR)?;
            notus_redis.redis_get_advisory(&aoid)
        })
        .await
        .unwrap()?;
        if nr.is_some() {
            return Ok(nr);
        }

        let aoid = oid.to_owned();
        let url = self.url.to_string();
        let nr = tokio::task::spawn_blocking(move || {
            let mut nvt_redis = RedisCtx::open(&url, FEEDUPDATE_SELECTOR)?;
            nvt_redis.redis_get_advisory(&aoid)
        })
        .await
        .unwrap()?;
        Ok(nr)
    }
    async fn vts<'a>(
        &self,
    ) -> Result<Box<dyn Iterator<Item = storage::item::Nvt> + Send + 'a>, Error> {
        let url = self.url.to_string();
        let noids = tokio::task::spawn_blocking(move || {
            let mut notus_redis = RedisCtx::open(&url, NOTUSUPDATE_SELECTOR)?;
            let noids = notus_redis
                .keys("internal*")?
                .into_iter()
                .filter_map(|x| x.split('/').last().map(|x| x.to_string()))
                .filter_map(move |oid| notus_redis.redis_get_advisory(&oid).ok())
                .flatten();
            Ok::<_, Error>(noids)
        });

        let url = self.url.to_string();
        let foids = tokio::task::spawn_blocking(move || {
            let mut nvt_redis = RedisCtx::open(&url, FEEDUPDATE_SELECTOR)?;
            let foids = nvt_redis
                .keys("nvt:*")?
                .into_iter()
                .filter_map(|x| x.split('/').last().map(|x| x.to_string()))
                .filter_map(move |oid| nvt_redis.redis_get_vt(&oid).ok())
                .flatten();
            Ok::<_, Error>(foids)
        });

        let noids = noids.await.unwrap()?;
        let foids = foids.await.unwrap()?;
        let results = noids.chain(foids);
        Ok(Box::new(results))
    }

    async fn oids(&self) -> Result<Box<dyn Iterator<Item = String> + Send>, Error> {
        let url = Arc::new(self.url.to_string());
        let noids = tokio::task::spawn_blocking(move || {
            let mut notus_redis = RedisCtx::open(&url, NOTUSUPDATE_SELECTOR)?;
            let noids = notus_redis
                .keys("internal*")?
                .into_iter()
                .filter_map(|x| x.split('/').last().map(|x| x.to_string()));
            Ok::<_, Error>(noids)
        });

        let url = Arc::new(self.url.to_string());
        let foids = tokio::task::spawn_blocking(move || {
            let mut nvt_redis = RedisCtx::open(&url, FEEDUPDATE_SELECTOR)?;
            let foids = nvt_redis
                .keys("nvt:*")?
                .into_iter()
                .filter_map(|x| x.split('/').last().map(|x| x.to_string()));
            Ok::<_, Error>(foids)
        });

        let noids = noids.await.unwrap()?;
        let foids = foids.await.unwrap()?;
        let results = noids.chain(foids);
        Ok(Box::new(results))
    }

    async fn feed_hash(&self) -> String {
        self.hash.read().await.to_string()
    }
}

#[async_trait]
impl<T> ScanStorer for Storage<T>
where
    T: super::Storage + std::marker::Sync,
{
    async fn insert_scan(&self, t: models::Scan) -> Result<(), Error> {
        self.underlying.insert_scan(t).await
    }
    async fn remove_scan(&self, id: &str) -> Result<(), Error> {
        self.underlying.remove_scan(id).await
    }
    async fn update_status(&self, id: &str, status: models::Status) -> Result<(), Error> {
        self.underlying.update_status(id, status).await
    }
}

#[async_trait]
impl<T> AppendFetchResult for Storage<T>
where
    T: super::Storage + std::marker::Sync,
{
    async fn append_fetched_result(&self, results: Vec<ScanResults>) -> Result<(), Error> {
        self.underlying.append_fetched_result(results).await
    }
}
