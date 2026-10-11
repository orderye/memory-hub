//! 作业调度器：按 cron 表达式到点触发，支持测试时钟注入
//!
//! 调度器只负责"什么时候跑"，不关心"跑什么"——任务通过
//! `ScheduledTask::run(now)` 回调注入，因此可以在测试中精确控制到点时刻。

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use common::cron::CronSchedule;
use common::Result;
use std::sync::Arc;

/// 可调度任务
#[async_trait]
pub trait ScheduledTask: Send + Sync {
    /// 任务名（日志用）
    fn name(&self) -> &str;

    /// 执行一次（幂等）
    async fn run(&self, now: DateTime<Utc>) -> Result<()>;
}

/// 调度器：一条 cron 表达式 + 一个任务
pub struct JobScheduler {
    schedule: CronSchedule,
    task: Arc<dyn ScheduledTask>,
}

impl JobScheduler {
    pub fn new(cron_expr: &str, task: Arc<dyn ScheduledTask>) -> Result<Self> {
        let schedule = CronSchedule::parse(cron_expr)
            .map_err(|e| common::HubError::InvalidArgument(format!("cron 表达式非法: {}", e)))?;
        Ok(Self { schedule, task })
    }

    /// 从 `last_run` 之后找到下一次到点时间（None 表示一年内不会触发）
    pub fn next_fire(&self, last_run: DateTime<Utc>) -> Option<DateTime<Utc>> {
        self.schedule.next_after(last_run)
    }

    /// 到点判定：当前时刻是否应触发
    pub fn is_due(&self, now: DateTime<Utc>, last_run: DateTime<Utc>) -> bool {
        self.schedule
            .next_after(last_run)
            .is_some_and(|next| next <= now)
    }

    /// 执行任务（到点由调用方判定）
    pub async fn run_if_due(&self, now: DateTime<Utc>, last_run: DateTime<Utc>) -> Result<bool> {
        if !self.is_due(now, last_run) {
            return Ok(false);
        }
        self.task.run(now).await?;
        Ok(true)
    }
}

/// 把任意 async 闭包包装成任务（便捷构造）
pub struct FnTask<F> {
    name: String,
    f: F,
}

#[async_trait]
impl<F, Fut> ScheduledTask for FnTask<F>
where
    F: Fn(DateTime<Utc>) -> Fut + Send + Sync,
    Fut: std::future::Future<Output = Result<()>> + Send,
{
    fn name(&self) -> &str {
        &self.name
    }

    async fn run(&self, now: DateTime<Utc>) -> Result<()> {
        (self.f)(now).await
    }
}

/// 用闭包构造任务
pub fn fn_task<F, Fut>(name: impl Into<String>, f: F) -> Arc<FnTask<F>>
where
    F: Fn(DateTime<Utc>) -> Fut + Send + Sync,
    Fut: std::future::Future<Output = Result<()>> + Send,
{
    Arc::new(FnTask {
        name: name.into(),
        f,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn test_next_fire_computation() {
        let counter = Arc::new(AtomicUsize::new(0));
        let c = counter.clone();
        let task = fn_task("count", move |_now| {
            let c = c.clone();
            async move {
                c.fetch_add(1, Ordering::SeqCst);
                Ok(())
            }
        });
        let scheduler = JobScheduler::new("0 3 * * *", task).unwrap();

        let last = Utc::now();
        let next = scheduler.next_fire(last).unwrap();
        assert!(next > last);
    }

    #[tokio::test]
    async fn test_run_if_due_respects_schedule() {
        let counter = Arc::new(AtomicUsize::new(0));
        let c = counter.clone();
        let task = fn_task("count", move |_now| {
            let c = c.clone();
            async move {
                c.fetch_add(1, Ordering::SeqCst);
                Ok(())
            }
        });
        // 每分钟触发
        let scheduler = JobScheduler::new("* * * * *", task).unwrap();

        let now = Utc::now();
        let last_run = now - chrono::Duration::hours(2);
        let ran = scheduler.run_if_due(now, last_run).await.unwrap();
        assert!(ran, "两小时前跑过，现在必然到点");
        assert_eq!(counter.load(Ordering::SeqCst), 1);

        // 刚刚跑过：不到点
        let ran2 = scheduler.run_if_due(now, now).await.unwrap();
        assert!(!ran2);
        assert_eq!(counter.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn test_invalid_cron_rejected() {
        let task = fn_task("noop", |_| async { Ok(()) });
        assert!(JobScheduler::new("bad cron", task).is_err());
    }
}
