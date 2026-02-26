#![deny(clippy::all)]

use globset::{Glob, GlobSetBuilder};
use napi::bindgen_prelude::AsyncTask;
use napi::{Env, Error, Result, Task};
use napi_derive::napi;
use std::borrow::Cow;
use std::fs::File;
use std::io::{BufReader, BufWriter, Write};
use std::path::PathBuf;
use walkdir::WalkDir;

use zip::write::SimpleFileOptions;
use zip::CompressionMethod;

const BUF_SIZE: usize = 262144; // 256KB

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

/// Validate compression level for the given algorithm and return the valid range description on error.
fn validate_compression_level(algorithm: &str, level: i32) -> std::result::Result<(), String> {
  let (min, max) = match algorithm {
    "deflate" => (0, 9),
    "bzip2" => (1, 9),
    "zstd" => (1, 22),
    _ => return Ok(()),
  };
  if level < min || level > max {
    Err(format!(
      "Compression level must be between {} and {} for {} (current: {})",
      min, max, algorithm, level
    ))
  } else {
    Ok(())
  }
}

#[napi(object)]
pub struct ZipOptions {
  pub level: Option<i32>,
  pub exclude: Option<Vec<String>>,
  /// Compression algorithm: "deflate" (default), "bzip2", or "zstd"
  pub algorithm: Option<String>,
  /// Whether to follow symbolic links (default: false)
  pub follow_symlinks: Option<bool>,
}

pub struct CompressTask {
  pub source_dir: PathBuf,
  pub output_path: PathBuf,
  pub options: ZipOptions,
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

    // 2. Configure compression options with per-algorithm level validation
    let algorithm_name = self.options.algorithm.as_deref().unwrap_or("deflate");
    let compression_level = self.options.level.unwrap_or(1);
    let compression_method = match algorithm_name {
      "deflate" => CompressionMethod::Deflated,
      "bzip2" => CompressionMethod::Bzip2,
      "zstd" => CompressionMethod::Zstd,
      alg => {
        return Err(Error::from_reason(format!(
          "Unsupported algorithm '{}'. Valid options: deflate, bzip2, zstd",
          alg
        )))
      }
    };

    validate_compression_level(algorithm_name, compression_level)
      .map_err(Error::from_reason)?;

    let base_options = SimpleFileOptions::default()
      .compression_method(compression_method)
      .compression_level(Some(compression_level as i64))
      .large_file(true);

    // 3. Create parent directories for output file
    if let Some(parent) = self.output_path.parent() {
      if !parent.as_os_str().is_empty() && !parent.exists() {
        std::fs::create_dir_all(parent).map_err(|e| {
          Error::from_reason(format!("Failed to create output directory: {}", e))
        })?;
      }
    }

    // 4. Create zip writer with buffered I/O
    let file = File::create(&self.output_path)
      .map_err(|e| Error::from_reason(format!("Failed to create zip file: {}", e)))?;
    let buf_writer = BufWriter::with_capacity(BUF_SIZE, file);
    let mut zip = zip::ZipWriter::new(buf_writer);

    let follow_symlinks = self.options.follow_symlinks.unwrap_or(false);
    let mut file_count = 0u32;

    // 5. Walk directory and stream files directly to zip (constant memory usage)
    let walker = WalkDir::new(&self.source_dir).follow_links(follow_symlinks);

    for result in walker {
      let entry = result.map_err(|e| {
        Error::from_reason(format!("Failed to read directory entry: {}", e))
      })?;

      let path = entry.path();
      let file_type = entry.file_type();

      // Calculate relative path
      let name_path = match path.strip_prefix(&self.source_dir) {
        Ok(p) => p,
        Err(_) => continue,
      };
      let name_str = match name_path.to_str() {
        Some(s) => s,
        None => continue,
      };

      // Skip root directory entry
      if name_str.is_empty() {
        continue;
      }

      // Filter by glob patterns
      if let Some(ref matcher) = exclude_matcher {
        if matcher.is_match(name_str) {
          continue;
        }
      }

      let name = normalize_path(name_str).into_owned();

      if file_type.is_file() {
        let mut options = base_options;

        #[cfg(unix)]
        {
          use std::os::unix::fs::PermissionsExt;
          if let Ok(metadata) = entry.metadata() {
            options = options.unix_permissions(metadata.permissions().mode());
          }
        }

        zip
          .start_file(&name, options)
          .map_err(|e| Error::from_reason(format!("Failed to write zip entry: {}", e)))?;

        // Stream file content directly to zip writer (no full file buffering)
        let f = File::open(path)
          .map_err(|e| Error::from_reason(format!("Failed to open file '{}': {}", name, e)))?;
        let mut reader = BufReader::with_capacity(BUF_SIZE, f);
        std::io::copy(&mut reader, &mut zip)
          .map_err(|e| Error::from_reason(format!("Failed to write data for '{}': {}", name, e)))?;

        file_count += 1;
      } else if file_type.is_dir() {
        let mut options = base_options;

        #[cfg(unix)]
        {
          use std::os::unix::fs::PermissionsExt;
          if let Ok(metadata) = entry.metadata() {
            options = options.unix_permissions(metadata.permissions().mode());
          }
        }

        zip
          .add_directory(&name, options)
          .map_err(|e| Error::from_reason(format!("Failed to add directory: {}", e)))?;
      }
      // Symlinks are skipped when follow_links is false
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
///   - `level`: Compression level (default: 1). Range depends on algorithm:
///     deflate: 0-9, bzip2: 1-9, zstd: 1-22
///   - `exclude`: Array of glob patterns to exclude files
///   - `algorithm`: Compression algorithm (deflate, bzip2, zstd)
///   - `followSymlinks`: Whether to follow symbolic links (default: false)
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
    follow_symlinks: None,
  });

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
    // Ensure output directory exists
    std::fs::create_dir_all(&self.output_dir).map_err(|e| {
      Error::from_reason(format!("Failed to create output directory: {}", e))
    })?;

    let file = File::open(&self.source_path)
      .map_err(|e| Error::from_reason(format!("Failed to open zip file: {}", e)))?;

    let buf_reader = BufReader::with_capacity(BUF_SIZE, file);
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
        let mut buf_writer = BufWriter::with_capacity(BUF_SIZE, outfile);

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
