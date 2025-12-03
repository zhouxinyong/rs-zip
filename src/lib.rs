#![deny(clippy::all)]

use glob::Pattern;
use napi::bindgen_prelude::AsyncTask;
use napi::{Env, Error, Result, Task};
use napi_derive::napi;
use std::fs::File;
use std::io::{BufWriter, Read, Write};
use std::path::PathBuf;
use walkdir::WalkDir;

use zip::CompressionMethod;
use zip::write::SimpleFileOptions;

#[napi(object)]
pub struct ZipOptions {
  pub level: Option<i32>,
  pub exclude: Option<Vec<String>>,
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
    // 1. Create file stream with buffer
    let file = File::create(&self.output_path)
      .map_err(|e| Error::from_reason(format!("Failed to create zip file: {}", e)))?;

    // 64KB write buffer
    let buf_writer = BufWriter::with_capacity(65536, file);
    let mut zip = zip::ZipWriter::new(buf_writer);

    // Parse exclude patterns
    let exclude_patterns: Vec<Pattern> = self
      .options
      .exclude
      .as_ref()
      .map(|patterns| {
        patterns
          .iter()
          .filter_map(|p| Pattern::new(p).ok())
          .collect()
      })
      .unwrap_or_default();

    // 2. Configure base compression options
    let compression_level = self.options.level.unwrap_or(1);
    let base_options = SimpleFileOptions::default()
      .compression_method(CompressionMethod::Deflated)
      .compression_level(Some(compression_level as i64))
      .large_file(true); // Enable Zip64

    let walk = WalkDir::new(&self.source_dir);
    let mut buffer = vec![0; 65536]; // Reusable 64KB read buffer
    let mut file_count = 0;

    for entry in walk.into_iter().filter_map(|e| e.ok()) {
      let path = entry.path();

      // 3. Calculate and normalize path
      let name_path = path
        .strip_prefix(&self.source_dir)
        .map_err(|e| Error::from_reason(format!("Path resolution error: {}", e)))?;

      let name_str = name_path
        .to_str()
        .ok_or(Error::from_reason("Path contains invalid characters"))?;

      // 4. Filter files
      let mut matched = false;
      for pattern in &exclude_patterns {
        if pattern.matches(name_str) {
          matched = true;
          break;
        }
      }
      if matched {
        continue;
      }

      // Normalize path separator to / on Windows
      #[cfg(windows)]
      let name = name_str.replace("\\", "/");
      #[cfg(not(windows))]
      let name = name_str.to_string();

      if path.is_file() {
        // 5. Get file permissions
        let mut options = base_options;

        #[cfg(unix)]
        {
          use std::os::unix::fs::PermissionsExt;
          if let Ok(metadata) = std::fs::metadata(path) {
            options = options.unix_permissions(metadata.permissions().mode());
          }
        }

        zip
          .start_file(name, options)
          .map_err(|e| Error::from_reason(format!("Failed to write zip entry: {}", e)))?;

        let mut f =
          File::open(path).map_err(|e| Error::from_reason(format!("Failed to read source file: {}", e)))?;

        // Stream copy
        loop {
          let count = f
            .read(&mut buffer)
            .map_err(|e| Error::from_reason(format!("File stream read interrupted: {}", e)))?;
          if count == 0 {
            break;
          }
          zip
            .write_all(&buffer[..count])
            .map_err(|e| Error::from_reason(format!("Failed to write data: {}", e)))?;
        }
        file_count += 1;
      } else if !name.is_empty() {
        // Add directory
        #[cfg(unix)]
        {
          // Preserve directory permissions
          let mut options = base_options;
          use std::os::unix::fs::PermissionsExt;
          if let Ok(metadata) = std::fs::metadata(path) {
            options = options.unix_permissions(metadata.permissions().mode());
          }
          zip
            .add_directory(name, options)
            .map_err(|e| Error::from_reason(format!("Failed to add directory: {}", e)))?;
        }
        #[cfg(not(unix))]
        {
          zip
            .add_directory(name, base_options)
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
#[napi(ts_return_type = "Promise<number>")]
pub fn zip(
  source_dir: String,
  output_path: String,
  options: Option<ZipOptions>,
) -> Result<AsyncTask<CompressTask>> {
  let opts = options.unwrap_or(ZipOptions {
    level: Some(1),
    exclude: None,
  });

  let compression_level = opts.level.unwrap_or(1);
  if !(0..=9).contains(&compression_level) {
    return Err(Error::from_reason(format!(
      "Compression level must be between 0 and 9 (current: {})",
      compression_level
    )));
  }

  Ok(AsyncTask::new(CompressTask {
    source_dir: PathBuf::from(source_dir),
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
    let mut archive = zip::ZipArchive::new(file)
      .map_err(|e| Error::from_reason(format!("Failed to read zip archive: {}", e)))?;

    for i in 0..archive.len() {
      let mut file = archive
        .by_index(i)
        .map_err(|e| Error::from_reason(format!("Failed to read zip entry: {}", e)))?;

      // Security check: Zip Slip
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
            std::fs::create_dir_all(p)
              .map_err(|e| Error::from_reason(format!("Failed to create parent directory: {}", e)))?;
          }
        }
        let mut outfile = File::create(&outpath)
          .map_err(|e| Error::from_reason(format!("Failed to create output file: {}", e)))?;
        std::io::copy(&mut file, &mut outfile)
          .map_err(|e| Error::from_reason(format!("Failed to decompress file content: {}", e)))?;
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
pub fn unzip(source_path: String, output_dir: String) -> AsyncTask<UncompressTask> {
  AsyncTask::new(UncompressTask {
    source_path: PathBuf::from(source_path),
    output_dir: PathBuf::from(output_dir),
  })
}
