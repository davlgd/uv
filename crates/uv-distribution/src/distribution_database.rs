use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use futures::{FutureExt, TryStreamExt};
use tokio::io::AsyncSeekExt;
use tokio_util::compat::FuturesAsyncReadCompatExt;
use tracing::{info_span, instrument, warn, Instrument};
use url::Url;

use distribution_filename::WheelFilename;
use distribution_types::{
    BuildableSource, BuiltDist, Dist, FileLocation, IndexLocations, LocalEditable, Name, SourceDist,
};
use platform_tags::Tags;
use pypi_types::Metadata23;
use uv_cache::{ArchiveTarget, ArchiveTimestamp, CacheBucket, CacheEntry, WheelCache};
use uv_client::{CacheControl, CachedClientError, Connectivity, RegistryClient};
use uv_types::{BuildContext, NoBinary, NoBuild};

use crate::download::{BuiltWheel, UnzippedWheel};
use crate::locks::Locks;
use crate::{DiskWheel, Error, LocalWheel, Reporter, SourceDistributionBuilder};

/// A cached high-level interface to convert distributions (a requirement resolved to a location)
/// to a wheel or wheel metadata.
///
/// For wheel metadata, this happens by either fetching the metadata from the remote wheel or by
/// building the source distribution. For wheel files, either the wheel is downloaded or a source
/// distribution is downloaded, built and the new wheel gets returned.
///
/// All kinds of wheel sources (index, url, path) and source distribution source (index, url, path,
/// git) are supported.
///
/// This struct also has the task of acquiring locks around source dist builds in general and git
/// operation especially.
pub struct DistributionDatabase<'a, Context: BuildContext + Send + Sync> {
    client: &'a RegistryClient,
    build_context: &'a Context,
    builder: SourceDistributionBuilder<'a, Context>,
    locks: Arc<Locks>,
}

impl<'a, Context: BuildContext + Send + Sync> DistributionDatabase<'a, Context> {
    pub fn new(client: &'a RegistryClient, build_context: &'a Context) -> Self {
        Self {
            client,
            build_context,
            builder: SourceDistributionBuilder::new(client, build_context),
            locks: Arc::new(Locks::default()),
        }
    }

    /// Set the [`Reporter`] to use for this source distribution fetcher.
    #[must_use]
    pub fn with_reporter(self, reporter: impl Reporter + 'static) -> Self {
        let reporter = Arc::new(reporter);
        Self {
            builder: self.builder.with_reporter(reporter),
            ..self
        }
    }

    /// Handle a specific `reqwest` error, and convert it to [`io::Error`].
    fn handle_response_errors(&self, err: reqwest::Error) -> io::Error {
        if err.is_timeout() {
            io::Error::new(
                io::ErrorKind::TimedOut,
                format!(
                    "Failed to download distribution due to network timeout. Try increasing UV_HTTP_TIMEOUT (current value: {}s).",  self.client.timeout()
                ),
            )
        } else {
            io::Error::new(io::ErrorKind::Other, err)
        }
    }

    /// Either fetch the wheel or fetch and build the source distribution
    ///
    /// If `no_remote_wheel` is set, the wheel will be built from a source distribution
    /// even if compatible pre-built wheels are available.
    #[instrument(skip_all, fields(%dist))]
    pub async fn get_or_build_wheel(&self, dist: &Dist, tags: &Tags) -> Result<LocalWheel, Error> {
        match dist {
            Dist::Built(built) => self.get_wheel(built).await,
            Dist::Source(source) => self.build_wheel(source, tags).await,
        }
    }

    /// Either fetch the only wheel metadata (directly from the index or with range requests) or
    /// fetch and build the source distribution.
    ///
    /// Returns the [`Metadata23`], along with a "precise" URL for the source distribution, if
    /// possible. For example, given a Git dependency with a reference to a branch or tag, return a
    /// URL with a precise reference to the current commit of that branch or tag.
    #[instrument(skip_all, fields(%dist))]
    pub async fn get_or_build_wheel_metadata(&self, dist: &Dist) -> Result<Metadata23, Error> {
        match dist {
            Dist::Built(built) => self.get_wheel_metadata(built).await,
            Dist::Source(source) => {
                self.build_wheel_metadata(&BuildableSource::Dist(source))
                    .await
            }
        }
    }

    /// Build a directory into an editable wheel.
    pub async fn build_wheel_editable(
        &self,
        editable: &LocalEditable,
        editable_wheel_dir: &Path,
    ) -> Result<(LocalWheel, Metadata23), Error> {
        let (dist, disk_filename, filename, metadata) = self
            .builder
            .build_editable(editable, editable_wheel_dir)
            .await?;

        let built_wheel = BuiltWheel {
            dist,
            filename,
            path: editable_wheel_dir.join(disk_filename),
            target: editable_wheel_dir.join(cache_key::digest(&editable.path)),
        };
        Ok((LocalWheel::Built(built_wheel), metadata))
    }

