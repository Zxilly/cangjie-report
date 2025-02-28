use git2::build::RepoBuilder;
use glob::glob;
use redis::{Client, Commands};
use serde::{Deserialize, Serialize};
use std::env;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::Command;
use std::time::SystemTime;
use std::{collections::HashMap, io::Cursor};
use tar::Archive;
use tokio::fs;
use toml::Value;
use url::Url;
use vercel_runtime::{run, Body, Error, Request, Response, StatusCode};
use zstd::stream::decode_all;
use rand::Rng;
use rand::distr::Alphanumeric;

static CJLINT_TAR_ZST: &'static [u8] = include!(env!("CJLINT_DATA_FILE"));

#[derive(Debug, Serialize, Deserialize, PartialEq)]
pub enum DefectLevel {
    #[serde(rename = "MANDATORY")]
    Mandatory,
    #[serde(rename = "SUGGESTIONS")]
    Suggestions,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct AnalysisResultItem {
    pub file: String,
    pub line: i32,
    pub column: i32,
    #[serde(rename = "endLine")]
    pub end_line: i32,
    #[serde(rename = "endColumn")]
    pub end_column: i32,
    #[serde(rename = "analyzerName")]
    pub analyzer_name: String,
    pub description: String,
    #[serde(rename = "defectLevel")]
    pub defect_level: DefectLevel,
    #[serde(rename = "defectType")]
    pub defect_type: String,
    pub language: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct AnalysisResult {
    pub cjlint: Vec<AnalysisResultItem>,
    pub created_at: i64,
    pub commit: String,
    pub package_name: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ApiResponse<T> {
    pub success: bool,
    pub message: Option<String>,
    pub data: Option<T>,
    pub error: Option<String>,
}

fn create_response<T: Serialize>(
    status_code: StatusCode,
    success: bool,
    message: Option<&str>,
    data: Option<T>,
    error: Option<&str>,
) -> Result<Response<Body>, Error> {
    let response = ApiResponse {
        success,
        message: message.map(String::from),
        data,
        error: error.map(String::from),
    };

    let body = serde_json::to_string(&response)
        .map_err(|e| Error::from(format!("Failed to serialize response: {}", e)))?;

    Ok(Response::builder()
        .status(status_code)
        .header("Content-Type", "application/json")
        .body(Body::from(body))?)
}

/// 生成一个指定长度的随机字符串
fn generate_random_string(length: usize) -> String {
    rand::rng()
        .sample_iter(Alphanumeric)
        .take(length)
        .map(char::from)
        .collect()
}

// 定义一个结构体来存储克隆结果
#[derive(Debug, Clone)]
struct CloneResult {
    repo_path: String,
    commit_hash: String,
}

// 定义一个结构体用于自动清理仓库目录
struct RepoCleanup {
    repo_path: String,
    cleaned: bool,
}

impl RepoCleanup {
    fn new(repo_path: String) -> Self {
        Self {
            repo_path,
            cleaned: false,
        }
    }

    // 手动清理方法，如果需要提前清理
    async fn cleanup(&mut self) -> Result<(), Error> {
        if !self.cleaned {
            if let Err(e) = fs::remove_dir_all(&self.repo_path).await {
                eprintln!("Failed to remove repository directory: {}", e);
                return Err(Error::from(format!("Failed to remove repository directory: {}", e)));
            }
            self.cleaned = true;
        }
        Ok(())
    }
}

impl Drop for RepoCleanup {
    fn drop(&mut self) {
        if !self.cleaned {
            if let Err(e) = std::fs::remove_dir_all(&self.repo_path) {
                eprintln!("Failed to remove repository directory in drop: {}", e);
            } else {
                self.cleaned = true;
            }
        }
    }
}

async fn ensure_cjlint_extracted() -> Result<(), std::io::Error> {
    let target_dir = Path::new("/tmp/cj");
    // /tmp/cj/tools/bin/cjlint
    let cjlint_path = target_dir.join("tools/bin/cjlint");

    if !target_dir.exists() || !cjlint_path.exists() {
        let cjlint_tar = decode_all(CJLINT_TAR_ZST.as_ref() as &[u8])?;

        fs::create_dir_all(target_dir).await?;

        let cursor = Cursor::new(cjlint_tar);
        let mut archive = Archive::new(cursor);
        archive.unpack(target_dir)?;

        eprintln!("cjlint_path: {:?}", cjlint_path);

        let mut perms = fs::metadata(&cjlint_path).await?.permissions();
        perms.set_mode(0o755);
    }

    Ok(())
}

async fn clone_repository(repo_url: &str) -> Result<CloneResult, Error> {
    let random_suffix = generate_random_string(10);
    let repo_dir_name = format!("cjrepo_{}", random_suffix);
    let target_dir = Path::new("/tmp").join(&repo_dir_name);
    let target_dir_str = target_dir.to_string_lossy().to_string();

    if target_dir.exists() {
        fs::remove_dir_all(&target_dir).await?;
    }

    fs::create_dir_all(&target_dir).await?;

    let mut option = git2::FetchOptions::default();
    option.depth(1);
    let repo = RepoBuilder::new()
        .fetch_options(option)
        .clone(repo_url, &target_dir)?;

    let head = repo.head().unwrap();
    let commit = head.peel_to_commit().unwrap();
    let hash = commit.id().to_string();

    Ok(CloneResult {
        repo_path: target_dir_str,
        commit_hash: hash,
    })
}

async fn find_package_name(repo_path: String) -> Result<String, Error> {
    let pattern = format!("{}/**/cjpm.toml", repo_path);
    let paths: Vec<_> = glob(&pattern)
        .map_err(|e| Error::from(format!("Failed to read glob pattern: {}", e)))?
        .filter_map(Result::ok)
        .collect();

    if paths.is_empty() {
        return Err(Error::from("No cjpm.toml found"));
    }

    let content = fs::read_to_string(&paths[0])
        .await
        .map_err(|e| Error::from(format!("Failed to read cjpm.toml: {}", e)))?;

    let value: Value = toml::from_str(&content)
        .map_err(|e| Error::from(format!("Failed to parse TOML: {}", e)))?;

    let package_name = value
        .get("package")
        .and_then(|p| p.get("name"))
        .and_then(|n| n.as_str())
        .ok_or_else(|| Error::from("package.name not found in cjpm.toml"))?;

    Ok(package_name.to_string())
}

async fn run_cjlint(repo_path: String) -> Result<String, Error> {
    let output_path = format!("/tmp/{}.json", generate_random_string(10));

    let status = Command::new("/tmp/cj/tools/bin/cjlint")
        .args(&["-f", &repo_path, "-r", "json", "-o", &output_path])
        .env("LD_LIBRARY_PATH", "/tmp/cj")
        .env("CANGJIE_HOME", "/tmp/cj")
        .status()
        .map_err(|e| Error::from(format!("Failed to execute cjlint: {}", e)))?;

    if !status.success() {
        return Err(Error::from(format!(
            "cjlint command failed with exit code: {}",
            status.code().unwrap_or(-1)
        )));
    }

    let json_content = fs::read_to_string(&output_path)
        .await
        .map_err(|e| Error::from(format!("Failed to read cjlint output: {}", e)))?;

    fs::remove_file(&output_path)
        .await
        .map_err(|e| Error::from(format!("Failed to delete cjlint output file: {}", e)))?;

    Ok(json_content)
}

async fn save_to_redis(repo: &str, content: &str) -> Result<(), Error> {
    let redis_url = env::var("KV_URL").map_err(|_| Error::from("KV_URL not set"))?;

    let client = Client::open(redis_url)
        .map_err(|e| Error::from(format!("Failed to create Redis client: {}", e)))?;

    let mut con = client.get_connection()?;

    let key = format!("cjlint_{}", repo);
    let _: () = con.set(key, content.to_string())?;

    Ok(())
}

#[tokio::main]
async fn main() -> Result<(), Error> {
    eprintln!("Starting...");
    if let Err(e) = ensure_cjlint_extracted().await {
        eprintln!("Failed to extract cjlint: {}", e);
        return Err(Error::from(e));
    }
    eprintln!("cjlint extracted");

    run(handler).await
}

pub async fn handler(req: Request) -> Result<Response<Body>, Error> {
    let url = Url::parse(&req.uri().to_string()).unwrap();
    let hash_query: HashMap<String, String> = url.query_pairs().into_owned().collect();
    let repo = hash_query.get("repo");
    let repo = match repo {
        Some(repo) => repo,
        None => {
            return create_response::<()>(
                StatusCode::BAD_REQUEST,
                false,
                None,
                None,
                Some("repo query parameter is required"),
            );
        }
    };

    let clone_result = match clone_repository(repo).await {
        Ok(result) => result,
        Err(e) => {
            return create_response::<()>(
                StatusCode::INTERNAL_SERVER_ERROR,
                false,
                None,
                None,
                Some(&format!("Failed to clone repository: {}", e)),
            );
        }
    };

    let mut repo_cleanup = RepoCleanup::new(clone_result.repo_path.clone());

    let package_name = match find_package_name(clone_result.repo_path.clone()).await {
        Ok(name) => name,
        Err(e) => {
            return create_response::<()>(
                StatusCode::INTERNAL_SERVER_ERROR,
                false,
                None,
                None,
                Some(&format!("Failed to find package name: {}", e)),
            );
        }
    };

    // 使用 cjlint 检查代码
    let content = match run_cjlint(clone_result.repo_path.clone()).await {
        Ok(result) => result,
        Err(e) => {
            return create_response::<()>(
                StatusCode::INTERNAL_SERVER_ERROR,
                false,
                None,
                None,
                Some(&format!("Failed to run cjlint: {}", e)),
            );
        }
    };

    let analysis_result: Vec<AnalysisResultItem> = match serde_json::from_str(&content) {
        Ok(result) => result,
        Err(e) => {
            return create_response::<()>(
                StatusCode::INTERNAL_SERVER_ERROR,
                false,
                None,
                None,
                Some(&format!("Failed to parse cjlint output: {}", e)),
            );
        }
    };

    // 处理file字段，去除repo_path前缀
    let repo_path = clone_result.repo_path.clone();
    let repo_path_with_slash = if repo_path.ends_with('/') {
        repo_path.clone()
    } else {
        format!("{}/", repo_path)
    };
    
    let processed_analysis_result: Vec<AnalysisResultItem> = analysis_result
        .into_iter()
        .map(|mut item| {
            // 去除file字段中的repo_path前缀
            if item.file.starts_with(&repo_path_with_slash) {
                item.file = item.file[repo_path_with_slash.len()..].to_string();
            } else if item.file.starts_with(&repo_path) {
                item.file = item.file[repo_path.len()..].to_string();
                // 如果去除前缀后以/开头，则去除这个/
                if item.file.starts_with('/') {
                    item.file = item.file[1..].to_string();
                }
            }
            item
        })
        .collect();

    let analysis_result = AnalysisResult {
        cjlint: processed_analysis_result,
        created_at: SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64,
        commit: clone_result.commit_hash,
        package_name,
    };

    // 将结果保存到Redis
    if let Err(e) = save_to_redis(repo, &serde_json::to_string(&analysis_result).unwrap()).await {
        return create_response::<()>(
            StatusCode::INTERNAL_SERVER_ERROR,
            false,
            None,
            None,
            Some(&format!("Failed to save to Redis: {}", e)),
        );
    }

    if let Err(e) = repo_cleanup.cleanup().await {
        eprintln!("Warning: Failed to clean up repository: {}", e);
    }

    return create_response(
        StatusCode::OK,
        true,
        Some("Analysis completed successfully"),
        Some(analysis_result),
        None,
    );
}
