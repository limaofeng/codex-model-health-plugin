use crate::invalid;
use gateway_plugin_sdk::{ErrorCode, PluginFault};
use serde_json::Value;
use std::{
    ffi::OsString,
    path::{Path, PathBuf},
    process::Stdio,
    sync::Arc,
    time::Duration,
};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    process::{Child, ChildStdin, ChildStdout, Command},
    sync::Mutex,
};

const LIMIT: usize = 2 * 1024 * 1024;
/// 引擎空闲超过该时间后退出，释放 Python 与 NumPy 占用的常驻内存；下次查询按需重启。
const IDLE: Duration = Duration::from_secs(120);
/// 关闭输入后等待引擎自行退出的时间；PyInstaller 引导进程借此清理解压目录。
const GRACE: Duration = Duration::from_secs(3);
const BUNDLED_PREFIX: &str = ".model-health-";

fn fault(message: impl Into<String>) -> PluginFault {
    PluginFault::new(ErrorCode::Fault, message)
}
struct Worker {
    child: Child,
    input: Option<ChildStdin>,
    output: BufReader<ChildStdout>,
}
#[derive(Default)]
struct Slot {
    worker: Option<Worker>,
    /// 每次查询递增；空闲回收只处理自己登记时的那一次，之后有新查询就放弃。
    epoch: u64,
}
pub struct Engine {
    command: Vec<OsString>,
    slot: Arc<Mutex<Slot>>,
    directory: Option<PathBuf>,
    idle: Duration,
}

impl Engine {
    pub fn command(command: Vec<OsString>) -> Self {
        Self {
            command,
            slot: Arc::default(),
            directory: None,
            idle: IDLE,
        }
    }

    /// 缩短空闲时间，仅用于测试。
    #[must_use]
    pub fn with_idle(mut self, idle: Duration) -> Self {
        self.idle = idle;
        self
    }

    /// 引擎进程当前是否在运行。
    pub async fn running(&self) -> bool {
        self.slot.lock().await.worker.is_some()
    }

    #[cfg(feature = "bundled-engine")]
    pub fn bundled() -> std::io::Result<Self> {
        use std::{
            io::Write,
            os::unix::fs::{DirBuilderExt, OpenOptionsExt},
        };
        // 宿主 cwd 是本次插件实例的私有包目录，退出后由宿主回收。
        // /tmp 在正式容器中为 noexec；引擎及其动态库都应位于插件执行目录。
        let base = std::env::current_dir()?;
        sweep_stale(&base);
        let directory = base.join(format!(
            "{BUNDLED_PREFIX}{}-{}",
            std::process::id(),
            crate::now_nanos()
        ));
        std::fs::DirBuilder::new().mode(0o700).create(&directory)?;
        let path = directory.join("meow-engine");
        let result = (|| {
            let mut file = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o700)
                .open(&path)?;
            file.write_all(include_bytes!(env!("MEOW_ENGINE_BINARY")))?;
            file.sync_all()
        })();
        if let Err(error) = result {
            let _ = std::fs::remove_dir_all(&directory);
            return Err(error);
        }
        let mut engine = Self::command(vec![path.into_os_string()]);
        engine.directory = Some(directory);
        Ok(engine)
    }

    pub async fn query(&self, request: Value) -> Result<Value, PluginFault> {
        let mut slot = self.slot.lock().await;
        if slot.worker.is_none() {
            // 上一个引擎若被强制结束，会在这里留下 PyInstaller 的解压目录。
            if let Some(directory) = &self.directory {
                sweep_extracted(directory);
            }
            slot.worker = Some(self.spawn()?);
        }
        slot.epoch += 1;
        let epoch = slot.epoch;
        let result = tokio::time::timeout(
            Duration::from_secs(20),
            slot.worker.as_mut().unwrap().exchange(request),
        )
        .await;
        match result {
            Ok(Ok(value)) => {
                drop(slot);
                self.schedule_idle(epoch);
                Ok(value)
            }
            Ok(Err(error)) => {
                if let Some(worker) = slot.worker.take() {
                    worker.shutdown().await;
                }
                Err(error)
            }
            Err(_) => {
                // 引擎已无响应，直接结束；残留的解压目录由下次启动前清理。
                slot.worker = None;
                Err(PluginFault::new(ErrorCode::Timeout, "检测引擎响应超时"))
            }
        }
    }

    fn spawn(&self) -> Result<Worker, PluginFault> {
        let program = self
            .command
            .first()
            .ok_or_else(|| fault("检测引擎命令为空"))?;
        let mut command = Command::new(program);
        command.args(&self.command[1..]);
        if let Some(directory) = &self.directory {
            // PyInstaller onefile 也会解压动态库，不能再使用系统 /tmp。
            command.env("TMPDIR", directory);
        }
        let mut child = command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| fault(format!("检测引擎启动失败：{e}")))?;
        let input = child
            .stdin
            .take()
            .ok_or_else(|| fault("检测引擎没有输入管道"))?;
        let output = BufReader::new(
            child
                .stdout
                .take()
                .ok_or_else(|| fault("检测引擎没有输出管道"))?,
        );
        Ok(Worker {
            child,
            input: Some(input),
            output,
        })
    }

    fn schedule_idle(&self, epoch: u64) {
        let slot = Arc::clone(&self.slot);
        let idle = self.idle;
        tokio::spawn(async move {
            tokio::time::sleep(idle).await;
            let mut slot = slot.lock().await;
            if slot.epoch == epoch
                && let Some(worker) = slot.worker.take()
            {
                worker.shutdown().await;
            }
        });
    }
}

