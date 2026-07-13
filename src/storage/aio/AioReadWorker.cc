#include "storage/aio/AioReadWorker.h"

#include <algorithm>
#include <folly/ScopeGuard.h>
#include <folly/logging/xlog.h>
#include <folly/system/ThreadName.h>

#include "common/monitor/Recorder.h"
#include "storage/aio/AioStatus.h"
#include "storage/aio/BatchReadJob.h"

namespace hf3fs::storage {

monitor::LatencyRecorder batchReadInQueueRecorder{"storage.batch_read_in_queue.latency"};
monitor::CountRecorder aioRunningThreadsCount{"storage.aio_running_threads.count", std::nullopt, false};

AioReadWorker::~AioReadWorker() { stopAndJoin(); }

Result<Void> AioReadWorker::start(const std::vector<int> &fds, const std::vector<struct iovec> &iovecs) {
  // flat fd → registered-index table shared read-only by all worker rings.
  int maxFd = -1;
  for (auto fd : fds) {
    maxFd = std::max(maxFd, fd);
  }
  fdToRegisteredIndex_.assign(maxFd < 0 ? 0 : size_t(maxFd) + 1, -1);
  for (size_t i = 0; i < fds.size(); ++i) {
    if (fds[i] >= 0) {
      fdToRegisteredIndex_[fds[i]] = int32_t(i);
    }
  }

  uint32_t numThreads = config_.num_threads();
  for (auto i = 0u; i < numThreads; ++i) {
    executors_.add([&]() {
      AioStatus aioStatus;
      IoUringStatus ioUringStatus;
      {
        SCOPE_EXIT { ++initialized_; };
        auto aioInitResult = aioStatus.init(config_.max_events());
        if (UNLIKELY(!aioInitResult)) {
          XLOGF(ERR, "aio status init failed: {}", aioInitResult.error());
          *initResult_.lock() = std::move(aioInitResult);
          return;
        }
        if (config_.enable_io_uring()) {
          auto ioUringResult = ioUringStatus.init(config_.max_events(), fds, iovecs, &fdToRegisteredIndex_);
          if (UNLIKELY(!ioUringResult)) {
            XLOGF(ERR, "io uring status init failed: {}", ioUringResult.error());
            *initResult_.lock() = std::move(ioUringResult);
            return;
          }
        }
      }
      run(aioStatus, ioUringStatus);
    });
  }
  for (int i = 0; initialized_ != numThreads; ++i) {
    XLOGF_IF(INFO, i % 5 == 0, "Waiting for AioReadWorker@{}::run start...", fmt::ptr(this));
    std::this_thread::sleep_for(100_ms);
  }
  RETURN_AND_LOG_ON_ERROR(*initResult_.lock());
  return Void{};
}

Result<Void> AioReadWorker::stopAndJoin() {
  for (auto i = 0u; i < config_.num_threads(); ++i) {
    queue_.enqueue(AioReadJobIterator{});
  }
  executors_.join();
  return Void{};
}

Result<Void> AioReadWorker::run(AioStatus &aioStatus, IoUringStatus &ioUringStatus) {
  aioRunningThreadsCount.addSample(1);
  auto guard = folly::makeGuard([] { aioRunningThreadsCount.addSample(-1); });

  while (true) {
    // 1. try to fetch a batch read job.
    aioRunningThreadsCount.addSample(-1);
    auto it = queue_.dequeue();  // waiting.
    aioRunningThreadsCount.addSample(1);
    if (it.isNull()) {
      XLOGF(DBG, "Stop AioReadWorker {}...", fmt::ptr(this));
      return Void{};
    }
    batchReadInQueueRecorder.addSample(RelativeTime::now() - it.startTime());
    it->batch().resetStartTime();

    IoStatus &status = config_.useIoUring() ? static_cast<IoStatus &>(ioUringStatus) : aioStatus;
    status.setAioReadJobIterator(it);

    // Keep the submission window full: refill right after each reap instead of
    // draining all inflight IOs to zero before collecting new jobs.
    bool stopping = false;
    do {
      // 2. collect a batch of read jobs.
      status.collect();

      // 3. the current iterator is exhausted but the window still has room:
      //    opportunistically pull more batches from the queue to keep the disk busy.
      while (!stopping && !status.hasUnfinishedBatchReadJob() && status.availableToSubmit()) {
        auto next = queue_.try_dequeue();
        if (!next.has_value()) {
          break;
        }
        if (next->isNull()) {
          // hand the stop signal back to sibling threads; finish inflight IOs first.
          stopping = true;
          queue_.enqueue(AioReadJobIterator{});
          break;
        }
        batchReadInQueueRecorder.addSample(RelativeTime::now() - next->startTime());
        (*next)->batch().resetStartTime();
        status.setAioReadJobIterator(*next);
        status.collect();
      }

      // 4. submit a batch of read jobs.
      status.submit();

      // 5. wait a batch of events, then go back to refill the window.
      if (status.inflight()) {
        status.reap(config_.min_complete());
      }
    } while (status.hasUnfinishedBatchReadJob() || status.inflight());
  }

  return Void{};
}

}  // namespace hf3fs::storage
