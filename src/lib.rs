#![deny(clippy::all)]

use globset::{Glob, GlobSetBuilder};
use napi::bindgen_prelude::AsyncTask;
use napi::{Env, Error, Result, Task};
use napi_derive::napi;
use rayon::prelude::*;
use std::borrow::Cow;
use std::fs::File;
use std::io::{BufReader, BufWriter, Cursor, Write};
use std::path::PathBuf;
use walkdir::WalkDir;

use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, ZipArchive, ZipWriter};

/// Buffer size for I/O operations (256KB)
const IO_BUFFER_SIZE: usize = 256 * 1024;

/// Number of chunks per thread for parallel compression work distribution
const CHUNKS_PER_THREAD: usize = 4;

/// File entry metadata for parallel compression
struct FileEntry {
  path: PathBuf,
  name: String,
  #[cfg(unix)]
  mode: Option<u32>,
}

/// Directory entry metadata
struct DirEntry {
  name: String,
  #[cfg(unix)]
  mode: Option<u32>,
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

    // 3. Walk directory and collect entries (separate files and directories)
    let mut file_entries: Vec<FileEntry> = Vec::new();
    let mut dir_entries: Vec<DirEntry> = Vec::new();

    for entry in WalkDir::new(&self.source_dir)
      .follow_links(false)
      .into_iter()
      .filter_map(|e| e.ok())
    {
      let path = entry.path();
      let ft = entry.file_type();

      let name_path = match path.strip_prefix(&self.source_dir) {
        Ok(p) => p,
        Err(_) => continue,
      };
      let name_str = match name_path.to_str() {
        Some(s) => s,
        None => continue,
      };

      // Filter by glob patterns
      if let Some(ref matcher) = exclude_matcher {
        if matcher.is_match(name_str) {
          continue;
        }
      }

      let name = normalize_path(name_str).into_owned();

      if ft.is_file() {
        #[cfg(unix)]
        let mode = {
          use std::os::unix::fs::PermissionsExt;
          entry.metadata().ok().map(|m| m.permissions().mode())
        };

        file_entries.push(FileEntry {
          path: path.to_path_buf(),
          name,
          #[cfg(unix)]
          mode,
        });
      } else if ft.is_dir() && !name.is_empty() {
        #[cfg(unix)]
        let mode = {
          use std::os::unix::fs::PermissionsExt;
          entry.metadata().ok().map(|m| m.permissions().mode())
        };

        dir_entries.push(DirEntry {
          name,
          #[cfg(unix)]
          mode,
        });
      }
    }

    // 4. Parallel compression: create mini-zip archives in parallel chunks
    //    Each chunk is compressed independently by a rayon thread, then merged
    //    into the main zip without re-compression via merge_archive().
    let file_count = file_entries.len() as u32;
    let chunk_size = if file_entries.is_empty() {
      1
    } else {
      (file_entries.len() / (rayon::current_num_threads() * CHUNKS_PER_THREAD)).max(1)
    };

    let mini_zips: Vec<Result<Vec<u8>>> = file_entries
      .par_chunks(chunk_size)
      .map(|chunk| {
        let cursor = Cursor::new(Vec::new());
        let mut mini_zip = ZipWriter::new(cursor);

        for fe in chunk {
          let content = std::fs::read(&fe.path)
            .map_err(|e| Error::from_reason(format!("Failed to read file: {}", e)))?;

          let mut options = base_options;
          #[cfg(unix)]
          if let Some(mode) = fe.mode {
            options = options.unix_permissions(mode);
          }

          mini_zip
            .start_file(&fe.name, options)
            .map_err(|e| Error::from_reason(format!("Failed to write zip entry: {}", e)))?;

          mini_zip
            .write_all(&content)
            .map_err(|e| Error::from_reason(format!("Failed to write data: {}", e)))?;
        }

        let cursor = mini_zip
          .finish()
          .map_err(|e| Error::from_reason(format!("Failed to finish mini zip: {}", e)))?;
        Ok(cursor.into_inner())
      })
      .collect();

    let mini_zips: Vec<Vec<u8>> = mini_zips.into_iter().collect::<Result<Vec<_>>>()?;

    // 5. Create main zip and merge all mini-zip archives
    let file = File::create(&self.output_path)
      .map_err(|e| Error::from_reason(format!("Failed to create zip file: {}", e)))?;
    let buf_writer = BufWriter::with_capacity(IO_BUFFER_SIZE, file);
    let mut zip = ZipWriter::new(buf_writer);

    // Add directories
    for dir in &dir_entries {
      let mut options = base_options;
      #[cfg(unix)]
      if let Some(mode) = dir.mode {
        options = options.unix_permissions(mode);
      }

      zip
        .add_directory(&dir.name, options)
        .map_err(|e| Error::from_reason(format!("Failed to add directory: {}", e)))?;
    }

    // Merge pre-compressed file entries (no re-compression)
    for mini_zip_data in mini_zips {
      let cursor = Cursor::new(mini_zip_data);
      let source = ZipArchive::new(cursor)
        .map_err(|e| Error::from_reason(format!("Failed to read mini archive: {}", e)))?;
      zip
        .merge_archive(source)
        .map_err(|e| Error::from_reason(format!("Failed to merge archive: {}", e)))?;
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

    let buf_reader = BufReader::with_capacity(IO_BUFFER_SIZE, file);
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
        let mut buf_writer = BufWriter::with_capacity(IO_BUFFER_SIZE, outfile);

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
