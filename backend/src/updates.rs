use crate::{domain::Method, now_ms};
use gateway_plugin_sdk::{ErrorCode, PluginFault, call::host::HttpRequest, client::HostClient};
use serde::{Deserialize, Serialize};
use std::time::{Duration, Instant};
use tokio::sync::Mutex;

const CACHE_TTL: Duration = Duration::from_secs(300);

#[derive(Deserialize)]
struct SourceMetadata {
    repository: String,
    commit: String,
}

#[derive(Clone, Serialize)]
pub struct Source {
    pub method: Method,
    pub label: &'static str,
    pub url: String,
    pub bundled_commit: String,
}

pub fn sources() -> Result<Vec<Source>, PluginFault> {
    [
        (
            Method::Meow,
            "Meow",
            include_str!("../../vendor/meow-source.json"),
        ),
        (
            Method::ModelTrace,
            "ModelTrace",
            include_str!("../../vendor/modeltrace/SOURCE.json"),
        ),
    ]
    .into_iter()
    .map(|(method, label, text)| {
        let metadata: SourceMetadata = serde_json::from_str(text)
            .map_err(|_| PluginFault::new(ErrorCode::Fault, "内置来源元数据格式错误"))?;
        let slug = metadata
            .repository
            .strip_prefix("https://github.com/")
            .unwrap_or("");
        if !valid_sha(&metadata.commit)
            || slug.split('/').count() != 2
            || slug.split('/').any(|part| {
                part.is_empty()
                    || part == "."
                    || part == ".."
                    || !part
                        .bytes()
                        .all(|c| c.is_ascii_alphanumeric() || b"-_.".contains(&c))
            })
        {
            return Err(PluginFault::new(ErrorCode::Fault, "内置来源元数据无效"));
        }
        Ok(Source {
            method,
            label,
            url: metadata.repository,
            bundled_commit: metadata.commit,
        })
    })
    .collect()
}

#[derive(Clone, Serialize)]
pub struct CheckResult {
    pub method: Method,
    pub status: String,
    pub checked_at: u64,
    pub latest_commit: String,
    pub ahead_by: u64,
    pub behind_by: u64,
    pub compare_url: String,
}

#[derive(Default)]
pub struct UpdateChecker {
    // 与检测任务的锁分离，检查 GitHub 不阻塞模型执行或状态保存。
    slots: [Mutex<Option<(Instant, CheckResult)>>; 2],
}

impl UpdateChecker {
    pub async fn check(
        &self,
        host: &HostClient,
        method: Method,
    ) -> Result<CheckResult, PluginFault> {
        let index = match method {
            Method::Meow => 0,
            Method::ModelTrace => 1,
        };
        let mut cache = self.slots[index].lock().await;
        if let Some((_, result)) = cache.as_ref().filter(|(t, _)| t.elapsed() < CACHE_TTL) {
            return Ok(result.clone());
        }
        let source = sources()?
            .into_iter()
            .find(|s| s.method == method)
            .ok_or_else(|| PluginFault::new(ErrorCode::Fault, "内置来源缺失"))?;
        let result = tokio::time::timeout(Duration::from_secs(15), check_source(host, &source))
            .await
            .map_err(|_| {
                PluginFault::new(ErrorCode::Timeout, "更新检查超过 15 秒，请稍后重试")
            })??;
        *cache = Some((Instant::now(), result.clone()));
        Ok(result)
    }
}

fn valid_sha(sha: &str) -> bool {
    sha.len() == 40 && sha.bytes().all(|c| c.is_ascii_hexdigit())
}

fn invalid_response() -> PluginFault {
    PluginFault::new(
        ErrorCode::Upstream,
        "GitHub 返回的提交信息格式异常，更新状态未确认",
    )
}

fn network_error(error: PluginFault) -> PluginFault {
    PluginFault::new(
        error.code,
        format!(
            "更新检查的宿主 HTTP 调用失败（{:?}），请稍后重试",
            error.code
        ),
    )
}

