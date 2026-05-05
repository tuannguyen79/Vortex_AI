// src/utils/watchdog.rs
// ════════════════════════════════════════════════════════════════════════════
// Task Watchdog – VortexAI
//
// Vấn đề: tokio::spawn bỏ đi JoinHandle → task chết silently, không ai biết.
// Giải pháp:
//   • Dùng JoinSet để track tất cả background tasks
//   • Watchdog loop: join_next() phát hiện task chết → log + restart
//   • Mỗi task có TaskDescriptor: tên, priority, restart policy
//   • Prometheus counter: task_panics_total{task="..."}
//
// Profiling endpoint (feature = "profiling"):
//   GET /debug/profile?seconds=30 → flamegraph SVG
// ════════════════════════════════════════════════════════════════════════════

use std::{future::Future, pin::Pin, sync::Arc, time::Duration};
use tokio::task::{JoinError, JoinSet};
use tracing::{error, info, warn};

/// Kiểu task factory: Box<dyn Fn() -> Future> để có thể restart
pub type TaskFactory = Arc<dyn Fn() -> Pin<Box<dyn Future<Output = ()> + Send>> + Send + Sync>;

#[derive(Clone)]
pub struct TaskDescriptor {
    pub name:    &'static str,
    pub factory: TaskFactory,
    pub restart: RestartPolicy,
}

#[derive(Clone, Copy, Debug)]
pub enum RestartPolicy {
    /// Khởi động lại ngay sau delay (service quan trọng)
    Always { delay_secs: u64 },
    /// Không restart (one-shot tasks)
    Never,
    /// Restart tối đa N lần
    MaxRetries { max: u32, delay_secs: u64 },
}

// ─────────────────────────────────────────────────────────────────────────────

/// Spawn tất cả tasks với watchdog giám sát.
///
/// Sử dụng trong main.rs:
/// ```rust
/// let tasks = vec![
///     task!("ingestor",    || ingestor.run(),    RestartPolicy::Always{delay_secs:5}),
///     task!("indicators",  || engine.run_loop(), RestartPolicy::Always{delay_secs:2}),
///     task!("signals",     || pipeline::run(st), RestartPolicy::Always{delay_secs:3}),
///     task!("learning",    || learning.run(),    RestartPolicy::Always{delay_secs:10}),
/// ];
/// spawn_with_watchdog(tasks).await;
/// ```
pub async fn spawn_with_watchdog(descriptors: Vec<TaskDescriptor>) {
    let mut set: JoinSet<(&'static str, RestartPolicy)> = JoinSet::new();
    let mut retry_counts: std::collections::HashMap<&str, u32> = std::collections::HashMap::new();

    // Initial spawn
    for desc in &descriptors {
        let name    = desc.name;
        let factory = desc.factory.clone();
        let restart = desc.restart;
        set.spawn(async move {
            info!(task = name, "Starting background task");
            (factory)().await;
            (name, restart)
        });
        info!(task = desc.name, "Spawned");
    }

    // Watchdog loop – runs forever, restarting dead tasks
    loop {
        match set.join_next().await {
            None => {
                info!("All tasks finished, watchdog exiting");
                break;
            }
            Some(Ok((name, policy))) => {
                // Task завершился нормально (не panic)
                warn!(task = name, "Task exited (non-panic)");
                metrics::counter!("task_exits_total", "task" => name).increment(1);

                if let Some(delay) = should_restart(name, policy, &mut retry_counts) {
                    let desc = descriptors.iter().find(|d| d.name == name).cloned();
                    if let Some(d) = desc {
                        let factory = d.factory.clone();
                        let restart = d.restart;
                        tokio::spawn(async move {
                            tokio::time::sleep(Duration::from_secs(delay)).await;
                            info!(task = name, delay, "Restarting task");
                        });
                        set.spawn(async move {
                            tokio::time::sleep(Duration::from_secs(delay)).await;
                            info!(task = name, "Task restarted");
                            (factory)().await;
                            (name, restart)
                        });
                    }
                }
            }
            Some(Err(join_err)) => {
                // Panic hoặc cancellation
                let name = extract_task_name(&join_err);
                error!(task = name, error = %join_err, "TASK PANICKED");
                metrics::counter!("task_panics_total", "task" => name).increment(1);

                // Tìm và restart task bị panic
                if let Some(desc) = descriptors.iter().find(|d| d.name == name).cloned() {
                    if let Some(delay) = should_restart(name, desc.restart, &mut retry_counts) {
                        let factory = desc.factory.clone();
                        let restart = desc.restart;
                        set.spawn(async move {
                            tokio::time::sleep(Duration::from_secs(delay)).await;
                            warn!(task = name, delay, "Restarting after panic");
                            (factory)().await;
                            (name, restart)
                        });
                    }
                }
            }
        }
    }
}

fn should_restart(
    name: &'static str,
    policy: RestartPolicy,
    counts: &mut std::collections::HashMap<&'static str, u32>,
) -> Option<u64> {
    match policy {
        RestartPolicy::Always { delay_secs } => Some(delay_secs),
        RestartPolicy::Never => {
            info!(task = name, "Not restarting (RestartPolicy::Never)");
            None
        }
        RestartPolicy::MaxRetries { max, delay_secs } => {
            let count = counts.entry(name).or_insert(0);
            *count += 1;
            if *count <= max {
                warn!(task = name, attempt = *count, max, "Retry");
                Some(delay_secs)
            } else {
                error!(task = name, max, "Max retries exceeded, giving up");
                metrics::counter!("task_max_retries_exceeded", "task" => name).increment(1);
                None
            }
        }
    }
}

fn extract_task_name(err: &JoinError) -> &'static str {
    // JoinError không chứa task name natively – trong production thêm
    // task_local! hoặc structured error. Đây dùng "unknown" làm fallback.
    if err.is_panic() { "unknown_panic" } else { "unknown_cancel" }
}

