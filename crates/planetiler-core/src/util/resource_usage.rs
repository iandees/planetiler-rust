//! Port of `util/ResourceUsage.java`: checks that estimated resource usage (memory, disk) will not exceed limits so
//! we can fail fast before starting.
//!
//! Each method mutates the instance and returns it for chaining. The `--force` option bypasses these checks.
//!
//! Java distinguishes JVM heap, direct memory, and free system memory outside the heap. In Rust there is a single
//! pool of process memory, so [`ResourceUsage::add_memory`] and [`ResourceUsage::add_direct_memory`] both count
//! against total system memory ([`MEMORY`]).

use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use parking_lot::Mutex;

use crate::stats::process_info;
use crate::util::file_utils;
use crate::util::format;

/// A resource (like RAM or disk space) that has a fixed limit on this system.
#[derive(Clone)]
pub enum LimitedResource {
    /// A shared global resource on this system.
    Global { description: String, how_to_fix: String, limit_provider: Arc<dyn Fn() -> Option<u64> + Send + Sync> },
    /// Limited disk space on the file system with this mount point.
    Disk { store: Option<file_utils::FileStore>, path: PathBuf },
}

impl fmt::Debug for LimitedResource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.description())
    }
}

impl PartialEq for LimitedResource {
    fn eq(&self, other: &Self) -> bool {
        self.key() == other.key()
    }
}

impl LimitedResource {
    /// Creates a new global resource with a limit computed by `limit_provider`.
    pub fn global(
        description: &str,
        how_to_fix: &str,
        limit_provider: impl Fn() -> Option<u64> + Send + Sync + 'static,
    ) -> LimitedResource {
        LimitedResource::Global {
            description: description.to_string(),
            how_to_fix: how_to_fix.to_string(),
            limit_provider: Arc::new(limit_provider),
        }
    }

    /// Disk space on the file system that `path` will exist on.
    pub fn disk(path: &Path) -> LimitedResource {
        LimitedResource::Disk { store: file_utils::get_file_store(path).ok(), path: path.to_path_buf() }
    }

    fn key(&self) -> String {
        match self {
            LimitedResource::Global { description, limit_provider, .. } => {
                format!("global:{description}:{:p}", Arc::as_ptr(limit_provider) as *const ())
            }
            LimitedResource::Disk { store: Some(store), .. } => format!("disk:{}", store.mount_point.display()),
            LimitedResource::Disk { store: None, path } => format!("disk-unknown:{}", path.display()),
        }
    }

    /// The total amount of this resource available, or `None` if unable to determine the limit.
    pub fn limit(&self) -> Option<u64> {
        match self {
            LimitedResource::Global { limit_provider, .. } => limit_provider(),
            LimitedResource::Disk { store, path } => {
                file_utils::get_file_store(path).ok().or_else(|| store.clone()).map(|s| s.unallocated_space())
            }
        }
    }

    pub fn description(&self) -> String {
        match self {
            LimitedResource::Global { description, .. } => description.clone(),
            LimitedResource::Disk { store: Some(store), .. } => format!("storage on {store}"),
            LimitedResource::Disk { store: None, path } => format!("storage for {}", path.display()),
        }
    }

    /// Instructions to increase this resource, if any.
    pub fn how_to_fix(&self) -> Option<&str> {
        match self {
            LimitedResource::Global { how_to_fix, .. } => Some(how_to_fix),
            LimitedResource::Disk { .. } => None,
        }
    }

    fn is_disk(&self) -> bool {
        matches!(self, LimitedResource::Disk { .. })
    }
}

/// Total system memory available to this process (Java's `HEAP`, `DIRECT_MEMORY`, and `OFF_HEAP_MEMORY`).
pub static MEMORY: std::sync::LazyLock<LimitedResource> = std::sync::LazyLock::new(|| {
    LimitedResource::global("system memory", "run on a machine with more RAM", || {
        Some(process_info::get_max_memory_bytes()).filter(|m| *m > 0)
    })
});

/// An amount of a resource that has been requested.
#[derive(Clone, Debug)]
pub struct Usage {
    pub resource: LimitedResource,
    pub amount: u64,
    pub description: String,
}

/// Storage type of a data structure, mirroring `collection::storage::Storage` ids (`ram`, `mmap`, `direct`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StorageKind {
    Ram,
    Mmap,
    Direct,
}

/// Accumulates resource requests for part of a run, then checks them against system limits.
#[derive(Debug)]
pub struct ResourceUsage {
    usages: Mutex<Vec<Usage>>,
    description: String,
}

impl ResourceUsage {
    /// Creates a new resource checker for a part of execution described by `description`.
    pub fn new(description: &str) -> ResourceUsage {
        ResourceUsage { usages: Mutex::new(Vec::new()), description: description.to_string() }
    }

    /// Returns the total amount of disk requested so far.
    pub fn disk_usage(&self) -> u64 {
        self.usages.lock().iter().filter(|u| u.resource.is_disk()).map(|u| u.amount).sum()
    }

    /// Returns the total amount requested so far for `types`.
    pub fn get(&self, types: &[&LimitedResource]) -> u64 {
        self.usages.lock().iter().filter(|u| types.contains(&&u.resource)).map(|u| u.amount).sum()
    }

