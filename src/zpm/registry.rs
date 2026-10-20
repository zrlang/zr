//! zpm 注册中心协议：REST API 端点定义与内置 HTTP 服务器。
//!
//! 实现了文档中定义的 6 个端点，使用 std 的 TcpListener 提供 HTTP 服务。

use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex};

// ==========================================================================
// 数据模型
// ==========================================================================

/// 包元数据。
#[derive(Debug, Clone)]
pub struct CrateMeta {
    pub name: String,
    pub description: String,
    pub latest_version: String,
    pub versions: Vec<String>,
}

/// 版本元数据。
#[derive(Debug, Clone)]
pub struct VersionMeta {
    pub name: String,
    pub version: String,
    pub checksum: String,
    pub yanked: bool,
}

/// 注册中心状态。
#[derive(Debug, Default)]
pub struct RegistryState {
    pub crates: HashMap<String, CrateMeta>,
    pub versions: HashMap<(String, String), VersionMeta>,
    /// 包的 tarball 内容：(name, version) → bytes。
    pub artifacts: HashMap<(String, String), Vec<u8>>,
    /// 依赖关系：(name, version) → [(dep_name, version_range)]。
    pub deps: HashMap<(String, String), Vec<(String, String)>>,
}

impl RegistryState {
    pub fn new() -> Self {
        Self::default()
    }

    /// 列出所有包。
    pub fn list_crates(&self) -> Vec<&CrateMeta> {
        self.crates.values().collect()
    }

    /// 获取包元数据。
    pub fn get_crate(&self, name: &str) -> Option<&CrateMeta> {
        self.crates.get(name)
    }

    /// 获取版本元数据。
    pub fn get_version(&self, name: &str, version: &str) -> Option<&VersionMeta> {
        self.versions.get(&(name.to_string(), version.to_string()))
    }

    /// 获取版本的依赖树。
    pub fn get_deps(&self, name: &str, version: &str) -> Option<&Vec<(String, String)>> {
        self.deps.get(&(name.to_string(), version.to_string()))
    }

    /// 下载 tarball。
    pub fn download(&self, name: &str, version: &str) -> Option<&Vec<u8>> {
        self.artifacts.get(&(name.to_string(), version.to_string()))
    }

    /// 发布新版本。
    pub fn publish(&mut self, meta: VersionMeta, artifact: Vec<u8>, deps: Vec<(String, String)>) {
        let key = (meta.name.clone(), meta.version.clone());
        self.versions.insert(key.clone(), meta.clone());
        self.artifacts.insert(key.clone(), artifact);
        self.deps.insert(key, deps);

        // 更新包元数据
        let crate_meta = self.crates.entry(meta.name.clone()).or_insert(CrateMeta {
            name: meta.name.clone(),
            description: String::new(),
            latest_version: String::new(),
            versions: Vec::new(),
        });
        if !crate_meta.versions.contains(&meta.version) {
            crate_meta.versions.push(meta.version.clone());
        }
        crate_meta.latest_version = meta.version;
    }
}

// ==========================================================================
// HTTP 服务器
// ==========================================================================

/// 注册中心服务器。
pub struct RegistryServer {
    state: Arc<Mutex<RegistryState>>,
    addr: String,
}

impl RegistryServer {
    pub fn new(addr: impl Into<String>) -> Self {
        Self {
            state: Arc::new(Mutex::new(RegistryState::new())),
            addr: addr.into(),
        }
    }

    /// 启动服务器（阻塞）。
    pub fn run(&self) -> std::io::Result<()> {
        let listener = TcpListener::bind(&self.addr)?;
        eprintln!("zpm registry listening on {}", self.addr);
        for stream in listener.incoming() {
            match stream {
                Ok(stream) => {
                    let state = self.state.clone();
                    std::thread::spawn(move || {
                        handle_connection(stream, state);
                    });
                }
                Err(e) => eprintln!("connection error: {}", e),
            }
        }
        Ok(())
    }
}

fn handle_connection(mut stream: TcpStream, state: Arc<Mutex<RegistryState>>) {
    let mut buffer = [0u8; 4096];
    let bytes_read = match stream.read(&mut buffer) {
        Ok(n) => n,
        Err(_) => return,
    };
    if bytes_read == 0 {
        return;
    }

    let request = String::from_utf8_lossy(&buffer[..bytes_read]);
    let request_line = request.lines().next().unwrap_or("");
    let parts: Vec<&str> = request_line.split_whitespace().collect();
    if parts.len() < 2 {
        send_response(&mut stream, 400, "Bad Request", "");
        return;
    }
    let method = parts[0];
    let path = parts[1];

    // 路由
    let response = route(method, path, &state);
    let (status, status_text, body, content_type) = response;
    let header = format!(
        "HTTP/1.1 {} {}\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        status,
        status_text,
        content_type,
        body.len()
    );
    let _ = stream.write_all(header.as_bytes());
    let _ = stream.write_all(body.as_bytes());
    let _ = stream.flush();
}

