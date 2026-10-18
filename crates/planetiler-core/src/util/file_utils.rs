//! Port of `util/FileUtils.java`: convenience methods for working with files on disk.
//!
//! Java walks zip files through a zip `FileSystem`, returning paths inside it. Rust has no zip file systems, so
//! [`walk_path_with_pattern`] returns entries inside a zip file as `<zip path>/<entry name>`; use [`split_zip_path`]
//! to get the zip file and entry name back, and [`read_zip_entry`] to read one.

use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use anyhow::{Context, bail};

use crate::util::format;

/// A resource backed by a file or directory on disk (Java's `util/DiskBacked`).
pub trait DiskBacked {
    /// Returns the current size of that file or directory in bytes.
    fn disk_usage_bytes(&self) -> u64;
}

impl<T: DiskBacked + ?Sized> DiskBacked for std::sync::Arc<T> {
    fn disk_usage_bytes(&self) -> u64 {
        (**self).disk_usage_bytes()
    }
}

// Prevent zip-bomb attack, see https://rules.sonarsource.com/java/RSPEC-5042
const ZIP_THRESHOLD_ENTRIES: usize = 10_000;
const ZIP_THRESHOLD_SIZE: u64 = 100_000_000_000;
const ZIP_THRESHOLD_RATIO: f64 = 1_000.0;

/// Size of a file, or total size of all files in a directory, in bytes.
pub fn size(path: &Path) -> io::Result<u64> {
    let meta = std::fs::metadata(path)?;
    if meta.is_dir() {
        let mut total = 0;
        for entry in std::fs::read_dir(path)? {
            total += size(&entry?.path()).unwrap_or(0);
        }
        Ok(total)
    } else {
        Ok(meta.len())
    }
}

/// Size of a file or directory at `path`, or 0 if missing/inaccessible.
pub fn size_or_zero(path: &Path) -> u64 {
    size(path).unwrap_or(0)
}

/// Size of a file, or 0 if missing/inaccessible.
pub fn file_size(path: &Path) -> u64 {
    std::fs::metadata(path).map_or(0, |m| m.len())
}

/// Returns true if `path` ends with `.extension` (case-insensitive).
pub fn has_extension(path: &Path, extension: &str) -> bool {
    path.to_string_lossy().to_lowercase().ends_with(&format!(".{}", extension.to_lowercase()))
}

/// Deletes a file if it exists, logging an error if it can't be deleted.
pub fn delete_file(path: &Path) {
    if let Err(e) = std::fs::remove_file(path)
        && e.kind() != io::ErrorKind::NotFound
    {
        tracing::error!("Unable to delete {}: {e}", path.display());
    }
}

/// Deletes all files under a directory, and the directory itself, failing silently if it doesn't exist.
pub fn delete_directory(path: &Path) {
    if let Err(e) = std::fs::remove_dir_all(path)
        && e.kind() != io::ErrorKind::NotFound
    {
        tracing::error!("Unable to delete {}: {e}", path.display());
    }
}

/// Deletes files or directories recursively, failing silently if missing.
pub fn delete<P: AsRef<Path>>(paths: &[P]) {
    for path in paths {
        let path = path.as_ref();
        if path.is_dir() {
            delete_directory(path);
        } else {
            delete_file(path);
        }
    }
}

/// Information about the file system (disk) that contains a path (Java's `FileStore`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileStore {
    pub name: String,
    pub mount_point: PathBuf,
    pub total_space: u64,
    pub available_space: u64,
}

impl FileStore {
    /// Unallocated space, approximated by available space.
    pub fn unallocated_space(&self) -> u64 {
        self.available_space
    }
}

impl std::fmt::Display for FileStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} ({})", self.mount_point.display(), self.name)
    }
}

/// Returns the file store (disk) for `path`, or its nearest parent directory if it does not exist yet.
pub fn get_file_store(path: &Path) -> anyhow::Result<FileStore> {
    let mut absolute = crate::util::file_watcher::normalize(path);
    let canonical = loop {
        if let Ok(c) = std::fs::canonicalize(&absolute) {
            break c;
        }
        if !absolute.pop() {
            bail!("Cannot get file store for {}", path.display());
        }
    };
    let disks = sysinfo::Disks::new_with_refreshed_list();
    disks
        .list()
        .iter()
        .filter(|d| canonical.starts_with(d.mount_point()))
        .max_by_key(|d| d.mount_point().as_os_str().len())
        .map(|d| FileStore {
            name: d.name().to_string_lossy().to_string(),
            mount_point: d.mount_point().to_path_buf(),
            total_space: d.total_space(),
            available_space: d.available_space(),
        })
        .ok_or_else(|| anyhow::anyhow!("Cannot get file store for {}", path.display()))
}

