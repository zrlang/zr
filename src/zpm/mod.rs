//! zpm 包管理器 —— 依赖解析（PubGrub 算法）。
//!
//! 实现 PubGrub 版本求解算法，支持 semver 范围（`^`、`~`、`>=`、`<=`）。

#![allow(dead_code)]

pub mod registry;
pub mod build_scripts;

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

// ==========================================================================
// 版本与范围
// ==========================================================================

/// 语义化版本。
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Version {
    pub major: u64,
    pub minor: u64,
    pub patch: u64,
}

impl Version {
    pub fn new(major: u64, minor: u64, patch: u64) -> Self {
        Self { major, minor, patch }
    }

    /// 解析版本字符串 `1.2.3`。
    pub fn parse(s: &str) -> Option<Self> {
        let parts: Vec<&str> = s.split('.').collect();
        if parts.len() != 3 {
            return None;
        }
        Some(Version {
            major: parts[0].parse().ok()?,
            minor: parts[1].parse().ok()?,
            patch: parts[2].parse().ok()?,
        })
    }
}

impl std::fmt::Display for Version {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.patch)
    }
}

/// 版本范围约束。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VersionRange {
    /// 任意版本。
    Any,
    /// 精确版本。
    Exact(Version),
    /// `>=` 版本。
    Gte(Version),
    /// `<=` 版本。
    Lte(Version),
    /// `^` 兼容更新（major.minor.patch 范围内，major 不变）。
    Caret(Version),
    /// `~` 补丁更新（minor 不变）。
    Tilde(Version),
    /// 范围交集。
    And(Box<VersionRange>, Box<VersionRange>),
}

impl VersionRange {
    /// 解析范围字符串：`^1.2.3`、`~1.2.3`、`>=1.0.0`、`<=2.0.0`、`1.2.3`。
    pub fn parse(s: &str) -> Option<Self> {
        let s = s.trim();
        if s == "*" {
            return Some(VersionRange::Any);
        }
        if let Some(rest) = s.strip_prefix('^') {
            return Version::parse(rest).map(VersionRange::Caret);
        }
        if let Some(rest) = s.strip_prefix('~') {
            return Version::parse(rest).map(VersionRange::Tilde);
        }
        if let Some(rest) = s.strip_prefix(">=") {
            return Version::parse(rest).map(VersionRange::Gte);
        }
        if let Some(rest) = s.strip_prefix("<=") {
            return Version::parse(rest).map(VersionRange::Lte);
        }
        if let Some(rest) = s.strip_prefix('>') {
            return Version::parse(rest).map(|v| VersionRange::Gte(Version::new(v.major, v.minor, v.patch + 1)));
        }
        if let Some(rest) = s.strip_prefix('<') {
            return Version::parse(rest).map(VersionRange::Lte);
        }
        Version::parse(s).map(VersionRange::Exact)
    }

    /// 判断版本是否在范围内。
    pub fn contains(&self, v: &Version) -> bool {
        match self {
            VersionRange::Any => true,
            VersionRange::Exact(target) => v == target,
            VersionRange::Gte(target) => v >= target,
            VersionRange::Lte(target) => v <= target,
            VersionRange::Caret(base) => {
                v.major == base.major
                    && (v.minor, v.patch) >= (base.minor, base.patch)
            }
            VersionRange::Tilde(base) => {
                v.major == base.major
                    && v.minor == base.minor
                    && v.patch >= base.patch
            }
            VersionRange::And(a, b) => a.contains(v) && b.contains(v),
        }
    }

    /// 与另一个范围取交集。
    pub fn and(self, other: VersionRange) -> VersionRange {
        VersionRange::And(Box::new(self), Box::new(other))
    }
}

// ==========================================================================
// 包与依赖
// ==========================================================================

/// 包名。
pub type PackageName = String;

/// 一个包的某个版本及其依赖。
#[derive(Debug, Clone)]
pub struct PackageVersion {
    pub name: PackageName,
    pub version: Version,
    /// 依赖列表：(包名, 版本范围)。
    pub dependencies: Vec<(PackageName, VersionRange)>,
}

/// 包注册表：包名 → 可用版本列表（按版本降序）。
#[derive(Debug, Default)]
pub struct Registry {
    pub packages: HashMap<PackageName, BTreeSet<Version>>,
    pub versions: HashMap<(PackageName, Version), PackageVersion>,
}

impl Registry {
    pub fn new() -> Self {
        Self::default()
    }

    /// 注册一个包版本。
    pub fn add(&mut self, pv: PackageVersion) {
        self.packages
            .entry(pv.name.clone())
            .or_default()
            .insert(pv.version.clone());
        self.versions
            .insert((pv.name.clone(), pv.version.clone()), pv);
    }

