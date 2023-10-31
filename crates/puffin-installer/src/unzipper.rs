use std::cmp::Reverse;
use std::path::Path;

use anyhow::Result;
use rayon::iter::ParallelBridge;
use rayon::iter::ParallelIterator;
use tracing::debug;
use zip::ZipArchive;

use pep440_rs::Version;
use puffin_distribution::CachedDistribution;
use puffin_package::package_name::PackageName;

use crate::cache::WheelCache;
use crate::downloader::InMemoryDistribution;
use crate::vendor::CloneableSeekableReader;

#[derive(Default)]
pub struct Unzipper {
    reporter: Option<Box<dyn Reporter>>,
}

impl Unzipper {
    /// Set the [`Reporter`] to use for this unzipper.
    #[must_use]
    pub fn with_reporter(self, reporter: impl Reporter + 'static) -> Self {
        Self {
            reporter: Some(Box::new(reporter)),
        }
    }

    /// Unzip a set of downloaded wheels.
    pub async fn unzip(
        &self,
        downloads: Vec<InMemoryDistribution>,
        target: &Path,
    ) -> Result<Vec<CachedDistribution>> {
        // Create the wheel cache subdirectory, if necessary.
        let wheel_cache = WheelCache::new(target);
        wheel_cache.init()?;

        // Sort the wheels by size.
        let mut downloads = downloads;
        downloads.sort_unstable_by_key(|wheel| Reverse(wheel.buffer.len()));

        let staging = tempfile::tempdir_in(wheel_cache.root())?;

        // Unpack the wheels into the cache.
        let mut wheels = Vec::with_capacity(downloads.len());
        for download in downloads {
            let remote = download.remote.clone();

            debug!("Unpacking wheel: {}", remote.file().filename);

            // Unzip the wheel.
            tokio::task::spawn_blocking({
                let target = staging.path().join(remote.id());
                move || unzip_wheel(download, &target)
            })
            .await??;

            // Write the unzipped wheel to the target directory.
            let result = fs_err::tokio::rename(
                staging.path().join(remote.id()),
                wheel_cache.entry(&remote.id()),
            )
            .await;

            if let Err(err) = result {
                // If the renaming failed because another instance was faster, that's fine
                // (`DirectoryNotEmpty` is not stable so we can't match on it)
                if !wheel_cache.entry(&remote.id()).is_dir() {
                    return Err(err.into());
                }
            }

            wheels.push(CachedDistribution::new(
                remote.name().clone(),
                remote.version().clone(),
                wheel_cache.entry(&remote.id()),
            ));

            if let Some(reporter) = self.reporter.as_ref() {
                reporter.on_unzip_progress(remote.name(), remote.version());
            }
        }

        if let Some(reporter) = self.reporter.as_ref() {
            reporter.on_unzip_complete();
        }

        Ok(wheels)
    }
}

/// Unzip a wheel into the target directory.
fn unzip_wheel(wheel: InMemoryDistribution, target: &Path) -> Result<()> {
    // Read the wheel into a buffer.
    let reader = std::io::Cursor::new(wheel.buffer);
    let archive = ZipArchive::new(CloneableSeekableReader::new(reader))?;

    // Unzip in parallel.
    (0..archive.len())
        .par_bridge()
        .map(|file_number| {
            let mut archive = archive.clone();
            let mut file = archive.by_index(file_number)?;

            // Determine the path of the file within the wheel.
            let file_path = match file.enclosed_name() {
                Some(path) => path.to_owned(),
                None => return Ok(()),
            };

            // Create necessary parent directories.
            let path = target.join(file_path);
            if file.is_dir() {
                fs_err::create_dir_all(path)?;
                return Ok(());
            }
            if let Some(parent) = path.parent() {
                fs_err::create_dir_all(parent)?;
            }

            // Write the file.
            let mut outfile = fs_err::File::create(&path)?;
            std::io::copy(&mut file, &mut outfile)?;

            // Set permissions.
            #[cfg(unix)]
            {
                use std::fs::Permissions;
                use std::os::unix::fs::PermissionsExt;

                if let Some(mode) = file.unix_mode() {
                    std::fs::set_permissions(&path, Permissions::from_mode(mode))?;
                }
            }

            Ok(())
        })
        .collect::<Result<_>>()
}

pub trait Reporter: Send + Sync {
    /// Callback to invoke when a wheel is unzipped.
    fn on_unzip_progress(&self, name: &PackageName, version: &Version);

    /// Callback to invoke when the operation is complete.
    fn on_unzip_complete(&self);
}
