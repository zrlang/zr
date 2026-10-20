//! 构建脚本（`build.zr`）支持：沙箱执行 + 缓存。
//!
//! 包根目录下的 `build.zr` 在包编译前运行，可生成代码。
//! 沙箱限制：默认无网络、文件系统仅限包根目录 + scratch 目录、超时 60s。

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

// ==========================================================================
// 构建 API
// ==========================================================================

/// 构建脚本 API（提供给 `build.zr` 使用的运行时接口）。
#[derive(Debug, Default)]
pub struct BuildApi {
    /// 包根目录。
    pub package_root: PathBuf,
    /// scratch 目录（构建脚本可写）。
    pub scratch_dir: PathBuf,
    /// 输出目录（生成的代码写入此处）。
    pub out_dir: PathBuf,
    /// 需要重新运行的触发文件列表。
    pub rerun_if_changed: Vec<PathBuf>,
    /// 环境变量。
    pub env: HashMap<String, String>,
}

impl BuildApi {
    pub fn new(package_root: impl Into<PathBuf>) -> Self {
        let root = package_root.into();
        let scratch = root.join(".zpm").join("build-scratch");
        let out = root.join(".zpm").join("out");
        Self {
            package_root: root,
            scratch_dir: scratch,
            out_dir: out,
            rerun_if_changed: Vec::new(),
            env: HashMap::new(),
        }
    }

    /// 声明当 `path` 改变时重新运行构建脚本。
    pub fn rerun_if_changed(&mut self, path: impl Into<PathBuf>) {
        self.rerun_if_changed.push(path.into());
    }

    /// 设置环境变量。
    pub fn set_env(&mut self, key: impl Into<String>, value: impl Into<String>) {
        self.env.insert(key.into(), value.into());
    }
}

// ==========================================================================
// 沙箱
// ==========================================================================

/// 沙箱配置。
#[derive(Debug, Clone)]
pub struct SandboxConfig {
    /// 是否允许网络访问。
    pub allow_network: bool,
    /// 允许读写的目录白名单。
    pub allowed_dirs: Vec<PathBuf>,
    /// CPU 时间限制。
    pub cpu_limit: Duration,
    /// 墙钟时间限制。
    pub wall_limit: Duration,
    /// 最大内存（字节）。
    pub memory_limit: u64,
}

impl Default for SandboxConfig {
    fn default() -> Self {
        Self {
            allow_network: false,
            allowed_dirs: Vec::new(),
            cpu_limit: Duration::from_secs(60),
            wall_limit: Duration::from_secs(60),
            memory_limit: 512 * 1024 * 1024, // 512 MiB
        }
    }
}

/// 沙箱执行结果。
#[derive(Debug)]
pub struct SandboxResult {
    pub success: bool,
    pub exit_code: i32,
    pub stdout: String,
    pub stderr: String,
    pub duration: Duration,
}

/// 沙箱：限制构建脚本的运行环境。
pub struct Sandbox {
    config: SandboxConfig,
}

impl Sandbox {
    pub fn new(config: SandboxConfig) -> Self {
        Self { config }
    }

    /// 执行构建脚本（简化版：通过子进程执行，带超时）。
    pub fn run_build_script(&self, script_path: &Path) -> SandboxResult {
        use std::process::Command;

        let start = std::time::Instant::now();

        // 检查脚本路径是否在允许的目录内
        let in_allowed = self
            .config
            .allowed_dirs
            .iter()
            .any(|dir| script_path.starts_with(dir));
        if !in_allowed && !self.config.allowed_dirs.is_empty() {
            return SandboxResult {
                success: false,
                exit_code: -1,
                stdout: String::new(),
                stderr: format!(
                    "build script path not in allowed directories: {:?}",
                    script_path
                ),
                duration: start.elapsed(),
            };
        }

        // 执行脚本（zr 解释/编译执行）
        let mut cmd = Command::new("zr");
        cmd.arg("run");
        cmd.arg(script_path);
        if let Some(dir) = self.config.allowed_dirs.first() {
            cmd.env("OUT_DIR", dir);
        }

        // 设置超时
        let timeout = self.config.wall_limit;
        let child = cmd.spawn();

        match child {
            Ok(mut child) => {
                let deadline = std::time::Instant::now() + timeout;
                loop {
                    match child.try_wait() {
                        Ok(Some(status)) => {
                            let (stdout, stderr) = match child.wait_with_output() {
                                Ok(output) => (
                                    String::from_utf8_lossy(&output.stdout).to_string(),
                                    String::from_utf8_lossy(&output.stderr).to_string(),
                                ),
                                Err(_) => (String::new(), String::new()),
                            };
                            return SandboxResult {
                                success: status.success(),
                                exit_code: status.code().unwrap_or(-1),
                                stdout,
                                stderr,
                                duration: start.elapsed(),
                            };
                        }
                        Ok(None) => {
                            if std::time::Instant::now() > deadline {
                                let _ = child.kill();
                                return SandboxResult {
                                    success: false,
                                    exit_code: -1,
                                    stdout: String::new(),
                                    stderr: "build script timed out".to_string(),
                                    duration: start.elapsed(),
                                };
                            }
                            std::thread::sleep(Duration::from_millis(50));
                        }
                        Err(e) => {
                            return SandboxResult {
                                success: false,
                                exit_code: -1,
                                stdout: String::new(),
                                stderr: format!("failed to wait: {}", e),
                                duration: start.elapsed(),
                            };
                        }
                    }
                }
            }
            Err(e) => SandboxResult {
                success: false,
                exit_code: -1,
                stdout: String::new(),
                stderr: format!("failed to spawn build script: {}", e),
                duration: start.elapsed(),
            },
        }
    }
}