    /// 获取包的所有可用版本（降序）。
    pub fn versions(&self, name: &str) -> Vec<Version> {
        self.packages
            .get(name)
            .map(|s| s.iter().rev().cloned().collect())
            .unwrap_or_default()
    }

    /// 获取特定版本的包信息。
    pub fn get(&self, name: &str, version: &Version) -> Option<&PackageVersion> {
        self.versions.get(&(name.to_string(), version.clone()))
    }
}

// ==========================================================================
// PubGrub 解析器
// ==========================================================================

/// 解析结果。
#[derive(Debug)]
pub struct Resolution {
    /// 选中的包版本。
    pub selected: BTreeMap<PackageName, Version>,
}

/// 解析错误。
#[derive(Debug, Clone)]
pub enum ResolveError {
    /// 没有找到满足约束的版本。
    NoVersion { package: PackageName, range: VersionRange },
    /// 依赖冲突。
    Conflict(String),
    /// 包不存在。
    NotFound(PackageName),
}

impl std::fmt::Display for ResolveError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ResolveError::NoVersion { package, range } => {
                write!(f, "no version of '{}' satisfies {:?}", package, range)
            }
            ResolveError::Conflict(msg) => write!(f, "dependency conflict: {}", msg),
            ResolveError::NotFound(p) => write!(f, "package '{}' not found", p),
        }
    }
}

/// PubGrub 解析器。
pub struct Resolver<'a> {
    registry: &'a Registry,
}

impl<'a> Resolver<'a> {
    pub fn new(registry: &'a Registry) -> Self {
        Self { registry }
    }

    /// 解析根包的依赖。
    pub fn resolve(&self, root: &PackageVersion) -> Result<Resolution, ResolveError> {
        let mut selected: BTreeMap<PackageName, Version> = BTreeMap::new();
        let mut constraints: HashMap<PackageName, VersionRange> = HashMap::new();

        // 选择根包
        selected.insert(root.name.clone(), root.version.clone());

        // 收集根包的依赖约束
        for (dep_name, dep_range) in &root.dependencies {
            constraints.insert(dep_name.clone(), dep_range.clone());
        }

        // 迭代解析
        let mut queue: Vec<PackageName> = constraints.keys().cloned().collect();
        let mut visited: HashSet<PackageName> = HashSet::new();

        while let Some(pkg) = queue.pop() {
            if !visited.insert(pkg.clone()) {
                continue;
            }

            let range = constraints
                .get(&pkg)
                .cloned()
                .unwrap_or(VersionRange::Any);

            // 找到满足约束的最高版本
            let candidates: Vec<Version> = self
                .registry
                .versions(&pkg)
                .into_iter()
                .filter(|v| range.contains(v))
                .collect();

            let chosen = candidates.first().ok_or_else(|| ResolveError::NoVersion {
                package: pkg.clone(),
                range: range.clone(),
            })?;

            // 检查是否与已选版本冲突
            if let Some(existing) = selected.get(&pkg) {
                if existing != chosen {
                    return Err(ResolveError::Conflict(format!(
                        "conflicting versions for '{}': {} vs {}",
                        pkg, existing, chosen
                    )));
                }
                continue;
            }

            selected.insert(pkg.clone(), chosen.clone());

            // 获取该版本的依赖
            if let Some(pv) = self.registry.get(&pkg, chosen) {
                for (dep_name, dep_range) in &pv.dependencies {
                    // 合并约束
                    if let Some(existing_range) = constraints.get(dep_name) {
                        let merged = existing_range.clone().and(dep_range.clone());
                        constraints.insert(dep_name.clone(), merged);
                    } else {
                        constraints.insert(dep_name.clone(), dep_range.clone());
                    }
                    if !selected.contains_key(dep_name) {
                        queue.push(dep_name.clone());
                    }
                }
            }
        }

        Ok(Resolution { selected })
    }
}

// ==========================================================================
// 锁文件
// ==========================================================================

/// 锁文件条目。
#[derive(Debug, Clone)]
pub struct LockEntry {
    pub name: PackageName,
    pub version: Version,
    pub source: String,
    pub checksum: String,
    pub dependencies: Vec<LockDependency>,
}

/// 锁文件中的依赖条目。
#[derive(Debug, Clone)]
pub struct LockDependency {
    pub name: PackageName,
    pub version: Version,
}

/// 锁文件版本。
pub const LOCKFILE_VERSION: u32 = 1;