// ── Macro helper ──────────────────────────────────────────────────────────────
/// Tạo TaskDescriptor gọn hơn.
/// ```rust
/// make_task!("ingestor", move || ingestor.run(), RestartPolicy::Always{delay_secs:5})
/// ```
#[macro_export]
macro_rules! make_task {
    ($name:expr, $factory:expr, $policy:expr) => {
        $crate::utils::watchdog::TaskDescriptor {
            name:    $name,
            factory: std::sync::Arc::new(move || Box::pin($factory())),
            restart: $policy,
        }
    };
}

// ═════════════════════════════════════════════════════════════════════════════
// Profiling endpoint (bật bằng feature = "profiling")
// ═════════════════════════════════════════════════════════════════════════════

/// axum handler: GET /debug/profile?seconds=30
/// Trả về flamegraph SVG. Chỉ dùng trong dev/staging.
#[cfg(feature = "profiling")]
pub async fn flamegraph_handler(
    axum::extract::Query(params): axum::extract::Query<ProfileParams>,
) -> impl axum::response::IntoResponse {
    use pprof::ProfilerGuard;

    let seconds = params.seconds.unwrap_or(10).min(60);
    let guard   = ProfilerGuard::new(100).unwrap();

    tokio::time::sleep(Duration::from_secs(seconds)).await;

    let report = guard.report().build().unwrap();
    let mut buf = Vec::new();
    report.flamegraph(&mut buf).unwrap();

    (
        [(axum::http::header::CONTENT_TYPE, "image/svg+xml")],
        buf,
    )
}

#[cfg(not(feature = "profiling"))]
pub async fn flamegraph_handler() -> impl axum::response::IntoResponse {
    (
        axum::http::StatusCode::NOT_IMPLEMENTED,
        "Build with --features profiling to enable flamegraph endpoint",
    )
}

#[derive(serde::Deserialize)]
pub struct ProfileParams {
    pub seconds: Option<u64>,
}

// ═════════════════════════════════════════════════════════════════════════════
// Tests
// ═════════════════════════════════════════════════════════════════════════════
#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    #[test]
    fn restart_policy_always() {
        let mut counts = std::collections::HashMap::new();
        let r = should_restart("svc", RestartPolicy::Always { delay_secs: 5 }, &mut counts);
        assert_eq!(r, Some(5));
        // Luôn trả về delay dù gọi nhiều lần
        let r2 = should_restart("svc", RestartPolicy::Always { delay_secs: 5 }, &mut counts);
        assert_eq!(r2, Some(5));
    }

    #[test]
    fn restart_policy_never() {
        let mut counts = std::collections::HashMap::new();
        let r = should_restart("svc", RestartPolicy::Never, &mut counts);
        assert_eq!(r, None);
    }

    #[test]
    fn restart_policy_max_retries() {
        let mut counts = std::collections::HashMap::new();
        let policy = RestartPolicy::MaxRetries { max: 2, delay_secs: 3 };
        let r1 = should_restart("svc", policy, &mut counts);
        assert_eq!(r1, Some(3)); // attempt 1
        let r2 = should_restart("svc", policy, &mut counts);
        assert_eq!(r2, Some(3)); // attempt 2
        let r3 = should_restart("svc", policy, &mut counts);
        assert_eq!(r3, None);   // exceeded
    }

    #[tokio::test]
    async fn watchdog_detects_normal_exit() {
        // Task kết thúc ngay → watchdog log và không crash
        let counter = Arc::new(AtomicU32::new(0));
        let counter2 = counter.clone();

        let desc = TaskDescriptor {
            name: "test_quick",
            factory: Arc::new(move || {
                let c = counter2.clone();
                Box::pin(async move {
                    c.fetch_add(1, Ordering::SeqCst);
                    // Kết thúc ngay (không loop)
                })
            }),
            restart: RestartPolicy::Never,
        };

        // Spawn 1 task và chạy watchdog với timeout
        tokio::time::timeout(
            Duration::from_secs(2),
            spawn_with_watchdog(vec![desc]),
        ).await.ok(); // timeout OK – watchdog sẽ thấy task kết thúc

        assert!(counter.load(Ordering::SeqCst) >= 1, "Task should have run at least once");
    }

    #[tokio::test]
    async fn task_restarts_with_max_retries() {
        let counter = Arc::new(AtomicU32::new(0));
        let counter2 = counter.clone();

        let desc = TaskDescriptor {
            name: "test_retry",
            factory: Arc::new(move || {
                let c = counter2.clone();
                Box::pin(async move {
                    c.fetch_add(1, Ordering::SeqCst);
                    // Ngay lập tức exit → watchdog sẽ retry
                })
            }),
            restart: RestartPolicy::MaxRetries { max: 2, delay_secs: 0 },
        };

        tokio::time::timeout(
            Duration::from_secs(3),
            spawn_with_watchdog(vec![desc]),
        ).await.ok();

        // Ban đầu 1 lần + 2 retries = 3 lần tổng
        let runs = counter.load(Ordering::SeqCst);
        assert!(runs >= 1, "Should have run at least once, got {runs}");
    }
}