type Response = (u16, &'static str, String, &'static str);

fn route(method: &str, path: &str, state: &Arc<Mutex<RegistryState>>) -> Response {
    let segments: Vec<&str> = path.trim_start_matches('/').split('/').collect();

    match (method, segments.as_slice()) {
        // GET /api/v1/crates
        ("GET", ["api", "v1", "crates"]) => {
            let state = state.lock().unwrap();
            let crates = state.list_crates();
            let json = format!(
                "[{}]",
                crates
                    .iter()
                    .map(|c| format!(
                        "{{\"name\":\"{}\",\"latest_version\":\"{}\",\"versions\":{}}}",
                        c.name,
                        c.latest_version,
                        json_string_array(&c.versions)
                    ))
                    .collect::<Vec<_>>()
                    .join(",")
            );
            (200, "OK", json, "application/json")
        }
        // GET /api/v1/crates/:name
        ("GET", ["api", "v1", "crates", name]) => {
            let state = state.lock().unwrap();
            match state.get_crate(name) {
                Some(c) => {
                    let json = format!(
                        "{{\"name\":\"{}\",\"description\":\"{}\",\"latest_version\":\"{}\",\"versions\":{}}}",
                        c.name,
                        c.description,
                        c.latest_version,
                        json_string_array(&c.versions)
                    );
                    (200, "OK", json, "application/json")
                }
                None => (
                    404,
                    "Not Found",
                    "{\"error\":\"not found\"}".to_string(),
                    "application/json",
                ),
            }
        }
        // GET /api/v1/crates/:name/:version
        ("GET", ["api", "v1", "crates", name, version]) => {
            let state = state.lock().unwrap();
            match state.get_version(name, version) {
                Some(v) => {
                    let json = format!(
                        "{{\"name\":\"{}\",\"version\":\"{}\",\"checksum\":\"{}\",\"yanked\":{}}}",
                        v.name, v.version, v.checksum, v.yanked
                    );
                    (200, "OK", json, "application/json")
                }
                None => (
                    404,
                    "Not Found",
                    "{\"error\":\"not found\"}".to_string(),
                    "application/json",
                ),
            }
        }
        // GET /api/v1/crates/:name/:version/deps
        ("GET", ["api", "v1", "crates", name, version, "deps"]) => {
            let state = state.lock().unwrap();
            match state.get_deps(name, version) {
                Some(deps) => {
                    let json = format!(
                        "[{}]",
                        deps.iter()
                            .map(|(n, r)| format!("{{\"name\":\"{}\",\"req\":\"{}\"}}", n, r))
                            .collect::<Vec<_>>()
                            .join(",")
                    );
                    (200, "OK", json, "application/json")
                }
                None => (
                    404,
                    "Not Found",
                    "{\"error\":\"not found\"}".to_string(),
                    "application/json",
                ),
            }
        }
        // GET /api/v1/crates/:name/:version/download
        ("GET", ["api", "v1", "crates", name, version, "download"]) => {
            let state = state.lock().unwrap();
            match state.download(name, version) {
                Some(artifact) => (
                    200,
                    "OK",
                    String::from_utf8_lossy(artifact).to_string(),
                    "application/x-tar+gzip",
                ),
                None => (
                    404,
                    "Not Found",
                    "{\"error\":\"not found\"}".to_string(),
                    "application/json",
                ),
            }
        }
        // PUT /api/v1/crates/:name/:version
        ("PUT", ["api", "v1", "crates", name, version]) => {
            let mut state = state.lock().unwrap();
            let meta = VersionMeta {
                name: name.to_string(),
                version: version.to_string(),
                checksum: String::from("dummy"),
                yanked: false,
            };
            state.publish(meta, Vec::new(), Vec::new());
            (200, "OK", "{\"ok\":true}".to_string(), "application/json")
        }
        _ => (
            404,
            "Not Found",
            "{\"error\":\"not found\"}".to_string(),
            "application/json",
        ),
    }
}

fn json_string_array(items: &[String]) -> String {
    format!(
        "[{}]",
        items
            .iter()
            .map(|s| format!("\"{}\"", s))
            .collect::<Vec<_>>()
            .join(",")
    )
}

fn send_response(stream: &mut TcpStream, status: u16, status_text: &str, body: &str) {
    let header = format!(
        "HTTP/1.1 {} {}\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        status,
        status_text,
        body.len()
    );
    let _ = stream.write_all(header.as_bytes());
    let _ = stream.write_all(body.as_bytes());
    let _ = stream.flush();
}

// ==========================================================================
// 客户端
// ==========================================================================

/// 注册中心客户端。
pub struct RegistryClient {
    base_url: String,
    token: Option<String>,
}

