#![deny(clippy::all)]

use globset::{Glob, GlobSet, GlobSetBuilder};
use napi::bindgen_prelude::AsyncTask;
use napi::{Env, Error, Result, Task};
use napi_derive::napi;
use rayon::prelude::*;
use std::borrow::Cow;
use std::fs::File;
use std::io::{BufReader, BufWriter, Seek, Write};
use std::path::{Path, PathBuf};
use walkdir::{DirEntry, WalkDir};

use zip::write::SimpleFileOptions;
use zip::CompressionMethod;

const BUF_SIZE: usize = 262144; // 256KB
const CHANNEL_CAPACITY: usize = 128;

// Zip format constants
const LOCAL_FILE_HEADER_SIG: u32 = 0x04034b50;
const CENTRAL_DIR_HEADER_SIG: u32 = 0x02014b50;
const END_OF_CENTRAL_DIR_SIG: u32 = 0x06054b50;
const ZIP64_END_OF_CENTRAL_DIR_SIG: u32 = 0x06064b50;
const ZIP64_END_OF_CENTRAL_DIR_LOCATOR_SIG: u32 = 0x07064b50;
const VERSION_MADE_BY: u16 = 0x0314; // Unix + Zip 2.0
const VERSION_NEEDED_DEFLATE: u16 = 20; // 2.0
const VERSION_NEEDED_ZIP64: u16 = 45; // 4.5
const UTF8_FLAG: u16 = 1 << 11;

/// Pre-compressed zip entry produced by parallel workers
struct CompressedEntry {
  name: Vec<u8>,
  compressed_data: Vec<u8>,
  crc32: u32,
  uncompressed_size: u64,
  compressed_size: u64,
  compression_method: u16,
  is_dir: bool,
  last_mod_time: u16,
  last_mod_date: u16,
  #[cfg(unix)]
  unix_mode: Option<u32>,
}