/// Moves a file.
pub fn move_file(from: &Path, to: &Path) -> io::Result<()> {
    std::fs::rename(from, to)
}

/// Ensures a directory and all parent directories exist.
pub fn create_directory(path: &Path) -> anyhow::Result<()> {
    std::fs::create_dir_all(path).with_context(|| format!("Unable to create directories {}", path.display()))
}

/// Ensures all parent directories of each path in `paths` exist.
pub fn create_parent_directories<P: AsRef<Path>>(paths: &[P]) -> anyhow::Result<()> {
    for path in paths {
        let path = path.as_ref();
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
            && !parent.exists()
        {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("Unable to create parent directories {}", path.display()))?;
        }
    }
    Ok(())
}

/// Copies bytes from `input` to `dest_path`, ensuring that the size is limited to a reasonable value.
pub fn safe_copy(mut input: impl Read, dest_path: &Path) -> anyhow::Result<()> {
    let mut output = std::fs::File::create(dest_path)?;
    let mut total_size = 0u64;
    // Java uses a 2 KB buffer; large writes are much cheaper for multi-GB entries
    let mut buffer = vec![0u8; 1 << 20];
    loop {
        let n = input.read(&mut buffer)?;
        if n == 0 {
            break;
        }
        output.write_all(&buffer[..n])?;
        total_size += n as u64;
        if total_size > ZIP_THRESHOLD_SIZE {
            bail!(
                "The uncompressed data size {}B is too much for the application resource capacity",
                format::storage(total_size, false)
            );
        }
    }
    Ok(())
}

/// Unzips a zip file read from `input` to `dest_dir`, guarding against zip-bomb attacks.
pub fn unzip(input: impl Read + io::Seek, dest_dir: &Path) -> anyhow::Result<()> {
    let mut zip = zip::ZipArchive::new(input)?;
    let mut total_size_archive = 0u64;
    let mut total_entry_archive = 0usize;
    for i in 0..zip.len() {
        let mut entry = zip.by_index(i)?;
        let Some(relative) = entry.enclosed_name() else {
            bail!("Bad zip entry: {}", entry.name());
        };
        let destination = dest_dir.join(relative);
        if entry.is_dir() {
            create_directory(&destination)?;
            continue;
        }
        create_parent_directories(&[&destination])?;
        let compressed_size = entry.compressed_size().max(1);
        let mut out = std::fs::File::options()
            .write(true)
            .create_new(true)
            .open(&destination)
            .with_context(|| format!("Unable to create {}", destination.display()))?;
        total_entry_archive += 1;
        let mut buffer = [0u8; 2048];
        let mut total_size_entry = 0u64;
        loop {
            let n = entry.read(&mut buffer)?;
            if n == 0 {
                break;
            }
            out.write_all(&buffer[..n])?;
            total_size_entry += n as u64;
            total_size_archive += n as u64;
            let ratio = total_size_entry as f64 / compressed_size as f64;
            if ratio > ZIP_THRESHOLD_RATIO {
                bail!(
                    "Ratio between compressed and uncompressed data is highly suspicious {}x, looks like a Zip Bomb Attack",
                    format::numeric(ratio, false)
                );
            }
        }
        if total_size_archive > ZIP_THRESHOLD_SIZE {
            bail!(
                "The uncompressed data size {}B is too much for the application resource capacity",
                format::storage(total_size_archive, false)
            );
        }
        if total_entry_archive > ZIP_THRESHOLD_ENTRIES {
            bail!(
                "Too many entries in this archive {}, can lead to inodes exhaustion of the system",
                format::integer(total_entry_archive)
            );
        }
    }
    Ok(())
}

/// Unzips the zip file at `zip_path` to `dest_dir`.
pub fn unzip_file(zip_path: &Path, dest_dir: &Path) -> anyhow::Result<()> {
    let file = std::fs::File::open(zip_path).with_context(|| format!("Unable to open {}", zip_path.display()))?;
    unzip(io::BufReader::new(file), dest_dir)
}

/// Returns the last modified time of `path`.
pub fn get_last_modified_time(path: &Path) -> io::Result<SystemTime> {
    std::fs::metadata(path)?.modified()
}

