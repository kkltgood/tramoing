//! 导出任务在后台执行，界面仅轮询结果；正常退出时等待文件写完。
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::thread::{self, JoinHandle};

pub struct ExportJob {
    result: Receiver<String>,
    worker: Option<JoinHandle<()>>,
}

impl ExportJob {
    pub fn start(
        export: impl FnOnce() -> String + Send + 'static,
        wake: impl FnOnce() + Send + 'static,
    ) -> std::io::Result<Self> {
        let (tx, result) = mpsc::channel();
        let worker = thread::Builder::new()
            .name("history-export".into())
            .spawn(move || {
                let message = export();
                let _ = tx.send(message);
                wake();
            })?;
        Ok(Self {
            result,
            worker: Some(worker),
        })
    }

    pub fn try_result(&self) -> Result<String, TryRecvError> {
        self.result.try_recv()
    }
}

impl Drop for ExportJob {
    fn drop(&mut self) {
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn slow_export_keeps_caller_available_and_wakes_it_after_completion() {
        let (release, blocked) = mpsc::channel();
        let (ready, woke) = mpsc::channel();
        let job = ExportJob::start(
            move || {
                blocked.recv().unwrap();
                "exported".into()
            },
            move || {
                ready.send(()).unwrap();
            },
        )
        .unwrap();
        assert_eq!(job.try_result(), Err(TryRecvError::Empty));
        release.send(()).unwrap();
        woke.recv_timeout(Duration::from_secs(5)).unwrap();
        assert_eq!(job.try_result().unwrap(), "exported");
    }

    #[test]
    fn shutdown_waits_for_the_export_to_finish() {
        let (done, completed) = mpsc::channel();
        let job = ExportJob::start(
            move || {
                done.send(()).unwrap();
                "done".into()
            },
            || {},
        )
        .unwrap();
        drop(job);
        assert_eq!(completed.try_recv(), Ok(()));
    }
}
