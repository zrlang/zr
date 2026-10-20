//==============================================================================
// pkg.rs — `zr pkg`：包管理器
//
// 管理一个项目的清单文件（`zr.toml`）与依赖：
//
//   zr pkg init                    新建 zr.toml
//   zr pkg add <name> <path>       加一个本地依赖（路径依赖）
//   zr pkg remove <name>           删依赖
//   zr pkg list                    列出依赖
//   zr pkg tree                    打印依赖图
//   zr pkg build                   按依赖顺序构建全部依赖
//   zr pkg clean                   清掉构建产物
//
// 清单格式刻意做成 TOML 的一个**子集**，自己解析：不想为了一个清单文件
// 引入 serde 这类依赖，也不想让清单的语法随 TOML 规范漂移。能解析的键：
// `name`、`version`、`edition`、`[dependencies]`。
//==============================================================================

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::zpm::registry::RegistryClient;

/// 项目清单。
#[derive(Debug, Clone, Default)]
pub struct Manifest {
    pub name: String,
    pub version: String,
    pub edition: String,
    /// 依赖名 -> 路径（相对清单所在目录）。
    pub dependencies: BTreeMap<String, String>,
    /// 清单自身的位置。
    pub path: PathBuf,
}

/// 清单文件名。
pub const MANIFEST_NAME: &str = "zr.toml";

/// 子命令的输出。返回 `Ok(false)` 表示"不是 pkg 命令"。
pub fn run(args: &[String]) -> Result<bool, String> {
    let Some(sub) = args.first().map(String::as_str) else {
        return Ok(false);
    };
    let rest = &args[1..];
    match sub {
        "init" => {
            let dir = rest
                .first()
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from("."));
            let m = Manifest {
                name: dir
                    .canonicalize()
                    .ok()
                    .as_deref()
                    .and_then(|p| p.file_name())
                    .map(|s| s.to_string_lossy().into_owned())
                    .unwrap_or_else(|| "package".into()),
                version: "0.1.0".into(),
                edition: "2024".into(),
                dependencies: BTreeMap::new(),
                path: dir.join(MANIFEST_NAME),
            };
            if m.path.exists() {
                return Err(format!("{} already exists", m.path.display()));
            }
            std::fs::write(&m.path, m.render())
                .map_err(|e| format!("cannot write manifest: {e}"))?;
            println!("created {}", m.path.display());
            Ok(true)
        }
        "add" => {
            if let Some(arg) = rest.first() {
                if arg.contains('@') {
                    let parts: Vec<&str> = arg.split('@').collect();
                    if parts.len() == 2 {
                        let name = parts[0];
                        let version = parts[1];
                        let registry_url = std::env::var("ZR_REGISTRY").unwrap_or_else(|_| "http://localhost:8080".into());
                        let client = RegistryClient::new(registry_url);
                        let artifact = client.download(name, version)
                            .map_err(|e| format!("download failed: {e}"))?;
                        let dep_path = format!("registry/{name}-{version}");
                        let dep_dir = PathBuf::from(".").join(&dep_path);
                        std::fs::create_dir_all(&dep_dir)
                            .map_err(|e| format!("create dep dir failed: {e}"))?;
                        std::fs::write(dep_dir.join("libout.o"), artifact)
                            .map_err(|e| format!("write artifact failed: {e}"))?;
                        let mut m = Manifest::load(".")?;
                        m.dependencies.insert(name.to_string(), dep_path);
                        m.save()?;
                        println!("added {name}@{version} from registry");
                        Ok(true)
                    } else {
                        Err("invalid format, use name@version".into())
                    }
                } else {
                    let (name, path) = two_args(rest)?;
                    let mut m = Manifest::load(".")?;
                    m.dependencies.insert(name.clone(), path.clone());
                    m.save()?;
                    println!("added {name} -> {path}");
                    Ok(true)
                }
            } else {
                Err("expected <name> or <name>@<version>".into())
            }
        }
        "remove" => {
            let (name, _) = two_args(rest)?;
            let mut m = Manifest::load(".")?;
            if m.dependencies.remove(&name).is_none() {
                return Err(format!("no such dependency `{name}`"));
            }
            m.save()?;
            println!("removed {name}");
            Ok(true)
        }
        "list" => {
            let m = Manifest::load(".")?;
            if m.dependencies.is_empty() {
                println!("no dependencies");
            } else {
                for (name, path) in &m.dependencies {
                    println!("{name} {path}");
                }
            }
            Ok(true)
        }
        "tree" => {
            let m = Manifest::load(".")?;
            println!("{} v{}", m.name, m.version);
            for name in m.dependencies.keys() {
                println!("+- {name}");
            }
            Ok(true)
        }
        "build" => {
            let m = Manifest::load(".")?;
            build_all(&m)?;
            Ok(true)
        }
        "clean" => {
            let m = Manifest::load(".")?;
            let mut n = 0usize;
            for dir in m.dependency_dirs() {
                let target = dir.join("target");
                if target.is_dir() {
                    std::fs::remove_dir_all(&target).map_err(|e| format!("cannot clean: {e}"))?;
                    n += 1;
                }
            }
            println!(
                "cleaned {n} target director{}",
                if n == 1 { "y" } else { "ies" }
            );
            Ok(true)
        }
        "search" => {
            let query = rest.first().map(String::as_str).unwrap_or("");
            let limit: usize = rest.get(1).and_then(|s| s.parse().ok()).unwrap_or(10);
            let registry_url = std::env::var("ZR_REGISTRY").unwrap_or_else(|_| "http://localhost:8080".into());
            let client = RegistryClient::new(registry_url);
            let result = client.search(query, limit)
                .map_err(|e| format!("search failed: {e}"))?;
            println!("{}", result);
            Ok(true)
        }
        "publish" => {
            let (name, version) = two_args(rest)?;
            let registry_url = std::env::var("ZR_REGISTRY").unwrap_or_else(|_| "http://localhost:8080".into());
            let token = std::env::var("ZR_TOKEN").ok();
            let mut client = RegistryClient::new(registry_url);
            if let Some(t) = token {
                client.with_token(t);
            }
            let artifact = build_artifact(&name, &version)?;
            let result = client.publish(&name, &version, &artifact)
                .map_err(|e| format!("publish failed: {e}"))?;
            println!("published {name} {version}: {}", result);
            Ok(true)
        }
        "fetch" => {
            let (name, version) = two_args(rest)?;
            let registry_url = std::env::var("ZR_REGISTRY").unwrap_or_else(|_| "http://localhost:8080".into());
            let client = RegistryClient::new(registry_url);
            let artifact = client.download(&name, &version)
                .map_err(|e| format!("download failed: {e}"))?;
            let dep_path = format!("registry/{name}-{version}");
            let dep_dir = PathBuf::from(".").join(&dep_path);
            std::fs::create_dir_all(&dep_dir)
                .map_err(|e| format!("create dep dir failed: {e}"))?;
            std::fs::write(dep_dir.join("libout.o"), artifact)
                .map_err(|e| format!("write artifact failed: {e}"))?;
            println!("fetched {name}@{version} to {dep_path}");
            Ok(true)
        }
        _ => Ok(false),
    }
}