/// Returns true if `src` is newer than `dest`, or if `dest` does not exist. Defaults to true if an error occurs.
pub fn is_newer(src: &Path, dest: &Path) -> bool {
    if !dest.exists() {
        return true;
    }
    match (get_last_modified_time(src), get_last_modified_time(dest)) {
        (Ok(s), Ok(d)) => s > d,
        _ => true,
    }
}

/// Expands (or creates) the file at `path` to `size` bytes.
pub fn set_length(path: &Path, size: u64) -> io::Result<()> {
    let file = std::fs::File::options().create(true).write(true).truncate(false).open(path)?;
    if file.metadata()?.len() < size {
        file.set_len(size)?;
    }
    Ok(())
}

/// Splits a path returned by [`walk_path_with_pattern`] into a `.zip` file and the entry name within it, or `None` if
/// the path does not point inside a zip file.
pub fn split_zip_path(path: &Path) -> Option<(PathBuf, String)> {
    let mut zip = PathBuf::new();
    let mut components = path.components();
    for component in components.by_ref() {
        zip.push(component);
        if has_extension(&zip, "zip") && zip.is_file() {
            let rest: PathBuf = components.collect();
            return Some((zip, rest.to_string_lossy().replace('\\', "/")));
        }
    }
    None
}

/// Reads the contents of `entry` from the zip file at `zip_path`.
pub fn read_zip_entry(zip_path: &Path, entry: &str) -> anyhow::Result<Vec<u8>> {
    let file = std::fs::File::open(zip_path)?;
    let mut zip = zip::ZipArchive::new(io::BufReader::new(file))?;
    let mut entry = zip.by_name(entry.trim_start_matches('/'))?;
    let mut result = Vec::with_capacity(entry.size() as usize);
    entry.read_to_end(&mut result)?;
    Ok(result)
}

/// Returns the names of all file entries in a zip file.
pub fn list_zip_entries(zip_path: &Path) -> anyhow::Result<Vec<String>> {
    let file = std::fs::File::open(zip_path)?;
    let zip = zip::ZipArchive::new(io::BufReader::new(file))?;
    Ok(zip.file_names().filter(|n| !n.ends_with('/')).map(str::to_string).collect())
}

/// Returns paths matching glob `pattern` within `base_path`, which can be a directory or zip file.
///
/// When `base_path` is a directory, `walk_zip_file` is invoked for each matching `.zip` file found, and should
/// return the paths of interest within it.
pub fn walk_path_with_pattern_and_zip(
    base_path: &Path,
    pattern: &str,
    walk_zip_file: impl Fn(&Path) -> Vec<PathBuf>,
) -> anyhow::Result<Vec<PathBuf>> {
    let matcher = Glob::new(pattern);
    if has_extension(base_path, "zip") {
        let mut result = vec![];
        for name in list_zip_entries(base_path)? {
            let file_name = name.rsplit('/').next().unwrap_or(&name);
            if matcher.matches(file_name) {
                result.push(base_path.join(&name));
            }
        }
        Ok(result)
    } else if base_path.is_dir() {
        let mut result = vec![];
        let mut stack = vec![base_path.to_path_buf()];
        while let Some(dir) = stack.pop() {
            let mut entries: Vec<_> = std::fs::read_dir(&dir)?.filter_map(Result::ok).map(|e| e.path()).collect();
            entries.sort();
            for path in entries {
                // follow links like Java's FOLLOW_LINKS
                if path.is_dir() {
                    stack.push(path.clone());
                }
                let file_name = path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
                let relative = path.strip_prefix(base_path).unwrap_or(&path).to_string_lossy().replace('\\', "/");
                if matcher.matches(&file_name) || matcher.matches(&relative) {
                    if has_extension(&path, "zip") {
                        result.extend(walk_zip_file(&path));
                    } else {
                        result.push(path);
                    }
                }
            }
        }
        Ok(result)
    } else {
        bail!("No files matching {}/{pattern}", base_path.display());
    }
}

/// Returns paths matching glob `pattern` within `base_path`, which can be a directory or zip file. Zip files found
/// inside a directory are returned as-is.
pub fn walk_path_with_pattern(base_path: &Path, pattern: &str) -> anyhow::Result<Vec<PathBuf>> {
    walk_path_with_pattern_and_zip(base_path, pattern, |p| vec![p.to_path_buf()])
}