/// Convert a SystemTime to DOS date/time format (used by zip format)
fn system_time_to_dos(time: std::time::SystemTime) -> (u16, u16) {
  // DOS date: bits 0-4 = day, bits 5-8 = month, bits 9-15 = year offset from 1980
  // DOS time: bits 0-4 = seconds/2, bits 5-10 = minutes, bits 11-15 = hours
  let duration = time
    .duration_since(std::time::UNIX_EPOCH)
    .unwrap_or_default();
  let secs = duration.as_secs();

  // Simple conversion from unix timestamp to date components
  // Using a basic algorithm that handles dates from 1980-2107
  let days = (secs / 86400) as i64;
  let time_of_day = secs % 86400;

  let hours = (time_of_day / 3600) as u16;
  let minutes = ((time_of_day % 3600) / 60) as u16;
  let seconds = ((time_of_day % 60) / 2) as u16; // DOS stores seconds/2

  // Calculate date from days since epoch (1970-01-01)
  // Adapted from Howard Hinnant's algorithm
  let z = days + 719468;
  let era = (if z >= 0 { z } else { z - 146096 }) / 146097;
  let doe = (z - era * 146097) as u64;
  let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
  let y = yoe as i64 + era * 400;
  let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
  let mp = (5 * doy + 2) / 153;
  let d = (doy - (153 * mp + 2) / 5 + 1) as u16;
  let m = if mp < 10 { mp + 3 } else { mp - 9 } as u16;
  let y = if m <= 2 { y + 1 } else { y };

  // DOS epoch starts at 1980
  let year_offset = (y.max(1980) - 1980).min(127) as u16;

  let dos_time = (hours << 11) | (minutes << 5) | seconds;
  let dos_date = (year_offset << 9) | (m << 5) | d;
  (dos_time, dos_date)
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

/// Build a compressed entry from a file (designed for parallel execution with rayon)
fn build_file_entry(
  name: &str,
  data: &[u8],
  compression_method: u16,
  compression_level: u32,
  last_mod_time: u16,
  last_mod_date: u16,
  #[cfg(unix)] unix_mode: Option<u32>,
) -> std::result::Result<CompressedEntry, String> {
  let mut crc = crc32fast::Hasher::new();
  crc.update(data);
  let crc32 = crc.finalize();
  let uncompressed_size = data.len() as u64;

  let compressed_data = if compression_method == 8 {
    // Deflate
    let mut encoder = flate2::write::DeflateEncoder::new(
      Vec::with_capacity(data.len()),
      flate2::Compression::new(compression_level),
    );
    encoder
      .write_all(data)
      .map_err(|e| format!("Failed to compress '{}': {}", name, e))?;
    encoder
      .finish()
      .map_err(|e| format!("Failed to finish compression for '{}': {}", name, e))?
  } else {
    // Stored (method 0)
    data.to_vec()
  };

  let compressed_size = compressed_data.len() as u64;

  Ok(CompressedEntry {
    name: name.as_bytes().to_vec(),
    compressed_data,
    crc32,
    uncompressed_size,
    compressed_size,
    compression_method,
    is_dir: false,
    last_mod_time,
    last_mod_date,
    #[cfg(unix)]
    unix_mode,
  })
}

/// Build a directory entry (no I/O or compression needed)
fn build_dir_entry(
  name: &str,
  last_mod_time: u16,
  last_mod_date: u16,
  #[cfg(unix)] unix_mode: Option<u32>,
) -> CompressedEntry {
  let mut dir_name = name.as_bytes().to_vec();
  if !dir_name.ends_with(b"/") {
    dir_name.push(b'/');
  }
  CompressedEntry {
    name: dir_name,
    compressed_data: Vec::new(),
    crc32: 0,
    uncompressed_size: 0,
    compressed_size: 0,
    compression_method: 0, // Stored for directories
    is_dir: true,
    last_mod_time,
    last_mod_date,
    #[cfg(unix)]
    unix_mode,
  }
}

/// Check if any entry needs Zip64 extensions
fn needs_zip64(entries: &[CompressedEntry]) -> bool {
  entries.len() > u16::MAX as usize
    || entries
      .iter()
      .any(|e| e.compressed_size > u32::MAX as u64 || e.uncompressed_size > u32::MAX as u64)
}

/// Write a complete zip archive from pre-compressed entries.
/// This is the fast path: all CPU work (compression + CRC) is already done.
fn write_zip_archive<W: Write + Seek>(
  writer: &mut W,
  entries: &[CompressedEntry],
) -> std::io::Result<()> {
  let use_zip64 = needs_zip64(entries);
  let version_needed = if use_zip64 {
    VERSION_NEEDED_ZIP64
  } else {
    VERSION_NEEDED_DEFLATE
  };

  let mut offsets = Vec::with_capacity(entries.len());

  // Write local file headers + data
  for entry in entries {
    offsets.push(writer.stream_position()?);

    let extra_field = build_extra_field(entry, use_zip64);

    // Local file header
    writer.write_all(&LOCAL_FILE_HEADER_SIG.to_le_bytes())?;
    writer.write_all(&version_needed.to_le_bytes())?;
    writer.write_all(&UTF8_FLAG.to_le_bytes())?; // general purpose bit flag
    writer.write_all(&entry.compression_method.to_le_bytes())?;
    writer.write_all(&entry.last_mod_time.to_le_bytes())?;
    writer.write_all(&entry.last_mod_date.to_le_bytes())?;
    writer.write_all(&entry.crc32.to_le_bytes())?;

    if use_zip64 {
      writer.write_all(&0xFFFFFFFFu32.to_le_bytes())?; // compressed size (in zip64 extra)
      writer.write_all(&0xFFFFFFFFu32.to_le_bytes())?; // uncompressed size (in zip64 extra)
    } else {
      writer.write_all(&(entry.compressed_size as u32).to_le_bytes())?;
      writer.write_all(&(entry.uncompressed_size as u32).to_le_bytes())?;
    }

    writer.write_all(&(entry.name.len() as u16).to_le_bytes())?;
    writer.write_all(&(extra_field.len() as u16).to_le_bytes())?;
    writer.write_all(&entry.name)?;
    writer.write_all(&extra_field)?;

    // File data
    writer.write_all(&entry.compressed_data)?;
  }

  // Central directory
  let central_dir_offset = writer.stream_position()?;

  for (i, entry) in entries.iter().enumerate() {
    let extra_field = build_central_extra_field(entry, offsets[i], use_zip64);

    writer.write_all(&CENTRAL_DIR_HEADER_SIG.to_le_bytes())?;
    writer.write_all(&VERSION_MADE_BY.to_le_bytes())?;
    writer.write_all(&version_needed.to_le_bytes())?;
    writer.write_all(&UTF8_FLAG.to_le_bytes())?; // flags
    writer.write_all(&entry.compression_method.to_le_bytes())?;
    writer.write_all(&entry.last_mod_time.to_le_bytes())?;
    writer.write_all(&entry.last_mod_date.to_le_bytes())?;
    writer.write_all(&entry.crc32.to_le_bytes())?;

    if use_zip64 {
      writer.write_all(&0xFFFFFFFFu32.to_le_bytes())?;
      writer.write_all(&0xFFFFFFFFu32.to_le_bytes())?;
    } else {
      writer.write_all(&(entry.compressed_size as u32).to_le_bytes())?;
      writer.write_all(&(entry.uncompressed_size as u32).to_le_bytes())?;
    }

    writer.write_all(&(entry.name.len() as u16).to_le_bytes())?;
    writer.write_all(&(extra_field.len() as u16).to_le_bytes())?;
    writer.write_all(&0u16.to_le_bytes())?; // comment length
    writer.write_all(&0u16.to_le_bytes())?; // disk number start
    writer.write_all(&0u16.to_le_bytes())?; // internal file attributes

    // External file attributes (Unix permissions in upper 16 bits)
    #[cfg(unix)]
    {
      let mode = entry
        .unix_mode
        .unwrap_or(if entry.is_dir { 0o40755 } else { 0o100644 });
      writer.write_all(&(mode << 16).to_le_bytes())?;
    }
    #[cfg(not(unix))]
    {
      let attr: u32 = if entry.is_dir { 0x10 } else { 0 };
      writer.write_all(&attr.to_le_bytes())?;
    }

    if use_zip64 {
      writer.write_all(&0xFFFFFFFFu32.to_le_bytes())?; // offset in zip64 extra
    } else {
      writer.write_all(&(offsets[i] as u32).to_le_bytes())?;
    }

    writer.write_all(&entry.name)?;
    writer.write_all(&extra_field)?;
  }

  let central_dir_end = writer.stream_position()?;
  let central_dir_size = central_dir_end - central_dir_offset;

  if use_zip64 {
    // Zip64 end of central directory record
    writer.write_all(&ZIP64_END_OF_CENTRAL_DIR_SIG.to_le_bytes())?;
    writer.write_all(&44u64.to_le_bytes())?; // size of remaining record
    writer.write_all(&VERSION_MADE_BY.to_le_bytes())?;
    writer.write_all(&VERSION_NEEDED_ZIP64.to_le_bytes())?;
    writer.write_all(&0u32.to_le_bytes())?; // disk number
    writer.write_all(&0u32.to_le_bytes())?; // disk with central dir
    writer.write_all(&(entries.len() as u64).to_le_bytes())?;
    writer.write_all(&(entries.len() as u64).to_le_bytes())?;
    writer.write_all(&central_dir_size.to_le_bytes())?;
    writer.write_all(&central_dir_offset.to_le_bytes())?;

    // Zip64 end of central directory locator
    writer.write_all(&ZIP64_END_OF_CENTRAL_DIR_LOCATOR_SIG.to_le_bytes())?;
    writer.write_all(&0u32.to_le_bytes())?; // disk with zip64 EOCD
    writer.write_all(&central_dir_end.to_le_bytes())?;
    writer.write_all(&1u32.to_le_bytes())?; // total disks
  }

  // End of central directory record
  writer.write_all(&END_OF_CENTRAL_DIR_SIG.to_le_bytes())?;
  writer.write_all(&0u16.to_le_bytes())?; // disk number
  writer.write_all(&0u16.to_le_bytes())?; // disk with central dir

  if use_zip64 || entries.len() > u16::MAX as usize {
    writer.write_all(&0xFFFFu16.to_le_bytes())?;
    writer.write_all(&0xFFFFu16.to_le_bytes())?;
  } else {
    writer.write_all(&(entries.len() as u16).to_le_bytes())?;
    writer.write_all(&(entries.len() as u16).to_le_bytes())?;
  }

  if use_zip64 {
    writer.write_all(&0xFFFFFFFFu32.to_le_bytes())?;
    writer.write_all(&0xFFFFFFFFu32.to_le_bytes())?;
  } else {
    writer.write_all(&(central_dir_size as u32).to_le_bytes())?;
    writer.write_all(&(central_dir_offset as u32).to_le_bytes())?;
  }

  writer.write_all(&0u16.to_le_bytes())?; // comment length

  writer.flush()?;
  Ok(())
}

/// Build extra field for local file header
fn build_extra_field(entry: &CompressedEntry, use_zip64: bool) -> Vec<u8> {
  if !use_zip64 {
    return Vec::new();
  }
  // Zip64 extended information extra field (header ID 0x0001)
  let mut extra = Vec::with_capacity(20);
  extra.extend_from_slice(&0x0001u16.to_le_bytes()); // header ID
  extra.extend_from_slice(&16u16.to_le_bytes()); // data size
  extra.extend_from_slice(&entry.uncompressed_size.to_le_bytes());
  extra.extend_from_slice(&entry.compressed_size.to_le_bytes());
  extra
}

/// Build extra field for central directory header
fn build_central_extra_field(entry: &CompressedEntry, offset: u64, use_zip64: bool) -> Vec<u8> {
  if !use_zip64 {
    return Vec::new();
  }
  // Zip64 extended information extra field (header ID 0x0001)
  let mut extra = Vec::with_capacity(28);
  extra.extend_from_slice(&0x0001u16.to_le_bytes()); // header ID
  extra.extend_from_slice(&24u16.to_le_bytes()); // data size
  extra.extend_from_slice(&entry.uncompressed_size.to_le_bytes());
  extra.extend_from_slice(&entry.compressed_size.to_le_bytes());
  extra.extend_from_slice(&offset.to_le_bytes());
  extra
}

/// Process a walkdir entry: read file content and compute metadata for parallel processing.
/// Returns Ok(Some((name, content, mode))) for files, Ok(None) for skipped entries.
fn collect_entry(
  entry: &DirEntry,
  source_dir: &Path,
  exclude_matcher: &Option<GlobSet>,
) -> std::result::Result<Option<CollectedEntry>, String> {
  let path = entry.path();
  let file_type = entry.file_type();

  let name_path = match path.strip_prefix(source_dir) {
    Ok(p) => p,
    Err(_) => return Ok(None),
  };
  let name_str = match name_path.to_str() {
    Some(s) => s,
    None => return Ok(None),
  };

  if name_str.is_empty() {
    return Ok(None);
  }

  if let Some(ref matcher) = exclude_matcher {
    if matcher.is_match(name_str) {
      return Ok(None);
    }
  }

  let name = normalize_path(name_str).into_owned();

  // Capture file modification time for zip entry timestamp
  let modified = entry
    .metadata()
    .ok()
    .and_then(|m| m.modified().ok())
    .unwrap_or_else(std::time::SystemTime::now);

  if file_type.is_file() {
    let content =
      std::fs::read(path).map_err(|e| format!("Failed to read file '{}': {}", name, e))?;

    #[cfg(unix)]
    let mode = {
      use std::os::unix::fs::PermissionsExt;
      entry.metadata().ok().map(|m| m.permissions().mode())
    };

    Ok(Some(CollectedEntry::File {
      name,
      content,
      modified,
      #[cfg(unix)]
      mode,
    }))
  } else if file_type.is_dir() {
    #[cfg(unix)]
    let mode = {
      use std::os::unix::fs::PermissionsExt;
      entry.metadata().ok().map(|m| m.permissions().mode())
    };

    Ok(Some(CollectedEntry::Dir {
      name,
      modified,
      #[cfg(unix)]
      mode,
    }))
  } else {
    Ok(None)
  }
}

enum CollectedEntry {
  File {
    name: String,
    content: Vec<u8>,
    modified: std::time::SystemTime,
    #[cfg(unix)]
    mode: Option<u32>,
  },
  Dir {
    name: String,
    modified: std::time::SystemTime,
    #[cfg(unix)]
    mode: Option<u32>,
  },
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
    // 1. Build GlobSet for pattern matching
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

    // 2. Validate options
    let algorithm_name = self.options.algorithm.as_deref().unwrap_or("deflate");
    let compression_level = self.options.level.unwrap_or(1);

    validate_compression_level(algorithm_name, compression_level).map_err(Error::from_reason)?;

    // 3. Create parent directories for output file
    if let Some(parent) = self.output_path.parent() {
      if !parent.as_os_str().is_empty() && !parent.exists() {
        std::fs::create_dir_all(parent)
          .map_err(|e| Error::from_reason(format!("Failed to create output directory: {}", e)))?;
      }
    }

    // Route to the appropriate implementation based on algorithm
    match algorithm_name {
      "deflate" | "stored" => self.compress_parallel_deflate(
        &exclude_matcher,
        compression_level,
        algorithm_name == "deflate",
      ),
      _ => self.compress_with_zip_crate(&exclude_matcher, algorithm_name, compression_level),
    }
  }

  fn resolve(&mut self, _env: Env, output: Self::Output) -> Result<Self::JsValue> {
    Ok(output)
  }
}