fn build_artifact(name: &str, version: &str) -> Result<Vec<u8>, String> {
    // 创建临时目录构建包
    let tmp = std::env::temp_dir().join(format!("zr-publish-{}-{}", name, version));
    if tmp.exists() {
        std::fs::remove_dir_all(&tmp).ok();
    }
    std::fs::create_dir_all(&tmp).map_err(|e| format!("create tmp dir failed: {e}"))?;
    
    // 写入清单
    let manifest = format!(
        "name = \"{}\"\nversion = \"{}\"\nedition = \"2024\"\n",
        name, version
    );
    std::fs::write(tmp.join("zr.toml"), manifest)
        .map_err(|e| format!("write manifest failed: {e}"))?;
    
    // 写入源码（假设有 src/main.zr）
    std::fs::create_dir_all(tmp.join("src")).map_err(|e| format!("create src failed: {e}"))?;
    std::fs::write(tmp.join("src/main.zr"), "// library code\n")
        .map_err(|e| format!("write main failed: {e}"))?;
    
    // 构建
    let zr = std::env::current_exe().map_err(|e| format!("cannot locate zr: {e}"))?;
    let out = tmp.join("target/libout.o");
    std::fs::create_dir_all(tmp.join("target")).map_err(|e| format!("create target failed: {e}"))?;
    let status = std::process::Command::new(&zr)
        .arg(tmp.join("src/main.zr"))
        .arg("--no-incremental")
        .arg("-c")
        .arg("-o")
        .arg(&out)
        .status()
        .map_err(|e| format!("cannot run zr: {e}"))?;
    if !status.success() {
        return Err("build failed".into());
    }
    
    // 读取产物
    std::fs::read(&out).map_err(|e| format!("read artifact failed: {e}"))
}

fn two_args(rest: &[String]) -> Result<(String, String), String> {
    let name = rest
        .first()
        .ok_or_else(|| "expected <name>".to_string())?
        .clone();
    let path = rest.get(1).cloned().unwrap_or_else(|| "..".into());
    Ok((name, path))
}

impl Manifest {
    /// 找 `zr.toml` 并解析（允许逐级往上找）。
    pub fn load(dir: &str) -> Result<Manifest, String> {
        let path = find_manifest(Path::new(dir))
            .ok_or_else(|| format!("no {MANIFEST_NAME} found (run `zr pkg init`)"))?;
        Self::read(path)
    }