    /// Fetch a wheel from the cache or download it from the index.
    async fn get_wheel(&self, dist: &BuiltDist) -> Result<LocalWheel, Error> {
        let no_binary = match self.build_context.no_binary() {
            NoBinary::None => false,
            NoBinary::All => true,
            NoBinary::Packages(packages) => packages.contains(dist.name()),
        };
        if no_binary {
            return Err(Error::NoBinary);
        }

        match dist {
            BuiltDist::Registry(wheel) => {
                let url = match &wheel.file.url {
                    FileLocation::RelativeUrl(base, url) => {
                        pypi_types::base_url_join_relative(base, url)?
                    }
                    FileLocation::AbsoluteUrl(url) => {
                        Url::parse(url).map_err(|err| Error::Url(url.clone(), err))?
                    }
                    FileLocation::Path(path) => {
                        let url = Url::from_file_path(path).expect("path is absolute");
                        let cache_entry = self.build_context.cache().entry(
                            CacheBucket::Wheels,
                            WheelCache::Url(&url).wheel_dir(wheel.name().as_ref()),
                            wheel.filename.stem(),
                        );

                        // If the file is already unzipped, and the unzipped directory is fresh,
                        // return it.
                        match cache_entry.path().canonicalize() {
                            Ok(archive) => {
                                if ArchiveTimestamp::up_to_date_with(
                                    path,
                                    ArchiveTarget::Cache(&archive),
                                )
                                .map_err(Error::CacheRead)?
                                {
                                    return Ok(LocalWheel::Unzipped(UnzippedWheel {
                                        dist: Dist::Built(dist.clone()),
                                        archive,
                                        filename: wheel.filename.clone(),
                                    }));
                                }
                            }
                            Err(err) if err.kind() == io::ErrorKind::NotFound => {}
                            Err(err) => return Err(Error::CacheRead(err)),
                        }

                        // Otherwise, unzip the file.
                        return Ok(LocalWheel::Disk(DiskWheel {
                            dist: Dist::Built(dist.clone()),
                            path: path.clone(),
                            target: cache_entry.into_path_buf(),
                            filename: wheel.filename.clone(),
                        }));
                    }
                };

                // Create a cache entry for the wheel.
                let wheel_entry = self.build_context.cache().entry(
                    CacheBucket::Wheels,
                    WheelCache::Index(&wheel.index).wheel_dir(wheel.name().as_ref()),
                    wheel.filename.stem(),
                );

                // Download and unzip.
                match self
                    .stream_wheel(url.clone(), &wheel.filename, &wheel_entry, dist)
                    .await
                {
                    Ok(archive) => Ok(LocalWheel::Unzipped(UnzippedWheel {
                        dist: Dist::Built(dist.clone()),
                        archive,
                        filename: wheel.filename.clone(),
                    })),
                    Err(Error::Extract(err)) if err.is_http_streaming_unsupported() => {
                        warn!(
                            "Streaming unsupported for {dist}; downloading wheel to disk ({err})"
                        );

                        // If the request failed because streaming is unsupported, download the
                        // wheel directly.
                        let archive = self
                            .download_wheel(url, &wheel.filename, &wheel_entry, dist)
                            .await?;
                        Ok(LocalWheel::Unzipped(UnzippedWheel {
                            dist: Dist::Built(dist.clone()),
                            archive,
                            filename: wheel.filename.clone(),
                        }))
                    }
                    Err(err) => Err(err),
                }
            }

            BuiltDist::DirectUrl(wheel) => {
                // Create a cache entry for the wheel.
                let wheel_entry = self.build_context.cache().entry(
                    CacheBucket::Wheels,
                    WheelCache::Url(&wheel.url).wheel_dir(wheel.name().as_ref()),
                    wheel.filename.stem(),
                );

                // Download and unzip.
                match self
                    .stream_wheel(wheel.url.raw().clone(), &wheel.filename, &wheel_entry, dist)
                    .await
                {
                    Ok(archive) => Ok(LocalWheel::Unzipped(UnzippedWheel {
                        dist: Dist::Built(dist.clone()),
                        archive,
                        filename: wheel.filename.clone(),
                    })),
                    Err(Error::Client(err)) if err.is_http_streaming_unsupported() => {
                        warn!(
                            "Streaming unsupported for {dist}; downloading wheel to disk ({err})"
                        );

                        // If the request failed because streaming is unsupported, download the
                        // wheel directly.
                        let archive = self
                            .download_wheel(
                                wheel.url.raw().clone(),
                                &wheel.filename,
                                &wheel_entry,
                                dist,
                            )
                            .await?;
                        Ok(LocalWheel::Unzipped(UnzippedWheel {
                            dist: Dist::Built(dist.clone()),
                            archive,
                            filename: wheel.filename.clone(),
                        }))
                    }
                    Err(err) => Err(err),
                }
            }

            BuiltDist::Path(wheel) => {
                let cache_entry = self.build_context.cache().entry(
                    CacheBucket::Wheels,
                    WheelCache::Url(&wheel.url).wheel_dir(wheel.name().as_ref()),
                    wheel.filename.stem(),
                );

                // If the file is already unzipped, and the unzipped directory is fresh,
                // return it.
                match cache_entry.path().canonicalize() {
                    Ok(archive) => {
                        if ArchiveTimestamp::up_to_date_with(
                            &wheel.path,
                            ArchiveTarget::Cache(&archive),
                        )
                        .map_err(Error::CacheRead)?
                        {
                            return Ok(LocalWheel::Unzipped(UnzippedWheel {
                                dist: Dist::Built(dist.clone()),
                                archive,
                                filename: wheel.filename.clone(),
                            }));
                        }
                    }
                    Err(err) if err.kind() == io::ErrorKind::NotFound => {}
                    Err(err) => return Err(Error::CacheRead(err)),
                }

                Ok(LocalWheel::Disk(DiskWheel {
                    dist: Dist::Built(dist.clone()),
                    path: wheel.path.clone(),
                    target: cache_entry.into_path_buf(),
                    filename: wheel.filename.clone(),
                }))
            }
        }
    }