impl CompressTask {
  /// Fast path: parallel compression + CRC32 with custom zip writer (deflate/stored)
  fn compress_parallel_deflate(
    &self,
    exclude_matcher: &Option<GlobSet>,
    compression_level: i32,
    use_deflate: bool,
  ) -> Result<u32> {
    let follow_symlinks = self.options.follow_symlinks.unwrap_or(false);

    // 1. Walk directory and collect all entries (fast: just readdir syscalls)
    let entries: Vec<DirEntry> = WalkDir::new(&self.source_dir)
      .follow_links(follow_symlinks)
      .into_iter()
      .collect::<std::result::Result<Vec<_>, _>>()
      .map_err(|e| Error::from_reason(format!("Failed to read directory: {}", e)))?;

    // 2. Read files in parallel + filter (rayon parallelizes across CPU cores)
    let source_dir = &self.source_dir;
    let collected: Vec<std::result::Result<Option<CollectedEntry>, String>> = entries
      .par_iter()
      .map(|entry| collect_entry(entry, source_dir, exclude_matcher))
      .collect();

    // Check for errors
    let mut valid_entries = Vec::with_capacity(collected.len());
    for result in collected {
      match result {
        Ok(Some(entry)) => valid_entries.push(entry),
        Ok(None) => {}
        Err(e) => return Err(Error::from_reason(e)),
      }
    }

    // 3. Compress all entries in parallel (CRC32 + deflate for files)
    let compression_method: u16 = if use_deflate { 8 } else { 0 };
    let level = compression_level as u32;

    let compressed_entries: Vec<std::result::Result<CompressedEntry, String>> = valid_entries
      .par_iter()
      .map(|entry| match entry {
        CollectedEntry::File {
          name,
          content,
          modified,
          #[cfg(unix)]
          mode,
        } => {
          let (time, date) = system_time_to_dos(*modified);
          build_file_entry(
            name,
            content,
            compression_method,
            level,
            time,
            date,
            #[cfg(unix)]
            *mode,
          )
        }
        CollectedEntry::Dir {
          name,
          modified,
          #[cfg(unix)]
          mode,
        } => {
          let (time, date) = system_time_to_dos(*modified);
          Ok(build_dir_entry(
            name,
            time,
            date,
            #[cfg(unix)]
            *mode,
          ))
        }
      })
      .collect();

    // Check for compression errors
    let mut final_entries = Vec::with_capacity(compressed_entries.len());
    for result in compressed_entries {
      final_entries.push(result.map_err(Error::from_reason)?);
    }

    // 4. Count files
    let file_count = final_entries.iter().filter(|e| !e.is_dir).count() as u32;

    // 5. Write the zip archive (sequential, but just raw byte writes - no CPU work)
    let file = File::create(&self.output_path)
      .map_err(|e| Error::from_reason(format!("Failed to create zip file: {}", e)))?;
    let mut buf_writer = BufWriter::with_capacity(BUF_SIZE, file);

    write_zip_archive(&mut buf_writer, &final_entries)
      .map_err(|e| Error::from_reason(format!("Failed to write zip archive: {}", e)))?;

    Ok(file_count)
  }

