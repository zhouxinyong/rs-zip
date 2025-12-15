#![deny(clippy::all)]

use globset::{Glob, GlobSet, GlobSetBuilder};
use napi::bindgen_prelude::AsyncTask;
use napi::{Env, Error, Result, Task};
use napi_derive::napi;
use rayon::prelude::*;
use std::borrow::Cow;
use std::fs::File;
use std::io::{BufReader, BufWriter, Write};
use std::path::PathBuf;
use walkdir::{DirEntry, WalkDir};

use zip::write::SimpleFileOptions;
use zip::CompressionMethod;

/// Prepared file data for parallel reading
struct FileData {
  name: String,
  content: Vec<u8>,
  #[cfg(unix)]
  mode: Option<u32>,
  #[cfg(not(unix))]
  _mode: (),
}

/// Prepared directory entry for writing
struct DirData {
  name: String,
  #[cfg(unix)]
  mode: Option<u32>,
  #[cfg(not(unix))]
  _mode: (),
}

/// Result of parallel file collection
enum EntryData {
  File(FileData),
  Dir(DirData),
}

#[napi(object)]
pub struct ZipOptions {
  pub level: Option<i32>,
  pub exclude: Option<Vec<String>>,
  /// Compression algorithm: "deflate" (default), "bzip2", or "zstd"
  pub algorithm: Option<String>,
}

pub struct CompressTask {
  pub source_dir: PathBuf,
  pub output_path: PathBuf,
  pub options: ZipOptions,
}

/// Normalize path to zip format (forward slashes)
fn normalize_path(name_str: &str) -> Cow<'_, str> {
  #[cfg(windows)]
  {
    if name_str.contains('\\') {
      Cow::Owned(name_str.replace('\\', "/"))
    } else {
      Cow::Borrowed(name_str)
    }
  }
  #[cfg(not(windows))]
  {
    Cow::Borrowed(name_str)
  }
}

/// Process a directory entry for parallel collection
fn process_entry(
  entry: &DirEntry,
  source_dir: &PathBuf,
  exclude_matcher: &Option<GlobSet>,
) -> Option<EntryData> {
  let path = entry.path();
  let file_type = entry.file_type();

  // Calculate relative path
  let name_path = path.strip_prefix(source_dir).ok()?;
  let name_str = name_path.to_str()?;

  // Filter by glob patterns
  if let Some(ref matcher) = exclude_matcher {
    if matcher.is_match(name_str) {
      return None;
    }
  }

  let name = normalize_path(name_str).into_owned();

  if file_type.is_file() {
    // Read file content
    let content = std::fs::read(path).ok()?;

    #[cfg(unix)]
    let mode = {
      use std::os::unix::fs::PermissionsExt;
      entry.metadata().ok().map(|m| m.permissions().mode())
    };

    Some(EntryData::File(FileData {
      name,
      content,
      #[cfg(unix)]
      mode,
      #[cfg(not(unix))]
      _mode: (),
    }))
  } else if file_type.is_dir() && !name.is_empty() {
    #[cfg(unix)]
    let mode = {
      use std::os::unix::fs::PermissionsExt;
      entry.metadata().ok().map(|m| m.permissions().mode())
    };

    Some(EntryData::Dir(DirData {
      name,
      #[cfg(unix)]
      mode,
      #[cfg(not(unix))]
      _mode: (),
    }))
  } else {
    None
  }
}

impl Task for CompressTask {
  type Output = u32;
  type JsValue = u32;