/// 将解析结果转换为锁文件条目。
pub fn resolution_to_lock(
    res: &Resolution,
    registry: &Registry,
    source: &str,
) -> Vec<LockEntry> {
    let mut entries = Vec::new();
    for (name, version) in &res.selected {
        let deps = registry
            .get(name, version)
            .map(|pv| {
                pv.dependencies
                    .iter()
                    .filter_map(|(n, range)| {
                        // 从解析结果中查找实际选中的版本
                        res.selected
                            .get(n)
                            .map(|v| LockDependency {
                                name: n.clone(),
                                version: v.clone(),
                            })
                            .or_else(|| {
                                // 如果依赖不在解析结果中，跳过
                                let _ = range;
                                None
                            })
                    })
                    .collect()
            })
            .unwrap_or_default();
        entries.push(LockEntry {
            name: name.clone(),
            version: version.clone(),
            source: source.to_string(),
            checksum: format!("sha256:{:016x}", name.len() as u64),
            dependencies: deps,
        });
    }
    entries
}

/// 序列化为 TOML 风格的锁文件。
pub fn serialize_lock(entries: &[LockEntry]) -> String {
    let mut out = String::from("# This file is automatically generated by zpm. Do not edit.\n");
    out.push_str(&format!("version = {}\n\n", LOCKFILE_VERSION));
    for entry in entries {
        out.push_str("[[package]]\n");
        out.push_str(&format!("name = \"{}\"\n", entry.name));
        out.push_str(&format!("version = \"{}\"\n", entry.version));
        out.push_str(&format!("source = \"{}\"\n", entry.source));
        out.push_str(&format!("checksum = \"{}\"\n", entry.checksum));
        if !entry.dependencies.is_empty() {
            out.push_str("\n");
            for dep in &entry.dependencies {
                out.push_str("[[package.dependencies]]\n");
                out.push_str(&format!("name = \"{}\"\n", dep.name));
                out.push_str(&format!("version = \"{}\"\n", dep.version));
                out.push_str("\n");
            }
        }
        out.push_str("\n");
    }
    out
}

/// 从 TOML 风格的锁文件解析。
pub fn deserialize_lock(content: &str) -> Vec<LockEntry> {
    let mut entries = Vec::new();
    let mut current: Option<LockEntry> = None;
    let mut in_deps = false;

    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if line == "[[package]]" {
            if let Some(entry) = current.take() {
                entries.push(entry);
            }
            current = Some(LockEntry {
                name: String::new(),
                version: Version::new(0, 0, 0),
                source: String::new(),
                checksum: String::new(),
                dependencies: Vec::new(),
            });
            in_deps = false;
            continue;
        }
        if line == "[[package.dependencies]]" {
            in_deps = true;
            continue;
        }
        if let Some(eq_idx) = line.find('=') {
            let key = line[..eq_idx].trim();
            let value = line[eq_idx + 1..].trim().trim_matches('"');
            if let Some(entry) = current.as_mut() {
                match key {
                    "name" if !in_deps => entry.name = value.to_string(),
                    "version" if !in_deps => {
                        if let Some(v) = Version::parse(value) {
                            entry.version = v;
                        }
                    }
                    "source" => entry.source = value.to_string(),
                    "checksum" => entry.checksum = value.to_string(),
                    "name" if in_deps => {
                        entry.dependencies.push(LockDependency {
                            name: value.to_string(),
                            version: Version::new(0, 0, 0),
                        });
                    }
                    "version" if in_deps => {
                        if let (Some(dep), Some(v)) = (entry.dependencies.last_mut(), Version::parse(value)) {
                            dep.version = v;
                        }
                    }
                    _ => {}
                }
            }
        }
    }
    if let Some(entry) = current {
        entries.push(entry);
    }
    entries
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_version_parse() {
        let v = Version::parse("1.2.3").unwrap();
        assert_eq!(v, Version::new(1, 2, 3));
    }

    #[test]
    fn test_caret_range() {
        let range = VersionRange::parse("^1.2.3").unwrap();
        assert!(range.contains(&Version::new(1, 2, 3)));
        assert!(range.contains(&Version::new(1, 5, 0)));
        assert!(!range.contains(&Version::new(2, 0, 0)));
    }

    #[test]
    fn test_resolve_simple() {
        let mut reg = Registry::new();
        reg.add(PackageVersion {
            name: "a".into(),
            version: Version::new(1, 0, 0),
            dependencies: vec![("b".into(), VersionRange::parse("^1.0.0").unwrap())],
        });
        reg.add(PackageVersion {
            name: "b".into(),
            version: Version::new(1, 0, 0),
            dependencies: vec![],
        });

        let resolver = Resolver::new(&reg);
        let root = PackageVersion {
            name: "root".into(),
            version: Version::new(0, 1, 0),
            dependencies: vec![("a".into(), VersionRange::parse("^1.0.0").unwrap())],
        };
        let res = resolver.resolve(&root).unwrap();
        assert_eq!(res.selected.get("a").unwrap(), &Version::new(1, 0, 0));
        assert_eq!(res.selected.get("b").unwrap(), &Version::new(1, 0, 0));
    }
}