  /// Fallback path: use the zip crate for bzip2/zstd (sequential with parallel I/O)
  fn compress_with_zip_crate(
    &self,
    exclude_matcher: &Option<GlobSet>,
    algorithm_name: &str,
    compression_level: i32,
  ) -> Result<u32> {
    let compression_method = match algorithm_name {
      "bzip2" => CompressionMethod::Bzip2,
      "zstd" => CompressionMethod::Zstd,
      alg => {
        return Err(Error::from_reason(format!(
          "Unsupported algorithm '{}'. Valid options: deflate, bzip2, zstd",
          alg
        )))
      }
    };

    let base_options = SimpleFileOptions::default()
      .compression_method(compression_method)
      .compression_level(Some(compression_level as i64));

    let follow_symlinks = self.options.follow_symlinks.unwrap_or(false);

    // Collect entries
    let entries: Vec<DirEntry> = WalkDir::new(&self.source_dir)
      .follow_links(follow_symlinks)
      .into_iter()
      .collect::<std::result::Result<Vec<_>, _>>()
      .map_err(|e| Error::from_reason(format!("Failed to read directory: {}", e)))?;

    // Parallel read + channel pipeline
    let (tx, rx) = std::sync::mpsc::sync_channel::<std::result::Result<CollectedEntry, String>>(
      CHANNEL_CAPACITY.min(entries.len()),
    );

    let source_dir = self.source_dir.clone();
    let exclude_matcher_clone = exclude_matcher.clone();

    rayon::spawn(move || {
      entries.par_iter().for_each_with(tx, |tx, entry| {
        match collect_entry(entry, &source_dir, &exclude_matcher_clone) {
          Ok(Some(data)) => {
            let _ = tx.send(Ok(data));
          }
          Ok(None) => {}
          Err(e) => {
            let _ = tx.send(Err(e));
          }
        }
      });
    });

    // Write to zip
    let file = File::create(&self.output_path)
      .map_err(|e| Error::from_reason(format!("Failed to create zip file: {}", e)))?;
    let buf_writer = BufWriter::with_capacity(BUF_SIZE, file);
    let mut zip = zip::ZipWriter::new(buf_writer);
    let mut file_count = 0u32;

    for result in rx {
      let entry = result.map_err(Error::from_reason)?;
      match entry {
        CollectedEntry::File {
          name,
          content,
          #[cfg(unix)]
          mode,
          ..
        } => {
          let mut options = base_options;
          #[cfg(unix)]
          if let Some(m) = mode {
            options = options.unix_permissions(m);
          }
          zip
            .start_file(&name, options)
            .map_err(|e| Error::from_reason(format!("Failed to write zip entry: {}", e)))?;
          zip
            .write_all(&content)
            .map_err(|e| Error::from_reason(format!("Failed to write data: {}", e)))?;
          file_count += 1;
        }
        CollectedEntry::Dir {
          name,
          #[cfg(unix)]
          mode,
          ..
        } => {
          let mut options = base_options;
          #[cfg(unix)]
          if let Some(m) = mode {
            options = options.unix_permissions(m);
          }
          zip
            .add_directory(&name, options)
            .map_err(|e| Error::from_reason(format!("Failed to add directory: {}", e)))?;
        }
      }
    }

    zip
      .finish()
      .map_err(|e| Error::from_reason(format!("Zip finalization failed: {}", e)))?;

    Ok(file_count)
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
    std::fs::create_dir_all(&self.output_dir)
      .map_err(|e| Error::from_reason(format!("Failed to create output directory: {}", e)))?;

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