    /// Convert a source distribution into a wheel, fetching it from the cache or building it if
    /// necessary.
    async fn build_wheel(&self, dist: &SourceDist, tags: &Tags) -> Result<LocalWheel, Error> {
        let lock = self.locks.acquire(&Dist::Source(dist.clone())).await;
        let _guard = lock.lock().await;

        let built_wheel = self
            .builder
            .download_and_build(&BuildableSource::Dist(dist), tags)
            .boxed()
            .await?;

        // If the wheel was unzipped previously, respect it. Source distributions are
        // cached under a unique build ID, so unzipped directories are never stale.
        match built_wheel.target.canonicalize() {
            Ok(archive) => Ok(LocalWheel::Unzipped(UnzippedWheel {
                dist: Dist::Source(dist.clone()),
                archive,
                filename: built_wheel.filename,
            })),
            Err(err) if err.kind() == io::ErrorKind::NotFound => {
                Ok(LocalWheel::Built(BuiltWheel {
                    dist: Dist::Source(dist.clone()),
                    path: built_wheel.path,
                    target: built_wheel.target,
                    filename: built_wheel.filename,
                }))
            }
            Err(err) => Err(Error::CacheRead(err)),
        }
    }

    /// Fetch the wheel metadata from the index, or from the cache if possible.
    pub async fn get_wheel_metadata(&self, dist: &BuiltDist) -> Result<Metadata23, Error> {
        match self.client.wheel_metadata(dist).boxed().await {
            Ok(metadata) => Ok(metadata),
            Err(err) if err.is_http_streaming_unsupported() => {
                warn!("Streaming unsupported when fetching metadata for {dist}; downloading wheel directly ({err})");

                // If the request failed due to an error that could be resolved by
                // downloading the wheel directly, try that.
                let wheel = self.get_wheel(dist).await?;
                Ok(wheel.metadata()?)
            }
            Err(err) => Err(err.into()),
        }
    }

    /// Build the wheel metadata for a source distribution, or fetch it from the cache if possible.
    pub async fn build_wheel_metadata(
        &self,
        source: &BuildableSource<'_>,
    ) -> Result<Metadata23, Error> {
        let no_build = match self.build_context.no_build() {
            NoBuild::All => true,
            NoBuild::None => false,
            NoBuild::Packages(packages) => {
                source.name().is_some_and(|name| packages.contains(name))
            }
        };

        // Optimization: Skip source dist download when we must not build them anyway.
        if no_build {
            return Err(Error::NoBuild);
        }

        let lock = self.locks.acquire(source).await;
        let _guard = lock.lock().await;

        let metadata = self
            .builder
            .download_and_build_metadata(source)
            .boxed()
            .await?;
        Ok(metadata)
    }

