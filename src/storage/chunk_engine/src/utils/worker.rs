//! 極簡的背景執行緒：反覆呼叫同一個閉包，依閉包回傳的 `WorkerState` 決定下一步。
//!
//! crate 內唯一的使用者是 `core/engine.rs` 的 `start_allocate_workers` (測試與 bench 用)；
//! 正式環境的背景配置由 C++ `src/storage/worker/AllocateWorker.cc` 驅動，不經過這裡。
//!
//! ```text
//!   spawn
//!     |
//!     v
//!   +-------------------+  yes
//!   | stopping == true? |------> exit thread
//!   +-------------------+
//!     ^       | no
//!     |       v
//!     |     f() returns
//!     |       +-- Continue --------------------------------+
//!     |       +-- Wait(d)  -> condvar.wait_timeout(d) -----+
//!     |       +-- Pause    -> condvar.wait()  ------------+|
//!     |       +-- Stop     -> exit thread                 ||
//!     |                                                   ||
//!     +---------------------------------------------------++
//!
//!   stop_and_join(): stopping = true -> condvar.notify_all() -> join()
//! ```
//!
//! 注意：
//! - 等待用的 `Mutex` 是執行緒內的區域變數，condvar 沒有搭配共享條件；若
//!   `stop_and_join` 的 `notify_all` 剛好落在「檢查 stopping 之後、開始 wait 之前」，
//!   通知會遺失。`Wait(d)` 最多晚 d 才退出，`Pause` 則可能一直等不到。
//! - 沒有實作 `Drop`：不呼叫 `stop_and_join` 就丟掉 `Worker`，執行緒會脫離繼續跑。
//!   `Engine` 在自己的 `Drop` 裡呼叫 `stop_and_join` 收尾。

use std::{
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Condvar, Mutex,
    },
    thread::JoinHandle,
};

/// 閉包每輪的回傳值，決定執行緒接下來的動作。
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum WorkerState {
    /// 立刻再呼叫一次閉包 (還有工作要做)。
    Continue,
    /// 無限期等待，直到 condvar 被 notify。
    Pause,
    /// 最多等待指定時間 (或提早被 notify)，再呼叫閉包。
    Wait(std::time::Duration),
    /// 結束執行緒。
    Stop,
}

/// `Worker` 的 builder：可選執行緒名稱，以及可與外部共用的 `Condvar`
/// (外部持有同一個 condvar 就能 `notify` 喚醒 `Pause` / `Wait` 中的 worker)。
#[derive(Default)]
pub struct WorkerBuilder {
    name: Option<String>,
    condvar: Option<Arc<Condvar>>,
}

impl WorkerBuilder {
    pub fn name(mut self, str: String) -> Self {
        self.name = Some(str);
        self
    }

    pub fn cond(mut self, condvar: Arc<Condvar>) -> Self {
        self.condvar = Some(condvar);
        self
    }

    pub fn spawn<F>(self, f: F) -> Worker
    where
        F: FnMut() -> WorkerState + Send + 'static,
    {
        Worker::new(f, self.name, self.condvar)
    }
}

/// 背景執行緒的 handle。`stopping` 與 `condvar` 和執行緒內共用，用來通知它結束。
pub struct Worker {
    stopping: Arc<AtomicBool>,
    condvar: Arc<Condvar>,
    handle: Option<JoinHandle<()>>,
}

impl Worker {
    /// 立刻 spawn 執行緒並開始迴圈；執行緒建立失敗會 panic (`unwrap`)。
    pub fn new<F>(mut f: F, name: Option<String>, condvar: Option<Arc<Condvar>>) -> Worker
    where
        F: FnMut() -> WorkerState + Send + 'static,
    {
        // 1. 準備與執行緒共用的停止旗標與 condvar (沒給就自己建一個)
        let stopping = Arc::new(AtomicBool::default());
        let stopping_clone = stopping.clone();
        let condvar = condvar.unwrap_or_default();
        let condvar_clone = condvar.clone();

        // 2. 建立 thread builder (有名稱就設定，方便在 top / gdb 中辨識)
        let builder = if let Some(name) = name {
            std::thread::Builder::new().name(name)
        } else {
            std::thread::Builder::new()
        };
        // 3. spawn 執行緒：每輪先看 stopping，再呼叫 f() 並依回傳值決定等待方式
        let handle = Some(
            builder
                .spawn(move || {
                    // 只為了滿足 Condvar::wait 需要 MutexGuard 的介面，沒有保護任何共享資料
                    let mutex = Mutex::new(());
                    while !stopping_clone.load(Ordering::Acquire) {
                        match f() {
                            WorkerState::Continue => continue,
                            WorkerState::Pause => {
                                drop(condvar_clone.wait(mutex.lock().unwrap()).unwrap());
                            }
                            WorkerState::Wait(duration) => {
                                drop(
                                    condvar_clone
                                        .wait_timeout(mutex.lock().unwrap(), duration)
                                        .unwrap(),
                                );
                            }
                            WorkerState::Stop => break,
                        }
                    }
                })
                .unwrap(),
        );

        Worker {
            stopping,
            condvar,
            handle,
        }
    }

    /// 設停止旗標、喚醒等待中的執行緒，再 join。可重複呼叫 (第二次 handle 已是 None)。
    pub fn stop_and_join(&mut self) {
        self.stopping.store(true, Ordering::Release);
        self.condvar.notify_all();
        if let Some(handle) = self.handle.take() {
            handle.join().unwrap();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // 閉包前 9 次回 Continue、第 10 次回 Pause，之後 stop_and_join 能喚醒並結束執行緒
    #[test]
    fn test_worker() {
        let count = Arc::new(std::sync::atomic::AtomicUsize::default());
        let condvar = Default::default();
        let count_clone = count.clone();
        let mut worker = WorkerBuilder::default()
            .name("Worker".into())
            .cond(condvar)
            .spawn(move || {
                if count_clone.fetch_add(1, Ordering::SeqCst) + 1 < 10 {
                    WorkerState::Continue
                } else {
                    WorkerState::Pause
                }
            });

        while count.load(Ordering::Acquire) < 10 {
            std::thread::sleep(std::time::Duration::from_millis(10));
        }

        worker.stop_and_join();
        assert_eq!(count.load(Ordering::Acquire), 10);
    }

    // 閉包回 Stop 時執行緒自行結束
    #[test]
    fn test_worker_2() {
        let worker = WorkerBuilder::default().spawn(move || WorkerState::Stop);
        let _ = worker.handle.unwrap().join();
    }
}