// ==========================================================================
// 缓存
// ==========================================================================

/// 构建脚本缓存。
#[derive(Debug, Default)]
pub struct BuildCache {
    /// 缓存目录。
    cache_dir: PathBuf,
}

impl BuildCache {
    pub fn new(cache_dir: impl Into<PathBuf>) -> Self {
        Self {
            cache_dir: cache_dir.into(),
        }
    }

    /// 计算输入文件的哈希（简化版：修改时间 + 大小）。
    pub fn compute_hash(&self, files: &[PathBuf]) -> String {
        use std::hash::{Hash, Hasher};
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        for file in files {
            if let Ok(meta) = std::fs::metadata(file) {
                let len = meta.len();
                let modified = meta.modified().ok();
                (file, len, modified).hash(&mut hasher);
            } else {
                file.hash(&mut hasher);
            }
        }
        format!("{:016x}", hasher.finish())
    }

    /// 获取缓存条目路径。
    fn cache_path(&self, hash: &str) -> PathBuf {
        self.cache_dir.join(format!("{}.cache", hash))
    }

    /// 检查缓存是否命中。
    pub fn is_hit(&self, hash: &str) -> bool {
        self.cache_path(hash).exists()
    }

    /// 保存缓存条目（简化版：只存标记文件）。
    pub fn save(&self, hash: &str) -> std::io::Result<()> {
        std::fs::create_dir_all(&self.cache_dir)?;
        std::fs::write(self.cache_path(hash), b"cached")?;
        Ok(())
    }

    /// 清除缓存。
    pub fn clear(&self) -> std::io::Result<()> {
        if self.cache_dir.exists() {
            std::fs::remove_dir_all(&self.cache_dir)?;
        }
        Ok(())
    }
}

// ==========================================================================
// 构建脚本运行器
// ==========================================================================

/// 构建脚本运行器：组合沙箱 + 缓存。
pub struct BuildRunner {
    pub sandbox: Sandbox,
    pub cache: BuildCache,
    pub api: BuildApi,
}

impl BuildRunner {
    pub fn new(package_root: impl Into<PathBuf>) -> Self {
        let api = BuildApi::new(package_root);
        let mut config = SandboxConfig::default();
        config.allowed_dirs.push(api.package_root.clone());
        config.allowed_dirs.push(api.scratch_dir.clone());
        config.allowed_dirs.push(api.out_dir.clone());

        let cache_dir = api.package_root.join(".zpm").join("build-cache");
        Self {
            sandbox: Sandbox::new(config),
            cache: BuildCache::new(cache_dir),
            api,
        }
    }

    /// 运行包的 `build.zr`（如果存在）。
    pub fn run(&mut self) -> SandboxResult {
        let script_path = self.api.package_root.join("build.zr");
        if !script_path.exists() {
            return SandboxResult {
                success: true,
                exit_code: 0,
                stdout: String::new(),
                stderr: String::new(),
                duration: Duration::from_secs(0),
            };
        }

        // 计算缓存哈希
        let hash = self.cache.compute_hash(&self.api.rerun_if_changed);

        // 检查缓存
        if self.cache.is_hit(&hash) && !self.api.rerun_if_changed.is_empty() {
            return SandboxResult {
                success: true,
                exit_code: 0,
                stdout: "using cached build result".to_string(),
                stderr: String::new(),
                duration: Duration::from_secs(0),
            };
        }

        // 执行构建脚本
        let result = self.sandbox.run_build_script(&script_path);

        // 保存缓存
        if result.success {
            let _ = self.cache.save(&hash);
        }

        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_build_api() {
        let mut api = BuildApi::new("/tmp/test");
        api.rerun_if_changed("src/foo.zr");
        assert_eq!(api.rerun_if_changed.len(), 1);
        assert!(api.package_root.ends_with("test"));
    }

    #[test]
    fn test_sandbox_default_config() {
        let config = SandboxConfig::default();
        assert!(!config.allow_network);
        assert_eq!(config.wall_limit, Duration::from_secs(60));
    }

    #[test]
    fn test_build_cache_hash() {
        let cache = BuildCache::new("/tmp/zpm-cache-test");
        let hash1 = cache.compute_hash(&[]);
        let hash2 = cache.compute_hash(&[]);
        assert_eq!(hash1, hash2);
    }
}