impl RegistryClient {
    pub fn new(base_url: impl Into<String>) -> Self {
        Self {
            base_url: base_url.into(),
            token: None,
        }
    }

    /// 设置认证 token。
    pub fn with_token(&mut self, token: impl Into<String>) {
        self.token = Some(token.into());
    }

    fn request(&self, method: &str, path: &str, body: Option<String>) -> std::io::Result<String> {
        let host = self
            .base_url
            .trim_start_matches("http://")
            .trim_start_matches("https://")
            .to_string();
        let mut stream = TcpStream::connect(host)?;
        let auth_header = self
            .token
            .as_ref()
            .map(|t| format!("Authorization: Bearer {t}\r\n"))
            .unwrap_or_default();
        let body_bytes = body.unwrap_or_default();
        let request = format!(
            "{} {} HTTP/1.1\r\nHost: {}\r\n{auth_header}Content-Length: {}\r\nConnection: close\r\n\r\n{}",
            method,
            path,
            self.base_url,
            body_bytes.len(),
            body_bytes
        );
        stream.write_all(request.as_bytes())?;
        let mut response = String::new();
        stream.read_to_string(&mut response)?;
        if let Some(idx) = response.find("\r\n\r\n") {
            Ok(response[idx + 4..].to_string())
        } else {
            Ok(response)
        }
    }

    /// 列出所有包。
    pub fn list_crates(&self) -> std::io::Result<String> {
        self.get("/api/v1/crates")
    }

    /// 获取包元数据。
    pub fn get_crate(&self, name: &str) -> std::io::Result<String> {
        self.get(&format!("/api/v1/crates/{}", name))
    }

    /// 获取特定版本信息。
    pub fn get_version(&self, name: &str, version: &str) -> std::io::Result<String> {
        self.get(&format!("/api/v1/crates/{}/{}", name, version))
    }

    /// 获取版本依赖。
    pub fn get_deps(&self, name: &str, version: &str) -> std::io::Result<String> {
        self.get(&format!("/api/v1/crates/{}/{}/deps", name, version))
    }

    /// 下载包 tarball。
    pub fn download(&self, name: &str, version: &str) -> std::io::Result<Vec<u8>> {
        let host = self
            .base_url
            .trim_start_matches("http://")
            .trim_start_matches("https://")
            .to_string();
        let mut stream = TcpStream::connect(host)?;
        let auth_header = self
            .token
            .as_ref()
            .map(|t| format!("Authorization: Bearer {}\r\n", t))
            .unwrap_or_default();
        let path = format!("/api/v1/crates/{}/{}/download", name, version);
        let request = format!(
            "GET {} HTTP/1.1\r\nHost: {}\r\n{auth_header}Connection: close\r\n\r\n",
            path, self.base_url
        );
        stream.write_all(request.as_bytes())?;
        let mut response = Vec::new();
        stream.read_to_end(&mut response)?;
        if let Some(idx) = response.windows(4).position(|w| w == b"\r\n\r\n") {
            Ok(response[idx + 4..].to_vec())
        } else {
            Ok(response)
        }
    }

    /// 发布新版本。
    pub fn publish(&self, name: &str, version: &str, artifact: &[u8]) -> std::io::Result<String> {
        self.put(&format!("/api/v1/crates/{}/{}", name, version), artifact)
    }

    fn get(&self, path: &str) -> std::io::Result<String> {
        self.request("GET", path, None)
    }

    fn put(&self, path: &str, body: &[u8]) -> std::io::Result<String> {
        self.request("PUT", path, Some(String::from_utf8_lossy(body).to_string()))
    }

    /// 搜索包（支持关键词查询）。
    pub fn search(&self, query: &str, limit: usize) -> std::io::Result<String> {
        let path = format!(
            "/api/v1/crates?q={}&limit={}",
            urlencoding::encode(query),
            limit
        );
        self.get(&path)
    }
}

mod urlencoding {
    pub fn encode(s: &str) -> String {
        let mut out = String::with_capacity(s.len() * 3);
        for c in s.chars() {
            match c {
                'A'..='Z' | 'a'..='z' | '0'..='9' | '-' | '_' | '.' | '~' => out.push(c),
                ' ' => out.push('+'),
                _ => {
                    out.push('%');
                    out.push_str(&format!("{:02X}", c as u32));
                }
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_registry_state() {
        let mut state = RegistryState::new();
        state.publish(
            VersionMeta {
                name: "foo".to_string(),
                version: "1.0.0".to_string(),
                checksum: "abc".to_string(),
                yanked: false,
            },
            vec![1, 2, 3],
            vec![("bar".to_string(), "^1.0.0".to_string())],
        );
        assert!(state.get_crate("foo").is_some());
        assert!(state.get_version("foo", "1.0.0").is_some());
        assert_eq!(state.download("foo", "1.0.0"), Some(&vec![1, 2, 3]));
    }
}