async fn github<T: serde::de::DeserializeOwned>(
    host: &HostClient,
    path: &str,
) -> Result<T, PluginFault> {
    // 路径只来自内置仓库和经校验的 SHA，不接受页面传入 URL，也不附加任何账号凭据。
    let mut response = host
        .http(
            HttpRequest {
                method: "GET".into(),
                url: format!("https://api.github.com/repos/{path}"),
                headers: vec![
                    (
                        "User-Agent".into(),
                        concat!("codex-model-health/", env!("CARGO_PKG_VERSION"))
                            .as_bytes()
                            .to_vec(),
                    ),
                    ("Accept".into(), b"application/vnd.github+json".to_vec()),
                    ("X-GitHub-Api-Version".into(), b"2022-11-28".to_vec()),
                ],
            },
            vec![],
        )
        .await
        .map_err(network_error)?;
    if response.status != 200 {
        let status = response.status;
        response.body.close().await.map_err(network_error)?;
        let message = match status {
            403 | 429 => format!("GitHub 暂时拒绝检查（HTTP {status}，可能触发限流），请稍后重试"),
            404 => "GitHub 仓库或提交未找到，更新状态未确认".into(),
            300..=399 => "GitHub 仓库地址已重定向，请核对来源地址后更新插件".into(),
            _ => format!("GitHub 检查失败（HTTP {status}），更新状态未确认"),
        };
        return Err(PluginFault::new(ErrorCode::Upstream, message));
    }
    let bytes = response
        .body
        .collect(256 * 1024)
        .await
        .map_err(network_error)?;
    serde_json::from_slice(&bytes).map_err(|_| invalid_response())
}

#[derive(Deserialize)]
struct Commit {
    sha: String,
}

#[derive(Deserialize)]
struct Comparison {
    status: String,
    ahead_by: u64,
    behind_by: u64,
}

async fn check_source(host: &HostClient, source: &Source) -> Result<CheckResult, PluginFault> {
    let slug = source
        .url
        .strip_prefix("https://github.com/")
        .ok_or_else(invalid_response)?;
    // 省略 sha 时 GitHub 查询默认分支，仓库改用其他分支也不依赖硬编码 main/master。
    let commits: Vec<Commit> = github(host, &format!("{slug}/commits?per_page=1")).await?;
    let latest = commits
        .first()
        .filter(|c| valid_sha(&c.sha))
        .ok_or_else(invalid_response)?;
    let comparison = if latest.sha == source.bundled_commit {
        Comparison {
            status: "identical".into(),
            ahead_by: 0,
            behind_by: 0,
        }
    } else {
        // 文件补丁只出现在第一页，第二页保留关系元数据，避免下载庞大的 diff。
        github(
            host,
            &format!(
                "{slug}/compare/{}...{}?per_page=1&page=2",
                source.bundled_commit, latest.sha
            ),
        )
        .await?
    };
    let consistent = match comparison.status.as_str() {
        "identical" => {
            latest.sha == source.bundled_commit
                && comparison.ahead_by == 0
                && comparison.behind_by == 0
        }
        "ahead" => comparison.ahead_by > 0 && comparison.behind_by == 0,
        "behind" => comparison.ahead_by == 0 && comparison.behind_by > 0,
        "diverged" => comparison.ahead_by > 0 && comparison.behind_by > 0,
        _ => false,
    };
    if !consistent {
        return Err(invalid_response());
    }
    Ok(CheckResult {
        method: source.method,
        status: comparison.status,
        checked_at: now_ms(),
        latest_commit: latest.sha.clone(),
        ahead_by: comparison.ahead_by,
        behind_by: comparison.behind_by,
        compare_url: format!(
            "{}/compare/{}...{}",
            source.url, source.bundled_commit, latest.sha
        ),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn sources_match_bundled_provenance() {
        let entries = sources().unwrap();
        assert_eq!(entries.len(), 2);
        assert!(include_str!("../../NOTICE").contains(&entries[0].bundled_commit));
        assert!(include_str!("../../NOTICE").contains(&entries[1].bundled_commit));
        assert_eq!(
            entries[0].url,
            "https://github.com/chen-006/meow-llm-detector"
        );
        assert_eq!(entries[1].url, "https://github.com/xqy2006/ModelTrace");
        assert!(!valid_sha("../../not-a-commit"));
    }
}
