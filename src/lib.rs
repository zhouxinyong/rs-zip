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
    // 1. 创建文件流 + 缓冲 (Buffer)
    let file = File::create(&self.output_path)
      .map_err(|e| Error::from_reason(format!("无法创建压缩文件: {}", e)))?;

    // 64KB 写入缓冲
    let buf_writer = BufWriter::with_capacity(65536, file);
    let mut zip = zip::ZipWriter::new(buf_writer);

    // 解析 exclude patterns
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

    // 2. 配置基础压缩选项
    let compression_level = self.options.level.unwrap_or(6);
    let base_options = SimpleFileOptions::default()
      .compression_method(CompressionMethod::Deflated)
      .compression_level(Some(compression_level as i64))
      .large_file(true); // Enable Zip64

    let walk = WalkDir::new(&self.source_dir);
    let mut buffer = vec![0; 65536]; // 复用 64KB 读取内存
    let mut file_count = 0;

    for entry in walk.into_iter().filter_map(|e| e.ok()) {
      let path = entry.path();

      // 3. 路径计算与标准化
      let name_path = path
        .strip_prefix(&self.source_dir)
        .map_err(|e| Error::from_reason(format!("路径解析错误: {}", e)))?;

      let name_str = name_path
        .to_str()
        .ok_or(Error::from_reason("路径包含非法字符"))?;

      // 4. 过滤文件 (Filter)
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

      // Windows 下路径分隔符统一转为 /
      #[cfg(windows)]
      let name = name_str.replace("\\", "/");
      #[cfg(not(windows))]
      let name = name_str.to_string();

      if path.is_file() {
        // 5. 获取文件权限 (Permissions)
        let mut options = base_options;

        #[cfg(unix)]
        {
          use std::os::unix::fs::PermissionsExt;
          if let Ok(metadata) = std::fs::metadata(path) {
            options = options.unix_permissions(metadata.permissions().mode());
          }
        }

        // 如果非 Unix 系统或获取失败，回退到默认 755 (代码中 base_options 没设默认，zip crate 默认 644/755 取决于实现，这里显式设一个兜底比较好，或者依赖 zip crate 行为)
        // zip crate 默认 unix_permissions 是 None

        zip
          .start_file(name, options)
          .map_err(|e| Error::from_reason(format!("写入Zip条目失败: {}", e)))?;

        let mut f =
          File::open(path).map_err(|e| Error::from_reason(format!("读取源文件失败: {}", e)))?;

        // 流式拷贝 (Stream Copy)
        loop {
          let count = f
            .read(&mut buffer)
            .map_err(|e| Error::from_reason(format!("文件流读取中断: {}", e)))?;
          if count == 0 {
            break;
          }
          zip
            .write_all(&buffer[..count])
            .map_err(|e| Error::from_reason(format!("写入数据失败: {}", e)))?;
        }
        file_count += 1;
      } else if !name.is_empty() {
        // 添加目录
        #[cfg(unix)]
        {
          // 目录也尽量保留权限
          let mut options = base_options;
          use std::os::unix::fs::PermissionsExt;
          if let Ok(metadata) = std::fs::metadata(path) {
            options = options.unix_permissions(metadata.permissions().mode());
          }
          zip
            .add_directory(name, options)
            .map_err(|e| Error::from_reason(format!("添加目录失败: {}", e)))?;
        }
        #[cfg(not(unix))]
        {
          zip
            .add_directory(name, base_options)
            .map_err(|e| Error::from_reason(format!("添加目录失败: {}", e)))?;
        }
      }
    }

    // 6. 结束写入
    zip
      .finish()
      .map_err(|e| Error::from_reason(format!("Zip 封包失败: {}", e)))?;

    Ok(file_count)
  }

  fn resolve(&mut self, _env: Env, output: Self::Output) -> Result<Self::JsValue> {
    Ok(output)
  }
}

/// 暴露给 JS 的异步方法
/// options: { level: number, exclude: string[] }
#[napi(ts_return_type = "Promise<number>")]
pub fn zip(
  source_dir: String,
  output_path: String,
  options: Option<ZipOptions>,
) -> Result<AsyncTask<CompressTask>> {
  let opts = options.unwrap_or(ZipOptions {
    level: Some(6),
    exclude: None,
  });

  let compression_level = opts.level.unwrap_or(6);
  if !(0..=9).contains(&compression_level) {
    return Err(Error::from_reason(format!(
      "压缩等级 level 需在 0 到 9 之间（当前为 {}）",
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
      .map_err(|e| Error::from_reason(format!("无法打开压缩文件: {}", e)))?;
    let mut archive = zip::ZipArchive::new(file)
      .map_err(|e| Error::from_reason(format!("读取Zip归档失败: {}", e)))?;

    for i in 0..archive.len() {
      let mut file = archive
        .by_index(i)
        .map_err(|e| Error::from_reason(format!("读取Zip条目失败: {}", e)))?;

      // 安全检查：Zip Slip
      let outpath = match file.enclosed_name() {
        Some(path) => self.output_dir.join(path),
        None => continue,
      };

      if file.name().ends_with('/') {
        std::fs::create_dir_all(&outpath)
          .map_err(|e| Error::from_reason(format!("创建目录失败: {}", e)))?;
      } else {
        if let Some(p) = outpath.parent() {
          if !p.exists() {
            std::fs::create_dir_all(p)
              .map_err(|e| Error::from_reason(format!("创建父目录失败: {}", e)))?;
          }
        }
        let mut outfile = File::create(&outpath)
          .map_err(|e| Error::from_reason(format!("创建输出文件失败: {}", e)))?;
        std::io::copy(&mut file, &mut outfile)
          .map_err(|e| Error::from_reason(format!("解压文件内容失败: {}", e)))?;
      }

      // 恢复权限 (Unix only)
      #[cfg(unix)]
      {
        use std::os::unix::fs::PermissionsExt;
        if let Some(mode) = file.unix_mode() {
          std::fs::set_permissions(&outpath, std::fs::Permissions::from_mode(mode))
            .map_err(|e| Error::from_reason(format!("设置文件权限失败: {}", e)))?;
        }
      }
    }

    Ok(())
  }

  fn resolve(&mut self, _env: Env, _output: Self::Output) -> Result<Self::JsValue> {
    Ok(())
  }
}

#[napi(ts_return_type = "Promise<void>")]
pub fn unzip(source_path: String, output_dir: String) -> AsyncTask<UncompressTask> {
  AsyncTask::new(UncompressTask {
    source_path: PathBuf::from(source_path),
    output_dir: PathBuf::from(output_dir),
  })
}