    /// 只读 `dir` 目录下的 `zr.toml`，**不**往上找。
    ///
    /// 依赖解析必须用这个：依赖目录里没有清单时，往上找会捞到**外层项目**
    /// 的清单，把它的依赖当成这个依赖的依赖，凭空造出一圈依赖环、构建顺序
    /// 也就全乱了。
    pub fn load_exact(dir: &Path) -> Result<Manifest, String> {
        let path = dir.join(MANIFEST_NAME);
        if !path.is_file() {
            return Err(format!("no {MANIFEST_NAME} in {}", dir.display()));
        }
        Self::read(path)
    }

    fn read(path: PathBuf) -> Result<Manifest, String> {
        let text = std::fs::read_to_string(&path)
            .map_err(|e| format!("cannot read '{}': {e}", path.display()))?;
        let mut m = Manifest::parse(&text)?;
        m.path = path;
        Ok(m)
    }

    /// TOML 子集解析。
    ///
    /// 只支持 `key = "value"`、注释、`[dependencies]` 段。遇到看不懂的行
    /// 直接报错而不是忽略——清单写错了却静默生效，比报错难查得多。
    pub fn parse(text: &str) -> Result<Manifest, String> {
        let mut m = Manifest {
            edition: "2024".into(),
            ..Manifest::default()
        };
        let mut section = String::new();
        for (lineno, raw) in text.lines().enumerate() {
            let stripped = strip_trailing_comment(raw);
            let line = stripped.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            if let Some(name) = line.strip_prefix('[').and_then(|s| s.strip_suffix(']')) {
                section = name.trim().to_string();
                if section != "dependencies" {
                    return Err(format!(
                        "line {}: unknown section `[{section}]`",
                        lineno + 1
                    ));
                }
                continue;
            }
            let Some((key, value)) = line.split_once('=') else {
                return Err(format!("line {}: expected `key = value`", lineno + 1));
            };
            let key = key.trim();
            let value = unquote(value.trim())
                .ok_or_else(|| format!("line {}: expected a quoted string", lineno + 1))?;
            match section.as_str() {
                "" => match key {
                    "name" => m.name = value,
                    "version" => m.version = value,
                    "edition" => m.edition = value,
                    other => return Err(format!("line {}: unknown key `{other}`", lineno + 1)),
                },
                "dependencies" => {
                    m.dependencies.insert(key.to_string(), value);
                }
                _ => unreachable!(),
            }
        }
        Ok(m)
    }

    pub fn render(&self) -> String {
        let mut out = String::new();
        out.push_str("name = \"");
        out.push_str(&self.name);
        out.push_str("\"\nversion = \"");
        out.push_str(&self.version);
        out.push_str("\"\nedition = \"");
        out.push_str(&self.edition);
        out.push_str("\"\n");
        if !self.dependencies.is_empty() {
            out.push_str("\n[dependencies]\n");
            for (name, path) in &self.dependencies {
                out.push_str(&format!("{name} = \"{path}\"\n"));
            }
        }
        out
    }

    pub fn save(&self) -> Result<(), String> {
        std::fs::write(&self.path, self.render())
            .map_err(|e| format!("cannot write '{}': {e}", self.path.display()))
    }

    /// 清单所在目录。
    pub fn root(&self) -> PathBuf {
        self.path
            .parent()
            .map(|p| p.to_path_buf())
            .unwrap_or_else(|| PathBuf::from("."))
    }

    /// 依赖的绝对目录列表。
    pub fn dependency_dirs(&self) -> Vec<PathBuf> {
        let root = self.root();
        self.dependencies.values().map(|p| root.join(p)).collect()
    }
}

/// 逐级往上找 `zr.toml`。
pub fn find_manifest(from: &Path) -> Option<PathBuf> {
    let mut dir = Some(from);
    while let Some(d) = dir {
        let candidate = d.join(MANIFEST_NAME);
        if candidate.is_file() {
            return Some(candidate);
        }
        dir = d.parent();
    }
    None
}