/// A glob pattern supporting `*`, `**`, `?`, `[abc]`/`[a-z]`/`[!a]` character classes and `{a,b}` alternatives,
/// like Java's `FileSystem.getPathMatcher("glob:...")`.
#[derive(Clone, Debug)]
pub struct Glob {
    regex: regex::Regex,
}

impl Glob {
    pub fn new(pattern: &str) -> Glob {
        let mut re = String::from("^");
        let chars: Vec<char> = pattern.chars().collect();
        let mut i = 0;
        let mut in_group = false;
        while i < chars.len() {
            let c = chars[i];
            match c {
                '*' if chars.get(i + 1) == Some(&'*') => {
                    re.push_str(".*");
                    i += 1;
                }
                '*' => re.push_str("[^/]*"),
                '?' => re.push_str("[^/]"),
                '[' => {
                    re.push('[');
                    if chars.get(i + 1) == Some(&'!') {
                        re.push('^');
                        i += 1;
                    }
                    i += 1;
                    while i < chars.len() && chars[i] != ']' {
                        if chars[i] == '\\' || chars[i] == '^' {
                            re.push('\\');
                        }
                        re.push(chars[i]);
                        i += 1;
                    }
                    re.push(']');
                }
                '{' => {
                    in_group = true;
                    re.push_str("(?:");
                }
                '}' if in_group => {
                    in_group = false;
                    re.push(')');
                }
                ',' if in_group => re.push('|'),
                '\\' if i + 1 < chars.len() => {
                    i += 1;
                    re.push_str(&regex::escape(&chars[i].to_string()));
                }
                c => re.push_str(&regex::escape(&c.to_string())),
            }
            i += 1;
        }
        re.push('$');
        Glob { regex: regex::Regex::new(&re).unwrap_or_else(|_| regex::Regex::new("$^").unwrap()) }
    }

    pub fn matches(&self, path: &str) -> bool {
        self.regex.is_match(path)
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::*;

    fn write_test_zip(path: &Path) {
        let file = std::fs::File::create(path).unwrap();
        let mut zip = zip::ZipWriter::new(file);
        let options = zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated);
        zip.add_directory("shapefile/", options).unwrap();
        for (name, contents) in [
            ("shapefile/stations.shx", "shx"),
            ("shapefile/stations.cpg", "UTF8"),
            ("shapefile/stations.shp", "shp"),
            ("shapefile/stations.dbf", "dbf"),
            ("shapefile/stations.prj", "GEOGCS[\"GCS_WGS_1984\"]"),
        ] {
            zip.start_file(name, options).unwrap();
            zip.write_all(contents.as_bytes()).unwrap();
        }
        zip.finish().unwrap();
    }

    #[test]
    fn create_and_delete_file_in_nested_directory() {
        let tmp = tempfile::tempdir().unwrap();
        let parent = tmp.path().join("a").join("b").join("c");
        let file = parent.join("file.txt");
        create_directory(&parent).unwrap();
        std::fs::write(&file, [1, 2, 3]).unwrap();
        assert_eq!(3, size_or_zero(&file));
        assert_eq!(3, size_or_zero(&parent));
        assert!(has_extension(&file, "txt"));
        assert!(!has_extension(&file, "png"));

        delete(&[tmp.path().join("a")]);
        assert!(!file.exists());
        assert!(!parent.exists());
        assert!(!tmp.path().join("a").exists());
        assert_eq!(0, size_or_zero(&file));
        assert_eq!(0, size_or_zero(&parent));
        assert_eq!(0, size_or_zero(tmp.path()));
    }

    #[test]
    fn file_store() {
        let tmp = tempfile::tempdir().unwrap();
        let Ok(store) = get_file_store(tmp.path()) else {
            // some sandboxes do not expose disk information
            return;
        };
        let mount = |p: &Path| get_file_store(p).unwrap().mount_point;
        assert_eq!(store.mount_point, mount(&tmp.path().join("nonexistant_file")));
        let nested = tmp.path().join("subdir").join("nonexistant_file");
        assert_eq!(store.mount_point, mount(&nested));
        create_parent_directories(&[&nested]).unwrap();
        assert!(tmp.path().join("subdir").is_dir());
        assert_eq!(store.mount_point, mount(&nested));
    }