  fn compute(&mut self) -> Result<Self::Output> {
    // 1. Build GlobSet for pattern matching (with error reporting)
    let exclude_matcher = if let Some(ref patterns) = self.options.exclude {
      let mut builder = GlobSetBuilder::new();
      for pattern in patterns {
        let glob = Glob::new(pattern)
          .map_err(|e| Error::from_reason(format!("Invalid glob pattern '{}': {}", pattern, e)))?;
        builder.add(glob);
      }
      Some(
        builder
          .build()
          .map_err(|e| Error::from_reason(format!("Failed to build glob matcher: {}", e)))?,
      )
    } else {
      None
    };

    // 2. Configure compression options with algorithm validation
    let compression_level = self.options.level.unwrap_or(1);
    let compression_method = match self.options.algorithm.as_deref() {
      Some("deflate") | None => CompressionMethod::Deflated,
      Some("bzip2") => CompressionMethod::Bzip2,
      Some("zstd") => CompressionMethod::Zstd,
      Some(alg) => {
        return Err(Error::from_reason(format!(
          "Unsupported algorithm '{}'. Valid options: deflate, bzip2, zstd",
          alg
        )))
      }
    };

    let base_options = SimpleFileOptions::default()
      .compression_method(compression_method)
      .compression_level(Some(compression_level as i64))
      .large_file(true);

    // 3. Collect all entries
    let entries: Vec<_> = WalkDir::new(&self.source_dir)
      .follow_links(false)
      .into_iter()
      .filter_map(|e| e.ok())
      .collect();

    // 4. Parallel processing: pipeline
    // Use a bounded channel to prevent memory from exploding if reading is faster than writing
    let (tx, rx) = std::sync::mpsc::sync_channel(32);

    // ARC needed for sharing data across threads
    let source_dir = self.source_dir.clone();

    // Spawn producer threads
    rayon::spawn(move || {
      entries.par_iter().for_each_with(tx, |tx, entry| {
        if let Some(data) = process_entry(entry, &source_dir, &exclude_matcher) {
          // If the channel is full, this will block the rayon thread, providing backpressure
          let _ = tx.send(data);
        }
      });
    });

    // 5. Create zip file and write sequentially
    let file = File::create(&self.output_path)
      .map_err(|e| Error::from_reason(format!("Failed to create zip file: {}", e)))?;
    let buf_writer = BufWriter::with_capacity(262144, file);
    let mut zip = zip::ZipWriter::new(buf_writer);

    let mut file_count = 0u32;

    // Consumer: write to zip as data arrives
    for data in rx {
      match data {
        EntryData::File(file_data) => {
          let mut options = base_options;

          #[cfg(unix)]
          if let Some(mode) = file_data.mode {
            options = options.unix_permissions(mode);
          }

          zip
            .start_file(&file_data.name, options)
            .map_err(|e| Error::from_reason(format!("Failed to write zip entry: {}", e)))?;

          zip
            .write_all(&file_data.content)
            .map_err(|e| Error::from_reason(format!("Failed to write data: {}", e)))?;

          file_count += 1;
        }
        EntryData::Dir(dir_data) => {
          let mut options = base_options;

          #[cfg(unix)]
          if let Some(mode) = dir_data.mode {
            options = options.unix_permissions(mode);
          }

          zip
            .add_directory(&dir_data.name, options)
            .map_err(|e| Error::from_reason(format!("Failed to add directory: {}", e)))?;
        }
      }
    }

    // 6. Finish writing
    zip
      .finish()
      .map_err(|e| Error::from_reason(format!("Zip finalization failed: {}", e)))?;

    Ok(file_count)
  }

  fn resolve(&mut self, _env: Env, output: Self::Output) -> Result<Self::JsValue> {
    Ok(output)
  }
}

/// Compress a directory into a zip file.
///
/// Returns the number of files compressed.
///
/// # Arguments
/// * `source_dir` - Source directory path
/// * `output_path` - Output zip file path
/// * `options` - Compression options
///   - `level`: Compression level (0-9, default: 1)
///   - `exclude`: Array of glob patterns to exclude files
///   - `algorithm`: Compression algorithm (deflate, bzip2, zstd)
#[napi(ts_return_type = "Promise<number>")]
pub fn zip(
  source_dir: String,
  output_path: String,
  options: Option<ZipOptions>,
) -> Result<AsyncTask<CompressTask>> {
  // Validate source directory
  let source_path = PathBuf::from(&source_dir);
  if !source_path.exists() {
    return Err(Error::from_reason(format!(
      "Source not found: {}",
      source_dir
    )));
  }
  if !source_path.is_dir() {
    return Err(Error::from_reason(format!(
      "Source is not a directory: {}",
      source_dir
    )));
  }

  let opts = options.unwrap_or(ZipOptions {
    level: Some(1),
    exclude: None,
    algorithm: None,
  });

  // Validate compression level
  let compression_level = opts.level.unwrap_or(1);
  if !(0..=9).contains(&compression_level) {
    return Err(Error::from_reason(format!(
      "Compression level must be between 0 and 9 (current: {})",
      compression_level
    )));
  }

  Ok(AsyncTask::new(CompressTask {
    source_dir: source_path,
    output_path: PathBuf::from(output_path),
    options: opts,
  }))
}