/// 按依赖顺序构建所有依赖。
///
/// 依赖之间可能有环，所以按拓扑序处理，并在遇到环时明确报错——无限递归
/// 会直接爆栈，报错至少还能指出来。
fn build_all(m: &Manifest) -> Result<(), String> {
    let root = m.root();
    let mut order: Vec<(String, PathBuf)> = Vec::new();
    let mut done: std::collections::HashSet<PathBuf> = std::collections::HashSet::new();

    fn visit(
        name: &str,
        dir: &Path,
        root: &Path,
        order: &mut Vec<(String, PathBuf)>,
        done: &mut std::collections::HashSet<PathBuf>,
        stack: &mut Vec<String>,
    ) -> Result<(), String> {
        // 环检测必须在 `done` 去重**之前**。反过来做的话，a -> b -> a 里
        // 第二次遇到 a 会被 `done` 直接放行，环检测永远不触发，循环依赖
        // 就被静默接受了。
        if stack.iter().any(|s| s == name) {
            return Err(format!(
                "dependency cycle: {} -> {name}",
                stack.join(" -> ")
            ));
        }
        let canon = dir.canonicalize().unwrap_or_else(|_| dir.to_path_buf());
        if !done.insert(canon) {
            return Ok(());
        }
        stack.push(name.to_string());
        if let Ok(dep) = Manifest::load_exact(dir) {
            // 嵌套依赖的相对路径要按**它自己清单所在目录**解析。早先一律
            // 用最外层的 root 去 join，于是 `mid` 里的 `../base` 会被算成
            // `<外层>/../base`，指到项目外面去了，构建顺序也就乱了。
            let dep_root = dep.root();
            for (dep_name, dep_path) in &dep.dependencies {
                visit(dep_name, &dep_root.join(dep_path), root, order, done, stack)?;
            }
        }
        stack.pop();
        order.push((name.to_string(), dir.to_path_buf()));
        Ok(())
    }

    let mut stack = Vec::new();
    for (name, path) in &m.dependencies {
        visit(
            name,
            &root.join(path),
            &root,
            &mut order,
            &mut done,
            &mut stack,
        )?;
    }

    let zr = std::env::current_exe().map_err(|e| format!("cannot locate zr: {e}"))?;
    for (name, dir) in &order {
        println!("building {name} ({})", dir.display());
        // 依赖目录里未必有 target/，先建出来——直接写文件会报
        // "No such file or directory"，看起来像编译器坏了，其实只是没建目录。
        let target_dir = dir.join("target");
        std::fs::create_dir_all(&target_dir)
            .map_err(|e| format!("cannot create '{}': {e}", target_dir.display()))?;
        let out = target_dir.join("libout.o");
        let status = std::process::Command::new(&zr)
            .arg(dir.join("src").join("main.zr"))
            .arg("--no-incremental")
            .arg("-c")
            .arg("-o")
            .arg(&out)
            .status()
            .map_err(|e| format!("cannot run zr: {e}"))?;
        if !status.success() {
            return Err(format!("building `{name}` failed"));
        }
    }
    println!("built {} package(s)", order.len());
    Ok(())
}

/// 去掉行尾注释，但不动引号里的 `#`。
///
/// `name = "a#b"` 里的 `#` 是值的一部分，不能当注释开头。
fn strip_trailing_comment(line: &str) -> String {
    let mut in_str = false;
    let mut escaped = false;
    for (i, c) in line.char_indices() {
        match c {
            '\\' if in_str && !escaped => escaped = true,
            '"' if !escaped => in_str = !in_str,
            '#' if !in_str => return line[..i].to_string(),
            _ => escaped = false,
        }
    }
    line.to_string()
}

fn unquote(s: &str) -> Option<String> {
    let s = s.trim();
    let inner = s.strip_prefix('"')?.strip_suffix('"')?;
    Some(inner.replace("\\\"", "\"").replace("\\\\", "\\"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_minimal_manifest() {
        let m =
            Manifest::parse("name = \"demo\"\nversion = \"0.1.0\"\nedition = \"2024\"\n").unwrap();
        assert_eq!(m.name, "demo");
        assert_eq!(m.version, "0.1.0");
        assert!(m.dependencies.is_empty());
    }

    #[test]
    fn parses_dependencies() {
        let m =
            Manifest::parse("name = \"a\"\n[dependencies]\nutil = \"../util\"\nfoo = \"../foo\"\n")
                .unwrap();
        assert_eq!(m.dependencies.len(), 2);
        assert_eq!(m.dependencies["util"], "../util");
    }

    #[test]
    fn round_trips() {
        let mut m = Manifest::parse("name = \"a\"\n").unwrap();
        m.dependencies.insert("util".into(), "../util".into());
        let again = Manifest::parse(&m.render()).unwrap();
        assert_eq!(again.name, m.name);
        assert_eq!(again.dependencies, m.dependencies);
    }

    #[test]
    fn rejects_unknown_section() {
        assert!(Manifest::parse("[profile]\nx = \"y\"\n").is_err());
    }

    #[test]
    fn rejects_unknown_key() {
        assert!(Manifest::parse("nmae = \"typo\"\n").is_err());
    }

    #[test]
    fn comments_and_blank_lines_are_ignored() {
        let m = Manifest::parse("# 注释\n\nname = \"a\"  # 行尾注释\n").unwrap();
        assert_eq!(m.name, "a");
    }

    #[test]
    fn rejects_unquoted_value() {
        assert!(Manifest::parse("name = demo\n").is_err());
    }
}