/// 删除目录下遗留的 PyInstaller 解压目录；调用时没有引擎在运行。
fn sweep_extracted(directory: &Path) {
    let Ok(entries) = std::fs::read_dir(directory) else {
        return;
    };
    for entry in entries.flatten() {
        if entry.file_name().to_string_lossy().starts_with("_MEI") {
            let _ = std::fs::remove_dir_all(entry.path());
        }
    }
}

/// 清理同一目录下已退出实例遗留的引擎目录；仍有存活进程的目录保持不动。
#[cfg_attr(not(feature = "bundled-engine"), allow(dead_code))]
fn sweep_stale(base: &Path) {
    let Ok(entries) = std::fs::read_dir(base) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(rest) = name.to_string_lossy().strip_prefix(BUNDLED_PREFIX).map(str::to_owned)
        else {
            continue;
        };
        let alive = rest
            .split('-')
            .next()
            .and_then(|pid| pid.parse::<u32>().ok())
            .is_some_and(|pid| {
                pid == std::process::id() || Path::new(&format!("/proc/{pid}")).exists()
            });
        if !alive && entry.path().is_dir() {
            let _ = std::fs::remove_dir_all(entry.path());
        }
    }
}

impl Worker {
    async fn exchange(&mut self, request: Value) -> Result<Value, PluginFault> {
        let mut bytes = serde_json::to_vec(&request).map_err(|_| invalid("检测输入编码失败"))?;
        if bytes.len() >= LIMIT {
            return Err(invalid("检测输入超过大小限制"));
        }
        bytes.push(b'\n');
        let input = self
            .input
            .as_mut()
            .ok_or_else(|| fault("检测引擎输入已关闭"))?;
        input
            .write_all(&bytes)
            .await
            .map_err(|e| fault(format!("检测引擎写入失败：{e}")))?;
        input
            .flush()
            .await
            .map_err(|e| fault(format!("检测引擎写入失败：{e}")))?;
        let mut output = Vec::new();
        (&mut self.output)
            .take((LIMIT + 1) as u64)
            .read_until(b'\n', &mut output)
            .await
            .map_err(|e| fault(format!("检测引擎读取失败：{e}")))?;
        if output.len() > LIMIT || output.last() != Some(&b'\n') {
            return Err(fault("检测引擎返回超限或中断的响应"));
        }
        let reply: Value =
            serde_json::from_slice(&output).map_err(|_| fault("检测引擎返回了无效 JSON"))?;
        if reply["ok"] != true {
            return Err(invalid(format!(
                "检测引擎：{}",
                reply["error"].as_str().unwrap_or("invalid_response")
            )));
        }
        Ok(reply["data"].clone())
    }

    /// 先关闭输入让引擎正常退出（PyInstaller 借此清理解压目录），超时再强制结束。
    async fn shutdown(mut self) {
        self.input.take();
        if tokio::time::timeout(GRACE, self.child.wait()).await.is_err() {
            let _ = self.child.start_kill();
            let _ = self.child.wait().await;
        }
    }
}
impl Drop for Worker {
    fn drop(&mut self) {
        let _ = self.child.start_kill();
    }
}
impl Drop for Engine {
    fn drop(&mut self) {
        if let Some(path) = &self.directory {
            let _ = std::fs::remove_dir_all(path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn echo() -> Engine {
        // 每收到一行就回一个固定响应，直到输入关闭。
        Engine::command(vec![
            "sh".into(),
            "-c".into(),
            r#"while read -r line; do echo '{"ok":true,"data":1}'; done"#.into(),
        ])
    }

    #[tokio::test]
    async fn idle_engine_exits_and_restarts_on_demand() {
        let engine = echo().with_idle(Duration::from_millis(150));
        assert!(!engine.running().await);
        assert_eq!(engine.query(serde_json::json!({})).await.unwrap(), 1);
        assert!(engine.running().await);
        tokio::time::sleep(Duration::from_millis(600)).await;
        assert!(!engine.running().await, "空闲后引擎应已退出");
        assert_eq!(engine.query(serde_json::json!({})).await.unwrap(), 1);
        assert!(engine.running().await);
    }

    #[tokio::test]
    async fn a_new_query_cancels_the_pending_idle_exit() {
        let engine = echo().with_idle(Duration::from_millis(400));
        engine.query(serde_json::json!({})).await.unwrap();
        tokio::time::sleep(Duration::from_millis(250)).await;
        engine.query(serde_json::json!({})).await.unwrap();
        tokio::time::sleep(Duration::from_millis(250)).await;
        assert!(engine.running().await, "第二次查询后的空闲时间尚未结束");
    }

    #[test]
    fn sweep_removes_only_dead_instances_and_extracted_directories() {
        let base = std::env::temp_dir().join(format!("mh-sweep-{}", crate::now_nanos()));
        std::fs::create_dir_all(&base).unwrap();
        let dead = base.join(format!("{BUNDLED_PREFIX}4194999-1"));
        let mine = base.join(format!("{BUNDLED_PREFIX}{}-2", std::process::id()));
        let other = base.join("keep-me");
        for dir in [&dead, &mine, &other] {
            std::fs::create_dir_all(dir).unwrap();
        }
        sweep_stale(&base);
        assert!(!dead.exists(), "已退出实例的目录应被清理");
        assert!(mine.exists(), "当前进程的目录不能删");
        assert!(other.exists(), "无关目录不能删");

        let extracted = base.join("_MEIabc123");
        std::fs::create_dir_all(&extracted).unwrap();
        sweep_extracted(&base);
        assert!(!extracted.exists());
        assert!(other.exists());
        let _ = std::fs::remove_dir_all(&base);
    }
}
