#include "storage/aio/BatchReadJob.h"

#include "common/monitor/Recorder.h"
#include "common/utils/Duration.h"
#include "storage/store/StorageTarget.h"

namespace hf3fs::storage {

monitor::CountRecorder rdmaWriteCount{"storage.rdma_write.count"};
monitor::CountRecorder rdmaWriteFails{"storage.rdma_write.fails"};
monitor::CountRecorder rdmaWriteBytes{"storage.rdma_write.bytes"};
monitor::LatencyRecorder batchReadLatency{"storage.aio.batch_latency"};

monitor::CountRecorder aioChecksumMismatch{"storage.aio.checksum_mismatch"};

AioReadJob::AioReadJob(const ReadIO &readIO, IOResult &result, BatchReadJob &batch)
    : readIO_(readIO),
      result_(result),
      batch_(batch) {
  state_.headLength = readIO_.offset % kAIOAlignSize;
  state_.tailLength = (kAIOAlignSize - (readIO_.offset + readIO_.length) % kAIOAlignSize) % kAIOAlignSize;
}

void AioReadJob::setResult(Result<uint32_t> lengthInfo) {
  if (lengthInfo) {
    auto checksumType = batch_.checksumType();

    if (checksumType == ChecksumType::NONE) {
      result_.checksum = {ChecksumType::NONE, 0U};  // do not return checksum
    } else if (checksumType == state_.chunkChecksum.type && readIO_.offset == 0 && *lengthInfo == state_.chunkLen) {
      result_.checksum = state_.chunkChecksum;  // use chunk checksum if the full chunk is read
    } else {                                    // calculate checksum of the read data
      auto dataBuf = state_.localbuf.subrange(state_.headLength, *lengthInfo);
      result_.checksum = ChecksumInfo::create(checksumType, dataBuf.ptr(), dataBuf.size());
    }

    // check chunk version.
    auto result = state_.storageTarget->aioFinishRead(*this);
    if (UNLIKELY(!result)) {
      lengthInfo = makeError(std::move(result.error()));
    }

    if (batch_.recalculateChecksum() && readIO_.offset == 0 && *lengthInfo == state_.chunkLen) {
      auto realChecksum = ChecksumInfo::create(state_.chunkChecksum.type, state_.localbuf.ptr(), *lengthInfo);
      if (UNLIKELY(realChecksum != state_.chunkChecksum)) {
        aioChecksumMismatch.addSample(1);
        auto msg = fmt::format("aio checksum mismatch, read: {}, state: {}, checksum: {}",
                               readIO(),
                               state(),
                               realChecksum.value);
        XLOG(CRITICAL, msg);
        lengthInfo = makeError(StorageCode::kChecksumMismatch, std::move(msg));
      }
    }
  }

  XLOGF_IF(WARN, !lengthInfo, "Read job failed, result: {}, read io: {}, state: {}", lengthInfo, readIO_, state_);
  XLOGF(DBG7, "Read job completed, result: {}, read io: {}, state: {}", lengthInfo, readIO_, state_);

  result_.lengthInfo = std::move(lengthInfo);
  state_.chunkEngineJob.reset();
  batch_.finish(this);
}

BatchReadJob::BatchReadJob(std::span<const ReadIO> readIOs, std::span<IOResult> results, ChecksumType checksumType)
    : checksumType_(checksumType) {
  auto batchSize = readIOs.size();
  jobs_.reserve(batchSize);
  for (auto i = 0ul; i < batchSize; ++i) {
    jobs_.emplace_back(readIOs[i], results[i], *this);
  }
}

size_t BatchReadJob::addBufferToBatch(serde::CallContext::RDMATransmission &batch) {
  return addRangeToBatch(batch, 0, jobs_.size());
}

size_t BatchReadJob::addRangeToBatch(serde::CallContext::RDMATransmission &batch, size_t begin, size_t end) {
  batch.reserve(end - begin, end - begin);  // upper bound: coalescing may use fewer entries
  size_t writeCount = 0;
  size_t writeBytes = 0;
  for (auto i = begin; i < end; ++i) {
    auto &job = jobs_[i];
    if (job.result().lengthInfo) {
      auto length = *job.result().lengthInfo;
      auto localbuf = job.state().localbuf.subrange(job.state().headLength, length);
      auto result = batch.add(job.readIO().rdmabuf, localbuf);
      if (UNLIKELY(!result)) {
        rdmaWriteFails.addSample(1);
        job.result().lengthInfo = makeError(std::move(result.error()));
      } else {
        ++writeCount;
        writeBytes += length;
      }
    }
  }
  rdmaWriteCount.addSample(writeCount);
  rdmaWriteBytes.addSample(writeBytes);
  return writeBytes;
}

void BatchReadJob::initCompletionQueue() {
  readyQueue_ = folly::MPMCQueue<AioReadJob *>(jobs_.size());
  completionDriven_ = true;
}

CoTask<AioReadJob *> BatchReadJob::takeReady() {
  co_await readySem_.co_wait();
  AioReadJob *job = nullptr;
  auto succ = readyQueue_.read(job);
  assert(succ);
  (void)succ;
  co_return job;
}

bool BatchReadJob::tryTakeReady(AioReadJob *&job) {
  if (!readySem_.try_wait()) {
    return false;
  }
  auto succ = readyQueue_.read(job);
  assert(succ);
  (void)succ;
  return true;
}

size_t BatchReadJob::addJobsToBatch(std::span<AioReadJob *const> jobs, serde::CallContext::RDMATransmission &batch) {
  batch.reserve(jobs.size(), jobs.size());
  size_t writeCount = 0;
  size_t writeBytes = 0;
  for (auto *jobPtr : jobs) {
    auto &job = *jobPtr;
    if (job.result().lengthInfo) {
      auto length = *job.result().lengthInfo;
      auto localbuf = job.state().localbuf.subrange(job.state().headLength, length);
      auto result = batch.add(job.readIO().rdmabuf, localbuf);
      if (UNLIKELY(!result)) {
        rdmaWriteFails.addSample(1);
        job.result().lengthInfo = makeError(std::move(result.error()));
      } else {
        ++writeCount;
        writeBytes += length;
      }
    }
  }
  rdmaWriteCount.addSample(writeCount);
  rdmaWriteBytes.addSample(writeBytes);
  return writeBytes;
}

void BatchReadJob::setJobsError(std::span<AioReadJob *const> jobs, const Status &error) {
  for (auto *job : jobs) {
    if (job->result().lengthInfo) {
      job->result().lengthInfo = makeError(error);
    }
  }
  anyPostFailed_.store(true, std::memory_order_relaxed);
}

size_t BatchReadJob::copyToRespBuffer(std::vector<uint8_t> &buffer) {
  // reserve the exact total once: the old per-job resize with a first-job-based
  // estimate caused repeated reallocations (and copies) for variable-sized jobs.
  size_t totalBytes = 0;
  for (auto &job : jobs_) {
    if (job.result().lengthInfo) {
      totalBytes += *job.result().lengthInfo;
    }
  }
  buffer.reserve(buffer.size() + totalBytes);

  size_t sendBytes = 0;
  for (auto &job : jobs_) {
    if (job.result().lengthInfo) {
      auto length = *job.result().lengthInfo;
      auto localbuf = job.state().localbuf.subrange(job.state().headLength, length);
      buffer.insert(buffer.end(), localbuf.ptr(), localbuf.ptr() + localbuf.size());
      sendBytes += length;
    }
  }
  return sendBytes;
}

void BatchReadJob::finish(AioReadJob *job) {
  if (completionDriven_) {
    // job result is fully written before this point; the queue write/read pair
    // and the semaphore provide the release/acquire edge to the sender.
    readyQueue_.write(job);
    readySem_.signal();
  }
  if (++finishedCount_ == jobs_.size()) {
    batchReadLatency.addSample(RelativeTime::now() - startTime());
    baton_.post();
  }
}

}  // namespace hf3fs::storage