    /// Stream a wheel from a URL, unzipping it into the cache as it's downloaded.
    async fn stream_wheel(
        &self,
        url: Url,
        filename: &WheelFilename,
        wheel_entry: &CacheEntry,
        dist: &BuiltDist,
    ) -> Result<PathBuf, Error> {
        // Create an entry for the HTTP cache.
        let http_entry = wheel_entry.with_file(format!("{}.http", filename.stem()));

        let download = |response: reqwest::Response| {
            async {
                let reader = response
                    .bytes_stream()
                    .map_err(|err| self.handle_response_errors(err))
                    .into_async_read();

                // Download and unzip the wheel to a temporary directory.
                let temp_dir = tempfile::tempdir_in(self.build_context.cache().root())
                    .map_err(Error::CacheWrite)?;
                uv_extract::stream::unzip(reader.compat(), temp_dir.path()).await?;

                // Persist the temporary directory to the directory store.
                let archive = self
                    .build_context
                    .cache()
                    .persist(temp_dir.into_path(), wheel_entry.path())
                    .await
                    .map_err(Error::CacheRead)?;
                Ok(archive)
            }
            .instrument(info_span!("wheel", wheel = %dist))
        };

        let cache_control = match self.client.connectivity() {
            Connectivity::Online => CacheControl::from(
                self.build_context
                    .cache()
                    .freshness(&http_entry, Some(&filename.name))
                    .map_err(Error::CacheRead)?,
            ),
            Connectivity::Offline => CacheControl::AllowStale,
        };

        let archive = self
            .client
            .cached_client()
            .get_serde(self.request(url)?, &http_entry, cache_control, download)
            .await
            .map_err(|err| match err {
                CachedClientError::Callback(err) => err,
                CachedClientError::Client(err) => Error::Client(err),
            })?;

        Ok(archive)
    }

    /// Download a wheel from a URL, then unzip it into the cache.
    async fn download_wheel(
        &self,
        url: Url,
        filename: &WheelFilename,
        wheel_entry: &CacheEntry,
        dist: &BuiltDist,
    ) -> Result<PathBuf, Error> {
        // Create an entry for the HTTP cache.
        let http_entry = wheel_entry.with_file(format!("{}.http", filename.stem()));

        let download = |response: reqwest::Response| {
            async {
                let reader = response
                    .bytes_stream()
                    .map_err(|err| self.handle_response_errors(err))
                    .into_async_read();

                // Download the wheel to a temporary file.
                let temp_file = tempfile::tempfile_in(self.build_context.cache().root())
                    .map_err(Error::CacheWrite)?;
                let mut writer = tokio::io::BufWriter::new(tokio::fs::File::from_std(temp_file));
                tokio::io::copy(&mut reader.compat(), &mut writer)
                    .await
                    .map_err(Error::CacheWrite)?;

                // Unzip the wheel to a temporary directory.
                let temp_dir = tempfile::tempdir_in(self.build_context.cache().root())
                    .map_err(Error::CacheWrite)?;
                let mut file = writer.into_inner();
                file.seek(io::SeekFrom::Start(0))
                    .await
                    .map_err(Error::CacheWrite)?;
                uv_extract::seek::unzip(file, temp_dir.path()).await?;

                // Persist the temporary directory to the directory store.
                let archive = self
                    .build_context
                    .cache()
                    .persist(temp_dir.into_path(), wheel_entry.path())
                    .await
                    .map_err(Error::CacheRead)?;
                Ok(archive)
            }
            .instrument(info_span!("wheel", wheel = %dist))
        };

        let cache_control = match self.client.connectivity() {
            Connectivity::Online => CacheControl::from(
                self.build_context
                    .cache()
                    .freshness(&http_entry, Some(&filename.name))
                    .map_err(Error::CacheRead)?,
            ),
            Connectivity::Offline => CacheControl::AllowStale,
        };

        let archive = self
            .client
            .cached_client()
            .get_serde(self.request(url)?, &http_entry, cache_control, download)
            .await
            .map_err(|err| match err {
                CachedClientError::Callback(err) => err,
                CachedClientError::Client(err) => Error::Client(err),
            })?;

        Ok(archive)
    }

    /// Returns a GET [`reqwest::Request`] for the given URL.
    fn request(&self, url: Url) -> Result<reqwest::Request, reqwest::Error> {
        self.client
            .uncached_client()
            .get(url)
            .header(
                // `reqwest` defaults to accepting compressed responses.
                // Specify identity encoding to get consistent .whl downloading
                // behavior from servers. ref: https://github.com/pypa/pip/pull/1688
                "accept-encoding",
                reqwest::header::HeaderValue::from_static("identity"),
            )
            .build()
    }

    /// Return the [`IndexLocations`] used by this resolver.
    pub fn index_locations(&self) -> &IndexLocations {
        self.build_context.index_locations()
    }
}