pub struct UncompressTask {
  pub source_path: PathBuf,
  pub output_dir: PathBuf,
}

impl Task for UncompressTask {
  type Output = ();
  type JsValue = ();

  fn compute(&mut self) -> Result<Self::Output> {
    let file = File::open(&self.source_path)
      .map_err(|e| Error::from_reason(format!("Failed to open zip file: {}", e)))?;

    let buf_reader = BufReader::with_capacity(262144, file);
    let mut archive = zip::ZipArchive::new(buf_reader)
      .map_err(|e| Error::from_reason(format!("Failed to read zip archive: {}", e)))?;

    for i in 0..archive.len() {
      let mut file = archive
        .by_index(i)
        .map_err(|e| Error::from_reason(format!("Failed to read zip entry: {}", e)))?;

      // Security: Zip Slip protection
      let outpath = match file.enclosed_name() {
        Some(path) => self.output_dir.join(path),
        None => continue,
      };

      if file.name().ends_with('/') {
        std::fs::create_dir_all(&outpath)
          .map_err(|e| Error::from_reason(format!("Failed to create directory: {}", e)))?;
      } else {
        #[allow(clippy::collapsible_if)]
        if let Some(p) = outpath.parent() {
          if !p.exists() {
            std::fs::create_dir_all(p).map_err(|e| {
              Error::from_reason(format!("Failed to create parent directory: {}", e))
            })?;
          }
        }

        let outfile = File::create(&outpath)
          .map_err(|e| Error::from_reason(format!("Failed to create output file: {}", e)))?;
        let mut buf_writer = BufWriter::with_capacity(262144, outfile);

        std::io::copy(&mut file, &mut buf_writer)
          .map_err(|e| Error::from_reason(format!("Failed to decompress file content: {}", e)))?;

        buf_writer
          .flush()
          .map_err(|e| Error::from_reason(format!("Failed to flush output file: {}", e)))?;
      }

      // Restore permissions (Unix only)
      #[cfg(unix)]
      {
        use std::os::unix::fs::PermissionsExt;
        if let Some(mode) = file.unix_mode() {
          std::fs::set_permissions(&outpath, std::fs::Permissions::from_mode(mode))
            .map_err(|e| Error::from_reason(format!("Failed to set file permissions: {}", e)))?;
        }
      }
    }

    Ok(())
  }

  fn resolve(&mut self, _env: Env, _output: Self::Output) -> Result<Self::JsValue> {
    Ok(())
  }
}

/// Decompress a zip file into a directory.
///
/// Automatically creates the output directory if it doesn't exist.
/// Safely handles paths to prevent writing outside the target directory (Zip Slip protection).
/// Restores file permissions on Unix systems.
///
/// # Arguments
/// * `source_path` - Source zip file path
/// * `output_dir` - Output directory path
#[napi(ts_return_type = "Promise<void>")]
pub fn unzip(source_path: String, output_dir: String) -> Result<AsyncTask<UncompressTask>> {
  // Validate source file
  let path = PathBuf::from(&source_path);
  if !path.exists() {
    return Err(Error::from_reason(format!(
      "Zip file not found: {}",
      source_path
    )));
  }
  if !path.is_file() {
    return Err(Error::from_reason(format!(
      "Path is not a file: {}",
      source_path
    )));
  }

  Ok(AsyncTask::new(UncompressTask {
    source_path: path,
    output_dir: PathBuf::from(output_dir),
  }))
}