    #[test]
    fn unzip_test() {
        let tmp = tempfile::tempdir().unwrap();
        let zip_path = tmp.path().join("shapefile.zip");
        write_test_zip(&zip_path);
        let dest = tmp.path().join("unzipped");
        unzip_file(&zip_path, &dest).unwrap();
        let mut files = BTreeSet::new();
        for entry in std::fs::read_dir(dest.join("shapefile")).unwrap() {
            files.insert(entry.unwrap().file_name().to_string_lossy().to_string());
        }
        assert_eq!(
            ["stations.cpg", "stations.dbf", "stations.prj", "stations.shp", "stations.shx"]
                .iter()
                .map(|s| s.to_string())
                .collect::<BTreeSet<_>>(),
            files
        );
        assert_eq!("UTF8", std::fs::read_to_string(dest.join("shapefile").join("stations.cpg")).unwrap());
    }

    #[test]
    fn safe_copy_test() {
        let tmp = tempfile::tempdir().unwrap();
        let dest = tmp.path().join("unzipped");
        let input = "a1".repeat(1200);
        safe_copy(input.as_bytes(), &dest).unwrap();
        assert_eq!(input, std::fs::read_to_string(&dest).unwrap());
    }

    #[test]
    fn walk_path_with_pattern_directory() {
        let tmp = tempfile::tempdir().unwrap();
        let parent = tmp.path().join("a").join("b").join("c");
        create_directory(&parent).unwrap();
        let txt_files: Vec<PathBuf> = ["1.txt", "2.txt"].iter().map(|f| parent.join(f)).collect();
        for file in &txt_files {
            std::fs::write(file, []).unwrap();
        }
        std::fs::write(parent.join("something-that-doesnt-match.blah"), []).unwrap();
        let mut matching = walk_path_with_pattern(&parent, "*.txt").unwrap();
        matching.sort();
        assert_eq!(txt_files, matching);
    }

    #[test]
    fn walk_path_with_pattern_directory_zip() {
        let tmp = tempfile::tempdir().unwrap();
        let parent = tmp.path().join("a").join("b").join("c");
        create_directory(&parent).unwrap();
        let zip_file = parent.join("fake-zip-file.zip");
        std::fs::write(&zip_file, []).unwrap();
        std::fs::write(parent.join("something-that-doesnt-match.blah"), []).unwrap();
        assert_eq!(vec![zip_file.clone()], walk_path_with_pattern(&parent, "*.zip").unwrap());
        assert_eq!(
            vec![zip_file.join("inner.txt")],
            walk_path_with_pattern_and_zip(&parent, "*.zip", |p| vec![p.join("inner.txt")]).unwrap()
        );
    }

    #[test]
    fn walk_path_with_pattern_single_zip() {
        let tmp = tempfile::tempdir().unwrap();
        let zip_path = tmp.path().join("shapefile.zip");
        write_test_zip(&zip_path);
        let mut matching = walk_path_with_pattern(&zip_path, "stations.sh[px]").unwrap();
        matching.sort();
        assert_eq!(vec![zip_path.join("shapefile/stations.shp"), zip_path.join("shapefile/stations.shx")], matching);
        let (zip, entry) = split_zip_path(&matching[0]).unwrap();
        assert_eq!(zip_path, zip);
        assert_eq!("shapefile/stations.shp", entry);
        assert_eq!(b"shp".to_vec(), read_zip_entry(&zip, &entry).unwrap());
        assert_eq!(None, split_zip_path(tmp.path()));
    }

    #[test]
    fn expand_file() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("toExpand");
        set_length(&path, 1000).unwrap();
        assert_eq!(1000, std::fs::metadata(&path).unwrap().len());
    }

    #[test]
    fn glob() {
        assert!(Glob::new("*.txt").matches("a.txt"));
        assert!(!Glob::new("*.txt").matches("a/b.txt"));
        assert!(Glob::new("**/*.txt").matches("a/b.txt"));
        assert!(Glob::new("stations.sh[px]").matches("stations.shp"));
        assert!(!Glob::new("stations.sh[px]").matches("stations.shz"));
        assert!(Glob::new("*.{shp,dbf}").matches("a.dbf"));
        assert!(Glob::new("a?c").matches("abc"));
        assert!(Glob::new("[!a]bc").matches("xbc"));
        assert!(!Glob::new("[!a]bc").matches("abc"));
    }

    #[test]
    fn newer() {
        let tmp = tempfile::tempdir().unwrap();
        let a = tmp.path().join("a");
        std::fs::write(&a, "a").unwrap();
        assert!(is_newer(&a, &tmp.path().join("missing")));
    }
}