    /// Requests `amount` bytes on the file system that contains `path`.
    pub fn add_disk(&self, path: Option<&Path>, amount: u64, description: &str) -> &Self {
        match path {
            Some(path) => self.add(LimitedResource::disk(path), amount, description),
            None => self,
        }
    }

    /// Requests `amount` bytes of RAM.
    pub fn add_memory(&self, amount: u64, description: &str) -> &Self {
        self.add(MEMORY.clone(), amount, description)
    }

    /// Requests `amount` bytes of memory outside the heap (in Rust, the same as [`ResourceUsage::add_memory`]).
    pub fn add_direct_memory(&self, amount: u64, description: &str) -> &Self {
        self.add_memory(amount, description)
    }

    /// Requests `amount` bytes using `storage`.
    pub fn add_storage(&self, path: &Path, storage: StorageKind, amount: u64, description: &str) -> &Self {
        match storage {
            StorageKind::Ram => self.add_memory(amount, description),
            StorageKind::Direct => self.add_direct_memory(amount, description),
            StorageKind::Mmap => self.add_disk(Some(path), amount, description),
        }
    }

    /// Requests `amount` of `resource`.
    pub fn add(&self, resource: LimitedResource, amount: u64, description: &str) -> &Self {
        self.add_usage(Usage { resource, amount, description: description.to_string() })
    }

    pub fn add_usage(&self, usage: Usage) -> &Self {
        if usage.amount != 0 {
            self.usages.lock().push(usage);
        }
        self
    }

    /// Adds all resource requests in `other` to this instance.
    pub fn add_all(&self, other: &ResourceUsage) -> &Self {
        let other = other.usages.lock().clone();
        self.usages.lock().extend(other);
        self
    }

    /// Adds up all the resource requests and logs an error if any exceed the limit for that resource.
    ///
    /// If `force` is false, returns an error when a limit is exceeded; otherwise just logs a warning. If `verbose`,
    /// logs each resource request even if it is under the limit.
    pub fn check_against_limits(&self, force: bool, verbose: bool) -> anyhow::Result<()> {
        let usages = self.usages.lock().clone();
        let mut grouped: Vec<(LimitedResource, Vec<Usage>)> = Vec::new();
        for usage in usages {
            match grouped.iter_mut().find(|(r, _)| *r == usage.resource) {
                Some((_, list)) => list.push(usage),
                None => grouped.push((usage.resource.clone(), vec![usage])),
            }
        }
        let mut issues = Vec::new();
        for (resource, usages) in grouped {
            let requested: u64 = usages.iter().map(|u| u.amount).sum();
            let requested_string = format::storage(requested, false);
            let Some(limit) = resource.limit() else {
                tracing::warn!(
                    "{requested_string} requested but unable to get limit for {}, may fail.",
                    resource.description()
                );
                continue;
            };
            let summary = format!(
                "{requested_string} {} requested for {}, {} available",
                resource.description(),
                self.description,
                format::storage(limit, false)
            );
            let fix = resource.how_to_fix().map_or(String::new(), |f| format!(" ({f})"));
            if limit < requested {
                tracing::warn!("❌️ {summary}{fix}");
                for usage in &usages {
                    tracing::warn!("   - {} used for {}", format::storage(usage.amount, false), usage.description);
                }
                issues.push(summary);
            } else if (limit as f64) < requested as f64 * 1.1 {
                tracing::info!("⚠️️ {summary}{fix}");
                for usage in &usages {
                    tracing::info!("   - {} used for {}", format::storage(usage.amount, false), usage.description);
                }
            } else if verbose {
                tracing::debug!("✓ {summary}");
                for usage in &usages {
                    tracing::debug!(" - {} used for {}", format::storage(usage.amount, false), usage.description);
                }
            }
        }
        if !force && !issues.is_empty() {
            anyhow::bail!(
                "Insufficient resources for {}, use the --force argument to continue anyway:\n{}",
                self.description,
                issues.join("\n")
            );
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty() {
        ResourceUsage::new("testing it out").check_against_limits(true, false).unwrap();
    }

    #[test]
    fn fake_resource() {
        let resource = LimitedResource::global("testing resource", "get more", || Some(10));
        let check = ResourceUsage::new("testing it out");
        check.add(resource.clone(), 9, "description");
        check.check_against_limits(false, false).unwrap();
        check.add(resource.clone(), 2, "more");
        assert!(check.check_against_limits(false, false).is_err());
        check.check_against_limits(true, false).unwrap();
        assert_eq!(11, check.get(&[&resource]));
    }

    #[test]
    fn too_much_ram() {
        let check = ResourceUsage::new("testing it out");
        check.add_memory(process_info::get_max_memory_bytes() - 1, "test");
        check.check_against_limits(false, false).unwrap();
        check.add_memory(2, "more");
        assert!(check.check_against_limits(false, false).is_err());
        check.check_against_limits(true, false).unwrap();
    }

    #[test]
    fn disk() {
        let tmp = tempfile::tempdir().unwrap();
        let check = ResourceUsage::new("disk");
        check.add_disk(Some(tmp.path()), 100, "a").add_disk(None, 100, "b");
        assert_eq!(100, check.disk_usage());
        check.check_against_limits(false, true).unwrap();
    }
}
